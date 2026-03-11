//! Customer billing and subscription management.

use amos_core::token::economics::AMOS_DISCOUNT_BPS;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Customer account record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub organization: Option<String>,
    pub plan: Plan,
    pub created_at: DateTime<Utc>,
    pub harness_id: Option<String>,
}

/// Subscription plan tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    /// Free tier: limited features for evaluation.
    Free,
    /// Starter: $99/month, suitable for small teams.
    Starter,
    /// Growth: $499/month, suitable for growing companies.
    Growth,
    /// Enterprise: custom pricing, full features + support.
    Enterprise,
}

impl Plan {
    /// Get the resource limits for this plan.
    pub fn limits(&self) -> PlanLimits {
        match self {
            Plan::Free => PlanLimits {
                max_conversations_per_month: 100,
                max_bots: 1,
                max_integrations: 2,
                storage_gb: 1,
                max_users: 1,
                support_level: "community".into(),
            },
            Plan::Starter => PlanLimits {
                max_conversations_per_month: 5_000,
                max_bots: 5,
                max_integrations: 10,
                storage_gb: 10,
                max_users: 5,
                support_level: "email".into(),
            },
            Plan::Growth => PlanLimits {
                max_conversations_per_month: 50_000,
                max_bots: 20,
                max_integrations: 50,
                storage_gb: 100,
                max_users: 25,
                support_level: "priority".into(),
            },
            Plan::Enterprise => PlanLimits {
                max_conversations_per_month: u64::MAX,
                max_bots: u64::MAX,
                max_integrations: u64::MAX,
                storage_gb: u64::MAX,
                max_users: u64::MAX,
                support_level: "dedicated".into(),
            },
        }
    }

    /// Monthly price in cents (USD).
    pub fn monthly_price_cents(&self) -> u64 {
        match self {
            Plan::Free => 0,
            Plan::Starter => 99_00,
            Plan::Growth => 499_00,
            Plan::Enterprise => 0, // Custom pricing
        }
    }
}

/// Resource limits for a subscription plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanLimits {
    pub max_conversations_per_month: u64,
    pub max_bots: u64,
    pub max_integrations: u64,
    pub storage_gb: u64,
    pub max_users: u64,
    pub support_level: String,
}

/// Active subscription record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub plan: Plan,
    pub status: SubscriptionStatus,
    pub started_at: DateTime<Utc>,
    pub current_period_start: DateTime<Utc>,
    pub current_period_end: DateTime<Utc>,
    pub cancel_at: Option<DateTime<Utc>>,
}

/// Subscription status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    PastDue,
    Canceled,
    Trialing,
}

/// Usage metrics for billing period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageMetrics {
    /// Total conversations in current billing period.
    pub conversations: u64,
    /// Total tokens processed (input + output).
    pub tokens_used: u64,
    /// Currently running bots.
    pub bots_running: u64,
    /// Storage used in GB.
    pub storage_used_gb: u64,
}

impl UsageMetrics {
    /// Check if usage exceeds plan limits.
    pub fn exceeds_limits(&self, limits: &PlanLimits) -> Vec<String> {
        let mut violations = Vec::new();

        if self.conversations > limits.max_conversations_per_month {
            violations.push(format!(
                "Conversations: {} > {} limit",
                self.conversations, limits.max_conversations_per_month
            ));
        }

        if self.bots_running > limits.max_bots {
            violations.push(format!(
                "Bots: {} > {} limit",
                self.bots_running, limits.max_bots
            ));
        }

        if self.storage_used_gb > limits.storage_gb {
            violations.push(format!(
                "Storage: {} GB > {} GB limit",
                self.storage_used_gb, limits.storage_gb
            ));
        }

        violations
    }
}

//    Compute Cost Tracking

/// AI model pricing per 1M tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub model_name: String,
    pub input_rate_per_million: f64,  // USD
    pub output_rate_per_million: f64, // USD
}

