//! Server-side rendered UI routes using Askama templates.
//!
//! Pages: login, register, dashboard, settings.
//! Authentication is cookie-based (JWT stored in httponly cookie).

use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    auth,
    provisioning::{alb::AlbRouter, HarnessConfig, InstanceSize},
    state::PlatformState,
};

const SESSION_COOKIE: &str = "amos_session";

/// Simple in-memory IP rate limiter for auth endpoints.
static AUTH_RATE_LIMITER: std::sync::LazyLock<Mutex<HashMap<String, Vec<Instant>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns true if the IP has exceeded the rate limit (max requests per window).
fn is_rate_limited(ip: &str, max_requests: usize, window_secs: u64) -> bool {
    let now = Instant::now();
    let mut map = AUTH_RATE_LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    let entries = map.entry(ip.to_string()).or_default();
    entries.retain(|t| now.duration_since(*t).as_secs() < window_secs);
    if entries.len() >= max_requests {
        return true;
    }
    entries.push(now);
    false
}

fn extract_client_ip(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .unwrap_or("unknown")
        .trim()
        .to_string()
}

/// Truncate a version/image tag for display (e.g., "sha-f73ede4..." → "sha-f73ed...").
fn truncate_version(v: &str) -> String {
    if v.len() > 16 {
        format!("{}...", &v[..13])
    } else {
        v.to_string()
    }
}

// ── ECS Polling Helper ──────────────────────────────────────────────────

/// Spawn a background task that polls ECS every 5 s (up to 2 min) until
/// the harness task reaches RUNNING, then updates the `harness_instances`
/// row with `status = 'running'`, the private IP URL, and `healthy = TRUE`.
///
/// When an ALB router is available and a subdomain is set, this also creates
/// a target group + ALB listener rule so the harness is reachable at
/// `https://{subdomain}.custom.amoslabs.com`.
///
/// Called from: `register_submit`, `harness_redeploy`, `deploy_new_harness`.
/// Spawn a background poller for ECS task status. Public so the webhook handler can use it.
pub fn spawn_ecs_status_poller_public(
    ecs: std::sync::Arc<crate::provisioning::ecs::EcsProvisioner>,
    db: sqlx::PgPool,
    task_arn: String,
    harness_id: Uuid,
    alb_router: Option<std::sync::Arc<AlbRouter>>,
    subdomain: Option<String>,
) {
    spawn_ecs_status_poller(ecs, db, task_arn, harness_id, alb_router, subdomain);
}

fn spawn_ecs_status_poller(
    ecs: std::sync::Arc<crate::provisioning::ecs::EcsProvisioner>,
    db: sqlx::PgPool,
    task_arn: String,
    harness_id: Uuid,
    alb_router: Option<std::sync::Arc<AlbRouter>>,
    subdomain: Option<String>,
) {
    tokio::spawn(async move {
        for attempt in 1..=24 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

            match ecs.describe_task(&task_arn).await {
                Ok((crate::provisioning::HarnessStatus::Running, private_ip)) => {
                    let internal_url: Option<String> =
                        private_ip.as_ref().map(|ip| format!("http://{}:3000", ip));

                    // Set up ALB subdomain routing if we have both a router and a subdomain.
                    let mut tg_arn: Option<String> = None;
                    let mut rule_arn: Option<String> = None;

                    if let (Some(ref router), Some(ref sub), Some(ref ip)) =
                        (&alb_router, &subdomain, &private_ip)
                    {
                        match router.setup_routing(sub, ip, 3000).await {
                            Ok(result) => {
                                info!(
                                    subdomain = %sub,
                                    public_url = %result.public_url,
                                    "ALB routing configured for harness"
                                );
                                tg_arn = Some(result.target_group_arn);
                                rule_arn = Some(result.listener_rule_arn);
                            }
                            Err(e) => {
                                warn!(
                                    harness_id = %harness_id,
                                    subdomain = %sub,
                                    "Failed to set up ALB routing (harness still reachable via internal URL): {}",
                                    e
                                );
                            }
                        }
                    }

                    let _ = sqlx::query(
                        "UPDATE harness_instances
                         SET status = 'running',
                             started_at = NOW(),
                             internal_url = $1,
                             healthy = TRUE,
                             target_group_arn = $3,
                             listener_rule_arn = $4
                         WHERE id = $2",
                    )
                    .bind(&internal_url)
                    .bind(harness_id)
                    .bind(&tg_arn)
                    .bind(&rule_arn)
                    .execute(&db)
                    .await;

                    info!(
                        harness_id = %harness_id,
                        ip = ?private_ip,
                        attempt = attempt,
                        "ECS harness task is running"
                    );
                    return;
                }
                Ok((crate::provisioning::HarnessStatus::Error, _)) => {
                    let _ =
                        sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                            .bind(harness_id)
                            .execute(&db)
                            .await;

                    warn!(harness_id = %harness_id, "ECS harness task failed");
                    return;
                }
                Ok((status, _)) => {
                    info!(
                        attempt = attempt,
                        status = ?status,
                        "ECS harness task not yet running"
                    );
                }
                Err(e) => {
                    warn!(attempt = attempt, "Failed to describe ECS task: {}", e);
                }
            }
        }

        // Timed out after 2 minutes — mark as error
        warn!(harness_id = %harness_id, "ECS harness task timed out after 2 minutes");
        let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
            .bind(harness_id)
            .execute(&db)
            .await;
    });
}

/// Public wrapper for admin API access.
pub async fn teardown_alb_routing_public(state: &PlatformState, harness_id: Uuid) {
    teardown_alb_routing(state, harness_id).await;
}

/// Helper: tear down ALB routing for a harness instance (used by stop/delete).
async fn teardown_alb_routing(state: &PlatformState, harness_id: Uuid) {
    if let Some(ref router) = state.alb_router {
        // Fetch the stored ALB ARNs
        let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT target_group_arn, listener_rule_arn FROM harness_instances WHERE id = $1",
        )
        .bind(harness_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        if let Some((tg_arn, rule_arn)) = row {
            if tg_arn.is_some() || rule_arn.is_some() {
                router
                    .teardown_routing(rule_arn.as_deref(), tg_arn.as_deref())
                    .await;

                // Clear the ARNs in the database
                let _ = sqlx::query(
                    "UPDATE harness_instances SET target_group_arn = NULL, listener_rule_arn = NULL WHERE id = $1",
                )
                .bind(harness_id)
                .execute(&state.db)
                .await;

                info!(harness_id = %harness_id, "ALB routing torn down");
            }
        }
    }
}

/// Public wrapper for admin API access.
pub async fn resolve_harness_database_public(
    db: &sqlx::PgPool,
    base_db_url: &str,
    harness_id: Uuid,
) -> HashMap<String, String> {
    resolve_harness_database(db, base_db_url, harness_id).await
}

