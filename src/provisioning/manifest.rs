//! The `amos.yaml` app manifest — the single file an app author writes.
//!
//! This is the whole contract: declare your services, which dockerfile builds
//! each, any build-only base layers, and which are public. The `deploy` verb
//! reads this and does everything else (build the graph, push, roll the
//! services) — the author never touches tags, task-defs, ARNs, or AWS.
//!
//! ```yaml
//! name: cuspr
//! services:
//!   - name: base                       # built, never run (a shared base layer)
//!     dockerfile: containers/base/Containerfile
//!     build_only: true
//!   - name: api
//!     dockerfile: containers/api/Containerfile
//!     base: base                       # platform injects BASE_IMAGE=<base's image>
//!     public: true
//!   - name: frontend
//!     dockerfile: containers/frontend/Containerfile
//!     public: true
//!   - name: worker
//!     dockerfile: containers/worker/Containerfile
//!     base: base
//! ```

use serde::Deserialize;

use amos_core::{AmosError, Result};

/// A parsed `amos.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct AmosManifest {
    /// App name (also the default ECR repo prefix and deployment name).
    pub name: String,
    #[serde(default)]
    pub services: Vec<ManifestService>,
    /// Optional public subdomain (under the platform's domain suffix).
    #[serde(default)]
    pub domain: Option<String>,
    /// Optional release command run **once** before the new version rolls — the
    /// governed pre-rollout hook (usually a DB migration). It is an arbitrary
    /// command string, **toolchain-agnostic**: the `deploy` verb runs it via
    /// `sh -lc "<release>"` on the freshly-built (digest-pinned) image and **gates
    /// the rollout on it** — non-zero exit aborts the deploy and the running
    /// version keeps serving. `None` (the default) = no release phase, so existing
    /// manifests deploy exactly as before.
    ///
    /// Examples (any toolchain):
    /// - Rails:         `release: bundle exec rake db:migrate`
    /// - Python/Alembic:`release: alembic upgrade head`
    /// - Rust/sqlx:     `release: sqlx migrate run`
    /// - Node:          `release: npm run migrate`
    #[serde(default)]
    pub release: Option<String>,
}

/// One service in the manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestService {
    pub name: String,
    /// Dockerfile path relative to the repo root.
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,
    /// Another service whose freshly-built image is injected into this build as
    /// the `BASE_IMAGE` build-arg (for shared base layers).
    #[serde(default)]
    pub base: Option<String>,
    /// Build the image but don't run it (e.g. a base layer other services FROM).
    #[serde(default)]
    pub build_only: bool,
    /// ECR repo to push to. Defaults to `{app}-{service}`.
    #[serde(default)]
    pub image_name: Option<String>,
    /// Image tag the running task def references (the moving tag CI overwrites).
    /// Defaults to `prod`.
    #[serde(default)]
    pub tag: Option<String>,
    /// Whether this service is internet-facing.
    #[serde(default)]
    pub public: bool,
}

fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

impl ManifestService {
    /// ECR repo name for this service (`image_name` or `{app}-{name}`).
    pub fn image_name(&self, app: &str) -> String {
        self.image_name
            .clone()
            .unwrap_or_else(|| format!("{app}-{}", self.name))
    }
    /// Tag the running service references (`tag` or `prod`).
    pub fn tag(&self) -> String {
        self.tag.clone().unwrap_or_else(|| "prod".to_string())
    }
}

impl AmosManifest {
    pub fn parse(yaml: &str) -> Result<Self> {
        let m: AmosManifest = serde_yaml::from_str(yaml)
            .map_err(|e| AmosError::Validation(format!("invalid amos.yaml: {e}")))?;
        if m.name.trim().is_empty() {
            return Err(AmosError::Validation(
                "amos.yaml: 'name' is required".into(),
            ));
        }
        if m.services.is_empty() {
            return Err(AmosError::Validation(
                "amos.yaml: at least one service is required".into(),
            ));
        }
        // Validate base references resolve to a declared service.
        for s in &m.services {
            if let Some(b) = &s.base {
                if !m.services.iter().any(|x| &x.name == b) {
                    return Err(AmosError::Validation(format!(
                        "service '{}' references unknown base '{}'",
                        s.name, b
                    )));
                }
            }
        }
        Ok(m)
    }

    /// Services in build order: a base must build before anything that lists it
    /// as `base`. Simple Kahn-style topological sort over the `base` edges
    /// (single-level base layers in practice; cycles error out).
    pub fn build_order(&self) -> Result<Vec<&ManifestService>> {
        let mut ordered: Vec<&ManifestService> = Vec::new();
        let mut placed: std::collections::HashSet<&str> = std::collections::HashSet::new();
        // Iterate until all placed; a service is placeable once its base (if any)
        // is already placed.
        let mut remaining: Vec<&ManifestService> = self.services.iter().collect();
        while !remaining.is_empty() {
            let before = remaining.len();
            remaining.retain(|s| {
                let ready = match &s.base {
                    Some(b) => placed.contains(b.as_str()),
                    None => true,
                };
                if ready {
                    ordered.push(s);
                    placed.insert(s.name.as_str());
                    false // remove from remaining
                } else {
                    true
                }
            });
            if remaining.len() == before {
                return Err(AmosError::Validation(
                    "amos.yaml: cyclic or unresolvable 'base' dependencies".into(),
                ));
            }
        }
        Ok(ordered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders_base_first() {
        let y = r#"
name: cuspr
services:
  - name: api
    dockerfile: containers/api/Containerfile
    base: base
    public: true
  - name: base
    dockerfile: containers/base/Containerfile
    build_only: true
  - name: frontend
    dockerfile: containers/frontend/Containerfile
    public: true
"#;
        let m = AmosManifest::parse(y).unwrap();
        assert_eq!(m.name, "cuspr");
        let order: Vec<&str> = m
            .build_order()
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(order.iter().position(|n| *n == "base") < order.iter().position(|n| *n == "api"));
        assert_eq!(m.services[0].image_name("cuspr"), "cuspr-api");
        assert_eq!(m.services[1].tag(), "prod");
    }

    #[test]
    fn rejects_unknown_base() {
        let y = "name: x\nservices:\n  - name: api\n    base: nope\n";
        assert!(AmosManifest::parse(y).is_err());
    }

    #[test]
    fn release_defaults_none_and_parses_when_set() {
        // Absent → None (existing manifests unchanged: no release phase).
        let without = AmosManifest::parse("name: x\nservices:\n  - name: api\n").unwrap();
        assert_eq!(without.release, None);
        // Present → captured verbatim.
        let with = AmosManifest::parse(
            "name: x\nrelease: bundle exec rake db:migrate\nservices:\n  - name: api\n",
        )
        .unwrap();
        assert_eq!(with.release.as_deref(), Some("bundle exec rake db:migrate"));
    }
}
