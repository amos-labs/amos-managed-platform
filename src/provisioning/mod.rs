//! Harness provisioning and lifecycle management.

use bollard::{
    container::{
        Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
        StopContainerOptions,
    },
    models::{HostConfig, PortBinding},
    Docker,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use amos_core::{AmosError, Result};

/// Harness lifecycle manager using Docker API.
pub struct HarnessManager {
    docker: Docker,
}

impl HarnessManager {
    /// Create a new harness manager connected to Docker daemon.
    pub fn new() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| AmosError::Internal(format!("Failed to connect to Docker: {}", e)))?;
        Ok(Self { docker })
    }

    /// Provision a new harness container for a customer.
    pub async fn provision(&self, config: &HarnessConfig) -> Result<String> {
        let container_name = format!("amos-harness-{}", config.customer_id);
        let image = "amos-harness:latest"; // TODO: Use versioned tags

        // Build environment variables
        let mut env_vars = vec![
            format!("CUSTOMER_ID={}", config.customer_id),
            format!("AMOS_ENV={}", config.environment),
            format!("PLATFORM_GRPC_URL={}", config.platform_grpc_url),
        ];

        // Forward all AMOS__* env vars from the platform process so the harness
        // gets database, redis, bedrock, platform, and auth configuration.
        // Rewrite localhost/127.0.0.1 to host.docker.internal so the container
        // can reach host-network services (DB, Redis, platform API).
        for (key, val) in std::env::vars() {
            if key.starts_with("AMOS__") {
                let docker_val = val
                    .replace("localhost", "host.docker.internal")
                    .replace("127.0.0.1", "host.docker.internal");
                env_vars.push(format!("{}={}", key, docker_val));
            }
        }

        // Forward platform-level cloud credentials so the harness can reach
        // AI backends. These are read from the platform process environment.
        let forwarded_env_keys = [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_PROFILE",
            "GOOGLE_CLOUD_PROJECT",
            "OPENCLAW_GATEWAY_URL",
        ];
        for key in &forwarded_env_keys {
            if let Ok(val) = std::env::var(key) {
                env_vars.push(format!("{}={}", key, val));
            }
        }

        // Harness-specific overrides (the AMOS__* forwarding above may have
        // copied platform-specific values like SERVER__PORT=4000 that must be
        // corrected for the harness).
        env_vars.push("AMOS__SERVER__PORT=3000".to_string());

        // The harness needs its own database, NOT the platform database.
        // In production this would be a per-customer database; for development
        // we use amos_dev. The platform DB URL (amos_platform_dev) was forwarded
        // above and must be overridden here.
        // TODO: In production, create per-customer databases dynamically.
        env_vars.push(
            "AMOS__DATABASE__URL=postgres://rickbarkley@host.docker.internal:5432/amos_dev"
                .to_string(),
        );

        // Point the harness's platform URL back at the host-running platform.
        env_vars.push(format!(
            "AMOS__PLATFORM__URL=http://host.docker.internal:{}",
            std::env::var("AMOS__SERVER__PORT").unwrap_or_else(|_| "4000".to_string())
        ));

        // Explicit config from HarnessConfig.env_vars takes highest precedence.
        for (key, value) in &config.env_vars {
            env_vars.push(format!("{}={}", key, value));
        }

        // Create container with host port bindings so it's reachable from the host.
        // Empty host_port ("") means Docker auto-assigns a random available port.
        let port_bindings = HashMap::from([
            (
                "3000/tcp".to_string(),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some("".to_string()), // auto-assign
                }]),
            ),
        ]);

        let container_config = Config {
            image: Some(image.to_string()),
            env: Some(env_vars),
            exposed_ports: Some(HashMap::from([
                ("3000/tcp".to_string(), HashMap::new()), // HTTP port
            ])),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                ..Default::default()
            }),
            labels: Some(HashMap::from([
                ("app".to_string(), "amos-harness".to_string()),
                ("customer_id".to_string(), config.customer_id.to_string()),
                ("region".to_string(), config.region.clone()),
            ])),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: container_name.clone(),
            platform: None,
        };

        let response = self
            .docker
            .create_container(Some(options), container_config)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to create container: {}", e)))?;

        Ok(response.id)
    }

    /// Start a harness container.
    pub async fn start(&self, container_id: &str) -> Result<()> {
        self.docker
            .start_container(container_id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to start container: {}", e)))?;
        Ok(())
    }

    /// Stop a harness container gracefully.
    pub async fn stop(&self, container_id: &str) -> Result<()> {
        let options = StopContainerOptions { t: 30 }; // 30 second timeout
        self.docker
            .stop_container(container_id, Some(options))
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to stop container: {}", e)))?;
        Ok(())
    }

    /// Remove a harness container and volumes.
    pub async fn deprovision(&self, container_id: &str) -> Result<()> {
        let options = RemoveContainerOptions {
            v: true,    // Remove volumes
            force: true, // Force removal even if running
            ..Default::default()
        };

        self.docker
            .remove_container(container_id, Some(options))
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to remove container: {}", e)))?;

        Ok(())
    }

    /// Inspect a running container and return the host port mapped to container port 3000.
    pub async fn inspect_host_port(&self, container_id: &str) -> Result<Option<u16>> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to inspect container: {}", e)))?;

        // Navigate: info.network_settings.ports["3000/tcp"][0].host_port
        let port = info
            .network_settings
            .and_then(|ns| ns.ports)
            .and_then(|ports| ports.get("3000/tcp").cloned())
            .flatten()
            .and_then(|bindings| bindings.first().cloned())
            .and_then(|binding| binding.host_port)
            .and_then(|p| p.parse::<u16>().ok());

        Ok(port)
    }

    /// Get current status of a harness container.
    pub async fn get_status(&self, container_id: &str) -> Result<HarnessStatus> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to inspect container: {}", e)))?;

        let state = info.state.ok_or_else(|| {
            AmosError::Internal("Container state missing from inspect response".into())
        })?;

        let status = match (state.running, state.paused, state.restarting) {
            (Some(true), _, _) => HarnessStatus::Running,
            (Some(false), _, Some(true)) => HarnessStatus::Provisioning,
            (Some(false), Some(true), _) => HarnessStatus::Stopped,
            (Some(false), _, _) if state.exit_code.unwrap_or(0) != 0 => HarnessStatus::Error,
            _ => HarnessStatus::Stopped,
        };

        Ok(status)
    }

    /// Get container logs (last 100 lines).
    pub async fn get_logs(&self, container_id: &str) -> Result<Vec<String>> {
        use bollard::container::LogsOptions;
        use futures::StreamExt;

        let options = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: "100".to_string(),
            ..Default::default()
        };

        let mut stream = self.docker.logs(container_id, Some(options));
        let mut logs = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(output) => logs.push(output.to_string()),
                Err(e) => {
                    return Err(AmosError::Internal(format!("Failed to read logs: {}", e)))
                }
            }
        }

        Ok(logs)
    }
}

