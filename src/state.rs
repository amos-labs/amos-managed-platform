//! Platform application state shared across all handlers.

use amos_core::{AmosError, AppConfig, Result};
use redis::aio::ConnectionManager;
use secrecy::ExposeSecret;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::Arc;
use tracing::{info, warn};

use crate::provisioning::alb::{AlbRouter, AlbRouterConfig};
use crate::provisioning::ecs::{EcsProvisioner, EcsProvisionerConfig};
use crate::provisioning::HarnessManager;
use crate::solana::SolanaClient;

/// Re-export the Stripe client type for handlers.
pub type StripeClient = stripe::Client;

/// Stripe billing configuration, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct StripeConfig {
    /// Stripe secret API key.
    pub secret_key: String,
    /// Stripe publishable key (for client-side if needed).
    pub publishable_key: Option<String>,
    /// Stripe webhook signing secret.
    pub webhook_secret: Option<String>,
    /// Price ID per harness size.
    pub price_small: Option<String>,
    pub price_medium: Option<String>,
    pub price_large: Option<String>,
}

impl StripeConfig {
    /// Load from env vars. Returns None if AMOS__STRIPE__SECRET_KEY is not set.
    pub fn from_env() -> Option<Self> {
        let secret_key = std::env::var("AMOS__STRIPE__SECRET_KEY").ok()?;
        Some(Self {
            secret_key,
            publishable_key: std::env::var("AMOS__STRIPE__PUBLISHABLE_KEY").ok(),
            webhook_secret: std::env::var("AMOS__STRIPE__WEBHOOK_SECRET").ok(),
            price_small: std::env::var("AMOS__STRIPE__PRICE_SMALL")
                .ok()
                // Backwards compat: PRICE_HOSTED maps to small
                .or_else(|| std::env::var("AMOS__STRIPE__PRICE_HOSTED").ok()),
            price_medium: std::env::var("AMOS__STRIPE__PRICE_MEDIUM").ok(),
            price_large: std::env::var("AMOS__STRIPE__PRICE_LARGE").ok(),
        })
    }

    /// Get the Stripe Price ID for a harness size.
    pub fn price_id_for_size(&self, size: &str) -> Option<&str> {
        match size {
            "small" => self.price_small.as_deref(),
            "medium" => self.price_medium.as_deref(),
            "large" => self.price_large.as_deref(),
            // Legacy: plan names map to small
            "hosted" | "starter" | "growth" | "enterprise" => self.price_small.as_deref(),
            _ => None,
        }
    }
}

/// Shared application state for the AMOS platform.
///
/// This struct is cloned cheaply (via Arc internally) and passed
/// to every HTTP and gRPC handler.
#[derive(Clone)]
pub struct PlatformState {
    /// PostgreSQL connection pool.
    pub db: PgPool,
    /// Redis connection manager.
    pub redis: ConnectionManager,
    /// Application configuration.
    pub config: Arc<AppConfig>,
    /// Optional Solana RPC client (None if feature disabled).
    pub solana: Option<Arc<SolanaClient>>,
    /// Harness provisioning manager (Docker API — local dev).
    pub harness_manager: Option<Arc<HarnessManager>>,
    /// ECS Fargate provisioner (production — used when Docker unavailable).
    pub ecs_provisioner: Option<Arc<EcsProvisioner>>,
    /// ALB router for subdomain-based harness routing (production).
    pub alb_router: Option<Arc<AlbRouter>>,
    /// Stripe API client (None if Stripe not configured).
    pub stripe_client: Option<StripeClient>,
    /// Stripe configuration (price IDs, webhook secret).
    pub stripe_config: Option<StripeConfig>,
}

