//! Multi-service application provisioner (local Docker / bollard).
//!
//! Renders an [`AppSpec`] to a set of containers on a shared, per-deployment
//! user-defined network so services resolve each other by name (e.g. the api
//! reaches `postgres:5432`). This is the generic "host an arbitrary app"
//! capability — the single-harness path in [`super::HarnessManager`] is
//! unchanged.
//!
//! v1 supports `ImageSource::Pull` fully; `ImageSource::Build` is implemented
//! in a later step (it returns a clear error until then), so pull-only apps
//! (postgres, redis, prebuilt images) work immediately.

use std::collections::HashMap;

use bollard::container::{
    Config, CreateContainerOptions, LogsOptions, RemoveContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{
    EndpointSettings, HostConfig, Mount, MountTypeEnum, PortBinding, RestartPolicy,
    RestartPolicyNameEnum,
};
use bollard::network::CreateNetworkOptions;
use bollard::volume::CreateVolumeOptions;
use bollard::Docker;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use amos_core::{AmosError, Result};

use super::app_spec::{AppSpec, ImageSource, ServiceSpec, VolumeMount};

/// Label keys applied to every resource so listing/teardown is a label query
/// (mirrors the harness labels in `mod.rs`).
const LABEL_APP: &str = "amos.app";
const LABEL_DEPLOYMENT: &str = "amos.deployment_id";
const LABEL_TENANT: &str = "amos.tenant_id";
const LABEL_SERVICE: &str = "amos.service";

/// Manages multi-service app deployments via the Docker API.
pub struct AppManager {
    docker: Docker,
}

/// The result of starting one service container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDeployResult {
    pub service_name: String,
    pub container_id: String,
    pub container_name: String,
    pub image: String,
    pub expose_public: bool,
    /// Host ports mapped to this service's container ports.
    pub host_ports: Vec<HostPortMapping>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostPortMapping {
    pub container_port: u16,
    pub host_port: u16,
    pub protocol: String,
}

/// The result of deploying a whole app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppDeployResult {
    pub network_id: String,
    pub services: Vec<ServiceDeployResult>,
}

