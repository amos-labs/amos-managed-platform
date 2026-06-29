//! Isolated environment provisioning (staging / preview) for an existing app.
//!
//! `provision_env` stands up a faithful, isolated copy of a running app: its own
//! database + secret + ECS service + ALB target-group/host-rule, linked as an
//! `app_deployments` row. This codifies the by-hand `cuspr-staging` build.
//!
//! Reproduction strategy is **task-definition clone**: we describe the base
//! service's live task def and re-register it under a new family with the
//! secret references repointed at the env's own secret. This works for any app —
//! including ones imported with only a thin `AppSpec` — because it copies the
//! real container definitions rather than rendering them from a spec.

use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, ContainerDefinition, KeyValuePair, LaunchType,
    LoadBalancer, NetworkConfiguration, Secret, TaskDefinition,
};
use aws_sdk_ecs::Client as EcsClient;
use aws_sdk_secretsmanager::Client as SecretsClient;
use tracing::{info, warn};

use amos_core::{AmosError, Result};
use std::collections::HashMap;

use super::aws_app::AwsAppProvisionerConfig;

/// AWS clients + shared substrate for env provisioning.
pub struct EnvProvisioner {
    ecs: EcsClient,
    secrets: SecretsClient,
    cluster: String,
    subnets: Vec<String>,
    security_groups: Vec<String>,
    region: String,
}

/// What we learned about the base service we're cloning.
#[derive(Debug, Clone)]
pub struct BaseService {
    pub task_def_arn: String,
    pub public_container: String,
    pub public_port: i32,
}

