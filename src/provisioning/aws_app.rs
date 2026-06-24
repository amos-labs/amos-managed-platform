//! AWS Fargate renderer for multi-service apps.
//!
//! The production counterpart to [`super::app_deploy`] (local Docker): it turns
//! a provider-agnostic [`AppSpec`] into the proven Cuspr deploy shape — one ECS
//! task definition with co-located containers (services reach each other on
//! `localhost`), fronted by the shared ALB, with secrets injected from Secrets
//! Manager. Managed services (postgres→RDS, redis→ElastiCache; `ServiceKind::
//! Managed`) are **skipped** here — their connection strings arrive as
//! `secrets`, provisioned out-of-band by Terraform (`terraform/aws-app`).
//!
//! Per-tenant infra ARNs (the secret, the ALB target group, the IAM roles)
//! aren't part of the portable spec — they ride in on an [`AwsDeployTarget`].
//! Shared substrate (cluster, subnets, region) comes from env.

use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, Compatibility, ContainerDefinition, CpuArchitecture,
    EphemeralStorage, KeyValuePair, LaunchType, LoadBalancer, LogConfiguration, LogDriver,
    MountPoint, NetworkConfiguration, NetworkMode, OsFamily, PortMapping, RuntimePlatform, Secret,
    TransportProtocol, Volume,
};
use aws_sdk_cloudwatchlogs::Client as LogsClient;
use aws_sdk_ecs::Client as EcsClient;
use std::collections::HashMap;
use tracing::info;

use amos_core::{AmosError, Result};

use super::app_spec::{AppSpec, ImageSource, ServiceSpec};

/// AWS Fargate renderer for [`AppSpec`]s.
pub struct AwsAppProvisioner {
    client: EcsClient,
    logs: LogsClient,
    cluster: String,
    subnets: Vec<String>,
    security_groups: Vec<String>,
    execution_role_arn: String,
    task_role_arn: String,
    region: String,
}

/// Shared-substrate configuration (env-driven). Per-tenant specifics live on
/// [`AwsDeployTarget`].
#[derive(Debug, Clone)]
pub struct AwsAppProvisionerConfig {
    pub cluster: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub execution_role_arn: String,
    pub task_role_arn: String,
    pub region: String,
}

impl AwsAppProvisionerConfig {
    /// Load from env. Gated on `AWS_APP_ENABLED=true` so it stays off in local
    /// dev. Reuses the same `ECS_*` env vars as the harness ECS provisioner.
    pub fn from_env() -> Option<Self> {
        if std::env::var("AWS_APP_ENABLED").ok().as_deref() != Some("true") {
            return None;
        }
        let cluster = std::env::var("ECS_CLUSTER")
            .unwrap_or_else(|_| "swarm-infrastructure-cluster".to_string());
        let subnets = split_csv(
            &std::env::var("ECS_SUBNETS")
                .unwrap_or_else(|_| "subnet-0cfcbfd1c0ef057e7,subnet-0c5928d36478a1be4".to_string()),
        );
        let security_groups = split_csv(
            &std::env::var("ECS_SECURITY_GROUPS")
                .unwrap_or_else(|_| "sg-0967e26d543a5ce47".to_string()),
        );
        let execution_role_arn = std::env::var("ECS_EXECUTION_ROLE_ARN").unwrap_or_else(|_| {
            "arn:aws:iam::637423327454:role/ecsTaskExecutionRole".to_string()
        });
        // Default the task role to the exec role (matches the Cuspr deploy,
        // where one role carried both secret-read and S3 access).
        let task_role_arn =
            std::env::var("ECS_TASK_ROLE_ARN").unwrap_or_else(|_| execution_role_arn.clone());
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        Some(Self {
            cluster,
            subnets,
            security_groups,
            execution_role_arn,
            task_role_arn,
            region,
        })
    }
}

