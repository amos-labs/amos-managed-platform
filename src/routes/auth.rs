//! Authentication API endpoints: register, login, refresh, logout.
//!
//! All endpoints return structured JSON with clear error messages for agent consumption.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    auth::{self, TokenPair},
    state::PlatformState,
};

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/refresh", post(refresh))
        .route("/auth/logout", post(logout))
}

// ── Shared error response ───────────────────────────────────────────────

#[derive(Serialize)]
struct AuthError {
    error: String,
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

// ── Register ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterRequest {
    /// Organization / tenant name.
    organization_name: String,
    /// User's email address.
    email: String,
    /// User's display name.
    name: String,
    /// Password (min 8 characters).
    password: String,
    /// Desired plan: free, starter, growth, enterprise.
    #[serde(default = "default_plan")]
    plan: String,
}

fn default_plan() -> String {
    "free".into()
}

#[derive(Serialize)]
struct RegisterResponse {
    tenant_id: Uuid,
    user_id: Uuid,
    slug: String,
    subdomain: Option<String>,
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
    expires_in: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    harness_id: Option<Uuid>,
    message: String,
}

async fn register(
    State(state): State<PlatformState>,
    Json(req): Json<RegisterRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<AuthError>)> {
    // ── Validation ──────────────────────────────────────────────────
    if req.organization_name.trim().is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(AuthError {
                error: "Organization name is required.".into(),
                code: "validation_error",
                field: Some("organization_name"),
                hint: Some("Provide a non-empty organization name (e.g. 'Acme Corp').".into()),
            }),
        ));
    }
    if req.email.trim().is_empty() || !req.email.contains('@') {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(AuthError {
                error: "A valid email address is required.".into(),
                code: "validation_error",
                field: Some("email"),
                hint: Some("Provide a valid email (e.g. 'user@example.com').".into()),
            }),
        ));
    }
    if req.password.len() < 8 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(AuthError {
                error: "Password must be at least 8 characters.".into(),
                code: "validation_error",
                field: Some("password"),
                hint: Some("Choose a password with at least 8 characters.".into()),
            }),
        ));
    }

    let valid_plans = ["free", "starter", "growth", "enterprise"];
    if !valid_plans.contains(&req.plan.as_str()) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(AuthError {
                error: format!("Invalid plan '{}'. Valid plans: {:?}.", req.plan, valid_plans),
                code: "validation_error",
                field: Some("plan"),
                hint: Some("Use one of: free, starter, growth, enterprise.".into()),
            }),
        ));
    }

    // ── Generate slug and subdomain ─────────────────────────────────
    let slug = auth::slugify(&req.organization_name);
    if slug.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(AuthError {
                error: "Organization name must contain at least one alphanumeric character.".into(),
                code: "validation_error",
                field: Some("organization_name"),
                hint: None,
            }),
        ));
    }

    // For managed plans, the slug doubles as subdomain
    let subdomain = Some(slug.clone());

    // ── Hash password ───────────────────────────────────────────────
    let password_hash = auth::hash_password(&req.password).map_err(|e| {
        error!("Password hashing failed: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Internal error during registration.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    // ── Create tenant ───────────────────────────────────────────────
    let tenant_id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO tenants (id, name, slug, plan, subdomain) VALUES ($1, $2, $3, $4, $5)"
    )
    .bind(tenant_id)
    .bind(&req.organization_name)
    .bind(&slug)
    .bind(&req.plan)
    .bind(&subdomain)
    .execute(&state.db)
    .await;

    if let Err(e) = result {
        let err_str = e.to_string();
        if err_str.contains("tenants_slug_key") || err_str.contains("tenants_subdomain_key") {
            return Err((
                StatusCode::CONFLICT,
                Json(AuthError {
                    error: format!(
                        "Organization slug '{}' is already taken. Choose a different name.",
                        slug
                    ),
                    code: "slug_conflict",
                    field: Some("organization_name"),
                    hint: Some(format!(
                        "Try a variation: '{}-2' or '{}-team'.",
                        req.organization_name, req.organization_name
                    )),
                }),
            ));
        }
        error!("Failed to create tenant: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Failed to create organization.".into(),
                code: "database_error",
                field: None,
                hint: None,
            }),
        ));
    }

    // ── Create user (owner) ─────────────────────────────────────────
    let user_id = Uuid::new_v4();
    let user_result = sqlx::query(
        "INSERT INTO users (id, tenant_id, email, name, password_hash, role, email_verified)
         VALUES ($1, $2, $3, $4, $5, 'owner', TRUE)"
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(&req.email)
    .bind(&req.name)
    .bind(&password_hash)
    .execute(&state.db)
    .await;

    if let Err(e) = user_result {
        let err_str = e.to_string();
        if err_str.contains("users_tenant_id_email_key") {
            // Rollback tenant
            let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
                .bind(tenant_id)
                .execute(&state.db)
                .await;
            return Err((
                StatusCode::CONFLICT,
                Json(AuthError {
                    error: format!("Email '{}' is already registered for this organization.", req.email),
                    code: "email_conflict",
                    field: Some("email"),
                    hint: Some("Use POST /api/v1/auth/login to sign in, or use a different email.".into()),
                }),
            ));
        }
        // Rollback tenant
        let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .execute(&state.db)
            .await;
        error!("Failed to create user: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Failed to create user account.".into(),
                code: "database_error",
                field: None,
                hint: None,
            }),
        ));
    }

    // ── Create harness instance record ──────────────────────────────
    let harness_id = Uuid::new_v4();
    let _ = sqlx::query(
        "INSERT INTO harness_instances (id, tenant_id, subdomain, status)
         VALUES ($1, $2, $3, 'pending')"
    )
    .bind(harness_id)
    .bind(tenant_id)
    .bind(&subdomain)
    .execute(&state.db)
    .await;

    // ── Auto-provision harness (if Docker available) ────────────────
    if let Some(ref manager) = state.harness_manager {
        let config = crate::provisioning::HarnessConfig {
            customer_id: tenant_id,
            region: "us-west-2".into(),
            instance_size: crate::provisioning::InstanceSize::Small,
            environment: "production".into(),
            platform_grpc_url: format!(
                "http://{}:{}",
                state.config.server.host, state.config.server.port
            ),
            env_vars: std::collections::HashMap::new(),
        };

        match manager.provision(&config).await {
            Ok(container_id) => {
                info!(
                    tenant_id = %tenant_id,
                    container_id = %container_id,
                    "Auto-provisioned harness container"
                );

                // Update harness record with container info
                let _ = sqlx::query(
                    "UPDATE harness_instances SET container_id = $1, status = 'provisioning', provisioned_at = NOW()
                     WHERE id = $2"
                )
                .bind(&container_id)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                // Auto-start
                if let Ok(()) = manager.start(&container_id).await {
                    let _ = sqlx::query(
                        "UPDATE harness_instances SET status = 'running', started_at = NOW() WHERE id = $1"
                    )
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                }
            }
            Err(e) => {
                warn!(
                    tenant_id = %tenant_id,
                    error = %e,
                    "Auto-provisioning failed (Docker may not be available). Harness left in pending state."
                );
            }
        }
    }

    // ── Issue tokens ────────────────────────────────────────────────
    let jwt_secret = get_jwt_secret(&state);
    let access_expiry = state.config.auth.access_token_expiry_secs as i64;

    let access_token = auth::create_access_token(
        user_id, tenant_id, "owner", &slug, &jwt_secret, access_expiry,
    )
    .map_err(|e| {
        error!("Token creation failed: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Failed to create authentication token.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    let refresh_token = auth::create_refresh_token();
    let refresh_hash = auth::hash_token(&refresh_token);
    let refresh_expiry = state.config.auth.refresh_token_expiry_secs as i64;

    // Store refresh token hash
    let _ = sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)"
    )
    .bind(user_id)
    .bind(&refresh_hash)
    .bind(Utc::now() + Duration::seconds(refresh_expiry))
    .execute(&state.db)
    .await;

    info!(
        tenant_id = %tenant_id,
        user_id = %user_id,
        slug = %slug,
        plan = %req.plan,
        "New tenant registered"
    );

    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            tenant_id,
            user_id,
            slug: slug.clone(),
            subdomain,
            access_token,
            refresh_token,
            token_type: "Bearer",
            expires_in: access_expiry,
            harness_id: Some(harness_id),
            message: format!(
                "Registration complete. Your harness will be available at {}.amos.ai once provisioned.",
                slug
            ),
        }),
    ))
}

