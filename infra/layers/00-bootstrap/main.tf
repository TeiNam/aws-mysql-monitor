# state 백엔드. **이 레이어만 로컬 state 로 시작**한다 (닭과 달걀).
#
# apply 후 `terraform init -migrate-state` 로 자기 state 를 S3 로 옮긴다.
# 그 다음부터 모든 레이어가 원격 state 를 쓴다.

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

  # 전 리소스에 태그를 붙인다. 비용 배분과 "이건 누가 만들었나"에 답하기 위해.
  default_tags {
    tags = {
      Project     = "dbmon"
      Environment = var.environment
      ManagedBy   = "terraform"
      Layer       = "00-bootstrap"
    }
  }
}

data "aws_caller_identity" "current" {}

locals {
  # 계정 ID 를 접미로 붙여 전역 유일성을 확보한다.
  state_bucket = "dbmon-tfstate-${data.aws_caller_identity.current.account_id}"
}

resource "aws_s3_bucket" "state" {
  bucket = local.state_bucket

  # state 를 잃으면 인프라를 코드로 재현할 수 없다. import 로 복구할 수는 있지만
  # 그건 며칠짜리 작업이다.
  lifecycle {
    prevent_destroy = true
  }
}

resource "aws_s3_bucket_versioning" "state" {
  bucket = aws_s3_bucket.state.id
  versioning_configuration {
    # state 손상 시 이전 버전으로 되돌릴 수 있어야 한다.
    status = "Enabled"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "state" {
  bucket = aws_s3_bucket.state.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
    bucket_key_enabled = true
  }
}

# **버킷 수준**에서만 퍼블릭 액세스를 차단한다.
# 계정 수준 설정은 건드리지 않는다 — 같은 계정에 프로덕션 워크로드가 있다.
resource "aws_s3_bucket_public_access_block" "state" {
  bucket                  = aws_s3_bucket.state.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# state 는 오래된 버전을 영구 보관할 이유가 없다. 90일이면 사고 복구에 충분하다.
resource "aws_s3_bucket_lifecycle_configuration" "state" {
  bucket = aws_s3_bucket.state.id
  rule {
    id     = "expire-old-versions"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration {
      noncurrent_days = 90
    }
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }
}

# 잠금 테이블.
#
# S3 네이티브 락(`use_lockfile`)이 6.x 에서 쓸 수 있지만 DynamoDB 락을 유지한다:
# 여러 사람이 동시에 apply 하는 상황에서 검증된 경로이고, 비용이 온디맨드로 사실상 0 이다.
resource "aws_dynamodb_table" "locks" {
  name         = "dbmon-tfstate-locks"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "LockID"

  attribute {
    name = "LockID"
    type = "S"
  }

  lifecycle {
    prevent_destroy = true
  }
}
