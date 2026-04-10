//! Admin dashboard API endpoints.
//!
//! All routes are under `/api/v1/admin` and require the `X-Admin-Key` header
//! to match the `AMOS__ADMIN__API_KEY` environment variable. If the env var
//! is empty or unset, the admin API is disabled (all requests get 401).

use axum::{
    extract::{FromRequestParts, Path, State},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    provisioning::{HarnessConfig, InstanceSize},
    routes::ui::spawn_ecs_status_poller_public,
    state::PlatformState,
};

// ── Auth Extractor ─────────────────────────────────────────────────────

/// Extractor that validates the `X-Admin-Key` header against the configured
/// admin API key.  Rejects the request with 401 if the key is missing,
/// wrong, or the admin API is disabled (empty key).
pub struct AdminAuth;

impl FromRequestParts<PlatformState> for AdminAuth {
    type Rejection = (StatusCode, Json<ErrorResponse>);

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &PlatformState,
    ) -> Result<Self, Self::Rejection> {
        let configured_key = std::env::var("AMOS__ADMIN__API_KEY").unwrap_or_default();

        if configured_key.is_empty() {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Admin API is disabled (AMOS__ADMIN__API_KEY not configured)".into(),
                }),
            ));
        }

        let provided_key = parts
            .headers
            .get("X-Admin-Key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if provided_key != configured_key {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid admin API key".into(),
                }),
            ));
        }

        Ok(AdminAuth)
    }
}

// ── Response/Request Types ─────────────────────────────────────────────

#[derive(Serialize)]
pub struct ErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct TenantSummary {
    id: Uuid,
    name: String,
    email: String,
    organization: Option<String>,
    plan: String,
    max_harnesses: i32,
    stripe_subscription_status: Option<String>,
    created_at: DateTime<Utc>,
    harnesses: Vec<HarnessInfo>,
}

#[derive(Serialize)]
struct HarnessInfo {
    id: Uuid,
    name: Option<String>,
    status: String,
    instance_size: Option<String>,
    internal_url: Option<String>,
    region: Option<String>,
}

#[derive(Serialize)]
struct TenantListResponse {
    tenants: Vec<TenantSummary>,
    total: usize,
}

#[derive(Deserialize)]
struct UpgradePlanRequest {
    plan: String,
    max_harnesses: Option<i32>,
}

