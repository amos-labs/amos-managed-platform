//! Live integration check for the cloud ImageBuilder against real AWS.
//!
//! Exercises the exact production path — zip context → S3 → CodeBuild →
//! poll → read ECR digest — without standing up the full platform.
//!
//! Run:
//!   AMOS_BUILD_SOURCE_BUCKET=amos-build-sources-637423327454 \
//!   AMOS_ECR_REGISTRY=637423327454.dkr.ecr.us-east-1.amazonaws.com \
//!   AWS_REGION=us-east-1 \
//!   cargo run --example build_image_live -- <context_dir> <image_name> <dockerfile> <tag>

use amos_platform::provisioning::image_builder::{BuildRequest, ImageBuilder, ImageBuilderConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let args: Vec<String> = std::env::args().collect();
    let context_dir = args.get(1).cloned().unwrap_or_else(|| ".".to_string());
    let image_name = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "amos-build-smoke".to_string());
    let dockerfile = args.get(3).cloned().unwrap_or_else(|| "Dockerfile".to_string());
    let tag = args.get(4).cloned().unwrap_or_else(|| "rust-v1".to_string());

    let config = ImageBuilderConfig::from_env()
        .ok_or("AMOS_BUILD_SOURCE_BUCKET not set — cannot run live build")?;
    println!("config: project={} bucket={}", config.project_name, config.source_bucket);

    let builder = ImageBuilder::new(config).await?;

    let req = BuildRequest {
        context_dir: std::path::PathBuf::from(&context_dir),
        dockerfile,
        image_name: image_name.clone(),
        tag: tag.clone(),
        build_args: Default::default(),
    };

    let build_id = builder.start_build(&req).await?;
    println!("started build: {build_id}");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        let s = builder.build_state(&build_id, &image_name, &tag).await?;
        println!("status={} phase={} done={}", s.status, s.current_phase, s.done);
        if s.done {
            println!("succeeded={}", s.succeeded);
            println!("digest={:?}", s.image_digest);
            println!(
                "image_ref={}",
                builder.image_ref(&image_name, &tag, s.image_digest.as_deref())
            );
            if let Some(url) = s.logs_url {
                println!("logs={url}");
            }
            if !s.succeeded {
                std::process::exit(1);
            }
            break;
        }
    }
    Ok(())
}