/// Resolve the per-harness database URL for an existing harness.
///
/// If the harness already has `database_name` set, builds the URL from that.
/// Otherwise creates a new per-harness database and records the name.
/// Returns env vars to inject into the harness config. Falls back to shared
/// DB (empty map) if anything goes wrong.
async fn resolve_harness_database(
    db: &sqlx::PgPool,
    base_db_url: &str,
    harness_id: Uuid,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if base_db_url.is_empty() {
        return env;
    }

    // Check if harness already has an isolated database.
    let existing: Option<String> =
        sqlx::query_scalar("SELECT database_name FROM harness_instances WHERE id = $1")
            .bind(harness_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();

    if let Some(db_name) = existing {
        // Ensure pgvector is installed on existing databases (may have been
        // created before the extension was added).
        if let Err(e) = crate::provisioning::db::ensure_pgvector(base_db_url, &db_name).await {
            tracing::warn!("Failed to ensure pgvector on {}: {}", db_name, e);
        }
        let db_url = crate::provisioning::db::database_url_for_harness(base_db_url, &db_name);
        env.insert("AMOS__DATABASE__URL".to_string(), db_url);
    } else {
        // Create new per-harness database.
        match crate::provisioning::db::create_harness_database(base_db_url, harness_id).await {
            Ok(db_url) => {
                let db_name = crate::provisioning::db::database_name_for_harness(harness_id);
                env.insert("AMOS__DATABASE__URL".to_string(), db_url);
                let _ = sqlx::query(
                    "UPDATE harness_instances SET database_name = $1 WHERE id = $2",
                )
                .bind(&db_name)
                .bind(harness_id)
                .execute(db)
                .await;
            }
            Err(e) => {
                error!(
                    "Failed to create harness database: {} — falling back to shared DB",
                    e
                );
            }
        }
    }
    env
}

// ── Template Structs ────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
    redirect: Option<String>,
}

#[derive(Template)]
#[template(path = "register.html")]
struct RegisterTemplate {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    tenant_name: String,
    tenant_slug: String,
    plan: String,
    plan_price: String,
    stripe_subscription_status: String,
    instances: Vec<HarnessInfo>,
    user_count: i64,
    api_key_count: i64,
    flash_message: Option<String>,
    flash_error: Option<String>,
    billing_enabled: bool,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    tenant_name: String,
    role: String,
    plan: String,
    plan_price: String,
    stripe_subscription_status: String,
    has_stripe_customer: bool,
    api_keys: Vec<ApiKeyInfo>,
    users: Vec<UserInfo>,
    new_api_key: Option<String>,
    flash_message: Option<String>,
}

#[derive(Template)]
#[template(path = "billing_upgrade.html")]
#[allow(dead_code)]
struct BillingUpgradeTemplate {
    tenant_name: String,
    current_plan: String,
}

#[derive(Template)]
#[template(path = "self_host.html")]
struct SelfHostTemplate {
    tenant_name: String,
}

// ── View model types ────────────────────────────────────────────────────

#[allow(dead_code)]
struct HarnessInfo {
    id: String,
    full_id: String,
    name: Option<String>,
    status: String,
    subdomain: Option<String>,
    region: String,
    instance_size: String,
    healthy: bool,
    endpoint_url: Option<String>,
    container_id_short: Option<String>,
    image_tag: Option<String>,
    image_tag_short: Option<String>,
    previous_image_tag: Option<String>,
    /// Set to the latest release version when it differs from image_tag.
    update_available: Option<String>,
    update_available_short: Option<String>,
}

struct ApiKeyInfo {
    name: String,
    key_prefix: String,
    is_active: bool,
    created_at: String,
}

struct UserInfo {
    email: String,
    name: Option<String>,
    role: String,
    is_active: bool,
}

// ── Form structs ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginForm {
    email: String,
    password: String,
    redirect: Option<String>,
}

#[derive(Deserialize)]
struct RegisterForm {
    organization_name: String,
    name: String,
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct CreateApiKeyForm {
    name: String,
}

// ── Routes ──────────────────────────────────────────────────────────────

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/register", get(register_page).post(register_submit))
        .route("/dashboard", get(dashboard_page))
        .route("/settings", get(settings_page))
        .route("/settings/api-keys", post(create_api_key_submit))
        .route("/billing/success", get(billing_success))
        .route("/billing/upgrade", get(billing_upgrade_page))
        .route("/billing/checkout", post(billing_checkout))
        .route("/billing/portal", post(billing_portal))
        .route("/dashboard/harness/new", post(deploy_new_harness))
        .route("/dashboard/harness/{id}/start", post(harness_start))
        .route("/dashboard/harness/{id}/stop", post(harness_stop))
        .route("/dashboard/harness/{id}/restart", post(harness_restart))
        .route("/dashboard/harness/{id}/redeploy", post(harness_redeploy))
        .route("/dashboard/harness/{id}/update", post(harness_update))
        .route("/dashboard/harness/{id}/rollback", post(harness_rollback))
        .route("/dashboard/harness/{id}/delete", post(harness_delete))
        .route("/self-host", get(self_host_page))
        .route("/logout", post(logout_submit))
}

// ── Login ───────────────────────────────────────────────────────────────

async fn login_page(uri: axum::http::Uri) -> impl IntoResponse {
    let redirect = uri
        .query()
        .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("redirect=")))
        .map(|v| urlencoding::decode(v).unwrap_or_default().into_owned());
    HtmlTemplate(LoginTemplate { error: None, redirect })
}

async fn login_submit(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    // Rate limit: 10 login attempts per IP per 60 seconds
    let ip = extract_client_ip(&headers);
    if is_rate_limited(&ip, 10, 60) {
        warn!(ip = %ip, "Login rate limited");
        return HtmlTemplate(LoginTemplate {
            error: Some("Too many login attempts. Please wait a minute and try again.".into()),
            redirect: form.redirect.clone(),
        })
        .into_response();
    }

    // Look up user by email
    let row = sqlx::query_as::<_, (Uuid, Uuid, String, String, String, bool)>(
        "SELECT u.id, u.tenant_id, u.password_hash, u.role, t.slug, u.is_active
         FROM users u JOIN tenants t ON u.tenant_id = t.id
         WHERE u.email = $1
         LIMIT 1",
    )
    .bind(&form.email)
    .fetch_optional(&state.db)
    .await;

    let row = match row {
        Ok(Some(r)) => r,
        Ok(None) => {
            return HtmlTemplate(LoginTemplate {
                error: Some("Invalid email or password.".into()),
                redirect: form.redirect.clone(),
            })
            .into_response();
        }
        Err(e) => {
            error!("Login query failed: {}", e);
            return HtmlTemplate(LoginTemplate {
                error: Some("An internal error occurred.".into()),
                redirect: form.redirect.clone(),
            })
            .into_response();
        }
    };

    let (user_id, tenant_id, password_hash, role, tenant_slug, is_active) = row;

    if !is_active {
        return HtmlTemplate(LoginTemplate {
            error: Some("Account is deactivated.".into()),
            redirect: form.redirect.clone(),
        })
        .into_response();
    }

    // Verify password
    let valid = match auth::verify_password(&form.password, &password_hash) {
        Ok(v) => v,
        Err(e) => {
            error!("Password verification error: {}", e);
            return HtmlTemplate(LoginTemplate {
                error: Some("An internal error occurred.".into()),
                redirect: form.redirect.clone(),
            })
            .into_response();
        }
    };

    if !valid {
        return HtmlTemplate(LoginTemplate {
            error: Some("Invalid email or password.".into()),
            redirect: form.redirect.clone(),
        })
        .into_response();
    }

    // Create JWT
    let jwt_secret = get_jwt_secret(&state);
    let access_expiry = state.config.auth.access_token_expiry_secs as i64;

    let token = match auth::create_access_token(
        user_id,
        tenant_id,
        &role,
        &tenant_slug,
        &jwt_secret,
        access_expiry,
    ) {
        Ok(t) => t,
        Err(e) => {
            error!("Token creation failed: {}", e);
            return HtmlTemplate(LoginTemplate {
                error: Some("An internal error occurred.".into()),
                redirect: form.redirect.clone(),
            })
            .into_response();
        }
    };

    // Update last_login_at
    let _ = sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await;

    // Set httponly cookie for platform
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_COOKIE, token, access_expiry
    );

    // If a redirect URL was provided (e.g., from a harness), redirect there
    // with the token so the harness can set its own session cookie.
    if let Some(ref redirect_url) = form.redirect {
        if redirect_url.contains(".custom.amoslabs.com") || redirect_url.contains("localhost") {
            // Extract the base URL (up to the path) and redirect to harness callback
            let harness_base = if let Some(idx) = redirect_url.find(".custom.amoslabs.com") {
                // Find the end of the hostname
                let after_host = &redirect_url[idx + ".custom.amoslabs.com".len()..];
                let path_start = after_host.find('/').map(|i| idx + ".custom.amoslabs.com".len() + i);
                &redirect_url[..path_start.unwrap_or(redirect_url.len())]
            } else {
                redirect_url.as_str()
            };
            let callback_url = format!("{}/auth/callback?token={}", harness_base, token);
            return ([(header::SET_COOKIE, cookie)], Redirect::to(&callback_url)).into_response();
        }
    }

    ([(header::SET_COOKIE, cookie)], Redirect::to("/dashboard")).into_response()
}

