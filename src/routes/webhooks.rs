//! Stripe webhook handler.
//!
//! `POST /webhooks/stripe` — receives Stripe events, verifies signatures,
//! and processes checkout completions, subscription updates, and invoice events.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::state::PlatformState;

/// Legacy / grandfathered monthly managed-AI credit, in microcents, for plans
/// that aren't app-hosting tiers (the old $49.99 harness plans). $5.00 = 50,000.
/// Microcents = hundredths of a cent (1¢ = 100 microcents, $1 = 10,000),
/// matching `metered_billing::calculate_cost_microcents`.
///
/// App-hosting tiers use tier-scaled credits — see
/// [`crate::billing::app_hosting::AppHostingTier::monthly_ai_credit_microcents`]
/// (Starter $10 / Pro $25 / Compliance $50). Granted on checkout, reset on each
/// invoice.paid; no rollover. Gating happens in the harness pre-call check.
const MONTHLY_CREDIT_GRANT_MICROCENTS: i64 = 50_000;

/// Monthly managed-AI credit for a tenant's plan, in microcents: tier-scaled for
/// app-hosting tiers, legacy $5 for grandfathered/non-app plans.
fn credit_grant_microcents(plan: Option<&str>) -> i64 {
    plan.and_then(crate::billing::app_hosting::AppHostingTier::parse)
        .map(|t| t.monthly_ai_credit_microcents())
        .unwrap_or(MONTHLY_CREDIT_GRANT_MICROCENTS)
}

