//! MCP (Model Context Protocol) control plane.
//!
//! Exposes the managed-platform's environment resources as MCP tools so that
//! Claude Code can *operate* an AMOS environment via tool calls instead of
//! blind discovery. This is the WS-1 "first slice" of AMOS-PIVOT-001:
//! **provision + deploy/release + logs + status/health + env config**.
//!
//! Transport: MCP Streamable HTTP. A single endpoint (`POST /mcp`) speaks
//! JSON-RPC 2.0. Each request gets a single `application/json` response (the
//! spec permits returning JSON directly instead of opening an SSE stream when
//! the server has nothing to push), which keeps the implementation small and
//! the request/response model obvious.
//!
//! Claude Code connects with:
//! ```text
//! claude mcp add --transport http amos https://<platform>/mcp \
//!     --header "Authorization: Bearer <api-key-or-jwt>"
//! ```
//!
//! Auth: every request must carry `Authorization: Bearer <token>`, validated
//! against the same JWT / API-key path the REST API uses. Tools are scoped to
//! the caller's tenant unless an explicit `tenant_id` is supplied and the
//! caller is an admin/owner.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use secrecy::ExposeSecret;
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

use crate::auth::{self, Claims};
use crate::provisioning::{HarnessConfig, InstanceSize};
use crate::state::PlatformState;

/// MCP protocol version this server implements. We echo the client's requested
/// version when it is one we understand, and fall back to this otherwise.
const SUPPORTED_PROTOCOL_VERSION: &str = "2025-06-18";

pub fn routes() -> Router<PlatformState> {
    Router::new().route("/mcp", post(handle_mcp))
}

// ── JSON-RPC transport ──────────────────────────────────────────────────

/// `ask_amos` — natural-language manager front door (embedded Bedrock loop).
mod ask_amos;

/// Finance engine — governed `finance_*` verb surface (reference business engine).
pub mod finance;

/// Standard JSON-RPC 2.0 error codes we use.
mod rpc_error {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
}

fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn rpc_err(id: Value, code: i64, message: impl Into<String>) -> Response {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    }))
    .into_response()
}

/// Main MCP endpoint. Validates auth, parses the JSON-RPC envelope, and
/// dispatches by method.
async fn handle_mcp(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    body: Json<Value>,
) -> Response {
    let req = body.0;

    // JSON-RPC id (absent for notifications). Default to null for error replies.
    let id = req.get("id").cloned().unwrap_or(Value::Null);

    let method = match req.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => return rpc_err(id, rpc_error::INVALID_REQUEST, "missing 'method'"),
    };

    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // `initialize` and notifications complete the handshake; everything else
    // operates on tenant resources and requires authentication. We still
    // require auth on `initialize` so an unauthenticated client is rejected at
    // connect time rather than on first tool call.
    let claims = match authenticate(&state, &headers).await {
        Some(c) => c,
        None => {
            // Notifications get no body; requests get a 401-shaped JSON-RPC error.
            if req.get("id").is_none() {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            return (
                StatusCode::UNAUTHORIZED,
                rpc_err(
                    id,
                    rpc_error::INVALID_REQUEST,
                    "missing or invalid bearer token",
                ),
            )
                .into_response();
        }
    };

    match method {
        "initialize" => {
            let client_version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(SUPPORTED_PROTOCOL_VERSION)
                .to_string();
            rpc_result(
                id,
                json!({
                    "protocolVersion": client_version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "amos-platform",
                        "version": crate::VERSION,
                    },
                    "instructions": "You are operating a business on AMOS, a governed control plane. \
                FIRST call `get_started` for the full capability manifest — verbs by category (each with its \
                required scope + whether you're allowed it), the golden paths, and the human-only steps to relay \
                to the owner — and `whoami` for your tenant + scopes. Ship new code with the `deploy` verb \
                (builds from amos.yaml, digest-pinned); never use app_redeploy to ship new code. Every write is \
                proof-carrying (see list_receipts). Tools are scoped to your tenant.",
                }),
            )
        }
        // Notifications carry no id and expect no response body.
        "notifications/initialized" | "notifications/cancelled" => {
            StatusCode::ACCEPTED.into_response()
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match dispatch_tool(&state, &claims, name, args).await {
                Ok(value) => rpc_result(id, tool_text_result(&value, false)),
                Err(ToolError::NotFound) => rpc_err(
                    id,
                    rpc_error::METHOD_NOT_FOUND,
                    format!("unknown tool '{name}'"),
                ),
                Err(ToolError::InvalidParams(msg)) => rpc_err(id, rpc_error::INVALID_PARAMS, msg),
                // Execution + authorization failures are reported as tool
                // results with isError=true (per MCP spec) rather than protocol
                // errors, so the model can read the failure and adapt.
                Err(ToolError::Execution(msg)) => {
                    rpc_result(id, tool_text_result(&json!({ "error": msg }), true))
                }
                Err(ToolError::Forbidden(msg)) => rpc_result(
                    id,
                    tool_text_result(&json!({ "error": msg, "code": "forbidden" }), true),
                ),
            }
        }
        _ => rpc_err(
            id,
            rpc_error::METHOD_NOT_FOUND,
            format!("unknown method '{method}'"),
        ),
    }
}

/// Build an MCP `tools/call` result envelope from a JSON payload.
fn tool_text_result(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

// ── Auth ──────────────────────────────────────────────────────────────────

/// Validate the `Authorization: Bearer <token>` header against JWT first, then
/// API keys (mirrors `middleware::require_api_key`). Returns the resolved
/// Claims on success. Shared with the REST app routes.
pub(crate) async fn authenticate(state: &PlatformState, headers: &HeaderMap) -> Option<Claims> {
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))?;

    // 1. JWT access token.
    let jwt_secret = state.config.auth.jwt_secret.expose_secret();
    if let Ok(claims) = auth::validate_access_token(token, jwt_secret) {
        return Some(claims);
    }

    // 2. API key (hashed lookup). Its `scopes` constrain the AI/machine
    //    principal (the AI axis) to a subset of the creating user's role.
    let key_hash = auth::hash_token(token);
    let row = sqlx::query_as::<_, (Uuid, Uuid, Vec<String>)>(
        "SELECT tenant_id, created_by, scopes FROM api_keys
         WHERE key_hash = $1 AND is_active = TRUE
         AND (expires_at IS NULL OR expires_at > NOW())",
    )
    .bind(&key_hash)
    .fetch_optional(&state.db)
    .await
    .ok()??;
    let (tenant_id, created_by, key_scopes) = row;

    let (role, tenant_slug) = sqlx::query_as::<_, (String, String)>(
        "SELECT u.role, t.slug FROM users u
         JOIN tenants t ON u.tenant_id = t.id
         WHERE u.id = $1",
    )
    .bind(created_by)
    .fetch_optional(&state.db)
    .await
    .ok()??;

    Some(Claims {
        sub: created_by.to_string(),
        tenant_id: tenant_id.to_string(),
        role,
        tenant_slug,
        iat: chrono::Utc::now().timestamp(),
        exp: chrono::Utc::now().timestamp() + 3600,
        scopes: Some(key_scopes),
    })
}

/// Resolve the tenant a tool call operates on. Non-admins are pinned to their
/// own tenant; admins/owners may target another tenant via `tenant_id`.
fn resolve_tenant(claims: &Claims, args: &Value) -> Result<Uuid, ToolError> {
    let own = Uuid::parse_str(&claims.tenant_id)
        .map_err(|_| ToolError::Execution("invalid tenant in credentials".into()))?;

    match args.get("tenant_id").and_then(|v| v.as_str()) {
        None => Ok(own),
        Some(requested) => {
            let requested = Uuid::parse_str(requested)
                .map_err(|_| ToolError::InvalidParams("tenant_id is not a valid UUID".into()))?;
            if requested == own || claims.role == "admin" || claims.role == "owner" {
                Ok(requested)
            } else {
                Err(ToolError::Execution(
                    "not authorized to operate on another tenant".into(),
                ))
            }
        }
    }
}

// ── Tool dispatch ───────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) enum ToolError {
    NotFound,
    InvalidParams(String),
    Execution(String),
    /// The principal lacks the scope required for the tool (RBAC).
    Forbidden(String),
}

impl<E: std::fmt::Display> From<E> for ToolError {
    fn from(e: E) -> Self {
        ToolError::Execution(e.to_string())
    }
}

async fn dispatch_tool(
    state: &PlatformState,
    claims: &Claims,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    // RBAC: enforce the tool's required scope before doing any work. JWT users
    // derive scopes from their role; API keys are narrowed to their scope list.
    if let Some(required) = crate::rbac::required_scope(name) {
        let effective = crate::rbac::effective_scopes(&claims.role, claims.scopes.as_deref());
        if !crate::rbac::allows(&effective, required) {
            return Err(ToolError::Forbidden(format!(
                "missing required scope '{required}' for tool '{name}'"
            )));
        }
    }

    match name {
        "list_harnesses" => tool_list_harnesses(state, claims, args).await,
        "provision_harness" => tool_provision_harness(state, claims, args).await,
        "harness_status" => tool_harness_status(state, claims, args).await,
        "harness_logs" => tool_harness_logs(state, claims, args).await,
        "harness_control" => tool_harness_control(state, claims, args).await,
        "list_releases" => tool_list_releases(state).await,
        "get_harness_config" => tool_get_harness_config(state, claims, args).await,
        "set_harness_config" => tool_set_harness_config(state, claims, args).await,
        // ── Multi-service app hosting ─────────────────────────────────────
        "deploy" => tool_deploy(state, claims, args).await,
        "deploy_status" => tool_deploy_status(state, claims, args).await,
        "deploy_app" => tool_deploy_app(state, claims, args).await,
        "list_apps" => tool_list_apps(state, claims, args).await,
        "app_status" => tool_app_status(state, claims, args).await,
        "app_logs" => tool_app_logs(state, claims, args).await,
        "app_control" => tool_app_control(state, claims, args).await,
        "app_redeploy" => tool_app_redeploy(state, claims, args).await,
        // ── Isolated environments (staging / preview) ─────────────────────
        "provision_env" => tool_provision_env(state, claims, args).await,
        "teardown_env" => tool_teardown_env(state, claims, args).await,
        // ── Build-as-a-service ────────────────────────────────────────────
        "build_image" => tool_build_image(state, claims, args).await,
        "build_status" => tool_build_status(state, claims, args).await,
        // ── Governed DB access ────────────────────────────────────────────
        "db_query" => tool_db_query(state, claims, args).await,
        "db_write" => tool_db_write(state, claims, args).await,
        "describe_table" => tool_describe_table(state, claims, args).await,

        // Finance engine (governed reference engine) — finance:* scopes + receipts on writes
        "finance_board" => finance::tool_finance_board(state, claims, args).await,
        "finance_history" => finance::tool_finance_history(state, claims, args).await,
        "finance_truth" => finance::tool_finance_truth(state, claims, args).await,
        "qbo_accounts" => finance::tool_qbo_accounts(state, claims, args).await,
        "revenue_summary" => finance::tool_revenue_summary(state, claims, args).await,
        "org_subscriptions" => finance::tool_org_subscriptions(state, claims, args).await,
        "churn_snapshot" => finance::tool_churn_snapshot(state, claims, args).await,
        "create_finance_line" => finance::tool_create_finance_line(state, claims, args).await,
        "update_finance_line" => finance::tool_update_finance_line(state, claims, args).await,
        "set_finance_actual" => finance::tool_set_finance_actual(state, claims, args).await,
        "set_finance_budget" => finance::tool_set_finance_budget(state, claims, args).await,
        "set_finance_mapping" => finance::tool_set_finance_mapping(state, claims, args).await,
        "set_billing_key" => finance::tool_set_billing_key(state, claims, args).await,
        // ── Tenant object storage (S3) ────────────────────────────────────
        "s3_list" => tool_s3_list(state, claims, args).await,
        "s3_get" => tool_s3_get(state, claims, args).await,
        "s3_put" => tool_s3_put(state, claims, args).await,
        "s3_delete" => tool_s3_delete(state, claims, args).await,
        // ── Introspection (capability/identity discovery) ─────────────────
        "whoami" => tool_whoami(state, claims, args).await,
        "get_started" => tool_get_started(state, claims, args).await,
        "onboard_app" => tool_onboard_app(state, claims, args).await,
        // ── Proof-carrying operations ─────────────────────────────────────
        "list_receipts" => tool_list_receipts(state, claims, args).await,
        // ── Natural-language manager front door ───────────────────────────
        "ask_amos" => ask_amos::run(state, claims, &arg_str(&args, "prompt")?).await,
        _ => Err(ToolError::NotFound),
    }
}

/// Map an orchestrator `AmosError` into a `ToolError` (NotFound vs Execution).
fn map_amos_err(e: amos_core::AmosError) -> ToolError {
    use amos_core::AmosError;
    match e {
        AmosError::NotFound { .. } => ToolError::Execution(e.to_string()),
        AmosError::Validation(msg) => ToolError::InvalidParams(msg),
        other => ToolError::Execution(other.to_string()),
    }
}

// ── App tool implementations (delegate to the shared orchestrator) ─────────

async fn tool_deploy_app(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    // The authenticated subject is the accountable actor on the proof receipt.
    let actor = claims.sub.as_str();

    // Provider selects the renderer: 'docker' (local, default) or 'aws'/'ecs'
    // (Fargate). The AWS path needs a resolved AppSpec (from spec or compose).
    let provider = args
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("docker");
    if provider == "aws" || provider == "ecs" {
        let spec = resolve_spec_arg(&args)?;
        let target = parse_aws_target(args.get("aws"));
        let callback_url = args
            .get("callback_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return crate::services::app_orchestrator::deploy_app_aws(
            state,
            tenant_id,
            actor,
            spec,
            target,
            callback_url,
        )
        .await
        .map_err(map_amos_err);
    }

    if let Some(compose_yaml) = args.get("compose_yaml").and_then(|v| v.as_str()) {
        // Relative bind mounts resolve against compose_dir; default to "." (the
        // platform's CWD) when the caller doesn't provide one.
        let dir = args
            .get("compose_dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        crate::services::app_orchestrator::deploy_from_compose(
            state,
            tenant_id,
            actor,
            compose_yaml,
            std::path::Path::new(dir),
        )
        .await
        .map_err(map_amos_err)
    } else if let Some(spec_val) = args.get("spec") {
        let spec: crate::provisioning::app_spec::AppSpec = serde_json::from_value(spec_val.clone())
            .map_err(|e| ToolError::InvalidParams(format!("invalid app spec: {e}")))?;
        crate::services::app_orchestrator::deploy_app(state, tenant_id, actor, spec)
            .await
            .map_err(map_amos_err)
    } else {
        Err(ToolError::InvalidParams(
            "provide either 'compose_yaml' or 'spec'".into(),
        ))
    }
}

/// Build an `AppSpec` from either a `spec` object or a `compose_yaml` string.
fn resolve_spec_arg(args: &Value) -> Result<crate::provisioning::app_spec::AppSpec, ToolError> {
    if let Some(spec_val) = args.get("spec") {
        serde_json::from_value(spec_val.clone())
            .map_err(|e| ToolError::InvalidParams(format!("invalid app spec: {e}")))
    } else if let Some(yaml) = args.get("compose_yaml").and_then(|v| v.as_str()) {
        let dir = args
            .get("compose_dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        crate::provisioning::compose_intake::app_spec_from_compose_str(
            yaml,
            std::path::Path::new(dir),
        )
        .map_err(ToolError::InvalidParams)
    } else {
        Err(ToolError::InvalidParams(
            "provide either 'spec' or 'compose_yaml'".into(),
        ))
    }
}

/// Parse the optional `aws` target object into an `AwsDeployTarget`.
fn parse_aws_target(aws: Option<&Value>) -> crate::provisioning::aws_app::AwsDeployTarget {
    use crate::provisioning::aws_app::AwsDeployTarget;
    let Some(a) = aws else {
        return AwsDeployTarget::default();
    };
    let s = |k: &str| a.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
    AwsDeployTarget {
        secret_arn: s("secret_arn"),
        target_group_arn: s("target_group_arn"),
        public_service: s("public_service"),
        public_url: s("public_url"),
        cpu: s("cpu"),
        memory: s("memory"),
        ephemeral_storage_gib: a
            .get("ephemeral_storage_gib")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        task_role_arn: s("task_role_arn"),
    }
}

async fn tool_list_apps(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    crate::services::app_orchestrator::list_apps(state, tenant_id)
        .await
        .map_err(map_amos_err)
}

async fn tool_list_receipts(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let operation = args.get("operation").and_then(|v| v.as_str());
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
    let receipts = crate::proof::list_for_tenant(&state.db, tenant_id, operation, limit)
        .await
        .map_err(map_amos_err)?;
    Ok(serde_json::json!({
        "tenant_id": tenant_id,
        "count": receipts.len(),
        "receipts": receipts,
    }))
}

// ── Build-as-a-service ─────────────────────────────────────────────────────

