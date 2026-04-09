//! Customer billing and subscription management.

pub mod stripe_service;

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

/// Account type for a tenant.
///
/// Two types: Free (self-hosted, BYOK, full features, 1 harness limit)
/// and Hosted (managed infrastructure, pay per harness).
/// No feature gates — both get everything. The difference is who runs infra.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    /// Free: self-hosted, full features, BYOK, 1 harness.
    Free,
    /// Hosted: managed infra, pay per harness ($45–$195/mo each).
    Hosted,
}

impl Plan {
    /// Default harness limits for the account type.
    pub fn limits(&self) -> PlanLimits {
        match self {
            Plan::Free => PlanLimits {
                max_harnesses: 1,
                support_level: "community".into(),
            },
            Plan::Hosted => PlanLimits {
                max_harnesses: 100, // effectively unlimited, real limit is billing
                support_level: "community".into(),
            },
        }
    }

    /// Whether this is a paid plan that requires Stripe checkout.
    pub fn is_paid(&self) -> bool {
        !matches!(self, Plan::Free)
    }

    /// Display name for the plan.
    pub fn display_name(&self) -> &'static str {
        match self {
            Plan::Free => "Free (Self-Hosted)",
            Plan::Hosted => "Hosted",
        }
    }

    /// Parse plan from string. Unknown values default to Free.
    pub fn from_str(s: &str) -> Self {
        match s {
            "hosted" => Plan::Hosted,
            // Legacy plan names map to Hosted for existing subscribers
            "starter" | "growth" | "enterprise" => Plan::Hosted,
            _ => Plan::Free,
        }
    }
}

/// Account-level limits for a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanLimits {
    pub max_harnesses: u64,
    pub support_level: String,
}

/// Per-harness instance size with pricing.
///
/// Each harness is billed independently based on its size.
/// Pricing: Small=$45/mo, Medium=$95/mo, Large=$195/mo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessSize {
    Small,  // 1 vCPU, 2 GB RAM, 10 GB storage
    Medium, // 2 vCPU, 4 GB RAM, 20 GB storage
    Large,  // 4 vCPU, 8 GB RAM, 50 GB storage
}

impl HarnessSize {
    /// Monthly price in cents (USD) per harness.
    pub fn monthly_price_cents(&self) -> u64 {
        match self {
            HarnessSize::Small => 4_500,
            HarnessSize::Medium => 9_500,
            HarnessSize::Large => 19_500,
        }
    }

    /// Monthly price as a formatted string.
    pub fn price_display(&self) -> &'static str {
        match self {
            HarnessSize::Small => "$45/mo",
            HarnessSize::Medium => "$95/mo",
            HarnessSize::Large => "$195/mo",
        }
    }

    /// Display name.
    pub fn display_name(&self) -> &'static str {
        match self {
            HarnessSize::Small => "Small",
            HarnessSize::Medium => "Medium",
            HarnessSize::Large => "Large",
        }
    }

    /// Resource specs for this size.
    pub fn resources(&self) -> HarnessResources {
        match self {
            HarnessSize::Small => HarnessResources {
                vcpu: 1,
                memory_gb: 2,
                storage_gb: 10,
            },
            HarnessSize::Medium => HarnessResources {
                vcpu: 2,
                memory_gb: 4,
                storage_gb: 20,
            },
            HarnessSize::Large => HarnessResources {
                vcpu: 4,
                memory_gb: 8,
                storage_gb: 50,
            },
        }
    }

    /// Parse from string. Defaults to Small.
    pub fn from_str(s: &str) -> Self {
        match s {
            "medium" => HarnessSize::Medium,
            "large" => HarnessSize::Large,
            _ => HarnessSize::Small,
        }
    }
}

/// Resource allocation for a harness instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessResources {
    pub vcpu: u64,
    pub memory_gb: u64,
    pub storage_gb: u64,
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
    /// Storage used in GB.
    pub storage_used_gb: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_paid() {
        assert!(!Plan::Free.is_paid());
        assert!(Plan::Hosted.is_paid());
    }

    #[test]
    fn legacy_plans_map_to_hosted() {
        assert_eq!(Plan::from_str("starter"), Plan::Hosted);
        assert_eq!(Plan::from_str("growth"), Plan::Hosted);
        assert_eq!(Plan::from_str("enterprise"), Plan::Hosted);
        assert_eq!(Plan::from_str("hosted"), Plan::Hosted);
        assert_eq!(Plan::from_str("free"), Plan::Free);
        assert_eq!(Plan::from_str("unknown"), Plan::Free);
    }

    #[test]
    fn free_plan_limits_one_harness() {
        let limits = Plan::Free.limits();
        assert_eq!(limits.max_harnesses, 1);
    }

    #[test]
    fn hosted_plan_allows_many_harnesses() {
        let limits = Plan::Hosted.limits();
        assert!(limits.max_harnesses > 1);
    }

    #[test]
    fn harness_size_pricing() {
        assert_eq!(HarnessSize::Small.monthly_price_cents(), 4_500);
        assert_eq!(HarnessSize::Medium.monthly_price_cents(), 9_500);
        assert_eq!(HarnessSize::Large.monthly_price_cents(), 19_500);
    }

    #[test]
    fn harness_size_resources_are_progressive() {
        let small = HarnessSize::Small.resources();
        let medium = HarnessSize::Medium.resources();
        let large = HarnessSize::Large.resources();

        assert!(small.vcpu < medium.vcpu);
        assert!(medium.vcpu < large.vcpu);
        assert!(small.memory_gb < medium.memory_gb);
        assert!(medium.memory_gb < large.memory_gb);
        assert!(small.storage_gb < medium.storage_gb);
        assert!(medium.storage_gb < large.storage_gb);
    }

    #[test]
    fn harness_size_from_str() {
        assert_eq!(HarnessSize::from_str("small"), HarnessSize::Small);
        assert_eq!(HarnessSize::from_str("medium"), HarnessSize::Medium);
        assert_eq!(HarnessSize::from_str("large"), HarnessSize::Large);
        assert_eq!(HarnessSize::from_str("unknown"), HarnessSize::Small);
    }

    #[test]
    fn harness_size_display() {
        assert_eq!(HarnessSize::Small.price_display(), "$45/mo");
        assert_eq!(HarnessSize::Medium.price_display(), "$95/mo");
        assert_eq!(HarnessSize::Large.price_display(), "$195/mo");
    }
}
