# 저장 기반. **이 레이어만 있으면 로컬 앱이 실제 DynamoDB 에 쓴다** (M0-13b).
#
# 컨테이너 이미지도, ALB 도, ECS 도 필요 없다. 그게 "앱 코드와 독립"의 의미다.

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
      Layer       = "10-foundation"
    }
  }
}

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}

# **기존 VPC 는 data source 로만 참조한다.**
# resource 로 관리하면 destroy 가 다른 워크로드를 지운다 (infra/README.md 참조).
data "aws_partition" "current" {}

data "aws_vpc" "target" {
  id = var.vpc_id
}

locals {
  account_id = data.aws_caller_identity.current.account_id
  suffix     = local.account_id
}

# ─────────────────────────────────────────────────────────────────────────────
# KMS — 고객 관리 키
# ─────────────────────────────────────────────────────────────────────────────
# CMK 를 쓰는 이유: 감사 추적(누가 언제 복호화했는가)과 키 정책으로 접근을 좁힐 수 있다.
#
# ⚠ 키를 잃거나 정책을 잘못 바꾸면 **모든 읽기·쓰기가 AccessDenied 로 실패**한다.
# 그 상황은 재시도로 해결되지 않으므로 앱이 버퍼를 유지하고 사람을 기다린다 (F25).
resource "aws_kms_key" "data" {
  description             = "dbmon 데이터 암호화 (DynamoDB, S3)"
  deletion_window_in_days = 30
  enable_key_rotation     = true

  lifecycle {
    prevent_destroy = true
  }
}

# 키 정책을 **명시한다.** 기본 키 정책은 계정 root 위임뿐이고, CloudWatch Logs 는
# IAM 위임이 아니라 **서비스 주체**로 키를 쓴다. 정책이 없으면 `CreateLogGroup(kmsKeyId=...)`
# 이 거부되어 40-compute 첫 apply 가 로그 그룹에서 즉시 실패한다.
#
# ⚠ 이 정책은 기본 정책을 **덮어쓴다.** root 위임 statement 를 빼면 키가 잠기고
# 되돌릴 방법이 없다 (`prevent_destroy` + 30일 삭제 대기).
data "aws_iam_policy_document" "kms_data" {
  statement {
    sid    = "EnableIAMUserPermissions"
    effect = "Allow"
    principals {
      type        = "AWS"
      identifiers = ["arn:${data.aws_partition.current.partition}:iam::${local.account_id}:root"]
    }
    actions   = ["kms:*"]
    resources = ["*"]
  }

  statement {
    sid    = "AllowCloudWatchLogs"
    effect = "Allow"
    principals {
      type        = "Service"
      identifiers = ["logs.${data.aws_region.current.region}.amazonaws.com"]
    }
    actions = [
      "kms:Encrypt*",
      "kms:Decrypt*",
      "kms:ReEncrypt*",
      "kms:GenerateDataKey*",
      "kms:Describe*",
    ]
    resources = ["*"]

    # 이 계정의 로그 그룹에만 쓰도록 좁힌다 — 다른 계정이 우리 키로 로그를 암호화하는
    # 것을 막는다(confused deputy).
    condition {
      test     = "ArnLike"
      variable = "kms:EncryptionContext:aws:logs:arn"
      values = [
        "arn:${data.aws_partition.current.partition}:logs:${data.aws_region.current.region}:${local.account_id}:log-group:*",
      ]
    }
  }
}

resource "aws_kms_key_policy" "data" {
  key_id = aws_kms_key.data.id
  policy = data.aws_iam_policy_document.kms_data.json

  # **키와 같은 보호를 받아야 한다.** 키에만 `prevent_destroy` 를 걸면 정책만 지워지는
  # 상태가 만들어진다: `AllowCloudWatchLogs` statement 가 사라지면 CMK 로 암호화한
  # 로그 그룹이 동작을 멈추는데, 키는 `prevent_destroy` + 30일 삭제 대기라 되돌릴 수 없다.
  lifecycle {
    prevent_destroy = true
  }
}

resource "aws_kms_alias" "data" {
  name          = "alias/dbmon-data-${var.environment}"
  target_key_id = aws_kms_key.data.key_id
}

