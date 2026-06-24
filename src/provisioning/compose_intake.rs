//! Docker Compose → [`AppSpec`] intake.
//!
//! This is the Motion-A "migration intake": point AMOS at an existing app's
//! `compose.yml` and get a provider-agnostic [`AppSpec`] back. The produced
//! spec is the editable source of truth — we derive it once, then deploy it.
//!
//! v1 maps the common compose fields and deliberately ignores the rest (see
//! `app_spec_from_compose_str`). It is not a full compose implementation.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::app_spec::{
    AppSpec, ImageSource, PortMap, ResourceLimits, ServiceKind, ServiceSpec, VolumeMount,
};

// ── Raw compose shapes (only the fields we read) ──────────────────────────

#[derive(Debug, Deserialize)]
struct ComposeFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    services: BTreeMap<String, ComposeService>,
}

#[derive(Debug, Deserialize)]
struct ComposeService {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    build: Option<ComposeBuild>,
    #[serde(default)]
    ports: Vec<serde_yaml::Value>,
    #[serde(default)]
    environment: Option<serde_yaml::Value>,
    #[serde(default)]
    command: Option<serde_yaml::Value>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    depends_on: Option<serde_yaml::Value>,
    #[serde(default)]
    deploy: Option<ComposeDeploy>,
    #[serde(default)]
    restart: Option<String>,
    /// Compose `profiles:` — services with any profile are skipped in v1
    /// (e.g. the build-only `base` service).
    #[serde(default)]
    profiles: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeBuild {
    /// `build: .`
    Short(String),
    /// `build: { context: ., dockerfile: ... }`
    Long {
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        dockerfile: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct ComposeDeploy {
    #[serde(default)]
    resources: Option<ComposeResources>,
}

#[derive(Debug, Deserialize)]
struct ComposeResources {
    #[serde(default)]
    limits: Option<ComposeResourceLimits>,
}

#[derive(Debug, Deserialize)]
struct ComposeResourceLimits {
    #[serde(default)]
    cpus: Option<serde_yaml::Value>,
    #[serde(default)]
    memory: Option<String>,
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Parse a compose file's contents into an [`AppSpec`].
///
/// `compose_dir` is the directory the compose file lives in; relative bind
/// mounts (`./x`) are resolved to absolute paths against it (bind mounts are
/// paths on the Docker daemon host).
///
/// Intentionally ignored in v1: `profiles` (those services are skipped),
/// `compose.override.yml` merging, `${VAR}` interpolation, `networks:` blocks
/// (we always create our own single network), `entrypoint`, `working_dir`.
pub fn app_spec_from_compose_str(yaml: &str, compose_dir: &Path) -> Result<AppSpec, String> {
    let parsed: ComposeFile =
        serde_yaml::from_str(yaml).map_err(|e| format!("invalid compose YAML: {e}"))?;

    let app_name = parsed
        .name
        .clone()
        .unwrap_or_else(|| "app".to_string());

    let mut services = Vec::new();
    for (svc_name, svc) in &parsed.services {
        // Skip profile-gated services (e.g. build-only `base`).
        if !svc.profiles.is_empty() {
            continue;
        }
        services.push(convert_service(&app_name, svc_name, svc, compose_dir)?);
    }

    if services.is_empty() {
        return Err("compose file produced no deployable services".into());
    }

    let spec = AppSpec {
        name: app_name,
        version: None,
        services,
    };
    spec.validate()?;
    Ok(spec)
}

/// Read and parse a compose file at `path`.
pub fn app_spec_from_compose_file(path: &Path) -> Result<AppSpec, String> {
    let yaml = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    app_spec_from_compose_str(&yaml, dir)
}

// ── Field conversion ───────────────────────────────────────────────────────

fn convert_service(
    app_name: &str,
    name: &str,
    svc: &ComposeService,
    compose_dir: &Path,
) -> Result<ServiceSpec, String> {
    let image = match (&svc.image, &svc.build) {
        (_, Some(build)) => {
            let (context, dockerfile) = match build {
                ComposeBuild::Short(ctx) => (ctx.clone(), "Dockerfile".to_string()),
                ComposeBuild::Long { context, dockerfile } => (
                    context.clone().unwrap_or_else(|| ".".to_string()),
                    dockerfile.clone().unwrap_or_else(|| "Dockerfile".to_string()),
                ),
            };
            ImageSource::Build {
                context: resolve_path(compose_dir, &context),
                dockerfile,
                tag: format!("amos-{app_name}-{name}:latest"),
            }
        }
        (Some(image), None) => ImageSource::Pull {
            reference: image.clone(),
        },
        (None, None) => {
            return Err(format!("service '{name}' has neither image nor build"));
        }
    };

    let ports = parse_ports(&svc.ports)?;
    let expose_public = !ports.is_empty();
    let env = parse_environment(&svc.environment);
    let command = parse_command(&svc.command);
    let volumes = parse_volumes(&svc.volumes, compose_dir);
    let depends_on = parse_depends_on(&svc.depends_on);
    let resources = parse_resources(&svc.deploy);
    // Infer managed backing resources from the image so the AWS renderer maps
    // them to RDS/ElastiCache instead of running them as Fargate containers.
    // The local Docker renderer ignores `kind` and runs them as usual.
    let kind = match &svc.image {
        Some(image) if is_managed_image(image) => ServiceKind::Managed,
        _ => ServiceKind::Service,
    };

    Ok(ServiceSpec {
        name: name.to_string(),
        kind,
        image,
        ports,
        env,
        secrets: std::collections::BTreeMap::new(),
        command,
        working_dir: svc.working_dir.clone(),
        volumes,
        depends_on,
        resources,
        expose_public,
        healthcheck: None,
        restart: svc.restart.clone(),
    })
}

/// Heuristic: does this image reference a managed backing service (Postgres,
/// Redis, …) that should map to a cloud-managed resource rather than a
/// throwaway container in production? Matches the image name (last path
/// segment, before the tag) against well-known stateful backends.
fn is_managed_image(image: &str) -> bool {
    let name = image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(':')
        .next()
        .unwrap_or(image)
        .to_ascii_lowercase();
    const MANAGED: &[&str] = &[
        "postgres", "postgresql", "redis", "valkey", "mysql", "mariadb",
        "mongo", "mongodb", "pgvector", "ankane/pgvector", "memcached",
    ];
    MANAGED.iter().any(|m| name == *m || name.starts_with(m))
}

/// Compose ports: "8000:8000", "5433:5432", "8000" → PortMap (container port =
/// RHS of the mapping; host port = LHS when present).
fn parse_ports(raw: &[serde_yaml::Value]) -> Result<Vec<PortMap>, String> {
    let mut out = Vec::new();
    for v in raw {
        let s = match v {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            _ => continue,
        };
        // Strip any protocol suffix ("8000/tcp").
        let (mapping, protocol) = match s.split_once('/') {
            Some((m, p)) => (m.to_string(), p.to_string()),
            None => (s.clone(), "tcp".to_string()),
        };
        let (host_port, container_port) = match mapping.split_once(':') {
            Some((host, cont)) => (
                host.rsplit(':').next().and_then(|h| h.parse::<u16>().ok()),
                cont.parse::<u16>()
                    .map_err(|_| format!("invalid container port in '{s}'"))?,
            ),
            None => (
                None,
                mapping
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port '{s}'"))?,
            ),
        };
        out.push(PortMap {
            container_port,
            host_port,
            protocol,
        });
    }
    Ok(out)
}

/// Compose `environment`: either a list of "K=V" / "K" or a map.
fn parse_environment(raw: &Option<serde_yaml::Value>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    match raw {
        Some(serde_yaml::Value::Sequence(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    match s.split_once('=') {
                        Some((k, v)) => {
                            env.insert(k.trim().to_string(), v.to_string());
                        }
                        None => {
                            // Bare "KEY" → pass through empty; daemon would
                            // normally inherit, but we have no host env to read.
                            env.insert(s.trim().to_string(), String::new());
                        }
                    }
                }
            }
        }
        Some(serde_yaml::Value::Mapping(map)) => {
            for (k, v) in map {
                if let Some(k) = k.as_str() {
                    env.insert(k.to_string(), yaml_scalar_to_string(v));
                }
            }
        }
        _ => {}
    }
    env
}

/// Compose `command`: list-form is argv as-is; string-form is **shell-split**
/// into argv, matching Docker Compose semantics (`command: sh -c "X"` →
/// `["sh","-c","X"]`). Wrapping the whole string in another `sh -c` would
/// double-wrap and mangle nested quotes/flags. Falls back to `["sh","-c",s]`
/// only if the string can't be lexed.
fn parse_command(raw: &Option<serde_yaml::Value>) -> Option<Vec<String>> {
    match raw {
        Some(serde_yaml::Value::String(s)) => match shlex::split(s) {
            Some(argv) if !argv.is_empty() => Some(argv),
            _ => Some(vec!["sh".to_string(), "-c".to_string(), s.clone()]),
        },
        Some(serde_yaml::Value::Sequence(items)) => Some(
            items
                .iter()
                .filter_map(|i| i.as_str().map(String::from))
                .collect(),
        ),
        _ => None,
    }
}

/// Compose `volumes`: "name:/path[:ro]" (named) or "./host:/path[:ro]" (bind).
fn parse_volumes(raw: &[String], compose_dir: &Path) -> Vec<VolumeMount> {
    let mut out = Vec::new();
    for v in raw {
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() < 2 {
            continue;
        }
        let source = parts[0];
        let target = parts[1].to_string();
        let read_only = parts.get(2).map(|m| *m == "ro").unwrap_or(false);

        if source.starts_with('/') || source.starts_with('.') {
            out.push(VolumeMount::Bind {
                source: resolve_path(compose_dir, source),
                target,
                read_only,
            });
        } else {
            out.push(VolumeMount::Named {
                volume: source.to_string(),
                target,
                read_only,
            });
        }
    }
    out
}

/// Compose `depends_on`: list of names, or a map keyed by service name.
fn parse_depends_on(raw: &Option<serde_yaml::Value>) -> Vec<String> {
    match raw {
        Some(serde_yaml::Value::Sequence(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        Some(serde_yaml::Value::Mapping(map)) => map
            .keys()
            .filter_map(|k| k.as_str().map(String::from))
            .collect(),
        _ => vec![],
    }
}

/// Compose `deploy.resources.limits` → ResourceLimits. "8G"→8192 MiB, "4"→4.0.
fn parse_resources(deploy: &Option<ComposeDeploy>) -> ResourceLimits {
    let limits = deploy
        .as_ref()
        .and_then(|d| d.resources.as_ref())
        .and_then(|r| r.limits.as_ref());
    let Some(limits) = limits else {
        return ResourceLimits::default();
    };
    ResourceLimits {
        cpus: limits.cpus.as_ref().and_then(|c| match c {
            serde_yaml::Value::String(s) => s.parse::<f64>().ok(),
            serde_yaml::Value::Number(n) => n.as_f64(),
            _ => None,
        }),
        memory_mb: limits.memory.as_ref().and_then(|m| parse_memory_mb(m)),
    }
}

/// "8G" → 8192, "512M" → 512, "1073741824" → 1024.
fn parse_memory_mb(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.trim().parse().ok()?;
    let mb = match unit.to_uppercase().as_str() {
        "G" | "GB" | "GIB" => n * 1024.0,
        "M" | "MB" | "MIB" => n,
        "K" | "KB" | "KIB" => n / 1024.0,
        "" => n / (1024.0 * 1024.0), // raw bytes
        _ => return None,
    };
    Some(mb.round() as u64)
}

fn yaml_scalar_to_string(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Resolve a relative path against the compose directory; absolute paths pass
/// through. Used for bind-mount sources (host-daemon paths).
fn resolve_path(compose_dir: &Path, p: &str) -> String {
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        compose_dir.join(path).to_string_lossy().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // A trimmed version of the real SmileWise compose.yml, exercising every
    // mapping path: named project, build + image services, ports, env list,
    // shell command, named + bind volumes, depends_on, deploy limits, a
    // profile-gated build-only service that must be skipped.
    const COMPOSE: &str = r#"
name: cuspr
services:
  base:
    image: smilewise-base:latest
    build:
      context: .
      dockerfile: containers/app/Containerfile.base
    profiles: ["base"]
  postgres:
    image: postgres:15-alpine
    environment:
      - POSTGRES_DB=dental_architect_dev
      - POSTGRES_USER=dentaladmin
    ports:
      - "5433:5432"
    volumes:
      - postgres_data:/var/lib/postgresql/data
      - ./scripts/init-test-db.sql:/docker-entrypoint-initdb.d/init-test-db.sql
  api:
    build:
      context: .
      dockerfile: containers/app/Containerfile.dev
    ports:
      - "8000:8000"
    deploy:
      resources:
        limits:
          cpus: "4"
          memory: 8G
    environment:
      - DATABASE_URL=postgresql://dentaladmin:pw@postgres:5432/dental_architect_dev
      - REDIS_URL=redis://redis:6379/0
    command: sh -c "alembic upgrade heads && uvicorn fastapi_app.main:app --host 0.0.0.0 --port 8000"
    volumes:
      - uploads_data:/app/uploads
    depends_on:
      - postgres
  worker:
    build:
      context: .
      dockerfile: containers/segmentation/Containerfile.dev
    environment:
      - APP_INTERNAL_URL=http://api:8000
    volumes:
      - uploads_data:/app/uploads
    depends_on:
      - postgres
    restart: unless-stopped
  frontend:
    image: node:20-alpine
    ports:
      - "3000:3000"
    depends_on:
      - api
  redis:
    image: redis:7.2
    ports:
      - "6379:6379"
"#;

    fn spec() -> AppSpec {
        app_spec_from_compose_str(COMPOSE, &PathBuf::from("/srv/cuspr")).expect("intake ok")
    }

    #[test]
    fn skips_profile_gated_base_service() {
        let s = spec();
        assert!(s.service("base").is_none(), "base must be skipped");
        // 5 runtime services remain.
        assert_eq!(s.services.len(), 5);
        assert_eq!(s.name, "cuspr");
    }

    #[test]
    fn build_vs_pull_and_tag() {
        let s = spec();
        match &s.service("api").unwrap().image {
            ImageSource::Build { tag, dockerfile, context } => {
                assert_eq!(tag, "amos-cuspr-api:latest");
                assert_eq!(dockerfile, "containers/app/Containerfile.dev");
                assert_eq!(context, "/srv/cuspr/."); // resolved against compose dir
            }
            _ => panic!("api should build"),
        }
        match &s.service("postgres").unwrap().image {
            ImageSource::Pull { reference } => assert_eq!(reference, "postgres:15-alpine"),
            _ => panic!("postgres should pull"),
        }
    }

    #[test]
    fn ports_set_expose_public_and_container_port() {
        let s = spec();
        let api = s.service("api").unwrap();
        assert!(api.expose_public);
        assert_eq!(api.ports[0].container_port, 8000);
        assert_eq!(api.ports[0].host_port, Some(8000));
        // postgres maps 5433->5432; container port is the RHS.
        let pg = s.service("postgres").unwrap();
        assert_eq!(pg.ports[0].container_port, 5432);
        assert_eq!(pg.ports[0].host_port, Some(5433));
        // worker has no ports → not public.
        assert!(!s.service("worker").unwrap().expose_public);
    }

    #[test]
    fn env_command_resources() {
        let s = spec();
        let api = s.service("api").unwrap();
        assert_eq!(
            api.env.get("REDIS_URL").map(String::as_str),
            Some("redis://redis:6379/0")
        );
        let cmd = api.command.as_ref().unwrap();
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        assert!(cmd[2].contains("alembic upgrade heads"));
        assert_eq!(api.resources.cpus, Some(4.0));
        assert_eq!(api.resources.memory_mb, Some(8192));
    }

    #[test]
    fn named_and_bind_volumes_resolved() {
        let s = spec();
        let pg = s.service("postgres").unwrap();
        // one named (postgres_data) + one bind (init-test-db.sql)
        assert!(pg
            .volumes
            .iter()
            .any(|v| matches!(v, VolumeMount::Named { volume, .. } if volume == "postgres_data")));
        assert!(pg.volumes.iter().any(|v| matches!(
            v,
            VolumeMount::Bind { source, .. } if source == "/srv/cuspr/./scripts/init-test-db.sql"
        )));
        // api + worker share the uploads_data named volume.
        let api_vol = s.service("api").unwrap().volumes[0].named_volume();
        let worker_vol = s.service("worker").unwrap().volumes[0].named_volume();
        assert_eq!(api_vol, Some("uploads_data"));
        assert_eq!(worker_vol, Some("uploads_data"));
    }

    #[test]
    fn depends_on_and_startup_order() {
        let s = spec();
        assert_eq!(s.service("api").unwrap().depends_on, vec!["postgres"]);
        assert_eq!(
            s.service("worker").unwrap().restart.as_deref(),
            Some("unless-stopped")
        );
        let order = s.startup_order().unwrap();
        let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
        assert!(pos("postgres") < pos("api"));
        assert!(pos("api") < pos("frontend"));
    }
}
