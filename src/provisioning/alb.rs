//! ALB-based subdomain routing for harness instances.
//!
//! When a harness ECS task reaches RUNNING and gets a private IP,
//! this module creates:
//!   1. An IP-based target group with the harness IP registered
//!   2. A host-based listener rule on the HTTPS listener that
//!      routes `{subdomain}.custom.amoslabs.com` to the target group
//!
//! On stop/deprovision, the rule and target group are cleaned up.

use aws_sdk_elasticloadbalancingv2::{
    types::{
        Action, ActionTypeEnum, ForwardActionConfig, RuleCondition, TargetDescription,
        TargetGroupTuple, TargetTypeEnum,
    },
    Client as ElbClient,
};

use amos_core::{AmosError, Result};
use tracing::{info, warn};

/// Manages ALB listener rules and target groups for harness subdomain routing.
pub struct AlbRouter {
    client: ElbClient,
    /// HTTPS listener ARN on the ALB.
    listener_arn: String,
    /// VPC ID for target group creation.
    vpc_id: String,
    /// Domain suffix, e.g. "custom.amoslabs.com".
    domain_suffix: String,
}

/// Configuration for the ALB router, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct AlbRouterConfig {
    pub listener_arn: String,
    pub vpc_id: String,
    pub domain_suffix: String,
    pub aws_region: String,
}

impl AlbRouterConfig {
    /// Load config from environment variables.
    ///
    /// Returns `None` if `ALB_HTTPS_LISTENER_ARN` is not set.
    pub fn from_env() -> Option<Self> {
        let listener_arn = std::env::var("ALB_HTTPS_LISTENER_ARN").ok()?;
        let vpc_id =
            std::env::var("ALB_VPC_ID").unwrap_or_else(|_| "vpc-004397889bd118cbc".to_string());
        let domain_suffix = std::env::var("ALB_DOMAIN_SUFFIX")
            .unwrap_or_else(|_| "custom.amoslabs.com".to_string());
        let aws_region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());

        Some(Self {
            listener_arn,
            vpc_id,
            domain_suffix,
            aws_region,
        })
    }
}

/// Result of setting up ALB routing for a harness.
#[derive(Debug, Clone)]
pub struct AlbRoutingResult {
    pub target_group_arn: String,
    pub listener_rule_arn: String,
    pub public_url: String,
}

