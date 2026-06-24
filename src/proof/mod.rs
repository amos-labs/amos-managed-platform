//! Proof-carrying operations — the runtime analog of the relay's bounty
//! proof receipt (`amos-relay/src/proof_receipt.rs`).
//!
//! The product is an AI-native business environment that an agent (Claude)
//! *operates*. Letting an agent run real infrastructure — deploy apps, build
//! images, move money — is only safe if every consequential action is
//! **accountable**. An [`OperationReceipt`] is that accountability: a
//! structured record of *what* an operation did, *why*, *what guardrails* were
//! enforced, and *what was verified*. Receipts are persisted per-tenant
//! (`operation_receipts`), returned in the tool result so the operator sees
//! them inline, and are reviewable by the Oracle.
//!
//! Mirroring the relay receipt's design philosophy: this module enforces
//! **shape** (required fields are present and non-empty). Whether the intent
//! is honest or the verification adequate is **semantic** content, judged by
//! Oracle review — deliberately not grown into this module.
//!
//! Vocabulary is deliberately aligned with the extracted proofgate schema
//! (`proofgate/src/types.ts`, itself adapted from the relay receipt): an
//! `intent` with a `self_modifying` flag, a validation list, and
//! `passed`/`failed`/`skipped` evidence statuses. proofgate gates *code PRs*
//! (diff-bound: `changed_files`, `diff_sha256`); this gates *runtime
//! operations* (deploy/build/bill), which have no diff — so it is a sibling
//! artifact, not a reuse of the PR receipt.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use amos_core::{AmosError, Result};

/// The current receipt schema version.
pub const RECEIPT_VERSION: &str = "1";

/// A structured, auditable record of one platform operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationReceipt {
    /// Schema version (`RECEIPT_VERSION`).
    pub receipt_version: String,
    /// The operation performed, e.g. `"build_image"`, `"deploy_app"`.
    pub operation: String,
    /// Tenant the operation acted on.
    pub tenant_id: Uuid,
    /// Who initiated it — the authenticated subject (user/agent id).
    pub actor: String,
    /// What the operation set out to do and how risky it is.
    pub intent: Intent,
    /// Guardrails enforced during the operation (not aspirational — things the
    /// code actually checked/applied, e.g. `no_plaintext_secrets`).
    pub policy: Policy,
    /// Operation-specific inputs (source ref, spec name, …). Free-form JSON.
    pub inputs: Value,
    /// Verification performed and its outcomes — the "is it actually working"
    /// evidence. Empty is allowed but discouraged (Oracle weighs it).
    pub validation: Vec<Check>,
    /// Operation-specific outputs (image digest, deployment id, public url, …).
    pub outputs: Value,
    /// One-line human summary of the result.
    pub result_summary: String,
    /// When the receipt was emitted.
    pub emitted_at: DateTime<Utc>,
}

/// What an operation intends and how risky it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    /// Plain-language summary of the intent.
    pub summary: String,
    /// Whether the operation modifies AMOS's own behavior/guardrails (RSI
    /// risk). Almost always false for tenant app operations.
    pub self_modifying: bool,
    /// Coarse scope tag, e.g. `"app_lifecycle"`, `"build"`, `"billing"`.
    pub scope_classification: String,
}

/// Guardrails the operation enforced.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Identifiers of invariants enforced, e.g. `["no_plaintext_secrets",
    /// "tenant_isolation"]`.
    #[serde(default)]
    pub guardrails: Vec<String>,
}

/// One verification check and its outcome. Field/status vocabulary matches
/// proofgate's `ExecutionEvidence` (`command`/`status`/`output_ref`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    /// Stable check id, e.g. `"health_endpoint"`, `"image_pushed"`.
    pub id: String,
    /// `passed`, `failed`, or `skipped`.
    pub status: CheckStatus,
    /// Short evidence/detail for the outcome (or an `output_ref` to full logs).
    #[serde(default)]
    pub detail: String,
}

/// The result of a verification check — aligned with proofgate's status enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Skipped,
}

impl OperationReceipt {
    /// Start building a receipt for `operation` by `actor` on `tenant_id`.
    pub fn new(
        operation: impl Into<String>,
        tenant_id: Uuid,
        actor: impl Into<String>,
        intent: Intent,
    ) -> Self {
        Self {
            receipt_version: RECEIPT_VERSION.to_string(),
            operation: operation.into(),
            tenant_id,
            actor: actor.into(),
            intent,
            policy: Policy::default(),
            inputs: json!({}),
            validation: Vec::new(),
            outputs: json!({}),
            result_summary: String::new(),
            emitted_at: Utc::now(),
        }
    }

    /// Record an enforced guardrail (chainable).
    pub fn guardrail(mut self, id: impl Into<String>) -> Self {
        self.policy.guardrails.push(id.into());
        self
    }

    /// Attach operation inputs (chainable).
    pub fn inputs(mut self, inputs: Value) -> Self {
        self.inputs = inputs;
        self
    }

    /// Record a verification check (chainable).
    pub fn check(
        mut self,
        id: impl Into<String>,
        status: CheckStatus,
        detail: impl Into<String>,
    ) -> Self {
        self.validation.push(Check {
            id: id.into(),
            status,
            detail: detail.into(),
        });
        self
    }

    /// Attach operation outputs (chainable).
    pub fn outputs(mut self, outputs: Value) -> Self {
        self.outputs = outputs;
        self
    }

