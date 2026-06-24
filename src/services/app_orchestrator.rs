//! Shared orchestration for multi-service app deployments.
//!
//! Both the MCP control plane (`src/mcp/mod.rs`) and the REST API
//! (`src/routes/apps.rs`) drive deployments through these functions, so the
//! DB-write + Docker-provision logic lives in exactly one place. Each function
//! takes an already-resolved `tenant_id` (callers handle auth/scoping) and
//! returns a JSON value suitable for either a tool result or an HTTP body.

use serde_json::{json, Value};
use uuid::Uuid;

use amos_core::{AmosError, Result};

use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
use crate::provisioning::app_spec::AppSpec;
use crate::provisioning::aws_app::AwsDeployTarget;
use crate::provisioning::compose_intake;
use crate::state::PlatformState;

/// Deploy an app from a ready `AppSpec`.
///
/// Provisioning runs in the **background**: this records the deployment +
/// service rows (status `provisioning`), spawns the Docker work (image
/// pull/build can take minutes), and returns immediately so the call never
/// trips the platform's HTTP timeout. Callers watch progress via
/// [`app_status`] / [`app_logs`]; a failure lands as `status = 'error'`.
///
/// `actor` is the authenticated subject (user/agent id) initiating the deploy;
/// it is recorded on the [`proof::OperationReceipt`] emitted when provisioning
/// finishes, so the action is accountable.
pub async fn deploy_app(
    state: &PlatformState,
    tenant_id: Uuid,
    actor: &str,
    spec: AppSpec,
) -> Result<Value> {
    // Fail fast if there's no provisioner at all.
    if state.app_manager.is_none() {
        return Err(AmosError::Internal(
            "no app provisioner available (Docker not connected)".into(),
        ));
    }

    spec.validate().map_err(AmosError::Validation)?;
    let spec_json = serde_json::to_value(&spec)
        .map_err(|e| AmosError::Internal(format!("failed to serialize spec: {e}")))?;

    let deployment_id = Uuid::new_v4();

    // Record the deployment + one row per service (status 'provisioning').
    sqlx::query(
        "INSERT INTO app_deployments (id, tenant_id, name, spec, provider, status)
         VALUES ($1, $2, $3, $4, 'docker', 'provisioning')",
    )
    .bind(deployment_id)
    .bind(tenant_id)
    .bind(&spec.name)
    .bind(&spec_json)
    .execute(&state.db)
    .await
    .map_err(AmosError::Database)?;

    for svc in &spec.services {
        sqlx::query(
            "INSERT INTO app_services (deployment_id, service_name, expose_public, status)
             VALUES ($1, $2, $3, 'provisioning')",
        )
        .bind(deployment_id)
        .bind(&svc.name)
        .bind(svc.expose_public)
        .execute(&state.db)
        .await
        .map_err(AmosError::Database)?;
    }

    // Spawn the heavy Docker work; return the accepted deployment immediately.
    let bg_state = state.clone();
    let bg_spec = spec.clone();
    let bg_actor = actor.to_string();
    tokio::spawn(async move {
        run_provision(&bg_state, deployment_id, tenant_id, &bg_actor, bg_spec).await;
    });

    Ok(json!({
        "deployment_id": deployment_id,
        "name": spec.name,
        "status": "provisioning",
        "message": "Deployment accepted; provisioning in the background. Poll app_status (and app_logs per service) to watch progress.",
        "services": spec.services.iter().map(|s| json!({
            "service_name": s.name,
            "status": "provisioning",
            "expose_public": s.expose_public,
        })).collect::<Vec<_>>(),
    }))
}