async fn tool_build_image(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::provisioning::image_builder::BuildRequest;

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let builder = state
        .image_builder
        .clone()
        .ok_or_else(|| ToolError::Execution("image builder not configured".into()))?;

    // Source is either a local context_dir (zipped → S3) or a git repo+ref
    // (CodeBuild clones it — the path CI uses). Exactly one is required.
    let context_dir = args
        .get("context_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let git_repo = args
        .get("git_repo")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let git_ref = args
        .get("git_ref")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if context_dir.is_none() && git_repo.is_none() {
        return Err(ToolError::Execution(
            "provide either context_dir (local build) or git_repo (git-source build)".into(),
        ));
    }
    let image_name = arg_str(&args, "image_name")?;
    let tag = args
        .get("tag")
        .and_then(|v| v.as_str())
        .unwrap_or("latest")
        .to_string();
    let dockerfile = args
        .get("dockerfile")
        .and_then(|v| v.as_str())
        .unwrap_or("Dockerfile")
        .to_string();

    // Optional --build-arg map (e.g. {"BASE_IMAGE": "…/cuspr-base:latest"}).
    let build_args: std::collections::BTreeMap<String, String> = args
        .get("build_args")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let req = BuildRequest {
        context_dir: context_dir.as_ref().map(std::path::PathBuf::from),
        git_repo: git_repo.clone(),
        git_ref: git_ref.clone(),
        dockerfile: dockerfile.clone(),
        image_name: image_name.clone(),
        tag: tag.clone(),
        build_args,
    };

    // Human-readable source descriptor for the proof receipt.
    let source_desc = match (&git_repo, &context_dir) {
        (Some(repo), _) => match &git_ref {
            Some(r) => format!("git:{repo}@{r}"),
            None => format!("git:{repo}"),
        },
        (None, Some(dir)) => dir.clone(),
        (None, None) => String::new(),
    };

    let build_id = builder.start_build(&req).await.map_err(map_amos_err)?;

    // Poll the build to completion in the background and emit a proof receipt —
    // builds take minutes; blocking the MCP call would time out. Callers watch
    // progress with build_status meanwhile.
    let callback_url = args
        .get("callback_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let bg = state.clone();
    let bg_build_id = build_id.clone();
    let bg_image = image_name.clone();
    let bg_tag = tag.clone();
    let bg_df = dockerfile.clone();
    let bg_ctx = source_desc.clone();
    tokio::spawn(async move {
        run_build_to_receipt(
            &bg,
            builder,
            &bg_build_id,
            &bg_image,
            &bg_tag,
            &bg_df,
            &bg_ctx,
            tenant_id,
            &actor,
            callback_url,
        )
        .await;
    });

    Ok(serde_json::json!({
        "build_id": build_id,
        "image_name": image_name,
        "tag": tag,
        "status": "building",
        "message": "Build started in the cloud (CodeBuild). Poll build_status; a proof receipt is emitted on completion.",
    }))
}

#[allow(clippy::too_many_arguments)]
async fn run_build_to_receipt(
    state: &PlatformState,
    builder: std::sync::Arc<crate::provisioning::image_builder::ImageBuilder>,
    build_id: &str,
    image_name: &str,
    tag: &str,
    dockerfile: &str,
    context_dir: &str,
    tenant_id: uuid::Uuid,
    actor: &str,
    callback_url: Option<String>,
) {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};

    // Poll up to ~60 min (CodeBuild's build_timeout), 20s cadence.
    let mut state_final = None;
    for _ in 0..180 {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        match builder.build_state(build_id, image_name, tag).await {
            Ok(s) if s.done => {
                state_final = Some(s);
                break;
            }
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(%build_id, "build_state poll error: {e}");
                continue;
            }
        }
    }

    let intent = Intent {
        summary: format!("Build image '{image_name}:{tag}' from {dockerfile} in the cloud"),
        self_modifying: false,
        scope_classification: "build".to_string(),
    };
    let receipt = OperationReceipt::new("build_image", tenant_id, actor, intent)
        // Built in an isolated CodeBuild env, not on a developer/agent machine.
        .guardrail("cloud_build_isolation")
        .guardrail("no_local_docker")
        .inputs(serde_json::json!({
            "image_name": image_name,
            "tag": tag,
            "dockerfile": dockerfile,
            "context_dir": context_dir,
            "build_id": build_id,
        }));

    let receipt = match state_final {
        Some(s) if s.succeeded => {
            let digest = s.image_digest.clone().unwrap_or_default();
            let image_ref = builder.image_ref(image_name, tag, s.image_digest.as_deref());
            receipt
                .check(
                    "image_built",
                    CheckStatus::Passed,
                    format!("CodeBuild SUCCEEDED ({})", s.current_phase),
                )
                .check(
                    "image_pushed",
                    if digest.is_empty() {
                        CheckStatus::Skipped
                    } else {
                        CheckStatus::Passed
                    },
                    if digest.is_empty() {
                        "pushed but digest unavailable".to_string()
                    } else {
                        digest.clone()
                    },
                )
                .outputs(serde_json::json!({
                    "image_digest": s.image_digest,
                    "image_ref": image_ref,
                    "logs_url": s.logs_url,
                }))
                .summary(format!("Built and pushed {image_name}:{tag}"))
        }
        Some(s) => receipt
            .check(
                "image_built",
                CheckStatus::Failed,
                format!("CodeBuild {} at phase {}", s.status, s.current_phase),
            )
            .outputs(serde_json::json!({ "logs_url": s.logs_url }))
            .summary(format!("Build of {image_name}:{tag} failed ({})", s.status)),
        None => receipt
            .check(
                "image_built",
                CheckStatus::Failed,
                "build did not finish within poll window",
            )
            .summary(format!("Build of {image_name}:{tag} timed out")),
    };

    if let Err(e) = proof::emit(&state.db, &receipt).await {
        tracing::warn!(%build_id, "failed to emit build receipt: {e}");
    }
    if let Some(url) = &callback_url {
        crate::services::app_orchestrator::ping_back(
            url,
            serde_json::json!({
                "event": if receipt.verified() { "build_image.completed" } else { "build_image.failed" },
                "build_id": build_id, "image_name": image_name, "tag": tag,
                "verified": receipt.verified(), "summary": receipt.result_summary,
            }),
        )
        .await;
    }
}

async fn tool_build_status(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let _tenant_id = resolve_tenant(claims, &args)?;
    let builder = state
        .image_builder
        .as_ref()
        .ok_or_else(|| ToolError::Execution("image builder not configured".into()))?;
    let build_id = arg_str(&args, "build_id")?;
    let image_name = arg_str(&args, "image_name")?;
    let tag = args.get("tag").and_then(|v| v.as_str()).unwrap_or("latest");

    let s = builder
        .build_state(&build_id, &image_name, tag)
        .await
        .map_err(map_amos_err)?;

    Ok(serde_json::json!({
        "build_id": build_id,
        "status": s.status,
        "current_phase": s.current_phase,
        "succeeded": s.succeeded,
        "done": s.done,
        "image_digest": s.image_digest,
        "image_ref": s.image_digest.as_deref().map(|d| builder.image_ref(&image_name, tag, Some(d))),
        "logs_url": s.logs_url,
    }))
}

async fn tool_app_status(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    crate::services::app_orchestrator::app_status(state, tenant_id, deployment_id)
        .await
        .map_err(map_amos_err)
}

async fn tool_app_logs(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let service = arg_str(&args, "service")?;
    let tail = args.get("tail").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
    crate::services::app_orchestrator::app_logs(state, tenant_id, deployment_id, &service, tail)
        .await
        .map_err(map_amos_err)
}

async fn tool_app_control(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let action = arg_str(&args, "action")?;
    let service = args.get("service").and_then(|v| v.as_str());
    let destroy_data = args
        .get("destroy_data")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    crate::services::app_orchestrator::app_control(
        state,
        tenant_id,
        deployment_id,
        &action,
        service,
        destroy_data,
    )
    .await
    .map_err(map_amos_err)
}

/// Look up a harness row scoped to the tenant, returning (container_id, status).
async fn resolve_harness(
    state: &PlatformState,
    tenant_id: Uuid,
    harness_id: Uuid,
) -> Result<(Option<String>, String), ToolError> {
    sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT container_id, status FROM harness_instances WHERE id = $1 AND tenant_id = $2",
    )
    .bind(harness_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("database error: {e}")))?
    .ok_or_else(|| ToolError::Execution("harness not found for this tenant".into()))
}

/// The standard one-call deploy: build an app from its `amos.yaml` manifest and
/// roll it. Scope `app:deploy`. The app author writes the manifest + a one-line
/// workflow; this verb does everything — clone (via git-source build), build the
/// service graph in dependency order (injecting BASE_IMAGE from base layers),
/// then force-new-deployment the app's services. No tags/task-defs/ARNs leak to
/// the caller. Resolves the app by `manifest.name` + tenant (no UUID needed).
/// Returns immediately; the build+roll runs in the background and emits a receipt.
async fn tool_deploy(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::provisioning::manifest::AmosManifest;

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let git_repo = arg_str(&args, "git_repo")?;
    let git_ref = arg_str(&args, "git_ref")?;
    let manifest = AmosManifest::parse(&arg_str(&args, "manifest")?).map_err(map_amos_err)?;
    // Validate the build graph up front (cycles/unknown base → clear error now).
    manifest.build_order().map_err(map_amos_err)?;

    // `env` targets an isolated environment (provisioned by provision_env): the
    // deployment is resolved as `{manifest.name}-{env}` instead of the base app.
    let app_name = match args.get("env").and_then(|v| v.as_str()) {
        Some(env) if !env.is_empty() => format!("{}-{}", manifest.name, env),
        _ => manifest.name.clone(),
    };

    let builder = state
        .image_builder
        .clone()
        .ok_or_else(|| ToolError::Execution("image builder not configured".into()))?;

    // Resolve the app by name + tenant (must be onboarded first).
    let row: Option<(Uuid, Option<Value>)> = sqlx::query_as(
        "SELECT id, aws_meta FROM app_deployments \
         WHERE tenant_id = $1 AND name = $2 AND status != 'deprovisioned'",
    )
    .bind(tenant_id)
    .bind(&app_name)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("lookup app: {e}")))?;
    let (deployment_id, aws_meta) = row.ok_or_else(|| {
        ToolError::Execution(format!(
            "no app named '{app_name}' for this tenant — onboard it (or provision_env) before deploying"
        ))
    })?;
    let ecs_services: Vec<String> = aws_meta
        .as_ref()
        .and_then(|m| m.get("ecs_services"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![app_name.clone()]);

    let n_build = manifest.services.len();
    let deploy_id = Uuid::new_v4();

    // Serialize per app: if a deploy is already in flight for this deployment,
    // do NOT spawn a colliding job (the collision previously no-op'd into a
    // confusing built:{} rolled:[] verified=false). Return the in-flight
    // deploy_id so the caller polls that one. The check-and-insert is atomic
    // under the mutex; run_deploy_to_receipt clears the entry on every exit
    // path via InFlightGuard.
    {
        let mut inflight = state
            .in_flight_deploys
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let existing = inflight.get(&deployment_id).copied();
        if !may_start_deploy(existing.is_some()) {
            return Ok(serde_json::json!({
                "app": deployment_id,
                "status": "already_deploying",
                "deploy_id": existing,
                "note": "a deploy is already in flight for this app; poll that deploy_id. This deploy was not started (superseded to avoid a colliding no-op).",
            }));
        }
        inflight.insert(deployment_id, deploy_id);
    }

    let bg = state.clone();
    tokio::spawn(async move {
        run_deploy_to_receipt(
            &bg,
            builder,
            deploy_id,
            tenant_id,
            actor,
            deployment_id,
            manifest,
            ecs_services,
            git_repo,
            git_ref,
        )
        .await;
    });

    Ok(serde_json::json!({
        "deploy_id": deploy_id,
        "app": deployment_id,
        "status": "deploying",
        "services_to_build": n_build,
        "note": "building + rolling in the background; poll deploy_status with this deploy_id until terminal",
    }))
}

/// Background: build every service in the manifest (git-source, dep order) then
/// force-new-deployment the app's ECS services, emitting one deploy receipt.
#[allow(clippy::too_many_arguments)]
async fn run_deploy_to_receipt(
    state: &PlatformState,
    builder: std::sync::Arc<crate::provisioning::image_builder::ImageBuilder>,
    deploy_id: Uuid,
    tenant_id: Uuid,
    actor: String,
    deployment_id: Uuid,
    manifest: crate::provisioning::manifest::AmosManifest,
    ecs_services: Vec<String>,
    git_repo: String,
    git_ref: String,
) {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
    use crate::provisioning::image_builder::BuildRequest;
    use std::collections::{BTreeMap, HashMap};
    use std::time::{Duration, Instant};

    // Clear the per-app in-flight marker on EVERY exit (success, failure, early
    // return, panic) so the next deploy for this app isn't wrongly rejected.
    struct InFlightGuard {
        map: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Uuid, Uuid>>>,
        deployment_id: Uuid,
    }
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            if let Ok(mut m) = self.map.lock() {
                m.remove(&self.deployment_id);
            }
        }
    }
    let _inflight_guard = InFlightGuard {
        map: state.in_flight_deploys.clone(),
        deployment_id,
    };

    let registry = std::env::var("AMOS_ECR_REGISTRY").unwrap_or_default();
    let order = match manifest.build_order() {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("deploy '{}': bad manifest: {e}", manifest.name);
            return;
        }
    };

    let mut built: BTreeMap<String, String> = BTreeMap::new();
    // repo image-name -> "registry/name@digest" for the freshly-built images, so
    // we deploy by DIGEST (no moving-tag drift / silent no-op).
    let mut pins: HashMap<String, String> = HashMap::new();
    let mut results: Vec<(String, bool, String)> = Vec::new();
    let mut all_ok = true;

    // Per-phase timing for the receipt (build-vs-roll + per-image), so a slow
    // deploy is diagnosable instead of inferred.
    let t_total = Instant::now();
    let mut build_secs: BTreeMap<String, u64> = BTreeMap::new();
    let mut release_secs: u64 = 0;
    let mut roll_secs: u64 = 0;

    // 1. Build every service in dependency order (base layers first, injecting
    //    BASE_IMAGE into dependents).
    for svc in order {
        let svc_t = Instant::now();
        let image_name = svc.image_name(&manifest.name);
        let tag = svc.tag();
        let mut build_args: BTreeMap<String, String> = BTreeMap::new();
        if let Some(base) = &svc.base {
            if let Some(r) = built.get(base) {
                build_args.insert("BASE_IMAGE".to_string(), r.clone());
            }
        }
        let req = BuildRequest {
            context_dir: None,
            git_repo: Some(git_repo.clone()),
            git_ref: Some(git_ref.clone()),
            dockerfile: svc.dockerfile.clone(),
            image_name: image_name.clone(),
            tag: tag.clone(),
            build_args,
        };
        let build_id = match builder.start_build(&req).await {
            Ok(b) => b,
            Err(e) => {
                all_ok = false;
                results.push((svc.name.clone(), false, format!("build start failed: {e}")));
                break;
            }
        };
        // Poll to terminal (~20 min cap), capturing the pushed digest.
        let mut succeeded = false;
        let mut digest: Option<String> = None;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(20)).await;
            if let Ok(s) = builder.build_state(&build_id, &image_name, &tag).await {
                if s.done {
                    succeeded = s.succeeded;
                    digest = s.image_digest.clone();
                    break;
                }
            }
        }
        if !succeeded {
            all_ok = false;
            results.push((
                svc.name.clone(),
                false,
                format!("{image_name}:{tag} build failed/timeout"),
            ));
            break;
        }
        built.insert(svc.name.clone(), format!("{registry}/{image_name}:{tag}"));
        // Pin the deploy to this exact digest (keyed by repo image-name).
        if let Some(d) = &digest {
            pins.insert(image_name.clone(), format!("{registry}/{image_name}@{d}"));
        }
        build_secs.insert(svc.name.clone(), svc_t.elapsed().as_secs());
        results.push((
            svc.name.clone(),
            true,
            match &digest {
                Some(d) => format!("built {image_name}:{tag} @ {d}"),
                None => format!("built {image_name}:{tag} (no digest — will roll on tag)"),
            },
        ));
    }

    // 2. Roll the app's ECS services by DIGEST-PINNING — only if every build
    //    succeeded. For each service we clone its live task def, repoint any
    //    container whose ECR repo we just built to that exact `name@digest`,
    //    register a new revision, and point the service at it. A new task def
    //    triggers the rollout (no force_new_deployment needed), and because the
    //    image is digest-pinned a moving-tag drift can never ship a stale image.
    let mut rolled: Vec<String> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();
    let mut deployed: Vec<serde_json::Value> = Vec::new();
    if all_ok {
        let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let ecs = aws_sdk_ecs::Client::new(&cfg);
        let cluster = std::env::var("ECS_CLUSTER")
            .unwrap_or_else(|_| "swarm-infrastructure-cluster".to_string());

        // ── RELEASE PHASE ── run the manifest's release command ONCE on the
        // freshly-built (digest-pinned) image, BEFORE rolling any service. A
        // non-zero exit ABORTS the deploy: no service is rolled, so the running
        // version keeps serving. No `release` in the manifest → skipped entirely
        // (existing manifests deploy exactly as before).
        if let Some(release_cmd) = manifest.release.as_deref() {
            let rel_t = Instant::now();
            match run_release_phase(&ecs, &cfg, &cluster, &ecs_services, &pins, release_cmd).await {
                Ok(detail) => results.push(("release".to_string(), true, detail)),
                Err(detail) => {
                    all_ok = false;
                    results.push(("release".to_string(), false, detail));
                }
            }
            release_secs = rel_t.elapsed().as_secs();
        }

        // Only roll the services if the release phase (if any) passed.
        if all_ok {
            let roll_t = Instant::now();
            for svc in &ecs_services {
                // Current task def of the service.
                let td_arn = match ecs
                    .describe_services()
                    .cluster(&cluster)
                    .services(svc)
                    .send()
                    .await
                {
                    Ok(r) => r
                        .services()
                        .first()
                        .and_then(|s| s.task_definition().map(str::to_string)),
                    Err(e) => {
                        all_ok = false;
                        results.push((
                            svc.clone(),
                            false,
                            format!("describe_services failed: {e}"),
                        ));
                        continue;
                    }
                };
                let Some(td_arn) = td_arn else {
                    all_ok = false;
                    results.push((svc.clone(), false, "service has no task definition".into()));
                    continue;
                };
                let base_td = match ecs
                    .describe_task_definition()
                    .task_definition(&td_arn)
                    .send()
                    .await
                {
                    Ok(r) => r.task_definition().cloned(),
                    Err(e) => {
                        all_ok = false;
                        results.push((
                            svc.clone(),
                            false,
                            format!("describe_task_definition failed: {e}"),
                        ));
                        continue;
                    }
                };
                let Some(base_td) = base_td else {
                    all_ok = false;
                    results.push((svc.clone(), false, "task definition not found".into()));
                    continue;
                };
                // Digest-pin matching containers.
                let containers =
                    match crate::provisioning::env_provision::clone_containers_with_image_pins(
                        base_td.container_definitions(),
                        &pins,
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            all_ok = false;
                            results.push((svc.clone(), false, format!("pin images failed: {e}")));
                            continue;
                        }
                    };
                // No-change short-circuit: if pinning produced an identical set
                // of container images (cache-hit → same digest, or the service
                // is already on the target digest), skip the pointless rollout
                // and record it as unchanged. This is a clean "already
                // up-to-date" — NOT a failure (the old behavior rolled an
                // identical image and a colliding/no-op deploy looked failed).
                let current_images: Vec<Option<String>> = base_td
                    .container_definitions()
                    .iter()
                    .map(|c| c.image().map(str::to_string))
                    .collect();
                let new_images: Vec<Option<String>> = containers
                    .iter()
                    .map(|c| c.image().map(str::to_string))
                    .collect();
                if current_images == new_images {
                    unchanged.push(svc.clone());
                    results.push((
                        svc.clone(),
                        true,
                        format!("{svc}: already on target image — no roll needed"),
                    ));
                    continue;
                }
                let matched: Vec<String> = base_td
                    .container_definitions()
                    .iter()
                    .filter_map(|c| c.image())
                    .filter_map(crate::provisioning::env_provision::repo_of)
                    .filter_map(|repo| pins.get(repo).cloned())
                    .collect();
                if matched.is_empty() {
                    tracing::warn!(service = %svc, "deploy: no container image matched a freshly-built repo — rolling unchanged image (likely a manifest/ecs_services misconfig)");
                }
                // Register a new task def revision with the pinned containers.
                let family = base_td.family().unwrap_or(svc).to_string();
                let mut reg = ecs
                    .register_task_definition()
                    .family(&family)
                    .set_container_definitions(Some(containers))
                    .set_cpu(base_td.cpu().map(str::to_string))
                    .set_memory(base_td.memory().map(str::to_string))
                    .set_network_mode(base_td.network_mode().cloned())
                    .set_requires_compatibilities(Some(base_td.requires_compatibilities().to_vec()))
                    .set_execution_role_arn(base_td.execution_role_arn().map(str::to_string))
                    .set_task_role_arn(base_td.task_role_arn().map(str::to_string))
                    .set_runtime_platform(base_td.runtime_platform().cloned());
                if !base_td.volumes().is_empty() {
                    reg = reg.set_volumes(Some(base_td.volumes().to_vec()));
                }
                let new_td_arn = match reg.send().await {
                    Ok(r) => r
                        .task_definition()
                        .and_then(|t| t.task_definition_arn().map(str::to_string)),
                    Err(e) => {
                        all_ok = false;
                        results.push((
                            svc.clone(),
                            false,
                            format!("register_task_definition failed: {e}"),
                        ));
                        continue;
                    }
                };
                let Some(new_td_arn) = new_td_arn else {
                    all_ok = false;
                    results.push((svc.clone(), false, "no task def ARN returned".into()));
                    continue;
                };
                // Point the service at the new revision (this triggers the rollout).
                match ecs
                    .update_service()
                    .cluster(&cluster)
                    .service(svc)
                    .task_definition(&new_td_arn)
                    .send()
                    .await
                {
                    Ok(_) => {
                        rolled.push(svc.clone());
                        deployed.push(serde_json::json!({
                            "service": svc,
                            "task_def": new_td_arn,
                            "images": matched,
                        }));
                    }
                    Err(e) => {
                        all_ok = false;
                        results.push((svc.clone(), false, format!("update_service failed: {e}")));
                    }
                }
            }
            roll_secs = roll_t.elapsed().as_secs();
        } // end: roll services (gated on release passing)
    }

    let total_secs = t_total.elapsed().as_secs();

    // 3. One deploy receipt.
    let intent = Intent {
        summary: format!("deploy '{}' from {git_repo} @ {git_ref}", manifest.name),
        self_modifying: false,
        scope_classification: "deploy".to_string(),
    };
    let mut receipt = OperationReceipt::new("deploy", tenant_id, actor, intent)
        .guardrail("manifest_driven")
        .guardrail("build_graph_ordered")
        .guardrail("rolling_health_checked")
        .guardrail("digest_pinned_deploy")
        .guardrail("release_gated")
        .inputs(serde_json::json!({
            "deploy_id": deploy_id,
            "deployment_id": deployment_id,
            "git_repo": git_repo,
            "git_ref": git_ref,
            "app": manifest.name,
        }));
    for (name, ok, msg) in &results {
        receipt = receipt.check(
            name.clone(),
            if *ok {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            },
            msg.clone(),
        );
    }
    let build_total: u64 = build_secs.values().sum();
    let outcome = deploy_outcome(all_ok, rolled.len(), unchanged.len());
    let no_changes = matches!(outcome, DeployOutcome::NoChanges);
    receipt = receipt
        .outputs(serde_json::json!({
            "built": built,
            "pinned": pins,
            "rolled": rolled,
            // Services already on the target image (cache-hit / same digest):
            // a clean no-op, not a failure.
            "unchanged": unchanged,
            "no_changes": no_changes,
            "deployed": deployed,
            // Per-phase wall-clock (seconds) so a slow deploy is diagnosable:
            // which image's build dominated, and build-vs-release-vs-roll.
            "timings": {
                "build_per_image_s": build_secs,
                "build_total_s": build_total,
                "release_s": release_secs,
                "roll_s": roll_secs,
                "total_s": total_secs,
            },
        }))
        .summary(match outcome {
            DeployOutcome::Rolled => format!(
                "deployed {} ({} svc rolled, digest-pinned) — build {build_total}s · release {release_secs}s · roll {roll_secs}s · total {total_secs}s",
                manifest.name,
                rolled.len()
            ),
            DeployOutcome::NoChanges => format!(
                "{} already up-to-date — {} service(s) already on the target image, nothing to roll (build {build_total}s · total {total_secs}s)",
                manifest.name,
                unchanged.len()
            ),
            DeployOutcome::Failed => format!(
                "deploy of {} FAILED — services not rolled (total {total_secs}s)",
                manifest.name
            ),
        });
    let _ = proof::emit(&state.db, &receipt).await;
}

