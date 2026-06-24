//! Provider-agnostic application deployment spec.
//!
//! An [`AppSpec`] describes a multi-service application (e.g. Cuspr/SmileWise:
//! postgres + redis + api + worker + frontend) independently of *where* it
//! runs. The local Docker provisioner ([`super::app_deploy`]) renders an
//! `AppSpec` to a set of containers on a shared network today; an ECS renderer
//! can consume the same spec later without changing these types.
//!
//! These are pure data types with no Docker/AWS dependencies, so they
//! serialize cleanly to/from the `app_deployments.spec` JSONB column and are
//! easy to unit-test.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::HarnessConfig;

/// A full multi-service application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSpec {
    /// Application name (e.g. "cuspr"). Used for image tags and labels.
    pub name: String,
    /// Optional version/tag for the deployment as a whole.
    #[serde(default)]
    pub version: Option<String>,
    /// The services that make up the app. Order is not significant — startup
    /// order is derived from `depends_on`.
    pub services: Vec<ServiceSpec>,
}

/// How a service is realized on a cloud provider.
///
/// Stateful backends (Postgres, Redis) run fine as throwaway containers in
/// local Docker, but in production they map to managed services (RDS,
/// ElastiCache) provisioned out-of-band by Terraform. Marking them `Managed`
/// lets the AWS renderer **skip** running them as Fargate containers and inject
/// their connection strings as secrets instead, while the local Docker renderer
/// still runs them as ordinary containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    /// An application container the platform runs (api, frontend, worker).
    #[default]
    Service,
    /// A managed backing resource (postgres→RDS, redis→ElastiCache). Run as a
    /// container locally; skipped on AWS (connection injected via `secrets`).
    Managed,
}

/// One service (container) within an app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// Service name. Becomes the container's DNS alias on the shared network,
    /// so other services can reach it as `http://{name}:{port}`.
    pub name: String,
    /// Whether this is an app container or a managed backing resource.
    #[serde(default)]
    pub kind: ServiceKind,
    /// Where the image comes from.
    pub image: ImageSource,
    /// Ports the container listens on. Host publishing happens only when
    /// `expose_public` is true.
    #[serde(default)]
    pub ports: Vec<PortMap>,
    /// Environment variables. `BTreeMap` keeps a stable, diff-friendly order.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Secret env vars, mapping the container env var name → the key to read
    /// from the deployment's secret store. On AWS these render to ECS `secrets`
    /// with `valueFrom = {secret_arn}:{key}::` (never plaintext in the task
    /// def). Ignored by the local Docker renderer (which has no Secrets
    /// Manager) — put dev secrets in `env` instead.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// Override the image's command. A compose shell-form string is stored as
    /// `["sh", "-c", "<cmd>"]`.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Working directory inside the container (compose `working_dir`). When the
    /// image's default WORKDIR differs from where the command expects to run
    /// (e.g. node:alpine defaults to `/` but the app lives at `/app`), this is
    /// load-bearing.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Volume mounts (named volumes for persistence, bind mounts for local dev).
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// Other service names that must start before this one.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// CPU/memory limits.
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Whether this service's ports are published to the host. True for
    /// user-facing services (frontend, api); false for internal ones
    /// (postgres, redis, worker).
    #[serde(default)]
    pub expose_public: bool,
    /// Optional container healthcheck. Captured now; readiness-gated startup
    /// is a v1.1 concern.
    #[serde(default)]
    pub healthcheck: Option<HealthCheckSpec>,
    /// Restart policy (e.g. "unless-stopped", "always", "no").
    #[serde(default)]
    pub restart: Option<String>,
}

/// Where a service's image comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ImageSource {
    /// Pull an existing image by reference (e.g. "postgres:15-alpine").
    Pull { reference: String },
    /// Build an image from a Dockerfile in a context directory, tagging it
    /// `tag`. The context is a path on the Docker daemon's host (local-only in
    /// v1).
    Build {
        context: String,
        #[serde(default = "default_dockerfile")]
        dockerfile: String,
        tag: String,
    },
}

fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

/// A container port and its optional host publish mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMap {
    pub container_port: u16,
    /// Desired host port. `None` = let Docker auto-assign (recommended; the
    /// actual port is read back after start).
    #[serde(default)]
    pub host_port: Option<u16>,
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