/// Background worker: run the Docker provision and persist results/status.
///
/// Emits a [`proof::OperationReceipt`] on completion (success *or* failure) so
/// every deploy is accountable — the receipt records what was provisioned,
/// which guardrails applied, and which services came up.
async fn run_provision(
    state: &PlatformState,
    deployment_id: Uuid,
    tenant_id: Uuid,
    actor: &str,
    spec: AppSpec,
) {
    let Some(app_manager) = state.app_manager.as_ref() else {
        return;
    };

    // The intent + guardrails are the same whether or not provisioning succeeds.
    let intent = Intent {
        summary: format!(
            "Deploy app '{}' ({} service(s)) on local Docker",
            spec.name,
            spec.services.len()
        ),
        self_modifying: false,
        scope_classification: "app_lifecycle".to_string(),
    };
    let base_receipt = || {
        OperationReceipt::new("deploy_app", tenant_id, actor, intent.clone())
            // Each deployment gets its own Docker network — services reach each
            // other by name and are isolated from other tenants' apps.
            .guardrail("tenant_isolation")
            .guardrail("managed_lifecycle")
            .inputs(json!({
                "provider": "docker",
                "name": spec.name,
                "deployment_id": deployment_id,
                "services": spec.services.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            }))
    };

    let result = match app_manager.deploy(deployment_id, tenant_id, &spec).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(%deployment_id, "app provisioning failed: {e}");
            let _ = sqlx::query("UPDATE app_deployments SET status = 'error' WHERE id = $1")
                .bind(deployment_id)
                .execute(&state.db)
                .await;

            // Accountability for failures matters as much as for successes.
            let receipt = base_receipt()
                .check("provision", CheckStatus::Failed, e.to_string())
                .summary(format!(
                    "Deploy of '{}' failed during provisioning",
                    spec.name
                ));
            if let Err(emit_err) = proof::emit(&state.db, &receipt).await {
                tracing::warn!(%deployment_id, "failed to emit deploy receipt: {emit_err}");
            }
            return;
        }
    };

    for svc in &result.services {
        let host_ports = serde_json::to_value(&svc.host_ports).unwrap_or_else(|_| json!([]));
        let _ = sqlx::query(
            "UPDATE app_services
             SET container_id = $1, container_name = $2, image = $3,
                 host_ports = $4, status = 'running'
             WHERE deployment_id = $5 AND service_name = $6",
        )
        .bind(&svc.container_id)
        .bind(&svc.container_name)
        .bind(&svc.image)
        .bind(&host_ports)
        .bind(deployment_id)
        .bind(&svc.service_name)
        .execute(&state.db)
        .await;
    }

    let _ =
        sqlx::query("UPDATE app_deployments SET network_id = $1, status = 'running' WHERE id = $2")
            .bind(&result.network_id)
            .bind(deployment_id)
            .execute(&state.db)
            .await;

    // Emit the success receipt: one passed check per service that came up.
    let mut receipt = base_receipt()
        .outputs(json!({
            "deployment_id": deployment_id,
            "network_id": result.network_id,
            "services": result.services.iter().map(|s| json!({
                "service_name": s.service_name,
                "container_id": s.container_id,
                "host_ports": s.host_ports,
            })).collect::<Vec<_>>(),
        }))
        .summary(format!(
            "Deployed '{}': {}/{} service(s) running",
            spec.name,
            result.services.len(),
            spec.services.len()
        ));
    for svc in &result.services {
        receipt = receipt.check(
            format!("service_running:{}", svc.service_name),
            CheckStatus::Passed,
            format!("container {}", svc.container_id),
        );
    }
    if let Err(emit_err) = proof::emit(&state.db, &receipt).await {
        tracing::warn!(%deployment_id, "failed to emit deploy receipt: {emit_err}");
    }
}

/// Deploy an app to AWS Fargate from a ready `AppSpec` + per-tenant infra
/// target. Like [`deploy_app`], provisioning runs in the background and the
/// call returns immediately; a [`proof::OperationReceipt`] is emitted on
/// completion. Requires the AWS app provisioner (gated on `AWS_APP_ENABLED`).
/// Fire-and-forget completion ping-back: POST a JSON payload to `url`. Used so
/// the system *notifies* on async-op completion instead of making the caller
/// poll. Failures are logged, never fatal.
pub(crate) async fn ping_back(url: &str, payload: Value) {
    let client = reqwest::Client::new();
    match client
        .post(url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(resp) => tracing::info!(%url, status = %resp.status(), "ping-back delivered"),
        Err(e) => tracing::warn!(%url, "ping-back failed: {e}"),
    }
}

