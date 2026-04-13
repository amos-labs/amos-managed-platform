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

    info!(
        tenant_id = %tenant_id,
        subscription_id = %subscription_id,
        "Checkout completed, activating subscription"
    );

    // Update tenant with Stripe IDs and activate subscription
    let _ = sqlx::query(
        "UPDATE tenants
         SET stripe_customer_id = $1,
             stripe_subscription_id = $2,
             stripe_subscription_status = 'active'
         WHERE id = $3",
    )
    .bind(&customer_id)
    .bind(&subscription_id)
    .bind(tenant_id)
    .execute(&state.db)
    .await;

    // Provision harness for the tenant
    provision_harness_for_tenant(state, tenant_id).await;
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

/// Handle invoice.paid: clear past_due status.
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

    info!(subscription_id = %sub_id, "Invoice paid, clearing past_due");

    let _ = sqlx::query(
        "UPDATE tenants
         SET stripe_subscription_status = 'active'
         WHERE stripe_subscription_id = $1 AND stripe_subscription_status = 'past_due'",
    )
    .bind(&sub_id)
    .execute(&state.db)
    .await;
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
