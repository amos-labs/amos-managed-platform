//! Health check endpoints.

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::state::PlatformState;

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/health", get(health))
        .route("/readiness", get(readiness))
}

#[derive(Serialize, Deserialize)]
struct HealthResponse {
    status: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    db: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    redis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    solana: Option<String>,
}

/// Basic liveness check (always returns OK).
async fn health() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok".into(),
        version: crate::VERSION.into(),
        db: None,
        redis: None,
        solana: None,
    })
}

/// Readiness check with DB, Redis, and Solana validation.
async fn readiness(State(state): State<PlatformState>) -> Result<impl IntoResponse, StatusCode> {
    let mut response = HealthResponse {
        status: "ready".into(),
        version: crate::VERSION.into(),
        db: None,
        redis: None,
        solana: None,
    };

    // Check database
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => response.db = Some("ok".into()),
        Err(e) => {
            error!("Database health check failed: {}", e);
            response.status = "not_ready".into();
            response.db = Some(format!("error: {}", e));
        }
    }

    // Check Redis
    use redis::AsyncCommands;
    let mut redis_conn = state.redis.clone();
    match redis_conn.get::<&str, Option<String>>("__health__").await {
        Ok(_) => response.redis = Some("ok".into()),
        Err(e) => {
            error!("Redis health check failed: {}", e);
            response.status = "not_ready".into();
            response.redis = Some(format!("error: {}", e));
        }
    }

    // Check Solana (optional)
    if let Some(ref solana) = state.solana {
        match solana.health_check().await {
            Ok(_) => response.solana = Some("ok".into()),
            Err(e) => {
                error!("Solana health check failed: {}", e);
                // Don't mark as not_ready since Solana is optional
                response.solana = Some(format!("warning: {}", e));
            }
        }
    } else {
        response.solana = Some("disabled".into());
    }

    if response.status == "ready" {
        Ok(Json(response))
    } else {
        Err(StatusCode::SERVICE_UNAVAILABLE)
    }
}
