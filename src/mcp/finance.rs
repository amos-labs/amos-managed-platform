//! Finance engine — the governed MCP verb surface for a tenant's finances.
//!
//! This is the **reference engine** in the AMOS engine architecture: a governed
//! business building block an app *enables*, never rebuilds. The verb surface
//! mirrors the proven Nuvola finance MCP 1:1 (board / truth / history / QBO /
//! revenue / subscriptions / churn for reads; line + actual/budget/mapping +
//! billing-key for writes), but here every call is **scoped** (`finance:*`,
//! enforced centrally in [`super::dispatch_tool`]) and every *write* emits a
//! **proof receipt**.
//!
//! The actual finance data + integrations live behind the
//! [`FinanceEngineClient`] trait. Today that's [`StubFinanceClient`] (canned
//! responses, so the governed surface is exercisable end-to-end); the real
//! multi-tenant backend — Nuvola's `Finance::*` extracted into a standalone
//! service with `tenant_id` + per-tenant vault creds — drops in behind the same
//! trait as an `HttpFinanceClient`, with **no change to the verbs or governance**.

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{resolve_tenant, ToolError};
use crate::auth::Claims;
use crate::proof::{self, CheckStatus, Intent, OperationReceipt};
use crate::state::PlatformState;

/// The backend a finance verb proxies to. One method per MCP verb; `tenant_id`
/// scopes the call and `args` is the verb's raw input (passed through, since the
/// extracted backend's exact schemas aren't frozen yet). Returns the JSON the
/// verb hands back to the operator.
#[async_trait]
pub trait FinanceEngineClient: Send + Sync {
    // ── reads (finance:read) ──────────────────────────────────────────────
    async fn finance_board(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn finance_history(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn finance_truth(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn qbo_accounts(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn revenue_summary(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn org_subscriptions(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn churn_snapshot(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    // ── writes (finance:write) ────────────────────────────────────────────
    async fn create_finance_line(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn update_finance_line(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn set_finance_actual(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn set_finance_budget(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    async fn set_finance_mapping(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
    // ── connect (finance:connect, owner-only) ─────────────────────────────
    async fn set_billing_key(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value>;
}

/// Placeholder backend: returns deterministic canned JSON so the governed verb
/// surface (scopes + proof receipts + dispatch) is testable end-to-end before
/// the real backend exists. **STUB — replace with an `HttpFinanceClient` against
/// the extracted multi-tenant finance service.** Every payload carries
/// `"stub": true` so it's never mistaken for live data.
pub struct StubFinanceClient;

#[async_trait]
impl FinanceEngineClient for StubFinanceClient {
    async fn finance_board(&self, tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "tenant_id": tenant_id, "board": [] }))
    }
    async fn finance_history(&self, tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "tenant_id": tenant_id, "history": [] }))
    }
    async fn finance_truth(&self, tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "tenant_id": tenant_id, "truth": {} }))
    }
    async fn qbo_accounts(&self, tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "tenant_id": tenant_id, "accounts": [] }))
    }
    async fn revenue_summary(&self, tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "tenant_id": tenant_id, "revenue": { "mrr": 0, "arr": 0 } }))
    }
    async fn org_subscriptions(&self, tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "tenant_id": tenant_id, "subscriptions": [] }))
    }
    async fn churn_snapshot(&self, tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "tenant_id": tenant_id, "churn": { "rate": 0.0, "count": 0 } }))
    }
    async fn create_finance_line(&self, _tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "ok": true, "id": "stub-line-0" }))
    }
    async fn update_finance_line(&self, _tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "ok": true }))
    }
    async fn set_finance_actual(&self, _tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "ok": true }))
    }
    async fn set_finance_budget(&self, _tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "ok": true }))
    }
    async fn set_finance_mapping(&self, _tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "ok": true }))
    }
    async fn set_billing_key(&self, _tenant_id: Uuid, _args: &Value) -> anyhow::Result<Value> {
        Ok(json!({ "stub": true, "ok": true, "connected": true }))
    }
}

/// Map a backend error to a tool execution error.
fn exec_err(e: anyhow::Error) -> ToolError {
    ToolError::Execution(e.to_string())
}

/// Emit a proof receipt for a finance **write**. Reads never call this — only
/// the six mutating verbs do, so every change to a tenant's finances is
/// accountable (operation = the verb, scope = the finance scope it required).
async fn emit_write_receipt(
    state: &PlatformState,
    tenant_id: Uuid,
    actor: String,
    operation: &str,
    scope: &str,
    inputs: Value,
    outputs: &Value,
    summary: &str,
) {
    let intent = Intent {
        summary: summary.to_string(),
        // Mutates tenant business data, not AMOS's own guardrails (no RSI risk).
        self_modifying: false,
        scope_classification: scope.to_string(),
    };
    let receipt = OperationReceipt::new(operation, tenant_id, actor, intent)
        .guardrail("finance_engine")
        .guardrail("tenant_isolation")
        .guardrail("scope_enforced")
        .inputs(inputs)
        .check(
            "write_accepted",
            CheckStatus::Passed,
            "finance engine accepted the write",
        )
        .outputs(outputs.clone())
        .summary(format!("{operation}: {summary}"));
    let _ = proof::emit(&state.db, &receipt).await;
}