/// How a manifest `deploy` ended — drives the receipt summary + `verified`.
/// `NoChanges` is a clean SUCCESS (every built digest already matches what's
/// running, so nothing needed rolling — e.g. a cache-hit re-deploy), distinct
/// from `Failed` (a real build/release/roll error). Previously a no-op deploy
/// looked like `Failed` (built:{} rolled:[] verified=false), confusing CI.
#[derive(Debug, PartialEq, Eq)]
enum DeployOutcome {
    Rolled,
    NoChanges,
    Failed,
}

/// Classify a deploy from whether it stayed healthy + how many services rolled
/// vs were already on the target image. Pure — unit-tested.
fn deploy_outcome(all_ok: bool, rolled: usize, unchanged: usize) -> DeployOutcome {
    if !all_ok {
        DeployOutcome::Failed
    } else if rolled == 0 && unchanged > 0 {
        DeployOutcome::NoChanges
    } else {
        DeployOutcome::Rolled
    }
}

/// Whether a new deploy may start, given whether one is already in flight for
/// the same app. `false` ⇒ the caller returns `already_deploying` instead of
/// spawning a colliding job. Pure — unit-tested.
fn may_start_deploy(already_in_flight: bool) -> bool {
    !already_in_flight
}

/// Outcome of the release gate: roll the new version, or abort and keep serving
/// the old one.
#[derive(Debug, PartialEq)]
enum ReleaseDecision {
    Proceed,
    Abort(String),
}

/// Decide whether the rollout may proceed from the release task's exit code.
/// Exit 0 → proceed; any non-zero, or no exit code at all (the task failed to
/// start or was killed) → abort. Pure — unit-tested.
fn release_gate(exit_code: Option<i32>) -> ReleaseDecision {
    match exit_code {
        Some(0) => ReleaseDecision::Proceed,
        Some(code) => ReleaseDecision::Abort(format!("release command exited non-zero ({code})")),
        None => ReleaseDecision::Abort(
            "release task produced no exit code (failed to start or was killed)".to_string(),
        ),
    }
}

/// Wrap the release command for a one-shot run. Always via `sh -lc` so any
/// toolchain's command string works verbatim (rake / alembic / sqlx / npm …)
/// without the platform parsing or knowing the toolchain.
fn release_argv(release_cmd: &str) -> Vec<String> {
    vec!["sh".to_string(), "-lc".to_string(), release_cmd.to_string()]
}

/// Run the manifest's `release` command as a ONE-SHOT ECS task on the freshly
/// built (digest-pinned) image, using the primary service's own task role /
/// secrets / network, and wait for it to finish. Returns `Ok(detail)` on exit 0
/// (the rollout may proceed) or `Err(detail)` on any failure (abort the deploy —
/// the running version keeps serving). It never rolls a service itself; it only
/// decides whether the caller should.
async fn run_release_phase(
    ecs: &aws_sdk_ecs::Client,
    aws_cfg: &aws_config::SdkConfig,
    cluster: &str,
    ecs_services: &[String],
    pins: &std::collections::HashMap<String, String>,
    release_cmd: &str,
) -> std::result::Result<String, String> {
    use aws_sdk_ecs::types::{ContainerOverride, LaunchType, TaskOverride};
    use std::time::Duration;

    // The release runs on the app's primary (first) service: its task role,
    // secrets, and network are what a migration needs.
    let primary = ecs_services
        .first()
        .ok_or_else(|| "no ECS service available to run the release on".to_string())?;

    // Describe the primary service → its live task def + network configuration.
    let svc = ecs
        .describe_services()
        .cluster(cluster)
        .services(primary)
        .send()
        .await
        .map_err(|e| format!("release: describe_services({primary}) failed: {e}"))?
        .services()
        .first()
        .cloned()
        .ok_or_else(|| format!("release: service '{primary}' not found"))?;
    let net = svc.network_configuration().cloned();
    let td_arn = svc
        .task_definition()
        .ok_or_else(|| format!("release: service '{primary}' has no task definition"))?
        .to_string();

    // Clone its task def with the freshly-built images digest-pinned, so the
    // release runs on EXACTLY the image about to be deployed.
    let base_td = ecs
        .describe_task_definition()
        .task_definition(&td_arn)
        .send()
        .await
        .map_err(|e| format!("release: describe_task_definition failed: {e}"))?
        .task_definition()
        .cloned()
        .ok_or_else(|| "release: task definition not found".to_string())?;
    let containers = crate::provisioning::env_provision::clone_containers_with_image_pins(
        base_td.container_definitions(),
        pins,
    )
    .map_err(|e| format!("release: pin images failed: {e}"))?;

    // The container the release runs in: prefer the one whose repo we just built
    // (the app image carrying the migration), else the first essential, else first.
    let release_container = base_td
        .container_definitions()
        .iter()
        .find(|c| {
            c.image()
                .and_then(crate::provisioning::env_provision::repo_of)
                .map(|repo| pins.contains_key(repo))
                .unwrap_or(false)
        })
        .or_else(|| {
            base_td
                .container_definitions()
                .iter()
                .find(|c| c.essential() == Some(true))
        })
        .or_else(|| base_td.container_definitions().first())
        .and_then(|c| c.name())
        .ok_or_else(|| "release: task def has no container to run in".to_string())?
        .to_string();

    // Register a one-shot task def revision with the pinned images.
    let family = base_td.family().unwrap_or(primary).to_string();
    let mut reg = ecs
        .register_task_definition()
        .family(&family)
        .set_container_definitions(Some(containers))
        .set_cpu(base_td.cpu().map(str::to_string))
        .set_memory(base_td.memory().map(str::to_string))
        .set_network_mode(base_td.network_mode().cloned())
        .set_requires_compatibilities(Some(base_td.requires_compatibilities().to_vec()))
        .set_execution_role_arn(base_td.execution_role_arn().map(str::to_string))
        .set_task_role_arn(base_td.task_role_arn().map(str::to_string))
        .set_runtime_platform(base_td.runtime_platform().cloned());
    if !base_td.volumes().is_empty() {
        reg = reg.set_volumes(Some(base_td.volumes().to_vec()));
    }
    let release_td_arn = reg
        .send()
        .await
        .map_err(|e| format!("release: register_task_definition failed: {e}"))?
        .task_definition()
        .and_then(|t| t.task_definition_arn().map(str::to_string))
        .ok_or_else(|| "release: no task def ARN returned".to_string())?;

    // Run the one-shot task with the release command overriding the container's
    // command, on the service's own network.
    let overrides = TaskOverride::builder()
        .container_overrides(
            ContainerOverride::builder()
                .name(&release_container)
                .set_command(Some(release_argv(release_cmd)))
                .build(),
        )
        .build();
    let mut run = ecs
        .run_task()
        .cluster(cluster)
        .task_definition(&release_td_arn)
        .launch_type(LaunchType::Fargate)
        .overrides(overrides)
        .started_by("amos-deploy-release");
    if let Some(n) = net.clone() {
        run = run.network_configuration(n);
    }
    let task_arn = run
        .send()
        .await
        .map_err(|e| format!("release: run_task failed: {e}"))?
        .tasks()
        .first()
        .and_then(|t| t.task_arn().map(str::to_string))
        .ok_or_else(|| "release: run_task returned no task".to_string())?;
    let task_id = task_arn.rsplit('/').next().unwrap_or(&task_arn).to_string();

    // Wait for the task to STOP (~10 min cap: 60 × 10s).
    let mut stopped: Option<aws_sdk_ecs::types::Task> = None;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        if let Ok(r) = ecs
            .describe_tasks()
            .cluster(cluster)
            .tasks(&task_arn)
            .send()
            .await
        {
            if let Some(t) = r.tasks().first() {
                if t.last_status() == Some("STOPPED") {
                    stopped = Some(t.clone());
                    break;
                }
            }
        }
    }
    let Some(task) = stopped else {
        return Err(format!(
            "release '{release_cmd}' did not finish within the time limit (task {task_id})"
        ));
    };

    // Exit code of the release container → the gate.
    let exit = task
        .containers()
        .iter()
        .find(|c| c.name() == Some(release_container.as_str()))
        .and_then(|c| c.exit_code());

    match release_gate(exit) {
        ReleaseDecision::Proceed => Ok(format!(
            "release '{release_cmd}' succeeded (exit 0, task {task_id})"
        )),
        ReleaseDecision::Abort(why) => {
            // Best-effort log tail so the failure is actionable from the receipt.
            let tail = release_log_tail(aws_cfg, &base_td, &release_container, &task_id)
                .await
                .unwrap_or_default();
            let reason = task.stopped_reason().unwrap_or("");
            Err(format!(
                "release '{release_cmd}' FAILED — {why}{}{} (task {task_id})",
                if reason.is_empty() {
                    String::new()
                } else {
                    format!("; stopped_reason: {reason}")
                },
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("; logs: {tail}")
                },
            ))
        }
    }
}

/// Best-effort tail of a stopped release task's CloudWatch logs (last few lines)
/// from the container's awslogs config. `None` if not awslogs-configured or
/// unreadable — diagnostics only, never fatal.
async fn release_log_tail(
    aws_cfg: &aws_config::SdkConfig,
    base_td: &aws_sdk_ecs::types::TaskDefinition,
    container: &str,
    task_id: &str,
) -> Option<String> {
    let logcfg = base_td
        .container_definitions()
        .iter()
        .find(|c| c.name() == Some(container))?
        .log_configuration()?;
    let opts = logcfg.options()?;
    let group = opts.get("awslogs-group")?.to_string();
    let prefix = opts
        .get("awslogs-stream-prefix")
        .cloned()
        .unwrap_or_else(|| "ecs".to_string());
    let stream = format!("{prefix}/{container}/{task_id}");
    let logs = aws_sdk_cloudwatchlogs::Client::new(aws_cfg);
    let resp = logs
        .get_log_events()
        .log_group_name(group)
        .log_stream_name(stream)
        .limit(15)
        .start_from_head(false)
        .send()
        .await
        .ok()?;
    let lines: Vec<String> = resp
        .events()
        .iter()
        .filter_map(|e| e.message().map(|m| m.trim().to_string()))
        .filter(|m| !m.is_empty())
        .collect();
    (!lines.is_empty()).then(|| lines.join(" | "))
}