impl AppManager {
    /// Connect to the local Docker daemon (same defaults as `HarnessManager`).
    pub fn new() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| AmosError::Internal(format!("Failed to connect to Docker: {}", e)))?;
        Ok(Self { docker })
    }

    fn network_name(deployment_id: Uuid) -> String {
        format!("amos-app-{}", deployment_id)
    }

    fn volume_name(deployment_id: Uuid, logical: &str) -> String {
        format!("amos-app-{}-{}", deployment_id, logical)
    }

    fn container_name(deployment_id: Uuid, service: &str) -> String {
        format!("amos-app-{}-{}", deployment_id, service)
    }

    fn labels(
        deployment_id: Uuid,
        tenant_id: Uuid,
        app: &str,
        service: Option<&str>,
    ) -> HashMap<String, String> {
        let mut labels = HashMap::from([
            (LABEL_APP.to_string(), app.to_string()),
            (LABEL_DEPLOYMENT.to_string(), deployment_id.to_string()),
            (LABEL_TENANT.to_string(), tenant_id.to_string()),
        ]);
        if let Some(s) = service {
            labels.insert(LABEL_SERVICE.to_string(), s.to_string());
        }
        labels
    }

    /// Deploy an app: create the network and named volumes, then create+start
    /// every service container in dependency order, attached to the network.
    pub async fn deploy(
        &self,
        deployment_id: Uuid,
        tenant_id: Uuid,
        spec: &AppSpec,
    ) -> Result<AppDeployResult> {
        spec.validate().map_err(AmosError::Validation)?;

        let network_name = Self::network_name(deployment_id);

        // 1. Create the per-app user-defined bridge network (DNS by service name).
        let network = self
            .docker
            .create_network(CreateNetworkOptions {
                name: network_name.clone(),
                driver: "bridge".to_string(),
                check_duplicate: true,
                attachable: true,
                labels: Self::labels(deployment_id, tenant_id, &spec.name, None),
                ..Default::default()
            })
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to create network: {}", e)))?;
        let network_id = if network.id.is_empty() {
            network_name.clone()
        } else {
            network.id
        };

        // 2. Create named volumes (dedup across services that share a volume).
        let mut created_volumes = std::collections::HashSet::new();
        for svc in &spec.services {
            for vm in &svc.volumes {
                if let Some(logical) = vm.named_volume() {
                    if created_volumes.insert(logical.to_string()) {
                        let vol_name = Self::volume_name(deployment_id, logical);
                        self.docker
                            .create_volume(CreateVolumeOptions {
                                name: vol_name,
                                driver: "local".to_string(),
                                labels: Self::labels(
                                    deployment_id,
                                    tenant_id,
                                    &spec.name,
                                    Some(&svc.name),
                                ),
                                ..Default::default()
                            })
                            .await
                            .map_err(|e| {
                                AmosError::Internal(format!("Failed to create volume: {}", e))
                            })?;
                    }
                }
            }
        }

        // 3. Start services in dependency order.
        let order = spec.startup_order().map_err(AmosError::Validation)?;
        let mut results = Vec::with_capacity(order.len());
        for name in order {
            let svc = spec
                .service(&name)
                .ok_or_else(|| AmosError::Internal(format!("service '{}' vanished", name)))?;
            let result = self
                .deploy_service(deployment_id, tenant_id, &spec.name, &network_name, svc)
                .await?;
            results.push(result);
        }

        Ok(AppDeployResult {
            network_id,
            services: results,
        })
    }

    /// Resolve the image (pull or build), then create and start one container.
    async fn deploy_service(
        &self,
        deployment_id: Uuid,
        tenant_id: Uuid,
        app_name: &str,
        network_name: &str,
        svc: &ServiceSpec,
    ) -> Result<ServiceDeployResult> {
        let image_ref = self.resolve_image(svc).await?;

        let container_name = Self::container_name(deployment_id, &svc.name);

        // Exposed ports + (only if public) host bindings.
        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for p in &svc.ports {
            let key = format!("{}/{}", p.container_port, p.protocol);
            exposed_ports.insert(key.clone(), HashMap::new());
            if svc.expose_public {
                port_bindings.insert(
                    key,
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        // empty = auto-assign; otherwise the requested host port
                        host_port: Some(p.host_port.map(|hp| hp.to_string()).unwrap_or_default()),
                    }]),
                );
            }
        }

        // Volume binds and mounts.
        let mut mounts: Vec<Mount> = Vec::new();
        for vm in &svc.volumes {
            match vm {
                VolumeMount::Named {
                    volume,
                    target,
                    read_only,
                } => {
                    mounts.push(Mount {
                        target: Some(target.clone()),
                        source: Some(Self::volume_name(deployment_id, volume)),
                        typ: Some(MountTypeEnum::VOLUME),
                        read_only: Some(*read_only),
                        ..Default::default()
                    });
                }
                VolumeMount::Bind {
                    source,
                    target,
                    read_only,
                } => {
                    mounts.push(Mount {
                        target: Some(target.clone()),
                        source: Some(source.clone()),
                        typ: Some(MountTypeEnum::BIND),
                        read_only: Some(*read_only),
                        ..Default::default()
                    });
                }
            }
        }

        let restart_policy = svc.restart.as_deref().map(|r| RestartPolicy {
            name: Some(match r {
                "always" => RestartPolicyNameEnum::ALWAYS,
                "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
                "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
                _ => RestartPolicyNameEnum::NO,
            }),
            ..Default::default()
        });

        let host_config = HostConfig {
            port_bindings: if port_bindings.is_empty() {
                None
            } else {
                Some(port_bindings)
            },
            mounts: if mounts.is_empty() {
                None
            } else {
                Some(mounts)
            },
            nano_cpus: svc.resources.nano_cpus(),
            memory: svc.resources.memory_bytes(),
            restart_policy,
            ..Default::default()
        };

        // Attach to the shared network with the service name as DNS alias.
        let endpoints = HashMap::from([(
            network_name.to_string(),
            EndpointSettings {
                aliases: Some(vec![svc.name.clone()]),
                ..Default::default()
            },
        )]);

        let env: Vec<String> = svc
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let config = Config {
            image: Some(image_ref.clone()),
            env: Some(env),
            cmd: svc.command.clone(),
            working_dir: svc.working_dir.clone(),
            exposed_ports: if exposed_ports.is_empty() {
                None
            } else {
                Some(exposed_ports)
            },
            host_config: Some(host_config),
            networking_config: Some(bollard::container::NetworkingConfig {
                endpoints_config: endpoints,
            }),
            labels: Some(Self::labels(
                deployment_id,
                tenant_id,
                app_name,
                Some(&svc.name),
            )),
            ..Default::default()
        };

        let create = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name.clone(),
                    platform: None,
                }),
                config,
            )
            .await
            .map_err(|e| {
                AmosError::Internal(format!("Failed to create container {}: {}", svc.name, e))
            })?;

        self.docker
            .start_container(&create.id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| {
                AmosError::Internal(format!("Failed to start container {}: {}", svc.name, e))
            })?;

        let host_ports = self
            .inspect_host_ports(&create.id, svc)
            .await
            .unwrap_or_default();

        Ok(ServiceDeployResult {
            service_name: svc.name.clone(),
            container_id: create.id,
            container_name,
            image: image_ref,
            expose_public: svc.expose_public,
            host_ports,
            status: "running".to_string(),
        })
    }

    /// Resolve a service image: pull (fully draining the progress stream) or
    /// build. Build is implemented in a later step.
    async fn resolve_image(&self, svc: &ServiceSpec) -> Result<String> {
        match &svc.image {
            ImageSource::Pull { reference } => {
                // If the image is already present locally (e.g. built by the
                // customer's existing CI/compose, or pulled earlier), use it
                // as-is. This is required for local-only images that aren't in
                // any registry, and avoids needless re-pulls otherwise.
                if self.docker.inspect_image(reference).await.is_ok() {
                    return Ok(reference.clone());
                }
                let (image, tag) = match reference.rsplit_once(':') {
                    // Avoid splitting on a registry-port colon (e.g. host:5000/img).
                    Some((img, tag)) if !tag.contains('/') => (img.to_string(), tag.to_string()),
                    _ => (reference.clone(), "latest".to_string()),
                };
                let mut stream = self.docker.create_image(
                    Some(CreateImageOptions {
                        from_image: image,
                        tag,
                        ..Default::default()
                    }),
                    None,
                    None,
                );
                // Must drain the stream to completion before creating the container.
                while let Some(item) = stream.next().await {
                    item.map_err(|e| {
                        AmosError::Internal(format!("Failed to pull {}: {}", reference, e))
                    })?;
                }
                Ok(reference.clone())
            }
            ImageSource::Build {
                context,
                dockerfile,
                tag,
            } => {
                self.build_image(&svc.name, context, dockerfile, tag)
                    .await?;
                Ok(tag.clone())
            }
        }
    }

    /// Build an image from a Dockerfile + context directory and tag it `tag`.
    ///
    /// The context (a path on the Docker daemon host) is packed into a tar,
    /// honoring `.dockerignore` and `.gitignore` so large/irrelevant paths
    /// (e.g. `.git`, model checkpoints, `node_modules`) are excluded. This is
    /// a local-dev capability: bind/build contexts are not portable to a
    /// remote daemon.
    async fn build_image(
        &self,
        service: &str,
        context: &str,
        dockerfile: &str,
        tag: &str,
    ) -> Result<()> {
        let context_path = std::path::PathBuf::from(context);
        if !context_path.is_dir() {
            return Err(AmosError::Internal(format!(
                "build context for service '{}' is not a directory: {}",
                service, context
            )));
        }

        // Pack the context (blocking fs+tar work off the async runtime).
        let context_for_tar = context_path.clone();
        let tar_bytes = tokio::task::spawn_blocking(move || tar_context(&context_for_tar))
            .await
            .map_err(|e| AmosError::Internal(format!("tar task join error: {e}")))?
            .map_err(|e| AmosError::Internal(format!("failed to package build context: {e}")))?;

        let options = bollard::image::BuildImageOptions {
            dockerfile: dockerfile.to_string(),
            t: tag.to_string(),
            rm: true,
            ..Default::default()
        };

        let mut stream =
            self.docker
                .build_image(options, None, Some(bytes::Bytes::from(tar_bytes)));
        while let Some(item) = stream.next().await {
            let info =
                item.map_err(|e| AmosError::Internal(format!("build error ({tag}): {e}")))?;
            // Docker reports build failures in-band via the `error` field.
            if let Some(err) = info.error {
                return Err(AmosError::Internal(format!(
                    "build failed for service '{service}' ({tag}): {err}"
                )));
            }
        }
        Ok(())
    }

    /// Read back the host ports Docker assigned for a service's container ports.
    async fn inspect_host_ports(
        &self,
        container_id: &str,
        svc: &ServiceSpec,
    ) -> Result<Vec<HostPortMapping>> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to inspect container: {}", e)))?;

        let ports = info
            .network_settings
            .and_then(|ns| ns.ports)
            .unwrap_or_default();

        let mut out = Vec::new();
        for p in &svc.ports {
            let key = format!("{}/{}", p.container_port, p.protocol);
            if let Some(Some(bindings)) = ports.get(&key) {
                if let Some(hp) = bindings
                    .first()
                    .and_then(|b| b.host_port.as_ref())
                    .and_then(|s| s.parse::<u16>().ok())
                {
                    out.push(HostPortMapping {
                        container_port: p.container_port,
                        host_port: hp,
                        protocol: p.protocol.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Fetch the last `tail` log lines of one service container.
    pub async fn logs(&self, container_id: &str, tail: usize) -> Result<Vec<String>> {
        let options = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            ..Default::default()
        };
        let mut stream = self.docker.logs(container_id, Some(options));
        let mut logs = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(output) => logs.push(output.to_string()),
                Err(e) => return Err(AmosError::Internal(format!("Failed to read logs: {}", e))),
            }
        }
        Ok(logs)
    }

    /// Start a single container.
    pub async fn start_container(&self, container_id: &str) -> Result<()> {
        self.docker
            .start_container(container_id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to start container: {}", e)))?;
        Ok(())
    }

    /// Stop a single container (304 = already stopped is not an error).
    pub async fn stop_container(&self, container_id: &str) -> Result<()> {
        match self
            .docker
            .stop_container(container_id, Some(StopContainerOptions { t: 10 }))
            .await
        {
            Ok(()) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 304, ..
            }) => Ok(()),
            Err(e) => Err(AmosError::Internal(format!(
                "Failed to stop container: {}",
                e
            ))),
        }
    }

    /// Restart a single container.
    pub async fn restart_container(&self, container_id: &str) -> Result<()> {
        let _ = self.stop_container(container_id).await;
        self.start_container(container_id).await
    }

    /// Get the live status string of a container.
    pub async fn container_status(&self, container_id: &str) -> Result<String> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to inspect container: {}", e)))?;
        let status = info
            .state
            .and_then(|s| s.status)
            .map(|s| format!("{:?}", s).to_lowercase())
            .unwrap_or_else(|| "unknown".to_string());
        Ok(status)
    }

    /// Tear down a whole deployment: remove all its containers and the network.
    /// Named volumes are removed only when `destroy_data` is true (so a stop or
    /// re-deploy preserves customer data).
    pub async fn teardown(&self, deployment_id: Uuid, destroy_data: bool) -> Result<()> {
        let filter = format!("{}={}", LABEL_DEPLOYMENT, deployment_id);
        let mut list_filters = HashMap::new();
        list_filters.insert("label".to_string(), vec![filter.clone()]);

        // Remove containers.
        let containers = self
            .docker
            .list_containers(Some(bollard::container::ListContainersOptions {
                all: true,
                filters: list_filters.clone(),
                ..Default::default()
            }))
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to list containers: {}", e)))?;
        for c in containers {
            if let Some(id) = c.id {
                let _ = self
                    .docker
                    .remove_container(
                        &id,
                        Some(RemoveContainerOptions {
                            force: true,
                            // Do not auto-remove anonymous volumes here; named
                            // volumes are handled explicitly below.
                            v: false,
                            ..Default::default()
                        }),
                    )
                    .await;
            }
        }

        // Remove the network.
        let _ = self
            .docker
            .remove_network(&Self::network_name(deployment_id))
            .await;

        // Remove named volumes only on explicit destroy.
        if destroy_data {
            let volumes = self
                .docker
                .list_volumes(Some(bollard::volume::ListVolumesOptions {
                    filters: list_filters,
                }))
                .await
                .map_err(|e| AmosError::Internal(format!("Failed to list volumes: {}", e)))?;
            if let Some(vols) = volumes.volumes {
                for v in vols {
                    let _ = self.docker.remove_volume(&v.name, None).await;
                }
            }
        }

        Ok(())
    }
}