// ── Read verbs (finance:read; no receipt — metadata/data read) ─────────────

macro_rules! finance_read {
    ($fn_name:ident, $method:ident) => {
        pub(crate) async fn $fn_name(
            state: &PlatformState,
            claims: &Claims,
            args: Value,
        ) -> Result<Value, ToolError> {
            let tenant_id = resolve_tenant(claims, &args)?;
            state
                .finance
                .$method(tenant_id, &args)
                .await
                .map_err(exec_err)
        }
    };
}

finance_read!(tool_finance_board, finance_board);
finance_read!(tool_finance_history, finance_history);
finance_read!(tool_finance_truth, finance_truth);
finance_read!(tool_qbo_accounts, qbo_accounts);
finance_read!(tool_revenue_summary, revenue_summary);
finance_read!(tool_org_subscriptions, org_subscriptions);
finance_read!(tool_churn_snapshot, churn_snapshot);

// ── Write verbs (finance:write; proof receipt on success) ──────────────────

macro_rules! finance_write {
    ($fn_name:ident, $method:ident, $op:literal, $summary:literal) => {
        pub(crate) async fn $fn_name(
            state: &PlatformState,
            claims: &Claims,
            args: Value,
        ) -> Result<Value, ToolError> {
            let tenant_id = resolve_tenant(claims, &args)?;
            let actor = claims.sub.clone();
            let out = state
                .finance
                .$method(tenant_id, &args)
                .await
                .map_err(exec_err)?;
            emit_write_receipt(
                state,
                tenant_id,
                actor,
                $op,
                "finance:write",
                args.clone(),
                &out,
                $summary,
            )
            .await;
            Ok(out)
        }
    };
}

finance_write!(
    tool_create_finance_line,
    create_finance_line,
    "create_finance_line",
    "create a finance line"
);
finance_write!(
    tool_update_finance_line,
    update_finance_line,
    "update_finance_line",
    "update a finance line"
);
finance_write!(
    tool_set_finance_actual,
    set_finance_actual,
    "set_finance_actual",
    "set a finance actual"
);
finance_write!(
    tool_set_finance_budget,
    set_finance_budget,
    "set_finance_budget",
    "set a finance budget"
);
finance_write!(
    tool_set_finance_mapping,
    set_finance_mapping,
    "set_finance_mapping",
    "set a QBO/category mapping"
);

// ── Connect verb (finance:connect, owner-only; proof receipt, secret redacted)

/// Connect a finance integration / set a billing credential. The most sensitive
/// finance verb (owner-only `finance:connect`). The receipt **redacts the secret
/// value** — it records *that* a key was set for a provider, never the key.
pub(crate) async fn tool_set_billing_key(
    state: &PlatformState,
    claims: &Claims,
    args: Value,
) -> Result<Value, ToolError> {
    let tenant_id = resolve_tenant(claims, &args)?;
    let actor = claims.sub.clone();
    let out = state
        .finance
        .set_billing_key(tenant_id, &args)
        .await
        .map_err(exec_err)?;
    // Never put the credential in the proof receipt.
    let redacted = json!({
        "provider": args.get("provider").cloned().unwrap_or(Value::Null),
        "key": "***redacted***",
    });
    emit_write_receipt(
        state,
        tenant_id,
        actor,
        "set_billing_key",
        "finance:connect",
        redacted,
        &out,
        "connect a finance integration / set a billing key",
    )
    .await;
    Ok(out)
}

// ── HttpFinanceClient — proxy to Nuvola's existing MCP (JSON-RPC) ───────────
//
// Step 0 of the finance-engine extraction: AMOS fronts Nuvola's *existing*
// (single-org) finance MCP, so the governed verbs operate real books today.
// Nuvola finance is single-org for now, so `tenant_id` is accepted and ignored;
// it gets threaded through once Nuvola's finance tables are org-scoped.

/// A `FinanceEngineClient` that proxies each verb to Nuvola's MCP endpoint
/// (`POST {base_url}`, JSON-RPC 2.0 `tools/call`, `Authorization: Bearer`).
pub struct HttpFinanceClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl HttpFinanceClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url,
            token,
            http: reqwest::Client::new(),
        }
    }

    /// Build from env. Requires `AMOS__NUVOLA__MCP_TOKEN` (an `mcp*` ApiKey);
    /// `AMOS__NUVOLA__MCP_URL` defaults to the prod endpoint. `None` when no
    /// token is set, so callers fall back to the stub.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("AMOS__NUVOLA__MCP_TOKEN").ok()?;
        let base_url = std::env::var("AMOS__NUVOLA__MCP_URL")
            .unwrap_or_else(|_| "https://app.nuvolaacademy.com/mcp".to_string());
        Some(Self::new(base_url, token))
    }

    /// Invoke one MCP verb and return its unwrapped JSON payload.
    async fn call(&self, verb: &str, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        // Inject the AMOS tenant_id so Nuvola scopes the call to that tenant's
        // books. (A call with no tenant_id falls back to Nuvola's primary
        // tenant; AMOS always sends one so routing is explicit + per-tenant.)
        let scoped = with_tenant_id(args, tenant_id);
        let body = rpc_request_body(verb, &scoped);
        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("nuvola mcp request failed: {e}"))?;
        let status = resp.status();
        let json: Value = resp.json().await.map_err(|e| {
            anyhow::anyhow!("nuvola mcp: invalid JSON response (status {status}): {e}")
        })?;
        unwrap_rpc_result(&json)
    }
}