/// Poll the outcome of a `deploy` by its `deploy_id`. Scope `app:read`. Reads
/// the terminal deploy receipt: `succeeded` (verified) / `failed`, or
/// `deploying` if none exists yet — so CI can block on the real deploy result.
async fn tool_deploy_status(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let deploy_id = arg_str(&args, "deploy_id")?;
    let row: Option<(bool, Value)> = sqlx::query_as(
        "SELECT verified, receipt FROM operation_receipts \
         WHERE tenant_id = $1 AND operation = 'deploy' \
           AND receipt->'inputs'->>'deploy_id' = $2 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .bind(&deploy_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("lookup deploy receipt: {e}")))?;
    match row {
        None => Ok(serde_json::json!({
            "deploy_id": deploy_id,
            "status": "deploying",
            "note": "no terminal receipt yet — build/roll still running",
        })),
        Some((verified, receipt)) => Ok(serde_json::json!({
            "deploy_id": deploy_id,
            "status": if verified { "succeeded" } else { "failed" },
            "verified": verified,
            "summary": receipt.get("summary").and_then(|v| v.as_str()).unwrap_or(""),
            "receipt": receipt,
        })),
    }
}

/// Stand up an isolated environment (staging/preview) for an existing app: its
/// own database + secret + ECS service + ALB host-rule, linked as a deployment.
/// Scope `env:provision`. Data isolation is tier-based: standard tenants get a
/// fresh database on the shared envs RDS (`ENVS_DATABASE_ADMIN_URL`); compliance
/// tenants pass `db_secret_arn` for a dedicated RDS (PHI never shares an
/// instance). Reproduces the app by cloning its live ECS task definition.
async fn tool_provision_env(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
    use crate::provisioning::{db, env_provision::EnvProvisioner};

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let app_name = arg_str(&args, "app_name")?;
    let env_name = arg_str(&args, "env_name")?;
    if env_name.is_empty()
        || !env_name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(ToolError::InvalidParams(
            "env_name must be lowercase letters, digits, or hyphen".into(),
        ));
    }
    let db_secret_arn_arg = args
        .get("db_secret_arn")
        .and_then(|v| v.as_str())
        .map(String::from);
    let ttl_hours = args.get("ttl_hours").and_then(|v| v.as_i64());
    let env_full = format!("{app_name}-{env_name}");

    let provisioner = EnvProvisioner::from_env().await.ok_or_else(|| {
        ToolError::Execution("env provisioner not configured (AWS_APP_ENABLED)".into())
    })?;
    let alb = state
        .alb_router
        .clone()
        .ok_or_else(|| ToolError::Execution("ALB router not configured".into()))?;

    // Resolve the base app + its primary service + secret.
    let row: Option<(Option<Value>,)> = sqlx::query_as(
        "SELECT aws_meta FROM app_deployments \
         WHERE tenant_id = $1 AND name = $2 AND status != 'deprovisioned'",
    )
    .bind(tenant_id)
    .bind(&app_name)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("lookup app: {e}")))?;
    let aws_meta = row
        .ok_or_else(|| ToolError::Execution(format!("no app named '{app_name}' for this tenant")))?
        .0
        .ok_or_else(|| ToolError::Execution("base app has no aws_meta".into()))?;
    let base_service = aws_meta
        .get("ecs_services")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::Execution("base app aws_meta.ecs_services is empty".into()))?
        .to_string();
    let base_secret_arn = aws_meta
        .get("db_secret_arn")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::Execution("base app aws_meta.db_secret_arn not set".into()))?
        .to_string();

    // Tier → data isolation.
    let plan: String = sqlx::query_scalar("SELECT plan FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| ToolError::Execution(format!("tenant plan: {e}")))?;
    let compliance = plan == "app_compliance";

    // Ingress first — the public URL feeds the env overrides.
    let base = provisioner
        .describe_base_service(&base_service)
        .await
        .map_err(map_amos_err)?;
    let route = alb
        .setup_service_routing(&env_full, base.public_port as u16, "/")
        .await
        .map_err(map_amos_err)?;

    let (env_secret_arn, db_mode) = if compliance {
        let arn = db_secret_arn_arg.ok_or_else(|| {
            ToolError::InvalidParams(
                "compliance-tier app: pass db_secret_arn for a dedicated RDS (its secret must hold \
                 DATABASE_URL + the app's other keys; PHI never shares an instance)"
                    .into(),
            )
        })?;
        (arn, "dedicated")
    } else {
        let admin = std::env::var("ENVS_DATABASE_ADMIN_URL").map_err(|_| {
            ToolError::Execution(
                "standard-tier env DB host not configured (set ENVS_DATABASE_ADMIN_URL)".into(),
            )
        })?;
        let db_name = format!("{app_name}_{env_name}").replace('-', "_");
        let env_db_url = db::create_named_database(&admin, &db_name)
            .await
            .map_err(map_amos_err)?;
        let secret_name = format!("{env_full}/app-config");
        let arn = provisioner
            .clone_secret(&base_secret_arn, &secret_name, &env_db_url)
            .await
            .map_err(map_amos_err)?;
        (arn, "shared")
    };

    // Clone the live task def with secrets repointed at the env secret.
    let base_td = provisioner
        .load_task_def(&base.task_def_arn)
        .await
        .map_err(map_amos_err)?;
    let overrides = vec![
        ("DEV_SEED_USERS".to_string(), "true".to_string()),
        ("CORS_ORIGINS".to_string(), route.public_url.clone()),
        ("NEXT_PUBLIC_API_URL".to_string(), route.public_url.clone()),
    ];
    let td_arn = provisioner
        .register_cloned_task_def(
            &env_full,
            &base_td,
            &base_secret_arn,
            &env_secret_arn,
            &overrides,
        )
        .await
        .map_err(map_amos_err)?;
    let service_arn = provisioner
        .create_env_service(
            &env_full,
            &td_arn,
            &route.target_group_arn,
            &base.public_container,
            base.public_port,
        )
        .await
        .map_err(map_amos_err)?;

    // Link as a deployment so deploy(env=) / db_query / db_write / app_status see it.
    let deployment_id = Uuid::new_v4();
    let expires_at =
        ttl_hours.map(|h| (chrono::Utc::now() + chrono::Duration::hours(h)).to_rfc3339());
    let spec = serde_json::json!({"name": env_full, "parent": app_name, "env": env_name});
    let meta = serde_json::json!({
        "task_def_arn": td_arn,
        "service_arn": service_arn,
        "public_url": route.public_url,
        "ecs_services": [env_full],
        "db_secret_arn": env_secret_arn,
        "db_mode": db_mode,
        "env": env_name,
        "parent_app": app_name,
        "ttl_hours": ttl_hours,
        "expires_at": expires_at,
        "target_group_arn": route.target_group_arn,
        "listener_rule_arn": route.listener_rule_arn,
    });
    sqlx::query(
        "INSERT INTO app_deployments (id, tenant_id, name, spec, provider, status, aws_meta) \
         VALUES ($1, $2, $3, $4, 'ecs', 'running', $5)",
    )
    .bind(deployment_id)
    .bind(tenant_id)
    .bind(&env_full)
    .bind(&spec)
    .bind(&meta)
    .execute(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("insert deployment: {e}")))?;
    let public_image = base_td
        .container_definitions()
        .iter()
        .find(|c| c.name() == Some(base.public_container.as_str()))
        .and_then(|c| c.image())
        .unwrap_or_default()
        .to_string();
    sqlx::query(
        "INSERT INTO app_services (deployment_id, service_name, image, expose_public, status) \
         VALUES ($1, $2, $3, TRUE, 'running') ON CONFLICT (deployment_id, service_name) DO NOTHING",
    )
    .bind(deployment_id)
    .bind(&env_full)
    .bind(&public_image)
    .execute(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("insert service: {e}")))?;

    let tier = if compliance { "compliance" } else { "standard" };
    let intent = Intent {
        summary: format!("provision isolated env '{env_name}' for app '{app_name}'"),
        self_modifying: false,
        scope_classification: "env_provision".to_string(),
    };
    let receipt = OperationReceipt::new("provision_env", tenant_id, actor, intent)
        .guardrail("tier_isolation")
        .guardrail(if compliance {
            "dedicated_rds"
        } else {
            "shared_instance_db"
        })
        .inputs(serde_json::json!({
            "app_name": app_name,
            "env_name": env_name,
            "tier": tier,
            "deployment_id": deployment_id,
        }))
        .check(
            "env_provisioned",
            CheckStatus::Passed,
            format!("service {env_full} + ingress {}", route.public_url),
        )
        .outputs(serde_json::json!({
            "deployment_id": deployment_id,
            "public_url": route.public_url,
            "db_mode": db_mode,
        }))
        .summary(format!(
            "provisioned env {env_full} at {}",
            route.public_url
        ));
    let _ = proof::emit(&state.db, &receipt).await;

    Ok(serde_json::json!({
        "deployment_id": deployment_id,
        "name": env_full,
        "public_url": route.public_url,
        "tier": tier,
        "db_mode": db_mode,
        "note": "service starting; poll app_status with the deployment_id. Roll code into it with deploy(env=...).",
    }))
}

/// Tear down an isolated environment created by `provision_env` (scope
/// `env:provision`): delete the ECS service + ALB rule/target-group, and — for
/// shared-tier envs — drop the env database and its secret. Dedicated (PHI)
/// RDS instances are operator-owned and left untouched. Best-effort on the AWS
/// deletes; always removes the deployment row.
async fn tool_teardown_env(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
    use crate::provisioning::{db, env_provision::EnvProvisioner};

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let app_name = arg_str(&args, "app_name")?;
    let env_name = arg_str(&args, "env_name")?;
    let env_full = format!("{app_name}-{env_name}");

    let row: Option<(Uuid, Option<Value>)> = sqlx::query_as(
        "SELECT id, aws_meta FROM app_deployments WHERE tenant_id = $1 AND name = $2",
    )
    .bind(tenant_id)
    .bind(&env_full)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("lookup env: {e}")))?;
    let (deployment_id, aws_meta) = row.ok_or(ToolError::NotFound)?;
    let aws_meta = aws_meta.unwrap_or_else(|| serde_json::json!({}));

    let provisioner = EnvProvisioner::from_env().await.ok_or_else(|| {
        ToolError::Execution("env provisioner not configured (AWS_APP_ENABLED)".into())
    })?;

    // ECS service.
    provisioner.delete_service(&env_full).await;
    // ALB rule + target group.
    if let Some(alb) = state.alb_router.clone() {
        alb.teardown_routing(
            aws_meta.get("listener_rule_arn").and_then(|v| v.as_str()),
            aws_meta.get("target_group_arn").and_then(|v| v.as_str()),
        )
        .await;
    }
    // Shared-tier data cleanup (dedicated RDS is operator-owned — leave it).
    let db_mode = aws_meta
        .get("db_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if db_mode == "shared" {
        if let Ok(admin) = std::env::var("ENVS_DATABASE_ADMIN_URL") {
            let db_name = format!("{app_name}_{env_name}").replace('-', "_");
            db::drop_harness_database(&admin, &db_name).await;
        }
        if let Some(arn) = aws_meta.get("db_secret_arn").and_then(|v| v.as_str()) {
            provisioner.delete_secret(arn).await;
        }
    }
    // Remove the deployment (app_services cascade via FK).
    sqlx::query("DELETE FROM app_deployments WHERE id = $1")
        .bind(deployment_id)
        .execute(&state.db)
        .await
        .map_err(|e| ToolError::Execution(format!("delete deployment: {e}")))?;

    let intent = Intent {
        summary: format!("tear down env '{env_name}' of app '{app_name}'"),
        self_modifying: false,
        scope_classification: "env_provision".to_string(),
    };
    let receipt = OperationReceipt::new("teardown_env", tenant_id, actor, intent)
        .guardrail("dedicated_rds_preserved")
        .inputs(serde_json::json!({ "app_name": app_name, "env_name": env_name, "deployment_id": deployment_id }))
        .check("env_torn_down", CheckStatus::Passed, format!("removed {env_full}"))
        .summary(format!("tore down env {env_full}"));
    let _ = proof::emit(&state.db, &receipt).await;

    Ok(serde_json::json!({ "torn_down": env_full, "deployment_id": deployment_id }))
}

/// Roll a deployment's ECS service(s) to pick up a freshly-built image. Scope
/// `app:deploy`. Issues `force-new-deployment` on each service in the
/// deployment's `aws_meta.ecs_services` (falling back to the deployment name) —
/// no task-def surgery, so all task settings are preserved. Pairs with CI that
/// rebuilds the image under the tag the task def already references. Multi-
/// service safe (Nuvola web+sidekiq) and single-service (Cuspr). Emits a receipt.
/// Whether a force-new-deployment would be a no-op for shipping new code: a
/// digest-pinned image (`repo@sha256:…`) is immutable, so re-pulling it can
/// never pick up a freshly-built image. Pure — unit-tested.
fn redeploy_would_noop(image: &str) -> bool {
    image.contains("@sha256:")
}

/// The container images referenced by a service's current task definition
/// (best-effort; empty on any lookup error).
async fn service_container_images(
    ecs: &aws_sdk_ecs::Client,
    cluster: &str,
    svc: &str,
) -> Vec<String> {
    let td_arn = match ecs
        .describe_services()
        .cluster(cluster)
        .services(svc)
        .send()
        .await
    {
        Ok(r) => r
            .services()
            .first()
            .and_then(|s| s.task_definition().map(str::to_string)),
        Err(_) => None,
    };
    let Some(td_arn) = td_arn else {
        return Vec::new();
    };
    match ecs
        .describe_task_definition()
        .task_definition(&td_arn)
        .send()
        .await
    {
        Ok(r) => r
            .task_definition()
            .map(|t| {
                t.container_definitions()
                    .iter()
                    .filter_map(|c| c.image().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn tool_app_redeploy(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let image = args
        .get("image")
        .and_then(|v| v.as_str())
        .unwrap_or("(current task-def tag)")
        .to_string();

    let row: Option<(String, Option<Value>)> = sqlx::query_as(
        "SELECT name, aws_meta FROM app_deployments WHERE id = $1 AND tenant_id = $2",
    )
    .bind(deployment_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("lookup deployment: {e}")))?;
    let (name, aws_meta) = row.ok_or(ToolError::NotFound)?;
    let services: Vec<String> = aws_meta
        .as_ref()
        .and_then(|m| m.get("ecs_services"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![name.clone()]);

    let cluster =
        std::env::var("ECS_CLUSTER").unwrap_or_else(|_| "swarm-infrastructure-cluster".to_string());
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let ecs = aws_sdk_ecs::Client::new(&cfg);

    // Guard: force-new-deployment re-pulls the image the task def already
    // references. If that image is digest-pinned (`@sha256:`), the roll can NOT
    // pick up newly-built code — it just restarts the frozen digest (this
    // silently no-op'd a tenant's CD for a full day, reporting success the whole
    // time). Refuse with a clear steer to `deploy` (which builds + re-pins a
    // fresh digest), unless force=true (an intentional restart of the same image).
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    if !force {
        let mut pinned: Vec<Value> = Vec::new();
        for svc in &services {
            for img in service_container_images(&ecs, &cluster, svc).await {
                if redeploy_would_noop(&img) {
                    pinned.push(serde_json::json!({ "service": svc, "image": img }));
                }
            }
        }
        if !pinned.is_empty() {
            let intent = Intent {
                summary: format!("app_redeploy no-op guard tripped for '{name}'"),
                self_modifying: false,
                scope_classification: "deploy".to_string(),
            };
            let receipt = OperationReceipt::new("app_redeploy", tenant_id, actor.clone(), intent)
                .guardrail("pinned_noop_guard")
                .inputs(serde_json::json!({
                    "deployment_id": deployment_id,
                    "image": image,
                    "services": services,
                }))
                .check(
                    "would_ship_new_code",
                    CheckStatus::Failed,
                    "task def image is digest-pinned; force-new-deployment re-pulls the same digest and ships no new code",
                )
                .outputs(serde_json::json!({ "pinned_services": pinned.clone() }))
                .summary(format!(
                    "app_redeploy is a no-op for '{name}' (digest-pinned) — use `deploy`"
                ));
            let _ = proof::emit(&state.db, &receipt).await;
            return Ok(serde_json::json!({
                "deployment_id": deployment_id,
                "no_op": true,
                "status": "blocked",
                "pinned_services": pinned,
                "message": "app_redeploy re-pulls the pinned digest(s) and will NOT pick up new code — use the `deploy` verb (it builds and re-pins a fresh digest into a new task-def revision). Pass force=true to restart the current image anyway.",
            }));
        }
    }

    let mut rolled = Vec::new();
    for svc in &services {
        ecs.update_service()
            .cluster(&cluster)
            .service(svc)
            .force_new_deployment(true)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("force-new-deployment {svc}: {e}")))?;
        rolled.push(svc.clone());
    }

    let intent = Intent {
        summary: format!("rolling redeploy of '{name}' ({} service(s))", rolled.len()),
        self_modifying: false,
        scope_classification: "deploy".to_string(),
    };
    let receipt = OperationReceipt::new("app_redeploy", tenant_id, actor, intent)
        .guardrail("force_new_deployment")
        .guardrail("rolling_health_checked")
        .inputs(serde_json::json!({
            "deployment_id": deployment_id,
            "image": image,
            "services": rolled,
        }))
        .check(
            "redeploy_triggered",
            CheckStatus::Passed,
            format!("{} service(s) rolling", rolled.len()),
        )
        .outputs(serde_json::json!({ "services": rolled }))
        .summary(format!(
            "rolling redeploy of {name} ({} service(s))",
            rolled.len()
        ));
    let _ = proof::emit(&state.db, &receipt).await;

    Ok(serde_json::json!({
        "deployment_id": deployment_id,
        "redeployed": rolled,
        "image": image,
        "note": "force-new-deployment issued; poll app_status for rollout state",
    }))
}

/// Governed read-only SQL against the tenant's *app* DB. Scope `db:read`.
/// Resolves the connection from the deployment's `aws_meta.db_secret_arn`
/// (read at query time — no creds stored in the control plane), runs inside a
/// READ ONLY transaction with a row cap + statement timeout, emits a receipt.
/// Ownership-checked resolution of a deployment's app-DB connection URL. The
/// `db_secret_arn` in the deployment's `aws_meta` is read from Secrets Manager
/// at call time and never persisted in the control plane. Shared by the
/// governed db_query (read) and db_write (write) verbs.
async fn deployment_db_url(
    state: &PlatformState,
    tenant_id: Uuid,
    deployment_id: Uuid,
) -> Result<String, ToolError> {
    use crate::provisioning::db_query;

    let row: Option<(Option<Value>,)> =
        sqlx::query_as("SELECT aws_meta FROM app_deployments WHERE id = $1 AND tenant_id = $2")
            .bind(deployment_id)
            .bind(tenant_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| ToolError::Execution(format!("lookup deployment: {e}")))?;
    let aws_meta = row.ok_or(ToolError::NotFound)?.0.ok_or_else(|| {
        ToolError::Execution(
            "deployment has no DB config (set aws_meta.db_secret_arn to enable DB access)".into(),
        )
    })?;
    let secret_arn = aws_meta
        .get("db_secret_arn")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::Execution("deployment aws_meta.db_secret_arn not set".into()))?;

    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let sm = aws_sdk_secretsmanager::Client::new(&cfg);
    let secret = sm
        .get_secret_value()
        .secret_id(secret_arn)
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("read DB secret: {e}")))?;
    let secret_str = secret.secret_string().unwrap_or_default();
    db_query::resolve_db_url(&aws_meta, secret_str).ok_or_else(|| {
        ToolError::Execution(
            "could not derive a postgres URL (set aws_meta.db_host/db_user/db_name)".into(),
        )
    })
}