impl EnvProvisioner {
    /// Gated on the same `AWS_APP_ENABLED`/`ECS_*` env as the AWS app provisioner.
    pub async fn from_env() -> Option<Self> {
        let cfg = AwsAppProvisionerConfig::from_env()?;
        let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(cfg.region.clone()))
            .load()
            .await;
        Some(Self {
            ecs: EcsClient::new(&aws),
            secrets: SecretsClient::new(&aws),
            cluster: cfg.cluster,
            subnets: cfg.subnets,
            security_groups: cfg.security_groups,
            region: cfg.region,
        })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    /// Describe the base ECS service: its current task def + the public
    /// container/port bound to the ALB (first load balancer).
    pub async fn describe_base_service(&self, service_name: &str) -> Result<BaseService> {
        let resp = self
            .ecs
            .describe_services()
            .cluster(&self.cluster)
            .services(service_name)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("describe_services({service_name}): {e}")))?;
        let svc = resp
            .services()
            .iter()
            .find(|s| s.status() == Some("ACTIVE"))
            .ok_or_else(|| {
                AmosError::Validation(format!("base service '{service_name}' not found/active"))
            })?;
        let task_def_arn = svc
            .task_definition()
            .ok_or_else(|| AmosError::Internal("base service has no task definition".into()))?
            .to_string();
        let lb = svc.load_balancers().first().ok_or_else(|| {
            AmosError::Validation(format!(
                "base service '{service_name}' has no load balancer to mirror (need a public service)"
            ))
        })?;
        let public_container = lb
            .container_name()
            .ok_or_else(|| AmosError::Internal("load balancer has no container_name".into()))?
            .to_string();
        let public_port = lb
            .container_port()
            .ok_or_else(|| AmosError::Internal("load balancer has no container_port".into()))?;
        Ok(BaseService {
            task_def_arn,
            public_container,
            public_port,
        })
    }

    /// Load a task definition by ARN.
    pub async fn load_task_def(&self, arn: &str) -> Result<TaskDefinition> {
        self.ecs
            .describe_task_definition()
            .task_definition(arn)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("describe_task_definition: {e}")))?
            .task_definition()
            .cloned()
            .ok_or_else(|| AmosError::Internal("no task definition returned".into()))
    }

    /// Read the base secret JSON, override `DATABASE_URL`, and create a new
    /// secret for the env. Returns the new secret's ARN.
    pub async fn clone_secret(
        &self,
        base_secret_arn: &str,
        new_name: &str,
        database_url: &str,
    ) -> Result<String> {
        let base = self
            .secrets
            .get_secret_value()
            .secret_id(base_secret_arn)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("read base secret: {e}")))?;
        let base_str = base.secret_string().unwrap_or("{}");
        let mut doc: serde_json::Value =
            serde_json::from_str(base_str).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = doc.as_object_mut() {
            obj.insert(
                "DATABASE_URL".to_string(),
                serde_json::Value::String(database_url.to_string()),
            );
        }
        let arn = self
            .secrets
            .create_secret()
            .name(new_name)
            .description("isolated env app config (provision_env)")
            .secret_string(doc.to_string())
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("create env secret: {e}")))?
            .arn()
            .ok_or_else(|| AmosError::Internal("create_secret returned no ARN".into()))?
            .to_string();
        Ok(arn)
    }

    /// Register a new task def family that clones `base` with secret references
    /// repointed from `old_secret_arn` to `new_secret_arn` and `env_overrides`
    /// applied to each container's environment. Returns the new task def ARN.
    pub async fn register_cloned_task_def(
        &self,
        family: &str,
        base: &TaskDefinition,
        old_secret_arn: &str,
        new_secret_arn: &str,
        env_overrides: &[(String, String)],
    ) -> Result<String> {
        let containers = clone_containers_with_secret_swap(
            base.container_definitions(),
            old_secret_arn,
            new_secret_arn,
            env_overrides,
        )?;

        let mut reg = self
            .ecs
            .register_task_definition()
            .family(family)
            .set_container_definitions(Some(containers))
            .set_cpu(base.cpu().map(|s| s.to_string()))
            .set_memory(base.memory().map(|s| s.to_string()))
            .set_network_mode(base.network_mode().cloned())
            .set_requires_compatibilities(Some(base.requires_compatibilities().to_vec()))
            .set_execution_role_arn(base.execution_role_arn().map(|s| s.to_string()))
            .set_task_role_arn(base.task_role_arn().map(|s| s.to_string()))
            .set_runtime_platform(base.runtime_platform().cloned());
        if !base.volumes().is_empty() {
            reg = reg.set_volumes(Some(base.volumes().to_vec()));
        }

        let arn = reg
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("register_task_definition: {e}")))?
            .task_definition()
            .and_then(|t| t.task_definition_arn().map(|s| s.to_string()))
            .ok_or_else(|| AmosError::Internal("no task def ARN returned".into()))?;
        info!(family, arn = %arn, "env task definition registered");
        Ok(arn)
    }

    /// Create the env's ECS service, bound to the ALB target group.
    pub async fn create_env_service(
        &self,
        service_name: &str,
        task_def_arn: &str,
        target_group_arn: &str,
        container: &str,
        port: i32,
    ) -> Result<Option<String>> {
        let arn = self
            .ecs
            .create_service()
            .cluster(&self.cluster)
            .service_name(service_name)
            .task_definition(task_def_arn)
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
                            .map_err(|e| AmosError::Internal(format!("vpc config: {e}")))?,
                    )
                    .build(),
            )
            .load_balancers(
                LoadBalancer::builder()
                    .target_group_arn(target_group_arn)
                    .container_name(container)
                    .container_port(port)
                    .build(),
            )
            .health_check_grace_period_seconds(120)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("create_service({service_name}): {e}")))?
            .service()
            .and_then(|s| s.service_arn().map(|s| s.to_string()));
        info!(service = %service_name, "env ECS service created");
        Ok(arn)
    }

    /// Best-effort teardown of the env's ECS service (scale to 0, delete).
    pub async fn delete_service(&self, service_name: &str) {
        let _ = self
            .ecs
            .update_service()
            .cluster(&self.cluster)
            .service(service_name)
            .desired_count(0)
            .send()
            .await;
        match self
            .ecs
            .delete_service()
            .cluster(&self.cluster)
            .service(service_name)
            .force(true)
            .send()
            .await
        {
            Ok(_) => info!(service = %service_name, "env ECS service deleted"),
            Err(e) => warn!(service = %service_name, "delete_service failed (continuing): {e}"),
        }
    }

    /// Best-effort delete of the env's secret (scheduled with no recovery window).
    pub async fn delete_secret(&self, secret_arn: &str) {
        match self
            .secrets
            .delete_secret()
            .secret_id(secret_arn)
            .force_delete_without_recovery(true)
            .send()
            .await
        {
            Ok(_) => info!(secret = %secret_arn, "env secret deleted"),
            Err(e) => warn!(secret = %secret_arn, "delete_secret failed (continuing): {e}"),
        }
    }
}

