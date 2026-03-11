//! HTTP middleware for authentication and error handling.

use amos_core::AmosError;
use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use secrecy::ExposeSecret;
use serde::Serialize;
use tracing::error;
use uuid::Uuid;

use crate::auth::{self, Claims};
use crate::state::PlatformState;

/// Global error handler middleware.
///
/// Maps AmosError variants to appropriate HTTP status codes and JSON responses.
pub async fn error_handler(
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    next.run(req).await
}

/// JSON error response format.
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
    }
}

pub fn amos_error_to_response(err: &AmosError) -> (StatusCode, Json<ErrorResponse>) {
    let status = StatusCode::from_u16(err.status_code())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let code = match &err {
        AmosError::Unauthorized(_) => "unauthorized",
        AmosError::Forbidden(_) => "forbidden",
        AmosError::NotFound { .. } => "not_found",
        AmosError::Validation(_) => "validation_error",
        AmosError::Database(_) => "database_error",
        AmosError::SolanaRpc(_) => "solana_error",
        AmosError::InsufficientStake { .. } => "insufficient_stake",
        AmosError::NoRevenueToClaim => "no_revenue",
        _ => "internal_error",
    };

    let response = ErrorResponse {
        error: err.to_string(),
        code: code.to_string(),
        details: None,
    };

    (status, Json(response))
}

/// Authentication middleware using JWT or API keys.
///
/// This middleware protects API routes by validating either:
/// 1. JWT access tokens (primary method)
/// 2. API keys (fallback for programmatic access)
///
/// Valid credentials result in Claims being injected into request extensions.
pub async fn require_api_key(
    State(state): State<PlatformState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Extract Authorization header
    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok());

    let token = match auth_header {
        Some(auth) if auth.starts_with("Bearer ") => &auth[7..],
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Missing or invalid Authorization header".into(),
                    code: "unauthorized".into(),
                    details: Some("Provide 'Authorization: Bearer <token>' header".into()),
                }),
            ).into_response();
        }
    };

    // Try JWT validation first
    let jwt_secret = state.config.auth.jwt_secret.expose_secret();
    match auth::validate_access_token(token, jwt_secret) {
        Ok(claims) => {
            // JWT is valid, inject claims and proceed
            req.extensions_mut().insert(claims);
            return next.run(req).await;
        }
        Err(e) => {
            // JWT validation failed, log and try API key
            error!("JWT validation failed: {}, attempting API key validation", e);
        }
    }

    // JWT failed, try API key validation
    let key_hash = auth::hash_token(token);

    let api_key_result = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT id, tenant_id, created_by FROM api_keys
         WHERE key_hash = $1 AND is_active = TRUE
         AND (expires_at IS NULL OR expires_at > NOW())"
    )
    .bind(&key_hash)
    .fetch_optional(&state.db)
    .await;

    match api_key_result {
        Ok(Some((api_key_id, tenant_id, created_by_user_id))) => {
            // API key is valid, need to fetch user role and tenant slug to create synthetic Claims
            let user_result = sqlx::query_as::<_, (String, String)>(
                "SELECT u.role, t.slug FROM users u
                 JOIN tenants t ON u.tenant_id = t.id
                 WHERE u.id = $1"
            )
            .bind(&created_by_user_id)
            .fetch_optional(&state.db)
            .await;

            match user_result {
                Ok(Some((role, tenant_slug))) => {
                    // Update last_used_at for the API key (fire and forget)
                    let db_clone = state.db.clone();
                    let key_id = api_key_id;
                    tokio::spawn(async move {
                        let _ = sqlx::query(
                            "UPDATE api_keys SET last_used_at = NOW() WHERE id = $1"
                        )
                        .bind(key_id)
                        .execute(&db_clone)
                        .await;
                    });

                    // Create synthetic Claims for API key
                    let claims = Claims {
                        sub: created_by_user_id.to_string(),
                        tenant_id: tenant_id.to_string(),
                        role,
                        tenant_slug,
                        iat: chrono::Utc::now().timestamp(),
                        exp: chrono::Utc::now().timestamp() + 3600, // synthetic expiry
                    };

                    req.extensions_mut().insert(claims);
                    return next.run(req).await;
                }
                Ok(None) => {
                    error!("API key references non-existent user: {}", created_by_user_id);
                }
                Err(e) => {
                    error!("Database error fetching user for API key: {}", e);
                }
            }
        }
        Ok(None) => {
            error!("API key not found or expired");
        }
        Err(e) => {
            error!("Database error validating API key: {}", e);
        }
    }

    // Both JWT and API key validation failed
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            error: "Invalid or expired credentials".into(),
            code: "unauthorized".into(),
            details: Some("Provide a valid JWT access token or API key".into()),
        }),
    ).into_response()
}