pub async fn deploy_app_aws(
    state: &PlatformState,
    tenant_id: Uuid,
    actor: &str,
    spec: AppSpec,
    target: AwsDeployTarget,
    callback_url: Option<String>,
) -> Result<Value> {
    if state.aws_app_provisioner.is_none() {
        return Err(AmosError::Internal(
            "AWS app provisioner not available (set AWS_APP_ENABLED=true)".into(),
        ));
    }
    spec.validate().map_err(AmosError::Validation)?;
    let spec_json = serde_json::to_value(&spec)
        .map_err(|e| AmosError::Internal(format!("failed to serialize spec: {e}")))?;

    let deployment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO app_deployments (id, tenant_id, name, spec, provider, status)
         VALUES ($1, $2, $3, $4, 'ecs', 'provisioning')",
    )
    .bind(deployment_id)
    .bind(tenant_id)
    .bind(&spec.name)
    .bind(&spec_json)
    .execute(&state.db)
    .await
    .map_err(AmosError::Database)?;

    for svc in &spec.services {
        sqlx::query(
            "INSERT INTO app_services (deployment_id, service_name, expose_public, status)
             VALUES ($1, $2, $3, 'provisioning')",
        )
        .bind(deployment_id)
        .bind(&svc.name)
        .bind(svc.expose_public)
        .execute(&state.db)
        .await
        .map_err(AmosError::Database)?;
    }

    let bg_state = state.clone();
    let bg_spec = spec.clone();
    let bg_actor = actor.to_string();
    tokio::spawn(async move {
        run_provision_aws(
            &bg_state,
            deployment_id,
            tenant_id,
            &bg_actor,
            bg_spec,
            target,
            callback_url,
        )
        .await;
    });

    Ok(json!({
        "deployment_id": deployment_id,
        "name": spec.name,
        "provider": "aws",
        "status": "provisioning",
        "message": "AWS deploy accepted; rendering to ECS in the background. Poll app_status, or pass callback_url to be pinged on completion. A proof receipt is emitted either way.",
    }))
}

/// Background worker for the AWS path: render to ECS, persist ARNs, emit a receipt.
async fn run_provision_aws(
    state: &PlatformState,
    deployment_id: Uuid,
    tenant_id: Uuid,
    actor: &str,
    spec: AppSpec,
    target: AwsDeployTarget,
    callback_url: Option<String>,
) {
    let Some(provisioner) = state.aws_app_provisioner.as_ref() else {
        return;
    };

    let intent = Intent {
        summary: format!(
            "Deploy app '{}' to AWS Fargate ({} service(s))",
            spec.name,
            spec.services.len()
        ),
        self_modifying: false,
        scope_classification: "app_lifecycle".to_string(),
    };
    let base = || {
        OperationReceipt::new("deploy_app", tenant_id, actor, intent.clone())
            .guardrail("tenant_isolation")
            .guardrail("no_plaintext_secrets")
            .guardrail("managed_lifecycle")
            .inputs(json!({
                "provider": "aws",
                "name": spec.name,
                "deployment_id": deployment_id,
                "services": spec.services.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            }))
    };

    match provisioner.deploy(&spec, &target).await {
        Ok(result) => {
            let aws_meta = json!({
                "task_def_arn": result.task_def_arn,
                "service_arn": result.service_arn,
                "public_url": result.public_url,
                "managed_skipped": result.managed_skipped,
            });
            let _ = sqlx::query(
                "UPDATE app_deployments SET status = 'running', aws_meta = $1 WHERE id = $2",
            )
            .bind(&aws_meta)
            .bind(deployment_id)
            .execute(&state.db)
            .await;
            // Non-managed services are now running under the ECS task.
            let _ =
                sqlx::query("UPDATE app_services SET status = 'running' WHERE deployment_id = $1")
                    .bind(deployment_id)
                    .execute(&state.db)
                    .await;

            let mut receipt = base()
                .check(
                    "task_definition_registered",
                    CheckStatus::Passed,
                    result.task_def_arn.clone(),
                )
                .check(
                    "ecs_service_deployed",
                    CheckStatus::Passed,
                    result
                        .service_arn
                        .clone()
                        .unwrap_or_else(|| result.service_name.clone()),
                )
                .outputs(aws_meta)
                .summary(format!(
                    "Deployed '{}' to AWS Fargate; {} managed resource(s) skipped",
                    spec.name,
                    result.managed_skipped.len()
                ));
            if !result.managed_skipped.is_empty() {
                receipt = receipt.check(
                    "managed_resources_injected",
                    CheckStatus::Passed,
                    format!(
                        "skipped as containers: {}",
                        result.managed_skipped.join(", ")
                    ),
                );
            }
            if let Err(e) = proof::emit(&state.db, &receipt).await {
                tracing::warn!(%deployment_id, "failed to emit aws deploy receipt: {e}");
            }
            if let Some(url) = &callback_url {
                ping_back(
                    url,
                    json!({
                        "event": "deploy_app.completed", "deployment_id": deployment_id,
                        "name": spec.name, "provider": "aws", "verified": receipt.verified(),
                        "summary": receipt.result_summary, "status": "running",
                    }),
                )
                .await;
            }
        }
        Err(e) => {
            tracing::error!(%deployment_id, "aws provisioning failed: {e}");
            let _ = sqlx::query("UPDATE app_deployments SET status = 'error' WHERE id = $1")
                .bind(deployment_id)
                .execute(&state.db)
                .await;
            let receipt = base()
                .check("ecs_service_deployed", CheckStatus::Failed, e.to_string())
                .summary(format!("AWS deploy of '{}' failed", spec.name));
            if let Err(emit_err) = proof::emit(&state.db, &receipt).await {
                tracing::warn!(%deployment_id, "failed to emit aws deploy receipt: {emit_err}");
            }
            if let Some(url) = &callback_url {
                ping_back(
                    url,
                    json!({
                        "event": "deploy_app.failed", "deployment_id": deployment_id,
                        "name": spec.name, "provider": "aws", "verified": false,
                        "summary": receipt.result_summary, "status": "error",
                    }),
                )
                .await;
            }
        }
    }
}

