//! App-hosting pricing: tiered base + container-usage scaling.
//!
//! Hosted apps (e.g. Cuspr) are billed on a **tier** (isolation + an included
//! container pool + features) plus **usage** for containers beyond the pool.
//! The billing unit is a *standard container* (1 vCPU + 2 GB) — the same shape
//! the platform already deploys (an AppSpec's services → ECS tasks with known
//! cpu/mem), so usage meters itself from what's actually running rather than
//! needing separate instrumentation.
//!
//! Prices here are starting values and are meant to be tuned — the *structure*
//! (unit = container, tier includes a pool, overage per unit-month) is the
//! stable part. The economics anchor: a tenant paying ~$1,600/mo elsewhere for
//! a footprint that costs ~$200/mo to run lands on Pro at $349/mo — a large
//! saving for them and a healthy margin for AMOS, scaling with growth.

use serde::{Deserialize, Serialize};

/// A "standard container" billing unit = 1 vCPU + this many GB of memory.
pub const GB_PER_UNIT: f64 = 2.0;

/// App-hosting tier: isolation level + included capacity + overage rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppHostingTier {
    /// Shared data infrastructure; for small / first-time apps.
    Starter,
    /// Dedicated data (RDS/ElastiCache/S3) per tenant. Cuspr's tier.
    Pro,
    /// Dedicated VPC + peering + priority support.
    Compliance,
}

impl AppHostingTier {
    /// Flat monthly base price, in US cents.
    pub fn base_cents(&self) -> u64 {
        match self {
            AppHostingTier::Starter => 9_900,
            AppHostingTier::Pro => 34_900,
            AppHostingTier::Compliance => 89_900,
        }
    }

    /// Container units included in the base price.
    pub fn included_units(&self) -> u32 {
        match self {
            AppHostingTier::Starter => 2,
            AppHostingTier::Pro => 8,
            AppHostingTier::Compliance => 16,
        }
    }

    /// Per-unit-month overage cost beyond the included pool, in US cents.
    pub fn overage_cents_per_unit(&self) -> u64 {
        match self {
            AppHostingTier::Starter => 3_000,
            AppHostingTier::Pro => 2_500,
            AppHostingTier::Compliance => 2_000,
        }
    }

    /// Isolation level (drives the Terraform substrate that's provisioned).
    pub fn isolation(&self) -> &'static str {
        match self {
            AppHostingTier::Starter => "shared_data",
            AppHostingTier::Pro => "dedicated_data",
            AppHostingTier::Compliance => "dedicated_vpc",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            AppHostingTier::Starter => "Starter",
            AppHostingTier::Pro => "Pro",
            AppHostingTier::Compliance => "Compliance",
        }
    }

    /// Parse from a plan string (e.g. stored on the tenant).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "app_starter" | "starter" => Some(AppHostingTier::Starter),
            "app_pro" | "pro" => Some(AppHostingTier::Pro),
            "app_compliance" | "compliance" => Some(AppHostingTier::Compliance),
            _ => None,
        }
    }

    /// The canonical plan key stored on the tenant / used for the Stripe price.
    pub fn plan_key(&self) -> &'static str {
        match self {
            AppHostingTier::Starter => "app_starter",
            AppHostingTier::Pro => "app_pro",
            AppHostingTier::Compliance => "app_compliance",
        }
    }
}

/// Container units for one Fargate task, given its ECS CPU units (1024 = 1
/// vCPU) and memory in MiB. A unit is `max(vCPU, GB/2)`, rounded up, min 1 —
/// so a memory- or CPU-heavy task is charged by its dominant dimension.
pub fn container_units(cpu_units: u32, mem_mib: u32) -> u32 {
    let vcpu = (cpu_units as f64 / 1024.0).ceil();
    let gb = mem_mib as f64 / 1024.0;
    let mem_units = (gb / GB_PER_UNIT).ceil();
    vcpu.max(mem_units).max(1.0) as u32
}

/// A line-item breakdown of an app-hosting charge for a billing period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppHostingCharge {
    pub tier: AppHostingTier,
    pub units_deployed: u32,
    pub included_units: u32,
    pub overage_units: u32,
    pub base_cents: u64,
    pub overage_cents: u64,
    pub total_cents: u64,
}

/// Compute the monthly charge for a tier given total deployed container units.
pub fn compute_charge(tier: AppHostingTier, units_deployed: u32) -> AppHostingCharge {
    let included = tier.included_units();
    let overage_units = units_deployed.saturating_sub(included);
    let base_cents = tier.base_cents();
    let overage_cents = overage_units as u64 * tier.overage_cents_per_unit();
    AppHostingCharge {
        tier,
        units_deployed,
        included_units: included,
        overage_units,
        base_cents,
        overage_cents,
        total_cents: base_cents + overage_cents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuspr_task_is_eight_units() {
        // Cuspr's task: 4096 cpu units (4 vCPU), 16384 MiB (16 GB) -> 16/2 = 8.
        assert_eq!(container_units(4096, 16384), 8);
    }

    #[test]
    fn units_take_dominant_dimension() {
        // CPU-heavy: 4 vCPU, 4 GB -> max(4, 2) = 4.
        assert_eq!(container_units(4096, 4096), 4);
        // Memory-heavy: 1 vCPU, 8 GB -> max(1, 4) = 4.
        assert_eq!(container_units(1024, 8192), 4);
        // Tiny still bills as at least 1.
        assert_eq!(container_units(256, 512), 1);
    }

    #[test]
    fn cuspr_on_pro_is_fully_included() {
        let c = compute_charge(AppHostingTier::Pro, 8);
        assert_eq!(c.overage_units, 0);
        assert_eq!(c.total_cents, 34_900); // $349, no overage
    }

    #[test]
    fn scaling_adds_metered_overage() {
        // Cuspr scaled to 12 units on Pro: 4 over * $25 = $100 over $349 = $449.
        let c = compute_charge(AppHostingTier::Pro, 12);
        assert_eq!(c.overage_units, 4);
        assert_eq!(c.overage_cents, 10_000);
        assert_eq!(c.total_cents, 44_900);
    }

    #[test]
    fn tiers_parse_round_trip() {
        for t in [
            AppHostingTier::Starter,
            AppHostingTier::Pro,
            AppHostingTier::Compliance,
        ] {
            assert_eq!(AppHostingTier::parse(t.plan_key()), Some(t));
        }
        assert_eq!(AppHostingTier::parse("free"), None);
    }
}