// ── Register ────────────────────────────────────────────────────────────

async fn register_page() -> impl IntoResponse {
    HtmlTemplate(RegisterTemplate { error: None })
}

async fn register_submit(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<RegisterForm>,
) -> Response {
    // Rate limit: 5 signups per IP per 5 minutes
    let ip = extract_client_ip(&headers);
    if is_rate_limited(&ip, 5, 300) {
        warn!(ip = %ip, "Registration rate limited");
        return HtmlTemplate(RegisterTemplate {
            error: Some("Too many signup attempts. Please wait a few minutes.".into()),
        })
        .into_response();
    }

    // Validation
    if form.organization_name.trim().is_empty() {
        return HtmlTemplate(RegisterTemplate {
            error: Some("Organization name is required.".into()),
        })
        .into_response();
    }
    if form.email.trim().is_empty() || !form.email.contains('@') {
        return HtmlTemplate(RegisterTemplate {
            error: Some("A valid email address is required.".into()),
        })
        .into_response();
    }
    if form.password.len() < 8 {
        return HtmlTemplate(RegisterTemplate {
            error: Some("Password must be at least 8 characters.".into()),
        })
        .into_response();
    }

    let slug = auth::slugify(&form.organization_name);
    if slug.is_empty() {
        return HtmlTemplate(RegisterTemplate {
            error: Some("Organization name must contain alphanumeric characters.".into()),
        })
        .into_response();
    }

    // Hash password
    let password_hash = match auth::hash_password(&form.password) {
        Ok(h) => h,
        Err(e) => {
            error!("Password hashing failed: {}", e);
            return HtmlTemplate(RegisterTemplate {
                error: Some("An internal error occurred.".into()),
            })
            .into_response();
        }
    };

    let tenant_id = Uuid::new_v4();

    // Try to create tenant, auto-suffix slug on collision
    let mut final_slug = slug.clone();
    let mut attempts = 0;
    loop {
        let subdomain = Some(final_slug.clone());
        let result = sqlx::query(
            "INSERT INTO tenants (id, name, slug, plan, subdomain) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(tenant_id)
        .bind(&form.organization_name)
        .bind(&final_slug)
        .bind("free")
        .bind(&subdomain)
        .execute(&state.db)
        .await;

        match result {
            Ok(_) => break,
            Err(e) => {
                let err_str = e.to_string();
                if (err_str.contains("tenants_slug_key")
                    || err_str.contains("tenants_subdomain_key"))
                    && attempts < 5
                {
                    attempts += 1;
                    let suffix: u16 = rand::random::<u16>() % 9000 + 1000;
                    final_slug = format!("{}-{}", slug, suffix);
                    continue;
                }
                if attempts >= 5 {
                    error!("Slug collision after 5 attempts for: {}", slug);
                    return HtmlTemplate(RegisterTemplate {
                        error: Some(
                            "Organization name is unavailable. Try a different name.".into(),
                        ),
                    })
                    .into_response();
                }
                error!("Failed to create tenant: {}", e);
                return HtmlTemplate(RegisterTemplate {
                    error: Some("Failed to create organization.".into()),
                })
                .into_response();
            }
        }
    }

    // Create user (owner)
    let user_id = Uuid::new_v4();
    let user_result = sqlx::query(
        "INSERT INTO users (id, tenant_id, email, name, password_hash, role, email_verified)
         VALUES ($1, $2, $3, $4, $5, 'owner', TRUE)",
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(&form.email)
    .bind(&form.name)
    .bind(&password_hash)
    .execute(&state.db)
    .await;

    if let Err(e) = user_result {
        // Rollback tenant
        let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .execute(&state.db)
            .await;
        let err_str = e.to_string();
        if err_str.contains("users_tenant_id_email_key") {
            return HtmlTemplate(RegisterTemplate {
                error: Some(format!("Email '{}' is already registered.", form.email)),
            })
            .into_response();
        }
        error!("Failed to create user: {}", e);
        return HtmlTemplate(RegisterTemplate {
            error: Some("Failed to create user account.".into()),
        })
        .into_response();
    }

    // Create harness instance record (pending for all plans — paid plans get
    // provisioned after Stripe checkout completes via webhook).
    let harness_id = Uuid::new_v4();
    let _ = sqlx::query(
        "INSERT INTO harness_instances (id, tenant_id, subdomain, status)
         VALUES ($1, $2, $3, 'pending')",
    )
    .bind(harness_id)
    .bind(tenant_id)
    .bind(&final_slug)
    .execute(&state.db)
    .await;

    // Issue JWT and set session cookie (needed for both flows)
    let jwt_secret = get_jwt_secret(&state);
    let access_expiry = state.config.auth.access_token_expiry_secs as i64;

    let token = match auth::create_access_token(
        user_id,
        tenant_id,
        "owner",
        &final_slug,
        &jwt_secret,
        access_expiry,
    ) {
        Ok(t) => t,
        Err(e) => {
            error!("Token creation failed: {}", e);
            return Redirect::to("/login").into_response();
        }
    };

    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_COOKIE, token, access_expiry
    );

    // Self-hosted / Stripe not configured: provision free-tier harness immediately.
    // When billing is enabled (managed hosting), users upgrade from the dashboard.
    if state.stripe_client.is_none() {
        info!(tenant_id = %tenant_id, "Stripe not configured — provisioning free-tier harness immediately");
        crate::routes::webhooks::provision_harness_for_tenant(&state, tenant_id).await;
    }

    ([(header::SET_COOKIE, cookie)], Redirect::to("/dashboard")).into_response()
}

// ── Dashboard ───────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct DashboardQuery {
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

async fn dashboard_page(
    State(state): State<PlatformState>,
    axum::extract::Query(query): axum::extract::Query<DashboardQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let claims = match extract_session_claims(&state, &headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    // Fetch tenant info (including billing status)
    let tenant_row = sqlx::query_as::<_, (String, String, String, Option<String>)>(
        "SELECT name, slug, plan, stripe_subscription_status FROM tenants WHERE id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (tenant_name, tenant_slug, plan, stripe_sub_status) = tenant_row.unwrap_or_else(|| {
        (
            "Unknown".into(),
            claims.tenant_slug.clone(),
            "free".into(),
            None,
        )
    });

    let plan_enum = crate::billing::Plan::from_str(&plan);
    let stripe_subscription_status = stripe_sub_status.unwrap_or_else(|| "none".into());

    // Fetch the latest available release version for update-available badge.
    let latest_release_version: Option<String> = sqlx::query_scalar(
        "SELECT version FROM releases WHERE status = 'available' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    // Fetch harness instances (including endpoint info and version tracking).
    let harness_rows = sqlx::query_as::<_, (Uuid, String, Option<String>, String, String, bool, Option<i32>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)>(
        "SELECT id, status, subdomain, region, instance_size, healthy, external_port, internal_url, container_id, image_tag, previous_image_tag, name
         FROM harness_instances WHERE tenant_id = $1 AND status != 'deprovisioned' ORDER BY created_at DESC"
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let instances: Vec<HarnessInfo> = harness_rows
        .into_iter()
        .map(
            |(
                id,
                status,
                subdomain,
                region,
                instance_size,
                healthy,
                _external_port,
                internal_url,
                container_id,
                image_tag,
                previous_image_tag,
                name,
            )| {
                // Prefer public subdomain URL over internal IP for the endpoint display.
                let endpoint_url = subdomain
                    .as_deref()
                    .map(|s| format!("https://{}.custom.amoslabs.com", s))
                    .or(internal_url);

                // Show update badge when latest release differs from current image_tag.
                let update_available = match (&latest_release_version, &image_tag) {
                    (Some(latest), Some(current)) if latest != current => Some(latest.clone()),
                    _ => None,
                };

                let image_tag_short = image_tag.as_ref().map(|t| truncate_version(t));
                let update_available_short = update_available.as_ref().map(|t| truncate_version(t));

                HarnessInfo {
                    id: id.to_string()[..8].to_string(),
                    full_id: id.to_string(),
                    name,
                    status,
                    subdomain,
                    region,
                    instance_size,
                    healthy,
                    endpoint_url,
                    container_id_short: container_id.map(|c| {
                        if c.len() > 12 {
                            c[..12].to_string()
                        } else {
                            c
                        }
                    }),
                    image_tag,
                    image_tag_short,
                    previous_image_tag,
                    update_available,
                    update_available_short,
                }
            },
        )
        .collect();

    // Fetch counts
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

    let api_key_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);

    HtmlTemplate(DashboardTemplate {
        tenant_name,
        tenant_slug,
        plan,
        plan_price: if plan_enum.is_paid() { "Per harness".to_string() } else { "Free".to_string() },
        stripe_subscription_status,
        instances,
        user_count,
        api_key_count,
        flash_message: query.msg,
        flash_error: query.error,
        billing_enabled: state.stripe_client.is_some(),
    })
    .into_response()
}

// ── Settings ────────────────────────────────────────────────────────────

async fn settings_page(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Response {
    settings_page_inner(&state, &headers, None, None).await
}

async fn settings_page_inner(
    state: &PlatformState,
    headers: &axum::http::HeaderMap,
    new_api_key: Option<String>,
    flash_message: Option<String>,
) -> Response {
    let claims = match extract_session_claims(state, headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    // Tenant name + billing info
    let tenant_row = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT name, plan, stripe_customer_id, stripe_subscription_status FROM tenants WHERE id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (tenant_name, plan, stripe_customer_id, stripe_sub_status) =
        tenant_row.unwrap_or_else(|| ("Unknown".into(), "free".into(), None, None));

    let plan_enum = crate::billing::Plan::from_str(&plan);

    // API keys
    let key_rows = sqlx::query_as::<_, (String, String, bool, String)>(
        "SELECT name, key_prefix, is_active, created_at::text
         FROM api_keys WHERE tenant_id = $1 ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let api_keys: Vec<ApiKeyInfo> = key_rows
        .into_iter()
        .map(|(name, key_prefix, is_active, created_at)| ApiKeyInfo {
            name,
            key_prefix,
            is_active,
            created_at,
        })
        .collect();

    // Users
    let user_rows = sqlx::query_as::<_, (String, Option<String>, String, bool)>(
        "SELECT email, name, role, is_active
         FROM users WHERE tenant_id = $1 ORDER BY created_at ASC",
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let users: Vec<UserInfo> = user_rows
        .into_iter()
        .map(|(email, name, role, is_active)| UserInfo {
            email,
            name,
            role,
            is_active,
        })
        .collect();

    HtmlTemplate(SettingsTemplate {
        tenant_name,
        role: claims.role.clone(),
        plan: plan.clone(),
        plan_price: if plan_enum.is_paid() { "Per harness".to_string() } else { "Free".to_string() },
        stripe_subscription_status: stripe_sub_status.unwrap_or_else(|| "none".into()),
        has_stripe_customer: stripe_customer_id.is_some(),
        api_keys,
        users,
        new_api_key,
        flash_message,
    })
    .into_response()
}

async fn create_api_key_submit(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<CreateApiKeyForm>,
) -> Response {
    let claims = match extract_session_claims(&state, &headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    if claims.role != "owner" && claims.role != "admin" {
        return settings_page_inner(
            &state,
            &headers,
            None,
            Some("Only owner or admin can create API keys.".into()),
        )
        .await;
    }

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let user_id: Uuid = match claims.sub.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    if form.name.trim().is_empty() {
        return settings_page_inner(
            &state,
            &headers,
            None,
            Some("API key name is required.".into()),
        )
        .await;
    }

    let (full_key, prefix, key_hash) = auth::generate_api_key();
    let key_id = Uuid::new_v4();

    let scopes: Vec<String> = vec![];
    let result = sqlx::query(
        "INSERT INTO api_keys (id, tenant_id, created_by, name, key_prefix, key_hash, scopes)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(key_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(&form.name)
    .bind(&prefix)
    .bind(&key_hash)
    .bind(&scopes)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => {
            settings_page_inner(
                &state,
                &headers,
                Some(full_key),
                Some("API key created successfully.".into()),
            )
            .await
        }
        Err(e) => {
            error!("Failed to create API key: {}", e);
            settings_page_inner(
                &state,
                &headers,
                None,
                Some("Failed to create API key.".into()),
            )
            .await
        }
    }
}

// ── Harness Management ────────────────────────────────────────────────

/// Helper: verify the session and that the harness instance belongs to the tenant.
async fn verify_harness_ownership(
    state: &PlatformState,
    headers: &axum::http::HeaderMap,
    harness_id: &str,
) -> Result<(auth::Claims, Uuid, Option<String>), Response> {
    let claims = extract_session_claims(state, headers)
        .ok_or_else(|| Redirect::to("/login").into_response())?;

    let tenant_id: Uuid = claims
        .tenant_id
        .parse()
        .map_err(|_| Redirect::to("/login").into_response())?;

    let harness_uuid: Uuid = harness_id
        .parse()
        .map_err(|_| Redirect::to("/dashboard").into_response())?;

    // Verify the instance belongs to this tenant and get container_id
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT container_id FROM harness_instances WHERE id = $1 AND tenant_id = $2",
    )
    .bind(harness_uuid)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        error!("DB error in verify_harness_ownership: {}", e);
        Redirect::to("/dashboard").into_response()
    })?;

    let container_id = match row {
        Some((cid,)) => cid,
        None => return Err(Redirect::to("/dashboard").into_response()),
    };

    Ok((claims, harness_uuid, container_id))
}

async fn harness_start(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (_claims, harness_id, container_id) =
        match verify_harness_ownership(&state, &headers, &id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

    let container_id = match container_id {
        Some(cid) => cid,
        None => return Redirect::to("/dashboard").into_response(),
    };

    if let Some(ref manager) = state.harness_manager {
        match manager.start(&container_id).await {
            Ok(()) => {
                let _ = sqlx::query("UPDATE harness_instances SET status = 'running', started_at = NOW(), healthy = TRUE WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(harness_id = %harness_id, "Harness started via dashboard");
            }
            Err(e) => {
                error!("Failed to start harness {}: {}", harness_id, e);
            }
        }
    } else if let Some(ref ecs) = state.ecs_provisioner {
        // ECS tasks can't be "started" — they must be re-provisioned.
        // For ECS, treat start as a redeploy.
        let tenant_slug: String = sqlx::query_scalar(
            "SELECT t.slug FROM tenants t JOIN harness_instances h ON h.tenant_id = t.id WHERE h.id = $1"
        )
        .bind(harness_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".into());

        let ecs_config = HarnessConfig {
            customer_id: Uuid::nil(),
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: HashMap::new(),
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        match ecs.provision(&ecs_config, &tenant_slug).await {
            Ok(task_arn) => {
                let _ = sqlx::query("UPDATE harness_instances SET container_id = $1, status = 'provisioning', provisioned_at = NOW() WHERE id = $2")
                    .bind(&task_arn)
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(task_arn = %task_arn, "ECS harness re-provisioned via dashboard");
            }
            Err(e) => {
                error!("Failed to re-provision ECS harness {}: {}", harness_id, e);
            }
        }
    }

    Redirect::to("/dashboard").into_response()
}

async fn harness_stop(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (_claims, harness_id, container_id) =
        match verify_harness_ownership(&state, &headers, &id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

    let container_id = match container_id {
        Some(cid) => cid,
        None => return Redirect::to("/dashboard").into_response(),
    };

    if let Some(ref manager) = state.harness_manager {
        match manager.stop(&container_id).await {
            Ok(()) => {
                let _ = sqlx::query("UPDATE harness_instances SET status = 'stopped', healthy = FALSE WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(harness_id = %harness_id, "Harness stopped via dashboard");
            }
            Err(e) => {
                error!("Failed to stop harness {}: {}", harness_id, e);
            }
        }
    } else if let Some(ref ecs) = state.ecs_provisioner {
        // Tear down ALB routing before stopping.
        teardown_alb_routing(&state, harness_id).await;

        match ecs.stop(&container_id).await {
            Ok(()) => {
                let _ = sqlx::query("UPDATE harness_instances SET status = 'stopped', healthy = FALSE WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(harness_id = %harness_id, "ECS harness stopped via dashboard");
            }
            Err(e) => {
                error!("Failed to stop ECS harness {}: {}", harness_id, e);
            }
        }
    }

    Redirect::to("/dashboard").into_response()
}

async fn harness_restart(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (_claims, harness_id, container_id) =
        match verify_harness_ownership(&state, &headers, &id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

    let container_id = match container_id {
        Some(cid) => cid,
        None => return Redirect::to("/dashboard").into_response(),
    };

    if let Some(ref manager) = state.harness_manager {
        // Stop then start
        let _ = manager.stop(&container_id).await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        match manager.start(&container_id).await {
            Ok(()) => {
                let _ = sqlx::query("UPDATE harness_instances SET status = 'running', started_at = NOW() WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(harness_id = %harness_id, "Harness restarted via dashboard");
            }
            Err(e) => {
                error!("Failed to restart harness {}: {}", harness_id, e);
                let _ = sqlx::query(
                    "UPDATE harness_instances SET status = 'error', healthy = FALSE WHERE id = $1",
                )
                .bind(harness_id)
                .execute(&state.db)
                .await;
            }
        }
    } else if let Some(ref ecs) = state.ecs_provisioner {
        // ECS: stop old task, provision new one
        let _ = ecs.stop(&container_id).await;

        let tenant_slug: String = sqlx::query_scalar(
            "SELECT t.slug FROM tenants t JOIN harness_instances h ON h.tenant_id = t.id WHERE h.id = $1"
        )
        .bind(harness_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".into());

        let ecs_config = HarnessConfig {
            customer_id: Uuid::nil(),
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: HashMap::new(),
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        match ecs.provision(&ecs_config, &tenant_slug).await {
            Ok(task_arn) => {
                let _ = sqlx::query("UPDATE harness_instances SET container_id = $1, status = 'provisioning', provisioned_at = NOW() WHERE id = $2")
                    .bind(&task_arn)
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(task_arn = %task_arn, "ECS harness restarted via dashboard");
            }
            Err(e) => {
                error!("Failed to restart ECS harness {}: {}", harness_id, e);
            }
        }
    }

    Redirect::to("/dashboard").into_response()
}

async fn harness_redeploy(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (_claims, harness_id, container_id) =
        match verify_harness_ownership(&state, &headers, &id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

    // Stop old container/task if it exists
    if let Some(cid) = &container_id {
        if let Some(ref manager) = state.harness_manager {
            let _ = manager.deprovision(cid).await;
        } else if let Some(ref ecs) = state.ecs_provisioner {
            let _ = ecs.stop(cid).await;
        }
    }

    // Get tenant info for provisioning
    let tenant_row = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT t.id, t.slug FROM tenants t JOIN harness_instances h ON h.tenant_id = t.id WHERE h.id = $1"
    )
    .bind(harness_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (tenant_id, tenant_slug) = match tenant_row {
        Some(r) => r,
        None => return Redirect::to("/dashboard").into_response(),
    };

    let _ = sqlx::query(
        "UPDATE harness_instances SET status = 'provisioning', healthy = FALSE WHERE id = $1",
    )
    .bind(harness_id)
    .execute(&state.db)
    .await;

    if let Some(ref manager) = state.harness_manager {
        let mut harness_env = HashMap::new();
        harness_env.insert(
            "AMOS__DATABASE__URL".to_string(),
            "postgres://rickbarkley@host.docker.internal:5432/amos_dev".to_string(),
        );
        harness_env.insert(
            "AMOS__REDIS__URL".to_string(),
            "redis://host.docker.internal:6379".to_string(),
        );
        harness_env.insert(
            "AMOS__PLATFORM__URL".to_string(),
            format!("http://host.docker.internal:{}", state.config.server.port),
        );

        let config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-west-2".to_string(),
            instance_size: InstanceSize::Small,
            environment: "development".to_string(),
            platform_grpc_url: format!(
                "http://{}:{}",
                state.config.server.host, state.config.server.port
            ),
            env_vars: harness_env,
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        match manager.provision(&config).await {
            Ok(new_container_id) => {
                let _ = sqlx::query("UPDATE harness_instances SET container_id = $1, provisioned_at = NOW() WHERE id = $2")
                    .bind(&new_container_id)
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;

                if let Err(e) = manager.start(&new_container_id).await {
                    warn!("Failed to start redeployed harness: {}", e);
                    let _ =
                        sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                            .bind(harness_id)
                            .execute(&state.db)
                            .await;
                } else {
                    // Quick port detection
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let port = manager
                        .inspect_host_port(&new_container_id)
                        .await
                        .ok()
                        .flatten();
                    let internal_url = port.map(|p| format!("http://localhost:{}", p));
                    let _ = sqlx::query("UPDATE harness_instances SET status = 'running', started_at = NOW(), external_port = $1, internal_url = $2, healthy = TRUE WHERE id = $3")
                        .bind(port.map(|p| p as i32))
                        .bind(&internal_url)
                        .bind(harness_id)
                        .execute(&state.db)
                        .await;
                    info!(harness_id = %harness_id, "Harness redeployed via dashboard");
                }
            }
            Err(e) => {
                error!("Failed to redeploy harness {}: {}", harness_id, e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
            }
        }
    } else if let Some(ref ecs) = state.ecs_provisioner {
        // Tear down old ALB routing — the poller will set up new routing for the new task.
        teardown_alb_routing(&state, harness_id).await;

        // Reuse existing per-harness database (or create if migrating from shared DB).
        let harness_env =
            resolve_harness_database(&state.db, ecs.harness_database_url(), harness_id).await;

        let ecs_config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: harness_env,
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        match ecs.provision(&ecs_config, &tenant_slug).await {
            Ok(task_arn) => {
                let _ = sqlx::query("UPDATE harness_instances SET container_id = $1, provisioned_at = NOW() WHERE id = $2")
                    .bind(&task_arn)
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(task_arn = %task_arn, "ECS harness redeployed via dashboard");

                // Fetch subdomain from the harness instance for ALB routing.
                let subdomain: Option<String> =
                    sqlx::query_scalar("SELECT subdomain FROM harness_instances WHERE id = $1")
                        .bind(harness_id)
                        .fetch_optional(&state.db)
                        .await
                        .ok()
                        .flatten();

                // Poll ECS in the background until task reaches RUNNING.
                spawn_ecs_status_poller(
                    ecs.clone(),
                    state.db.clone(),
                    task_arn,
                    harness_id,
                    state.alb_router.clone(),
                    subdomain,
                );
            }
            Err(e) => {
                error!("Failed to redeploy ECS harness {}: {}", harness_id, e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
            }
        }
    }

    Redirect::to("/dashboard").into_response()
}

/// Update a harness to the latest available release.
async fn harness_update(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (_claims, harness_id, container_id) =
        match verify_harness_ownership(&state, &headers, &id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

    // Look up the latest available release.
    let release = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT version, harness_image, agent_image FROM releases WHERE status = 'available' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (new_version, harness_image, agent_image) = match release {
        Some(r) => r,
        None => {
            warn!("No available release found for update");
            return Redirect::to(
                "/dashboard?error=No+release+available+for+update.+Push+a+new+build+first.",
            )
            .into_response();
        }
    };

    // Save current image_tag as previous_image_tag for rollback.
    let _ =
        sqlx::query("UPDATE harness_instances SET previous_image_tag = image_tag WHERE id = $1")
            .bind(harness_id)
            .execute(&state.db)
            .await;

    // Stop old task if it exists.
    if let Some(cid) = &container_id {
        if let Some(ref ecs) = state.ecs_provisioner {
            let _ = ecs.stop(cid).await;
        } else if let Some(ref manager) = state.harness_manager {
            let _ = manager.deprovision(cid).await;
        }
    }

    // Tear down ALB routing — poller will set up new routing.
    teardown_alb_routing(&state, harness_id).await;

    let _ = sqlx::query(
        "UPDATE harness_instances SET status = 'provisioning', healthy = FALSE WHERE id = $1",
    )
    .bind(harness_id)
    .execute(&state.db)
    .await;

    // Get tenant info for provisioning.
    let tenant_row = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT t.id, t.slug FROM tenants t JOIN harness_instances h ON h.tenant_id = t.id WHERE h.id = $1",
    )
    .bind(harness_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (tenant_id, tenant_slug) = match tenant_row {
        Some(r) => r,
        None => return Redirect::to("/dashboard").into_response(),
    };

    if let Some(ref ecs) = state.ecs_provisioner {
        // Resolve per-harness database for the update.
        let harness_env =
            resolve_harness_database(&state.db, ecs.harness_database_url(), harness_id).await;

        let ecs_config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: harness_env,
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        match ecs
            .provision_with_images(
                &ecs_config,
                &tenant_slug,
                &harness_image,
                agent_image.as_deref(),
            )
            .await
        {
            Ok(task_arn) => {
                let _ = sqlx::query(
                    "UPDATE harness_instances SET container_id = $1, image_tag = $2, provisioned_at = NOW() WHERE id = $3",
                )
                .bind(&task_arn)
                .bind(&new_version)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                info!(task_arn = %task_arn, version = %new_version, "Harness updated to new release");

                let subdomain: Option<String> =
                    sqlx::query_scalar("SELECT subdomain FROM harness_instances WHERE id = $1")
                        .bind(harness_id)
                        .fetch_optional(&state.db)
                        .await
                        .ok()
                        .flatten();

                spawn_ecs_status_poller(
                    ecs.clone(),
                    state.db.clone(),
                    task_arn,
                    harness_id,
                    state.alb_router.clone(),
                    subdomain,
                );
            }
            Err(e) => {
                error!("Failed to update harness {}: {}", harness_id, e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                return Redirect::to(
                    "/dashboard?error=Failed+to+update+harness.+Check+logs+for+details.",
                )
                .into_response();
            }
        }
    }

    Redirect::to("/dashboard?msg=Harness+update+started.").into_response()
}

/// Roll back a harness to the previous version.
async fn harness_rollback(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (_claims, harness_id, container_id) =
        match verify_harness_ownership(&state, &headers, &id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

    // Read the previous_image_tag.
    let prev_tag: Option<String> =
        sqlx::query_scalar("SELECT previous_image_tag FROM harness_instances WHERE id = $1")
            .bind(harness_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let prev_tag = match prev_tag {
        Some(t) => t,
        None => {
            warn!(
                "No previous image tag for rollback on harness {}",
                harness_id
            );
            return Redirect::to("/dashboard?error=No+previous+version+available+for+rollback.")
                .into_response();
        }
    };

    // Look up the release for that version.
    let release = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT harness_image, agent_image FROM releases WHERE version = $1",
    )
    .bind(&prev_tag)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (harness_image, agent_image) = match release {
        Some(r) => r,
        None => {
            warn!("Release not found for version {} during rollback", prev_tag);
            return Redirect::to("/dashboard?error=Release+record+not+found+for+previous+version.")
                .into_response();
        }
    };

    // Stop old task.
    if let Some(cid) = &container_id {
        if let Some(ref ecs) = state.ecs_provisioner {
            let _ = ecs.stop(cid).await;
        } else if let Some(ref manager) = state.harness_manager {
            let _ = manager.deprovision(cid).await;
        }
    }

    teardown_alb_routing(&state, harness_id).await;

    let _ = sqlx::query(
        "UPDATE harness_instances SET status = 'provisioning', healthy = FALSE WHERE id = $1",
    )
    .bind(harness_id)
    .execute(&state.db)
    .await;

    let tenant_row = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT t.id, t.slug FROM tenants t JOIN harness_instances h ON h.tenant_id = t.id WHERE h.id = $1",
    )
    .bind(harness_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (tenant_id, tenant_slug) = match tenant_row {
        Some(r) => r,
        None => return Redirect::to("/dashboard").into_response(),
    };

    if let Some(ref ecs) = state.ecs_provisioner {
        // Resolve per-harness database for the rollback.
        let harness_env =
            resolve_harness_database(&state.db, ecs.harness_database_url(), harness_id).await;

        let ecs_config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: harness_env,
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        match ecs
            .provision_with_images(
                &ecs_config,
                &tenant_slug,
                &harness_image,
                agent_image.as_deref(),
            )
            .await
        {
            Ok(task_arn) => {
                // Swap tags: current becomes previous, previous becomes current.
                let _ = sqlx::query(
                    "UPDATE harness_instances SET container_id = $1, previous_image_tag = image_tag, image_tag = $2, provisioned_at = NOW() WHERE id = $3",
                )
                .bind(&task_arn)
                .bind(&prev_tag)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                info!(task_arn = %task_arn, version = %prev_tag, "Harness rolled back");

                let subdomain: Option<String> =
                    sqlx::query_scalar("SELECT subdomain FROM harness_instances WHERE id = $1")
                        .bind(harness_id)
                        .fetch_optional(&state.db)
                        .await
                        .ok()
                        .flatten();

                spawn_ecs_status_poller(
                    ecs.clone(),
                    state.db.clone(),
                    task_arn,
                    harness_id,
                    state.alb_router.clone(),
                    subdomain,
                );
            }
            Err(e) => {
                error!("Failed to rollback harness {}: {}", harness_id, e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                return Redirect::to(
                    "/dashboard?error=Failed+to+roll+back+harness.+Check+logs+for+details.",
                )
                .into_response();
            }
        }
    }

    Redirect::to("/dashboard?msg=Harness+rollback+started.").into_response()
}

async fn harness_delete(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (_claims, harness_id, container_id) =
        match verify_harness_ownership(&state, &headers, &id).await {
            Ok(v) => v,
            Err(r) => return r,
        };

    // Tear down ALB routing before removing the container/task.
    teardown_alb_routing(&state, harness_id).await;

    // Stop and remove the container/task.
    // For Docker, deprovision() force-removes even running containers.
    // For ECS, stop() is the only option (tasks are ephemeral).
    if let Some(cid) = &container_id {
        if let Some(ref manager) = state.harness_manager {
            // deprovision() uses force:true so it handles running containers
            if let Err(e) = manager.deprovision(cid).await {
                error!(
                    "Failed to deprovision container {} for harness {}: {}",
                    cid, harness_id, e
                );
            }
        } else if let Some(ref ecs) = state.ecs_provisioner {
            if let Err(e) = ecs.stop(cid).await {
                error!(
                    "Failed to stop ECS task {} for harness {}: {}",
                    cid, harness_id, e
                );
            }
        }
    }

    // Drop the per-harness database if one was created.
    let db_name: Option<String> =
        sqlx::query_scalar("SELECT database_name FROM harness_instances WHERE id = $1")
            .bind(harness_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    if let Some(db_name) = db_name {
        let base_url = state
            .ecs_provisioner
            .as_ref()
            .map(|e| e.harness_database_url().to_string())
            .unwrap_or_default();
        if !base_url.is_empty() {
            crate::provisioning::db::drop_harness_database(&base_url, &db_name).await;
        }
    }

    // Delete from database
    if let Err(e) = sqlx::query("DELETE FROM harness_instances WHERE id = $1")
        .bind(harness_id)
        .execute(&state.db)
        .await
    {
        error!(
            "Failed to delete harness {} from database: {}",
            harness_id, e
        );
    }

    info!(harness_id = %harness_id, "Harness deleted via dashboard");
    Redirect::to("/dashboard").into_response()
}

#[derive(Deserialize)]
struct DeployHarnessForm {
    name: Option<String>,
}

async fn deploy_new_harness(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<DeployHarnessForm>,
) -> Response {
    let claims = match extract_session_claims(&state, &headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    let tenant_slug = claims.tenant_slug.clone();
    let subdomain = Some(format!(
        "{}-{}",
        tenant_slug,
        &Uuid::new_v4().to_string()[..4]
    ));

    // Create harness instance record
    let harness_id = Uuid::new_v4();
    let harness_name = form.name.filter(|n| !n.trim().is_empty());
    let _ = sqlx::query(
        "INSERT INTO harness_instances (id, tenant_id, subdomain, name, status)
         VALUES ($1, $2, $3, $4, 'pending')",
    )
    .bind(harness_id)
    .bind(tenant_id)
    .bind(&subdomain)
    .bind(&harness_name)
    .execute(&state.db)
    .await;

    // Provision via Docker or ECS
    if let Some(ref manager) = state.harness_manager {
        let _ = sqlx::query("UPDATE harness_instances SET status = 'provisioning' WHERE id = $1")
            .bind(harness_id)
            .execute(&state.db)
            .await;

        let mut harness_env = HashMap::new();
        harness_env.insert(
            "AMOS__DATABASE__URL".to_string(),
            "postgres://rickbarkley@host.docker.internal:5432/amos_dev".to_string(),
        );
        harness_env.insert(
            "AMOS__REDIS__URL".to_string(),
            "redis://host.docker.internal:6379".to_string(),
        );
        harness_env.insert(
            "AMOS__PLATFORM__URL".to_string(),
            format!("http://host.docker.internal:{}", state.config.server.port),
        );

        let config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-west-2".to_string(),
            instance_size: InstanceSize::Small,
            environment: "development".to_string(),
            platform_grpc_url: format!(
                "http://{}:{}",
                state.config.server.host, state.config.server.port
            ),
            env_vars: harness_env,
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        match manager.provision(&config).await {
            Ok(container_id) => {
                let _ = sqlx::query("UPDATE harness_instances SET container_id = $1, provisioned_at = NOW() WHERE id = $2")
                    .bind(&container_id)
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;

                if let Err(e) = manager.start(&container_id).await {
                    warn!("Failed to start new harness: {}", e);
                    let _ =
                        sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                            .bind(harness_id)
                            .execute(&state.db)
                            .await;
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let port = manager
                        .inspect_host_port(&container_id)
                        .await
                        .ok()
                        .flatten();
                    let internal_url = port.map(|p| format!("http://localhost:{}", p));
                    let _ = sqlx::query("UPDATE harness_instances SET status = 'running', started_at = NOW(), external_port = $1, internal_url = $2, healthy = TRUE WHERE id = $3")
                        .bind(port.map(|p| p as i32))
                        .bind(&internal_url)
                        .bind(harness_id)
                        .execute(&state.db)
                        .await;
                    info!(harness_id = %harness_id, "New harness deployed via dashboard");
                }
            }
            Err(e) => {
                error!("Failed to deploy new harness: {}", e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
            }
        }
    } else if let Some(ref ecs) = state.ecs_provisioner {
        let _ = sqlx::query("UPDATE harness_instances SET status = 'provisioning' WHERE id = $1")
            .bind(harness_id)
            .execute(&state.db)
            .await;

        // Create per-harness isolated database.
        let harness_env =
            resolve_harness_database(&state.db, ecs.harness_database_url(), harness_id).await;

        let ecs_config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: harness_env,
            harness_role: "primary".to_string(),
            packages: vec![],
            harness_id: None,
        };

        // Look up the current release version so we can track what we deployed.
        let current_version: Option<String> = sqlx::query_scalar(
            "SELECT version FROM releases WHERE status = 'available' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        match ecs.provision(&ecs_config, &tenant_slug).await {
            Ok(task_arn) => {
                let _ = sqlx::query("UPDATE harness_instances SET container_id = $1, provisioned_at = NOW(), image_tag = $2 WHERE id = $3")
                    .bind(&task_arn)
                    .bind(&current_version)
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                info!(task_arn = %task_arn, "New ECS harness deployed via dashboard");

                // Poll ECS in the background until task reaches RUNNING.
                spawn_ecs_status_poller(
                    ecs.clone(),
                    state.db.clone(),
                    task_arn,
                    harness_id,
                    state.alb_router.clone(),
                    subdomain.clone(),
                );
            }
            Err(e) => {
                error!("Failed to deploy new ECS harness: {}", e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
            }
        }
    } else {
        warn!("No provisioner available — harness created as pending only");
    }

    Redirect::to("/dashboard").into_response()
}

// ── Billing Routes ───────────────────────────────────────────────────────

/// `GET /billing/success` — post-Stripe-checkout redirect.
async fn billing_success(
    axum::extract::Query(_query): axum::extract::Query<DashboardQuery>,
) -> Response {
    // Simply redirect to dashboard with a success message
    Redirect::to("/dashboard?msg=Payment+successful!+Your+harness+is+being+provisioned.")
        .into_response()
}

/// `GET /billing/upgrade` — pricing comparison page for free/current users.
async fn billing_upgrade_page(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let claims = match extract_session_claims(&state, &headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    let tenant_row =
        sqlx::query_as::<_, (String, String)>("SELECT name, plan FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let (tenant_name, current_plan) =
        tenant_row.unwrap_or_else(|| ("Unknown".into(), "free".into()));

    HtmlTemplate(BillingUpgradeTemplate {
        tenant_name,
        current_plan,
    })
    .into_response()
}

// ── Self-Host ──────────────────────────────────────────────────────────

async fn self_host_page(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let claims = match extract_session_claims(&state, &headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    let tenant_name = sqlx::query_scalar::<_, String>("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".into());

    HtmlTemplate(SelfHostTemplate { tenant_name }).into_response()
}

/// Form for the checkout button on the upgrade page.
#[derive(Deserialize)]
struct CheckoutForm {
    /// Harness size: "small", "medium", or "large"
    size: String,
}

/// `POST /billing/checkout` — create Stripe Checkout session for plan upgrade.
async fn billing_checkout(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<CheckoutForm>,
) -> Response {
    let claims = match extract_session_claims(&state, &headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    let (client, stripe_cfg) = match (&state.stripe_client, &state.stripe_config) {
        (Some(c), Some(cfg)) => (c, cfg),
        _ => {
            return Redirect::to("/dashboard?error=Stripe+not+configured").into_response();
        }
    };

    // Resolve price by harness size
    let size = match form.size.as_str() {
        "small" | "medium" | "large" => form.size.as_str(),
        _ => "small",
    };
    let price_id = match stripe_cfg.price_id_for_size(size) {
        Some(p) => p,
        None => {
            return Redirect::to("/billing/upgrade?error=Invalid+plan").into_response();
        }
    };

    // Get or create Stripe customer
    let customer_row = sqlx::query_as::<_, (Option<String>, String, String)>(
        "SELECT stripe_customer_id, u.email, u.name FROM tenants t
         JOIN users u ON u.tenant_id = t.id AND u.role = 'owner'
         WHERE t.id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (stripe_customer_id, email, name) = match customer_row {
        Some(row) => row,
        None => {
            return Redirect::to("/dashboard?error=Tenant+not+found").into_response();
        }
    };

    let customer_id = match stripe_customer_id {
        Some(id) => id,
        None => {
            // Create a new Stripe customer
            match crate::billing::stripe_service::create_customer(client, &email, &name, tenant_id)
                .await
            {
                Ok(id) => {
                    let id_str = id.to_string();
                    let _ = sqlx::query("UPDATE tenants SET stripe_customer_id = $1 WHERE id = $2")
                        .bind(&id_str)
                        .bind(tenant_id)
                        .execute(&state.db)
                        .await;
                    id_str
                }
                Err(e) => {
                    error!("Failed to create Stripe customer: {}", e);
                    return Redirect::to("/dashboard?error=Payment+setup+failed").into_response();
                }
            }
        }
    };

    let base_url = format!(
        "{}://{}:{}",
        if state.config.server.port == 443 {
            "https"
        } else {
            "http"
        },
        state.config.server.host,
        state.config.server.port
    );
    let success_url = format!(
        "{}/billing/success?session_id={{CHECKOUT_SESSION_ID}}",
        base_url
    );
    let cancel_url = format!("{}/billing/upgrade", base_url);

    match crate::billing::stripe_service::create_checkout_session(
        client,
        &customer_id,
        price_id,
        &success_url,
        &cancel_url,
        tenant_id,
    )
    .await
    {
        Ok(url) if !url.is_empty() => Redirect::to(&url).into_response(),
        Ok(_) => Redirect::to("/billing/upgrade?error=Checkout+failed").into_response(),
        Err(e) => {
            error!("Failed to create checkout session: {}", e);
            Redirect::to("/billing/upgrade?error=Checkout+failed").into_response()
        }
    }
}

/// `POST /billing/portal` — redirect to Stripe Customer Portal.
async fn billing_portal(
    State(state): State<PlatformState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let claims = match extract_session_claims(&state, &headers) {
        Some(c) => c,
        None => return Redirect::to("/login").into_response(),
    };

    let tenant_id: Uuid = match claims.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Redirect::to("/login").into_response(),
    };

    let client = match &state.stripe_client {
        Some(c) => c,
        None => {
            return Redirect::to("/settings?msg=Stripe+not+configured").into_response();
        }
    };

    let customer_id: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let customer_id = match customer_id {
        Some(id) => id,
        None => {
            return Redirect::to("/settings?msg=No+billing+account+found").into_response();
        }
    };

    let return_url = format!(
        "{}://{}:{}/settings",
        if state.config.server.port == 443 {
            "https"
        } else {
            "http"
        },
        state.config.server.host,
        state.config.server.port
    );

    match crate::billing::stripe_service::create_portal_session(client, &customer_id, &return_url)
        .await
    {
        Ok(url) => Redirect::to(&url).into_response(),
        Err(e) => {
            error!("Failed to create portal session: {}", e);
            Redirect::to("/settings?msg=Could+not+open+billing+portal").into_response()
        }
    }
}

// ── Logout ──────────────────────────────────────────────────────────────

async fn logout_submit() -> Response {
    let cookie = format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
        SESSION_COOKIE
    );

    ([(header::SET_COOKIE, cookie)], Redirect::to("/login")).into_response()
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn get_jwt_secret(state: &PlatformState) -> String {
    use secrecy::ExposeSecret;
    state.config.auth.jwt_secret.expose_secret().to_string()
}

fn extract_session_claims(
    state: &PlatformState,
    headers: &axum::http::HeaderMap,
) -> Option<auth::Claims> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;

    // Parse cookies to find amos_session
    let token = cookie_header
        .split(';')
        .map(|s| s.trim())
        .find(|s| s.starts_with(&format!("{}=", SESSION_COOKIE)))?
        .strip_prefix(&format!("{}=", SESSION_COOKIE))?;

    if token.is_empty() {
        return None;
    }

    let jwt_secret = get_jwt_secret(state);
    auth::validate_access_token(token, &jwt_secret).ok()
}

/// Wrapper to render Askama templates as HTML responses.
struct HtmlTemplate<T: Template>(T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html,
            )
                .into_response(),
            Err(e) => {
                error!("Template render error: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
            }
        }
    }
}
