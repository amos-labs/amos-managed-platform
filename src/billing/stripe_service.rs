//! Stripe API integration for customer billing, checkout, and portal sessions.

use stripe::{
    BillingPortalSession, CheckoutSession, CheckoutSessionMode, Client, CreateBillingPortalSession,
    CreateCheckoutSession, CreateCheckoutSessionLineItems, CreateCustomer, Customer, CustomerId,
};
use uuid::Uuid;

use crate::state::PlatformState;

/// Create a Stripe customer and return their ID.
pub async fn create_customer(
    client: &Client,
    email: &str,
    name: &str,
    tenant_id: Uuid,
) -> Result<CustomerId, stripe::StripeError> {
    let mut params = CreateCustomer::new();
    params.email = Some(email);
    params.name = Some(name);
    params.metadata = Some(
        [("tenant_id".to_string(), tenant_id.to_string())]
            .into_iter()
            .collect(),
    );

    let customer = Customer::create(client, params).await?;
    Ok(customer.id)
}

/// Create a Stripe Checkout Session for a new subscription.
///
/// Returns the checkout URL the user should be redirected to.
pub async fn create_checkout_session(
    client: &Client,
    customer_id: &str,
    price_id: &str,
    success_url: &str,
    cancel_url: &str,
    tenant_id: Uuid,
) -> Result<String, stripe::StripeError> {
    let mut params = CreateCheckoutSession::new();
    params.customer = Some(customer_id.parse().map_err(|_| {
        stripe::StripeError::ClientError(format!("Invalid customer ID: {}", customer_id))
    })?);
    params.mode = Some(CheckoutSessionMode::Subscription);
    params.success_url = Some(success_url);
    params.cancel_url = Some(cancel_url);
    params.line_items = Some(vec![CreateCheckoutSessionLineItems {
        price: Some(price_id.to_string()),
        quantity: Some(1),
        ..Default::default()
    }]);
    params.metadata = Some(
        [("tenant_id".to_string(), tenant_id.to_string())]
            .into_iter()
            .collect(),
    );
    params.subscription_data = Some(stripe::CreateCheckoutSessionSubscriptionData {
        metadata: Some(
            [("tenant_id".to_string(), tenant_id.to_string())]
                .into_iter()
                .collect(),
        ),
        ..Default::default()
    });

    let session = CheckoutSession::create(client, params).await?;
    Ok(session.url.unwrap_or_default())
}

/// Create a Stripe Customer Portal session.
///
/// Returns the portal URL the user should be redirected to.
pub async fn create_portal_session(
    client: &Client,
    customer_id: &str,
    return_url: &str,
) -> Result<String, stripe::StripeError> {
    let mut params = CreateBillingPortalSession::new(customer_id.parse().map_err(|_| {
        stripe::StripeError::ClientError(format!("Invalid customer ID: {}", customer_id))
    })?);
    params.return_url = Some(return_url);

    let session = BillingPortalSession::create(client, params).await?;
    Ok(session.url)
}

/// Cancel a Stripe subscription at period end.
pub async fn cancel_subscription(
    client: &Client,
    subscription_id: &str,
) -> Result<(), stripe::StripeError> {
    let sub_id: stripe::SubscriptionId = subscription_id.parse().map_err(|_| {
        stripe::StripeError::ClientError(format!("Invalid subscription ID: {}", subscription_id))
    })?;
    let mut params = stripe::UpdateSubscription::new();
    params.cancel_at_period_end = Some(true);
    stripe::Subscription::update(client, &sub_id, params).await?;
    Ok(())
}

/// Resolve the Stripe Price ID for a given harness size.
pub fn price_id_for_size(state: &PlatformState, size: &str) -> Option<String> {
    state
        .stripe_config
        .as_ref()
        .and_then(|cfg| cfg.price_id_for_size(size).map(|s| s.to_string()))
}

#[cfg(test)]
mod tests {
    use crate::state::StripeConfig;

    fn test_config() -> StripeConfig {
        StripeConfig {
            secret_key: "sk_test_fake".into(),
            publishable_key: None,
            webhook_secret: None,
            price_small: Some("price_small_123".into()),
            price_medium: Some("price_medium_456".into()),
            price_large: Some("price_large_789".into()),
        }
    }

    #[test]
    fn price_id_for_each_size() {
        let cfg = test_config();
        assert_eq!(cfg.price_id_for_size("small"), Some("price_small_123"));
        assert_eq!(cfg.price_id_for_size("medium"), Some("price_medium_456"));
        assert_eq!(cfg.price_id_for_size("large"), Some("price_large_789"));
    }

    #[test]
    fn legacy_plan_names_map_to_small() {
        let cfg = test_config();
        assert_eq!(cfg.price_id_for_size("hosted"), Some("price_small_123"));
        assert_eq!(cfg.price_id_for_size("starter"), Some("price_small_123"));
        assert_eq!(cfg.price_id_for_size("growth"), Some("price_small_123"));
        assert_eq!(cfg.price_id_for_size("enterprise"), Some("price_small_123"));
    }

    #[test]
    fn free_and_unknown_return_none() {
        let cfg = test_config();
        assert_eq!(cfg.price_id_for_size("free"), None);
        assert_eq!(cfg.price_id_for_size("unknown"), None);
    }

    #[test]
    fn missing_price_config_returns_none() {
        let cfg = StripeConfig {
            secret_key: "sk_test_fake".into(),
            publishable_key: None,
            webhook_secret: None,
            price_small: None,
            price_medium: None,
            price_large: None,
        };
        assert_eq!(cfg.price_id_for_size("small"), None);
        assert_eq!(cfg.price_id_for_size("medium"), None);
        assert_eq!(cfg.price_id_for_size("large"), None);
    }

    #[test]
    fn stripe_config_from_env_requires_secret_key() {
        std::env::remove_var("AMOS__STRIPE__SECRET_KEY");
        assert!(StripeConfig::from_env().is_none());
    }
}