/// Ownership-checked lookup of a deployment's `aws_meta` JSON (used by the
/// storage verbs, mirrors `deployment_db_url`'s lookup).
async fn deployment_aws_meta(
    state: &PlatformState,
    tenant_id: Uuid,
    deployment_id: Uuid,
) -> Result<Value, ToolError> {
    let row: Option<(Option<Value>,)> =
        sqlx::query_as("SELECT aws_meta FROM app_deployments WHERE id = $1 AND tenant_id = $2")
            .bind(deployment_id)
            .bind(tenant_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| ToolError::Execution(format!("lookup deployment: {e}")))?;
    row.ok_or(ToolError::NotFound)?
        .0
        .ok_or_else(|| ToolError::Execution("deployment has no aws_meta".into()))
}

/// Resolve the tenant app's S3 bucket (and optional confining prefix) from a
/// deployment's `aws_meta`. Every storage op is confined to this bucket+prefix.
fn resolve_bucket(aws_meta: &Value) -> Result<(String, Option<String>), ToolError> {
    let bucket = aws_meta
        .get("s3_bucket")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::Execution("deployment aws_meta.s3_bucket not set".into()))?
        .to_string();
    let prefix = aws_meta
        .get("s3_prefix")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string());
    Ok((bucket, prefix))
}

/// Build the effective object key: reject traversal, then confine under the
/// deployment's `s3_prefix` (if any). `key` may be empty (used for listing).
fn scoped_key(prefix: &Option<String>, key: &str) -> Result<String, ToolError> {
    if key.contains("..") || key.starts_with('/') {
        return Err(ToolError::InvalidParams(
            "key must not contain '..' or start with '/'".into(),
        ));
    }
    Ok(match prefix {
        Some(p) => format!("{}/{}", p, key.trim_start_matches('/')),
        None => key.to_string(),
    })
}

const S3_INLINE_MAX: usize = 5 * 1024 * 1024; // 5 MiB inline get/put cap

async fn s3_client() -> aws_sdk_s3::Client {
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    aws_sdk_s3::Client::new(&cfg)
}

/// List objects in the app's bucket under the (confining + caller) prefix.
async fn tool_s3_list(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let user_prefix = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
    let max_keys = args
        .get("max_keys")
        .and_then(|v| v.as_i64())
        .unwrap_or(1000)
        .clamp(1, 1000) as i32;

    let aws_meta = deployment_aws_meta(state, tenant_id, deployment_id).await?;
    let (bucket, prefix) = resolve_bucket(&aws_meta)?;
    let list_prefix = scoped_key(&prefix, user_prefix)?;

    let s3 = s3_client().await;
    let resp = s3
        .list_objects_v2()
        .bucket(&bucket)
        .prefix(&list_prefix)
        .max_keys(max_keys)
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("s3 list failed: {e}")))?;

    // Strip the confining prefix back off for display so callers see app-relative keys.
    let strip = prefix.as_ref().map(|p| format!("{p}/"));
    let keys: Vec<Value> = resp
        .contents()
        .iter()
        .map(|o| {
            let k = o.key().unwrap_or_default();
            let disp = match &strip {
                Some(s) => k.strip_prefix(s.as_str()).unwrap_or(k),
                None => k,
            };
            serde_json::json!({
                "key": disp,
                "size": o.size().unwrap_or(0),
                "last_modified_epoch": o.last_modified().map(|t| t.secs()),
            })
        })
        .collect();
    Ok(serde_json::json!({ "bucket": bucket, "count": keys.len(), "keys": keys }))
}

/// Read an object (inline, ≤5 MiB) or, with `presign:true`, return a temporary
/// presigned GET URL (no inline size limit — use for large/binary objects).
async fn tool_s3_get(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use base64::Engine;

    let tenant_id = resolve_tenant(claims, &args)?;
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let key = arg_str(&args, "key")?;
    let presign = args
        .get("presign")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let expires_in = args
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(900)
        .clamp(1, 604800) as u64;

    let aws_meta = deployment_aws_meta(state, tenant_id, deployment_id).await?;
    let (bucket, prefix) = resolve_bucket(&aws_meta)?;
    let full_key = scoped_key(&prefix, &key)?;
    let s3 = s3_client().await;

    if presign {
        let pc = aws_sdk_s3::presigning::PresigningConfig::expires_in(
            std::time::Duration::from_secs(expires_in),
        )
        .map_err(|e| ToolError::Execution(format!("presign config: {e}")))?;
        let req = s3
            .get_object()
            .bucket(&bucket)
            .key(&full_key)
            .presigned(pc)
            .await
            .map_err(|e| ToolError::Execution(format!("presign get failed: {e}")))?;
        return Ok(serde_json::json!({
            "key": key, "method": "GET",
            "presigned_url": req.uri().to_string(), "expires_in": expires_in,
        }));
    }

    let resp = s3
        .get_object()
        .bucket(&bucket)
        .key(&full_key)
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("s3 get failed: {e}")))?;
    let content_type = resp.content_type().map(|s| s.to_string());
    let data = resp
        .body
        .collect()
        .await
        .map_err(|e| ToolError::Execution(format!("read body: {e}")))?
        .into_bytes();
    if data.len() > S3_INLINE_MAX {
        return Err(ToolError::InvalidParams(format!(
            "object is {} bytes (> {S3_INLINE_MAX} inline cap); use presign:true for a download URL",
            data.len()
        )));
    }
    let (content, is_b64) = match std::str::from_utf8(&data) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (
            base64::engine::general_purpose::STANDARD.encode(&data),
            true,
        ),
    };
    Ok(serde_json::json!({
        "key": key, "content_type": content_type, "size": data.len(),
        "content": content, "base64": is_b64,
    }))
}

/// Write an object (inline body, ≤5 MiB) or, with `presign:true`, return a
/// presigned PUT URL for a browser/client to upload directly. Emits a receipt.
async fn tool_s3_put(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
    use base64::Engine;

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let key = arg_str(&args, "key")?;
    let presign = args
        .get("presign")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let content_type = args
        .get("content_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let aws_meta = deployment_aws_meta(state, tenant_id, deployment_id).await?;
    let (bucket, prefix) = resolve_bucket(&aws_meta)?;
    let full_key = scoped_key(&prefix, &key)?;
    let s3 = s3_client().await;

    if presign {
        let expires_in = args
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(900)
            .clamp(1, 604800) as u64;
        let pc = aws_sdk_s3::presigning::PresigningConfig::expires_in(
            std::time::Duration::from_secs(expires_in),
        )
        .map_err(|e| ToolError::Execution(format!("presign config: {e}")))?;
        let mut b = s3.put_object().bucket(&bucket).key(&full_key);
        if let Some(ct) = &content_type {
            b = b.content_type(ct);
        }
        let req = b
            .presigned(pc)
            .await
            .map_err(|e| ToolError::Execution(format!("presign put failed: {e}")))?;
        return Ok(serde_json::json!({
            "key": key, "method": "PUT",
            "presigned_url": req.uri().to_string(), "expires_in": expires_in,
        }));
    }

    let body = arg_str(&args, "body")?;
    let is_b64 = args
        .get("base64")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let bytes: Vec<u8> = if is_b64 {
        base64::engine::general_purpose::STANDARD
            .decode(body.as_bytes())
            .map_err(|e| ToolError::InvalidParams(format!("invalid base64 body: {e}")))?
    } else {
        body.into_bytes()
    };
    if bytes.len() > S3_INLINE_MAX {
        return Err(ToolError::InvalidParams(format!(
            "body is {} bytes (> {S3_INLINE_MAX} inline cap); use presign:true to upload directly",
            bytes.len()
        )));
    }
    let size = bytes.len();
    let mut b = s3
        .put_object()
        .bucket(&bucket)
        .key(&full_key)
        .body(aws_sdk_s3::primitives::ByteStream::from(bytes));
    if let Some(ct) = &content_type {
        b = b.content_type(ct);
    }
    let resp = b
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("s3 put failed: {e}")))?;
    let etag = resp.e_tag().map(|s| s.to_string());

    let intent = Intent {
        summary: format!("write object '{key}' to app deployment {deployment_id}"),
        self_modifying: true,
        scope_classification: "storage_write".to_string(),
    };
    let receipt = OperationReceipt::new("s3_put", tenant_id, actor, intent)
        .guardrail("bucket_scoped")
        .guardrail("prefix_confined")
        .inputs(serde_json::json!({ "deployment_id": deployment_id, "bucket": bucket, "key": key, "size": size }))
        .check("object_written", CheckStatus::Passed, format!("{size} bytes"))
        .outputs(serde_json::json!({ "etag": etag }))
        .summary(format!("s3_put wrote {size} bytes to {key}"));
    let _ = proof::emit(&state.db, &receipt).await;

    Ok(serde_json::json!({ "key": key, "etag": etag, "size": size }))
}

/// Delete an object from the app's bucket (prefix-confined). Emits a receipt.
async fn tool_s3_delete(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let key = arg_str(&args, "key")?;

    let aws_meta = deployment_aws_meta(state, tenant_id, deployment_id).await?;
    let (bucket, prefix) = resolve_bucket(&aws_meta)?;
    let full_key = scoped_key(&prefix, &key)?;
    let s3 = s3_client().await;
    s3.delete_object()
        .bucket(&bucket)
        .key(&full_key)
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("s3 delete failed: {e}")))?;

    let intent = Intent {
        summary: format!("delete object '{key}' from app deployment {deployment_id}"),
        self_modifying: true,
        scope_classification: "storage_write".to_string(),
    };
    let receipt = OperationReceipt::new("s3_delete", tenant_id, actor, intent)
        .guardrail("bucket_scoped")
        .guardrail("prefix_confined")
        .inputs(serde_json::json!({ "deployment_id": deployment_id, "bucket": bucket, "key": key }))
        .check("object_deleted", CheckStatus::Passed, key.clone())
        .summary(format!("s3_delete removed {key}"));
    let _ = proof::emit(&state.db, &receipt).await;

    Ok(serde_json::json!({ "deleted": key }))
}

async fn tool_db_query(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
    use crate::provisioning::db_query;

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let sql = arg_str(&args, "sql")?;
    let max_rows = args
        .get("max_rows")
        .and_then(|v| v.as_i64())
        .unwrap_or(1000);

    let db_url = deployment_db_url(state, tenant_id, deployment_id).await?;

    let rows = db_query::run_readonly_query(&db_url, &sql, max_rows)
        .await
        .map_err(map_amos_err)?;

    // Proof receipt — every query the AI runs is recorded (intent = the SQL).
    let intent = Intent {
        summary: format!("read-only query on app deployment {deployment_id}"),
        self_modifying: false,
        scope_classification: "db_read".to_string(),
    };
    let receipt = OperationReceipt::new("db_query", tenant_id, actor, intent)
        .guardrail("read_only_transaction")
        .guardrail("row_cap")
        .guardrail("statement_timeout")
        .inputs(serde_json::json!({
            "deployment_id": deployment_id,
            "sql": sql,
            "max_rows": max_rows,
        }))
        .check(
            "query_executed",
            CheckStatus::Passed,
            format!("{} row(s)", rows.len()),
        )
        .outputs(serde_json::json!({ "row_count": rows.len() }))
        .summary(format!("db_query returned {} row(s)", rows.len()));
    let _ = proof::emit(&state.db, &receipt).await;

    Ok(serde_json::json!({
        "deployment_id": deployment_id,
        "row_count": rows.len(),
        "rows": rows,
    }))
}

/// Governed single-statement DML write (scope `db:write`, owner-only). Mirrors
/// db_query but inverted for danger: INSERT/UPDATE/DELETE only, a WHERE is
/// required on UPDATE/DELETE (unless `allow_unfiltered`), the affected-row count
/// is capped, and `dry_run` measures the effect then rolls back. Every call —
/// including a dry run — emits a `self_modifying` proof receipt with the exact
/// SQL, params, and affected rows.
async fn tool_db_write(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
    use crate::provisioning::db_query;

    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let sql = arg_str(&args, "sql")?;
    let params: Vec<Value> = args
        .get("params")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let max_rows = args.get("max_rows").and_then(|v| v.as_i64()).unwrap_or(100);
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_unfiltered = args
        .get("allow_unfiltered")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let db_url = deployment_db_url(state, tenant_id, deployment_id).await?;

    let outcome =
        db_query::run_write_dml(&db_url, &sql, &params, max_rows, dry_run, allow_unfiltered)
            .await
            .map_err(map_amos_err)?;

    // Proof receipt — a write is self-modifying; record intent + exact effect.
    let mode = if dry_run { "dry_run" } else { "apply" };
    let intent = Intent {
        summary: format!("{mode} DML write on app deployment {deployment_id}"),
        self_modifying: true,
        scope_classification: "db_write".to_string(),
    };
    let mut receipt = OperationReceipt::new("db_write", tenant_id, actor, intent)
        .guardrail("single_statement")
        .guardrail("dml_only")
        .guardrail("where_required")
        .guardrail("row_cap")
        .guardrail("transaction")
        .inputs(serde_json::json!({
            "deployment_id": deployment_id,
            "sql": sql,
            "params": params,
            "max_rows": max_rows,
            "dry_run": dry_run,
            "allow_unfiltered": allow_unfiltered,
        }))
        .outputs(serde_json::json!({
            "affected_rows": outcome.affected,
            "committed": outcome.committed,
        }));
    receipt = if dry_run {
        receipt.check(
            "dry_run",
            CheckStatus::Passed,
            format!("would affect {} row(s); rolled back", outcome.affected),
        )
    } else {
        receipt.check(
            "write_committed",
            CheckStatus::Passed,
            format!("committed; {} row(s) affected", outcome.affected),
        )
    };
    let receipt = receipt.summary(format!(
        "db_write {} — {} row(s) affected{}",
        mode,
        outcome.affected,
        if outcome.committed {
            ""
        } else {
            " (not committed)"
        }
    ));
    let _ = proof::emit(&state.db, &receipt).await;

    Ok(serde_json::json!({
        "deployment_id": deployment_id,
        "affected_rows": outcome.affected,
        "committed": outcome.committed,
        "dry_run": dry_run,
        "rows": outcome.rows,
    }))
}

/// Read-only schema introspection against an app's database. With no `table`,
/// lists the public base tables; with a `table`, lists its columns (name, type,
/// nullability). Removes the "guess the column name → failed query → round-trip"
/// loop. Scope `db:read`; no receipt (schema metadata, not a data operation).
async fn tool_describe_table(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    use crate::provisioning::db_query;

    let tenant_id = resolve_tenant(claims, &args)?;
    let deployment_id = arg_uuid(&args, "deployment_id")?;
    let table = args
        .get("table")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let db_url = deployment_db_url(state, tenant_id, deployment_id).await?;
    let sql = match &table {
        Some(t) => {
            // The table name is used as a string literal below; require a plain
            // identifier so it can't carry SQL.
            if t.is_empty() || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(ToolError::InvalidParams(
                    "table must be a plain identifier (letters, digits, underscore)".into(),
                ));
            }
            format!(
                "SELECT column_name, data_type, is_nullable, character_maximum_length \
                 FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = '{t}' \
                 ORDER BY ordinal_position"
            )
        }
        None => "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
                 ORDER BY table_name"
            .to_string(),
    };

    let rows = db_query::run_readonly_query(&db_url, &sql, 2000)
        .await
        .map_err(map_amos_err)?;

    if table.is_some() && rows.is_empty() {
        return Err(ToolError::NotFound);
    }
    Ok(serde_json::json!({
        "deployment_id": deployment_id,
        "table": table,
        "kind": if table.is_some() { "columns" } else { "tables" },
        "count": rows.len(),
        "rows": rows,
    }))
}

/// Return the caller's identity and *effective* scopes — so an agent knows what
/// it can do before it tries, instead of discovering a missing scope at call
/// time. No scope gate: any authenticated principal may introspect itself.
async fn tool_whoami(
    _state: &PlatformState,
    claims: &Claims,
    _args: Value,
) -> Result<Value, ToolError> {
    let mut scopes: Vec<String> =
        crate::rbac::effective_scopes(&claims.role, claims.scopes.as_deref())
            .into_iter()
            .collect();
    scopes.sort();
    Ok(serde_json::json!({
        "sub": claims.sub,
        "tenant_id": claims.tenant_id,
        "tenant_slug": claims.tenant_slug,
        "role": claims.role,
        // API-key principals carry an explicit scope list; JWT users don't.
        "principal_type": if claims.scopes.is_some() { "api_key" } else { "user" },
        "scopes": scopes,
    }))
}