#[derive(Serialize)]
struct UpgradePlanResponse {
    id: Uuid,
    plan: String,
    max_harnesses: Option<i32>,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct ProvisionRequest {
    size: Option<String>,
    name: Option<String>,
}

#[derive(Serialize)]
struct ProvisionResponse {
    harness_id: Uuid,
    status: String,
    container_id: Option<String>,
    provisioned_at: DateTime<Utc>,
    provider: String,
}

#[derive(Serialize)]
struct RestartResponse {
    harness_id: Uuid,
    status: String,
    restarted_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct StatsResponse {
    total_tenants: i64,
    total_harnesses: i64,
    active_harnesses: i64,
    harnesses_by_plan: Vec<PlanHarnessCount>,
}

#[derive(Serialize)]
struct PlanHarnessCount {
    plan: String,
    count: i64,
}

// ── Routes ─────────────────────────────────────────────────────────────

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/admin/tenants", get(list_tenants))
        .route("/admin/tenants/{id}", get(get_tenant))
        .route("/admin/tenants/{id}/upgrade", post(upgrade_plan))
        .route("/admin/tenants/{id}/provision", post(provision_harness))
        .route("/admin/harnesses/{id}/restart", post(restart_harness))
        .route("/admin/harnesses/{id}/redeploy", post(redeploy_harness))
        .route("/admin/harnesses/{id}/rename", post(rename_harness))
        .route("/admin/harnesses/{id}/deprovision", post(deprovision_harness))
        .route("/admin/stats", get(get_stats))
}

// ── Handlers ───────────────────────────────────────────────────────────

/// GET /api/v1/admin/tenants
///
/// List all tenants with their harness instances.
async fn list_tenants(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    // First, fetch all tenants with owner email.
    let tenant_rows = sqlx::query_as::<_, (Uuid, String, String, Option<String>, String, i32, Option<String>, DateTime<Utc>)>(
        r#"
        SELECT
            t.id,
            t.name,
            COALESCE(u.email, ''),
            t.slug,
            t.plan,
            t.max_harnesses,
            t.stripe_subscription_status,
            t.created_at
        FROM tenants t
        LEFT JOIN users u ON u.tenant_id = t.id AND u.role = 'owner'
        ORDER BY t.created_at DESC
        "#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to query tenants: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    // Fetch all harness instances in one query.
    let harness_rows = sqlx::query_as::<_, (Uuid, Uuid, Option<String>, String, Option<String>, Option<String>, Option<String>)>(
        r#"
        SELECT
            h.id,
            h.tenant_id,
            h.name,
            h.status,
            h.instance_size,
            h.internal_url,
            h.region
        FROM harness_instances h
        WHERE h.status != 'deprovisioned'
        ORDER BY h.tenant_id, h.name
        "#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to query harness instances: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    // Group harnesses by tenant_id.
    let mut harness_map: std::collections::HashMap<Uuid, Vec<HarnessInfo>> =
        std::collections::HashMap::new();
    for (id, tenant_id, name, status, instance_size, internal_url, region) in harness_rows {
        harness_map
            .entry(tenant_id)
            .or_default()
            .push(HarnessInfo {
                id,
                name,
                status,
                instance_size,
                internal_url,
                region,
            });
    }

    let tenants: Vec<TenantSummary> = tenant_rows
        .into_iter()
        .map(
            |(id, name, email, slug, plan, max_harnesses, stripe_status, created_at)| TenantSummary {
                id,
                name,
                email,
                organization: slug,
                plan,
                max_harnesses,
                stripe_subscription_status: stripe_status,
                created_at,
                harnesses: harness_map.remove(&id).unwrap_or_default(),
            },
        )
        .collect();

    let total = tenants.len();

    Ok(Json(TenantListResponse { tenants, total }))
}

/// GET /api/v1/admin/tenants/{id}
///
/// Get a single tenant with full harness info.
async fn get_tenant(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let tenant_row = sqlx::query_as::<_, (Uuid, String, String, Option<String>, String, i32, Option<String>, DateTime<Utc>)>(
        r#"
        SELECT
            t.id,
            t.name,
            COALESCE(u.email, ''),
            t.slug,
            t.plan,
            t.max_harnesses,
            t.stripe_subscription_status,
            t.created_at
        FROM tenants t
        LEFT JOIN users u ON u.tenant_id = t.id AND u.role = 'owner'
        WHERE t.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    let (tenant_id, name, email, slug, plan, max_harnesses, stripe_status, created_at) =
        tenant_row.ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "Tenant not found".into(),
                }),
            )
        })?;

    let harness_rows = sqlx::query_as::<_, (Uuid, Option<String>, String, Option<String>, Option<String>, Option<String>)>(
        r#"
        SELECT h.id, h.name, h.status, h.instance_size, h.internal_url, h.region
        FROM harness_instances h
        WHERE h.tenant_id = $1 AND h.status != 'deprovisioned'
        ORDER BY h.name
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    let harnesses: Vec<HarnessInfo> = harness_rows
        .into_iter()
        .map(|(id, name, status, instance_size, internal_url, region)| HarnessInfo {
            id,
            name,
            status,
            instance_size,
            internal_url,
            region,
        })
        .collect();

    Ok(Json(TenantSummary {
        id: tenant_id,
        name,
        email,
        organization: slug,
        plan,
        max_harnesses,
        stripe_subscription_status: stripe_status,
        created_at,
        harnesses,
    }))
}

/// POST /api/v1/admin/tenants/{id}/upgrade
///
/// Admin override to upgrade a tenant's plan (no Stripe checkout).
async fn upgrade_plan(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpgradePlanRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    // Validate plan value.
    let plan = match req.plan.as_str() {
        "free" | "hosted" => &req.plan,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid plan '{}'. Must be 'free' or 'hosted'.", req.plan),
                }),
            ));
        }
    };

    let result = sqlx::query(
        r#"
        UPDATE tenants
        SET plan = $1, max_harnesses = COALESCE($2, max_harnesses), updated_at = NOW()
        WHERE id = $3
        "#,
    )
    .bind(plan)
    .bind(req.max_harnesses)
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    if result.rows_affected() == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Tenant not found".into(),
            }),
        ));
    }

    tracing::info!(
        tenant_id = %id,
        plan = %plan,
        max_harnesses = ?req.max_harnesses,
        "Admin upgraded tenant plan"
    );

    Ok(Json(UpgradePlanResponse {
        id,
        plan: plan.clone(),
        max_harnesses: req.max_harnesses,
        updated_at: Utc::now(),
    }))
}