/// Deploy from a raw compose.yml string.
pub async fn deploy_from_compose(
    state: &PlatformState,
    tenant_id: Uuid,
    actor: &str,
    compose_yaml: &str,
    compose_dir: &std::path::Path,
) -> Result<Value> {
    let spec = compose_intake::app_spec_from_compose_str(compose_yaml, compose_dir)
        .map_err(AmosError::Validation)?;
    deploy_app(state, tenant_id, actor, spec).await
}

/// List a tenant's app deployments (excluding deprovisioned).
pub async fn list_apps(state: &PlatformState, tenant_id: Uuid) -> Result<Value> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, Option<String>)>(
        "SELECT id, name, status, network_id
         FROM app_deployments
         WHERE tenant_id = $1 AND status != 'deprovisioned'
         ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(AmosError::Database)?;

    let mut apps = Vec::new();
    for (id, name, status, network_id) in rows {
        let services = service_rows(state, id).await?;
        apps.push(json!({
            "deployment_id": id,
            "name": name,
            "status": status,
            "network_id": network_id,
            "services": services,
        }));
    }
    Ok(json!({ "tenant_id": tenant_id, "count": apps.len(), "apps": apps }))
}

/// Live status of one deployment: DB rows joined with live container state.
pub async fn app_status(
    state: &PlatformState,
    tenant_id: Uuid,
    deployment_id: Uuid,
) -> Result<Value> {
    let (name, db_status, network_id) = deployment_row(state, tenant_id, deployment_id).await?;
    let app_manager = state.app_manager.as_ref();

    // Provider determines where "live" status comes from.
    let provider: String = sqlx::query_scalar("SELECT provider FROM app_deployments WHERE id = $1")
        .bind(deployment_id)
        .fetch_one(&state.db)
        .await
        .map_err(AmosError::Database)?;

    // AWS deploys: surface the live ECS rollout (state, running/desired,
    // per-container status) so deploy progress is visible from app_status.
    if provider == "ecs" {
        let aws = match state.aws_app_provisioner.as_ref() {
            Some(p) => p
                .service_status(&name)
                .await
                .unwrap_or_else(|e| json!({ "error": format!("ECS status unavailable: {e}") })),
            None => json!({ "error": "AWS app provisioner not available" }),
        };
        let svc_rows = sqlx::query_as::<_, (String, bool, String)>(
            "SELECT service_name, expose_public, status FROM app_services
             WHERE deployment_id = $1 ORDER BY service_name",
        )
        .bind(deployment_id)
        .fetch_all(&state.db)
        .await
        .map_err(AmosError::Database)?;
        return Ok(json!({
            "deployment_id": deployment_id,
            "name": name,
            "provider": "aws",
            "status": db_status,
            "aws": aws,
            "services": svc_rows.iter().map(|(n, pub_, st)| json!({
                "service_name": n, "expose_public": pub_, "db_status": st,
            })).collect::<Vec<_>>(),
        }));
    }

    let svc_rows = sqlx::query_as::<_, (String, Option<String>, bool, Value, String)>(
        "SELECT service_name, container_id, expose_public, host_ports, status
         FROM app_services WHERE deployment_id = $1 ORDER BY service_name",
    )
    .bind(deployment_id)
    .fetch_all(&state.db)
    .await
    .map_err(AmosError::Database)?;

    let mut services = Vec::new();
    for (service_name, container_id, expose_public, host_ports, status) in svc_rows {
        let live = match (app_manager, &container_id) {
            (Some(mgr), Some(cid)) => mgr
                .container_status(cid)
                .await
                .unwrap_or_else(|_| "unknown".to_string()),
            _ => "no-container".to_string(),
        };
        services.push(json!({
            "service_name": service_name,
            "db_status": status,
            "live_status": live,
            "expose_public": expose_public,
            "host_ports": host_ports,
        }));
    }

    Ok(json!({
        "deployment_id": deployment_id,
        "name": name,
        "status": db_status,
        "network_id": network_id,
        "services": services,
    }))
}