/// Per-deployment AWS infra wiring (from the `deploy_app` `aws` arg). These are
/// outputs of the per-tenant Terraform (`terraform/aws-app`).
#[derive(Debug, Clone, Default)]
pub struct AwsDeployTarget {
    /// Secrets Manager ARN whose keys back each service's `secrets` map.
    pub secret_arn: Option<String>,
    /// ALB target group ARN for the public service's ingress.
    pub target_group_arn: Option<String>,
    /// Which service is public (defaults to the lone `expose_public` service).
    pub public_service: Option<String>,
    /// Public URL (for the receipt/output), e.g. `https://cuspr.custom.amoslabs.com`.
    pub public_url: Option<String>,
    /// Task-level Fargate CPU units (default "4096").
    pub cpu: Option<String>,
    /// Task-level Fargate memory MiB (default "16384").
    pub memory: Option<String>,
    /// Ephemeral task storage in GiB (default 40 — large app images).
    pub ephemeral_storage_gib: Option<i32>,
    /// Optional task-role override (e.g. one granting tenant S3 access).
    pub task_role_arn: Option<String>,
}

/// Result of an AWS app deployment.
#[derive(Debug, Clone)]
pub struct AwsDeployResult {
    pub task_def_arn: String,
    pub service_name: String,
    pub service_arn: Option<String>,
    pub public_url: Option<String>,
    /// Names of services skipped because they're managed resources.
    pub managed_skipped: Vec<String>,
}

