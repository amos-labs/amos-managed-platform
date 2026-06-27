//! Exact usage metering for app-hosting billing, from CloudWatch Container
//! Insights.
//!
//! Two dimensions, both metered per ECS *service* over a billing period:
//! - **unit-hours**: integrate `max(vCPU, GB/2)` from hourly `CpuReserved` /
//!   `MemoryReserved` samples. Reserved (provisioned) capacity is what we bill;
//!   hours with no running tasks have no datapoint and contribute 0, so
//!   scale-to-zero and autoscaling are metered precisely.
//! - **egress GB**: sum `NetworkTxBytes` (data transmitted out of the tasks)
//!   over the period.
//!
//! Container Insights must be enabled on the cluster (it is on
//! `swarm-infrastructure-cluster`). Service names + cluster come from each
//! deployment's `aws_meta`.

use super::app_hosting::{compute_period_charge, AppHostingPeriodCharge, AppHostingTier};
use aws_sdk_cloudwatch::types::Dimension;
use aws_sdk_cloudwatch::Client as CwClient;
use chrono::{DateTime, Utc};
use tracing::warn;

/// A tenant's exact, metered app-hosting position for a billing period.
#[derive(Debug, Clone)]
pub struct TenantPeriodSummary {
    pub plan: String,
    pub tier: Option<AppHostingTier>,
    pub usage: ServiceUsage,
    pub charge: Option<AppHostingPeriodCharge>,
    /// Egress soft-cap status vs the tier's included allowance. `alert` flips
    /// at 80% so the tenant can be warned *before* incurring overage. `None`
    /// when not on an app-hosting tier (no allowance to measure against).
    pub egress_cap: Option<crate::billing::guards::CapStatus>,
}

const INSIGHTS_NAMESPACE: &str = "ECS/ContainerInsights";

/// Metered usage for one service (or a tenant, when summed) over a period.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceUsage {
    /// Integrated container-unit-hours (compute) over the period.
    pub unit_hours: f64,
    /// Egress (data transmitted out) over the period, in GB.
    pub egress_gb: f64,
}

impl ServiceUsage {
    fn add(&mut self, other: &ServiceUsage) {
        self.unit_hours += other.unit_hours;
        self.egress_gb += other.egress_gb;
    }
}

/// Reduce hourly reserved-capacity samples to container-unit-hours.
///
/// Each sample is `(cpu_reserved_units, mem_reserved_mib)` averaged over one
/// hour (1024 cpu units = 1 vCPU). A unit is `max(vCPU, GB/2)`; summing the
/// per-hour units over the hours gives unit-hours. Pulled out as a pure fn so
/// the integration is unit-testable without CloudWatch.
pub fn unit_hours_from_hourly_samples(samples: &[(f64, f64)], hours_per_sample: f64) -> f64 {
    samples
        .iter()
        .map(|(cpu_units, mem_mib)| {
            let vcpu = cpu_units / 1024.0;
            let gb_over_2 = mem_mib / 2048.0; // (mem_mib/1024) / 2
            vcpu.max(gb_over_2) * hours_per_sample
        })
        .sum()
}

/// CloudWatch-backed meter for a single ECS cluster.
#[derive(Clone)]
pub struct UsageMeter {
    client: CwClient,
    cluster: String,
}

impl UsageMeter {
    /// Build a meter from loaded AWS config, for the given ECS cluster.
    pub fn new(conf: &aws_config::SdkConfig, cluster: impl Into<String>) -> Self {
        Self {
            client: CwClient::new(conf),
            cluster: cluster.into(),
        }
    }