    /// Set the one-line result summary (chainable).
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.result_summary = summary.into();
        self
    }

    /// Whether every recorded check passed (no fails). Vacuously true when no
    /// checks were recorded — callers should record at least one for a
    /// meaningful verdict.
    pub fn verified(&self) -> bool {
        self.validation
            .iter()
            .all(|c| c.status != CheckStatus::Failed)
    }

    /// Validate the receipt's *shape* (required fields present and non-empty).
    /// Semantic adequacy is the Oracle's job.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.operation.trim().is_empty() {
            return Err("operation is required".into());
        }
        if self.actor.trim().is_empty() {
            return Err("actor is required".into());
        }
        if self.intent.summary.trim().is_empty() {
            return Err("intent.summary is required".into());
        }
        if self.intent.scope_classification.trim().is_empty() {
            return Err("intent.scope_classification is required".into());
        }
        if self.result_summary.trim().is_empty() {
            return Err("result_summary is required".into());
        }
        Ok(())
    }
}

/// Validate and persist a receipt to `operation_receipts`, returning its id.
///
/// This is the single emission point: it shape-validates first (a malformed
/// receipt is a programming error and must not be silently dropped), then
/// writes the row. The full receipt is stored as JSONB so it stays
/// self-describing and Oracle-reviewable without a schema migration per field.
pub async fn emit(pool: &PgPool, receipt: &OperationReceipt) -> Result<Uuid> {
    receipt
        .validate()
        .map_err(|e| AmosError::Internal(format!("invalid operation receipt: {e}")))?;

    let body = serde_json::to_value(receipt)
        .map_err(|e| AmosError::Internal(format!("failed to serialize receipt: {e}")))?;

    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO operation_receipts
            (tenant_id, operation, actor, verified, receipt)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(receipt.tenant_id)
    .bind(&receipt.operation)
    .bind(&receipt.actor)
    .bind(receipt.verified())
    .bind(&body)
    .fetch_one(pool)
    .await
    .map_err(AmosError::Database)?;

    Ok(id)
}

/// List a tenant's most recent operation receipts (newest first), optionally
/// filtered to one operation. This is the *read* side of accountability — the
/// operator (and the Oracle) inspect what the assistant did and whether it
/// verified. `limit` is clamped to a sane maximum.
pub async fn list_for_tenant(
    pool: &PgPool,
    tenant_id: Uuid,
    operation: Option<&str>,
    limit: i64,
) -> Result<Vec<Value>> {
    let limit = limit.clamp(1, 200);
    let rows = sqlx::query_as::<_, (Uuid, String, String, bool, Value, chrono::DateTime<Utc>)>(
        "SELECT id, operation, actor, verified, receipt, created_at
         FROM operation_receipts
         WHERE tenant_id = $1 AND ($2::text IS NULL OR operation = $2)
         ORDER BY created_at DESC
         LIMIT $3",
    )
    .bind(tenant_id)
    .bind(operation)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(AmosError::Database)?;

    Ok(rows
        .into_iter()
        .map(|(id, operation, actor, verified, receipt, created_at)| {
            json!({
                "id": id,
                "operation": operation,
                "actor": actor,
                "verified": verified,
                "created_at": created_at,
                "receipt": receipt,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> Intent {
        Intent {
            summary: "Deploy cuspr api+frontend+worker to AWS".into(),
            self_modifying: false,
            scope_classification: "app_lifecycle".into(),
        }
    }

    fn receipt() -> OperationReceipt {
        OperationReceipt::new("deploy_app", Uuid::nil(), "user-123", intent())
            .guardrail("no_plaintext_secrets")
            .guardrail("tenant_isolation")
            .inputs(json!({ "spec": "cuspr" }))
            .check("image_pushed", CheckStatus::Passed, "digest sha256:abc")
            .check("health_endpoint", CheckStatus::Passed, "200 on /")
            .outputs(json!({ "deployment_id": "dep-1" }))
            .summary("cuspr live; 3/3 services running")
    }

    #[test]
    fn well_formed_receipt_validates() {
        receipt().validate().unwrap();
    }

    #[test]
    fn missing_summary_fails() {
        let mut r = receipt();
        r.result_summary = "  ".into();
        assert!(r.validate().unwrap_err().contains("result_summary"));
    }

    #[test]
    fn missing_intent_summary_fails() {
        let mut r = receipt();
        r.intent.summary = String::new();
        assert!(r.validate().unwrap_err().contains("intent.summary"));
    }

    #[test]
    fn verified_true_when_no_fails() {
        assert!(receipt().verified());
    }

    #[test]
    fn verified_false_with_a_failing_check() {
        let r = receipt().check("smoke_e2e", CheckStatus::Failed, "login 500");
        assert!(!r.verified());
    }

    #[test]
    fn guardrails_accumulate() {
        let r = receipt();
        assert_eq!(r.policy.guardrails.len(), 2);
        assert!(r
            .policy
            .guardrails
            .contains(&"tenant_isolation".to_string()));
    }

    #[test]
    fn serializes_round_trip() {
        let r = receipt();
        let v = serde_json::to_value(&r).unwrap();
        let back: OperationReceipt = serde_json::from_value(v).unwrap();
        assert_eq!(back.operation, "deploy_app");
        assert_eq!(back.validation.len(), 2);
    }
}