/// POST /api/v1/admin/tenants/{id}/provision
///
/// Admin-triggered harness provisioning for a tenant.
async fn provision_harness(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
    Path(tenant_id): Path<Uuid>,
    Json(req): Json<ProvisionRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let instance_size = match req.size.as_deref() {
        Some("medium") => InstanceSize::Medium,
        Some("large") => InstanceSize::Large,
        _ => InstanceSize::Small,
    };

    let harness_id = Uuid::new_v4();
    let harness_name = req
        .name
        .unwrap_or_else(|| format!("harness-{}", &harness_id.to_string()[..8]));

    let size_str = match instance_size {
        InstanceSize::Small => "small",
        InstanceSize::Medium => "medium",
        InstanceSize::Large => "large",
    };

    // Check if tenant already has a primary harness.
    let has_primary = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM harness_instances WHERE tenant_id = $1 AND harness_role = 'primary' AND status != 'deprovisioned'",
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c > 0)
    .unwrap_or(false);

    let harness_role = if has_primary { "specialist" } else { "primary" };

    // Look up tenant slug for subdomain generation.
    let tenant_slug: String = sqlx::query_scalar("SELECT slug FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or_else(|_| format!("{}", &tenant_id.to_string()[..8]));

    // Generate subdomain: {slug}-{4char} (matches normal provisioning flow).
    let subdomain = format!("{}-{}", tenant_slug, &harness_id.to_string()[..4]);

    // Insert the harness record.
    sqlx::query(
        r#"
        INSERT INTO harness_instances
            (id, tenant_id, name, harness_role, status, instance_size, environment, subdomain)
        VALUES ($1, $2, $3, $5, 'provisioning', $4, 'production', $6)
        "#,
    )
    .bind(harness_id)
    .bind(tenant_id)
    .bind(&harness_name)
    .bind(size_str)
    .bind(harness_role)
    .bind(&subdomain)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to insert harness instance: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    let platform_url = format!(
        "http://{}:{}",
        state.config.server.host, state.config.server.port
    );

    let config = HarnessConfig {
        customer_id: tenant_id,
        region: "us-west-2".into(),
        instance_size,
        environment: "production".into(),
        platform_grpc_url: platform_url,
        env_vars: std::collections::HashMap::new(),
        harness_role: harness_role.to_string(),
        packages: vec![],
        harness_id: Some(harness_id),
    };

    let mut container_id: Option<String> = None;
    #[allow(unused_assignments)]
    let mut provider = "none".to_string();

    if let Some(manager) = state.harness_manager.as_ref() {
        match manager.provision(&config).await {
            Ok(cid) => {
                if let Err(e) = manager.start(&cid).await {
                    tracing::error!("Failed to auto-start harness: {}", e);
                }
                container_id = Some(cid.clone());
                provider = "docker".to_string();

                let _ = sqlx::query(
                    "UPDATE harness_instances SET container_id = $1, status = 'running' WHERE id = $2",
                )
                .bind(&cid)
                .bind(harness_id)
                .execute(&state.db)
                .await;
            }
            Err(e) => {
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("Docker provisioning failed: {}", e),
                    }),
                ));
            }
        }
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        let slug = format!("{}-{}", tenant_id, harness_name);
        match ecs.provision(&config, &slug).await {
            Ok(task_arn) => {
                container_id = Some(task_arn.clone());
                provider = "ecs".to_string();
                let _ = sqlx::query(
                    "UPDATE harness_instances SET container_id = $1, status = 'provisioning' WHERE id = $2",
                )
                .bind(&task_arn)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                // Spawn background poller: monitors ECS task, sets up ALB routing,
                // updates status to 'running' once the harness is reachable.
                spawn_ecs_status_poller_public(
                    ecs.clone(),
                    state.db.clone(),
                    task_arn.clone(),
                    harness_id,
                    state.alb_router.clone(),
                    Some(subdomain.clone()),
                );
            }
            Err(e) => {
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("ECS provisioning failed: {}", e),
                    }),
                ));
            }
        }
    } else {
        // No provisioner: record stays in DB with 'provisioning' status.
        // This is acceptable for local dev without Docker.
        provider = "none".to_string();
        let _ = sqlx::query(
            "UPDATE harness_instances SET status = 'pending' WHERE id = $1",
        )
        .bind(harness_id)
        .execute(&state.db)
        .await;
    }

    tracing::info!(
        harness_id = %harness_id,
        tenant_id = %tenant_id,
        provider = %provider,
        "Admin provisioned harness"
    );

    Ok((
        StatusCode::CREATED,
        Json(ProvisionResponse {
            harness_id,
            status: "provisioning".into(),
            container_id,
            provisioned_at: Utc::now(),
            provider,
        }),
    ))
}

