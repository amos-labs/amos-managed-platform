//! Live integration check for the AwsAppProvisioner against real ECS.
//!
//! Renders a minimal AppSpec to a Fargate task definition + ECS service
//! (internal-only, no ALB), confirms it, then tears it down. Exercises the
//! exact production SDK path (register_task_definition → create_service →
//! describe_services → delete_service) without disturbing real tenant apps.
//!
//! Run:
//!   AWS_APP_ENABLED=true AWS_REGION=us-east-1 \
//!   cargo run --example deploy_aws_live

use std::collections::BTreeMap;

use amos_platform::provisioning::app_spec::{AppSpec, ImageSource, ServiceKind, ServiceSpec};
use amos_platform::provisioning::aws_app::{
    AwsAppProvisioner, AwsAppProvisionerConfig, AwsDeployTarget,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let cfg = AwsAppProvisionerConfig::from_env()
        .ok_or("AWS_APP_ENABLED!=true — refusing to run live ECS deploy")?;
    let provisioner = AwsAppProvisioner::new(cfg).await?;

    // Minimal internal app: one container that just sleeps (uses the tiny image
    // build-as-a-service produced earlier). No ports, no ALB, no secrets.
    let spec = AppSpec {
        name: "amos-renderer-smoke".to_string(),
        version: None,
        services: vec![ServiceSpec {
            name: "app".to_string(),
            kind: ServiceKind::Service,
            image: ImageSource::Pull {
                reference: "637423327454.dkr.ecr.us-east-1.amazonaws.com/amos-build-smoke:rust-v1"
                    .to_string(),
            },
            ports: vec![],
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            command: Some(vec!["sleep".to_string(), "600".to_string()]),
            working_dir: None,
            volumes: vec![],
            depends_on: vec![],
            resources: Default::default(),
            expose_public: false,
            healthcheck: None,
            restart: None,
        }],
    };

    // Small task sizing for the smoke test.
    let target = AwsDeployTarget {
        cpu: Some("256".to_string()),
        memory: Some("512".to_string()),
        ephemeral_storage_gib: Some(21),
        ..Default::default()
    };

    println!("deploying renderer smoke app…");
    let result = provisioner.deploy(&spec, &target).await?;
    println!("task_def_arn = {}", result.task_def_arn);
    println!("service_arn  = {:?}", result.service_arn);
    println!("service_name = {}", result.service_name);

    // Give ECS a moment, then tear it down so we leave nothing behind.
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    println!("tearing down…");
    provisioner.teardown(&result.service_name).await?;
    println!("OK — renderer validated live (task def registered, service created + deleted)");
    Ok(())
}