fn default_protocol() -> String {
    "tcp".to_string()
}

/// A volume mount.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum VolumeMount {
    /// A named, persistent volume managed by the provisioner.
    Named {
        /// Logical volume name (namespaced per-deployment at provision time).
        volume: String,
        /// Mount path inside the container.
        target: String,
        #[serde(default)]
        read_only: bool,
    },
    /// A host path bind mount (local-dev only; not portable to remote daemons).
    Bind {
        /// Absolute path on the Docker daemon host.
        source: String,
        target: String,
        #[serde(default)]
        read_only: bool,
    },
}

impl ServiceSpec {
    /// Whether this service is a managed backing resource (skipped on AWS).
    pub fn is_managed(&self) -> bool {
        self.kind == ServiceKind::Managed
    }
}

impl VolumeMount {
    /// The logical name of a named volume, if this is one. Used to dedup
    /// volume creation across services that share a volume (e.g. api + worker
    /// sharing `uploads_data`).
    pub fn named_volume(&self) -> Option<&str> {
        match self {
            VolumeMount::Named { volume, .. } => Some(volume),
            VolumeMount::Bind { .. } => None,
        }
    }
}

/// CPU/memory limits for a service.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Fractional CPUs (e.g. 2.0). Rendered to bollard `nano_cpus` (× 1e9).
    #[serde(default)]
    pub cpus: Option<f64>,
    /// Memory in MiB. Rendered to bollard `memory` (× 1 MiB).
    #[serde(default)]
    pub memory_mb: Option<u64>,
}

impl ResourceLimits {
    /// Docker `nano_cpus` value, if a CPU limit is set.
    pub fn nano_cpus(&self) -> Option<i64> {
        self.cpus.map(|c| (c * 1_000_000_000.0) as i64)
    }

    /// Docker `memory` value in bytes, if a memory limit is set.
    pub fn memory_bytes(&self) -> Option<i64> {
        self.memory_mb.map(|mb| (mb * 1024 * 1024) as i64)
    }
}

/// Container healthcheck (captured for v1.1 readiness gating).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckSpec {
    /// Test command, e.g. `["CMD-SHELL", "curl -f http://localhost:8000/health"]`.
    pub test: Vec<String>,
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub retries: Option<u64>,
}

impl AppSpec {
    /// Express the existing single-harness deployment as an `AppSpec`.
    ///
    /// This is a backward-compatibility / superset proof: it shows the harness
    /// is just an app with one service. It does **not** replace
    /// [`super::HarnessManager::provision`] — the harness path is unchanged.
    pub fn from_harness(config: &HarnessConfig) -> Self {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("CUSTOMER_ID".to_string(), config.customer_id.to_string());
        env.insert("AMOS_ENV".to_string(), config.environment.clone());
        env.insert("AMOS__SERVER__PORT".to_string(), "3000".to_string());
        for (k, v) in &config.env_vars {
            env.insert(k.clone(), v.clone());
        }

        let resources = ResourceLimits {
            cpus: Some(config.instance_size.cpu_limit()),
            memory_mb: Some(config.instance_size.memory_mb()),
        };

        AppSpec {
            name: format!("harness-{}", config.customer_id),
            version: None,
            services: vec![ServiceSpec {
                name: "amos-harness".to_string(),
                kind: ServiceKind::Service,
                image: ImageSource::Pull {
                    reference: "amos-harness:latest".to_string(),
                },
                ports: vec![PortMap {
                    container_port: 3000,
                    host_port: None,
                    protocol: "tcp".to_string(),
                }],
                env,
                secrets: BTreeMap::new(),
                command: None,
                working_dir: None,
                volumes: vec![],
                depends_on: vec![],
                resources,
                expose_public: true,
                healthcheck: None,
                restart: None,
            }],
        }
    }