/// Recent logs for one service in a deployment.
pub async fn app_logs(
    state: &PlatformState,
    tenant_id: Uuid,
    deployment_id: Uuid,
    service: &str,
    tail: usize,
) -> Result<Value> {
    // Validate ownership + resolve the provider and app name.
    let (name, provider) = sqlx::query_as::<_, (String, String)>(
        "SELECT name, provider FROM app_deployments WHERE id = $1 AND tenant_id = $2",
    )
    .bind(deployment_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(AmosError::Database)?
    .ok_or_else(|| AmosError::NotFound {
        entity: "app deployment".into(),
        id: deployment_id.to_string(),
    })?;

    // AWS deploys: pull the service's logs straight from CloudWatch — so the
    // operator debugs from the MCP tool, never the AWS console/CLI.
    if provider == "ecs" {
        let provisioner = state.aws_app_provisioner.as_ref().ok_or_else(|| {
            AmosError::Internal("AWS app provisioner not available for log fetch".into())
        })?;
        let logs = provisioner.logs(&name, service, tail).await?;
        return Ok(json!({
            "deployment_id": deployment_id, "service": service, "provider": "aws",
            "source": format!("cloudwatch:/ecs/{name}"), "logs": logs,
        }));
    }

    // Local Docker: read the container's logs via the Docker API.
    let app_manager = state
        .app_manager
        .as_ref()
        .ok_or_else(|| AmosError::Internal("no app provisioner available".into()))?;

    let container_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT container_id FROM app_services WHERE deployment_id = $1 AND service_name = $2",
    )
    .bind(deployment_id)
    .bind(service)
    .fetch_optional(&state.db)
    .await
    .map_err(AmosError::Database)?
    .flatten()
    .ok_or_else(|| AmosError::NotFound {
        entity: "app service container".into(),
        id: service.to_string(),
    })?;

    let logs = app_manager.logs(&container_id, tail).await?;
    Ok(
        json!({ "deployment_id": deployment_id, "service": service, "provider": "docker", "logs": logs }),
    )
}