/// Merge the AMOS `tenant_id` into a verb's arguments so Nuvola scopes the call
/// to that tenant's books. Non-object args (e.g. null) become a fresh object.
fn with_tenant_id(args: &Value, tenant_id: Uuid) -> Value {
    let mut obj = match args {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.insert("tenant_id".to_string(), json!(tenant_id.to_string()));
    Value::Object(obj)
}

/// Build the JSON-RPC 2.0 `tools/call` request body for a verb.
fn rpc_request_body(verb: &str, args: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": verb, "arguments": args },
    })
}

/// Unwrap a Nuvola MCP JSON-RPC response down to the verb's JSON payload.
/// Nuvola returns `result.content[0].text` as a JSON *string* (or surfaces a
/// JSON-RPC `error`); this turns either into a `Result<Value>`.
fn unwrap_rpc_result(resp: &Value) -> anyhow::Result<Value> {
    if let Some(err) = resp.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("nuvola mcp error {code}: {msg}");
    }
    let text = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("nuvola mcp: missing result.content[0].text"))?;
    serde_json::from_str(text).map_err(|e| anyhow::anyhow!("nuvola mcp: result text not JSON: {e}"))
}

// Each verb proxies to the MCP tool of the same name, injecting the AMOS
// `tenant_id` (via `call`) so Nuvola scopes the result to that tenant's books.
#[async_trait]
impl FinanceEngineClient for HttpFinanceClient {
    async fn finance_board(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("finance_board", tenant_id, args).await
    }
    async fn finance_history(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("finance_history", tenant_id, args).await
    }
    async fn finance_truth(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("finance_truth", tenant_id, args).await
    }
    async fn qbo_accounts(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("qbo_accounts", tenant_id, args).await
    }
    async fn revenue_summary(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("revenue_summary", tenant_id, args).await
    }
    async fn org_subscriptions(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("org_subscriptions", tenant_id, args).await
    }
    async fn churn_snapshot(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("churn_snapshot", tenant_id, args).await
    }
    async fn create_finance_line(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("create_finance_line", tenant_id, args).await
    }
    async fn update_finance_line(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("update_finance_line", tenant_id, args).await
    }
    async fn set_finance_actual(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("set_finance_actual", tenant_id, args).await
    }
    async fn set_finance_budget(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("set_finance_budget", tenant_id, args).await
    }
    async fn set_finance_mapping(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("set_finance_mapping", tenant_id, args).await
    }
    async fn set_billing_key(&self, tenant_id: Uuid, args: &Value) -> anyhow::Result<Value> {
        self.call("set_billing_key", tenant_id, args).await
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    #[test]
    fn with_tenant_id_injects_into_args() {
        let t = Uuid::nil();
        // Object args: tenant_id added, existing keys preserved.
        let merged = with_tenant_id(&json!({ "from": "2026-04-01" }), t);
        assert_eq!(merged["from"], "2026-04-01");
        assert_eq!(merged["tenant_id"], t.to_string());
        // Non-object args: a fresh object with just tenant_id.
        let from_null = with_tenant_id(&Value::Null, t);
        assert_eq!(from_null["tenant_id"], t.to_string());
    }

    #[test]
    fn rpc_request_body_is_tools_call() {
        let b = rpc_request_body("finance_board", &json!({ "from": "2026-04-01" }));
        assert_eq!(b["jsonrpc"], "2.0");
        assert_eq!(b["method"], "tools/call");
        assert_eq!(b["params"]["name"], "finance_board");
        assert_eq!(b["params"]["arguments"]["from"], "2026-04-01");
    }

    #[test]
    fn unwrap_parses_content_text_json() {
        // Nuvola wraps the verb payload as result.content[0].text = JSON string.
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "content": [{
                "type": "text",
                "text": "{\"months\":[\"2026-06-01\"],\"revenue\":[],\"expense\":[]}"
            }] }
        });
        let out = unwrap_rpc_result(&resp).unwrap();
        assert_eq!(out["months"][0], "2026-06-01");
        assert!(out["revenue"].is_array());
    }

    #[test]
    fn unwrap_surfaces_jsonrpc_error() {
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32_602, "message": "Unknown tool: nope" }
        });
        let err = unwrap_rpc_result(&resp).unwrap_err().to_string();
        assert!(err.contains("-32602"), "got: {err}");
        assert!(err.contains("Unknown tool"), "got: {err}");
    }

    #[test]
    fn unwrap_errors_on_missing_content() {
        let resp = json!({ "jsonrpc": "2.0", "id": 1, "result": {} });
        assert!(unwrap_rpc_result(&resp).is_err());
    }
}
