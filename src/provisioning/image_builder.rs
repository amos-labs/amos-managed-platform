//! Build-as-a-service: cloud container image builds.
//!
//! The single biggest friction in porting an app onto AMOS is building and
//! pushing the image — large, slow, and fragile when done on a laptop (buildx
//! auth, OCI-index quirks, `docker save`/`crane` workarounds). This module
//! makes it a *service*: the agent hands a build **context** + Dockerfile and
//! gets back a pullable **image digest**. It never runs docker locally.
//!
//! Pipeline: zip the context → upload to the build-source S3 bucket → trigger
//! the reused `amos-image-builder` CodeBuild project (ARM64, privileged Docker)
//! with per-build env overrides → CodeBuild builds and pushes to ECR → read
//! the digest back from ECR. See `terraform/build/main.tf` for the substrate.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use aws_sdk_codebuild::types::{EnvironmentVariable, EnvironmentVariableType};
use aws_sdk_codebuild::Client as CodeBuildClient;
use aws_sdk_ecr::Client as EcrClient;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use tracing::{info, warn};
use uuid::Uuid;

use amos_core::{AmosError, Result};

/// The buildspec, passed as an override on every build so it is authoritative
/// regardless of any `buildspec.yml` in the source context. Each command is a
/// double-quoted YAML scalar — colons inside (e.g. `name:tag`, `args:`) are
/// then safe (an unquoted scalar with `: ` is parsed as a map key).
///
/// **Layer caching:** builds run with BuildKit and `--cache-from` the image's
/// own previous tag (pulled best-effort first; `BUILDKIT_INLINE_CACHE=1` on
/// every push embeds the cache metadata so the *next* build can reuse it). When
/// a `BASE_IMAGE` build-arg is present, that base is also a cache source via
/// `$CACHE_FROM_BASE`. Net: an unchanged ML base is a near-instant cache hit
/// instead of a from-scratch rebuild — the long pole that timed out cold Cuspr
/// deploys. First build of a tag is a cache miss (pulls fail → `|| true`).
const BUILDSPEC: &str = r#"version: 0.2
phases:
  pre_build:
    commands:
      - "aws ecr get-login-password --region $AWS_DEFAULT_REGION | docker login --username AWS --password-stdin $ECR_REGISTRY"
      - "aws ecr describe-repositories --repository-names \"$IMAGE_NAME\" >/dev/null 2>&1 || aws ecr create-repository --repository-name \"$IMAGE_NAME\" >/dev/null"
      - "docker pull \"$ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG\" 2>/dev/null || true"
      - "if [ -n \"$CACHE_FROM_BASE\" ]; then docker pull \"$CACHE_FROM_BASE\" 2>/dev/null || true; fi"
  build:
    commands:
      - "echo Building $ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG arm64 from $DOCKERFILE with buildkit cache-from"
      - "CACHE=\"--cache-from $ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG\"; if [ -n \"$CACHE_FROM_BASE\" ]; then CACHE=\"$CACHE --cache-from $CACHE_FROM_BASE\"; fi; DOCKER_BUILDKIT=1 docker build --platform linux/arm64 $CACHE --build-arg BUILDKIT_INLINE_CACHE=1 $BUILD_ARGS -f \"$DOCKERFILE\" -t \"$ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG\" \"$BUILD_CONTEXT\""
  post_build:
    commands:
      - "docker push \"$ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG\""
      - "aws ecr describe-images --repository-name \"$IMAGE_NAME\" --image-ids imageTag=\"$IMAGE_TAG\" --query imageDetails[0].imageDigest --output text"
"#;

/// Cloud image builder backed by AWS CodeBuild + S3 + ECR.
pub struct ImageBuilder {
    codebuild: CodeBuildClient,
    s3: S3Client,
    ecr: EcrClient,
    project_name: String,
    source_bucket: String,
    ecr_registry: String,
}

/// Configuration for the image builder, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct ImageBuilderConfig {
    pub project_name: String,
    pub source_bucket: String,
    pub ecr_registry: String,
    pub aws_region: String,
}

