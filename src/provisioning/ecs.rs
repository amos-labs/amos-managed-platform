//! ECS Fargate-based harness provisioning for production.
//!
//! When running in AWS (no Docker socket available), the platform
//! provisions harness containers as ECS Fargate tasks instead of
//! local Docker containers.

use aws_sdk_ecs::{
    types::{
        AssignPublicIp, AwsVpcConfiguration, Compatibility, ContainerDefinition, KeyValuePair,
        LaunchType, LogConfiguration, LogDriver, NetworkConfiguration, NetworkMode, PortMapping,
        TransportProtocol,
    },
    Client as EcsClient,
};

use amos_core::{AmosError, Result};
use tracing::info;

use super::{HarnessConfig, HarnessStatus, InstanceSize};

/// ECS Fargate-based harness provisioner.
pub struct EcsProvisioner {
    client: EcsClient,
    cluster: String,
    harness_image: String,
    /// Optional agent sidecar image. When set, the agent container is added
    /// to the ECS task definition alongside the harness container.
    agent_image: Option<String>,
    subnets: Vec<String>,
    security_groups: Vec<String>,
    execution_role_arn: String,
    task_role_arn: String,
    log_group: String,
    aws_region: String,
    /// Database URL for harness containers (shared DB for now).
    harness_database_url: String,
    /// Redis URL for harness containers.
    harness_redis_url: String,
    /// Platform URL that harness containers use for sync/heartbeat.
    platform_url: String,
    /// Vault master key for credential encryption (base64-encoded 32 bytes).
    vault_master_key: String,
    /// Brave Search API key for web search tools.
    brave_api_key: String,
}

/// Configuration for the ECS provisioner, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct EcsProvisionerConfig {
    pub cluster: String,
    pub harness_image: String,
    /// Docker image for the agent sidecar (e.g., `637423327454.dkr.ecr.us-east-1.amazonaws.com/amos-agent:latest`).
    /// If not set, harness tasks are deployed without an agent sidecar.
    pub agent_image: Option<String>,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub execution_role_arn: String,
    pub task_role_arn: String,
    pub log_group: String,
    pub aws_region: String,
    pub harness_database_url: String,
    pub harness_redis_url: String,
    pub platform_url: String,
    pub vault_master_key: String,
    pub brave_api_key: String,
}

impl EcsProvisionerConfig {
    /// Load config from environment variables.
    ///
    /// Returns `None` if `ECS_HARNESS_IMAGE` is not set, which means
    /// ECS provisioning is not configured (i.e., local dev mode).
    pub fn from_env() -> Option<Self> {
        // ECS_HARNESS_IMAGE is the gate: if not set, ECS provisioning is disabled.
        let harness_image = std::env::var("ECS_HARNESS_IMAGE").ok()?;

        let cluster = std::env::var("ECS_CLUSTER")
            .unwrap_or_else(|_| "swarm-infrastructure-cluster".to_string());

        let subnets = std::env::var("ECS_SUBNETS")
            .unwrap_or_else(|_| "subnet-0cfcbfd1c0ef057e7,subnet-0c5928d36478a1be4".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let security_groups = std::env::var("ECS_SECURITY_GROUPS")
            .unwrap_or_else(|_| "sg-0967e26d543a5ce47".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let execution_role_arn = std::env::var("ECS_EXECUTION_ROLE_ARN").unwrap_or_else(|_| {
            "arn:aws:iam::637423327454:role/swarm-infrastructure-ecs-execution-role".to_string()
        });

        let task_role_arn = std::env::var("ECS_TASK_ROLE_ARN").unwrap_or_else(|_| {
            "arn:aws:iam::637423327454:role/swarm-infrastructure-rails-task-role".to_string()
        });

        let log_group = std::env::var("ECS_HARNESS_LOG_GROUP")
            .unwrap_or_else(|_| "/ecs/amos-harness".to_string());

        let aws_region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());

        let harness_database_url = std::env::var("ECS_HARNESS_DATABASE_URL").unwrap_or_default();

        let harness_redis_url = std::env::var("ECS_HARNESS_REDIS_URL")
            .or_else(|_| std::env::var("AMOS__REDIS__URL"))
            .unwrap_or_else(|_| "redis://localhost:6379/2".to_string());

        let platform_url = std::env::var("ECS_PLATFORM_URL")
            .unwrap_or_else(|_| "https://app.amoslabs.com".to_string());

        let agent_image = std::env::var("ECS_AGENT_IMAGE").ok();

        let vault_master_key = std::env::var("ECS_HARNESS_VAULT_MASTER_KEY")
            .or_else(|_| std::env::var("AMOS__VAULT__MASTER_KEY"))
            .unwrap_or_default();

        let brave_api_key = std::env::var("ECS_HARNESS_BRAVE_API_KEY")
            .or_else(|_| std::env::var("BRAVE_API_KEY"))
            .unwrap_or_default();

        Some(Self {
            cluster,
            harness_image,
            agent_image,
            subnets,
            security_groups,
            execution_role_arn,
            task_role_arn,
            log_group,
            aws_region,
            harness_database_url,
            harness_redis_url,
            platform_url,
            vault_master_key,
            brave_api_key,
        })
    }
}

impl EcsProvisioner {
    /// Create a new ECS provisioner and verify connectivity.
    pub async fn new(config: EcsProvisionerConfig) -> Result<Self> {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.aws_region.clone()))
            .load()
            .await;