/// Configuration for provisioning a new harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub customer_id: Uuid,
    pub region: String,
    pub instance_size: InstanceSize,
    pub environment: String, // "production", "staging", "development"
    pub platform_grpc_url: String,
    pub env_vars: HashMap<String, String>,
}

/// Harness instance size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceSize {
    Small,  // 1 CPU, 2GB RAM
    Medium, // 2 CPU, 4GB RAM
    Large,  // 4 CPU, 8GB RAM
}

impl InstanceSize {
    pub fn cpu_limit(&self) -> f64 {
        match self {
            Self::Small => 1.0,
            Self::Medium => 2.0,
            Self::Large => 4.0,
        }
    }

    pub fn memory_mb(&self) -> u64 {
        match self {
            Self::Small => 2048,
            Self::Medium => 4096,
            Self::Large => 8192,
        }
    }
}

/// Harness operational status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessStatus {
    /// Container is being created.
    Provisioning,
    /// Container is running and healthy.
    Running,
    /// Container is stopped.
    Stopped,
    /// Container encountered an error.
    Error,
    /// Container has been removed.
    Deprovisioned,
}

/// Metadata about a provisioned harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessMetadata {
    pub harness_id: String,
    pub customer_id: Uuid,
    pub container_id: String,
    pub status: HarnessStatus,
    pub region: String,
    pub instance_size: InstanceSize,
    pub provisioned_at: DateTime<Utc>,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_sizes_are_progressive() {
        assert!(InstanceSize::Small.cpu_limit() < InstanceSize::Medium.cpu_limit());
        assert!(InstanceSize::Medium.memory_mb() < InstanceSize::Large.memory_mb());
    }
}