impl PlatformState {
    /// Initialize platform state with database, Redis, and optional Solana client.
    pub async fn new(config: AppConfig) -> Result<Self> {
        // Connect to PostgreSQL
        info!("Connecting to PostgreSQL...");
        let db = PgPoolOptions::new()
            .max_connections(config.database.pool_size)
            .connect(config.database.url.expose_secret())
            .await
            .map_err(AmosError::Database)?;
        info!("PostgreSQL connection pool established");

        // Connect to Redis
        info!("Connecting to Redis at {}...", config.redis.url);
        let redis_client = redis::Client::open(config.redis.url.as_str())
            .map_err(|e| AmosError::Internal(format!("Failed to create Redis client: {}", e)))?;
        let redis = ConnectionManager::new(redis_client)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to connect to Redis: {}", e)))?;
        info!("Redis connection established");

        // Initialize Solana client (optional, may fail in dev)
        let solana = match SolanaClient::new(
            &config.solana.rpc_url,
            &config.solana.treasury_program_id,
            &config.solana.governance_program_id,
            &config.solana.bounty_program_id,
        ) {
            Ok(client) => {
                info!("Solana client initialized: {}", config.solana.rpc_url);
                Some(Arc::new(client))
            }
            Err(e) => {
                warn!("Solana client initialization failed (optional): {}", e);
                None
            }
        };

        // Initialize Docker-based harness manager (optional, may fail if Docker not available)
        let harness_manager = match HarnessManager::new() {
            Ok(manager) => {
                info!("Harness manager initialized (Docker connected)");
                Some(Arc::new(manager))
            }
            Err(e) => {
                warn!("Harness manager initialization failed (optional): {}", e);
                None
            }
        };

        // Initialize ECS provisioner (production — used when Docker is not available).
        // Only activates when ECS_HARNESS_IMAGE env var is set.
        let ecs_provisioner = if harness_manager.is_none() {
            match EcsProvisionerConfig::from_env() {
                Some(ecs_config) => match EcsProvisioner::new(ecs_config).await {
                    Ok(provisioner) => {
                        info!("ECS provisioner initialized (Fargate mode)");
                        Some(Arc::new(provisioner))
                    }
                    Err(e) => {
                        warn!("ECS provisioner initialization failed (optional): {}", e);
                        None
                    }
                },
                None => {
                    info!("ECS provisioner not configured (ECS_HARNESS_IMAGE not set)");
                    None
                }
            }
        } else {
            // Docker is available, no need for ECS provisioner.
            None
        };

        // Initialize ALB router for subdomain-based harness routing (production).
        // Only activates when ALB_HTTPS_LISTENER_ARN env var is set.
        let alb_router = if ecs_provisioner.is_some() {
            match AlbRouterConfig::from_env() {
                Some(alb_config) => match AlbRouter::new(alb_config).await {
                    Ok(router) => {
                        info!("ALB router initialized (subdomain routing enabled)");
                        Some(Arc::new(router))
                    }
                    Err(e) => {
                        warn!("ALB router initialization failed (optional): {}", e);
                        None
                    }
                },
                None => {
                    info!("ALB router not configured (ALB_HTTPS_LISTENER_ARN not set)");
                    None
                }
            }
        } else {
            None
        };

        // Initialize Stripe client (optional, requires AMOS__STRIPE__SECRET_KEY).
        let stripe_config = StripeConfig::from_env();
        let stripe_client = stripe_config.as_ref().map(|cfg| {
            info!("Stripe client initialized");
            stripe::Client::new(&cfg.secret_key)
        });

        if stripe_client.is_none() {
            info!("Stripe not configured (AMOS__STRIPE__SECRET_KEY not set)");
        }

        Ok(Self {
            db,
            redis,
            config: Arc::new(config),
            solana,
            harness_manager,
            ecs_provisioner,
            alb_router,
            stripe_client,
            stripe_config,
        })
    }

    /// Run database migrations (idempotent).
    pub async fn run_migrations(&self) -> Result<()> {
        info!("Running database migrations...");
        sqlx::migrate!("./migrations")
            .run(&self.db)
            .await
            .map_err(|e| AmosError::Database(e.into()))?;
        info!("Database migrations completed successfully");
        Ok(())
    }

    /// Health check: verify DB and Redis are reachable.
    pub async fn health_check(&self) -> Result<()> {
        // Check PostgreSQL
        sqlx::query("SELECT 1")
            .execute(&self.db)
            .await
            .map_err(AmosError::Database)?;

        // Check Redis
        use redis::AsyncCommands;
        let mut conn = self.redis.clone();
        conn.get::<&str, Option<String>>("__health__")
            .await
            .map_err(|e| AmosError::Internal(format!("Redis health check failed: {}", e)))?;

        Ok(())
    }
}
