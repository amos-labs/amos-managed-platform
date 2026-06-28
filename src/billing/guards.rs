//! Usage guards — the safety layer that keeps every metered consumable
//! **bounded and billed**, never silently unbounded.
//!
//! Two concerns live here:
//!
//! 1. **Circuit-breaker / past-due gate** (pure decisions, no money moves) —
//!    given a tenant's credit balance + subscription status, decide whether the
//!    shared Bedrock backend may still be used. The harness polls this via the
//!    balance endpoint and hard-stops shared calls when the answer is "no",
//!    bounding the worst-case overrun to a single sync interval.
//!
//! 2. **Overage capture** (math + idempotent Stripe submission) — the small
//!    amount of shared spend that slips through before the breaker trips, plus
//!    egress beyond the included allowance, is billed at cost **× uplift** as a
//!    one-time invoice item. The math is pure + tested; the Stripe write is a
//!    single idempotent call so it is safe to retry.
//!
//! Design rule: **self-hosted / Stripe-not-configured tenants are never gated.**
//! Billing is bypassed entirely in that mode, so the breaker must default open.

/// Uplift applied to pass-through overage costs (Bedrock, egress) expressed as
/// a rational `num/den`. 110/100 = +10%. Configurable here so the uplift is a
/// single source of truth across the codebase.
pub const OVERAGE_UPLIFT_NUM: i64 = 110;
pub const OVERAGE_UPLIFT_DEN: i64 = 100;

/// Fraction of the included allowance at which we raise a soft-cap **alert**
/// (not a hard stop) so the tenant can be warned before they incur overage.
pub const SOFT_CAP_ALERT_PCT: f64 = 80.0;

/// Hard ceiling (in cents) on a *single* auto-submitted overage invoice item.
/// A computed overage above this is NOT auto-charged — it is held and alerted
/// for human review. This bounds the blast radius of any over-count (e.g. a
/// charge-succeeded-but-mark-failed row set that bleeds into a later period, or
/// a future math bug): we always err toward **not** over-charging. Set well
/// above any plausible single-tenant monthly AI spend at current scale.
pub const OVERAGE_SANITY_CEILING_CENTS: i64 = 200_000; // $2,000

/// Whether a computed overage is safe to auto-submit, or must be held for human
/// review because it exceeds [`OVERAGE_SANITY_CEILING_CENTS`].
pub fn overage_within_sanity_ceiling(cents: i64) -> bool {
    cents <= OVERAGE_SANITY_CEILING_CENTS
}

/// Egress overage auto-billing switch. **ON.** The period-close egress arm now
/// meters CloudWatch over the CLOSING window `[credits_granted_at, now]` (not
/// `invoice.period_*`, which on a renewal describes the *upcoming* period under
/// advance billing). Kill-switch retained: set `false` to disable egress billing
/// without a code change to the billing path.
pub const BILL_EGRESS_OVERAGE: bool = true;

/// Subscription statuses that block use of the shared backend even if a credit
/// balance remains: the account has a billing problem and must be resolved.
fn status_blocks_shared(status: &str) -> bool {
    matches!(
        status,
        "past_due" | "unpaid" | "canceled" | "incomplete_expired"
    )
}

/// Why shared Bedrock is (dis)allowed — surfaced to the harness + UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BedrockGateReason {
    /// Billing not configured (self-hosted / dev): never gated.
    BillingDisabled,
    /// Credits remain and the subscription is in good standing.
    Ok,
    /// Monthly credit balance is exhausted; tenant must BYOK to continue.
    CreditsExhausted,
    /// Subscription is past-due/unpaid/canceled: resolve billing to continue.
    BillingPastDue,
}

/// The shared-Bedrock gate decision for a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SharedBedrockDecision {
    /// Whether the harness may make shared-backend Bedrock calls right now.
    pub allow_shared_bedrock: bool,
    /// True when the block is specifically due to a zero credit balance
    /// (distinct from a billing/past-due block) — drives the "Set up a key"
    /// vs "Fix billing" CTA in the harness/banner.
    pub exhausted: bool,
    /// Machine-readable reason.
    pub reason: BedrockGateReason,
}

