//! REST API for multi-service app deployments.
//!
//! A thin mirror of the MCP app tools — both drive the shared
//! [`crate::services::app_orchestrator`], so there is no duplicated
//! provision/DB logic. Auth reuses the same JWT/API-key path as the MCP
//! control plane ([`crate::mcp::authenticate`]); deployments are scoped to the
//! caller's tenant.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use amos_core::AmosError;

use crate::services::app_orchestrator;
use crate::state::PlatformState;

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/apps", get(list_apps).post(deploy_app))
        .route("/apps/{id}", get(app_status).delete(deprovision_app))
        .route("/apps/{id}/services/{name}/logs", get(app_logs))
        .route("/apps/{id}/control", post(app_control))
}

/// Resolve the caller's tenant and actor (subject) from the bearer token, or
/// 401. The actor is recorded on proof receipts for write operations.
async fn tenant_of(
    state: &PlatformState,
    headers: &HeaderMap,
) -> Result<(Uuid, String), (StatusCode, Json<serde_json::Value>)> {
    let claims = crate::mcp::authenticate(state, headers)
        .await
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing or invalid bearer token"})),
            )
        })?;
    let tenant_id = Uuid::parse_str(&claims.tenant_id).map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid tenant in credentials"})),
        )
    })?;
    Ok((tenant_id, claims.sub))
}

/// Map an orchestrator error to an HTTP status + JSON body.
fn err_response(e: AmosError) -> (StatusCode, Json<serde_json::Value>) {
    let status = StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({ "error": e.to_string() })))
}

#[derive(Deserialize)]
struct DeployRequest {
    compose_yaml: Option<String>,
    compose_dir: Option<String>,
    spec: Option<serde_json::Value>,
}

async fn deploy_app(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Json(req): Json<DeployRequest>,
) -> impl IntoResponse {
    let (tenant_id, actor) = match tenant_of(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    let result = if let Some(yaml) = req.compose_yaml {
        let dir = req.compose_dir.unwrap_or_else(|| ".".to_string());
        app_orchestrator::deploy_from_compose(
            &state,
            tenant_id,
            &actor,
            &yaml,
            std::path::Path::new(&dir),
        )
        .await
    } else if let Some(spec_val) = req.spec {
        match serde_json::from_value(spec_val) {
            Ok(spec) => app_orchestrator::deploy_app(&state, tenant_id, &actor, spec).await,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error": format!("invalid app spec: {e}")})),
                )
                    .into_response()
            }
        }
    } else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "provide either 'compose_yaml' or 'spec'"})),
        )
            .into_response();
    };

    match result {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(e) => err_response(e).into_response(),
    }
}

async fn list_apps(State(state): State<PlatformState>, headers: HeaderMap) -> impl IntoResponse {
    let (tenant_id, _) = match tenant_of(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    match app_orchestrator::list_apps(&state, tenant_id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e).into_response(),
    }
}

async fn app_status(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let (tenant_id, _) = match tenant_of(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    match app_orchestrator::app_status(&state, tenant_id, id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e).into_response(),
    }
}

async fn app_logs(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Path((id, name)): Path<(Uuid, String)>,
) -> impl IntoResponse {
    let (tenant_id, _) = match tenant_of(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    match app_orchestrator::app_logs(&state, tenant_id, id, &name, 200).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e).into_response(),
    }
}

#[derive(Deserialize)]
struct ControlRequest {
    action: String,
    service: Option<String>,
    #[serde(default)]
    destroy_data: bool,
}

async fn app_control(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<ControlRequest>,
) -> impl IntoResponse {
    let (tenant_id, _) = match tenant_of(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    match app_orchestrator::app_control(
        &state,
        tenant_id,
        id,
        &req.action,
        req.service.as_deref(),
        req.destroy_data,
    )
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e).into_response(),
    }
}

async fn deprovision_app(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let (tenant_id, _) = match tenant_of(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    match app_orchestrator::app_control(&state, tenant_id, id, "deprovision", None, false).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e).into_response(),
    }
}