/// Handle incoming Stripe webhook events.
///
/// This endpoint is mounted **outside** the auth middleware so Stripe
/// can reach it. Signature verification provides authentication.
pub async fn stripe_webhook(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let stripe_config = match &state.stripe_config {
        Some(cfg) => cfg,
        None => {
            warn!("Stripe webhook received but Stripe is not configured");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };

    // Verify webhook signature
    let sig = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let webhook_secret = match &stripe_config.webhook_secret {
        Some(s) => s.as_str(),
        None => {
            warn!("Stripe webhook received but no webhook secret configured");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };

    let event = match stripe::Webhook::construct_event(&body, sig, webhook_secret) {
        Ok(e) => e,
        Err(e) => {
            warn!("Stripe webhook signature verification failed: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };

    let event_id = event.id.to_string();
    let event_type = format!("{:?}", event.type_);

    // Idempotency: skip if we've already processed this event
    let already_processed = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM stripe_webhook_events WHERE stripe_event_id = $1)",
    )
    .bind(&event_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);

    if already_processed {
        info!(event_id = %event_id, "Stripe webhook event already processed, skipping");
        return StatusCode::OK;
    }

    // Log the event
    let _ = sqlx::query(
        "INSERT INTO stripe_webhook_events (stripe_event_id, event_type, payload)
         VALUES ($1, $2, $3::jsonb)
         ON CONFLICT (stripe_event_id) DO NOTHING",
    )
    .bind(&event_id)
    .bind(&event_type)
    .bind(&body)
    .execute(&state.db)
    .await;

    info!(event_id = %event_id, event_type = %event_type, "Processing Stripe webhook");

    // Dispatch based on event type
    use stripe::EventType;
    match event.type_ {
        EventType::CheckoutSessionCompleted => {
            handle_checkout_completed(&state, &event).await;
        }
        EventType::CustomerSubscriptionUpdated | EventType::CustomerSubscriptionCreated => {
            handle_subscription_updated(&state, &event).await;
        }
        EventType::CustomerSubscriptionDeleted => {
            handle_subscription_deleted(&state, &event).await;
        }
        EventType::InvoicePaid => {
            handle_invoice_paid(&state, &event).await;
        }
        EventType::InvoicePaymentFailed => {
            handle_invoice_payment_failed(&state, &event).await;
        }
        _ => {
            info!(event_type = %event_type, "Unhandled Stripe event type, ignoring");
        }
    }

    StatusCode::OK
}

// ── Event Handlers ───────────────────────────────────────────────────────

/// Handle checkout.session.completed: activate subscription, provision harness.
async fn handle_checkout_completed(state: &PlatformState, event: &stripe::Event) {
    use stripe::EventObject;

    let session = match &event.data.object {
        EventObject::CheckoutSession(s) => s,
        _ => {
            warn!("checkout.session.completed: unexpected event object type");
            return;
        }
    };

    // Extract tenant_id from metadata
    let tenant_id = match session
        .metadata
        .as_ref()
        .and_then(|m| m.get("tenant_id"))
        .and_then(|v| Uuid::parse_str(v).ok())
    {
        Some(id) => id,
        None => {
            error!("checkout.session.completed: no tenant_id in metadata");
            return;
        }
    };

    // Extract subscription ID
    let subscription_id = match &session.subscription {
        Some(expandable) => expandable.id().to_string(),
        None => {
            error!("checkout.session.completed: no subscription in session");
            return;
        }
    };

    // Extract customer ID
    let customer_id = match &session.customer {
        Some(expandable) => expandable.id().to_string(),
        None => {
            error!("checkout.session.completed: no customer in session");
            return;
        }
    };

    // Plan purchased (from checkout metadata). App-hosting tiers (`app_*`) host
    // a deployed app, not a harness, so they record the tier as the plan and
    // skip harness provisioning. Harness sizes keep the existing behaviour.
    let plan = session
        .metadata
        .as_ref()
        .and_then(|m| m.get("plan"))
        .cloned();
    let is_app_tier = plan
        .as_deref()
        .and_then(crate::billing::app_hosting::AppHostingTier::parse)
        .is_some();

    info!(
        tenant_id = %tenant_id,
        subscription_id = %subscription_id,
        plan = ?plan,
        is_app_tier,
        "Checkout completed, activating subscription"
    );

    // Update tenant with Stripe IDs, activate subscription, record the plan if
    // one was supplied, and grant the initial managed-AI (Bedrock) credit —
    // tier-scaled for app-hosting tiers ($10/$25/$50), legacy $5 otherwise — so
    // the user can start operating immediately.
    let grant_microcents = credit_grant_microcents(plan.as_deref());
    let _ = sqlx::query(
        "UPDATE tenants
         SET stripe_customer_id = $1,
             stripe_subscription_id = $2,
             stripe_subscription_status = 'active',
             plan = COALESCE($5, plan),
             credit_balance_microcents = $4,
             credits_granted_at = NOW()
         WHERE id = $3",
    )
    .bind(&customer_id)
    .bind(&subscription_id)
    .bind(tenant_id)
    .bind(grant_microcents)
    .bind(&plan)
    .execute(&state.db)
    .await;

    info!(
        tenant_id = %tenant_id,
        microcents = grant_microcents,
        plan = ?plan,
        "Initial managed-AI credit grant"
    );

    // App-hosting tenants run a deployed app (via deploy_app), not a harness —
    // don't provision a harness for them.
    if is_app_tier {
        info!(tenant_id = %tenant_id, "App-hosting tier — skipping harness provisioning");
    } else {
        provision_harness_for_tenant(state, tenant_id).await;
    }
}

/// Handle customer.subscription.updated: sync status changes.
async fn handle_subscription_updated(state: &PlatformState, event: &stripe::Event) {
    use stripe::EventObject;

    let sub = match &event.data.object {
        EventObject::Subscription(s) => s,
        _ => {
            warn!("subscription.updated: unexpected event object type");
            return;
        }
    };

    let subscription_id = sub.id.to_string();
    let status = sub.status.as_str();
    let period_end = chrono::DateTime::from_timestamp(sub.current_period_end, 0);

    // Find the tenant by subscription ID
    let tenant_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM tenants WHERE stripe_subscription_id = $1")
            .bind(&subscription_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let tenant_id = match tenant_id {
        Some(id) => id,
        None => {
            // Also try metadata
            if let Some(tid) = sub
                .metadata
                .get("tenant_id")
                .and_then(|v| Uuid::parse_str(v).ok())
            {
                tid
            } else {
                warn!(subscription_id = %subscription_id, "subscription.updated: tenant not found");
                return;
            }
        }
    };

    info!(
        tenant_id = %tenant_id,
        subscription_id = %subscription_id,
        status = %status,
        "Subscription updated"
    );

    let _ = sqlx::query(
        "UPDATE tenants
         SET stripe_subscription_status = $1,
             subscription_current_period_end = $2
         WHERE id = $3",
    )
    .bind(status)
    .bind(period_end)
    .bind(tenant_id)
    .execute(&state.db)
    .await;
}

/// Handle customer.subscription.deleted: mark canceled, stop harness.
async fn handle_subscription_deleted(state: &PlatformState, event: &stripe::Event) {
    use stripe::EventObject;

    let sub = match &event.data.object {
        EventObject::Subscription(s) => s,
        _ => {
            warn!("subscription.deleted: unexpected event object type");
            return;
        }
    };

    let subscription_id = sub.id.to_string();

    let tenant_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM tenants WHERE stripe_subscription_id = $1")
            .bind(&subscription_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let tenant_id = match tenant_id {
        Some(id) => id,
        None => {
            warn!(subscription_id = %subscription_id, "subscription.deleted: tenant not found");
            return;
        }
    };

    info!(tenant_id = %tenant_id, "Subscription canceled, stopping harness");

    // Mark subscription as canceled
    let _ = sqlx::query("UPDATE tenants SET stripe_subscription_status = 'canceled' WHERE id = $1")
        .bind(tenant_id)
        .execute(&state.db)
        .await;

    // Stop all running harness instances for this tenant
    let harness_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM harness_instances WHERE tenant_id = $1 AND status = 'running'",
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    for harness_id in harness_ids {
        stop_harness(state, harness_id).await;
    }
}

/// Handle invoice.paid: clear past_due status and reset the monthly managed-AI
/// credit (tier-scaled per the tenant's plan; legacy $5 for grandfathered
/// plans). Fires on every successful payment — initial signup invoice and every
/// renewal — so this is the recurring refresh point. No rollover by design.
async fn handle_invoice_paid(state: &PlatformState, event: &stripe::Event) {
    use stripe::EventObject;

    let invoice = match &event.data.object {
        EventObject::Invoice(i) => i,
        _ => return,
    };

    let sub_id = match &invoice.subscription {
        Some(expandable) => expandable.id().to_string(),
        None => return,
    };

    // Tier-scale the credit by the tenant's recorded plan (legacy $5 fallback).
    let plan: Option<String> =
        sqlx::query_scalar("SELECT plan FROM tenants WHERE stripe_subscription_id = $1")
            .bind(&sub_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    let grant_microcents = credit_grant_microcents(plan.as_deref());

    info!(
        subscription_id = %sub_id,
        microcents = grant_microcents,
        plan = ?plan,
        "Invoice paid, clearing past_due and resetting managed-AI credit"
    );

    // Bill the CLOSING period's overage BEFORE resetting credit for the new
    // period. Order is independent (overage = Σcost − granted credit, not the
    // running balance), but closing-then-opening keeps the timeline clean.
    bill_period_close_overage(state, invoice, &sub_id).await;

    let _ = sqlx::query(
        "UPDATE tenants
         SET stripe_subscription_status = CASE
                 WHEN stripe_subscription_status = 'past_due' THEN 'active'
                 ELSE stripe_subscription_status
             END,
             credit_balance_microcents = $1,
             credits_granted_at = NOW()
         WHERE stripe_subscription_id = $2",
    )
    .bind(grant_microcents)
    .bind(&sub_id)
    .execute(&state.db)
    .await;
}

/// At invoice.paid (= period close), bill the overage that accrued during the
/// closing period as one-time invoice items on the *next* invoice:
///
/// - **Bedrock**: shared-backend usage beyond the tier's granted monthly credit,
///   at the ×1.10 uplift ([`guards::with_overage_uplift`]). Only usage on
///   `shared_bedrock` harnesses counts — BYOK usage never drew down credit.
/// - **Egress**: data-out beyond the tier's included allowance. The egress rate
///   already carries margin, so **no uplift** is applied.
///
/// Both submissions are **idempotent** on (tenant, invoice, kind): Stripe
/// redelivers the same invoice id on webhook retry, so the deterministic key
/// collapses duplicates and a retry can never double-charge. Usage rows are
/// marked `billed = TRUE` **only after** a successful Bedrock submission (strict
/// submit→mark ordering); on submit failure the rows are left unbilled and the
/// idempotent key makes the next retry safe.
///
/// Skipped entirely for self-hosted / legacy / non-app-tier tenants (no managed
/// billing relationship to invoice against).
async fn bill_period_close_overage(state: &PlatformState, invoice: &stripe::Invoice, sub_id: &str) {
    use crate::billing::{app_hosting::AppHostingTier, guards};

    // Inside a signature-verified webhook, so Stripe is configured — but guard
    // anyway: no client => no billing (self-hosted bypass).
    let client = match &state.stripe_client {
        Some(c) => c,
        None => return,
    };

    // Resolve tenant + Stripe customer + plan. A missing customer id means there
    // is nothing to invoice against; bail.
    let row: Option<(Uuid, Option<String>, String)> = sqlx::query_as(
        "SELECT id, stripe_customer_id, plan FROM tenants WHERE stripe_subscription_id = $1",
    )
    .bind(sub_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let (tenant_id, customer_id, plan) = match row {
        Some((t, Some(c), p)) => (t, c, p),
        _ => return,
    };

    // Only managed app-hosting tiers get overage billing (legacy/free skip).
    let tier = match AppHostingTier::parse(&plan) {
        Some(t) => t,
        None => return,
    };

    // Deterministic period anchor = the invoice being paid. Stripe redelivers the
    // same invoice id on retry, so this is the idempotency boundary.
    let period = invoice
        .number
        .clone()
        .unwrap_or_else(|| invoice.id.to_string());

    // ── Bedrock overage ──────────────────────────────────────────────────
    // Closing-period shared usage = unbilled records on shared_bedrock harnesses.
    // BYOK usage (other provider modes) never drew down credit and is excluded.
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT r.id, r.cost_microcents
         FROM llm_usage_records r
         JOIN harness_instances h ON h.id = r.harness_id
         WHERE r.tenant_id = $1 AND r.billed = FALSE
           AND h.llm_provider_mode = 'shared_bedrock'",
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let total_cost: i64 = rows.iter().map(|(_, c)| *c).sum();
    let credit_granted = tier.monthly_ai_credit_microcents();
    let overage_micro = guards::bedrock_overage_microcents(total_cost, credit_granted);
    let overage_cents =
        guards::microcents_to_cents_ceil(guards::with_overage_uplift(overage_micro));

    if overage_cents > 0 && !guards::overage_within_sanity_ceiling(overage_cents) {
        // Suspiciously large overage — HOLD for human review, never auto-charge.
        // Rows are left unbilled (re-evaluated next period); a legitimately large
        // overage simply stays held until a human submits it. This bounds the
        // blast radius of any over-count (e.g. a charge-succeeded-but-mark-failed
        // row set bleeding into a later period) toward NOT over-charging.
        error!(
            tenant_id = %tenant_id, %period, cents = overage_cents,
            ceiling = guards::OVERAGE_SANITY_CEILING_CENTS,
            "bedrock overage exceeds sanity ceiling; HELD for review, not auto-charged"
        );
    } else if overage_cents > 0 {
        let desc = format!("Managed AI overage ({} tier)", tier.plan_key());
        match guards::submit_overage_invoice_item(
            client,
            &customer_id,
            overage_cents,
            &desc,
            tenant_id,
            &period,
            "bedrock",
        )
        .await
        {
            Ok(item_id) => {
                // STRICT ORDER: mark billed ONLY after Stripe accepted the charge.
                let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
                let marked = sqlx::query(
                    "UPDATE llm_usage_records SET billed = TRUE, stripe_usage_record_id = $1
                     WHERE id = ANY($2)",
                )
                .bind(&item_id)
                .bind(&ids)
                .execute(&state.db)
                .await;
                if let Err(e) = marked {
                    // Charge submitted but rows not marked: the idempotent key
                    // makes the next invoice.paid retry safe (no double-charge).
                    error!(tenant_id = %tenant_id, invoice_item = %item_id, "overage charged but failed to mark rows billed: {e}");
                } else {
                    info!(tenant_id = %tenant_id, invoice_item = %item_id, cents = overage_cents, rows = ids.len(), "billed bedrock overage");
                }
            }
            Err(e) => {
                warn!(tenant_id = %tenant_id, %period, "bedrock overage submit failed; rows left unbilled for idempotent retry: {e}");
            }
        }
    } else if !rows.is_empty() {
        // Within credit (or zero): no charge, but close out the period so these
        // rows don't carry into the next period's Σcost.
        let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
        let _ = sqlx::query("UPDATE llm_usage_records SET billed = TRUE WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&state.db)
            .await;
    }

    // ── Egress overage ───────────────────────────────────────────────────
    // Metered from CloudWatch over the invoice's billing window. No DB rows to
    // mark — the idempotent (tenant, invoice, "egress") key is the only dedup.
    // HELD OFF (guards::BILL_EGRESS_OVERAGE) until the closing-period window is
    // verified against a real renewal invoice — see the const's doc comment.
    if guards::BILL_EGRESS_OVERAGE {
        if let (Some(meter), Some(start_ts), Some(end_ts)) =
            (&state.usage_meter, invoice.period_start, invoice.period_end)
        {
            if let (Some(start), Some(end)) = (
                chrono::DateTime::from_timestamp(start_ts, 0),
                chrono::DateTime::from_timestamp(end_ts, 0),
            ) {
                let summary = meter
                    .tenant_period_summary(&state.db, tenant_id, start, end)
                    .await;
                let egress_cents = summary
                    .charge
                    .map(|c| c.egress_overage_cents as i64)
                    .unwrap_or(0);
                if egress_cents > 0 {
                    let desc = format!("Egress overage ({} tier)", tier.plan_key());
                    // Egress rate already includes margin — no uplift.
                    match guards::submit_overage_invoice_item(
                        client,
                        &customer_id,
                        egress_cents,
                        &desc,
                        tenant_id,
                        &period,
                        "egress",
                    )
                    .await
                    {
                        Ok(item_id) => {
                            info!(tenant_id = %tenant_id, invoice_item = %item_id, cents = egress_cents, "billed egress overage")
                        }
                        Err(e) => {
                            warn!(tenant_id = %tenant_id, %period, "egress overage submit failed (idempotent retry safe): {e}")
                        }
                    }
                }
            }
        }
    } // if guards::BILL_EGRESS_OVERAGE
}

/// Handle invoice.payment_failed: mark as past_due.
async fn handle_invoice_payment_failed(state: &PlatformState, event: &stripe::Event) {
    use stripe::EventObject;

    let invoice = match &event.data.object {
        EventObject::Invoice(i) => i,
        _ => return,
    };

    let sub_id = match &invoice.subscription {
        Some(expandable) => expandable.id().to_string(),
        None => return,
    };

    warn!(subscription_id = %sub_id, "Invoice payment failed, marking past_due");

    let _ = sqlx::query(
        "UPDATE tenants
         SET stripe_subscription_status = 'past_due'
         WHERE stripe_subscription_id = $1",
    )
    .bind(&sub_id)
    .execute(&state.db)
    .await;
}

// ── Harness Provisioning Helper ──────────────────────────────────────────

/// Provision a harness instance for a tenant. Called after Stripe checkout
/// completes or when upgrading from the free plan.
pub async fn provision_harness_for_tenant(state: &PlatformState, tenant_id: Uuid) {
    use std::collections::HashMap;

    // Check if tenant already has a running or provisioning harness
    let existing: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, status FROM harness_instances WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    if let Some((harness_id, status)) = &existing {
        if status == "running" || status == "provisioning" {
            info!(tenant_id = %tenant_id, harness_id = %harness_id, status = %status, "Harness already exists, skipping provisioning");
            return;
        }
    }

    // Get tenant slug for subdomain
    let slug: Option<String> = sqlx::query_scalar("SELECT slug FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

    let slug = match slug {
        Some(s) => s,
        None => {
            error!(tenant_id = %tenant_id, "Cannot provision harness: tenant not found");
            return;
        }
    };

    // Create or reuse harness instance record
    let harness_id = match existing {
        Some((id, _)) => {
            let _ = sqlx::query("UPDATE harness_instances SET status = 'pending' WHERE id = $1")
                .bind(id)
                .execute(&state.db)
                .await;
            id
        }
        None => {
            let id = Uuid::new_v4();
            let _ = sqlx::query(
                "INSERT INTO harness_instances (id, tenant_id, subdomain, status) VALUES ($1, $2, $3, 'pending')",
            )
            .bind(id)
            .bind(tenant_id)
            .bind(&slug)
            .execute(&state.db)
            .await;
            id
        }
    };

    info!(tenant_id = %tenant_id, harness_id = %harness_id, "Provisioning harness");

    // Use ECS provisioner if available, otherwise Docker
    if let Some(ref ecs) = state.ecs_provisioner {
        use crate::provisioning::{HarnessConfig, InstanceSize};

        let _ = sqlx::query("UPDATE harness_instances SET status = 'provisioning' WHERE id = $1")
            .bind(harness_id)
            .execute(&state.db)
            .await;

        // Create per-harness isolated database.
        let base_db_url = ecs.harness_database_url();
        let mut harness_env = HashMap::new();
        if !base_db_url.is_empty() {
            match crate::provisioning::db::create_harness_database(base_db_url, harness_id).await {
                Ok(db_url) => {
                    let db_name = crate::provisioning::db::database_name_for_harness(harness_id);
                    harness_env.insert("AMOS__DATABASE__URL".to_string(), db_url);
                    let _ = sqlx::query(
                        "UPDATE harness_instances SET database_name = $1 WHERE id = $2",
                    )
                    .bind(&db_name)
                    .bind(harness_id)
                    .execute(&state.db)
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

        let current_version: Option<String> = sqlx::query_scalar(
            "SELECT version FROM releases WHERE status = 'available' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        match ecs.provision(&ecs_config, &slug).await {
            Ok(task_arn) => {
                info!(task_arn = %task_arn, "ECS task launched for tenant");

                let _ = sqlx::query(
                    "UPDATE harness_instances SET container_id = $1, provisioned_at = NOW(), image_tag = $2 WHERE id = $3",
                )
                .bind(&task_arn)
                .bind(&current_version)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                // Spawn background poller (same pattern as register_submit)
                crate::routes::ui::spawn_ecs_status_poller_public(
                    state.ecs_provisioner.clone().unwrap(),
                    state.db.clone(),
                    task_arn,
                    harness_id,
                    state.alb_router.clone(),
                    Some(slug),
                );
            }
            Err(e) => {
                error!("Failed to provision ECS harness: {}", e);
                let _ = sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                    .bind(harness_id)
                    .execute(&state.db)
                    .await;
            }
        }
    } else if let Some(ref manager) = state.harness_manager {
        use crate::provisioning::{HarnessConfig, InstanceSize};

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
                let _ = sqlx::query(
                    "UPDATE harness_instances SET container_id = $1, provisioned_at = NOW() WHERE id = $2",
                )
                .bind(&container_id)
                .bind(harness_id)
                .execute(&state.db)
                .await;

                if let Err(e) = manager.start(&container_id).await {
                    warn!("Failed to start harness container: {}", e);
                    let _ =
                        sqlx::query("UPDATE harness_instances SET status = 'error' WHERE id = $1")
                            .bind(harness_id)
                            .execute(&state.db)
                            .await;
                } else {
                    let _ = sqlx::query(
                        "UPDATE harness_instances SET status = 'running', started_at = NOW(), healthy = TRUE WHERE id = $1",
                    )
                    .bind(harness_id)
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
    } else {
        warn!("No provisioner available, harness stays pending");
    }
}

/// Stop a harness instance (used when subscription is canceled).
async fn stop_harness(state: &PlatformState, harness_id: Uuid) {
    let container_id: Option<String> =
        sqlx::query_scalar("SELECT container_id FROM harness_instances WHERE id = $1")
            .bind(harness_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let container_id = match container_id {
        Some(id) => id,
        None => return,
    };

    if let Some(ref ecs) = state.ecs_provisioner {
        if let Err(e) = ecs.stop(&container_id).await {
            warn!(harness_id = %harness_id, "Failed to stop ECS task: {}", e);
        }
    } else if let Some(ref manager) = state.harness_manager {
        if let Err(e) = manager.stop(&container_id).await {
            warn!(harness_id = %harness_id, "Failed to stop container: {}", e);
        }
    }

    let _ = sqlx::query(
        "UPDATE harness_instances SET status = 'stopped', healthy = FALSE WHERE id = $1",
    )
    .bind(harness_id)
    .execute(&state.db)
    .await;

    info!(harness_id = %harness_id, "Harness stopped due to subscription cancellation");
}