// ── Login ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    user_id: Uuid,
    tenant_id: Uuid,
    tenant_slug: String,
    role: String,
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
    expires_in: i64,
}

async fn login(
    State(state): State<PlatformState>,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<AuthError>)> {
    // Look up user by email (across all tenants)
    let row = sqlx::query_as::<_, (Uuid, Uuid, String, String, String, bool)>(
        "SELECT u.id, u.tenant_id, u.password_hash, u.role, t.slug, u.is_active
         FROM users u JOIN tenants t ON u.tenant_id = t.id
         WHERE u.email = $1
         LIMIT 1"
    )
    .bind(&req.email)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        error!("Login query failed: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Internal error during login.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    let (user_id, tenant_id, password_hash, role, tenant_slug, is_active) = match row {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(AuthError {
                    error: "Invalid email or password.".into(),
                    code: "invalid_credentials",
                    field: None,
                    hint: Some("Check your email and password. Use POST /api/v1/auth/register to create an account.".into()),
                }),
            ));
        }
    };

    if !is_active {
        return Err((
            StatusCode::FORBIDDEN,
            Json(AuthError {
                error: "Account is deactivated. Contact your organization admin.".into(),
                code: "account_disabled",
                field: None,
                hint: None,
            }),
        ));
    }

    // Verify password
    let valid = auth::verify_password(&req.password, &password_hash).map_err(|e| {
        error!("Password verification error: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Internal error during login.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    if !valid {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(AuthError {
                error: "Invalid email or password.".into(),
                code: "invalid_credentials",
                field: None,
                hint: Some("Check your email and password. Use POST /api/v1/auth/register to create an account.".into()),
            }),
        ));
    }

    // Update last_login_at
    let _ = sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await;

    // Issue tokens
    let jwt_secret = get_jwt_secret(&state);
    let access_expiry = state.config.auth.access_token_expiry_secs as i64;
    let refresh_expiry = state.config.auth.refresh_token_expiry_secs as i64;

    let access_token = auth::create_access_token(
        user_id, tenant_id, &role, &tenant_slug, &jwt_secret, access_expiry,
    )
    .map_err(|e| {
        error!("Token creation failed: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Failed to create authentication token.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    let refresh_token = auth::create_refresh_token();
    let refresh_hash = auth::hash_token(&refresh_token);

    let _ = sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)"
    )
    .bind(user_id)
    .bind(&refresh_hash)
    .bind(Utc::now() + Duration::seconds(refresh_expiry))
    .execute(&state.db)
    .await;

    info!(user_id = %user_id, tenant_slug = %tenant_slug, "User logged in");

    Ok(Json(LoginResponse {
        user_id,
        tenant_id,
        tenant_slug,
        role,
        access_token,
        refresh_token,
        token_type: "Bearer",
        expires_in: access_expiry,
    }))
}