/// Verb catalog for the `get_started` manifest: `(verb, category, purpose)`.
/// The required SCOPE is derived from [`crate::rbac::required_scope`] at runtime
/// (not stored here) so the manifest can never drift from the actual gate.
const VERB_CATALOG: &[(&str, &str, &str)] = &[
    ("whoami", "meta", "Your identity, tenant, and effective scopes."),
    ("get_started", "meta", "This capability manifest + golden paths (start here)."),
    ("onboard_app", "meta", "Scaffold a NEW app: generates a validated amos.yaml + the exact deploy playbook + human checklist."),
    ("list_apps", "app", "List your deployed apps and their services."),
    ("app_status", "app", "Live status + last deploy of an app."),
    ("app_logs", "app", "Recent logs for an app service."),
    ("deploy", "app", "THE way to ship new code: build from git (amos.yaml) + roll, digest-pinned."),
    ("deploy_status", "app", "Poll a deploy_id to its terminal result + proof receipt (with timings)."),
    ("deploy_app", "app", "Lower-level app deploy (prefer `deploy`)."),
    ("app_redeploy", "app", "Restart the CURRENT image — does NOT ship new code (re-pulls the pinned digest). Use `deploy`."),
    ("app_control", "app", "Start / stop / restart / deprovision an app."),
    ("provision_env", "env", "Stand up an isolated preview/staging environment."),
    ("teardown_env", "env", "Tear down a provisioned environment."),
    ("build_image", "build", "Build a container image (git or context) via CodeBuild."),
    ("build_status", "build", "Poll a build to completion."),
    ("db_query", "db", "Read-only SQL against your app database."),
    ("db_write", "db", "Single-statement DML write to your app DB (owner-only, proof-carrying)."),
    ("describe_table", "db", "Inspect a table's schema before querying."),
    ("s3_list", "storage", "List objects in your app bucket."),
    ("s3_get", "storage", "Read an object."),
    ("s3_put", "storage", "Write an object."),
    ("s3_delete", "storage", "Delete an object."),
    ("finance_board", "finance", "Budget vs actual vs live, by category and month."),
    ("finance_truth", "finance", "Ground-truth monthly cash (Stripe net + QBO income + subs)."),
    ("finance_history", "finance", "Historical revenue snapshots."),
    ("revenue_summary", "finance", "Latest per-company revenue / MRR."),
    ("qbo_accounts", "finance", "QuickBooks P&L account names (for mappings)."),
    ("org_subscriptions", "finance", "Subscriptions grouped by org."),
    ("churn_snapshot", "finance", "Cancellations + pending cancels."),
    ("create_finance_line", "finance", "Create a finance category/line (proof-carrying)."),
    ("update_finance_line", "finance", "Update a finance line (proof-carrying)."),
    ("set_finance_actual", "finance", "Record a monthly actual (proof-carrying)."),
    ("set_finance_budget", "finance", "Set a monthly budget (proof-carrying)."),
    ("set_finance_mapping", "finance", "Map a line to a QBO account / live source (proof-carrying)."),
    ("set_billing_key", "finance", "Connect a finance integration credential (owner-only, secret redacted in the receipt)."),
    ("list_harnesses", "harness", "List harness instances."),
    ("harness_status", "harness", "Harness status."),
    ("harness_logs", "harness", "Harness logs."),
    ("get_harness_config", "harness", "Read harness config."),
    ("set_harness_config", "harness", "Set harness config."),
    ("provision_harness", "harness", "Provision a harness."),
    ("harness_control", "harness", "Start / stop a harness."),
    ("list_releases", "harness", "List available harness releases."),
    ("list_receipts", "audit", "Proof receipts for every governed operation on your tenant."),
];

/// `get_started` — the agent-facing capability manifest. Pure metadata (no scope
/// gate): a freshly-connected AI reads this to know its full verb surface (with
/// per-verb scope + an `allowed` flag personalized to the caller), the golden
/// paths, and the human-only steps it should relay to the owner.
async fn tool_get_started(
    _state: &PlatformState,
    claims: &Claims,
    _args: Value,
) -> Result<Value, ToolError> {
    let caps = crate::rbac::effective_scopes(&claims.role, claims.scopes.as_deref());
    let mut my_scopes: Vec<String> = caps.iter().cloned().collect();
    my_scopes.sort();

    // Verbs grouped by category; scope derived from rbac (cannot drift) and an
    // `allowed` flag so the AI sees what IT can do, not just what exists.
    let mut by_cat: std::collections::BTreeMap<&str, Vec<Value>> =
        std::collections::BTreeMap::new();
    for (name, cat, purpose) in VERB_CATALOG {
        let scope = crate::rbac::required_scope(name);
        let allowed = scope.map(|s| caps.contains(s)).unwrap_or(true);
        by_cat.entry(cat).or_default().push(serde_json::json!({
            "verb": name,
            "purpose": purpose,
            "scope": scope,
            "allowed": allowed,
        }));
    }

    Ok(serde_json::json!({
        "you": {
            "tenant_id": claims.tenant_id,
            "role": claims.role,
            "scopes": my_scopes,
            "principal_type": if claims.scopes.is_some() { "api_key" } else { "user" },
        },
        "verbs_by_category": by_cat,
        "golden_paths": [
            { "goal": "Ship new code", "scopes": ["app:deploy"], "steps": [
                "Write an amos.yaml: name + services (dockerfile per service) + optional `release:` (a migration/command run, gated, before rollout).",
                "Call deploy(git_repo, git_ref, manifest=<amos.yaml>). It builds from git, runs the gated release step, and rolls services DIGEST-PINNED.",
                "Poll deploy_status(deploy_id) until succeeded/failed; the receipt shows per-phase timings (build vs release vs roll).",
                "Do NOT use app_redeploy to ship new code — it re-pulls the pinned digest and changes nothing."
            ]},
            { "goal": "Stand up a preview/staging env", "scopes": ["env:provision"], "steps": [
                "provision_env(...) to create an isolated env, deploy(env=<name>) to load it, teardown_env when done."
            ]},
            { "goal": "Operate finances", "scopes": ["finance:read", "finance:write"], "steps": [
                "finance_board to see budget vs actual vs live.",
                "set_finance_budget / set_finance_actual to edit (proof-carrying).",
                "qbo_accounts + set_finance_mapping to wire QuickBooks; set_billing_key (finance:connect, owner) to connect credentials."
            ]},
            { "goal": "Query your app data", "scopes": ["db:read"], "steps": [
                "describe_table to learn the schema, then db_query (read-only SQL). db_write for single-statement DML (owner-only)."
            ]},
            { "goal": "Know what you can do", "scopes": [], "steps": [
                "whoami → your scopes. This manifest marks each verb allowed:true/false for you."
            ]},
            { "goal": "Audit what happened", "scopes": ["receipts:read"], "steps": [
                "list_receipts → every governed operation with its proof receipt."
            ]}
        ],
        "human_steps": onboarding_human_steps(),
        "note": "Every write is proof-carrying — see list_receipts. Tools are tenant-scoped and an API key never exceeds its creator's role. Call whoami for your exact scopes.",
    }))
}

/// Human-only onboarding steps the AI relays to the owner. Shared by
/// `get_started` and `onboard_app` so the guidance can't drift.
fn onboarding_human_steps() -> Vec<Value> {
    vec![
        serde_json::json!({ "when": "Billing / subscription", "tell_the_user": "Open your AMOS billing page and choose a plan (Starter/Pro/Compliance) — provisioning + managed-AI credits activate on checkout." }),
        serde_json::json!({ "when": "Grant this AI scoped access", "tell_the_user": "In AMOS settings → API keys, create a key scoped to exactly what this AI should touch (e.g. finance:read,finance:write). A key can never exceed your role." }),
        serde_json::json!({ "when": "Connect QuickBooks / Stripe for finance", "tell_the_user": "Authorize the provider; the credential is stored per-tenant via set_billing_key (finance:connect, owner-only)." }),
        serde_json::json!({ "when": "Custom domain", "tell_the_user": "Point your domain's DNS at the AMOS ingress target (the team will provide it); TLS is issued automatically." }),
    ]
}

/// Always double-quote + escape a scalar so values containing `:` (e.g. a
/// release command) or spaces are valid YAML.
fn yaml_scalar(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Enable-guidance for a requested engine (data for `onboard_app`).
fn engine_enable_step(engine: &str) -> Value {
    match engine {
        "finance" => serde_json::json!({
            "engine": "finance",
            "available": true,
            "scopes": ["finance:read", "finance:write", "finance:connect"],
            "enable": "Owner grants a key the finance:* scopes (see human_steps). Then: finance_board (view budget vs actual vs live), set_finance_budget / set_finance_actual (edit, proof-carrying), qbo_accounts + set_finance_mapping + set_billing_key to wire QuickBooks/Stripe.",
        }),
        other => serde_json::json!({
            "engine": other,
            "available": false,
            "note": format!("No '{other}' engine yet — available engines: finance. (marketing / CRM are on the roadmap.)"),
        }),
    }
}

/// `onboard_app` — advisory onboarding helper (no scope gate, no state change).
/// Given an app description it returns a GUARANTEED-VALID amos.yaml (round-tripped
/// through `AmosManifest::parse` + `build_order`) plus the exact deploy playbook
/// and the human checklist — so a new customer's AI is ready to deploy in one
/// call. It never writes the DB: the app record + infra are created by the
/// `deploy`/`deploy_app`/`provision_env` verbs the plan points to.
async fn tool_onboard_app(
    _state: &PlatformState,
    _claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    build_onboarding_plan(&args)
}

/// Pure core of `onboard_app`: validate inputs → emit a valid amos.yaml → plan.
fn build_onboarding_plan(args: &Value) -> Result<Value, ToolError> {
    use crate::provisioning::manifest::AmosManifest;

    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::InvalidParams(
                "onboard_app: 'name' (string) is required — your app's name".into(),
            )
        })?;
    let services = args
        .get("services")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            ToolError::InvalidParams(
                "onboard_app: 'services' is required — a non-empty array of { name, dockerfile?, public?, base?, image_name? }".into(),
            )
        })?;
    let release = args
        .get("release")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let git_repo = args
        .get("git_repo")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("<your https git repo url>");
    let engines: Vec<String> = args
        .get("engines")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Build the amos.yaml from the inputs.
    let mut yaml = String::new();
    yaml.push_str(&format!("name: {}\n", yaml_scalar(name)));
    if let Some(r) = release {
        yaml.push_str(&format!("release: {}\n", yaml_scalar(r)));
    }
    yaml.push_str("services:\n");
    for (i, svc) in services.iter().enumerate() {
        let sname = svc
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidParams(format!("onboard_app: services[{i}].name is required"))
            })?;
        let dockerfile = svc
            .get("dockerfile")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Dockerfile");
        yaml.push_str(&format!("  - name: {}\n", yaml_scalar(sname)));
        yaml.push_str(&format!("    dockerfile: {}\n", yaml_scalar(dockerfile)));
        if let Some(b) = svc
            .get("base")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            yaml.push_str(&format!("    base: {}\n", yaml_scalar(b)));
        }
        if let Some(im) = svc
            .get("image_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            yaml.push_str(&format!("    image_name: {}\n", yaml_scalar(im)));
        }
        if svc.get("public").and_then(|v| v.as_bool()).unwrap_or(false) {
            yaml.push_str("    public: true\n");
        }
        if svc
            .get("build_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            yaml.push_str("    build_only: true\n");
        }
    }

    // Validate by parsing through the real schema + build graph — so what we hand
    // back is guaranteed to deploy.
    let manifest = AmosManifest::parse(&yaml)
        .map_err(|e| ToolError::InvalidParams(format!("generated amos.yaml is invalid: {e}")))?;
    manifest.build_order().map_err(|e| {
        ToolError::InvalidParams(format!(
            "generated amos.yaml has an unresolvable service graph: {e}"
        ))
    })?;

    let engines_plan: Vec<Value> = engines.iter().map(|e| engine_enable_step(e)).collect();

    let mut human = onboarding_human_steps();
    human.insert(
        0,
        serde_json::json!({
            "when": "First-time hosting setup (net-new app)",
            "tell_the_user": "A brand-new app needs its initial ECS services + ingress/domain provisioned once. Ask the AMOS team to provision the substrate (or use provision_env for a preview). After that, deploys are fully self-serve via the deploy verb.",
        }),
    );

    Ok(serde_json::json!({
        "app": name,
        "amos_yaml": yaml,
        "next_steps": [
            "1. Commit the amos.yaml above to your repo root.",
            format!("2. Ship it: deploy(git_repo='{git_repo}', git_ref='<branch>', manifest=<the amos.yaml above>). It builds from git, runs the gated `release` step (if set), and rolls digest-pinned."),
            "3. Poll deploy_status(deploy_id) until succeeded — the receipt shows per-phase timings (build vs release vs roll).",
            "Note: do NOT use app_redeploy to ship new code — it re-pulls the pinned digest and changes nothing.",
        ],
        "engines": engines_plan,
        "human_steps": human,
        "note": "Advisory: a VALIDATED amos.yaml + plan, nothing was created. The deploy/provision verbs create the app record + infra. Call get_started for the full capability map.",
    }))
}

fn arg_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidParams(format!("missing required argument '{key}'")))
}

fn arg_uuid(args: &Value, key: &str) -> Result<Uuid, ToolError> {
    let s = arg_str(args, key)?;
    Uuid::parse_str(&s)
        .map_err(|_| ToolError::InvalidParams(format!("'{key}' is not a valid UUID")))
}

fn parse_instance_size(args: &Value) -> InstanceSize {
    match args.get("instance_size").and_then(|v| v.as_str()) {
        Some("medium") => InstanceSize::Medium,
        Some("large") => InstanceSize::Large,
        _ => InstanceSize::Small,
    }
}

// ── Tool implementations ──────────────────────────────────────────────────

async fn tool_list_harnesses(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;

    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            Option<String>,
            Option<String>,
            String,
            Option<bool>,
            String,
            String,
        ),
    >(
        "SELECT id, name, container_id, status, healthy, instance_size, environment
         FROM harness_instances
         WHERE tenant_id = $1 AND status != 'deprovisioned'
         ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;

    let harnesses: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, container_id, status, healthy, size, env)| {
            json!({
                "harness_id": id,
                "name": name,
                "container_id": container_id,
                "status": status,
                "healthy": healthy.unwrap_or(false),
                "instance_size": size,
                "environment": env,
            })
        })
        .collect();

    Ok(json!({ "tenant_id": tenant_id, "count": harnesses.len(), "harnesses": harnesses }))
}

async fn tool_provision_harness(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let instance_size = parse_instance_size(&args);
    let env_vars: HashMap<String, String> = args
        .get("env_vars")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let size_str = match instance_size {
        InstanceSize::Small => "small",
        InstanceSize::Medium => "medium",
        InstanceSize::Large => "large",
    };

    // A tenant has at most one primary harness (idx_one_primary_per_tenant), and
    // registration pre-creates a 'pending' primary row. So we provision a
    // *container* for the existing primary rather than inserting a new row.
    // Targeting: an explicit harness_id, else the tenant's existing primary,
    // else insert a fresh primary (brand-new tenant with no row).
    let explicit_id = match args.get("harness_id").and_then(|v| v.as_str()) {
        Some(s) => Some(
            Uuid::parse_str(s)
                .map_err(|_| ToolError::InvalidParams("'harness_id' is not a valid UUID".into()))?,
        ),
        None => None,
    };

    let existing = sqlx::query_as::<_, (Uuid, Option<String>, Option<String>, Value, String)>(
        "SELECT id, name, container_id, packages, status
         FROM harness_instances
         WHERE tenant_id = $1 AND harness_role = 'primary' AND status != 'deprovisioned'
           AND ($2::uuid IS NULL OR id = $2)
         ORDER BY created_at
         LIMIT 1",
    )
    .bind(tenant_id)
    .bind(explicit_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;

    let environment = args
        .get("environment")
        .and_then(|v| v.as_str())
        .unwrap_or("production")
        .to_string();

    // Resolve the target row (reuse or create), and the effective name/packages.
    let (harness_id, name, packages) = if let Some((id, row_name, container_id, row_pkgs, status)) =
        existing
    {
        if container_id.is_some() && status == "running" {
            return Ok(json!({
                "harness_id": id,
                "status": "running",
                "note": "primary harness already provisioned and running; use harness_control to restart",
                "container_id": container_id,
            }));
        }
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or(row_name)
            .unwrap_or_else(|| "primary".to_string());
        let packages: Vec<String> = args
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| {
                row_pkgs
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            });
        // Move the existing row into 'provisioning' with the requested sizing.
        sqlx::query(
            "UPDATE harness_instances
             SET status = 'provisioning', instance_size = $2, environment = $3,
                 name = $4, packages = $5, updated_at = NOW()
             WHERE id = $1",
        )
        .bind(id)
        .bind(size_str)
        .bind(&environment)
        .bind(&name)
        .bind(serde_json::to_value(&packages).unwrap_or_else(|_| json!([])))
        .execute(&state.db)
        .await
        .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;
        (id, name, packages)
    } else if explicit_id.is_some() {
        return Err(ToolError::Execution(
            "harness_id not found for this tenant".into(),
        ));
    } else {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("primary")
            .to_string();
        let packages: Vec<String> = args
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let harness_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO harness_instances
                (id, tenant_id, name, harness_role, packages, status, instance_size, environment)
             VALUES ($1, $2, $3, 'primary', $4, 'provisioning', $5, $6)",
        )
        .bind(harness_id)
        .bind(tenant_id)
        .bind(&name)
        .bind(serde_json::to_value(&packages).unwrap_or_else(|_| json!([])))
        .bind(size_str)
        .bind(&environment)
        .execute(&state.db)
        .await
        .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;
        (harness_id, name, packages)
    };

    let platform_url = format!(
        "http://{}:{}",
        state.config.server.host, state.config.server.port
    );
    let config = HarnessConfig {
        customer_id: tenant_id,
        region: "us-west-2".into(),
        instance_size,
        environment: environment.clone(),
        platform_grpc_url: platform_url,
        env_vars,
        harness_role: "primary".to_string(),
        packages: packages.clone(),
        harness_id: Some(harness_id),
    };

    let (container_id, provider, status) = if let Some(manager) = state.harness_manager.as_ref() {
        let cid = manager
            .provision(&config)
            .await
            .map_err(|e| ToolError::Execution(format!("docker provisioning failed: {e}")))?;
        let _ = manager.start(&cid).await;
        (cid, "docker", "running")
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        let slug = format!("{}-{}", tenant_id, name.to_lowercase().replace(' ', "-"));
        let arn = ecs
            .provision(&config, &slug)
            .await
            .map_err(|e| ToolError::Execution(format!("ECS provisioning failed: {e}")))?;
        (arn, "ecs", "provisioning")
    } else {
        // Roll back the DB row so we don't leave an orphan in 'provisioning'.
        let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
            .bind(harness_id)
            .execute(&state.db)
            .await;
        return Err(ToolError::Execution(
            "no provisioner available (neither Docker nor ECS Fargate is configured)".into(),
        ));
    };

    let _ = sqlx::query(
        "UPDATE harness_instances SET container_id = $1, status = $2, provisioned_at = NOW() WHERE id = $3",
    )
    .bind(&container_id)
    .bind(status)
    .bind(harness_id)
    .execute(&state.db)
    .await;

    Ok(json!({
        "harness_id": harness_id,
        "name": name,
        "status": status,
        "provider": provider,
        "container_id": container_id,
        "environment": environment,
        "packages": packages,
    }))
}

