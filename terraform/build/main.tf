# AMOS image-build substrate — "build-as-a-service".
#
# A single reusable CodeBuild project that AMOS triggers (via the platform's
# ImageBuilder / the MCP `build_image` tool) to build a tenant app's container
# image in the cloud and push it to ECR. The agent never runs docker/buildx/
# crane locally — it hands a build context and gets back an image digest.
#
# Source is S3 (a zipped build context the platform uploads); per-build details
# (ECR repo, tag, dockerfile, context) come in as env-var overrides on
# start-build, so this one project serves every tenant and every image.

terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 5.0" }
  }
}

provider "aws" {
  region = var.region
}

variable "region" { default = "us-east-1" }
variable "project_name" { default = "amos-image-builder" }

data "aws_caller_identity" "current" {}

locals {
  account_id   = data.aws_caller_identity.current.account_id
  ecr_registry = "${local.account_id}.dkr.ecr.${var.region}.amazonaws.com"
}

# ── S3 bucket for zipped build contexts ─────────────────────────────────────
resource "aws_s3_bucket" "sources" {
  bucket = "amos-build-sources-${local.account_id}"
  tags   = { amos_component = "build" }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "sources" {
  bucket = aws_s3_bucket.sources.id
  rule {
    apply_server_side_encryption_by_default { sse_algorithm = "AES256" }
  }
}

resource "aws_s3_bucket_public_access_block" "sources" {
  bucket                  = aws_s3_bucket.sources.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# Build contexts are transient — expire them so the bucket doesn't grow forever.
resource "aws_s3_bucket_lifecycle_configuration" "sources" {
  bucket = aws_s3_bucket.sources.id
  rule {
    id     = "expire-contexts"
    status = "Enabled"
    filter { prefix = "contexts/" }
    expiration { days = 7 }
  }
}

# ── IAM role for the CodeBuild project ──────────────────────────────────────
resource "aws_iam_role" "build" {
  name = "${var.project_name}-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "codebuild.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
  tags = { amos_component = "build" }
}

# Scoped permissions: pull source from the build bucket, push images to ECR,
# write CloudWatch logs. ECR auth token is account-wide (required); push is
# scoped to the account's repositories.
resource "aws_iam_role_policy" "build" {
  name = "${var.project_name}-policy"
  role = aws_iam_role.build.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "Logs"
        Effect   = "Allow"
        Action   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/codebuild/${var.project_name}*"
      },
      {
        Sid      = "EcrAuth"
        Effect   = "Allow"
        Action   = "ecr:GetAuthorizationToken"
        Resource = "*"
      },
      {
        Sid    = "EcrPush"
        Effect = "Allow"
        Action = [
          "ecr:BatchCheckLayerAvailability",
          "ecr:InitiateLayerUpload",
          "ecr:UploadLayerPart",
          "ecr:CompleteLayerUpload",
          "ecr:PutImage",
          "ecr:BatchGetImage",
          "ecr:GetDownloadUrlForLayer",
          "ecr:CreateRepository",
          "ecr:DescribeRepositories",
          "ecr:DescribeImages"
        ]
        Resource = "arn:aws:ecr:${var.region}:${local.account_id}:repository/*"
      },
      {
        Sid      = "SourceRead"
        Effect   = "Allow"
        Action   = ["s3:GetObject", "s3:GetObjectVersion"]
        Resource = "${aws_s3_bucket.sources.arn}/*"
      }
    ]
  })
}

# ── The reusable CodeBuild project ──────────────────────────────────────────
# ARM64 build environment (Fargate runs ARM64), privileged for Docker. The
# buildspec is inline and fully parameterized by env overrides supplied at
# start-build time.
resource "aws_codebuild_project" "builder" {
  name          = var.project_name
  description   = "AMOS reusable image builder (S3 context -> ECR), driven by start-build env overrides."
  service_role  = aws_iam_role.build.arn
  build_timeout = 60

  artifacts { type = "NO_ARTIFACTS" }

  environment {
    compute_type    = "BUILD_GENERAL1_LARGE" # ARM requires LARGE
    image           = "aws/codebuild/amazonlinux2-aarch64-standard:3.0"
    type            = "ARM_CONTAINER"
    privileged_mode = true

    environment_variable {
      name  = "ECR_REGISTRY"
      value = local.ecr_registry
    }
    # Defaults; every start-build overrides these per image.
    environment_variable {
      name  = "IMAGE_NAME"
      value = "placeholder"
    }
    environment_variable {
      name  = "IMAGE_TAG"
      value = "latest"
    }
    environment_variable {
      name  = "DOCKERFILE"
      value = "Dockerfile"
    }
    environment_variable {
      name  = "BUILD_CONTEXT"
      value = "."
    }
    # Space-separated `--build-arg K=V` tokens, supplied per build.
    environment_variable {
      name  = "BUILD_ARGS"
      value = ""
    }
  }

  # S3 source; the actual object is set via sourceLocationOverride per build.
  source {
    type     = "S3"
    location = "${aws_s3_bucket.sources.bucket}/contexts/"
    buildspec = <<-BUILDSPEC
      version: 0.2
      phases:
        pre_build:
          commands:
            - echo "Logging in to ECR $ECR_REGISTRY"
            - aws ecr get-login-password --region $AWS_DEFAULT_REGION | docker login --username AWS --password-stdin $ECR_REGISTRY
            - aws ecr describe-repositories --repository-names "$IMAGE_NAME" >/dev/null 2>&1 || aws ecr create-repository --repository-name "$IMAGE_NAME" >/dev/null
        build:
          commands:
            - echo "Building $ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG (arm64) from $DOCKERFILE in $BUILD_CONTEXT (args: $BUILD_ARGS)"
            - docker build --platform linux/arm64 $BUILD_ARGS -f "$DOCKERFILE" -t "$ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG" "$BUILD_CONTEXT"
        post_build:
          commands:
            - docker push "$ECR_REGISTRY/$IMAGE_NAME:$IMAGE_TAG"
            - DIGEST=$(aws ecr describe-images --repository-name "$IMAGE_NAME" --image-ids imageTag="$IMAGE_TAG" --query 'imageDetails[0].imageDigest' --output text)
            - echo "PUSHED_DIGEST=$DIGEST"
    BUILDSPEC
  }

  logs_config {
    cloudwatch_logs {
      group_name  = "/aws/codebuild/${var.project_name}"
      stream_name = "build"
    }
  }

  tags = { amos_component = "build" }
}

# ── Outputs (consumed by the platform ImageBuilder) ─────────────────────────
output "project_name" { value = aws_codebuild_project.builder.name }
output "source_bucket" { value = aws_s3_bucket.sources.bucket }
output "ecr_registry" { value = local.ecr_registry }
output "build_role_arn" { value = aws_iam_role.build.arn }
