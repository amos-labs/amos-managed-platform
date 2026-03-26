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
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    auth,
    provisioning::{alb::AlbRouter, HarnessConfig, InstanceSize},
    state::PlatformState,
};

const SESSION_COOKIE: &str = "amos_session";

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

// ── Template Structs ────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
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
    instances: Vec<HarnessInfo>,
    user_count: i64,
    api_key_count: i64,
    flash_message: Option<String>,
    flash_error: Option<String>,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    tenant_name: String,
    role: String,
    api_keys: Vec<ApiKeyInfo>,
    users: Vec<UserInfo>,
    new_api_key: Option<String>,
    flash_message: Option<String>,
}

// ── View model types ────────────────────────────────────────────────────

#[allow(dead_code)]
struct HarnessInfo {
    id: String,
    full_id: String,
    status: String,
    subdomain: Option<String>,
    region: String,
    instance_size: String,
    healthy: bool,
    endpoint_url: Option<String>,
    container_id_short: Option<String>,
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
}

#[derive(Deserialize)]
struct RegisterForm {
    organization_name: String,
    name: String,
    email: String,
    password: String,
    plan: String,
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
        .route("/dashboard/harness/new", post(deploy_new_harness))
        .route("/dashboard/harness/{id}/start", post(harness_start))
        .route("/dashboard/harness/{id}/stop", post(harness_stop))
        .route("/dashboard/harness/{id}/restart", post(harness_restart))
        .route("/dashboard/harness/{id}/redeploy", post(harness_redeploy))
        .route("/dashboard/harness/{id}/delete", post(harness_delete))
        .route("/logout", post(logout_submit))
}

// ── Login ───────────────────────────────────────────────────────────────

async fn login_page() -> impl IntoResponse {
    HtmlTemplate(LoginTemplate { error: None })
}

async fn login_submit(State(state): State<PlatformState>, Form(form): Form<LoginForm>) -> Response {
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
            })
            .into_response();
        }
        Err(e) => {
            error!("Login query failed: {}", e);
            return HtmlTemplate(LoginTemplate {
                error: Some("An internal error occurred.".into()),
            })
            .into_response();
        }
    };

    let (user_id, tenant_id, password_hash, role, tenant_slug, is_active) = row;

    if !is_active {
        return HtmlTemplate(LoginTemplate {
            error: Some("Account is deactivated.".into()),
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
            })
            .into_response();
        }
    };

    if !valid {
        return HtmlTemplate(LoginTemplate {
            error: Some("Invalid email or password.".into()),
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
            })
            .into_response();
        }
    };

    // Update last_login_at
    let _ = sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await;

    // Set httponly cookie and redirect to dashboard
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_COOKIE, token, access_expiry
    );

    ([(header::SET_COOKIE, cookie)], Redirect::to("/dashboard")).into_response()
}

// ── Register ────────────────────────────────────────────────────────────

async fn register_page() -> impl IntoResponse {
    HtmlTemplate(RegisterTemplate { error: None })
}