/// A single compute usage record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeUsageRecord {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub model_name: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd_cents: u64,     // Raw AWS cost
    pub charge_usd_cents: u64,   // Customer charge (cost * 1.20)
    pub customer_owned_model: bool, // Sovereign AI: true if customer-owned infrastructure
    pub timestamp: DateTime<Utc>,
}

/// Monthly billing summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingSummary {
    pub customer_id: Uuid,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub subscription_charge_cents: u64,
    pub compute_charges_cents: u64,
    pub total_charges_cents: u64,
    pub amos_discount_applied: bool,
    pub discount_amount_cents: u64,
    pub final_amount_cents: u64,
}

//    Cost Calculation Functions

/// Calculate AI cost from token usage.
/// Returns cost in USD cents.
pub fn calculate_ai_cost(
    input_tokens: u64,
    output_tokens: u64,
    pricing: &ModelPricing,
) -> u64 {
    let input_cost = (input_tokens as f64 / 1_000_000.0) * pricing.input_rate_per_million;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * pricing.output_rate_per_million;
    let total_usd = input_cost + output_cost;

    // Convert to cents and round
    (total_usd * 100.0).round() as u64
}

/// Apply markup to raw cost. Customer-owned models get no markup.
pub fn calculate_customer_charge(cost_cents: u64, customer_owned: bool) -> u64 {
    if customer_owned {
        // No markup for customer-owned models — they pay their own infrastructure
        cost_cents
    } else {
        // Standard 20% markup for AMOS-managed compute
        (cost_cents as f64 * 1.20).round() as u64
    }
}

/// Calculate full billing summary for a customer.
pub fn calculate_billing_summary(
    customer: &Customer,
    subscription: &Subscription,
    compute_records: &[ComputeUsageRecord],
    pay_with_amos: bool,
) -> BillingSummary {
    // Calculate subscription charge
    let subscription_charge_cents = subscription.plan.monthly_price_cents();

    // Calculate total compute charges
    let compute_charges_cents: u64 = compute_records
        .iter()
        .map(|r| r.charge_usd_cents)
        .sum();

    // Total before discount
    let total_charges_cents = subscription_charge_cents + compute_charges_cents;

    // Apply AMOS discount if paying with tokens
    let (discount_amount_cents, final_amount_cents) = if pay_with_amos {
        // AMOS_DISCOUNT_BPS is 2000 (20%) - apply to total
        let discount = (total_charges_cents as f64 * (AMOS_DISCOUNT_BPS as f64 / 10_000.0)).round() as u64;
        let final_amount = total_charges_cents.saturating_sub(discount);
        (discount, final_amount)
    } else {
        (0, total_charges_cents)
    };

    BillingSummary {
        customer_id: customer.id,
        period_start: subscription.current_period_start,
        period_end: subscription.current_period_end,
        subscription_charge_cents,
        compute_charges_cents,
        total_charges_cents,
        amos_discount_applied: pay_with_amos,
        discount_amount_cents,
        final_amount_cents,
    }
}

//    Default Model Pricing

/// Get default AI model pricing.
pub fn default_model_pricing() -> Vec<ModelPricing> {
    vec![
        ModelPricing {
            model_name: "qwen3-next-80b".into(),
            input_rate_per_million: 0.20,
            output_rate_per_million: 0.80,
        },
        ModelPricing {
            model_name: "claude-3.5-haiku".into(),
            input_rate_per_million: 0.25,
            output_rate_per_million: 1.25,
        },
        ModelPricing {
            model_name: "claude-3.5-sonnet".into(),
            input_rate_per_million: 3.00,
            output_rate_per_million: 15.00,
        },
    ]
}