/// start / stop / restart / deprovision a deployment (or a single service).
pub async fn app_control(
    state: &PlatformState,
    tenant_id: Uuid,
    deployment_id: Uuid,
    action: &str,
    service: Option<&str>,
    destroy_data: bool,
) -> Result<Value> {
    let _ = deployment_row(state, tenant_id, deployment_id).await?;
    let app_manager = state
        .app_manager
        .as_ref()
        .ok_or_else(|| AmosError::Internal("no app provisioner available".into()))?;

    if action == "deprovision" {
        app_manager.teardown(deployment_id, destroy_data).await?;
        sqlx::query("UPDATE app_deployments SET status = 'deprovisioned' WHERE id = $1")
            .bind(deployment_id)
            .execute(&state.db)
            .await
            .map_err(AmosError::Database)?;
        sqlx::query("UPDATE app_services SET status = 'deprovisioned' WHERE deployment_id = $1")
            .bind(deployment_id)
            .execute(&state.db)
            .await
            .map_err(AmosError::Database)?;
        return Ok(json!({
            "deployment_id": deployment_id,
            "action": action,
            "status": "deprovisioned",
            "data_destroyed": destroy_data,
        }));
    }

    // start / stop / restart — gather the target containers.
    let targets = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT service_name, container_id FROM app_services
         WHERE deployment_id = $1 AND ($2::text IS NULL OR service_name = $2)",
    )
    .bind(deployment_id)
    .bind(service)
    .fetch_all(&state.db)
    .await
    .map_err(AmosError::Database)?;

    if targets.is_empty() {
        return Err(AmosError::NotFound {
            entity: "app service".into(),
            id: service.unwrap_or("*").to_string(),
        });
    }

    let (new_svc_status, new_dep_status) = match action {
        "start" => ("running", "running"),
        "stop" => ("stopped", "stopped"),
        "restart" => ("running", "running"),
        other => {
            return Err(AmosError::Validation(format!(
                "unknown action '{other}' (expected start|stop|restart|deprovision)"
            )))
        }
    };

    for (_svc, container_id) in &targets {
        let Some(cid) = container_id else { continue };
        match action {
            "start" => app_manager.start_container(cid).await?,
            "stop" => app_manager.stop_container(cid).await?,
            "restart" => app_manager.restart_container(cid).await?,
            _ => unreachable!(),
        }
    }

    // Update service rows; only flip the deployment status for whole-app actions.
    sqlx::query(
        "UPDATE app_services SET status = $1
         WHERE deployment_id = $2 AND ($3::text IS NULL OR service_name = $3)",
    )
    .bind(new_svc_status)
    .bind(deployment_id)
    .bind(service)
    .execute(&state.db)
    .await
    .map_err(AmosError::Database)?;

    if service.is_none() {
        sqlx::query("UPDATE app_deployments SET status = $1 WHERE id = $2")
            .bind(new_dep_status)
            .bind(deployment_id)
            .execute(&state.db)
            .await
            .map_err(AmosError::Database)?;
    }

    Ok(json!({
        "deployment_id": deployment_id,
        "action": action,
        "service": service,
        "status": new_svc_status,
    }))
}

// ── internal helpers ───────────────────────────────────────────────────────

/// Fetch a deployment scoped to the tenant, or NotFound.
async fn deployment_row(
    state: &PlatformState,
    tenant_id: Uuid,
    deployment_id: Uuid,
) -> Result<(String, String, Option<String>)> {
    sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT name, status, network_id FROM app_deployments WHERE id = $1 AND tenant_id = $2",
    )
    .bind(deployment_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(AmosError::Database)?
    .ok_or_else(|| AmosError::NotFound {
        entity: "app deployment".into(),
        id: deployment_id.to_string(),
    })
}

async fn service_rows(state: &PlatformState, deployment_id: Uuid) -> Result<Vec<Value>> {
    let rows = sqlx::query_as::<_, (String, String, bool, Value)>(
        "SELECT service_name, status, expose_public, host_ports
         FROM app_services WHERE deployment_id = $1 ORDER BY service_name",
    )
    .bind(deployment_id)
    .fetch_all(&state.db)
    .await
    .map_err(AmosError::Database)?;
    Ok(rows
        .into_iter()
        .map(|(service_name, status, expose_public, host_ports)| {
            json!({
                "service_name": service_name,
                "status": status,
                "expose_public": expose_public,
                "host_ports": host_ports,
            })
        })
        .collect())
}
