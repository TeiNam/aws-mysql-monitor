# 시드 MySQL 8.4 — **관측 대상**.
#
# 계정 전 리전에 RDS 가 0개다([18 §1.3](../../../docs/18-dev-environment.md)).
# 관측할 대상이 없으면 M1 검증 스파이크의 절반(IAM DB Auth·다이제스트 스냅샷 실측)을
# 돌릴 수 없다. 그래서 이 레이어가 대상을 만든다.
#
# **파라미터 그룹이 이 레이어의 핵심이다.** 기본값으로는 우리가 고치려는 절단 버그를
# 그대로 재현하므로, 실측으로 확인한 값을 넣는다([19](../../../docs/19-m1-findings.md)).

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
      Layer       = "60-seed"
      # 우리가 만든 시드 인스턴스임을 태그로 명확히 한다 — 탐색 필터가 이걸 본다.
      DbmonRole = "seed-target"
    }
  }
}

data "aws_vpc" "target" {
  id = var.vpc_id
}

locals {
  name = "dbmon-seed-${var.environment}"
}

# ─────────────────────────────────────────────────────────────────────────────
# 파라미터 그룹
# ─────────────────────────────────────────────────────────────────────────────
# ⚠ `max_digest_length` 와 `performance_schema_max_digest_length` 는 **다른 변수**다
# (M1-13 실측). 전자가 다이제스트 **계산** 버퍼이고 해시를 바꾼다.
# 후자만 올리면 저장 길이만 늘어나고 절단은 그대로다.
#
# 아래 `apply_method = "pending-reboot"` 항목은 **정적 파라미터**다. 재부팅해야 적용된다.
resource "aws_db_parameter_group" "seed" {
  name        = local.name
  family      = "mysql8.4"
  description = "dbmon seed target: performance_schema and slow log tuned for observation"

  # ── performance_schema ────────────────────────────────────────────────────
  parameter {
    name         = "performance_schema"
    value        = "1"
    apply_method = "pending-reboot"
  }

  # 다이제스트 계산 버퍼. 1024(기본)이면 긴 쿼리의 DIGEST 가 인스턴스 설정에 따라 갈라진다.
  parameter {
    name         = "max_digest_length"
    value        = "4096"
    apply_method = "pending-reboot"
  }
  parameter {
    name         = "performance_schema_max_digest_length"
    value        = "4096"
    apply_method = "pending-reboot"
  }

  # `events_statements_current.SQL_TEXT` 길이. 폴백 경로가 여기에 의존한다.
  parameter {
    name         = "performance_schema_max_sql_text_length"
    value        = "4096"
    apply_method = "pending-reboot"
  }

  # 다이제스트 테이블 크기. 기본 10,000 이면 바쁜 인스턴스에서 오버플로가 난다.
  parameter {
    name         = "performance_schema_digests_size"
    value        = "20000"
    apply_method = "pending-reboot"
  }

  # ── 슬로우로그 ────────────────────────────────────────────────────────────
  # 3소스 병합(F5) 검증에 필요하다. RDS 와 같은 FILE 출력을 쓴다.
  parameter {
    name  = "slow_query_log"
    value = "1"
  }
  parameter {
    name  = "long_query_time"
    value = "1"
  }
  parameter {
    name  = "log_output"
    value = "FILE"
  }
  parameter {
    name  = "log_slow_admin_statements"
    value = "1"
  }

  # ── 시나리오 재현 ─────────────────────────────────────────────────────────
  # 락 경합·데드락 시나리오를 짧게 만든다.
  parameter {
    name  = "innodb_lock_wait_timeout"
    value = "10"
  }

  lifecycle {
    create_before_destroy = true
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# 접근 제어
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_security_group" "seed" {
  name        = local.name
  description = "dbmon seed MySQL target"
  vpc_id      = data.aws_vpc.target.id

  lifecycle {
    create_before_destroy = true
  }
}

# 개발자 IP 에서 직접 붙는다. **CIDR 를 명시해야 한다** — 기본값을 두지 않는다.
resource "aws_vpc_security_group_ingress_rule" "from_developer" {
  for_each = toset(var.allowed_client_cidrs)

  security_group_id = aws_security_group.seed.id
  description       = "MySQL from allowed client CIDR"
  cidr_ipv4         = each.value
  from_port         = 3306
  to_port           = 3306
  ip_protocol       = "tcp"
}

# ECS 태스크에서 붙는다. `40-compute` 를 올린 뒤에만 유효하다.
resource "aws_vpc_security_group_ingress_rule" "from_tasks" {
  count = var.task_security_group_id == "" ? 0 : 1

  security_group_id            = aws_security_group.seed.id
  description                  = "MySQL from dbmon tasks"
  referenced_security_group_id = var.task_security_group_id
  from_port                    = 3306
  to_port                      = 3306
  ip_protocol                  = "tcp"
}

# ─────────────────────────────────────────────────────────────────────────────
# 인스턴스
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_db_instance" "seed" {
  identifier     = local.name
  engine         = "mysql"
  engine_version = var.engine_version
  instance_class = var.instance_class

  allocated_storage = 20
  storage_type      = "gp3"
  storage_encrypted = true

  db_name  = "shop"
  username = var.master_username

  # **비밀번호를 Terraform state 에 두지 않는다.** RDS 관리형 마스터 시크릿을 쓰면
  # AWS 가 Secrets Manager 에 만들고 로테이션까지 관리한다.
  # 부트스트랩(07)이 이 시크릿을 읽어 모니터링 계정을 만든다.
  manage_master_user_password = true

  # **IAM DB 인증** — ADR-007 의 전제. 활성화에 재부팅이 필요한지가 M1-5 다.
  iam_database_authentication_enabled = true

  db_subnet_group_name   = var.db_subnet_group_name
  vpc_security_group_ids = [aws_security_group.seed.id]
  parameter_group_name   = aws_db_parameter_group.seed.name

  # dev 는 퍼블릭 액세스를 켠다. 프라이빗 서브넷의 0.0.0.0/0 이 blackhole 이고
  # NAT 가 없어서 다른 경로가 없다([18 §1](../../../docs/18-dev-environment.md)).
  # 보안 그룹이 CIDR 를 좁히므로 인터넷 전체에 열리지는 않는다.
  publicly_accessible = var.publicly_accessible

  # 슬로우로그를 CloudWatch 로 내보낸다. 3소스 병합의 소스 C 다.
  enabled_cloudwatch_logs_exports = ["slowquery", "error"]

  # dev 이므로 백업을 최소화한다. 단 PITR 을 완전히 끄지는 않는다 —
  # 실수로 시드 데이터를 날렸을 때 되돌릴 수 있어야 한다.
  backup_retention_period = var.backup_retention_days
  skip_final_snapshot     = var.environment != "prd"
  deletion_protection     = var.environment == "prd"

  # dev 비용 절감: 다중 AZ 를 쓰지 않는다.
  multi_az = var.environment == "prd"

  auto_minor_version_upgrade = true
  apply_immediately          = var.environment != "prd"

  # Enhanced Monitoring 은 OS 지표(M12-11)에 필요하지만 월 비용이 있다. dev 는 끈다.
  monitoring_interval = 0

  lifecycle {
    ignore_changes = [
      # AWS 가 마이너 버전을 올릴 수 있다. Terraform 이 되돌리지 않게 한다.
      engine_version,
    ]
  }
}

data "aws_caller_identity" "current" {}