// ── Refresh ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

async fn refresh(
    State(state): State<PlatformState>,
    Json(req): Json<RefreshRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<AuthError>)> {
    let token_hash = auth::hash_token(&req.refresh_token);

    // Look up the refresh token
    let row = sqlx::query_as::<_, (Uuid, Uuid, bool)>(
        "SELECT rt.user_id, rt.id, rt.revoked
         FROM refresh_tokens rt
         WHERE rt.token_hash = $1 AND rt.expires_at > NOW()
         LIMIT 1"
    )
    .bind(&token_hash)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        error!("Refresh token lookup failed: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Internal error during token refresh.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    let (user_id, token_id, revoked) = match row {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(AuthError {
                    error: "Refresh token is invalid or expired.".into(),
                    code: "invalid_refresh_token",
                    field: Some("refresh_token"),
                    hint: Some("Use POST /api/v1/auth/login to obtain a new token pair.".into()),
                }),
            ));
        }
    };

    if revoked {
        // Token reuse detected - revoke ALL tokens for this user (security measure)
        warn!(user_id = %user_id, "Refresh token reuse detected, revoking all sessions");
        let _ = sqlx::query("UPDATE refresh_tokens SET revoked = TRUE WHERE user_id = $1")
            .bind(user_id)
            .execute(&state.db)
            .await;

        return Err((
            StatusCode::UNAUTHORIZED,
            Json(AuthError {
                error: "Refresh token has been revoked. All sessions have been invalidated for security.".into(),
                code: "token_revoked",
                field: None,
                hint: Some("Use POST /api/v1/auth/login to sign in again.".into()),
            }),
        ));
    }

    // Revoke the old refresh token (rotation)
    let _ = sqlx::query("UPDATE refresh_tokens SET revoked = TRUE WHERE id = $1")
        .bind(token_id)
        .execute(&state.db)
        .await;

    // Look up user details for new token
    let user_row = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT u.tenant_id, u.role, t.slug
         FROM users u JOIN tenants t ON u.tenant_id = t.id
         WHERE u.id = $1 AND u.is_active = TRUE"
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        error!("User lookup for refresh failed: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Internal error during token refresh.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    let (tenant_id, role, tenant_slug) = match user_row {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(AuthError {
                    error: "User account is no longer active.".into(),
                    code: "account_disabled",
                    field: None,
                    hint: None,
                }),
            ));
        }
    };

    // Issue new token pair
    let jwt_secret = get_jwt_secret(&state);
    let access_expiry = state.config.auth.access_token_expiry_secs as i64;
    let refresh_expiry = state.config.auth.refresh_token_expiry_secs as i64;

    let access_token = auth::create_access_token(
        user_id, tenant_id, &role, &tenant_slug, &jwt_secret, access_expiry,
    )
    .map_err(|e| {
        error!("Token creation failed: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthError {
                error: "Failed to create authentication token.".into(),
                code: "internal_error",
                field: None,
                hint: None,
            }),
        )
    })?;

    let new_refresh_token = auth::create_refresh_token();
    let new_refresh_hash = auth::hash_token(&new_refresh_token);

    let _ = sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)"
    )
    .bind(user_id)
    .bind(&new_refresh_hash)
    .bind(Utc::now() + Duration::seconds(refresh_expiry))
    .execute(&state.db)
    .await;

    Ok(Json(LoginResponse {
        user_id,
        tenant_id,
        tenant_slug,
        role,
        access_token,
        refresh_token: new_refresh_token,
        token_type: "Bearer",
        expires_in: access_expiry,
    }))
}

// ── Logout ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LogoutRequest {
    refresh_token: String,
}

#[derive(Serialize)]
struct LogoutResponse {
    message: &'static str,
}

async fn logout(
    State(state): State<PlatformState>,
    Json(req): Json<LogoutRequest>,
) -> impl IntoResponse {
    let token_hash = auth::hash_token(&req.refresh_token);

    // Revoke the token
    let _ = sqlx::query("UPDATE refresh_tokens SET revoked = TRUE WHERE token_hash = $1")
        .bind(&token_hash)
        .execute(&state.db)
        .await;

    Json(LogoutResponse {
        message: "Logged out successfully. The refresh token has been revoked.",
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn get_jwt_secret(state: &PlatformState) -> String {
    use secrecy::ExposeSecret;
    state
        .config
        .auth
        .jwt_secret
        .expose_secret()
        .to_string()
}