/// Rebuild each container definition, repointing secret `valueFrom`s from
/// `old_secret_arn` to `new_secret_arn` and applying `env_overrides` (upsert by
/// name). Pure (no AWS calls) so the secret-swap logic is unit-testable.
pub fn clone_containers_with_secret_swap(
    containers: &[ContainerDefinition],
    old_secret_arn: &str,
    new_secret_arn: &str,
    env_overrides: &[(String, String)],
) -> Result<Vec<ContainerDefinition>> {
    let mut out = Vec::with_capacity(containers.len());
    for c in containers {
        // Environment: existing values, then upsert overrides.
        let mut env: Vec<(String, String)> = c
            .environment()
            .iter()
            .filter_map(|kv| Some((kv.name()?.to_string(), kv.value()?.to_string())))
            .collect();
        for (k, v) in env_overrides {
            if let Some(slot) = env.iter_mut().find(|(n, _)| n == k) {
                slot.1 = v.clone();
            } else {
                env.push((k.clone(), v.clone()));
            }
        }
        let environment: Vec<KeyValuePair> = env
            .into_iter()
            .map(|(n, v)| KeyValuePair::builder().name(n).value(v).build())
            .collect();

        // Secrets: repoint valueFrom to the new secret ARN.
        let mut secrets: Vec<Secret> = Vec::new();
        for s in c.secrets() {
            let name = s.name();
            let value_from = s.value_from().replace(old_secret_arn, new_secret_arn);
            secrets.push(
                Secret::builder()
                    .name(name)
                    .value_from(value_from)
                    .build()
                    .map_err(|e| AmosError::Internal(format!("secret rebuild: {e}")))?,
            );
        }

        let mut b = ContainerDefinition::builder()
            .set_name(c.name().map(|s| s.to_string()))
            .set_image(c.image().map(|s| s.to_string()))
            .set_essential(c.essential())
            .set_environment(if environment.is_empty() {
                None
            } else {
                Some(environment)
            })
            .set_secrets(if secrets.is_empty() {
                None
            } else {
                Some(secrets)
            })
            .set_port_mappings(ov(c.port_mappings()))
            .set_mount_points(ov(c.mount_points()))
            .set_ulimits(ov(c.ulimits()))
            .set_depends_on(ov(c.depends_on()))
            .set_command(ov(c.command()))
            .set_entry_point(ov(c.entry_point()))
            .set_resource_requirements(ov(c.resource_requirements()))
            .set_working_directory(c.working_directory().map(|s| s.to_string()))
            .set_user(c.user().map(|s| s.to_string()))
            .set_cpu(Some(c.cpu()))
            .set_memory(c.memory())
            .set_memory_reservation(c.memory_reservation())
            .set_readonly_root_filesystem(c.readonly_root_filesystem())
            .set_log_configuration(c.log_configuration().cloned())
            .set_health_check(c.health_check().cloned())
            .set_linux_parameters(c.linux_parameters().cloned());
        // `cpu` is non-optional on the struct in some SDK shapes; guard at 0.
        let _ = &mut b;
        out.push(b.build());
    }
    Ok(out)
}

/// `Some(vec)` when non-empty, else `None` (avoids sending empty lists).
fn ov<T: Clone>(s: &[T]) -> Option<Vec<T>> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_vec())
    }
}