/// Decide whether shared Bedrock is allowed.
///
/// - `stripe_enabled`: is billing configured on this platform at all? If not
///   (self-hosted / dev), the gate defaults **open** — billing is bypassed.
/// - `balance_microcents`: the tenant's remaining monthly credit (>0 = has credit).
/// - `sub_status`: the tenant's `stripe_subscription_status` (`None` = no
///   subscription on record — legacy/free; not treated as a billing failure).
///
/// Order matters: a past-due account is blocked even if a stale balance remains.
pub fn shared_bedrock_decision(
    stripe_enabled: bool,
    balance_microcents: i64,
    sub_status: Option<&str>,
) -> SharedBedrockDecision {
    if !stripe_enabled {
        return SharedBedrockDecision {
            allow_shared_bedrock: true,
            exhausted: false,
            reason: BedrockGateReason::BillingDisabled,
        };
    }
    if let Some(s) = sub_status {
        if status_blocks_shared(s) {
            return SharedBedrockDecision {
                allow_shared_bedrock: false,
                exhausted: false,
                reason: BedrockGateReason::BillingPastDue,
            };
        }
    }
    if balance_microcents <= 0 {
        return SharedBedrockDecision {
            allow_shared_bedrock: false,
            exhausted: true,
            reason: BedrockGateReason::CreditsExhausted,
        };
    }
    SharedBedrockDecision {
        allow_shared_bedrock: true,
        exhausted: false,
        reason: BedrockGateReason::Ok,
    }
}

/// Apply the overage uplift to a pass-through cost (in microcents). Saturates
/// rather than overflows. Used for both Bedrock-past-credit and egress overage
/// so the markup is consistent.
pub fn with_overage_uplift(cost_microcents: i64) -> i64 {
    if cost_microcents <= 0 {
        return 0;
    }
    (cost_microcents as i128 * OVERAGE_UPLIFT_NUM as i128 / OVERAGE_UPLIFT_DEN as i128)
        .min(i64::MAX as i128) as i64
}

/// The portion of a shared-Bedrock charge that exceeds the available credit —
/// i.e. the amount that must be billed as overage (before uplift).
///
/// `balance_before` is the credit balance *before* this charge was deducted.
pub fn bedrock_overage_microcents(cost_microcents: i64, balance_before: i64) -> i64 {
    let covered = cost_microcents.min(balance_before.max(0));
    (cost_microcents - covered).max(0)
}

/// Convert microcents to whole cents, **rounding up** so a sub-cent overage is
/// never silently dropped (we round in our own favor, by at most $0.01). 1¢ =
/// 100 microcents. Stripe invoice-item amounts are in cents.
pub fn microcents_to_cents_ceil(microcents: i64) -> i64 {
    if microcents <= 0 {
        return 0;
    }
    (microcents + 99) / 100
}

/// Soft-cap status for a metered allowance (egress today; reusable for others).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct CapStatus {
    pub used: f64,
    pub included: f64,
    /// Used as a percentage of the included allowance (0 included => 100 if any used).
    pub pct_of_included: f64,
    /// True once usage crosses [`SOFT_CAP_ALERT_PCT`] of the allowance.
    pub alert: bool,
    /// Amount used beyond the included allowance (0 if within).
    pub over: f64,
}

/// Compute soft-cap status for `used` against an `included` allowance.
pub fn cap_status(used: f64, included: f64) -> CapStatus {
    let pct = if included > 0.0 {
        used / included * 100.0
    } else if used > 0.0 {
        100.0
    } else {
        0.0
    };
    CapStatus {
        used,
        included,
        pct_of_included: pct,
        alert: pct >= SOFT_CAP_ALERT_PCT,
        over: (used - included).max(0.0),
    }
}