/// POST /api/v1/admin/harnesses/{id}/restart
///
/// Stop and then start a harness container.
async fn restart_harness(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
    Path(harness_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    // Look up container_id from DB.
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT container_id FROM harness_instances WHERE id = $1",
    )
    .bind(harness_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    let container_id = row.and_then(|(cid,)| cid).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Harness not found or has no container".into(),
            }),
        )
    })?;

    if let Some(manager) = state.harness_manager.as_ref() {
        let _ = manager.stop(&container_id).await;
        manager.start(&container_id).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to restart: {}", e),
                }),
            )
        })?;
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        // For ECS, stop then re-run is not straightforward. Just stop for now.
        ecs.stop(&container_id).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to stop ECS task: {}", e),
                }),
            )
        })?;
        // ECS tasks can't be restarted; a new task must be provisioned.
        // For now, update status and return.
        let _ = sqlx::query(
            "UPDATE harness_instances SET status = 'stopped', stopped_at = NOW() WHERE id = $1",
        )
        .bind(harness_id)
        .execute(&state.db)
        .await;

        return Ok(Json(RestartResponse {
            harness_id,
            status: "stopped (ECS tasks require re-provision to restart)".into(),
            restarted_at: Utc::now(),
        }));
    } else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No provisioner available".into(),
            }),
        ));
    }

    // Update DB status.
    let _ = sqlx::query(
        "UPDATE harness_instances SET status = 'running', started_at = NOW() WHERE id = $1",
    )
    .bind(harness_id)
    .execute(&state.db)
    .await;

    tracing::info!(harness_id = %harness_id, "Admin restarted harness");

    Ok(Json(RestartResponse {
        harness_id,
        status: "running".into(),
        restarted_at: Utc::now(),
    }))
}

/// POST /api/v1/admin/harnesses/{id}/rename
///
/// Rename a harness instance.
async fn rename_harness(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
    Path(harness_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let name = body["name"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Missing 'name' field".into(),
            }),
        )
    })?;

    let rows = sqlx::query("UPDATE harness_instances SET name = $1 WHERE id = $2")
        .bind(name)
        .bind(harness_id)
        .execute(&state.db)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Database error: {}", e),
                }),
            )
        })?;

    if rows.rows_affected() == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Harness not found".into(),
            }),
        ));
    }

    tracing::info!(harness_id = %harness_id, new_name = %name, "Admin renamed harness");

    Ok(Json(serde_json::json!({
        "harness_id": harness_id,
        "name": name
    })))
}