async fn tool_harness_status(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let harness_id = arg_uuid(&args, "harness_id")?;
    let (container_id, db_status) = resolve_harness(state, tenant_id, harness_id).await?;

    let Some(container_id) = container_id else {
        return Ok(json!({
            "harness_id": harness_id,
            "db_status": db_status,
            "note": "harness has no container yet",
        }));
    };

    let live = if let Some(manager) = state.harness_manager.as_ref() {
        match manager.get_status(&container_id).await {
            Ok(s) => json!({ "provider": "docker", "live_status": s }),
            Err(e) => json!({ "provider": "docker", "error": e.to_string() }),
        }
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        match ecs.describe_task(&container_id).await {
            Ok((s, ip)) => json!({ "provider": "ecs", "live_status": s, "private_ip": ip }),
            Err(e) => json!({ "provider": "ecs", "error": e.to_string() }),
        }
    } else {
        json!({ "provider": "none" })
    };

    Ok(json!({
        "harness_id": harness_id,
        "container_id": container_id,
        "db_status": db_status,
        "runtime": live,
    }))
}

async fn tool_harness_logs(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let harness_id = arg_uuid(&args, "harness_id")?;
    let (container_id, _) = resolve_harness(state, tenant_id, harness_id).await?;
    let container_id = container_id
        .ok_or_else(|| ToolError::Execution("harness has no container to read logs from".into()))?;

    let logs = if let Some(manager) = state.harness_manager.as_ref() {
        manager
            .get_logs(&container_id)
            .await
            .map_err(|e| ToolError::Execution(format!("failed to read logs: {e}")))?
    } else if let Some(ecs) = state.ecs_provisioner.as_ref() {
        vec![ecs.logs_hint(&container_id)]
    } else {
        return Err(ToolError::Execution("no provisioner available".into()));
    };

    Ok(json!({ "harness_id": harness_id, "container_id": container_id, "logs": logs }))
}

async fn tool_harness_control(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let harness_id = arg_uuid(&args, "harness_id")?;
    let action = arg_str(&args, "action")?;
    let (container_id, _) = resolve_harness(state, tenant_id, harness_id).await?;
    let container_id = container_id
        .ok_or_else(|| ToolError::Execution("harness has no container to control".into()))?;

    let manager = state.harness_manager.as_ref();
    let ecs = state.ecs_provisioner.as_ref();

    let new_status = match action.as_str() {
        "start" => {
            let m = manager.ok_or_else(|| {
                ToolError::Execution("start is only supported for Docker harnesses".into())
            })?;
            m.start(&container_id).await?;
            "running"
        }
        "stop" => {
            if let Some(m) = manager {
                m.stop(&container_id).await?;
            } else if let Some(e) = ecs {
                e.stop(&container_id).await?;
            } else {
                return Err(ToolError::Execution("no provisioner available".into()));
            }
            "stopped"
        }
        "restart" => {
            let m = manager.ok_or_else(|| {
                ToolError::Execution("restart is only supported for Docker harnesses".into())
            })?;
            let _ = m.stop(&container_id).await;
            m.start(&container_id).await?;
            "running"
        }
        "deprovision" => {
            if let Some(m) = manager {
                m.deprovision(&container_id).await?;
            } else if let Some(e) = ecs {
                e.stop(&container_id).await?;
            } else {
                return Err(ToolError::Execution("no provisioner available".into()));
            }
            "deprovisioned"
        }
        other => {
            return Err(ToolError::InvalidParams(format!(
                "unknown action '{other}' (expected start|stop|restart|deprovision)"
            )))
        }
    };

    let _ =
        sqlx::query("UPDATE harness_instances SET status = $1, updated_at = NOW() WHERE id = $2")
            .bind(new_status)
            .bind(harness_id)
            .execute(&state.db)
            .await;

    Ok(json!({ "harness_id": harness_id, "action": action, "status": new_status }))
}

async fn tool_list_releases(state: &PlatformState) -> Result<Value, ToolError> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, String, Option<String>, String)>(
        "SELECT id, version, commit_sha, status, release_notes, created_at::text
         FROM releases
         WHERE status = 'available'
         ORDER BY created_at DESC
         LIMIT 20",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;

    let releases: Vec<Value> = rows
        .into_iter()
        .map(|(id, version, commit_sha, status, notes, created_at)| {
            json!({
                "id": id,
                "version": version,
                "commit_sha": commit_sha,
                "status": status,
                "release_notes": notes,
                "created_at": created_at,
            })
        })
        .collect();

    Ok(json!({ "count": releases.len(), "releases": releases }))
}

async fn tool_get_harness_config(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let row = sqlx::query_as::<_, (bool, Value)>(
        "SELECT enabled, feature_flags FROM harness_configs WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;

    match row {
        Some((enabled, feature_flags)) => Ok(json!({
            "tenant_id": tenant_id,
            "enabled": enabled,
            "feature_flags": feature_flags,
        })),
        None => Ok(json!({
            "tenant_id": tenant_id,
            "enabled": true,
            "feature_flags": {},
            "note": "no config row yet; defaults shown",
        })),
    }
}

async fn tool_set_harness_config(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let enabled = args.get("enabled").and_then(|v| v.as_bool());
    let feature_flags = args.get("feature_flags").cloned();

    if enabled.is_none() && feature_flags.is_none() {
        return Err(ToolError::InvalidParams(
            "provide at least one of 'enabled' or 'feature_flags'".into(),
        ));
    }
    if let Some(ff) = &feature_flags {
        if !ff.is_object() {
            return Err(ToolError::InvalidParams(
                "'feature_flags' must be a JSON object".into(),
            ));
        }
    }

    // Upsert, updating only the fields supplied.
    sqlx::query(
        "INSERT INTO harness_configs (tenant_id, enabled, feature_flags)
         VALUES ($1, COALESCE($2, TRUE), COALESCE($3, '{}'::jsonb))
         ON CONFLICT (tenant_id) DO UPDATE SET
            enabled = COALESCE($2, harness_configs.enabled),
            feature_flags = COALESCE($3, harness_configs.feature_flags),
            updated_at = NOW()",
    )
    .bind(tenant_id)
    .bind(enabled)
    .bind(&feature_flags)
    .execute(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;

    let (enabled, feature_flags) = sqlx::query_as::<_, (bool, Value)>(
        "SELECT enabled, feature_flags FROM harness_configs WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| ToolError::Execution(format!("database error: {e}")))?;

    Ok(json!({
        "tenant_id": tenant_id,
        "enabled": enabled,
        "feature_flags": feature_flags,
        "message": "config updated; restart the harness for changes to take effect",
    }))
}

// ── Tool schemas (advertised via tools/list) ────────────────────────────

fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_harnesses",
            "description": "List the AMOS environments (harness instances) for your tenant, with status and health. Call this first to discover what exists before operating on an env.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only; defaults to your own tenant)" }
                }
            }
        },
        {
            "name": "provision_harness",
            "description": "Provision/start the container for a governed AMOS environment. By default this provisions the tenant's existing primary harness (created at registration); pass harness_id to target a specific one. Returns the harness_id used by the other tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "harness_id": { "type": "string", "description": "Existing harness UUID to provision (default: the tenant's primary harness)" },
                    "name": { "type": "string", "description": "Human-readable name (default 'primary')" },
                    "environment": { "type": "string", "enum": ["production", "staging", "development"], "description": "Deployment environment (default 'production')" },
                    "instance_size": { "type": "string", "enum": ["small", "medium", "large"], "description": "Compute size (default 'small')" },
                    "packages": { "type": "array", "items": { "type": "string" }, "description": "Packages to enable on the env" },
                    "env_vars": { "type": "object", "description": "Extra environment variables (key->string)" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "harness_status",
            "description": "Get the live runtime status and health of a provisioned environment by harness_id, including the underlying container/task state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "harness_id": { "type": "string", "description": "Harness UUID from list_harnesses/provision_harness" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["harness_id"]
            }
        },
        {
            "name": "harness_logs",
            "description": "Fetch recent logs for an environment by harness_id (Docker: last 100 lines; ECS: a CloudWatch pointer).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "harness_id": { "type": "string" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["harness_id"]
            }
        },
        {
            "name": "harness_control",
            "description": "Operate an environment lifecycle: start, stop, restart, or deprovision it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "harness_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["start", "stop", "restart", "deprovision"] },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["harness_id", "action"]
            }
        },
        {
            "name": "list_releases",
            "description": "List available platform releases (harness/agent images registered by CI). Use this to see what versions can be deployed.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_harness_config",
            "description": "Read the environment configuration (enabled flag + feature flags) for your tenant.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "set_harness_config",
            "description": "Update the environment configuration for your tenant. Provide 'enabled' and/or 'feature_flags' (a JSON object). Restart the harness for changes to take effect.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" },
                    "feature_flags": { "type": "object", "description": "Feature flags object, e.g. {\"sovereign_ai\": true}" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "deploy_app",
            "description": "Deploy a multi-service application (e.g. a ported app like Cuspr) onto AMOS. Provide either 'compose_yaml' (a docker-compose file's contents — the easiest way to port an existing app) or a structured 'spec'. With provider='docker' (default) it runs locally on a per-app network; with provider='aws' it renders to AWS Fargate (one ECS task of co-located containers behind the ALB), skipping managed services (postgres/redis) and injecting their connection strings from Secrets Manager via the 'aws' target. Returns a deployment_id used by the other app_* tools; a proof receipt is emitted on completion.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "enum": ["docker", "aws"], "description": "Where to deploy (default 'docker')." },
                    "compose_yaml": { "type": "string", "description": "Contents of a docker-compose.yml to deploy. Profiles are skipped; build services require pre-built images (use build_image first)." },
                    "compose_dir": { "type": "string", "description": "Directory the compose file came from (resolves relative bind-mount paths). Defaults to the platform working dir." },
                    "spec": { "type": "object", "description": "A structured AppSpec (alternative to compose_yaml)." },
                    "aws": {
                        "type": "object",
                        "description": "AWS deploy target (provider='aws'): per-tenant infra ARNs from terraform/aws-app.",
                        "properties": {
                            "secret_arn": { "type": "string", "description": "Secrets Manager ARN backing each service's secret env keys." },
                            "target_group_arn": { "type": "string", "description": "ALB target group ARN for the public service's ingress." },
                            "public_service": { "type": "string", "description": "Which service is public (defaults to the lone expose_public service)." },
                            "public_url": { "type": "string", "description": "Public URL, for the receipt/output." },
                            "cpu": { "type": "string", "description": "Task-level Fargate CPU units (default '4096')." },
                            "memory": { "type": "string", "description": "Task-level Fargate memory MiB (default '16384')." },
                            "ephemeral_storage_gib": { "type": "integer", "description": "Ephemeral task storage GiB (default 40)." },
                            "task_role_arn": { "type": "string", "description": "Optional task-role override (e.g. one granting tenant S3 access)." }
                        }
                    },
                    "callback_url": { "type": "string", "description": "Optional URL the platform POSTs to when the deploy reaches a terminal state (ping-back), so you don't have to poll app_status." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "list_apps",
            "description": "List the multi-service app deployments for your tenant, with per-service status. Call this to discover deployment_ids before operating on an app.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "app_status",
            "description": "Live status of a deployed app by deployment_id, including each service's container state and published host ports.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id"]
            }
        },
        {
            "name": "app_logs",
            "description": "Fetch recent logs for one service of a deployed app (e.g. service='api' to see Alembic migrations + uvicorn startup).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "service": { "type": "string", "description": "Service name from the app spec (e.g. api, worker, frontend)" },
                    "tail": { "type": "integer", "description": "Number of log lines (default 200)" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id", "service"]
            }
        },
        {
            "name": "app_control",
            "description": "Operate a deployed app: start, stop, restart, or deprovision it. Optionally target a single service. Deprovision keeps data unless destroy_data is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["start", "stop", "restart", "deprovision"] },
                    "service": { "type": "string", "description": "Optional: target only this service (omit for the whole app)" },
                    "destroy_data": { "type": "boolean", "description": "On deprovision, also delete named volumes (default false)" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id", "action"]
            }
        },
        {
            "name": "build_image",
            "description": "Build a container image in the cloud (AWS CodeBuild) from a build context and push it to ECR — no local Docker required. Returns a build_id immediately; the build runs asynchronously. Poll build_status to watch it, and a proof receipt (with the pushed image digest) is emitted on completion. The resulting image_ref can be used in a deploy_app spec.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "context_dir": { "type": "string", "description": "Absolute path to the build context directory (on the platform host). Use this OR git_repo. For CI/remote callers, use git_repo instead." },
                    "git_repo": { "type": "string", "description": "Git source URL (e.g. 'https://github.com/org/repo.git'). CodeBuild clones it directly — no local context. Use this from CI. Private repos require GitHub source credentials in CodeBuild." },
                    "git_ref": { "type": "string", "description": "Git ref to build with git_repo: a commit SHA (recommended for CI), branch, or tag." },
                    "image_name": { "type": "string", "description": "ECR repository name to push to, e.g. 'cuspr-api' (created if absent)." },
                    "dockerfile": { "type": "string", "description": "Dockerfile path relative to the context/repo root (default 'Dockerfile')." },
                    "tag": { "type": "string", "description": "Image tag (default 'latest')." },
                    "build_args": { "type": "object", "description": "Docker --build-arg values, e.g. {\"BASE_IMAGE\":\"…/cuspr-base:latest\"}. Values must not contain spaces." },
                    "callback_url": { "type": "string", "description": "Optional URL the platform POSTs to when the build reaches a terminal state (ping-back), so you don't have to poll build_status." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["image_name"]
            }
        },
        {
            "name": "build_status",
            "description": "Check the status of a cloud image build started with build_image. Returns CodeBuild status/phase and, on success, the pushed image digest and full image_ref. On failure, returns a CloudWatch logs link for diagnosis.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "build_id": { "type": "string" },
                    "image_name": { "type": "string" },
                    "tag": { "type": "string", "description": "default 'latest'" },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["build_id", "image_name"]
            }
        },
        {
            "name": "db_query",
            "description": "Run a READ-ONLY SQL query against an app's database (for inspecting data, e.g. 'does this query return rows?'). Requires the db:read scope. The query runs inside a read-only transaction with a row cap and statement timeout, and every call emits a proof receipt. Only SELECT-style queries work (it is wrapped as a subquery); writes/DDL are rejected. Use list_apps to get the deployment_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string", "description": "The app deployment to query (from list_apps)." },
                    "sql": { "type": "string", "description": "A read-only SELECT query, e.g. 'SELECT count(*) FROM users'." },
                    "max_rows": { "type": "integer", "description": "Row cap (default 1000, max 10000)." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id", "sql"]
            }
        },
        {
            "name": "db_write",
            "description": "Run a single-statement WRITE (INSERT/UPDATE/DELETE) against an app's database. Requires the db:write scope (owner-only, granted per key by a human). Guardrails: single statement only; DDL (DROP/ALTER/CREATE/TRUNCATE) and SELECT are rejected (schema changes go through the app's migrations/deploy); UPDATE/DELETE must have a WHERE unless allow_unfiltered=true; the affected-row count is capped by max_rows or the whole write is rolled back. Pass values as bound params ($1,$2,…) in 'params', not string-interpolated. Use dry_run=true first to preview the affected count + rows (it rolls back). Every call emits a proof receipt. Use list_apps to get the deployment_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string", "description": "The app deployment to write to (from list_apps)." },
                    "sql": { "type": "string", "description": "One INSERT/UPDATE/DELETE statement, e.g. 'UPDATE users SET active = $1 WHERE id = $2'. Use $1,$2,… for values." },
                    "params": { "type": "array", "description": "Values bound to $1,$2,… (typed by JSON: string/number/bool/null/json). Default []." },
                    "max_rows": { "type": "integer", "description": "Max rows the write may affect before it is rolled back (default 100, max 10000)." },
                    "dry_run": { "type": "boolean", "description": "If true, run + measure + ROLL BACK (preview only). Default false." },
                    "allow_unfiltered": { "type": "boolean", "description": "Allow an UPDATE/DELETE with no WHERE clause. Default false." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id", "sql"]
            }
        },
        {
            "name": "describe_table",
            "description": "Inspect an app database's schema (read-only, db:read). With no 'table', returns the list of public tables. With a 'table', returns its columns (name, data_type, is_nullable, max length). Use this before writing db_query/db_write so you don't guess column names. Use list_apps to get the deployment_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string", "description": "The app deployment (from list_apps)." },
                    "table": { "type": "string", "description": "A table name to describe. Omit to list all tables." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id"]
            }
        },
        {
            "name": "finance_board",
            "description": "Read the tenant's finance board — budget vs actuals by category for a period (scope finance:read). The reference finance engine; an app enables it, never rebuilds it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "period": { "type": "string", "description": "Period to view, e.g. '2026-06' or 'Q2-2026'. Omit for current." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "finance_history",
            "description": "Read historical finance lines/totals over time (scope finance:read).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Start period (e.g. '2026-01')." },
                    "to": { "type": "string", "description": "End period (e.g. '2026-06')." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "finance_truth",
            "description": "Read the reconciled finance 'truth' (actuals reconciled against the source of record / QBO) (scope finance:read).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "period": { "type": "string", "description": "Period to reconcile. Omit for current." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "qbo_accounts",
            "description": "List the connected QuickBooks Online chart of accounts available for mapping (scope finance:read).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "revenue_summary",
            "description": "Read a revenue summary (MRR/ARR/new/expansion) for the tenant (scope finance:read).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "period": { "type": "string", "description": "Period to summarize. Omit for current." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "org_subscriptions",
            "description": "List the tenant's active subscriptions / plans driving recurring revenue (scope finance:read).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "churn_snapshot",
            "description": "Read a churn snapshot (rate + count for a period) (scope finance:read).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "period": { "type": "string", "description": "Period for the snapshot. Omit for current." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "create_finance_line",
            "description": "Create a finance line (a budget/actual row for a category) (scope finance:write; emits a proof receipt).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "category": { "type": "string", "description": "Finance category / line name." },
                    "period": { "type": "string", "description": "Period the line belongs to, e.g. '2026-06'." },
                    "amount": { "type": "number", "description": "Amount for the line (currency units)." },
                    "kind": { "type": "string", "description": "'budget' or 'actual'." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["category", "period"]
            }
        },
        {
            "name": "update_finance_line",
            "description": "Update an existing finance line by id (scope finance:write; emits a proof receipt).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Finance line id to update." },
                    "amount": { "type": "number", "description": "New amount." },
                    "category": { "type": "string", "description": "New category/name (optional)." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "set_finance_actual",
            "description": "Set the actual value for a category/period (scope finance:write; emits a proof receipt).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "category": { "type": "string" },
                    "period": { "type": "string", "description": "Period, e.g. '2026-06'." },
                    "amount": { "type": "number", "description": "Actual amount." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["category", "period", "amount"]
            }
        },
        {
            "name": "set_finance_budget",
            "description": "Set the budget value for a category/period (scope finance:write; emits a proof receipt).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "category": { "type": "string" },
                    "period": { "type": "string", "description": "Period, e.g. '2026-06'." },
                    "amount": { "type": "number", "description": "Budgeted amount." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["category", "period", "amount"]
            }
        },
        {
            "name": "set_finance_mapping",
            "description": "Map a QBO account (or source category) to a finance board category (scope finance:write; emits a proof receipt).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "qbo_account": { "type": "string", "description": "QBO account id/name (from qbo_accounts)." },
                    "category": { "type": "string", "description": "Finance board category to map it to." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["qbo_account", "category"]
            }
        },
        {
            "name": "set_billing_key",
            "description": "Connect a finance integration / set a billing API credential (scope finance:connect, owner-only; emits a proof receipt with the secret REDACTED). The most sensitive finance verb.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "Integration provider, e.g. 'stripe' or 'quickbooks'." },
                    "key": { "type": "string", "description": "The API key / credential. Never logged; redacted in the proof receipt." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["provider", "key"]
            }
        },
        {
            "name": "whoami",
            "description": "Return your identity (sub, tenant, role) and your EFFECTIVE scopes — what you're allowed to do — so you know your capabilities up front instead of discovering a missing scope at call time. No arguments, no scope required.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_started",
            "description": "START HERE. The capability manifest for operating this tenant: every verb grouped by category (each with its required scope and whether YOU're allowed it), the golden paths (ship code, preview env, operate finances, query data, audit), and the human-only steps to relay to the owner. No arguments, no scope required.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "onboard_app",
            "description": "Onboard a NEW app in one call: returns a guaranteed-valid amos.yaml (round-tripped through the real schema) built from your inputs, the exact deploy playbook, requested-engine enable steps, and the human-only checklist. Advisory — generates the manifest + plan, creates nothing (the deploy/provision verbs do that). No scope required.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The app's name (= amos.yaml name)." },
                    "services": {
                        "type": "array",
                        "description": "One per buildable image: { name, dockerfile? (default Dockerfile), public? (internet-facing), base? (another service to FROM), image_name?, build_only? }.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "dockerfile": { "type": "string" },
                                "public": { "type": "boolean" },
                                "base": { "type": "string" },
                                "image_name": { "type": "string" },
                                "build_only": { "type": "boolean" }
                            },
                            "required": ["name"]
                        }
                    },
                    "release": { "type": "string", "description": "Optional gated pre-rollout command, e.g. 'bundle exec rake db:migrate' or 'alembic upgrade head'." },
                    "git_repo": { "type": "string", "description": "Optional https git URL (used to fill the deploy call in the plan)." },
                    "engines": { "type": "array", "items": { "type": "string" }, "description": "Optional engines to enable, e.g. [\"finance\"]." }
                },
                "required": ["name", "services"]
            }
        },
        {
            "name": "s3_list",
            "description": "List objects in the app's S3 bucket (scope storage:read). Confined to the deployment's bucket (and prefix, if configured); keys are shown app-relative. Requires aws_meta.s3_bucket on the deployment. Use list_apps for the deployment_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "prefix": { "type": "string", "description": "Optional key prefix to filter by (app-relative)." },
                    "max_keys": { "type": "integer", "description": "Max keys to return (default 1000, max 1000)." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id"]
            }
        },
        {
            "name": "s3_get",
            "description": "Read an object from the app's bucket (scope storage:read). Inline returns content (utf8, or base64 for binary) for objects ≤5 MiB. For larger/binary objects or browser download, pass presign:true to get a temporary presigned GET URL instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "key": { "type": "string", "description": "Object key (app-relative)." },
                    "presign": { "type": "boolean", "description": "If true, return a presigned GET URL instead of inline content." },
                    "expires_in": { "type": "integer", "description": "Presigned URL TTL in seconds (default 900, max 604800)." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id", "key"]
            }
        },
        {
            "name": "s3_put",
            "description": "Write an object to the app's bucket (scope storage:write; emits a proof receipt). Inline body up to 5 MiB (set base64:true for binary). For large/binary uploads or browser-direct upload, pass presign:true to get a temporary presigned PUT URL instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "key": { "type": "string", "description": "Object key (app-relative)." },
                    "body": { "type": "string", "description": "Object contents (utf8, or base64 if base64=true). Omit when presign=true." },
                    "base64": { "type": "boolean", "description": "Treat body as base64-encoded bytes." },
                    "content_type": { "type": "string", "description": "Content-Type to store (e.g. image/png)." },
                    "presign": { "type": "boolean", "description": "If true, return a presigned PUT URL instead of writing inline." },
                    "expires_in": { "type": "integer", "description": "Presigned URL TTL in seconds (default 900, max 604800)." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id", "key"]
            }
        },
        {
            "name": "s3_delete",
            "description": "Delete an object from the app's bucket (scope storage:write; emits a proof receipt). Prefix-confined to the deployment's bucket.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "key": { "type": "string", "description": "Object key (app-relative)." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id", "key"]
            }
        },
        {
            "name": "deploy",
            "description": "Deploy an app from its amos.yaml manifest — the standard one-call CD step. Builds every service in the manifest from source (git_ref), in dependency order (base layers first, injecting BASE_IMAGE), then rolls the app's services. The app is resolved by manifest 'name' + tenant (no deployment_id needed). Returns immediately; the build+roll runs in the background and emits a proof receipt. This is the single verb CI calls.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "git_repo": { "type": "string", "description": "Git URL of the app repo (e.g. https://github.com/org/app.git)." },
                    "git_ref": { "type": "string", "description": "Commit SHA / branch / tag to build (usually $GITHUB_SHA)." },
                    "manifest": { "type": "string", "description": "Contents of the repo's amos.yaml." },
                    "env": { "type": "string", "description": "Optional: target an isolated environment (from provision_env). Rolls the deployment named '{manifest.name}-{env}' instead of the base app." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["git_repo", "git_ref", "manifest"]
            }
        },
        {
            "name": "provision_env",
            "description": "Stand up an isolated environment (staging/preview) for an existing app — its own database, secret, ECS service, and {app}-{env}.custom.amoslabs.com ingress — by cloning the app's live task definition. Scope env:provision. Data isolation is automatic by plan: standard tenants get a fresh database on the shared envs RDS; compliance (PHI) tenants must pass db_secret_arn for a dedicated RDS. Then roll code into it with deploy(env=...). Optional ttl_hours marks an ephemeral preview for later teardown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app_name": { "type": "string", "description": "The base app to clone (its deployment name)." },
                    "env_name": { "type": "string", "description": "Environment name (lowercase letters/digits/hyphen), e.g. 'staging' or 'pr-42'." },
                    "db_secret_arn": { "type": "string", "description": "Compliance tier only: Secrets Manager ARN of the dedicated RDS secret (must hold DATABASE_URL + the app's other keys)." },
                    "ttl_hours": { "type": "integer", "description": "Optional: mark as ephemeral; records an expiry for later teardown." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["app_name", "env_name"]
            }
        },
        {
            "name": "teardown_env",
            "description": "Tear down an environment created by provision_env: deletes the ECS service, ALB rule + target group, and (for shared-tier envs) the env database and secret. Dedicated (PHI) RDS instances are left untouched. Scope env:provision.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app_name": { "type": "string", "description": "The base app name." },
                    "env_name": { "type": "string", "description": "The environment name to tear down." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["app_name", "env_name"]
            }
        },
        {
            "name": "deploy_status",
            "description": "Poll the outcome of a deploy by the deploy_id returned from `deploy`. Returns status: 'deploying' (still running), 'succeeded', or 'failed' (with the receipt summary). Use this in CI to block on the real deploy result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deploy_id": { "type": "string", "description": "The deploy_id returned by the deploy verb." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deploy_id"]
            }
        },
        {
            "name": "app_redeploy",
            "description": "Roll a deployed app to pick up a freshly-built image, ONLY when the task def references a MUTABLE tag (force-new-deployment re-pulls that tag). If the task def is digest-pinned (image is repo@sha256:…), force-new-deployment re-pulls the SAME frozen digest and ships NO new code — so this verb refuses with no_op:true and steers you to `deploy` (which builds + re-pins a fresh digest). To ship new code, prefer `deploy`. Use app_redeploy only to restart a mutable-tag service (or with force=true to restart the current pinned image deliberately). Poll app_status for the rollout.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string", "description": "The app deployment to roll (from list_apps)." },
                    "image": { "type": "string", "description": "Optional: the image ref/sha just built, recorded in the proof receipt for audit." },
                    "force": { "type": "boolean", "description": "Restart even if the task def is digest-pinned (intentional restart of the current image; bypasses the no-op guard). Default false." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["deployment_id"]
            }
        },
        {
            "name": "list_receipts",
            "description": "List proof-carrying operation receipts for your tenant (newest first). Every consequential operation (e.g. deploy_app, build_image) emits a receipt recording intent, the guardrails enforced, the verification checks and their outcomes, and whether it verified. Use this to audit what was done on the tenant's behalf and whether it actually succeeded.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "description": "Optional filter, e.g. 'deploy_app'." },
                    "limit": { "type": "integer", "description": "Max receipts to return (default 20, max 200)." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                }
            }
        },
        {
            "name": "ask_amos",
            "description": "Ask the AMOS manager about your environment in plain language. It investigates using the read-only control verbs (apps, deploy/build status, database, schema, storage, receipts, harness) and answers conversationally. Read-only: it will not change anything — if you ask for a change (deploy, scale, write data, provision/teardown an env, upload/delete files) it describes what it would do and says to run it explicitly. Examples: 'why can't this customer log in?', 'what changed last night?', 'is cuspr-staging healthy?'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Your question or request, in plain language." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["prompt"]
            }
        }
    ])
}