// ── Idempotent Stripe overage submission ──────────────────────────────────
//
// NOTE (live money): this writes a real invoice item to a live Stripe account.
// It is intentionally NOT auto-invoked from the sync hot path — it is meant to
// be driven by a period-close job (or an explicit admin action) after review.
// Idempotency is enforced via a deterministic idempotency key, so a retry for
// the same tenant+period+kind never double-charges.

/// Build the deterministic Stripe idempotency key for an overage submission.
/// Same tenant + period + kind => same key => Stripe collapses retries.
pub fn overage_idempotency_key(tenant_id: uuid::Uuid, period: &str, kind: &str) -> String {
    format!("overage:{kind}:{tenant_id}:{period}")
}

/// Submit a single overage invoice item to Stripe for `amount_cents` (already
/// uplifted), idempotent on (tenant, period, kind). Returns the invoice item id.
///
/// Caller is responsible for marking the underlying usage rows billed only on
/// success. `customer_id` must be the tenant's `stripe_customer_id`.
pub async fn submit_overage_invoice_item(
    client: &stripe::Client,
    customer_id: &str,
    amount_cents: i64,
    description: &str,
    tenant_id: uuid::Uuid,
    period: &str,
    kind: &str,
) -> Result<String, stripe::StripeError> {
    if amount_cents <= 0 {
        return Err(stripe::StripeError::ClientError(
            "refusing to submit non-positive overage".into(),
        ));
    }
    let customer: stripe::CustomerId = customer_id.parse().map_err(|_| {
        stripe::StripeError::ClientError(format!("invalid customer id: {customer_id}"))
    })?;

    let mut params = stripe::CreateInvoiceItem::new(customer);
    params.amount = Some(amount_cents);
    params.currency = Some(stripe::Currency::USD);
    params.description = Some(description);
    params.metadata = Some(
        [
            ("tenant_id".to_string(), tenant_id.to_string()),
            ("period".to_string(), period.to_string()),
            ("kind".to_string(), kind.to_string()),
        ]
        .into_iter()
        .collect(),
    );

    let idempotency_key = overage_idempotency_key(tenant_id, period, kind);
    let item = stripe::InvoiceItem::create(
        &client
            .clone()
            .with_strategy(stripe::RequestStrategy::Idempotent(idempotency_key)),
        params,
    )
    .await?;
    Ok(item.id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_is_never_gated() {
        let d = shared_bedrock_decision(false, 0, Some("past_due"));
        assert!(d.allow_shared_bedrock);
        assert_eq!(d.reason, BedrockGateReason::BillingDisabled);
    }

    #[test]
    fn credits_remaining_in_good_standing_allows() {
        let d = shared_bedrock_decision(true, 50_000, Some("active"));
        assert!(d.allow_shared_bedrock);
        assert!(!d.exhausted);
        assert_eq!(d.reason, BedrockGateReason::Ok);
    }

    #[test]
    fn zero_balance_blocks_and_marks_exhausted() {
        let d = shared_bedrock_decision(true, 0, Some("active"));
        assert!(!d.allow_shared_bedrock);
        assert!(d.exhausted);
        assert_eq!(d.reason, BedrockGateReason::CreditsExhausted);

        // Negative (over-deducted) balance behaves the same.
        assert!(!shared_bedrock_decision(true, -10, Some("active")).allow_shared_bedrock);
    }

    #[test]
    fn past_due_blocks_even_with_balance() {
        let d = shared_bedrock_decision(true, 999_999, Some("past_due"));
        assert!(!d.allow_shared_bedrock);
        assert!(!d.exhausted); // billing problem, not exhaustion
        assert_eq!(d.reason, BedrockGateReason::BillingPastDue);

        for s in ["unpaid", "canceled", "incomplete_expired"] {
            assert!(!shared_bedrock_decision(true, 999_999, Some(s)).allow_shared_bedrock);
        }
    }

    #[test]
    fn no_subscription_record_is_not_a_billing_failure() {
        // Legacy/free tenant with credit but no subscription row: allowed.
        let d = shared_bedrock_decision(true, 10_000, None);
        assert!(d.allow_shared_bedrock);
        // grace statuses pass the past-due check
        assert!(shared_bedrock_decision(true, 10_000, Some("trialing")).allow_shared_bedrock);
        assert!(shared_bedrock_decision(true, 10_000, Some("incomplete")).allow_shared_bedrock);
    }

    #[test]
    fn uplift_is_ten_percent_and_saturating() {
        assert_eq!(with_overage_uplift(0), 0);
        assert_eq!(with_overage_uplift(-5), 0);
        assert_eq!(with_overage_uplift(1_000), 1_100);
        assert_eq!(with_overage_uplift(10_000), 11_000);
        // No panic/overflow at the top of the range.
        assert!(with_overage_uplift(i64::MAX) > 0);
    }

    #[test]
    fn overage_is_amount_past_available_credit() {
        // Fully covered by credit -> no overage.
        assert_eq!(bedrock_overage_microcents(1_000, 5_000), 0);
        // Exactly covered.
        assert_eq!(bedrock_overage_microcents(5_000, 5_000), 0);
        // Partially covered -> remainder is overage.
        assert_eq!(bedrock_overage_microcents(8_000, 5_000), 3_000);
        // No credit at all -> whole cost is overage.
        assert_eq!(bedrock_overage_microcents(8_000, 0), 8_000);
        // Negative (already overdrawn) balance treated as zero credit.
        assert_eq!(bedrock_overage_microcents(8_000, -100), 8_000);
    }

    #[test]
    fn cap_status_alerts_at_eighty_percent() {
        let c = cap_status(40.0, 50.0); // 80%
        assert!(c.alert);
        assert_eq!(c.over, 0.0);

        let under = cap_status(39.0, 50.0); // 78%
        assert!(!under.alert);

        let over = cap_status(60.0, 50.0); // 120%
        assert!(over.alert);
        assert_eq!(over.over, 10.0);

        // Zero allowance: any usage is "100%+".
        assert!(cap_status(1.0, 0.0).alert);
        assert!(!cap_status(0.0, 0.0).alert);
    }

    #[test]
    fn period_close_overage_is_sum_minus_credit_floored_then_uplifted() {
        // Period close: overage = max(0, Σ shared cost − credit granted),
        // then ×1.10, then microcents→cents (ceil).
        let credit = 100_000; // $10 tier credit (microcents)

        // Under credit: nothing billed.
        let under = bedrock_overage_microcents(60_000, credit);
        assert_eq!(under, 0);
        assert_eq!(microcents_to_cents_ceil(with_overage_uplift(under)), 0);

        // Exactly at credit: nothing billed.
        assert_eq!(bedrock_overage_microcents(100_000, credit), 0);

        // Over credit by $5 (50_000 microcents): overage ×1.10 = 55_000 micro = 550¢ = $5.50.
        let over = bedrock_overage_microcents(150_000, credit);
        assert_eq!(over, 50_000);
        let uplifted = with_overage_uplift(over);
        assert_eq!(uplifted, 55_000);
        assert_eq!(microcents_to_cents_ceil(uplifted), 550);

        // Sub-cent rounds up, never dropped.
        assert_eq!(microcents_to_cents_ceil(1), 1);
        assert_eq!(microcents_to_cents_ceil(100), 1);
        assert_eq!(microcents_to_cents_ceil(101), 2);
        assert_eq!(microcents_to_cents_ceil(0), 0);
    }

    #[test]
    fn idempotency_key_is_deterministic_per_tenant_period_kind() {
        let t = uuid::Uuid::nil();
        let a = overage_idempotency_key(t, "2026-06", "bedrock");
        let b = overage_idempotency_key(t, "2026-06", "bedrock");
        let c = overage_idempotency_key(t, "2026-06", "egress");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
