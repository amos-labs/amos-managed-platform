-- AWS Fargate deploys (deploy_app provider='ecs') need somewhere to record the
-- registered task-definition ARN, the ECS service ARN, and the public URL.
-- A single JSONB column keeps this provider-specific without a column per field
-- (the docker provider leaves it null).

ALTER TABLE app_deployments
    ADD COLUMN IF NOT EXISTS aws_meta JSONB;

COMMENT ON COLUMN app_deployments.aws_meta IS
    'AWS deploy metadata for provider=ecs: {task_def_arn, service_arn, public_url, managed_skipped}.';