async fn register_submit(
    State(state): State<PlatformState>,
    Form(form): Form<RegisterForm>,
) -> Response {
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

    let subdomain = Some(slug.clone());
    let tenant_id = Uuid::new_v4();

    // Create tenant
    let result = sqlx::query(
        "INSERT INTO tenants (id, name, slug, plan, subdomain) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(&form.organization_name)
    .bind(&slug)
    .bind(&form.plan)
    .bind(&subdomain)
    .execute(&state.db)
    .await;

    if let Err(e) = result {
        let err_str = e.to_string();
        if err_str.contains("tenants_slug_key") || err_str.contains("tenants_subdomain_key") {
            return HtmlTemplate(RegisterTemplate {
                error: Some(format!("Organization '{}' is already taken.", slug)),
            })
            .into_response();
        }
        error!("Failed to create tenant: {}", e);
        return HtmlTemplate(RegisterTemplate {
            error: Some("Failed to create organization.".into()),
        })
        .into_response();
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

    // Create harness instance record
    let harness_id = Uuid::new_v4();
    let _ = sqlx::query(
        "INSERT INTO harness_instances (id, tenant_id, subdomain, status)
         VALUES ($1, $2, $3, 'pending')",
    )
    .bind(harness_id)
    .bind(tenant_id)
    .bind(&subdomain)
    .execute(&state.db)
    .await;

    // Provision Docker container for the harness
    if let Some(ref manager) = state.harness_manager {
        let platform_url = format!(
            "http://{}:{}",
            state.config.server.host, state.config.server.port
        );

        // Build env vars the harness container needs.
        // Inside Docker on macOS, host.docker.internal resolves to the host machine.
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
            platform_grpc_url: platform_url,
            env_vars: harness_env,
        };

        info!(tenant_id = %tenant_id, harness_id = %harness_id, "Provisioning harness container for new tenant");

        // Update status to provisioning
        let _ = sqlx::query("UPDATE harness_instances SET status = 'provisioning' WHERE id = $1")
            .bind(harness_id)
            .execute(&state.db)
            .await;

        match manager.provision(&config).await {
            Ok(container_id) => {
                info!(container_id = %container_id, "Harness container provisioned, starting...");

                // Record container_id immediately, keep status as 'provisioning'
                let _ = sqlx::query(
                    "UPDATE harness_instances
                     SET container_id = $1, provisioned_at = NOW()
                     WHERE id = $2",
                )
                .bind(&container_id)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                // Start the container
                if let Err(e) = manager.start(&container_id).await {
                    warn!(
                        "Failed to auto-start harness container {}: {}",
                        container_id, e
                    );
                    let _ =
                        sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                            .bind(harness_id)
                            .execute(&state.db)
                            .await;
                } else {
                    info!(container_id = %container_id, "Harness container start issued");

                    // Wait for the container to become healthy with a retry loop.
                    // The container needs time to boot and bind its port.
                    let mut external_port: Option<i32> = None;
                    let mut final_status = "provisioning";

                    for attempt in 1..=10 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                        // Check if container is still running
                        match manager.get_status(&container_id).await {
                            Ok(crate::provisioning::HarnessStatus::Running) => {
                                // Container is alive, try to get the port
                                match manager.inspect_host_port(&container_id).await {
                                    Ok(Some(port)) => {
                                        info!(
                                            port = port,
                                            attempt = attempt,
                                            "Harness port detected"
                                        );
                                        external_port = Some(port as i32);
                                        final_status = "running";
                                        break;
                                    }
                                    Ok(None) => {
                                        info!(attempt = attempt, "Waiting for port binding...");
                                    }
                                    Err(e) => {
                                        warn!(attempt = attempt, "Port inspect error: {}", e);
                                    }
                                }
                            }
                            Ok(crate::provisioning::HarnessStatus::Error) => {
                                warn!("Harness container exited with error");
                                final_status = "error";
                                break;
                            }
                            Ok(crate::provisioning::HarnessStatus::Stopped) => {
                                warn!("Harness container stopped unexpectedly");
                                final_status = "error";
                                break;
                            }
                            Ok(other) => {
                                info!(attempt = attempt, status = ?other, "Container not yet running");
                            }
                            Err(e) => {
                                warn!(attempt = attempt, "Failed to check container status: {}", e);
                                final_status = "error";
                                break;
                            }
                        }
                    }

                    let internal_url = external_port.map(|p| format!("http://localhost:{}", p));
                    let healthy = final_status == "running" && external_port.is_some();

                    let _ = sqlx::query(
                        "UPDATE harness_instances
                         SET status = $2,
                             started_at = CASE WHEN $2 = 'running' THEN NOW() ELSE NULL END,
                             external_port = $4, internal_url = $5, healthy = $6
                         WHERE id = $3",
                    )
                    .bind(&container_id)
                    .bind(final_status)
                    .bind(harness_id)
                    .bind(external_port)
                    .bind(&internal_url)
                    .bind(healthy)
                    .execute(&state.db)
                    .await;
                }
            }
            Err(e) => {
                error!("Failed to provision harness container: {}", e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
            }
        }
    } else if let Some(ref ecs) = state.ecs_provisioner {
        // Production path: provision harness as an ECS Fargate task.
        info!(tenant_id = %tenant_id, harness_id = %harness_id, "Provisioning harness via ECS Fargate");

        let _ = sqlx::query("UPDATE harness_instances SET status = 'provisioning' WHERE id = $1")
            .bind(harness_id)
            .execute(&state.db)
            .await;

        let ecs_config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(), // Not used by ECS provisioner
            env_vars: HashMap::new(),         // ECS provisioner sets its own env
        };

        match ecs.provision(&ecs_config, &slug).await {
            Ok(task_arn) => {
                info!(task_arn = %task_arn, "ECS task launched for tenant");

                // Store the task ARN as container_id
                let _ = sqlx::query(
                    "UPDATE harness_instances
                     SET container_id = $1, provisioned_at = NOW()
                     WHERE id = $2",
                )
                .bind(&task_arn)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                // Poll ECS in the background until task reaches RUNNING.
                spawn_ecs_status_poller(
                    state.ecs_provisioner.clone().unwrap(),
                    state.db.clone(),
                    task_arn,
                    harness_id,
                    state.alb_router.clone(),
                    subdomain.clone(),
                );
            }
            Err(e) => {
                error!("Failed to provision ECS harness task: {}", e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
            }
        }
    } else {
        warn!(
            "No provisioner available (Docker or ECS) — harness instance created as pending only"
        );
    }

    // Issue JWT and set cookie
    let jwt_secret = get_jwt_secret(&state);
    let access_expiry = state.config.auth.access_token_expiry_secs as i64;

    let token = match auth::create_access_token(
        user_id,
        tenant_id,
        "owner",
        &slug,
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

    ([(header::SET_COOKIE, cookie)], Redirect::to("/dashboard")).into_response()
}

// ── Dashboard ───────────────────────────────────────────────────────────

async fn dashboard_page(
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

    // Fetch tenant info
    let tenant_row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT name, slug, plan FROM tenants WHERE id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (tenant_name, tenant_slug, plan) =
        tenant_row.unwrap_or_else(|| ("Unknown".into(), claims.tenant_slug.clone(), "free".into()));

    // Fetch harness instances (including endpoint info)
    let harness_rows = sqlx::query_as::<_, (Uuid, String, Option<String>, String, String, bool, Option<i32>, Option<String>, Option<String>)>(
        "SELECT id, status, subdomain, region, instance_size, healthy, external_port, internal_url, container_id
         FROM harness_instances WHERE tenant_id = $1 ORDER BY created_at DESC"
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
            )| {
                // Prefer public subdomain URL over internal IP for the endpoint display.
                let endpoint_url = subdomain
                    .as_deref()
                    .map(|s| format!("https://{}.custom.amoslabs.com", s))
                    .or(internal_url);

                HarnessInfo {
                    id: id.to_string()[..8].to_string(),
                    full_id: id.to_string(),
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
        instances,
        user_count,
        api_key_count,
        flash_message: None,
        flash_error: None,
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

    // Tenant name
    let tenant_name: String = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".into());

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

        let ecs_config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: HashMap::new(),
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

async fn deploy_new_harness(
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

    let tenant_slug = claims.tenant_slug.clone();
    let subdomain = Some(format!(
        "{}-{}",
        tenant_slug,
        &Uuid::new_v4().to_string()[..4]
    ));

    // Create harness instance record
    let harness_id = Uuid::new_v4();
    let _ = sqlx::query(
        "INSERT INTO harness_instances (id, tenant_id, subdomain, status)
         VALUES ($1, $2, $3, 'pending')",
    )
    .bind(harness_id)
    .bind(tenant_id)
    .bind(&subdomain)
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

        let ecs_config = HarnessConfig {
            customer_id: tenant_id,
            region: "us-east-1".to_string(),
            instance_size: InstanceSize::Small,
            environment: "production".to_string(),
            platform_grpc_url: String::new(),
            env_vars: HashMap::new(),
        };

        match ecs.provision(&ecs_config, &tenant_slug).await {
            Ok(task_arn) => {
                let _ = sqlx::query("UPDATE harness_instances SET container_id = $1, provisioned_at = NOW() WHERE id = $2")
                    .bind(&task_arn)
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
