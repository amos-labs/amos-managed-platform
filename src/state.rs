//! Platform application state shared across all handlers.

use amos_core::{AmosError, AppConfig, Result};
use redis::aio::ConnectionManager;
use secrecy::ExposeSecret;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::Arc;
use tracing::{info, warn};

use crate::billing::usage_metering::UsageMeter;
use crate::provisioning::alb::{AlbRouter, AlbRouterConfig};
use crate::provisioning::app_deploy::AppManager;
use crate::provisioning::aws_app::{AwsAppProvisioner, AwsAppProvisionerConfig};
use crate::provisioning::ecs::{EcsProvisioner, EcsProvisionerConfig};
use crate::provisioning::image_builder::{ImageBuilder, ImageBuilderConfig};
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
    /// Price ID per app-hosting tier (Starter/Pro/Compliance).
    pub price_app_starter: Option<String>,
    pub price_app_pro: Option<String>,
    pub price_app_compliance: Option<String>,
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
            price_app_starter: std::env::var("AMOS__STRIPE__PRICE_APP_STARTER").ok(),
            price_app_pro: std::env::var("AMOS__STRIPE__PRICE_APP_PRO").ok(),
            price_app_compliance: std::env::var("AMOS__STRIPE__PRICE_APP_COMPLIANCE").ok(),
        })
    }

    /// Stripe Price ID for an app-hosting tier plan key (`app_starter` etc.).
    pub fn price_id_for_app_tier(&self, plan_key: &str) -> Option<&str> {
        match plan_key {
            "app_starter" => self.price_app_starter.as_deref(),
            "app_pro" => self.price_app_pro.as_deref(),
            "app_compliance" => self.price_app_compliance.as_deref(),
            _ => None,
        }
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
    /// Multi-service application provisioner (Docker API — local dev).
    pub app_manager: Option<Arc<AppManager>>,
    /// ECS Fargate provisioner (production — used when Docker unavailable).
    pub ecs_provisioner: Option<Arc<EcsProvisioner>>,
    /// ALB router for subdomain-based harness routing (production).
    pub alb_router: Option<Arc<AlbRouter>>,
    /// Cloud image builder (build-as-a-service via CodeBuild → ECR).
    pub image_builder: Option<Arc<ImageBuilder>>,
    /// AWS Fargate renderer for multi-service app deploys (deploy_app provider=aws).
    pub aws_app_provisioner: Option<Arc<AwsAppProvisioner>>,
    /// CloudWatch-backed usage meter for exact app-hosting billing (unit-hours
    /// + egress). Initialized alongside the AWS app provisioner.
    pub usage_meter: Option<Arc<UsageMeter>>,
    /// Stripe API client (None if Stripe not configured).
    pub stripe_client: Option<StripeClient>,
    /// Stripe configuration (price IDs, webhook secret).
    pub stripe_config: Option<StripeConfig>,
    /// Finance engine backend (the reference governed business engine). Defaults
    /// to a stub; swapped for an HTTP client against the extracted multi-tenant
    /// finance service. The `finance_*` MCP verbs proxy to this behind `finance:*`
    /// scopes + proof receipts.
    pub finance: Arc<dyn crate::mcp::finance::FinanceEngineClient>,
    /// Transactional email sender (SES). `None` unless `AMOS__EMAIL__FROM` is
    /// set; when present, password-reset links are emailed to the user.
    pub mailer: Option<Arc<crate::email::Mailer>>,
    /// In-flight manifest `deploy`s, keyed `deployment_id → deploy_id`, so a
    /// second deploy for the same app returns `already_deploying` cleanly
    /// instead of spawning a colliding job (which previously no-op'd into a
    /// confusing `verified=false`). In-memory: correct at the current
    /// single-instance scale; a Postgres advisory lock is the multi-instance
    /// upgrade.
    pub in_flight_deploys: Arc<std::sync::Mutex<std::collections::HashMap<uuid::Uuid, uuid::Uuid>>>,
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

        // Initialize multi-service app provisioner (Docker API — local dev).
        // Shares the same daemon as the harness manager.
        let app_manager = match AppManager::new() {
            Ok(manager) => {
                info!("App manager initialized (Docker connected)");
                Some(Arc::new(manager))
            }
            Err(e) => {
                warn!("App manager initialization failed (optional): {}", e);
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

        // Initialize the cloud image builder (build-as-a-service). Independent
        // of Docker/ECS — it just needs AWS creds + the CodeBuild substrate.
        // Gated on AMOS_BUILD_SOURCE_BUCKET.
        let image_builder = match ImageBuilderConfig::from_env() {
            Some(cfg) => match ImageBuilder::new(cfg).await {
                Ok(b) => {
                    info!("Image builder initialized (CodeBuild build-as-a-service)");
                    Some(Arc::new(b))
                }
                Err(e) => {
                    warn!("Image builder initialization failed (optional): {}", e);
                    None
                }
            },
            None => {
                info!("Image builder not configured (AMOS_BUILD_SOURCE_BUCKET not set)");
                None
            }
        };

        // Initialize the AWS Fargate app renderer (deploy_app provider=aws) and
        // the CloudWatch usage meter that bills it. Both gated on the same
        // AWS_APP_ENABLED config (cluster + region).
        let (aws_app_provisioner, usage_meter) = match AwsAppProvisionerConfig::from_env() {
            Some(cfg) => {
                // Build the usage meter from the same cluster/region before the
                // provisioner consumes the config.
                let cluster = cfg.cluster.clone();
                let region = cfg.region.clone();
                let aws_conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(aws_config::Region::new(region))
                    .load()
                    .await;
                let meter = Some(Arc::new(UsageMeter::new(&aws_conf, cluster)));
                info!("Usage meter initialized (CloudWatch metered app-hosting billing)");

                let provisioner = match AwsAppProvisioner::new(cfg).await {
                    Ok(p) => {
                        info!("AWS app provisioner initialized (Fargate deploys enabled)");
                        Some(Arc::new(p))
                    }
                    Err(e) => {
                        warn!(
                            "AWS app provisioner initialization failed (optional): {}",
                            e
                        );
                        None
                    }
                };
                (provisioner, meter)
            }
            None => {
                info!("AWS app provisioner not configured (AWS_APP_ENABLED != true)");
                (None, None)
            }
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

        // Finance engine backend: proxy to Nuvola's existing MCP when configured
        // (AMOS__NUVOLA__MCP_TOKEN), else the stub (dev / unconfigured).
        let finance: Arc<dyn crate::mcp::finance::FinanceEngineClient> =
            match crate::mcp::finance::HttpFinanceClient::from_env() {
                Some(client) => {
                    info!("Finance engine: proxying to Nuvola MCP");
                    Arc::new(client)
                }
                None => {
                    info!("Finance engine: using stub backend (AMOS__NUVOLA__MCP_TOKEN not set)");
                    Arc::new(crate::mcp::finance::StubFinanceClient)
                }
            };

        // Transactional email (SES) — only when AMOS__EMAIL__FROM is configured.
        // Loading the AWS config is skipped entirely when email is disabled.
        let mailer = if std::env::var("AMOS__EMAIL__FROM")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            let aws_conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .load()
                .await;
            match crate::email::Mailer::from_env(&aws_conf) {
                Some(m) => {
                    info!("Mailer initialized (SES password-reset email enabled)");
                    Some(Arc::new(m))
                }
                None => None,
            }
        } else {
            info!(
                "Mailer not configured (AMOS__EMAIL__FROM not set; reset links delivered by admin)"
            );
            None
        };

        Ok(Self {
            db,
            redis,
            config: Arc::new(config),
            solana,
            harness_manager,
            app_manager,
            ecs_provisioner,
            alb_router,
            image_builder,
            aws_app_provisioner,
            usage_meter,
            stripe_client,
            stripe_config,
            finance,
            mailer,
            in_flight_deploys: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