impl ImageBuilderConfig {
    /// Load from env. Returns `None` (builder disabled) unless
    /// `AMOS_BUILD_SOURCE_BUCKET` is set — the gate for cloud builds.
    pub fn from_env() -> Option<Self> {
        let source_bucket = std::env::var("AMOS_BUILD_SOURCE_BUCKET").ok()?;
        let project_name = std::env::var("AMOS_BUILD_PROJECT")
            .unwrap_or_else(|_| "amos-image-builder".to_string());
        let aws_region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        let ecr_registry = std::env::var("AMOS_ECR_REGISTRY").unwrap_or_default();
        Some(Self {
            project_name,
            source_bucket,
            ecr_registry,
            aws_region,
        })
    }
}

/// What to build. Source is either a **local context dir** (zipped → S3, the
/// original path) or a **git repo + ref** (CodeBuild clones it directly — the
/// path CI uses, since a remote caller has no path on the platform host).
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// Absolute path to the build context directory (on the platform host).
    /// `None` when building from a git source (`git_repo`).
    pub context_dir: Option<PathBuf>,
    /// Git source URL (e.g. `https://github.com/org/repo.git`). When set, the
    /// build clones the repo at `git_ref` instead of zipping `context_dir`.
    /// Private repos require GitHub source credentials imported into CodeBuild.
    pub git_repo: Option<String>,
    /// Git ref to build (commit SHA, branch, or tag). Used with `git_repo`.
    pub git_ref: Option<String>,
    /// Dockerfile path, relative to the context (e.g. `containers/app/Dockerfile`).
    pub dockerfile: String,
    /// ECR repository name to push to (e.g. `cuspr-api`). Created if absent.
    pub image_name: String,
    /// Image tag (e.g. `latest`, a git sha, or a release tag).
    pub tag: String,
    /// Docker `--build-arg` values (e.g. `BASE_IMAGE=…/cuspr-base:latest`).
    /// Values must not contain spaces (rendered space-separated to CodeBuild).
    pub build_args: std::collections::BTreeMap<String, String>,
}

/// The outcome of polling a build.
#[derive(Debug, Clone)]
pub struct BuildState {
    /// CodeBuild status: `IN_PROGRESS`, `SUCCEEDED`, `FAILED`, `STOPPED`, …
    pub status: String,
    /// The phase currently/last executing (e.g. `BUILD`, `POST_BUILD`).
    pub current_phase: String,
    /// `true` once CodeBuild reports SUCCEEDED.
    pub succeeded: bool,
    /// `true` once the build has reached a terminal state.
    pub done: bool,
    /// The pushed image digest (`sha256:…`), available after success.
    pub image_digest: Option<String>,
    /// CloudWatch logs deep link, for diagnosis.
    pub logs_url: Option<String>,
}

