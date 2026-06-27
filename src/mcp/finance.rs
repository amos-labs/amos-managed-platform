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
