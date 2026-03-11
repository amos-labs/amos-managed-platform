//! HTTP and gRPC server setup.

use crate::{middleware, routes, state::PlatformState, Result};
use axum::{
    http::{header, Method},
    Router,
};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};
use tracing::info;

/// Start the HTTP REST API server (Axum).
pub async fn start_http_server(state: PlatformState) -> Result<()> {
    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);
    info!("Starting HTTP server on {}", addr);

    let app = build_http_router(state);

    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| amos_core::AmosError::Internal(format!("Failed to bind {}: {}", addr, e)))?;

    info!("HTTP server listening on {}", addr);

    axum::serve(listener, app)
        .await
        .map_err(|e| amos_core::AmosError::Internal(format!("HTTP server error: {}", e)))?;

    Ok(())
}

/// Build the Axum router with all routes and middleware.
fn build_http_router(state: PlatformState) -> Router {
    let api_routes = routes::api_routes();

    let ui_routes = routes::ui::routes();

    Router::new()
        // UI routes (SSR pages: /login, /register, /dashboard, /settings)
        .merge(ui_routes)
        // Root path: redirect browsers to login, serve API catalog for agents
        .route("/", axum::routing::get(routes::discovery::api_catalog))
        .nest("/api/v1", api_routes)
        // Fallback: agent-friendly 404 with suggestions
        .fallback(routes::discovery::not_found)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new())
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
                .layer(
                    CorsLayer::new()
                        .allow_origin(Any)
                        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]),
                )
                .layer(axum::middleware::from_fn(middleware::error_handler)),
        )
        .with_state(state)
}

/// Start the gRPC server for harness communication.
///
/// TODO: Implement gRPC service definitions using tonic.
/// For now, this is a placeholder that binds the port.
pub async fn start_grpc_server(state: PlatformState) -> Result<()> {
    let addr = format!("{}:{}", state.config.server.host, state.config.server.grpc_port);
    info!("gRPC server would start on {} (not yet implemented)", addr);

    // TODO: Add tonic server with HarnessService implementation
    // let addr = addr.parse().unwrap();
    // tonic::transport::Server::builder()
    //     .add_service(harness_service_server::HarnessServiceServer::new(HarnessServiceImpl { state }))
    //     .serve(addr)
    //     .await
    //     .map_err(|e| AmosError::Internal(format!("gRPC server error: {}", e)))?;

    // Placeholder: sleep forever
    tokio::time::sleep(Duration::from_secs(u64::MAX)).await;
    Ok(())
}
