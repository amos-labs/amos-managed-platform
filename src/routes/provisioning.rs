//! Harness provisioning API endpoints.
//!
//! Routes try the Docker-based `HarnessManager` first (local dev).
//! When Docker is unavailable, they fall back to the `EcsProvisioner`
//! (production Fargate mode).

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
    /// Optional tenant slug for ECS task naming (defaults to customer_id).
    tenant_slug: Option<String>,
}

#[derive(Serialize)]
struct ProvisionHarnessResponse {
    harness_id: String,
    status: HarnessStatus,
    container_id: Option<String>,
    http_endpoint: String,
    provisioned_at: DateTime<Utc>,
    /// Which provisioner handled the request.
    provider: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

async fn provision_harness(
    State(state): State<PlatformState>,
    Json(req): Json<ProvisionHarnessRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
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

    // Try Docker first, then fall back to ECS.
    if let Some(manager) = state.harness_manager.as_ref() {
        let container_id = manager.provision(&config).await.map_err(|e| {
            error!("Failed to provision harness via Docker: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Docker provisioning failed: {}", e),
                }),
            )
        })?;

        // Auto-start the container after provisioning.
        if let Err(e) = manager.start(&container_id).await {
            error!("Failed to auto-start harness {}: {}", container_id, e);
        }

        let harness_id = format!("harness-{}", req.customer_id);
        let http_endpoint = format!("http://{}:3000", harness_id);

        info!(container_id = %container_id, harness_id = %harness_id, "Harness provisioned via Docker");

        Ok((
            StatusCode::CREATED,
            Json(ProvisionHarnessResponse {
                harness_id,
                status: HarnessStatus::Running,
                container_id: Some(container_id),
                http_endpoint,
                provisioned_at: Utc::now(),
                provider: "docker".into(),
            }),
        ))
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        let tenant_slug = req
            .tenant_slug
            .unwrap_or_else(|| req.customer_id.to_string());

        let task_arn = ecs.provision(&config, &tenant_slug).await.map_err(|e| {
            error!("Failed to provision harness via ECS: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("ECS provisioning failed: {}", e),
                }),
            )
        })?;

        let harness_id = format!("harness-{}", req.customer_id);

        info!(task_arn = %task_arn, harness_id = %harness_id, "Harness provisioned via ECS Fargate");

        Ok((
            StatusCode::CREATED,
            Json(ProvisionHarnessResponse {
                harness_id,
                status: HarnessStatus::Provisioning,
                container_id: Some(task_arn),
                http_endpoint: "pending (ECS task starting)".into(),
                provisioned_at: Utc::now(),
                provider: "ecs".into(),
            }),
        ))
    } else {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No provisioner available. Neither Docker nor ECS Fargate is configured."
                    .into(),
            }),
        ))
    }
}

//    Get Harness Status

#[derive(Serialize)]
struct HarnessStatusResponse {
    harness_id: String,
    status: HarnessStatus,
    container_id: String,
    /// Private IP of the ECS task (ECS only).
    #[serde(skip_serializing_if = "Option::is_none")]
    private_ip: Option<String>,
    provider: String,
}

async fn get_harness_status(
    State(state): State<PlatformState>,
    Path(container_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    // Try Docker first, then ECS.
    if let Some(manager) = state.harness_manager.as_ref() {
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
            private_ip: None,
            provider: "docker".into(),
        }))
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        let (status, private_ip) = ecs.describe_task(&container_id).await.map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("ECS task not found: {}", e),
                }),
            )
        })?;

        Ok(Json(HarnessStatusResponse {
            harness_id: container_id.clone(),
            status,
            container_id,
            private_ip,
            provider: "ecs".into(),
        }))
    } else {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No provisioner available".into(),
            }),
        ))
    }
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
    // Start is only supported for Docker containers.
    // ECS tasks auto-start when run_task is called.
    let manager = state.harness_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(ErrorResponse {
                error: "Start is not supported for ECS tasks (tasks auto-start on provision)"
                    .into(),
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
    info!(container_id = %container_id, "Stopping harness");

    if let Some(manager) = state.harness_manager.as_ref() {
        manager.stop(&container_id).await.map_err(|e| {
            error!("Failed to stop container {}: {}", container_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to stop: {}", e),
                }),
            )
        })?;
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        ecs.stop(&container_id).await.map_err(|e| {
            error!("Failed to stop ECS task {}: {}", container_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to stop ECS task: {}", e),
                }),
            )
        })?;
    } else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No provisioner available".into(),
            }),
        ));
    }

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
    info!(container_id = %container_id, "Deprovisioning harness");

    if let Some(manager) = state.harness_manager.as_ref() {
        manager.deprovision(&container_id).await.map_err(|e| {
            error!("Failed to deprovision container {}: {}", container_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to deprovision: {}", e),
                }),
            )
        })?;
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        // For ECS, "deprovision" means stop the task.
        ecs.stop(&container_id).await.map_err(|e| {
            error!("Failed to stop ECS task {}: {}", container_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to deprovision ECS task: {}", e),
                }),
            )
        })?;
    } else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No provisioner available".into(),
            }),
        ));
    }

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
    if let Some(manager) = state.harness_manager.as_ref() {
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
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        // ECS logs are in CloudWatch; return a hint.
        let hint = ecs.logs_hint(&container_id);
        Ok(Json(HarnessLogsResponse {
            harness_id: container_id,
            logs: vec![hint],
        }))
    } else {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No provisioner available".into(),
            }),
        ))
    }
}