/// Pack a build context directory into an uncompressed tar, honoring
/// `.dockerignore` and `.gitignore`. Returns the tar bytes. Blocking — call
/// via `spawn_blocking`.
fn tar_context(context: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use ignore::WalkBuilder;

    let mut builder = tar::Builder::new(Vec::new());

    let walk = WalkBuilder::new(context)
        .hidden(false) // include dotfiles unless an ignore file excludes them
        .git_ignore(true)
        .git_exclude(true)
        // Honor both Docker- and Podman/Buildah-style ignore files, matching
        // what the engine's own `COPY .` build context would exclude.
        .add_custom_ignore_filename(".dockerignore")
        .add_custom_ignore_filename(".containerignore")
        .build();

    for entry in walk {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        // Skip the context root itself.
        let rel = match path.strip_prefix(context) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        // Always exclude the VCS dir even if not ignored.
        if rel.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let file_type = entry.file_type();
        match file_type {
            Some(ft) if ft.is_dir() => {
                // Directories are created implicitly by file entries; tar needs
                // them only for empty dirs, which Docker doesn't require.
                continue;
            }
            Some(ft) if ft.is_file() => {
                let mut f = std::fs::File::open(path)?;
                builder.append_file(rel, &mut f)?;
            }
            _ => continue, // skip symlinks/other for v1 simplicity
        }
    }

    builder.into_inner()
}