    /// Validate the spec: non-empty, unique service names, and `depends_on`
    /// references resolve to real services with no cycle.
    pub fn validate(&self) -> Result<(), String> {
        if self.services.is_empty() {
            return Err("app spec has no services".into());
        }
        let names: std::collections::HashSet<&str> =
            self.services.iter().map(|s| s.name.as_str()).collect();
        if names.len() != self.services.len() {
            return Err("duplicate service names in app spec".into());
        }
        for svc in &self.services {
            for dep in &svc.depends_on {
                if !names.contains(dep.as_str()) {
                    return Err(format!(
                        "service '{}' depends_on unknown service '{}'",
                        svc.name, dep
                    ));
                }
            }
        }
        // Cycle detection is performed by the topological sort at deploy time.
        self.startup_order().map(|_| ())
    }

    /// Return service names in dependency order (dependencies first) via a
    /// Kahn topological sort. Errors if `depends_on` contains a cycle.
    pub fn startup_order(&self) -> Result<Vec<String>, String> {
        use std::collections::{HashMap, VecDeque};

        let mut indegree: HashMap<&str, usize> = HashMap::new();
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for svc in &self.services {
            indegree.entry(svc.name.as_str()).or_insert(0);
            for dep in &svc.depends_on {
                // edge dep -> svc (dep must start before svc)
                *indegree.entry(svc.name.as_str()).or_insert(0) += 1;
                dependents
                    .entry(dep.as_str())
                    .or_default()
                    .push(svc.name.as_str());
            }
        }

        let mut queue: VecDeque<&str> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&n, _)| n)
            .collect();
        // Deterministic order among ready nodes.
        let mut ready: Vec<&str> = queue.drain(..).collect();
        ready.sort_unstable();
        let mut queue: VecDeque<&str> = ready.into_iter().collect();

        let mut order = Vec::with_capacity(self.services.len());
        while let Some(n) = queue.pop_front() {
            order.push(n.to_string());
            if let Some(children) = dependents.get(n) {
                let mut newly_ready = Vec::new();
                for &c in children {
                    let e = indegree.get_mut(c).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        newly_ready.push(c);
                    }
                }
                newly_ready.sort_unstable();
                for c in newly_ready {
                    queue.push_back(c);
                }
            }
        }

        if order.len() != self.services.len() {
            return Err("dependency cycle detected in app spec".into());
        }
        Ok(order)
    }

    /// Look up a service by name.
    pub fn service(&self, name: &str) -> Option<&ServiceSpec> {
        self.services.iter().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, deps: &[&str]) -> ServiceSpec {
        ServiceSpec {
            name: name.to_string(),
            kind: ServiceKind::Service,
            image: ImageSource::Pull {
                reference: format!("{name}:latest"),
            },
            ports: vec![],
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            command: None,
            working_dir: None,
            volumes: vec![],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            resources: ResourceLimits::default(),
            expose_public: false,
            healthcheck: None,
            restart: None,
        }
    }

    #[test]
    fn startup_order_respects_dependencies() {
        // cuspr-like: postgres, redis (no deps); api depends on postgres;
        // worker depends on postgres; frontend depends on api.
        let spec = AppSpec {
            name: "cuspr".into(),
            version: None,
            services: vec![
                svc("frontend", &["api"]),
                svc("api", &["postgres"]),
                svc("worker", &["postgres"]),
                svc("postgres", &[]),
                svc("redis", &[]),
            ],
        };
        let order = spec.startup_order().unwrap();
        let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
        assert!(pos("postgres") < pos("api"));
        assert!(pos("postgres") < pos("worker"));
        assert!(pos("api") < pos("frontend"));
        assert_eq!(order.len(), 5);
    }

    #[test]
    fn cycle_is_rejected() {
        let spec = AppSpec {
            name: "bad".into(),
            version: None,
            services: vec![svc("a", &["b"]), svc("b", &["a"])],
        };
        assert!(spec.startup_order().is_err());
        assert!(spec.validate().is_err());
    }

    #[test]
    fn unknown_dependency_is_rejected() {
        let spec = AppSpec {
            name: "bad".into(),
            version: None,
            services: vec![svc("a", &["ghost"])],
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn named_volume_dedup_helper() {
        let m = VolumeMount::Named {
            volume: "uploads_data".into(),
            target: "/app/uploads".into(),
            read_only: false,
        };
        assert_eq!(m.named_volume(), Some("uploads_data"));
        let b = VolumeMount::Bind {
            source: "/host/x".into(),
            target: "/app/x".into(),
            read_only: true,
        };
        assert_eq!(b.named_volume(), None);
    }
}