/// Parse the ECR repo name from a full image ref — strip the registry host
/// (everything up to the first `/`) and any `:tag` / `@digest`. E.g.
/// `637…amazonaws.com/nuvola:prod` → `nuvola`; `…/cuspr-api@sha256:…` → `cuspr-api`.
pub fn repo_of(image: &str) -> Option<&str> {
    let rest = image.split_once('/').map(|(_, r)| r).unwrap_or(image);
    let rest = rest.split('@').next().unwrap_or(rest);
    let rest = rest.split(':').next().unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

/// Clone container definitions, repinning any container whose ECR repo matches a
/// key in `pins` to that pinned image ref (`registry/name@digest`). Containers
/// whose repo isn't in `pins` pass through unchanged. Used by `deploy` to pin the
/// freshly-built images **by digest**, so moving-tag drift can never ship a
/// stale image (the silent-no-op deploy bug). Secrets/env/all other fields are
/// copied through verbatim.
pub fn clone_containers_with_image_pins(
    containers: &[ContainerDefinition],
    pins: &HashMap<String, String>,
) -> Result<Vec<ContainerDefinition>> {
    let mut out = Vec::with_capacity(containers.len());
    for c in containers {
        let original = c.image().map(|s| s.to_string());
        let pinned = original
            .as_deref()
            .and_then(repo_of)
            .and_then(|repo| pins.get(repo).cloned());
        let final_image = pinned.or(original);

        let b = ContainerDefinition::builder()
            .set_name(c.name().map(|s| s.to_string()))
            .set_image(final_image)
            .set_essential(c.essential())
            .set_environment(ov(c.environment()))
            .set_secrets(ov(c.secrets()))
            .set_port_mappings(ov(c.port_mappings()))
            .set_mount_points(ov(c.mount_points()))
            .set_ulimits(ov(c.ulimits()))
            .set_depends_on(ov(c.depends_on()))
            .set_command(ov(c.command()))
            .set_entry_point(ov(c.entry_point()))
            .set_resource_requirements(ov(c.resource_requirements()))
            .set_working_directory(c.working_directory().map(|s| s.to_string()))
            .set_user(c.user().map(|s| s.to_string()))
            .set_cpu(Some(c.cpu()))
            .set_memory(c.memory())
            .set_memory_reservation(c.memory_reservation())
            .set_readonly_root_filesystem(c.readonly_root_filesystem())
            .set_log_configuration(c.log_configuration().cloned())
            .set_health_check(c.health_check().cloned())
            .set_linux_parameters(c.linux_parameters().cloned());
        out.push(b.build());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container_with_secret(name: &str, secret_arn: &str) -> ContainerDefinition {
        ContainerDefinition::builder()
            .name(name)
            .image("example:latest")
            .essential(true)
            .environment(KeyValuePair::builder().name("FOO").value("bar").build())
            .secrets(
                Secret::builder()
                    .name("DATABASE_URL")
                    .value_from(format!("{secret_arn}:DATABASE_URL::"))
                    .build()
                    .unwrap(),
            )
            .build()
    }

    #[test]
    fn swaps_secret_arn_and_applies_overrides() {
        let old = "arn:aws:secretsmanager:us-east-1:1:secret:cuspr/app-config-AAA";
        let new = "arn:aws:secretsmanager:us-east-1:1:secret:cuspr-staging/app-config-BBB";
        let containers = vec![container_with_secret("api", old)];
        let out = clone_containers_with_secret_swap(
            &containers,
            old,
            new,
            &[("DEV_SEED_USERS".to_string(), "true".to_string())],
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        let c = &out[0];
        // secret repointed
        let vf = c.secrets()[0].value_from();
        assert!(
            vf.starts_with(new),
            "value_from should use the new ARN: {vf}"
        );
        assert!(!vf.contains("app-config-AAA"));
        // override applied + existing env preserved
        let env: std::collections::HashMap<_, _> = c
            .environment()
            .iter()
            .map(|kv| (kv.name().unwrap(), kv.value().unwrap()))
            .collect();
        assert_eq!(env.get("DEV_SEED_USERS"), Some(&"true"));
        assert_eq!(env.get("FOO"), Some(&"bar"));
    }

    #[test]
    fn passes_through_container_without_secrets() {
        let c = ContainerDefinition::builder()
            .name("worker")
            .image("w:latest")
            .essential(false)
            .build();
        let out = clone_containers_with_secret_swap(&[c], "old", "new", &[]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name(), Some("worker"));
        assert!(out[0].secrets().is_empty());
    }

    #[test]
    fn repo_of_strips_host_tag_and_digest() {
        assert_eq!(
            repo_of("637.dkr.ecr.us-east-1.amazonaws.com/nuvola:prod"),
            Some("nuvola")
        );
        assert_eq!(
            repo_of("637.dkr.ecr.us-east-1.amazonaws.com/cuspr-api@sha256:abc"),
            Some("cuspr-api")
        );
        assert_eq!(repo_of("nuvola"), Some("nuvola"));
    }

    #[test]
    fn image_pins_swap_matching_repo_only() {
        let reg = "637.dkr.ecr.us-east-1.amazonaws.com";
        let web = ContainerDefinition::builder()
            .name("web")
            .image(format!("{reg}/nuvola:prod"))
            .essential(true)
            .secrets(
                Secret::builder()
                    .name("DATABASE_URL")
                    .value_from("arn:secret:DATABASE_URL::")
                    .build()
                    .unwrap(),
            )
            .build();
        let sidecar = ContainerDefinition::builder()
            .name("logger")
            .image("public.ecr.aws/docker/library/fluent:latest")
            .essential(false)
            .build();
        let mut pins = HashMap::new();
        pins.insert(
            "nuvola".to_string(),
            format!("{reg}/nuvola@sha256:deadbeef"),
        );
        let out = clone_containers_with_image_pins(&[web, sidecar], &pins).unwrap();
        // matching repo → pinned to digest
        assert_eq!(
            out[0].image(),
            Some(format!("{reg}/nuvola@sha256:deadbeef").as_str())
        );
        // secret preserved untouched
        assert_eq!(
            out[0].secrets()[0].value_from(),
            "arn:secret:DATABASE_URL::"
        );
        // non-matching container → unchanged
        assert_eq!(
            out[1].image(),
            Some("public.ecr.aws/docker/library/fluent:latest")
        );
    }
}