# ─────────────────────────────────────────────────────────────────────────────
# DynamoDB — 단일 테이블 2개 ([04](../../../docs/04-data-model.md))
# ─────────────────────────────────────────────────────────────────────────────
# 과금은 **온디맨드**. 쓰기가 정시 롤업 플러시로 버스트하므로 프로비저닝은 스로틀을 부른다
# ([OPEN-Q-13](../../../docs/OPEN-QUESTIONS.md) 에서 3개월 후 재검토).
resource "aws_dynamodb_table" "data" {
  name         = "dbmon-data-${var.environment}"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "PK"
  range_key    = "SK"

  attribute {
    name = "PK"
    type = "S"
  }
  attribute {
    name = "SK"
    type = "S"
  }
  attribute {
    name = "GSI1PK"
    type = "S"
  }
  attribute {
    name = "GSI1SK"
    type = "S"
  }
  attribute {
    name = "GSI2PK"
    type = "S"
  }
  attribute {
    name = "GSI2SK"
    type = "S"
  }

  # 다이제스트 축 + 활성 알림 + 진행 중 레코드.
  #
  # **희소 인덱스**다: `GSI1PK` 속성이 없는 항목은 인덱스에 들어가지 않는다.
  #
  # 엔티티마다 쓰는 방식이 다르다:
  #
  # | 엔티티 | in_flight / firing | 확정 / 해소 |
  # |---|---|---|
  # | `SlowQuery` | `SQS#in_flight` (AP-18 고아 정리, F4) | **`DG#<app_digest>` 로 교체** (AP-3) |
  # | `Alert` | `ALS#firing` (AP-9) | 속성 **제거** → 인덱스에서 탈락 |
  #
  # `SlowQuery` 는 제거가 아니라 **교체**다. 확정된 레코드는 AP-3(다이제스트 → 최근 실행
  # 샘플)에서 여전히 보여야 한다. 제거하면 그 접근 패턴이 조용히 0건을 반환한다.
  # 한 항목은 `GSI1PK` 를 하나만 가질 수 있으므로 상태 전이로 두 패턴을 나눈다
  # ([04 §2.3](../../../docs/04-data-model.md)).
  global_secondary_index {
    name = "GSI1"
    # provider 6.x 에서 GSI 의 `hash_key`/`range_key` 는 deprecated 다.
    key_schema {
      attribute_name = "GSI1PK"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "GSI1SK"
      key_type       = "RANGE"
    }
    projection_type = "INCLUDE"
    # `sql_text` · `plan_json` 을 사영하지 않는다 — 인덱스 크기와 쓰기 비용이 배가 된다.
    # 상세는 기본 테이블에서 GetItem 한다.
    non_key_attributes = [
      "record_id", "instance_id", "env", "started_at_ms", "duration_ms",
      "app_digest", "statement_type", "state", "last_seen_at_ms", "owner_worker",
      "exec_count", "total_time_ms", "severity", "rule_id",
      # F4 고아 스윕이 소유권을 확인하고 정리 대상을 판정하는 데 필요하다.
      "owner_epoch", "thread_id", "abandoned_reason",
    ]
  }

  # 환경 × 실행시간 구간 × 시간 축.
  global_secondary_index {
    name = "GSI2"
    key_schema {
      attribute_name = "GSI2PK"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "GSI2SK"
      key_type       = "RANGE"
    }
    projection_type = "INCLUDE"
    non_key_attributes = [
      "record_id", "instance_id", "env", "started_at_ms", "duration_ms",
      "app_digest", "statement_type", "schema_name", "db_user", "kind",
    ]
  }

  ttl {
    attribute_name = "ttl"
    enabled        = true
  }

  # **PITR 은 증분 내보내기의 전제 조건**이다 (FR-STO-12).
  # 끄면 아카이브 파이프라인 전체가 동작하지 않는다.
  point_in_time_recovery {
    enabled = true
  }

  server_side_encryption {
    enabled     = true
    kms_key_arn = aws_kms_key.data.arn
  }

  lifecycle {
    prevent_destroy = true
  }
}

