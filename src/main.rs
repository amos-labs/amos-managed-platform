//! # AMOS Platform Binary
//!
//! Main entry point for the centralized platform service.
//!
//! This binary starts:
//! - HTTP REST API server (Axum) on port 4000
//! - gRPC server for harness communication on port 4001
//! - Background workers for decay, emission, billing
//! - Metrics and tracing exporters

use amos_core::AppConfig;
use amos_platform::{server, services::BountyService, state::PlatformState, Result};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    init_tracing()?;

    info!("Starting AMOS Platform v{}", amos_platform::VERSION);

    // Load configuration
    let config = AppConfig::load()?;
    info!(
        "Configuration loaded: HTTP port={}, gRPC port={}",
        config.server.port, config.server.grpc_port
    );

    // Initialize platform state (DB, Redis, Solana client)
    let state = PlatformState::new(config).await?;
    info!("Platform state initialized successfully");

    // Run database migrations
    state.run_migrations().await?;
    info!("Database migrations completed");

    // Start the nightly bounty service scheduler
    let bounty_service = BountyService::new(state.clone());
    bounty_service.start_nightly_scheduler();
    info!("Bounty service nightly scheduler started");

    // Start both HTTP and gRPC servers concurrently
    let http_server = server::start_http_server(state.clone());
    let grpc_server = server::start_grpc_server(state.clone());

    tokio::select! {
        result = http_server => {
            error!("HTTP server exited: {:?}", result);
            result
        }
        result = grpc_server => {
            error!("gRPC server exited: {:?}", result);
            result
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down gracefully");
            Ok(())
        }
    }
}

/// Initialize OpenTelemetry tracing with OTLP exporter.
fn init_tracing() -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,amos_platform=debug,amos_core=debug"));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .json();

    // TODO: Add OpenTelemetry layer when OTLP_ENDPOINT is configured
    // For now, just use stdout JSON logging
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
        .map_err(|e| amos_core::AmosError::Internal(format!("Failed to init tracing: {}", e)))?;

    Ok(())
}