/// POST /api/v1/admin/harnesses/{id}/redeploy
///
/// Stop the current harness task, create a per-harness isolated database
/// (if one doesn't already exist), and provision a new ECS task.
async fn redeploy_harness(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
    Path(harness_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    // Look up harness + tenant info.
    let row = sqlx::query_as::<_, (Option<String>, Uuid, String)>(
        "SELECT h.container_id, h.tenant_id, t.slug \
         FROM harness_instances h JOIN tenants t ON t.id = h.tenant_id \
         WHERE h.id = $1",
    )
    .bind(harness_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    let (container_id, tenant_id, tenant_slug) = row.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Harness not found".into(),
            }),
        )
    })?;

    let ecs = state.ecs_provisioner.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "ECS provisioner not available".into(),
            }),
        )
    })?;

    // Stop old task.
    if let Some(cid) = &container_id {
        let _ = ecs.stop(cid).await;
    }

    // Tear down ALB routing.
    crate::routes::ui::teardown_alb_routing_public(&state, harness_id).await;

    let _ = sqlx::query(
        "UPDATE harness_instances SET status = 'provisioning', healthy = FALSE WHERE id = $1",
    )
    .bind(harness_id)
    .execute(&state.db)
    .await;

    // Resolve per-harness isolated database.
    let base_db_url = ecs.harness_database_url();
    let harness_env =
        crate::routes::ui::resolve_harness_database_public(&state.db, base_db_url, harness_id)
            .await;

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

    let task_arn = ecs.provision(&ecs_config, &tenant_slug).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to provision: {}", e),
            }),
        )
    })?;

    let _ = sqlx::query(
        "UPDATE harness_instances SET container_id = $1, provisioned_at = NOW() WHERE id = $2",
    )
    .bind(&task_arn)
    .bind(harness_id)
    .execute(&state.db)
    .await;

    // Fetch subdomain for ALB routing.
    let subdomain: Option<String> =
        sqlx::query_scalar("SELECT subdomain FROM harness_instances WHERE id = $1")
            .bind(harness_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    // Poll ECS in the background.
    spawn_ecs_status_poller_public(
        ecs.clone(),
        state.db.clone(),
        task_arn.clone(),
        harness_id,
        state.alb_router.clone(),
        subdomain,
    );

    tracing::info!(harness_id = %harness_id, task_arn = %task_arn, "Admin redeployed harness");

    Ok(Json(serde_json::json!({
        "harness_id": harness_id,
        "task_arn": task_arn,
        "status": "provisioning"
    })))
}

/// POST /api/v1/admin/harnesses/{id}/deprovision
///
/// Mark a harness as deprovisioned and stop its container/task if running.
async fn deprovision_harness(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
    Path(harness_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let row = sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT container_id, status FROM harness_instances WHERE id = $1",
    )
    .bind(harness_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    let (container_id, current_status) = row.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Harness not found".into(),
            }),
        )
    })?;

    // Try to stop the container/task if it has one
    if let Some(cid) = &container_id {
        if current_status != "stopped" && current_status != "error" {
            if let Some(ecs) = state.ecs_provisioner.as_ref() {
                let _ = ecs.stop(cid).await;
            } else if let Some(manager) = state.harness_manager.as_ref() {
                let _ = manager.stop(cid).await;
            }
        }
    }

    sqlx::query(
        "UPDATE harness_instances SET status = 'deprovisioned', stopped_at = NOW() WHERE id = $1",
    )
    .bind(harness_id)
    .execute(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Database error: {}", e),
            }),
        )
    })?;

    tracing::info!(harness_id = %harness_id, "Admin deprovisioned harness");

    Ok(Json(serde_json::json!({
        "harness_id": harness_id,
        "status": "deprovisioned"
    })))
}

/// GET /api/v1/admin/stats
///
/// Aggregate statistics for the admin dashboard.
async fn get_stats(
    _auth: AdminAuth,
    State(state): State<PlatformState>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let total_tenants = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM tenants")
        .fetch_one(&state.db)
        .await
        .map(|(c,)| c)
        .unwrap_or(0);

    let total_harnesses = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM harness_instances WHERE status != 'deprovisioned'",
    )
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    let active_harnesses = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM harness_instances WHERE status = 'running'",
    )
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    let plan_rows = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT t.plan, COUNT(h.id)
        FROM tenants t
        LEFT JOIN harness_instances h ON h.tenant_id = t.id AND h.status != 'deprovisioned'
        GROUP BY t.plan
        ORDER BY t.plan
        "#,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let harnesses_by_plan: Vec<PlanHarnessCount> = plan_rows
        .into_iter()
        .map(|(plan, count)| PlanHarnessCount { plan, count })
        .collect();

    Ok(Json(StatsResponse {
        total_tenants,
        total_harnesses,
        active_harnesses,
        harnesses_by_plan,
    }))
}
