# 20-data — S3 Tables(Iceberg) · Athena 워크그룹 · Glue 카탈로그
#
# **언제 필요한가**: M4 (아카이브)
#
# 이 레이어는 아직 리소스를 만들지 않는다. 골격만 두는 이유는 두 가지다.
# 1. `terraform validate` 가 통과하는지로 **레이어 독립성**을 검증할 수 있다 (M0-13)
# 2. apply 순서와 의존 관계를 코드로 남긴다 — README 만으로는 잊는다
#
# 리소스는 해당 마일스톤에서 추가한다. 미리 만들면 쓰지 않는 리소스에 비용이 든다.

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
      Layer       = "20-data"
    }
  }
}