impl AlbRouter {
    /// Create a new ALB router.
    pub async fn new(config: AlbRouterConfig) -> Result<Self> {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.aws_region.clone()))
            .load()
            .await;

        let client = ElbClient::new(&aws_config);

        info!(
            listener_arn = %config.listener_arn,
            vpc_id = %config.vpc_id,
            domain = %config.domain_suffix,
            "ALB router initialized"
        );

        Ok(Self {
            client,
            listener_arn: config.listener_arn,
            vpc_id: config.vpc_id,
            domain_suffix: config.domain_suffix,
        })
    }

    /// The public URL for a given subdomain.
    pub fn public_url(&self, subdomain: &str) -> String {
        format!("https://{}.{}", subdomain, self.domain_suffix)
    }

    /// Create a target group, register the harness IP, and create a listener rule.
    ///
    /// Returns the target group ARN, listener rule ARN, and public URL.
    pub async fn setup_routing(
        &self,
        subdomain: &str,
        private_ip: &str,
        port: u16,
    ) -> Result<AlbRoutingResult> {
        let tg_name = format!("harness-{}", truncate_for_tg_name(subdomain));

        // 1. Create IP-type target group
        let create_tg_result = self
            .client
            .create_target_group()
            .name(&tg_name)
            .protocol(aws_sdk_elasticloadbalancingv2::types::ProtocolEnum::Http)
            .port(port as i32)
            .vpc_id(&self.vpc_id)
            .target_type(TargetTypeEnum::Ip)
            .health_check_enabled(true)
            .health_check_path("/api/v1/health")
            .health_check_interval_seconds(30)
            .healthy_threshold_count(2)
            .unhealthy_threshold_count(3)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to create target group: {}", e)))?;

        let target_group_arn = create_tg_result
            .target_groups()
            .first()
            .and_then(|tg| tg.target_group_arn())
            .ok_or_else(|| AmosError::Internal("Target group ARN missing from response".into()))?
            .to_string();

        info!(tg_arn = %target_group_arn, name = %tg_name, "Target group created");

        // 2. Register the harness private IP as a target
        let register_result = self
            .client
            .register_targets()
            .target_group_arn(&target_group_arn)
            .targets(
                TargetDescription::builder()
                    .id(private_ip)
                    .port(port as i32)
                    .build(),
            )
            .send()
            .await;

        if let Err(e) = register_result {
            // Clean up the target group we just created
            let _ = self
                .client
                .delete_target_group()
                .target_group_arn(&target_group_arn)
                .send()
                .await;
            return Err(AmosError::Internal(format!(
                "Failed to register target: {}",
                e
            )));
        }

        info!(ip = %private_ip, port = port, "Target registered");

        // 3. Find the next available priority for the listener rule.
        //    We start at 10 and skip any that are taken.
        let priority = self.next_available_priority().await?;

        // 4. Create host-based listener rule
        let host_header = format!("{}.{}", subdomain, self.domain_suffix);

        let create_rule_result = self
            .client
            .create_rule()
            .listener_arn(&self.listener_arn)
            .priority(priority)
            .conditions(
                RuleCondition::builder()
                    .field("host-header")
                    .values(&host_header)
                    .build(),
            )
            .actions(
                Action::builder()
                    .r#type(ActionTypeEnum::Forward)
                    .forward_config(
                        ForwardActionConfig::builder()
                            .target_groups(
                                TargetGroupTuple::builder()
                                    .target_group_arn(&target_group_arn)
                                    .weight(1)
                                    .build(),
                            )
                            .build(),
                    )
                    .build(),
            )
            .send()
            .await;

        match create_rule_result {
            Ok(result) => {
                let listener_rule_arn = result
                    .rules()
                    .first()
                    .and_then(|r| r.rule_arn())
                    .ok_or_else(|| {
                        AmosError::Internal("Listener rule ARN missing from response".into())
                    })?
                    .to_string();

                let public_url = self.public_url(subdomain);

                info!(
                    rule_arn = %listener_rule_arn,
                    host = %host_header,
                    priority = priority,
                    url = %public_url,
                    "ALB listener rule created"
                );

                Ok(AlbRoutingResult {
                    target_group_arn,
                    listener_rule_arn,
                    public_url,
                })
            }
            Err(e) => {
                // Clean up target group on failure
                warn!("Failed to create listener rule, cleaning up target group");
                let _ = self
                    .client
                    .delete_target_group()
                    .target_group_arn(&target_group_arn)
                    .send()
                    .await;
                Err(AmosError::Internal(format!(
                    "Failed to create listener rule: {}",
                    e
                )))
            }
        }
    }

    /// Remove the listener rule and delete the target group.
    pub async fn teardown_routing(
        &self,
        listener_rule_arn: Option<&str>,
        target_group_arn: Option<&str>,
    ) {
        // Delete listener rule first (must be removed before target group).
        if let Some(rule_arn) = listener_rule_arn {
            match self.client.delete_rule().rule_arn(rule_arn).send().await {
                Ok(_) => info!(rule_arn = %rule_arn, "ALB listener rule deleted"),
                Err(e) => warn!(rule_arn = %rule_arn, "Failed to delete listener rule: {}", e),
            }
        }

        // Delete target group.
        if let Some(tg_arn) = target_group_arn {
            // Small delay to allow rule deletion to propagate
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            match self
                .client
                .delete_target_group()
                .target_group_arn(tg_arn)
                .send()
                .await
            {
                Ok(_) => info!(tg_arn = %tg_arn, "Target group deleted"),
                Err(e) => warn!(tg_arn = %tg_arn, "Failed to delete target group: {}", e),
            }
        }
    }

    /// Update the target IP in an existing target group (for redeploy).
    pub async fn update_target(
        &self,
        target_group_arn: &str,
        new_ip: &str,
        port: u16,
    ) -> Result<()> {
        // Deregister all current targets
        let describe = self
            .client
            .describe_target_health()
            .target_group_arn(target_group_arn)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to describe target health: {}", e)))?;

        for thd in describe.target_health_descriptions() {
            if let Some(target) = thd.target() {
                let _ = self
                    .client
                    .deregister_targets()
                    .target_group_arn(target_group_arn)
                    .targets(
                        TargetDescription::builder()
                            .id(target.id().unwrap_or_default())
                            .set_port(target.port())
                            .build(),
                    )
                    .send()
                    .await;
            }
        }

        // Register the new target
        self.client
            .register_targets()
            .target_group_arn(target_group_arn)
            .targets(
                TargetDescription::builder()
                    .id(new_ip)
                    .port(port as i32)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to register new target: {}", e)))?;

        info!(tg_arn = %target_group_arn, ip = %new_ip, "Target updated");
        Ok(())
    }

    /// Find the next available priority number for a listener rule.
    async fn next_available_priority(&self) -> Result<i32> {
        let rules = self
            .client
            .describe_rules()
            .listener_arn(&self.listener_arn)
            .send()
            .await
            .map_err(|e| {
                AmosError::Internal(format!("Failed to describe listener rules: {}", e))
            })?;

        let mut used_priorities: Vec<i32> = rules
            .rules()
            .iter()
            .filter_map(|r| r.priority().and_then(|p| p.parse::<i32>().ok()))
            .collect();

        used_priorities.sort();

        // Start at priority 10 (leave 1-9 for manual rules), go up to 50000.
        for candidate in 10..50_000 {
            if !used_priorities.contains(&candidate) {
                return Ok(candidate);
            }
        }

        Err(AmosError::Internal(
            "No available listener rule priorities".into(),
        ))
    }
}

/// Truncate a subdomain to fit ALB target group name constraints (max 32 chars).
/// Target group names must be <= 32 characters, start with alphanumeric, and
/// contain only alphanumeric and hyphens.
fn truncate_for_tg_name(subdomain: &str) -> String {
    let clean: String = subdomain
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect();

    // "harness-" prefix is 8 chars, so we have 24 chars for the subdomain part.
    if clean.len() > 24 {
        clean[..24].to_string()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_for_tg_name_short() {
        assert_eq!(truncate_for_tg_name("test-corp"), "test-corp");
    }

    #[test]
    fn truncate_for_tg_name_long() {
        let long = "this-is-a-very-long-subdomain-name-that-exceeds-the-limit";
        let result = truncate_for_tg_name(long);
        assert!(result.len() <= 24);
    }

    #[test]
    fn truncate_strips_invalid_chars() {
        assert_eq!(truncate_for_tg_name("test_corp.foo"), "testcorpfoo");
    }

    #[test]
    fn config_from_env_disabled_without_listener() {
        std::env::remove_var("ALB_HTTPS_LISTENER_ARN");
        assert!(AlbRouterConfig::from_env().is_none());
    }
}