        let client = EcsClient::new(&aws_config);

        // Verify connectivity by describing the cluster.
        let result = client
            .describe_clusters()
            .clusters(&config.cluster)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to connect to ECS: {}", e)))?;

        let active = result
            .clusters()
            .iter()
            .any(|c| c.status() == Some("ACTIVE"));

        if !active {
            return Err(AmosError::Internal(format!(
                "ECS cluster '{}' not found or not active",
                config.cluster
            )));
        }

        info!(
            cluster = %config.cluster,
            image = %config.harness_image,
            agent_image = ?config.agent_image,
            "ECS provisioner connected"
        );

        Ok(Self {
            client,
            cluster: config.cluster,
            harness_image: config.harness_image,
            agent_image: config.agent_image,
            subnets: config.subnets,
            security_groups: config.security_groups,
            execution_role_arn: config.execution_role_arn,
            task_role_arn: config.task_role_arn,
            log_group: config.log_group,
            aws_region: config.aws_region,
            harness_database_url: config.harness_database_url,
            harness_redis_url: config.harness_redis_url,
            platform_url: config.platform_url,
            vault_master_key: config.vault_master_key,
            brave_api_key: config.brave_api_key,
        })
    }

    /// Provision a new harness as an ECS Fargate task using the default images
    /// configured on this provisioner.
    ///
    /// Returns the ECS task ARN (stored as `container_id` in the DB).
    pub async fn provision(&self, config: &HarnessConfig, tenant_slug: &str) -> Result<String> {
        self.provision_inner(
            config,
            tenant_slug,
            &self.harness_image,
            self.agent_image.as_deref(),
        )
        .await
    }

    /// Provision a new harness as an ECS Fargate task with caller-provided images.
    ///
    /// Used by the update/rollback flow to deploy a specific release version.
    pub async fn provision_with_images(
        &self,
        config: &HarnessConfig,
        tenant_slug: &str,
        harness_image: &str,
        agent_image: Option<&str>,
    ) -> Result<String> {
        self.provision_inner(config, tenant_slug, harness_image, agent_image)
            .await
    }

    /// Inner provisioning logic shared by `provision()` and `provision_with_images()`.
    async fn provision_inner(
        &self,
        config: &HarnessConfig,
        tenant_slug: &str,
        harness_image: &str,
        agent_image: Option<&str>,
    ) -> Result<String> {
        let family = format!("amos-harness-{}", tenant_slug);

        // If agent sidecar is configured, bump resources to accommodate both containers.
        let (cpu, memory) = if agent_image.is_some() {
            fargate_resources_with_agent(config.instance_size)
        } else {
            fargate_resources(config.instance_size)
        };

        // Build environment variables for the harness container.
        let mut env_vars = vec![
            kv("CUSTOMER_ID", &config.customer_id.to_string()),
            kv("AMOS_ENV", &config.environment),
            kv("AMOS__SERVER__PORT", "3000"),
            kv("AMOS__SERVER__HOST", "0.0.0.0"),
            kv("AMOS__PLATFORM__URL", &self.platform_url),
            kv("RUST_LOG", "info,amos_harness=debug"),
            kv("RUST_BACKTRACE", "1"),
        ];

        // Tell harness where the agent sidecar is (same task = localhost).
        if agent_image.is_some() {
            env_vars.push(kv("AGENT_URL", "http://localhost:3100"));
        }

        if !self.harness_database_url.is_empty() {
            env_vars.push(kv("AMOS__DATABASE__URL", &self.harness_database_url));
        }
        if !self.harness_redis_url.is_empty() {
            env_vars.push(kv("AMOS__REDIS__URL", &self.harness_redis_url));
        }
        if !self.vault_master_key.is_empty() {
            env_vars.push(kv("AMOS__VAULT__MASTER_KEY", &self.vault_master_key));
        }
        if !self.brave_api_key.is_empty() {
            env_vars.push(kv("BRAVE_API_KEY", &self.brave_api_key));
        }

        // Multi-harness: pass role, packages, and harness ID
        env_vars.push(kv("AMOS_HARNESS_ROLE", &config.harness_role));
        if !config.packages.is_empty() {
            env_vars.push(kv("AMOS_PACKAGES", &config.packages.join(",")));
        }
        if let Some(harness_id) = &config.harness_id {
            env_vars.push(kv("AMOS_HARNESS_ID", &harness_id.to_string()));
        }

        // Forward any extra env vars from HarnessConfig.
        for (key, value) in &config.env_vars {
            env_vars.push(kv(key, value));
        }

        // Build log configuration for harness container.
        let mut harness_log_options = std::collections::HashMap::new();
        harness_log_options.insert("awslogs-group".to_string(), self.log_group.clone());
        harness_log_options.insert("awslogs-region".to_string(), self.aws_region.clone());
        harness_log_options.insert(
            "awslogs-stream-prefix".to_string(),
            format!("{}-harness", tenant_slug),
        );

        // Build harness container definition.
        let harness_container = ContainerDefinition::builder()
            .name("amos-harness")
            .image(harness_image)
            .essential(true)
            .port_mappings(
                PortMapping::builder()
                    .container_port(3000)
                    .host_port(3000)
                    .protocol(TransportProtocol::Tcp)
                    .build(),
            )
            .set_environment(Some(env_vars))
            .log_configuration(
                LogConfiguration::builder()
                    .log_driver(LogDriver::Awslogs)
                    .set_options(Some(harness_log_options))
                    .build()
                    .map_err(|e| {
                        AmosError::Internal(format!("Failed to build log config: {}", e))
                    })?,
            )
            .build();

        // Register a task definition for this tenant.
        let mut task_def_builder = self
            .client
            .register_task_definition()
            .family(&family)
            .requires_compatibilities(Compatibility::Fargate)
            .network_mode(NetworkMode::Awsvpc)
            .cpu(cpu)
            .memory(memory)
            .execution_role_arn(&self.execution_role_arn)
            .task_role_arn(&self.task_role_arn)
            .container_definitions(harness_container);

        // Add agent sidecar container if configured.
        if let Some(agent_image) = agent_image {
            let mut agent_log_options = std::collections::HashMap::new();
            agent_log_options.insert("awslogs-group".to_string(), self.log_group.clone());
            agent_log_options.insert("awslogs-region".to_string(), self.aws_region.clone());
            agent_log_options.insert(
                "awslogs-stream-prefix".to_string(),
                format!("{}-agent", tenant_slug),
            );

            let mut agent_env_vars = vec![
                kv("HARNESS_URL", "http://localhost:3000"),
                kv("AGENT_PORT", "3100"),
                kv("RUST_LOG", "info,amos_agent=debug"),
                kv("RUST_BACKTRACE", "1"),
                kv("AWS_REGION", &self.aws_region),
            ];
            if !self.brave_api_key.is_empty() {
                agent_env_vars.push(kv("BRAVE_API_KEY", &self.brave_api_key));
            }

            let agent_container = ContainerDefinition::builder()
                .name("amos-agent")
                .image(agent_image)
                .essential(false) // Agent crash shouldn't kill the harness
                .port_mappings(
                    PortMapping::builder()
                        .container_port(3100)
                        .host_port(3100)
                        .protocol(TransportProtocol::Tcp)
                        .build(),
                )
                .set_environment(Some(agent_env_vars))
                .log_configuration(
                    LogConfiguration::builder()
                        .log_driver(LogDriver::Awslogs)
                        .set_options(Some(agent_log_options))
                        .build()
                        .map_err(|e| {
                            AmosError::Internal(format!("Failed to build agent log config: {}", e))
                        })?,
                )
                .build();

            task_def_builder = task_def_builder.container_definitions(agent_container);

            info!(agent_image = %agent_image, "Including agent sidecar in task definition");
        }

        let task_def_result = task_def_builder.send().await.map_err(|e| {
            AmosError::Internal(format!("Failed to register task definition: {}", e))
        })?;

        let task_def_arn = task_def_result
            .task_definition()
            .and_then(|td| td.task_definition_arn())
            .ok_or_else(|| AmosError::Internal("Task definition ARN missing from response".into()))?
            .to_string();

        info!(family = %family, arn = %task_def_arn, "Task definition registered");

        // Run the task.
        let run_result = self
            .client
            .run_task()
            .cluster(&self.cluster)
            .task_definition(&task_def_arn)
            .launch_type(LaunchType::Fargate)
            .count(1)
            .network_configuration(
                NetworkConfiguration::builder()
                    .awsvpc_configuration(
                        AwsVpcConfiguration::builder()
                            .set_subnets(Some(self.subnets.clone()))
                            .set_security_groups(Some(self.security_groups.clone()))
                            .assign_public_ip(AssignPublicIp::Enabled)
                            .build()
                            .map_err(|e| {
                                AmosError::Internal(format!("Failed to build VPC config: {}", e))
                            })?,
                    )
                    .build(),
            )
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to run ECS task: {}", e)))?;

        let task_arn = run_result
            .tasks()
            .first()
            .and_then(|t| t.task_arn())
            .ok_or_else(|| {
                let reason = run_result
                    .failures()
                    .first()
                    .map(|f| format!("{}: {}", f.arn().unwrap_or("?"), f.reason().unwrap_or("?")))
                    .unwrap_or_else(|| "Unknown failure".to_string());
                AmosError::Internal(format!("ECS task failed to start: {}", reason))
            })?
            .to_string();

        info!(task_arn = %task_arn, family = %family, "ECS task launched");

        Ok(task_arn)
    }

    /// Describe a task and return its status and private IP.
    pub async fn describe_task(&self, task_arn: &str) -> Result<(HarnessStatus, Option<String>)> {
        let result = self
            .client
            .describe_tasks()
            .cluster(&self.cluster)
            .tasks(task_arn)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to describe task: {}", e)))?;

        let task = result
            .tasks()
            .first()
            .ok_or_else(|| AmosError::Internal("Task not found in ECS".into()))?;

        let status = match task.last_status() {
            Some("RUNNING") => HarnessStatus::Running,
            Some("PROVISIONING") | Some("PENDING") | Some("ACTIVATING") => {
                HarnessStatus::Provisioning
            }
            Some("STOPPED") | Some("DEACTIVATING") | Some("STOPPING") => {
                // Check stop code for error vs normal stop.
                let is_error = task
                    .stop_code()
                    .map(|c| {
                        c.as_str() == "TaskFailedToStart"
                            || c.as_str() == "EssentialContainerExited"
                    })
                    .unwrap_or(false);
                if is_error {
                    HarnessStatus::Error
                } else {
                    HarnessStatus::Stopped
                }
            }
            _ => HarnessStatus::Provisioning,
        };

        // Extract private IP from ENI attachment details.
        let private_ip = task
            .attachments()
            .iter()
            .filter(|a| a.r#type() == Some("ElasticNetworkInterface"))
            .flat_map(|a| a.details())
            .find(|d| d.name() == Some("privateIPv4Address"))
            .and_then(|d| d.value().map(|v| v.to_string()));

        Ok((status, private_ip))
    }

    /// Stop a running ECS task.
    pub async fn stop(&self, task_arn: &str) -> Result<()> {
        self.client
            .stop_task()
            .cluster(&self.cluster)
            .task(task_arn)
            .reason("Stopped by AMOS Platform")
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to stop ECS task: {}", e)))?;

        info!(task_arn = %task_arn, "ECS task stopped");
        Ok(())
    }

    /// Get logs hint — ECS logs are in CloudWatch, not fetchable inline.
    pub fn logs_hint(&self, tenant_slug: &str) -> String {
        format!(
            "Logs are in CloudWatch: {} (stream prefix: {})",
            self.log_group, tenant_slug
        )
    }
}

