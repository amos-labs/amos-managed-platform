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
                    "instructions": "AMOS managed-environment control plane. Use these tools to \
                provision, inspect, operate, and configure governed AMOS environments without re-discovering \
                infrastructure. Tools are scoped to your tenant.",
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

enum ToolError {
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
        "deploy_app" => tool_deploy_app(state, claims, args).await,
        "list_apps" => tool_list_apps(state, claims, args).await,
        "app_status" => tool_app_status(state, claims, args).await,
        "app_logs" => tool_app_logs(state, claims, args).await,
        "app_control" => tool_app_control(state, claims, args).await,
        // ── Build-as-a-service ────────────────────────────────────────────
        "build_image" => tool_build_image(state, claims, args).await,
        "build_status" => tool_build_status(state, claims, args).await,
        // ── Proof-carrying operations ─────────────────────────────────────
        "list_receipts" => tool_list_receipts(state, claims, args).await,
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

    let context_dir = arg_str(&args, "context_dir")?;
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
        context_dir: std::path::PathBuf::from(&context_dir),
        dockerfile: dockerfile.clone(),
        image_name: image_name.clone(),
        tag: tag.clone(),
        build_args,
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
    let bg_ctx = context_dir.clone();
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
                    "context_dir": { "type": "string", "description": "Absolute path to the build context directory (on the platform host)." },
                    "image_name": { "type": "string", "description": "ECR repository name to push to, e.g. 'cuspr-api' (created if absent)." },
                    "dockerfile": { "type": "string", "description": "Dockerfile path relative to the context (default 'Dockerfile')." },
                    "tag": { "type": "string", "description": "Image tag (default 'latest')." },
                    "build_args": { "type": "object", "description": "Docker --build-arg values, e.g. {\"BASE_IMAGE\":\"…/cuspr-base:latest\"}. Values must not contain spaces." },
                    "callback_url": { "type": "string", "description": "Optional URL the platform POSTs to when the build reaches a terminal state (ping-back), so you don't have to poll build_status." },
                    "tenant_id": { "type": "string", "description": "Tenant UUID (admins only)" }
                },
                "required": ["context_dir", "image_name"]
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
        }
    ])
}
