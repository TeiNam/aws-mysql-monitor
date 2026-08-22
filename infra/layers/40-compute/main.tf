# 컴퓨트 — ECR + ECS Fargate.
#
# # 왜 EC2 ASG 가 아니라 ECS 인가 (ADR-022)
#
# 초기 설계는 EC2 ASG + systemd 였다. ECS 로 바꾸면 다음이 **사라진다**:
#
# | EC2 + systemd | ECS |
# |---|---|
# | AMI 빌드·패치 파이프라인 | 컨테이너 이미지 하나 |
# | `sd_notify(READY=1)` · `WatchdogSec` | 태스크 정의의 `healthCheck` |
# | ASG Lifecycle Hook (`Terminating:Wait`) | `stopTimeout` |
# | `SetInstanceHealth` 자체 unhealthy 판정 | 헬스체크 실패 → ECS 가 교체 |
# | Launch Template · user-data · CloudWatch agent | 태스크 정의 · `awslogs` |
#
# 대가: 컨테이너 이미지 빌드·푸시가 배포 전제가 된다. 그래서 이 레이어만
# **앱 코드에 의존**하고, 나머지 레이어는 독립이다.
#
# # 이 레이어는 마지막에 apply 한다
#
# M0~M5 는 로컬 `cargo run` 으로 실제 AWS 리소스에 붙어 개발한다.
# 상시 가동이 필요해지는 M6 부터 이 레이어를 올린다.

terraform {
  required_version = ">= 1.9"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }
}

provider "aws" {
  region = var.region
  default_tags {
    tags = {
      Project     = "dbmon"
      Environment = var.environment
      ManagedBy   = "terraform"
      Layer       = "40-compute"
    }
  }
}

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}

# 앞 레이어의 출력을 읽는다. **레이어별 독립 state** 의 연결 지점이다.
data "terraform_remote_state" "foundation" {
  backend = "s3"
  config = {
    bucket = var.state_bucket
    key    = "10-foundation/terraform.tfstate"
    region = var.region
  }
}

data "aws_vpc" "target" {
  id = var.vpc_id
}

locals {
  name       = "dbmon-${var.environment}"
  account_id = data.aws_caller_identity.current.account_id

  foundation = data.terraform_remote_state.foundation.outputs

  # 앱에 주입할 환경변수. **비밀은 넣지 않는다** — Secrets Manager 를 `secrets` 로 참조한다.
  app_env = {
    DBMON__DEPLOYMENT_ENV                 = var.environment
    DBMON__ROLE                           = var.role
    DBMON__AWS__REGION                    = var.region
    DBMON__AWS__ACCOUNT_ID                = local.account_id
    DBMON__STORAGE__DATA_TABLE            = local.foundation.data_table
    DBMON__STORAGE__CONFIG_TABLE          = local.foundation.config_table
    # `DBMON__STORAGE__PLAN_BUCKET` 을 주지 않는다 — 앱에 그 설정이 없다.
    # 주면 `deny_unknown_fields` 에 걸려 **기동이 실패한다.**
    DBMON__DISCOVERY__ALLOWED_VPC_IDS     = join(",", var.allowed_vpc_ids)
    DBMON__HTTP__PORT                     = tostring(var.container_port)
    DBMON__HTTP__SHUTDOWN_GRACE_SECS      = tostring(var.shutdown_grace_secs)
    DBMON__HTTP__DEREGISTRATION_WAIT_SECS = tostring(var.enable_alb ? var.deregistration_wait_secs : 0)
    DBMON_LOG                             = var.log_level
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# ECR
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_ecr_repository" "app" {
  name                 = local.name
  image_tag_mutability = "IMMUTABLE" # 같은 태그로 다른 이미지를 밀어넣을 수 없다

  image_scanning_configuration {
    scan_on_push = true
  }

  encryption_configuration {
    encryption_type = "KMS"
    kms_key         = local.foundation.kms_key_arn
  }
}

# 태그 없는 이미지가 무한히 쌓이면 스토리지 비용이 조용히 늘어난다.
resource "aws_ecr_lifecycle_policy" "app" {
  repository = aws_ecr_repository.app.name
  policy = jsonencode({
    rules = [
      {
        rulePriority = 1
        description  = "태그 없는 이미지는 1일 후 삭제"
        selection = {
          tagStatus   = "untagged"
          countType   = "sinceImagePushed"
          countUnit   = "days"
          countNumber = 1
        }
        action = { type = "expire" }
      },
      {
        rulePriority = 2
        description  = "최근 20개만 유지 — 롤백 대상은 그 안에 있다"
        selection = {
          tagStatus   = "any"
          countType   = "imageCountMoreThan"
          countNumber = 20
        }
        action = { type = "expire" }
      },
    ]
  })
}