/// Map `InstanceSize` to valid Fargate CPU/memory strings (harness only).
fn fargate_resources(size: InstanceSize) -> (&'static str, &'static str) {
    match size {
        InstanceSize::Small => ("512", "1024"),   // 0.5 vCPU, 1 GB
        InstanceSize::Medium => ("1024", "2048"), // 1 vCPU, 2 GB
        InstanceSize::Large => ("2048", "4096"),  // 2 vCPU, 4 GB
    }
}

/// Map `InstanceSize` to valid Fargate CPU/memory strings when running
/// harness + agent sidecar. Both containers share the task's CPU/memory,
/// so we bump each tier up to accommodate the agent.
fn fargate_resources_with_agent(size: InstanceSize) -> (&'static str, &'static str) {
    match size {
        InstanceSize::Small => ("1024", "2048"),  // 1 vCPU, 2 GB
        InstanceSize::Medium => ("2048", "4096"), // 2 vCPU, 4 GB
        InstanceSize::Large => ("4096", "8192"),  // 4 vCPU, 8 GB
    }
}

/// Helper to build a `KeyValuePair` for ECS container environment.
fn kv(name: &str, value: &str) -> KeyValuePair {
    KeyValuePair::builder().name(name).value(value).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fargate_resources_valid_combos() {
        // Verify all combos are valid Fargate CPU/memory pairs.
        assert_eq!(fargate_resources(InstanceSize::Small), ("512", "1024"));
        assert_eq!(fargate_resources(InstanceSize::Medium), ("1024", "2048"));
        assert_eq!(fargate_resources(InstanceSize::Large), ("2048", "4096"));
    }

    #[test]
    fn fargate_resources_with_agent_valid_combos() {
        // With agent sidecar, resources are bumped up one tier.
        assert_eq!(
            fargate_resources_with_agent(InstanceSize::Small),
            ("1024", "2048")
        );
        assert_eq!(
            fargate_resources_with_agent(InstanceSize::Medium),
            ("2048", "4096")
        );
        assert_eq!(
            fargate_resources_with_agent(InstanceSize::Large),
            ("4096", "8192")
        );
    }

    #[test]
    fn config_from_env_disabled_without_image() {
        // Without ECS_HARNESS_IMAGE, config returns None.
        std::env::remove_var("ECS_HARNESS_IMAGE");
        assert!(EcsProvisionerConfig::from_env().is_none());
    }
}