/// Get pricing for a specific model by name.
pub fn get_model_pricing(model_name: &str) -> Option<ModelPricing> {
    default_model_pricing()
        .into_iter()
        .find(|p| p.model_name == model_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_limits_are_progressive() {
        let free = Plan::Free.limits();
        let starter = Plan::Starter.limits();
        let growth = Plan::Growth.limits();

        assert!(starter.max_conversations_per_month > free.max_conversations_per_month);
        assert!(growth.max_conversations_per_month > starter.max_conversations_per_month);
    }

    #[test]
    fn usage_violations_detected() {
        let limits = Plan::Starter.limits();
        let usage = UsageMetrics {
            conversations: 10_000, // Exceeds 5,000 limit
            tokens_used: 1_000_000,
            bots_running: 3,
            storage_used_gb: 5,
        };

        let violations = usage.exceeds_limits(&limits);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("Conversations"));
    }

    //    Cost Calculation Tests

    #[test]
    fn calculate_ai_cost_small_usage() {
        let pricing = ModelPricing {
            model_name: "test-model".into(),
            input_rate_per_million: 1.00,
            output_rate_per_million: 2.00,
        };

        // 100K input, 50K output
        let cost = calculate_ai_cost(100_000, 50_000, &pricing);

        // Expected: (100K / 1M) * $1.00 + (50K / 1M) * $2.00
        //         = 0.1 * 1.00 + 0.05 * 2.00
        //         = 0.10 + 0.10 = $0.20 = 20 cents
        assert_eq!(cost, 20);
    }

    #[test]
    fn calculate_ai_cost_large_usage() {
        let pricing = ModelPricing {
            model_name: "claude-3.5-sonnet".into(),
            input_rate_per_million: 3.00,
            output_rate_per_million: 15.00,
        };

        // 1M input, 500K output
        let cost = calculate_ai_cost(1_000_000, 500_000, &pricing);

        // Expected: (1M / 1M) * $3.00 + (500K / 1M) * $15.00
        //         = 1.0 * 3.00 + 0.5 * 15.00
        //         = 3.00 + 7.50 = $10.50 = 1050 cents
        assert_eq!(cost, 1050);
    }

    #[test]
    fn calculate_ai_cost_zero_usage() {
        let pricing = ModelPricing {
            model_name: "test-model".into(),
            input_rate_per_million: 1.00,
            output_rate_per_million: 2.00,
        };

        let cost = calculate_ai_cost(0, 0, &pricing);
        assert_eq!(cost, 0);
    }

    #[test]
    fn calculate_customer_charge_applies_markup() {
        // 20% markup on $1.00 = $1.20
        assert_eq!(calculate_customer_charge(100, false), 120);

        // 20% markup on $10.00 = $12.00
        assert_eq!(calculate_customer_charge(1000, false), 1200);

        // 20% markup on $0.50 = $0.60
        assert_eq!(calculate_customer_charge(50, false), 60);
    }

    #[test]
    fn calculate_customer_charge_zero() {
        assert_eq!(calculate_customer_charge(0, false), 0);
    }

    #[test]
    fn calculate_customer_charge_sovereign_ai_no_markup() {
        // Customer-owned model: no markup
        assert_eq!(calculate_customer_charge(100, true), 100);
        assert_eq!(calculate_customer_charge(1000, true), 1000);
        assert_eq!(calculate_customer_charge(0, true), 0);

        // AMOS-managed: 20% markup still applies
        assert_eq!(calculate_customer_charge(100, false), 120);
    }

    #[test]
    fn billing_summary_without_amos_discount() {
        let customer = Customer {
            id: Uuid::new_v4(),
            name: "Test Customer".into(),
            email: "test@example.com".into(),
            organization: None,
            plan: Plan::Starter,
            created_at: Utc::now(),
            harness_id: None,
        };

        let subscription = Subscription {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            plan: Plan::Starter,
            status: SubscriptionStatus::Active,
            started_at: Utc::now(),
            current_period_start: Utc::now(),
            current_period_end: Utc::now(),
            cancel_at: None,
        };

        let compute_records = vec![
            ComputeUsageRecord {
                id: Uuid::new_v4(),
                customer_id: customer.id,
                model_name: "claude-3.5-haiku".into(),
                input_tokens: 1_000_000,
                output_tokens: 500_000,
                cost_usd_cents: 100,
                charge_usd_cents: 120, // 20% markup
                customer_owned_model: false,
                timestamp: Utc::now(),
            },
            ComputeUsageRecord {
                id: Uuid::new_v4(),
                customer_id: customer.id,
                model_name: "claude-3.5-haiku".into(),
                input_tokens: 500_000,
                output_tokens: 250_000,
                cost_usd_cents: 50,
                charge_usd_cents: 60,
                customer_owned_model: false,
                timestamp: Utc::now(),
            },
        ];

        let summary = calculate_billing_summary(&customer, &subscription, &compute_records, false);

        assert_eq!(summary.customer_id, customer.id);
        assert_eq!(summary.subscription_charge_cents, 99_00); // Starter plan
        assert_eq!(summary.compute_charges_cents, 180); // 120 + 60
        assert_eq!(summary.total_charges_cents, 99_00 + 180);
        assert!(!summary.amos_discount_applied);
        assert_eq!(summary.discount_amount_cents, 0);
        assert_eq!(summary.final_amount_cents, 99_00 + 180);
    }

    #[test]
    fn billing_summary_with_amos_discount() {
        let customer = Customer {
            id: Uuid::new_v4(),
            name: "Test Customer".into(),
            email: "test@example.com".into(),
            organization: None,
            plan: Plan::Growth,
            created_at: Utc::now(),
            harness_id: None,
        };

        let subscription = Subscription {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            plan: Plan::Growth,
            status: SubscriptionStatus::Active,
            started_at: Utc::now(),
            current_period_start: Utc::now(),
            current_period_end: Utc::now(),
            cancel_at: None,
        };

        let compute_records = vec![
            ComputeUsageRecord {
                id: Uuid::new_v4(),
                customer_id: customer.id,
                model_name: "claude-3.5-sonnet".into(),
                input_tokens: 10_000_000,
                output_tokens: 5_000_000,
                cost_usd_cents: 10_000,
                charge_usd_cents: 12_000,
                customer_owned_model: false,
                timestamp: Utc::now(),
            },
        ];

        let summary = calculate_billing_summary(&customer, &subscription, &compute_records, true);

        assert_eq!(summary.subscription_charge_cents, 499_00); // Growth plan
        assert_eq!(summary.compute_charges_cents, 12_000);
        assert_eq!(summary.total_charges_cents, 499_00 + 12_000);
        assert!(summary.amos_discount_applied);

        // 20% discount on total (AMOS_DISCOUNT_BPS = 2000 = 20%)
        let expected_discount = ((499_00 + 12_000) as f64 * 0.20).round() as u64;
        assert_eq!(summary.discount_amount_cents, expected_discount);
        assert_eq!(summary.final_amount_cents, (499_00 + 12_000) - expected_discount);
    }

    #[test]
    fn billing_summary_free_plan() {
        let customer = Customer {
            id: Uuid::new_v4(),
            name: "Free User".into(),
            email: "free@example.com".into(),
            organization: None,
            plan: Plan::Free,
            created_at: Utc::now(),
            harness_id: None,
        };

        let subscription = Subscription {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            plan: Plan::Free,
            status: SubscriptionStatus::Active,
            started_at: Utc::now(),
            current_period_start: Utc::now(),
            current_period_end: Utc::now(),
            cancel_at: None,
        };

        let compute_records = vec![];

        let summary = calculate_billing_summary(&customer, &subscription, &compute_records, false);

        assert_eq!(summary.subscription_charge_cents, 0);
        assert_eq!(summary.compute_charges_cents, 0);
        assert_eq!(summary.total_charges_cents, 0);
        assert_eq!(summary.final_amount_cents, 0);
    }

    #[test]
    fn default_model_pricing_has_expected_models() {
        let pricing = default_model_pricing();

        assert_eq!(pricing.len(), 3);

        let qwen = pricing.iter().find(|p| p.model_name == "qwen3-next-80b");
        assert!(qwen.is_some());
        let qwen = qwen.unwrap();
        assert_eq!(qwen.input_rate_per_million, 0.20);
        assert_eq!(qwen.output_rate_per_million, 0.80);

        let haiku = pricing.iter().find(|p| p.model_name == "claude-3.5-haiku");
        assert!(haiku.is_some());
        let haiku = haiku.unwrap();
        assert_eq!(haiku.input_rate_per_million, 0.25);
        assert_eq!(haiku.output_rate_per_million, 1.25);

        let sonnet = pricing.iter().find(|p| p.model_name == "claude-3.5-sonnet");
        assert!(sonnet.is_some());
        let sonnet = sonnet.unwrap();
        assert_eq!(sonnet.input_rate_per_million, 3.00);
        assert_eq!(sonnet.output_rate_per_million, 15.00);
    }

    #[test]
    fn get_model_pricing_finds_existing_model() {
        let pricing = get_model_pricing("claude-3.5-sonnet");
        assert!(pricing.is_some());

        let pricing = pricing.unwrap();
        assert_eq!(pricing.model_name, "claude-3.5-sonnet");
        assert_eq!(pricing.input_rate_per_million, 3.00);
        assert_eq!(pricing.output_rate_per_million, 15.00);
    }

    #[test]
    fn get_model_pricing_returns_none_for_unknown_model() {
        let pricing = get_model_pricing("unknown-model");
        assert!(pricing.is_none());
    }

    #[test]
    fn end_to_end_billing_calculation() {
        // Create a customer using Qwen model
        let customer = Customer {
            id: Uuid::new_v4(),
            name: "E2E Test".into(),
            email: "e2e@example.com".into(),
            organization: Some("Test Org".into()),
            plan: Plan::Starter,
            created_at: Utc::now(),
            harness_id: Some("harness-123".into()),
        };

        let subscription = Subscription {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            plan: Plan::Starter,
            status: SubscriptionStatus::Active,
            started_at: Utc::now(),
            current_period_start: Utc::now(),
            current_period_end: Utc::now(),
            cancel_at: None,
        };

        // Get pricing and calculate costs
        let qwen_pricing = get_model_pricing("qwen3-next-80b").unwrap();

        // Simulate 10M input tokens, 5M output tokens
        let raw_cost = calculate_ai_cost(10_000_000, 5_000_000, &qwen_pricing);
        // Expected: (10M / 1M) * 0.20 + (5M / 1M) * 0.80 = 2.00 + 4.00 = $6.00 = 600 cents
        assert_eq!(raw_cost, 600);

        let customer_charge = calculate_customer_charge(raw_cost, false);
        // Expected: 600 * 1.20 = 720 cents
        assert_eq!(customer_charge, 720);

        // Create usage record
        let compute_records = vec![ComputeUsageRecord {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            model_name: "qwen3-next-80b".into(),
            input_tokens: 10_000_000,
            output_tokens: 5_000_000,
            cost_usd_cents: raw_cost,
            charge_usd_cents: customer_charge,
            customer_owned_model: false,
            timestamp: Utc::now(),
        }];

        // Calculate billing summary with AMOS discount
        let summary = calculate_billing_summary(&customer, &subscription, &compute_records, true);

        assert_eq!(summary.subscription_charge_cents, 99_00);
        assert_eq!(summary.compute_charges_cents, 720);
        assert_eq!(summary.total_charges_cents, 99_00 + 720);
        assert!(summary.amos_discount_applied);

        // 20% discount on 10620 cents = 2124 cents
        assert_eq!(summary.discount_amount_cents, 2124);
        assert_eq!(summary.final_amount_cents, 10620 - 2124);
    }
}
