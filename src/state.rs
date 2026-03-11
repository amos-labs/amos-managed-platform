//! Platform application state shared across all handlers.

use amos_core::{AmosError, AppConfig, Result};
use redis::aio::ConnectionManager;
use secrecy::ExposeSecret;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::Arc;
use tracing::{info, warn};

use crate::provisioning::HarnessManager;
use crate::solana::SolanaClient;

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
    /// Harness provisioning manager (Docker API).
    pub harness_manager: Option<Arc<HarnessManager>>,
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
            .map_err(|e| AmosError::Database(e.into()))?;
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

        Ok(Self {
            db,
            redis,
            config: Arc::new(config),
            solana,
            harness_manager,
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
            .map_err(|e| AmosError::Database(e.into()))?;

        // Check Redis
        use redis::AsyncCommands;
        let mut conn = self.redis.clone();
        conn.get::<&str, Option<String>>("__health__")
            .await
            .map_err(|e| AmosError::Internal(format!("Redis health check failed: {}", e)))?;

        Ok(())
    }
}