#[cfg(test)]
mod manifest_tests {
    use super::VERB_CATALOG;

    // The get_started manifest must not drift from the real RBAC gate: every
    // non-meta verb has a required_scope, and the two meta verbs have none.
    #[test]
    fn catalog_scopes_match_rbac() {
        for (verb, cat, _purpose) in VERB_CATALOG {
            let scope = crate::rbac::required_scope(verb);
            if *cat == "meta" {
                assert!(scope.is_none(), "meta verb {verb} should be unscoped");
            } else {
                assert!(
                    scope.is_some(),
                    "verb {verb} (cat {cat}) is in the manifest but has no required_scope — drift",
                );
            }
        }
    }

    #[test]
    fn catalog_has_no_duplicate_verbs() {
        let mut seen = std::collections::HashSet::new();
        for (verb, _, _) in VERB_CATALOG {
            assert!(seen.insert(*verb), "duplicate verb in catalog: {verb}");
        }
        // get_started lists itself + whoami so an agent discovers the entry points.
        assert!(seen.contains("get_started") && seen.contains("whoami"));
    }
}

#[cfg(test)]
mod s3_tests {
    use super::scoped_key;

    #[test]
    fn scoped_key_applies_prefix() {
        let p = Some("tenants/abc".to_string());
        assert_eq!(
            scoped_key(&p, "meshes/x.stl").ok(),
            Some("tenants/abc/meshes/x.stl".to_string())
        );
        // empty key under a prefix → the prefix itself (with trailing slash), for listing
        assert_eq!(scoped_key(&p, "").ok(), Some("tenants/abc/".to_string()));
    }

    #[test]
    fn scoped_key_no_prefix_passthrough() {
        assert_eq!(
            scoped_key(&None, "a/b.txt").ok(),
            Some("a/b.txt".to_string())
        );
    }

    #[test]
    fn scoped_key_rejects_traversal_and_absolute() {
        assert!(scoped_key(&None, "../etc/passwd").is_err());
        assert!(scoped_key(&Some("p".into()), "a/../../b").is_err());
        assert!(scoped_key(&None, "/abs").is_err());
    }
}

#[cfg(test)]
mod release_phase_tests {
    use super::{release_argv, release_gate, ReleaseDecision};

    #[test]
    fn gate_proceeds_only_on_exit_zero() {
        assert_eq!(release_gate(Some(0)), ReleaseDecision::Proceed);
        // any non-zero aborts
        assert!(matches!(release_gate(Some(1)), ReleaseDecision::Abort(_)));
        assert!(matches!(release_gate(Some(137)), ReleaseDecision::Abort(_)));
        // no exit code (failed to start / killed) aborts
        assert!(matches!(release_gate(None), ReleaseDecision::Abort(_)));
    }

    #[test]
    fn argv_wraps_any_toolchain_via_sh_lc() {
        // Toolchain-agnostic: the command is passed verbatim to `sh -lc`.
        assert_eq!(
            release_argv("bundle exec rake db:migrate"),
            vec!["sh", "-lc", "bundle exec rake db:migrate"]
        );
        assert_eq!(
            release_argv("alembic upgrade head"),
            vec!["sh", "-lc", "alembic upgrade head"]
        );
        assert_eq!(release_argv("sqlx migrate run").len(), 3);
    }
}

#[cfg(test)]
mod deploy_concurrency_tests {
    use super::{deploy_outcome, may_start_deploy, redeploy_would_noop, DeployOutcome};

    #[test]
    fn no_change_is_success_not_failure() {
        // Every service already on the target image → clean NoChanges, NOT Failed.
        assert_eq!(deploy_outcome(true, 0, 3), DeployOutcome::NoChanges);
        // Some rolled → Rolled (a real change shipped).
        assert_eq!(deploy_outcome(true, 2, 1), DeployOutcome::Rolled);
        assert_eq!(deploy_outcome(true, 2, 0), DeployOutcome::Rolled);
        // A build/release/roll error → Failed regardless of counts.
        assert_eq!(deploy_outcome(false, 0, 0), DeployOutcome::Failed);
        assert_eq!(deploy_outcome(false, 0, 3), DeployOutcome::Failed);
        // Degenerate (no services): healthy but nothing rolled or unchanged.
        assert_eq!(deploy_outcome(true, 0, 0), DeployOutcome::Rolled);
    }

    #[test]
    fn second_concurrent_deploy_is_rejected_not_spawned() {
        assert!(may_start_deploy(false)); // none in flight → proceed
        assert!(!may_start_deploy(true)); // already deploying → return already_deploying
    }

    #[test]
    fn redeploy_noop_detects_digest_pinned_images() {
        // Digest-pinned → force-new-deployment re-pulls the same frozen image →
        // app_redeploy can't ship new code → guard must trip.
        assert!(redeploy_would_noop(
            "637.dkr.ecr.us-east-1.amazonaws.com/nuvola@sha256:abc123"
        ));
        // Mutable tags → force-new-deployment legitimately re-pulls the tag.
        assert!(!redeploy_would_noop(
            "637.dkr.ecr.us-east-1.amazonaws.com/nuvola:prod"
        ));
        assert!(!redeploy_would_noop("nuvola:latest"));
    }
}

#[cfg(test)]
mod onboard_tests {
    use super::build_onboarding_plan;
    use crate::provisioning::manifest::AmosManifest;
    use serde_json::json;

    #[test]
    fn generates_a_valid_manifest_and_plan() {
        let out = build_onboarding_plan(&json!({
            "name": "acme",
            "git_repo": "https://github.com/acme/app.git",
            "release": "bundle exec rake db:migrate",
            "services": [
                { "name": "base", "dockerfile": "docker/base.Dockerfile", "build_only": true },
                { "name": "web", "dockerfile": "Dockerfile", "base": "base", "public": true }
            ],
            "engines": ["finance", "marketing"]
        }))
        .expect("plan");
        // The returned amos.yaml must itself parse + build-order cleanly.
        let yaml = out["amos_yaml"].as_str().unwrap();
        let m = AmosManifest::parse(yaml).expect("yaml parses");
        assert_eq!(m.name, "acme");
        assert_eq!(m.release.as_deref(), Some("bundle exec rake db:migrate"));
        m.build_order().expect("graph resolves (base before web)");
        // Plan surfaces the deploy call + engines (finance available, marketing not).
        assert!(out["next_steps"].as_array().unwrap().iter().any(|s| s
            .as_str()
            .unwrap()
            .contains("deploy(git_repo='https://github.com/acme/app.git'")));
        let engines = out["engines"].as_array().unwrap();
        assert_eq!(engines[0]["available"], json!(true)); // finance
        assert_eq!(engines[1]["available"], json!(false)); // marketing (roadmap)
        assert!(!out["human_steps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn missing_name_or_services_errors_clearly() {
        assert!(build_onboarding_plan(&json!({ "services": [{ "name": "web" }] })).is_err());
        assert!(build_onboarding_plan(&json!({ "name": "x" })).is_err());
        assert!(build_onboarding_plan(&json!({ "name": "x", "services": [] })).is_err());
        // A service missing its name is rejected.
        assert!(build_onboarding_plan(&json!({ "name": "x", "services": [{}] })).is_err());
    }

    #[test]
    fn unknown_base_reference_is_caught_by_build_order() {
        // web references a base service that doesn't exist → build_order errors,
        // so onboard_app never hands back an undeployable manifest.
        let r = build_onboarding_plan(&json!({
            "name": "x",
            "services": [{ "name": "web", "base": "nope" }]
        }));
        assert!(r.is_err());
    }
}