# 설정·리스·감사. 작고 읽기 편중이며 수명주기가 데이터와 다르다.
resource "aws_dynamodb_table" "config" {
  name         = "dbmon-config-${var.environment}"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "PK"
  range_key    = "SK"

  attribute {
    name = "PK"
    type = "S"
  }
  attribute {
    name = "SK"
    type = "S"
  }

  ttl {
    attribute_name = "ttl"
    enabled        = true
  }

  point_in_time_recovery {
    enabled = true
  }

  server_side_encryption {
    enabled     = true
    kms_key_arn = aws_kms_key.data.arn
  }

  lifecycle {
    prevent_destroy = true
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# S3 — 플랜 오프로드
# ─────────────────────────────────────────────────────────────────────────────
# 300KB 초과 플랜을 여기에 둔다. **키 접두에 만료 티어를 넣어** Lifecycle 을 분리한다 (F27).
#
#   ttl35/<record_id>.zst    → 40일   (일반 SlowQuery. 항목 TTL 35일 + 5일 여유)
#   ttl400/<app_digest>.zst  → 405일  (SLOWEST 고정 샘플)
#
# 불변식: **항목의 TTL 이 참조하는 S3 객체의 Lifecycle 보다 길지 않다.**
# 어기면 "35일 후에도 남는 실행 가능 샘플"의 플랜 참조가 끊긴다.
resource "aws_s3_bucket" "plans" {
  bucket = "dbmon-plans-${var.environment}-${local.suffix}"
}

resource "aws_s3_bucket_public_access_block" "plans" {
  bucket                  = aws_s3_bucket.plans.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "plans" {
  bucket = aws_s3_bucket.plans.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm     = "aws:kms"
      kms_master_key_id = aws_kms_key.data.arn
    }
    # 요청마다 KMS 를 부르지 않는다. 플랜 오프로드가 잦으면 KMS 비용이 무시할 수 없다.
    bucket_key_enabled = true
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "plans" {
  bucket = aws_s3_bucket.plans.id

  rule {
    id     = "ttl35"
    status = "Enabled"
    filter {
      prefix = "ttl35/"
    }
    expiration {
      days = 40
    }
  }

  rule {
    id     = "ttl400"
    status = "Enabled"
    filter {
      prefix = "ttl400/"
    }
    expiration {
      days = 405
    }
  }

  rule {
    id     = "abort-incomplete"
    status = "Enabled"
    filter {}
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# VPC 게이트웨이 엔드포인트 — DynamoDB · S3
# ─────────────────────────────────────────────────────────────────────────────
# **게이트웨이 엔드포인트는 무료다.** 인터페이스 엔드포인트(시간당 $0.01 × AZ)와 다르다.
# NAT 를 거치지 않으므로 데이터 처리 비용($0.045/GB)도 사라진다 — DynamoDB 트래픽이
# 월 수십 GB 이므로 이것만으로 의미 있는 절감이다.
#
# 라우트 테이블은 **호출자가 명시한다.** VPC 의 전체 목록을 쓰면 남의 서브넷 라우트까지
# 건드리고, destroy 가 그 워크로드의 유일한 S3 경로를 끊는다 (`endpoint_route_table_ids`).
resource "aws_vpc_endpoint" "dynamodb" {
  vpc_id            = data.aws_vpc.target.id
  service_name      = "com.amazonaws.${data.aws_region.current.region}.dynamodb"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = var.endpoint_route_table_ids
}

# S3 는 dev VPC 에 **이미 있다.** 기본값이 `false` 인 이유는 `create_s3_gateway_endpoint`
# 설명에 있다. 이미 있는 것을 참조만 한다.
data "aws_vpc_endpoint" "s3_existing" {
  count        = var.create_s3_gateway_endpoint ? 0 : 1
  vpc_id       = data.aws_vpc.target.id
  service_name = "com.amazonaws.${data.aws_region.current.region}.s3"
}

resource "aws_vpc_endpoint" "s3" {
  count             = var.create_s3_gateway_endpoint ? 1 : 0
  vpc_id            = data.aws_vpc.target.id
  service_name      = "com.amazonaws.${data.aws_region.current.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = var.endpoint_route_table_ids
}