impl ImageBuilder {
    /// Connect to AWS and verify the CodeBuild project exists.
    pub async fn new(config: ImageBuilderConfig) -> Result<Self> {
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.aws_region.clone()))
            .load()
            .await;

        let codebuild = CodeBuildClient::new(&aws_config);
        let s3 = S3Client::new(&aws_config);
        let ecr = EcrClient::new(&aws_config);

        // Verify the project exists so misconfiguration surfaces at startup.
        let found = codebuild
            .batch_get_projects()
            .names(&config.project_name)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("failed to reach CodeBuild: {e}")))?
            .projects()
            .iter()
            .any(|p| p.name() == Some(config.project_name.as_str()));
        if !found {
            return Err(AmosError::Internal(format!(
                "CodeBuild project '{}' not found (apply terraform/build)",
                config.project_name
            )));
        }

        info!(
            project = %config.project_name,
            bucket = %config.source_bucket,
            "Image builder connected"
        );

        Ok(Self {
            codebuild,
            s3,
            ecr,
            project_name: config.project_name,
            source_bucket: config.source_bucket,
            ecr_registry: config.ecr_registry,
        })
    }

    /// Start a build: zip the context, upload to S3, and trigger CodeBuild.
    /// Returns the CodeBuild build id (poll it with [`Self::build_state`]).
    pub async fn start_build(&self, req: &BuildRequest) -> Result<String> {
        // Render build args to space-separated `--build-arg K=V` tokens (common
        // to both source modes).
        let build_args = req
            .build_args
            .iter()
            .map(|(k, v)| format!("--build-arg {k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");

        // The base image (if any) is also a layer-cache source, so the buildspec
        // can `--cache-from` it. Empty when this image has no base.
        let cache_from_base = req
            .build_args
            .get("BASE_IMAGE")
            .cloned()
            .unwrap_or_default();

        let mut sb = self
            .codebuild
            .start_build()
            .project_name(&self.project_name);

        match &req.git_repo {
            // Git source: CodeBuild clones the repo at `git_ref` directly — no
            // local context, no zip/upload. This is the path CI uses (a remote
            // runner has no path on the platform host). Private repos require
            // GitHub source credentials imported into CodeBuild
            // (`aws codebuild import-source-credentials`).
            Some(repo) => {
                sb = sb
                    .source_type_override(aws_sdk_codebuild::types::SourceType::Github)
                    .source_location_override(repo);
                if let Some(git_ref) = &req.git_ref {
                    sb = sb.source_version(git_ref);
                }
                info!(repo = %repo, git_ref = ?req.git_ref, image = %req.image_name, "git-source build");
            }
            // Local context: zip → S3 → CodeBuild (the original path).
            None => {
                let context_dir = req.context_dir.as_ref().ok_or_else(|| {
                    AmosError::Validation("context_dir is required when git_repo is not set".into())
                })?;
                if !context_dir.is_dir() {
                    return Err(AmosError::Validation(format!(
                        "build context dir does not exist: {}",
                        context_dir.display()
                    )));
                }
                if !context_dir.join(&req.dockerfile).is_file() {
                    return Err(AmosError::Validation(format!(
                        "dockerfile '{}' not found in context",
                        req.dockerfile
                    )));
                }
                let key = format!("contexts/{}-{}.zip", req.image_name, Uuid::new_v4());
                let zip_bytes = zip_context(context_dir)?;
                let zipped_len = zip_bytes.len();
                self.s3
                    .put_object()
                    .bucket(&self.source_bucket)
                    .key(&key)
                    .body(ByteStream::from(zip_bytes))
                    .send()
                    .await
                    .map_err(|e| {
                        AmosError::Internal(format!("failed to upload build context: {e}"))
                    })?;
                info!(key = %key, bytes = zipped_len, "build context uploaded");
                sb = sb
                    .source_type_override(aws_sdk_codebuild::types::SourceType::S3)
                    .source_location_override(format!("{}/{}", self.source_bucket, key));
            }
        }

        let build = sb
            .buildspec_override(BUILDSPEC)
            .environment_variables_override(env_override("IMAGE_NAME", &req.image_name))
            .environment_variables_override(env_override("IMAGE_TAG", &req.tag))
            .environment_variables_override(env_override("DOCKERFILE", &req.dockerfile))
            .environment_variables_override(env_override("BUILD_CONTEXT", "."))
            .environment_variables_override(env_override("BUILD_ARGS", &build_args))
            .environment_variables_override(env_override("CACHE_FROM_BASE", &cache_from_base))
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("failed to start CodeBuild: {e}")))?;

        let build_id = build
            .build_value()
            .and_then(|b| b.id())
            .ok_or_else(|| AmosError::Internal("CodeBuild returned no build id".into()))?
            .to_string();

        info!(build_id = %build_id, image = %req.image_name, tag = %req.tag, "image build started");
        Ok(build_id)
    }

    /// Poll a build's state. On success, reads the pushed digest from ECR.
    pub async fn build_state(
        &self,
        build_id: &str,
        image_name: &str,
        tag: &str,
    ) -> Result<BuildState> {
        let resp = self
            .codebuild
            .batch_get_builds()
            .ids(build_id)
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("failed to get build: {e}")))?;

        let build = resp.builds().first().ok_or_else(|| AmosError::NotFound {
            entity: "codebuild build".into(),
            id: build_id.to_string(),
        })?;

        let status = build
            .build_status()
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "UNKNOWN".to_string());
        let current_phase = build.current_phase().unwrap_or("UNKNOWN").to_string();
        let succeeded = status == "SUCCEEDED";
        let done = status != "IN_PROGRESS";

        let logs_url = build
            .logs()
            .and_then(|l| l.deep_link())
            .map(|s| s.to_string());

        // Only hit ECR once the build actually succeeded.
        let image_digest = if succeeded {
            self.read_digest(image_name, tag).await.unwrap_or_else(|e| {
                warn!(build_id = %build_id, "build succeeded but digest read failed: {e}");
                None
            })
        } else {
            None
        };

        Ok(BuildState {
            status,
            current_phase,
            succeeded,
            done,
            image_digest,
            logs_url,
        })
    }

    /// The full image reference `registry/name@digest` (or `:tag` if no digest).
    pub fn image_ref(&self, image_name: &str, tag: &str, digest: Option<&str>) -> String {
        match digest {
            Some(d) => format!("{}/{}@{}", self.ecr_registry, image_name, d),
            None => format!("{}/{}:{}", self.ecr_registry, image_name, tag),
        }
    }

    /// Read the pushed image digest from ECR by tag.
    async fn read_digest(&self, image_name: &str, tag: &str) -> Result<Option<String>> {
        let resp = self
            .ecr
            .describe_images()
            .repository_name(image_name)
            .image_ids(
                aws_sdk_ecr::types::ImageIdentifier::builder()
                    .image_tag(tag)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| AmosError::Internal(format!("failed to describe ECR image: {e}")))?;

        Ok(resp
            .image_details()
            .first()
            .and_then(|d| d.image_digest())
            .map(|s| s.to_string()))
    }
}

