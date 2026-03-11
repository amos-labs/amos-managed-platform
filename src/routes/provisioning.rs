//! Harness provisioning API endpoints.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info};
use uuid::Uuid;

use crate::{
    provisioning::{HarnessConfig, HarnessStatus, InstanceSize},
    state::PlatformState,
};

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/provision/harness", post(provision_harness))
        .route("/provision/harness/{id}", get(get_harness_status))
        .route("/provision/harness/{id}/start", post(start_harness))
        .route("/provision/harness/{id}/stop", post(stop_harness))
        .route("/provision/harness/{id}", delete(deprovision_harness))
        .route("/provision/harness/{id}/logs", get(get_harness_logs))
}

//    Provision Harness

#[derive(Deserialize)]
struct ProvisionHarnessRequest {
    customer_id: Uuid,
    region: Option<String>,
    instance_size: Option<String>, // "small", "medium", "large"
    environment: Option<String>,   // "production", "staging", "development"
    env_vars: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
struct ProvisionHarnessResponse {
    harness_id: String,
    status: HarnessStatus,
    container_id: Option<String>,
    http_endpoint: String,
    provisioned_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

async fn provision_harness(
    State(state): State<PlatformState>,
    Json(req): Json<ProvisionHarnessRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let manager = state.harness_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Docker is not available. Harness provisioning requires Docker daemon."
                    .into(),
            }),
        )
    })?;

    let instance_size = match req.instance_size.as_deref() {
        Some("medium") => InstanceSize::Medium,
        Some("large") => InstanceSize::Large,
        _ => InstanceSize::Small,
    };

    let platform_url = format!(
        "http://{}:{}",
        state.config.server.host, state.config.server.port
    );

    let config = HarnessConfig {
        customer_id: req.customer_id,
        region: req.region.unwrap_or_else(|| "us-west-2".into()),
        instance_size,
        environment: req.environment.unwrap_or_else(|| "development".into()),
        platform_grpc_url: platform_url,
        env_vars: req.env_vars.unwrap_or_default(),
    };

    info!(
        customer_id = %config.customer_id,
        region = %config.region,
        size = ?config.instance_size,
        "Provisioning new harness container"
    );

    let container_id = manager.provision(&config).await.map_err(|e| {
        error!("Failed to provision harness: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Provisioning failed: {}", e),
            }),
        )
    })?;

    // Auto-start the container after provisioning
    if let Err(e) = manager.start(&container_id).await {
        error!("Failed to auto-start harness {}: {}", container_id, e);
        // Container is provisioned but not started - return as Provisioning status
    }

    let harness_id = format!("harness-{}", req.customer_id);
    let http_endpoint = format!("http://{}:3000", harness_id);

    info!(
        container_id = %container_id,
        harness_id = %harness_id,
        "Harness provisioned and started"
    );

    Ok((
        StatusCode::CREATED,
        Json(ProvisionHarnessResponse {
            harness_id,
            status: HarnessStatus::Running,
            container_id: Some(container_id),
            http_endpoint,
            provisioned_at: Utc::now(),
        }),
    ))
}

//    Get Harness Status

#[derive(Serialize)]
struct HarnessStatusResponse {
    harness_id: String,
    status: HarnessStatus,
    container_id: String,
}

async fn get_harness_status(
    State(state): State<PlatformState>,
    Path(container_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let manager = state.harness_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Docker is not available".into(),
            }),
        )
    })?;

    let status = manager.get_status(&container_id).await.map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Container not found: {}", e),
            }),
        )
    })?;

    Ok(Json(HarnessStatusResponse {
        harness_id: container_id.clone(),
        status,
        container_id,
    }))
}

//    Start Harness

#[derive(Serialize)]
struct StartHarnessResponse {
    harness_id: String,
    status: HarnessStatus,
    started_at: DateTime<Utc>,
}

async fn start_harness(
    State(state): State<PlatformState>,
    Path(container_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let manager = state.harness_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Docker is not available".into(),
            }),
        )
    })?;

    info!(container_id = %container_id, "Starting harness container");

    manager.start(&container_id).await.map_err(|e| {
        error!("Failed to start container {}: {}", container_id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to start: {}", e),
            }),
        )
    })?;

    Ok(Json(StartHarnessResponse {
        harness_id: container_id,
        status: HarnessStatus::Running,
        started_at: Utc::now(),
    }))
}

//    Stop Harness

#[derive(Serialize)]
struct StopHarnessResponse {
    harness_id: String,
    status: HarnessStatus,
    stopped_at: DateTime<Utc>,
}

async fn stop_harness(
    State(state): State<PlatformState>,
    Path(container_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let manager = state.harness_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Docker is not available".into(),
            }),
        )
    })?;

    info!(container_id = %container_id, "Stopping harness container");

    manager.stop(&container_id).await.map_err(|e| {
        error!("Failed to stop container {}: {}", container_id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to stop: {}", e),
            }),
        )
    })?;

    Ok(Json(StopHarnessResponse {
        harness_id: container_id,
        status: HarnessStatus::Stopped,
        stopped_at: Utc::now(),
    }))
}

//    Deprovision Harness

#[derive(Serialize)]
struct DeprovisionHarnessResponse {
    harness_id: String,
    status: HarnessStatus,
    deprovisioned_at: DateTime<Utc>,
}

async fn deprovision_harness(
    State(state): State<PlatformState>,
    Path(container_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let manager = state.harness_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Docker is not available".into(),
            }),
        )
    })?;

    info!(container_id = %container_id, "Deprovisioning harness container");

    manager.deprovision(&container_id).await.map_err(|e| {
        error!("Failed to deprovision container {}: {}", container_id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to deprovision: {}", e),
            }),
        )
    })?;

    Ok(Json(DeprovisionHarnessResponse {
        harness_id: container_id,
        status: HarnessStatus::Deprovisioned,
        deprovisioned_at: Utc::now(),
    }))
}

//    Get Harness Logs

#[derive(Serialize)]
struct HarnessLogsResponse {
    harness_id: String,
    logs: Vec<String>,
}

async fn get_harness_logs(
    State(state): State<PlatformState>,
    Path(container_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let manager = state.harness_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Docker is not available".into(),
            }),
        )
    })?;

    let logs = manager.get_logs(&container_id).await.map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Failed to get logs: {}", e),
            }),
        )
    })?;

    Ok(Json(HarnessLogsResponse {
        harness_id: container_id,
        logs,
    }))
}
