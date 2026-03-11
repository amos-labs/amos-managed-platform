//! Tenant management API endpoints.
//!
//! These endpoints are protected by JWT auth and scoped to the authenticated tenant.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use tracing::error;
use uuid::Uuid;

use crate::{auth, middleware, state::PlatformState};

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/tenants/me", get(get_current_tenant))
        .route("/tenants/me/users", get(list_tenant_users))
        .route("/tenants/me/harness", get(get_tenant_harness))
        .route("/tenants/me/api-keys", get(list_api_keys).post(create_api_key))
}

// ── Shared error response ───────────────────────────────────────────────

#[derive(Serialize)]
struct TenantError {
    error: String,
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

// ── Get Current Tenant ──────────────────────────────────────────────────

#[derive(Serialize)]
struct TenantResponse {
    id: Uuid,
    name: String,
    slug: String,
    plan: String,
    deployment_mode: String,
    subdomain: Option<String>,
    created_at: String,
}

async fn get_current_tenant(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<TenantError>)> {
    let claims = extract_claims(&state, &headers)?;
    let tenant_id: Uuid = claims.tenant_id.parse().map_err(|_| auth_error())?;

    let row = sqlx::query_as::<_, (Uuid, String, String, String, String, Option<String>, String)>(
        "SELECT id, name, slug, plan, deployment_mode, subdomain, created_at::text
         FROM tenants WHERE id = $1"
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        error!("Tenant query failed: {}", e);
        db_error()
    })?;

    match row {
        Some((id, name, slug, plan, deployment_mode, subdomain, created_at)) => {
            Ok(Json(TenantResponse {
                id, name, slug, plan, deployment_mode, subdomain, created_at,
            }))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(TenantError {
                error: "Tenant not found.".into(),
                code: "not_found",
                hint: None,
            }),
        )),
    }
}

// ── List Tenant Users ───────────────────────────────────────────────────

#[derive(Serialize)]
struct UserResponse {
    id: Uuid,
    email: String,
    name: Option<String>,
    role: String,
    is_active: bool,
    last_login_at: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
struct UsersListResponse {
    users: Vec<UserResponse>,
    total: i64,
}

async fn list_tenant_users(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<TenantError>)> {
    let claims = extract_claims(&state, &headers)?;
    let tenant_id: Uuid = claims.tenant_id.parse().map_err(|_| auth_error())?;

    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>, String, bool, Option<String>, String)>(
        "SELECT id, email, name, role, is_active, last_login_at::text, created_at::text
         FROM users WHERE tenant_id = $1 ORDER BY created_at ASC"
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        error!("Users query failed: {}", e);
        db_error()
    })?;

    let total = rows.len() as i64;
    let users: Vec<UserResponse> = rows
        .into_iter()
        .map(|(id, email, name, role, is_active, last_login_at, created_at)| UserResponse {
            id, email, name, role, is_active, last_login_at, created_at,
        })
        .collect();

    Ok(Json(UsersListResponse { users, total }))
}

// ── Get Tenant Harness ──────────────────────────────────────────────────

#[derive(Serialize)]
struct HarnessResponse {
    id: Uuid,
    status: String,
    subdomain: Option<String>,
    region: String,
    instance_size: String,
    harness_version: Option<String>,
    healthy: bool,
    last_heartbeat: Option<String>,
    provisioned_at: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
struct HarnessListResponse {
    instances: Vec<HarnessResponse>,
    total: i64,
}

async fn get_tenant_harness(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<TenantError>)> {
    let claims = extract_claims(&state, &headers)?;
    let tenant_id: Uuid = claims.tenant_id.parse().map_err(|_| auth_error())?;

    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>, String, String, Option<String>, bool, Option<String>, Option<String>, String)>(
        "SELECT id, status, subdomain, region, instance_size, harness_version, healthy,
                last_heartbeat::text, provisioned_at::text, created_at::text
         FROM harness_instances WHERE tenant_id = $1 ORDER BY created_at DESC"
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        error!("Harness query failed: {}", e);
        db_error()
    })?;

    let total = rows.len() as i64;
    let instances: Vec<HarnessResponse> = rows
        .into_iter()
        .map(|(id, status, subdomain, region, instance_size, harness_version, healthy, last_heartbeat, provisioned_at, created_at)| {
            HarnessResponse {
                id, status, subdomain, region, instance_size, harness_version, healthy, last_heartbeat, provisioned_at, created_at,
            }
        })
        .collect();

    Ok(Json(HarnessListResponse { instances, total }))
}