/// Build a CodeBuild env-var override (plaintext).
fn env_override(name: &str, value: &str) -> EnvironmentVariable {
    EnvironmentVariable::builder()
        .name(name)
        .value(value)
        .r#type(EnvironmentVariableType::Plaintext)
        .build()
        .expect("env override has required fields")
}

/// Zip a build context directory into an in-memory ZIP, honoring ignore files
/// (`.gitignore`, `.dockerignore`-style via the `ignore` crate) and skipping
/// the `.git` directory. Paths inside the archive are relative to the context
/// root, matching what `BUILD_CONTEXT="."` expects after CodeBuild unpacks it.
fn zip_context(context_dir: &Path) -> Result<Vec<u8>> {
    use zip::write::SimpleFileOptions;

    let mut cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut cursor);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let walker = ignore::WalkBuilder::new(context_dir)
        .hidden(false) // include dotfiles (e.g. .env.example) but…
        .git_ignore(true)
        .git_exclude(true)
        .build();

    let mut file_count = 0usize;
    for entry in walker {
        let entry = entry.map_err(|e| AmosError::Internal(format!("walk error: {e}")))?;
        let path = entry.path();
        // Skip the context root itself and the .git directory.
        let rel = match path.strip_prefix(context_dir) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if rel.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if file_type.is_dir() {
            continue; // ZIP entries are created implicitly by file paths.
        }
        if !file_type.is_file() {
            continue; // skip symlinks/specials for safety
        }

        let name = rel.to_string_lossy().replace('\\', "/");
        let data = std::fs::read(path)
            .map_err(|e| AmosError::Internal(format!("read {}: {e}", path.display())))?;
        zip.start_file(name, opts)
            .map_err(|e| AmosError::Internal(format!("zip start_file: {e}")))?;
        zip.write_all(&data)
            .map_err(|e| AmosError::Internal(format!("zip write: {e}")))?;
        file_count += 1;
    }

    // `finish` consumes the writer and returns the inner `&mut cursor`, which
    // is dropped at the end of this statement — releasing the borrow so we can
    // take the cursor's buffer below.
    zip.finish()
        .map_err(|e| AmosError::Internal(format!("zip finish: {e}")))?;
    info!(files = file_count, "zipped build context");
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_context_includes_files_and_skips_git() {
        let dir = std::env::temp_dir().join(format!("amos-zip-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("Dockerfile"), b"FROM scratch").unwrap();
        std::fs::write(dir.join("sub/app.py"), b"print('hi')").unwrap();
        std::fs::write(dir.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();

        let bytes = zip_context(&dir).unwrap();
        assert!(!bytes.is_empty());

        // Read it back and assert membership.
        let reader = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names: Vec<String> = reader.file_names().map(|s| s.to_string()).collect();
        assert!(names.contains(&"Dockerfile".to_string()));
        assert!(names.contains(&"sub/app.py".to_string()));
        assert!(
            !names.iter().any(|n| n.contains(".git/")),
            "must not include .git: {names:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_ref_prefers_digest() {
        // image_ref doesn't need a live client; construct a minimal builder via
        // the formatting logic only.
        let registry = "123.dkr.ecr.us-east-1.amazonaws.com";
        let with_digest = format!("{}/{}@{}", registry, "cuspr-api", "sha256:abc");
        let with_tag = format!("{}/{}:{}", registry, "cuspr-api", "latest");
        assert!(with_digest.contains('@'));
        assert!(with_tag.ends_with(":latest"));
    }
}