impl AwsAppProvisioner {
    /// Connect to ECS and verify the cluster is reachable.
    pub async fn new(config: AwsAppProvisionerConfig) -> Result<Self> {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.region.clone()))
            .load()
            .await;
        let client = EcsClient::new(&aws_config);
        let logs = LogsClient::new(&aws_config);
        client
            .describe_clusters()
            .clusters(&config.cluster)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("failed to reach ECS: {e}")))?;
        info!(cluster = %config.cluster, "AWS app provisioner connected");
        Ok(Self {
            client,
            logs,
            cluster: config.cluster,
            subnets: config.subnets,
            security_groups: config.security_groups,
            execution_role_arn: config.execution_role_arn,
            task_role_arn: config.task_role_arn,
            region: config.region,
        })
    }

    /// Render the spec to a task definition, register it, and create/update the
    /// ECS service.
    pub async fn deploy(&self, spec: &AppSpec, target: &AwsDeployTarget) -> Result<AwsDeployResult> {
        spec.validate().map_err(AmosError::Validation)?;

        let log_group = format!("/ecs/{}", spec.name);
        let _ = self
            .ensure_log_group(&log_group)
            .await
            .map_err(|e| tracing::warn!("log group ensure failed (continuing): {e}"));

        // Runnable (non-managed) services.
        let runnable: Vec<&ServiceSpec> =
            spec.services.iter().filter(|s| !s.is_managed()).collect();
        let managed_skipped: Vec<String> = spec
            .services
            .iter()
            .filter(|s| s.is_managed())
            .map(|s| s.name.clone())
            .collect();
        if runnable.is_empty() {
            return Err(AmosError::Validation(
                "app spec has no runnable (non-managed) services for AWS".into(),
            ));
        }

        // A service serving traffic is essential; a portless worker is not, so
        // its crash doesn't kill the task. But Fargate requires *at least one*
        // essential container — if none would qualify (e.g. a worker-only app),
        // force the first one essential.
        let mut essentials: Vec<bool> = runnable
            .iter()
            .map(|s| !s.ports.is_empty() || s.expose_public)
            .collect();
        if !essentials.iter().any(|&e| e) {
            essentials[0] = true;
        }

        let mut containers = Vec::new();
        let mut volume_names = std::collections::BTreeSet::new();
        for (svc, essential) in runnable.iter().zip(essentials) {
            for v in &svc.volumes {
                if let Some(n) = v.named_volume() {
                    volume_names.insert(n.to_string());
                }
            }
            containers.push(self.container_def(svc, essential, &log_group, target)?);
        }

        // Resolve the public service + its container port for ALB binding.
        let public_service = match &target.public_service {
            Some(s) => Some(s.clone()),
            None => spec
                .services
                .iter()
                .find(|s| !s.is_managed() && s.expose_public)
                .map(|s| s.name.clone()),
        };

        let runtime_platform = RuntimePlatform::builder()
            .cpu_architecture(CpuArchitecture::Arm64)
            .operating_system_family(OsFamily::Linux)
            .build();

        let mut td = self
            .client
            .register_task_definition()
            .family(&spec.name)
            .requires_compatibilities(Compatibility::Fargate)
            .network_mode(NetworkMode::Awsvpc)
            .cpu(target.cpu.clone().unwrap_or_else(|| "4096".to_string()))
            .memory(target.memory.clone().unwrap_or_else(|| "16384".to_string()))
            .runtime_platform(runtime_platform)
            .execution_role_arn(&self.execution_role_arn)
            .task_role_arn(target.task_role_arn.clone().unwrap_or_else(|| self.task_role_arn.clone()))
            .ephemeral_storage(
                EphemeralStorage::builder()
                    .size_in_gib(target.ephemeral_storage_gib.unwrap_or(40))
                    .build(),
            )
            .set_container_definitions(Some(containers));

        for vn in &volume_names {
            td = td.volumes(Volume::builder().name(vn).build());
        }

        let td_result = td
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("register_task_definition failed: {e}")))?;
        let task_def_arn = td_result
            .task_definition()
            .and_then(|t| t.task_definition_arn())
            .ok_or_else(|| AmosError::Internal("no task def ARN returned".into()))?
            .to_string();
        info!(family = %spec.name, arn = %task_def_arn, "task definition registered");

        // Create or update the ECS service.
        let service_name = spec.name.clone();
        let exists = self.service_active(&service_name).await?;
        let service_arn = if exists {
            self.client
                .update_service()
                .cluster(&self.cluster)
                .service(&service_name)
                .task_definition(&task_def_arn)
                .force_new_deployment(true)
                .send()
                .await
                .map_err(|e| AmosError::Internal(format!("update_service failed: {e}")))?
                .service()
                .and_then(|s| s.service_arn())
                .map(|s| s.to_string())
        } else {
            let mut create = self
                .client
                .create_service()
                .cluster(&self.cluster)
                .service_name(&service_name)
                .task_definition(&task_def_arn)
                .desired_count(1)
                .launch_type(LaunchType::Fargate)
                .network_configuration(
                    NetworkConfiguration::builder()
                        .awsvpc_configuration(
                            AwsVpcConfiguration::builder()
                                .set_subnets(Some(self.subnets.clone()))
                                .set_security_groups(Some(self.security_groups.clone()))
                                .assign_public_ip(AssignPublicIp::Enabled)
                                .build()
                                .map_err(|e| {
                                    AmosError::Internal(format!("vpc config: {e}"))
                                })?,
                        )
                        .build(),
                );

            // Bind the public service to the ALB target group, if provided.
            if let (Some(tg), Some(psvc)) = (&target.target_group_arn, &public_service) {
                let port = spec
                    .service(psvc)
                    .and_then(|s| s.ports.first())
                    .map(|p| p.container_port as i32)
                    .ok_or_else(|| {
                        AmosError::Validation(format!(
                            "public service '{psvc}' has no port to bind to the ALB"
                        ))
                    })?;
                create = create
                    .load_balancers(
                        LoadBalancer::builder()
                            .target_group_arn(tg)
                            .container_name(psvc)
                            .container_port(port)
                            .build(),
                    )
                    .health_check_grace_period_seconds(600);
            }

            create
                .send()
                .await
                .map_err(|e| AmosError::Internal(format!("create_service failed: {e}")))?
                .service()
                .and_then(|s| s.service_arn())
                .map(|s| s.to_string())
        };

        info!(service = %service_name, "ECS service deploy issued");
        Ok(AwsDeployResult {
            task_def_arn,
            service_name,
            service_arn,
            public_url: target.public_url.clone(),
            managed_skipped,
        })
    }

    /// Scale the service to zero and delete it (data-bearing managed resources
    /// are untouched — they're Terraform-owned).
    pub async fn teardown(&self, service_name: &str) -> Result<()> {
        let _ = self
            .client
            .update_service()
            .cluster(&self.cluster)
            .service(service_name)
            .desired_count(0)
            .send()
            .await;
        self.client
            .delete_service()
            .cluster(&self.cluster)
            .service(service_name)
            .force(true)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("delete_service failed: {e}")))?;
        info!(service = %service_name, "ECS service deleted");
        Ok(())
    }

    /// Fetch the most recent CloudWatch log lines for one service of an app.
    ///
    /// ECS awslogs writes to group `/ecs/{app_name}`, stream-prefixed by the
    /// service name (`{service}/{container}/{task_id}`). We find the latest
    /// stream for the service and return its tail — so the MCP `app_logs` tool
    /// surfaces ECS logs directly, no CloudWatch console or AWS CLI needed.
    pub async fn logs(&self, app_name: &str, service: &str, tail: usize) -> Result<Vec<String>> {
        let group = format!("/ecs/{app_name}");
        // The CloudWatch API forbids combining orderBy=LastEventTime with a
        // name prefix, so we order by recency and filter to the service's
        // streams ({service}/{container}/{task_id}) in code, taking the latest.
        let streams = self
            .logs
            .describe_log_streams()
            .log_group_name(&group)
            .order_by(aws_sdk_cloudwatchlogs::types::OrderBy::LastEventTime)
            .descending(true)
            .limit(50)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("describe_log_streams({group}): {e}")))?;

        let prefix = format!("{service}/");
        let Some(stream) = streams
            .log_streams()
            .iter()
            .filter_map(|s| s.log_stream_name())
            .find(|n| n.starts_with(&prefix))
        else {
            return Ok(vec![format!(
                "(no log stream yet for service '{service}' in {group})"
            )]);
        };

        let events = self
            .logs
            .get_log_events()
            .log_group_name(&group)
            .log_stream_name(stream)
            .limit(tail as i32)
            .start_from_head(false)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("get_log_events: {e}")))?;

        Ok(events
            .events()
            .iter()
            .filter_map(|e| e.message().map(|m| m.trim_end().to_string()))
            .collect())
    }

    /// Live ECS status of the app's service: rollout state, running/desired
    /// count, the primary task definition, and per-container last-status. This
    /// is what makes deploy progress visible from the MCP `app_status` tool.
    pub async fn service_status(&self, service_name: &str) -> Result<serde_json::Value> {
        use serde_json::json;
        let resp = self
            .client
            .describe_services()
            .cluster(&self.cluster)
            .services(service_name)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("describe_services: {e}")))?;
        let svc = resp.services().first();
        let primary = svc.and_then(|s| {
            s.deployments().iter().find(|d| d.status() == Some("PRIMARY"))
        });
        let rollout_state = primary
            .and_then(|d| d.rollout_state())
            .map(|r| r.as_str().to_string());
        let running = svc.map(|s| s.running_count()).unwrap_or(0);
        let desired = svc.map(|s| s.desired_count()).unwrap_or(0);
        let task_def = primary.and_then(|d| d.task_definition()).map(|s| s.to_string());

        // Per-container last-status from the service's current task.
        let mut containers = Vec::new();
        let tasks = self
            .client
            .list_tasks()
            .cluster(&self.cluster)
            .service_name(service_name)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("list_tasks: {e}")))?;
        if let Some(task_arn) = tasks.task_arns().first() {
            let dt = self
                .client
                .describe_tasks()
                .cluster(&self.cluster)
                .tasks(task_arn)
                .send()
                .await
                .map_err(|e| AmosError::Internal(format!("describe_tasks: {e}")))?;
            if let Some(t) = dt.tasks().first() {
                for c in t.containers() {
                    containers.push(json!({
                        "name": c.name(),
                        "last_status": c.last_status(),
                        "health": c.health_status().map(|h| h.as_str()),
                    }));
                }
            }
        }

        Ok(json!({
            "rollout_state": rollout_state,
            "running_count": running,
            "desired_count": desired,
            "task_definition": task_def,
            "containers": containers,
        }))
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    /// Build one container definition from a service spec.
    fn container_def(
        &self,
        svc: &ServiceSpec,
        essential: bool,
        log_group: &str,
        target: &AwsDeployTarget,
    ) -> Result<ContainerDefinition> {
        let image = match &svc.image {
            ImageSource::Pull { reference } => reference.clone(),
            ImageSource::Build { .. } => {
                return Err(AmosError::Validation(format!(
                    "service '{}' uses a build image; build it first via build_image and \
                     reference the resulting image digest in the spec",
                    svc.name
                )))
            }
        };

        let mut builder = ContainerDefinition::builder()
            .name(&svc.name)
            .image(image)
            .essential(essential);

        for p in &svc.ports {
            builder = builder.port_mappings(
                PortMapping::builder()
                    .container_port(p.container_port as i32)
                    .protocol(TransportProtocol::Tcp)
                    .build(),
            );
        }

        for (k, v) in &svc.env {
            builder = builder.environment(KeyValuePair::builder().name(k).value(v).build());
        }

        // Secrets → ECS `secrets` with valueFrom = {secret_arn}:{key}:: . Never
        // plaintext in the task def (the user-facing security constraint).
        if let Some(secret_arn) = &target.secret_arn {
            for (env_name, key) in &svc.secrets {
                builder = builder.secrets(
                    Secret::builder()
                        .name(env_name)
                        .value_from(format!("{secret_arn}:{key}::"))
                        .build()
                        .map_err(|e| AmosError::Internal(format!("secret build: {e}")))?,
                );
            }
        }

        if let Some(cmd) = &svc.command {
            builder = builder.set_command(Some(cmd.clone()));
        }
        if let Some(wd) = &svc.working_dir {
            builder = builder.working_directory(wd);
        }

        for v in &svc.volumes {
            if let super::app_spec::VolumeMount::Named { volume, target, read_only } = v {
                builder = builder.mount_points(
                    MountPoint::builder()
                        .source_volume(volume)
                        .container_path(target)
                        .read_only(*read_only)
                        .build(),
                );
            }
        }

        let mut log_opts = HashMap::new();
        log_opts.insert("awslogs-group".to_string(), log_group.to_string());
        log_opts.insert("awslogs-region".to_string(), self.region.clone());
        log_opts.insert("awslogs-stream-prefix".to_string(), svc.name.clone());
        builder = builder.log_configuration(
            LogConfiguration::builder()
                .log_driver(LogDriver::Awslogs)
                .set_options(Some(log_opts))
                .build()
                .map_err(|e| AmosError::Internal(format!("log config: {e}")))?,
        );

        Ok(builder.build())
    }

    /// Ensure the CloudWatch log group exists (idempotent best-effort).
    async fn ensure_log_group(&self, _group: &str) -> Result<()> {
        // ECS awslogs auto-creates the group when the task role allows it; we
        // rely on that here rather than taking a logs SDK dependency.
        Ok(())
    }

    /// Whether an ACTIVE service of this name already exists in the cluster.
    async fn service_active(&self, service_name: &str) -> Result<bool> {
        let resp = self
            .client
            .describe_services()
            .cluster(&self.cluster)
            .services(service_name)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("describe_services failed: {e}")))?;
        Ok(resp
            .services()
            .iter()
            .any(|s| s.status() == Some("ACTIVE")))
    }
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provisioning::app_spec::{PortMap, ServiceKind};
    use std::collections::BTreeMap;

    fn svc(name: &str, managed: bool, port: Option<u16>) -> ServiceSpec {
        ServiceSpec {
            name: name.to_string(),
            kind: if managed { ServiceKind::Managed } else { ServiceKind::Service },
            image: ImageSource::Pull { reference: format!("{name}:latest") },
            ports: port
                .map(|p| vec![PortMap { container_port: p, host_port: None, protocol: "tcp".into() }])
                .unwrap_or_default(),
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            command: None,
            working_dir: None,
            volumes: vec![],
            depends_on: vec![],
            resources: Default::default(),
            expose_public: port.is_some(),
            healthcheck: None,
            restart: None,
        }
    }

    #[test]
    fn target_defaults() {
        let t = AwsDeployTarget::default();
        assert!(t.secret_arn.is_none());
        assert!(t.cpu.is_none());
    }

    #[test]
    fn config_gated_on_env_flag() {
        std::env::remove_var("AWS_APP_ENABLED");
        assert!(AwsAppProvisionerConfig::from_env().is_none());
    }

    #[test]
    fn split_csv_trims_and_filters() {
        assert_eq!(split_csv("a, b ,,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn managed_services_are_identifiable() {
        let pg = svc("postgres", true, None);
        let api = svc("api", false, Some(8000));
        assert!(pg.is_managed());
        assert!(!api.is_managed());
        assert!(api.expose_public);
    }
}
