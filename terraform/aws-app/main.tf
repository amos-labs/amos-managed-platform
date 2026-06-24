# AMOS customer-app substrate (Phase 1, Cuspr).
#
# Provisions the per-tenant managed data + ingress for a ported app, reusing
# the existing AMOS shared VPC/ALB/cluster. Dedicated DATA (RDS + ElastiCache
# + Secrets) gives PHI-grade data isolation now; strict per-tenant VPC + peering
# is Phase 2. The ECS services themselves are deployed by the platform.

terraform {
  required_version = ">= 1.5"
  required_providers {
    aws    = { source = "hashicorp/aws", version = "~> 5.0" }
    random = { source = "hashicorp/random", version = "~> 3.0" }
  }
}

provider "aws" {
  region = var.region
}

# ── Inputs (defaults wired to the existing AMOS shared infra) ───────────────
variable "region" { default = "us-east-1" }
variable "tenant" { default = "cuspr" }
variable "vpc_id" { default = "vpc-004397889bd118cbc" } # swarm-infrastructure-vpc
variable "ecs_sg_id" { default = "sg-0967e26d543a5ce47" } # swarm-infrastructure-ecs-sg (the task SG)
variable "db_subnet_group" { default = "swarm-infrastructure-databasesubnetgroup-si0ctinz14uq" }
variable "cache_subnet_group" { default = "swarm-infrastructure-redissubnetgroup-opg3v2wnctda" }
variable "https_listener_arn" {
  default = "arn:aws:elasticloadbalancing:us-east-1:637423327454:listener/app/swarm-infrastructure-alb/f3e43f5728d9a740/d6ee2b21fa81582c"
}
variable "domain" { default = "cuspr.custom.amoslabs.com" }
variable "db_name" { default = "dental_architect" }
variable "db_username" { default = "cuspradmin" }

# ── Data SG: allow the ECS task SG to reach Postgres + Redis ────────────────
resource "aws_security_group" "data" {
  name        = "${var.tenant}-data-sg"
  description = "${var.tenant} RDS + Redis, ingress from ECS tasks only"
  vpc_id      = var.vpc_id

  ingress {
    description     = "Postgres from ECS tasks"
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [var.ecs_sg_id]
  }
  ingress {
    description     = "Redis from ECS tasks"
    from_port       = 6379
    to_port         = 6379
    protocol        = "tcp"
    security_groups = [var.ecs_sg_id]
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  tags = { amos_tenant = var.tenant }
}

resource "random_password" "db" {
  length  = 24
  special = false
}

# ── RDS Postgres (dedicated, single-AZ dev) ─────────────────────────────────
resource "aws_db_instance" "pg" {
  identifier              = "${var.tenant}-postgres"
  engine                  = "postgres"
  engine_version          = "15.13"
  instance_class          = "db.t4g.small"
  allocated_storage       = 20
  storage_type            = "gp3"
  db_name                 = var.db_name
  username                = var.db_username
  password                = random_password.db.result
  db_subnet_group_name    = var.db_subnet_group
  vpc_security_group_ids  = [aws_security_group.data.id]
  multi_az                = false
  publicly_accessible     = false
  skip_final_snapshot     = true
  backup_retention_period = 1
  apply_immediately       = true
  tags                    = { amos_tenant = var.tenant }
}

# ── ElastiCache Redis (dedicated) ───────────────────────────────────────────
resource "aws_elasticache_cluster" "redis" {
  cluster_id           = "${var.tenant}-redis"
  engine               = "redis"
  node_type            = "cache.t4g.micro"
  num_cache_nodes      = 1
  parameter_group_name = "default.redis7"
  port                 = 6379
  subnet_group_name    = var.cache_subnet_group
  security_group_ids   = [aws_security_group.data.id]
  tags                 = { amos_tenant = var.tenant }
}

resource "random_password" "app_secret" {
  length  = 32
  special = false
}
resource "random_password" "worker_secret" {
  length  = 24
  special = false
}

# ── Secrets Manager: connection strings + app secrets ───────────────────────
resource "aws_secretsmanager_secret" "app" {
  name = "${var.tenant}/app-config"
  tags = { amos_tenant = var.tenant }
}

resource "aws_secretsmanager_secret_version" "app" {
  secret_id = aws_secretsmanager_secret.app.id
  secret_string = jsonencode({
    DATABASE_URL  = "postgresql://${var.db_username}:${random_password.db.result}@${aws_db_instance.pg.address}:5432/${var.db_name}"
    REDIS_URL     = "redis://${aws_elasticache_cluster.redis.cache_nodes[0].address}:6379/0"
    SECRET_KEY    = random_password.app_secret.result
    WORKER_SECRET = random_password.worker_secret.result
  })
}

# ── Ingress: frontend target group + host rule on the existing 443 listener ─
# Single public service (frontend); it proxies to the api over localhost in the
# same task. Next.js returns 307 on / (redirect to /login) so allow 200-399.
resource "aws_lb_target_group" "frontend" {
  name        = "${var.tenant}-frontend"
  port        = 3000
  protocol    = "HTTP"
  vpc_id      = var.vpc_id
  target_type = "ip"
  health_check {
    path                = "/"
    matcher             = "200-399"
    interval            = 30
    healthy_threshold   = 2
    unhealthy_threshold = 5
    timeout             = 10
  }
  tags = { amos_tenant = var.tenant }
}

resource "aws_lb_listener_rule" "frontend" {
  listener_arn = var.https_listener_arn
  priority     = 200
  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.frontend.arn
  }
  condition {
    host_header { values = [var.domain] }
  }
  tags = { amos_tenant = var.tenant }
}

# ── Object storage: patient meshes + segmentation results (PHI) ─────────────
resource "aws_s3_bucket" "meshes" {
  bucket = "${var.tenant}-meshes-${data.aws_caller_identity.current.account_id}"
  tags   = { amos_tenant = var.tenant }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "meshes" {
  bucket = aws_s3_bucket.meshes.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_public_access_block" "meshes" {
  bucket                  = aws_s3_bucket.meshes.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

data "aws_caller_identity" "current" {}

# ── Outputs (consumed by the ECS deploy step) ───────────────────────────────
output "db_endpoint" { value = aws_db_instance.pg.address }
output "redis_endpoint" { value = aws_elasticache_cluster.redis.cache_nodes[0].address }
output "secret_arn" { value = aws_secretsmanager_secret.app.arn }
output "frontend_target_group_arn" { value = aws_lb_target_group.frontend.arn }
output "data_sg_id" { value = aws_security_group.data.id }
output "public_url" { value = "https://${var.domain}" }
output "s3_bucket" { value = aws_s3_bucket.meshes.bucket }