// ── API Key Management ──────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiKeyResponse {
    id: Uuid,
    name: String,
    key_prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_key: Option<String>, // Only set on creation
    scopes: Vec<String>,
    is_active: bool,
    last_used_at: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
struct ApiKeysListResponse {
    api_keys: Vec<ApiKeyResponse>,
    total: i64,
}

async fn list_api_keys(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<TenantError>)> {
    let claims = extract_claims(&state, &headers)?;
    let tenant_id: Uuid = claims.tenant_id.parse().map_err(|_| auth_error())?;

    let rows = sqlx::query_as::<_, (Uuid, String, String, Vec<String>, bool, Option<String>, String)>(
        "SELECT id, name, key_prefix, scopes, is_active, last_used_at::text, created_at::text
         FROM api_keys WHERE tenant_id = $1 ORDER BY created_at DESC"
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        error!("API keys query failed: {}", e);
        db_error()
    })?;

    let total = rows.len() as i64;
    let api_keys: Vec<ApiKeyResponse> = rows
        .into_iter()
        .map(|(id, name, key_prefix, scopes, is_active, last_used_at, created_at)| ApiKeyResponse {
            id, name, key_prefix, full_key: None, scopes, is_active, last_used_at, created_at,
        })
        .collect();

    Ok(Json(ApiKeysListResponse { api_keys, total }))
}

#[derive(serde::Deserialize)]
struct CreateApiKeyRequest {
    name: String,
    #[serde(default)]
    scopes: Vec<String>,
}

async fn create_api_key(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<TenantError>)> {
    let claims = extract_claims(&state, &headers)?;
    let tenant_id: Uuid = claims.tenant_id.parse().map_err(|_| auth_error())?;
    let user_id: Uuid = claims.sub.parse().map_err(|_| auth_error())?;

    // Only owner/admin can create API keys
    if claims.role != "owner" && claims.role != "admin" {
        return Err((
            StatusCode::FORBIDDEN,
            Json(TenantError {
                error: "Only owner or admin can create API keys.".into(),
                code: "forbidden",
                hint: Some("Contact your organization admin to create API keys.".into()),
            }),
        ));
    }

    if req.name.trim().is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(TenantError {
                error: "API key name is required.".into(),
                code: "validation_error",
                hint: Some("Provide a descriptive name (e.g. 'Production API Key').".into()),
            }),
        ));
    }

    let (full_key, prefix, key_hash) = auth::generate_api_key();
    let key_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO api_keys (id, tenant_id, created_by, name, key_prefix, key_hash, scopes)
         VALUES ($1, $2, $3, $4, $5, $6, $7)"
    )
    .bind(key_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(&req.name)
    .bind(&prefix)
    .bind(&key_hash)
    .bind(&req.scopes)
    .execute(&state.db)
    .await
    .map_err(|e| {
        error!("Failed to create API key: {}", e);
        db_error()
    })?;

    Ok((
        StatusCode::CREATED,
        Json(ApiKeyResponse {
            id: key_id,
            name: req.name,
            key_prefix: prefix,
            full_key: Some(full_key), // Only shown once!
            scopes: req.scopes,
            is_active: true,
            last_used_at: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        }),
    ))
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn extract_claims(
    state: &PlatformState,
    headers: &axum::http::HeaderMap,
) -> Result<auth::Claims, (StatusCode, Json<TenantError>)> {
    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| (
            StatusCode::UNAUTHORIZED,
            Json(TenantError {
                error: "Missing or malformed Authorization header.".into(),
                code: "unauthorized",
                hint: Some("Provide 'Authorization: Bearer <access_token>' header. Get a token from POST /api/v1/auth/login.".into()),
            }),
        ))?;

    let jwt_secret = {
        use secrecy::ExposeSecret;
        state.config.auth.jwt_secret.expose_secret().to_string()
    };

    auth::validate_access_token(auth_header, &jwt_secret).map_err(|e| (
        StatusCode::UNAUTHORIZED,
        Json(TenantError {
            error: format!("Authentication failed: {}", e),
            code: "unauthorized",
            hint: Some("Token may be expired. Use POST /api/v1/auth/refresh to get a new token.".into()),
        }),
    ))
}

fn auth_error() -> (StatusCode, Json<TenantError>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(TenantError {
            error: "Invalid authentication token.".into(),
            code: "unauthorized",
            hint: Some("Use POST /api/v1/auth/login to get a valid token.".into()),
        }),
    )
}

fn db_error() -> (StatusCode, Json<TenantError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(TenantError {
            error: "Database error.".into(),
            code: "internal_error",
            hint: None,
        }),
    )
}