    /// A tenant's exact metered position for `[start, end]`: its tier × the
    /// unit-hours + egress actually used, run through [`compute_period_charge`].
    pub async fn tenant_period_summary(
        &self,
        pool: &sqlx::PgPool,
        tenant_id: uuid::Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> TenantPeriodSummary {
        let plan: String = sqlx::query_scalar("SELECT plan FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let tier = AppHostingTier::parse(&plan);
        let usage = self.meter_tenant(pool, tenant_id, start, end).await;
        let period_hours = (end - start).num_seconds().max(0) as f64 / 3600.0;
        let charge =
            tier.map(|t| compute_period_charge(t, usage.unit_hours, usage.egress_gb, period_hours));
        let egress_cap = tier.map(|t| {
            crate::billing::guards::cap_status(usage.egress_gb, t.included_egress_gb() as f64)
        });
        TenantPeriodSummary {
            plan,
            tier,
            usage,
            charge,
            egress_cap,
        }
    }

    /// Meter a tenant's total compute + egress over `[start, end]`, summed
    /// across the ECS services backing its running app deployments. The ECS
    /// service name comes from each deployment's `aws_meta.service`, falling
    /// back to the deployment `name`.
    pub async fn meter_tenant(
        &self,
        pool: &sqlx::PgPool,
        tenant_id: uuid::Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> ServiceUsage {
        let rows = sqlx::query_as::<_, (String, Option<serde_json::Value>)>(
            "SELECT DISTINCT ON (name) name, aws_meta
             FROM app_deployments
             WHERE tenant_id = $1 AND provider = 'ecs' AND status = 'running'
             ORDER BY name, created_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let mut total = ServiceUsage::default();
        for (name, meta) in rows {
            let service = meta
                .as_ref()
                .and_then(|m| m.get("service"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or(name);
            let usage = self.meter_service(&service, start, end).await;
            total.add(&usage);
        }
        total
    }

    /// Meter one service's compute + egress over `[start, end]`.
    pub async fn meter_service(
        &self,
        service: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> ServiceUsage {
        let unit_hours = self.unit_hours(service, start, end).await;
        let egress_gb = self.egress_gb(service, start, end).await;
        ServiceUsage {
            unit_hours,
            egress_gb,
        }
    }

    /// Integrate compute unit-hours from hourly reserved-capacity samples.
    async fn unit_hours(&self, service: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
        let cpu = self
            .hourly_average(service, "CpuReserved", start, end)
            .await;
        let mem = self
            .hourly_average(service, "MemoryReserved", start, end)
            .await;
        // Pair samples by timestamp; a missing dimension in an hour -> 0 there.
        let samples: Vec<(f64, f64)> = cpu
            .iter()
            .map(|(ts, c)| {
                let m = mem
                    .iter()
                    .find(|(mts, _)| mts == ts)
                    .map(|(_, v)| *v)
                    .unwrap_or(0.0);
                (*c, m)
            })
            .collect();
        unit_hours_from_hourly_samples(&samples, 1.0)
    }

    /// Hourly `Average` datapoints for a Container Insights service metric,
    /// returned as `(timestamp_secs, value)` pairs.
    async fn hourly_average(
        &self,
        service: &str,
        metric: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<(i64, f64)> {
        let resp = self
            .client
            .get_metric_statistics()
            .namespace(INSIGHTS_NAMESPACE)
            .metric_name(metric)
            .dimensions(dim("ServiceName", service))
            .dimensions(dim("ClusterName", &self.cluster))
            .start_time(to_aws(start))
            .end_time(to_aws(end))
            .period(3600)
            .statistics(aws_sdk_cloudwatch::types::Statistic::Average)
            .send()
            .await;
        match resp {
            Ok(o) => o
                .datapoints()
                .iter()
                .filter_map(|d| Some((d.timestamp()?.secs(), d.average()?)))
                .collect(),
            Err(e) => {
                warn!(service, metric, "metric query failed: {e}");
                Vec::new()
            }
        }
    }

    /// Sum `NetworkTxBytes` over the period and convert to GB.
    async fn egress_gb(&self, service: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
        // Period must cover the whole window in <=1440 datapoints; use daily
        // buckets and Sum, then total.
        let resp = self
            .client
            .get_metric_statistics()
            .namespace(INSIGHTS_NAMESPACE)
            .metric_name("NetworkTxBytes")
            .dimensions(dim("ServiceName", service))
            .dimensions(dim("ClusterName", &self.cluster))
            .start_time(to_aws(start))
            .end_time(to_aws(end))
            .period(86_400)
            .statistics(aws_sdk_cloudwatch::types::Statistic::Sum)
            .send()
            .await;
        let bytes: f64 = match resp {
            Ok(o) => o.datapoints().iter().filter_map(|d| d.sum()).sum(),
            Err(e) => {
                warn!(service, "egress query failed: {e}");
                0.0
            }
        };
        bytes / 1_000_000_000.0
    }
}

fn dim(name: &str, value: &str) -> Dimension {
    Dimension::builder().name(name).value(value).build()
}

fn to_aws(dt: DateTime<Utc>) -> aws_sdk_cloudwatch::primitives::DateTime {
    aws_sdk_cloudwatch::primitives::DateTime::from_secs(dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_hours_picks_dominant_dimension() {
        // Cuspr: 4 vCPU (4096 cpu units), 16 GB (16384 MiB) -> max(4, 8) = 8.
        let samples = vec![(4096.0, 16384.0); 3]; // 3 hours
        assert_eq!(unit_hours_from_hourly_samples(&samples, 1.0), 24.0);
    }

    #[test]
    fn unit_hours_cpu_heavy() {
        // 4 vCPU, 4 GB -> max(4, 1) = 4, over 2 hours = 8.
        let samples = vec![(4096.0, 4096.0); 2];
        assert_eq!(unit_hours_from_hourly_samples(&samples, 1.0), 8.0);
    }

    #[test]
    fn unit_hours_empty_is_zero() {
        // No datapoints (scaled to zero all period) -> 0 unit-hours.
        assert_eq!(unit_hours_from_hourly_samples(&[], 1.0), 0.0);
    }

    #[test]
    fn service_usage_sums() {
        let mut a = ServiceUsage {
            unit_hours: 10.0,
            egress_gb: 1.0,
        };
        a.add(&ServiceUsage {
            unit_hours: 5.0,
            egress_gb: 2.0,
        });
        assert_eq!(a.unit_hours, 15.0);
        assert_eq!(a.egress_gb, 3.0);
    }
}