/// Admin authentication middleware.
///
/// Requires that the user has admin or owner role.
/// Must be chained after `require_api_key` which injects Claims.
pub async fn require_admin(
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Extract Claims from request extensions (inserted by require_api_key)
    let claims = match req.extensions().get::<Claims>() {
        Some(c) => c.clone(),
        None => {
            error!("require_admin called without Claims in extensions");
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Authentication required".into(),
                    code: "unauthorized".into(),
                    details: Some("Missing authentication credentials".into()),
                }),
            ).into_response();
        }
    };

    // Check if user has admin or owner role
    if claims.role != "admin" && claims.role != "owner" {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Insufficient permissions".into(),
                code: "forbidden".into(),
                details: Some(format!("Requires 'admin' or 'owner' role, but user has '{}' role", claims.role)),
            }),
        ).into_response();
    }

    // User is admin/owner, proceed
    next.run(req).await
}

/// Harness service authentication.
///
/// Validates that the request comes from a legitimate harness container.
/// Harness containers receive their own JWTs and use the X-Harness-Token header.
pub async fn require_harness_auth(
    State(state): State<PlatformState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Extract X-Harness-Token header
    let auth_header = req
        .headers()
        .get("X-Harness-Token")
        .and_then(|h| h.to_str().ok());

    let token = match auth_header {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Missing harness authentication token".into(),
                    code: "unauthorized".into(),
                    details: Some("Provide 'X-Harness-Token' header".into()),
                }),
            ).into_response();
        }
    };

    // Validate token as JWT
    let jwt_secret = state.config.auth.jwt_secret.expose_secret();
    let claims = match auth::validate_access_token(token, jwt_secret) {
        Ok(c) => c,
        Err(e) => {
            error!("Harness token validation failed: {}", e);
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid or expired harness token".into(),
                    code: "unauthorized".into(),
                    details: Some(format!("Token validation error: {}", e)),
                }),
            ).into_response();
        }
    };

    // Verify that the tenant_id in the token corresponds to a valid harness instance
    let tenant_id = match Uuid::parse_str(&claims.tenant_id) {
        Ok(id) => id,
        Err(e) => {
            error!("Invalid tenant_id in harness token: {}", e);
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid tenant_id in token".into(),
                    code: "unauthorized".into(),
                    details: None,
                }),
            ).into_response();
        }
    };

    let harness_check = sqlx::query_as::<_, (i64,)>(
        "SELECT 1 FROM harness_instances
         WHERE tenant_id = $1 AND status != 'deprovisioned'"
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await;

    match harness_check {
        Ok(Some(_)) => {
            // Harness instance exists and is not deprovisioned
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Ok(None) => {
            error!("Harness token references non-existent or deprovisioned harness for tenant {}", tenant_id);
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Harness instance not found or deprovisioned".into(),
                    code: "unauthorized".into(),
                    details: None,
                }),
            ).into_response()
        }
        Err(e) => {
            error!("Database error verifying harness instance: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Database error".into(),
                    code: "internal_error".into(),
                    details: None,
                }),
            ).into_response()
        }
    }
}
