# 시드 MySQL 플릿 — **관측 대상**.
#
# 계정 전 리전에 RDS 가 0개다([18 §1.3](../../../.claude/docs/18-dev-environment.md)).
# 관측할 대상이 없으면 M1 검증 스파이크의 절반(IAM DB Auth·다이제스트 스냅샷 실측)을
# 돌릴 수 없다. 그래서 이 레이어가 환경 태그별로 실제 토폴로지를 흉내 낸 플릿을 만든다:
#
#   env(prd/dev) × { Aurora MySQL 클러스터(writer+reader), RDS MySQL(primary+replica) }
#   + prd 에 Aurora MySQL 8.4 (신 메이저) writer 1대 — 8.4 대응 검증용
#
# **전부 프라이빗이다.** db 서브넷은 라우트가 local 뿐이라 인터넷에서 도달 불가하고,
# 접근은 베스천(bastion.tf)의 SSM 포트 포워딩으로만 한다. SSM 인터페이스 엔드포인트
# 3종이 이미 있고 그 SG 가 10.1.0.0/16:443 을 허용하므로(실측) NAT 도 퍼블릭 IP 도
# 필요 없다 — 네트워크 비용 $0 유지.
#
# **파라미터 그룹이 이 레이어의 핵심이다.** 기본값으로는 우리가 고치려는 절단 버그를
# 그대로 재현하므로, 실측으로 확인한 값을 넣는다([19](../../../.claude/docs/19-m1-findings.md)).

terraform {
  required_version = ">= 1.10" # backend.tf 의 use_lockfile (S3 네이티브 락)
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
    tls = {
      source  = "hashicorp/tls"
      version = "~> 4.0"
    }
  }
}

provider "aws" {
  region = var.region
  default_tags {
    tags = {
      Project   = "dbmon"
      ManagedBy = "terraform"
      Layer     = "60-seed"
      # 우리가 만든 시드 리소스임을 명확히 한다 — 비용 배분과 정리가 이걸 본다.
      DbmonRole = "seed-target"
      # DB 리소스는 시뮬레이션 환경(prd/dev)을 리소스 태그로 **덮어쓴다**.
      Environment = var.environment
    }
  }
}

data "aws_vpc" "target" {
  id = var.vpc_id
}

data "aws_caller_identity" "current" {}

locals {
  name = "dbmon-seed"
  envs = toset(var.seed_environments)

  log_types = ["slowquery", "error"]

  # Aurora 는 클러스터 단위 로그 그룹, RDS 는 인스턴스 단위 로그 그룹을 쓴다.
  aurora_log_groups = {
    for p in setproduct(var.seed_environments, local.log_types) :
    "${p[0]}/${p[1]}" => "/aws/rds/cluster/${local.name}-${p[0]}-aurora/${p[1]}"
  }
  aurora84_log_groups = {
    for p in setproduct(var.aurora84_environments, local.log_types) :
    "${p[0]}/${p[1]}" => "/aws/rds/cluster/${local.name}-${p[0]}-aurora84/${p[1]}"
  }
  mysql_log_groups = {
    for p in setproduct(var.seed_environments, ["mysql", "mysql-ro"], local.log_types) :
    "${p[0]}/${p[1]}/${p[2]}" => "/aws/rds/instance/${local.name}-${p[0]}-${p[1]}/${p[2]}"
  }

  # 시뮬레이션 환경 태그. 탐색기가 `env` → `Environment` 순으로 읽으므로 둘 다 박는다.
  env_tags = { for e in var.seed_environments : e => { env = e, Environment = e } }
}

# ─────────────────────────────────────────────────────────────────────────────
# 네트워크 배치
# ─────────────────────────────────────────────────────────────────────────────
# 기존 `dev-rds-subnet-group` 은 우리가 만들지 않았다. destroy 가 남의 리소스를
# 건드리지 않도록 우리 것을 만든다 (README 규약: 기존 리소스는 data 로만).
resource "aws_db_subnet_group" "seed" {
  name_prefix = "${local.name}-"
  subnet_ids  = var.db_subnet_ids
  description = "dbmon seed fleet: db-only subnets (local route only, bastion access)"

  lifecycle {
    create_before_destroy = true
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# 파라미터 그룹 — RDS MySQL 8.4 (인스턴스)
# ─────────────────────────────────────────────────────────────────────────────
# ⚠ `max_digest_length` 와 `performance_schema_max_digest_length` 는 **다른 변수**다
# (M1-13 실측). 전자가 다이제스트 **계산** 버퍼이고 해시를 바꾼다.
# 후자만 올리면 저장 길이만 늘어나고 절단은 그대로다.
#
# `apply_method = "pending-reboot"` 항목은 **정적 파라미터**다. 재부팅해야 적용된다.
resource "aws_db_parameter_group" "mysql" {
  name_prefix = "${local.name}-mysql84-"
  family      = "mysql8.4"
  description = "dbmon seed: performance_schema and slow log tuned for observation"

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

  # **버퍼 풀을 명시적으로 낮춘다.** t4g.micro 는 메모리 1 GiB 이고 RDS 기본
  # `innodb_buffer_pool_size` 는 `DBInstanceClassMemory*3/4`(≈750 MB) 다. 위의
  # performance_schema 설정(다이제스트 20,000 × 4,096 바이트만 대략 100~160 MB)과
  # IAM DB Auth 가 얹히면 mysqld 가 기동에 실패해 인스턴스가 `incompatible-parameters`
  # 로 빠진다 — 재부팅으로 풀리지 않고 파라미터를 되돌려야 한다.
  parameter {
    name         = "innodb_buffer_pool_size"
    value        = "{DBInstanceClassMemory*1/2}"
    apply_method = "pending-reboot"
  }

  # ── 슬로우로그 ────────────────────────────────────────────────────────────
  # 3소스 병합(F5) 검증에 필요하다.
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
# 파라미터 그룹 — Aurora MySQL 3.x (인스턴스 레벨)
# ─────────────────────────────────────────────────────────────────────────────
# 11개 파라미터 전부 aurora-mysql8.0 **인스턴스** PG 에서 수정 가능함을 실측했다
# (2026-08-21, describe-engine-default-parameters). 클러스터 PG 는 기본값을 쓴다.
#
# RDS 쪽과 달리 `innodb_buffer_pool_size` 를 **건드리지 않는다** — Aurora 는
# 스토리지 엔진이 달라 메모리 관리를 자체적으로 하고, t4g.medium(4 GiB)에서는
# performance_schema 오버헤드(~100 MB)가 기동을 위협하지 않는다.
resource "aws_db_parameter_group" "aurora" {
  name_prefix = "${local.name}-aurora80-"
  family      = "aurora-mysql8.0"
  description = "dbmon seed: performance_schema and slow log tuned for observation (Aurora)"

  parameter {
    name         = "performance_schema"
    value        = "1"
    apply_method = "pending-reboot"
  }
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
  parameter {
    name         = "performance_schema_max_sql_text_length"
    value        = "4096"
    apply_method = "pending-reboot"
  }
  parameter {
    name         = "performance_schema_digests_size"
    value        = "20000"
    apply_method = "pending-reboot"
  }

  parameter {
    name  = "slow_query_log"
    value = "1"
  }
  parameter {
    name  = "long_query_time"
    value = "1"
  }
  parameter {
    # Aurora 는 이 파라미터를 static 으로 저장한다(배포 실측 — immediate 로 두면
    # 영구 diff 가 남는다). 어차피 Aurora MySQL 은 FILE 만 지원한다.
    name         = "log_output"
    value        = "FILE"
    apply_method = "pending-reboot"
  }
  parameter {
    name  = "log_slow_admin_statements"
    value = "1"
  }
  parameter {
    name  = "innodb_lock_wait_timeout"
    value = "10"
  }

  lifecycle {
    create_before_destroy = true
  }
}

# Aurora MySQL 8.4 (신 메이저, prd 검증용) — 패밀리가 다르므로 PG 도 분리한다.
# 10개 파라미터 전부 aurora-mysql8.4 에서 수정 가능함을 실측했다 (2026-08-21).
resource "aws_db_parameter_group" "aurora84" {
  count = length(var.aurora84_environments) > 0 ? 1 : 0

  name_prefix = "${local.name}-aurora84-"
  family      = "aurora-mysql8.4"
  description = "dbmon seed: performance_schema and slow log tuned for observation (Aurora MySQL 8.4)"

  parameter {
    name         = "performance_schema"
    value        = "1"
    apply_method = "pending-reboot"
  }
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
  parameter {
    name         = "performance_schema_max_sql_text_length"
    value        = "4096"
    apply_method = "pending-reboot"
  }
  parameter {
    name         = "performance_schema_digests_size"
    value        = "20000"
    apply_method = "pending-reboot"
  }

  parameter {
    name  = "slow_query_log"
    value = "1"
  }
  parameter {
    name  = "long_query_time"
    value = "1"
  }
  parameter {
    name         = "log_output"
    value        = "FILE"
    apply_method = "pending-reboot"
  }
  parameter {
    name  = "log_slow_admin_statements"
    value = "1"
  }
  parameter {
    name  = "innodb_lock_wait_timeout"
    value = "10"
  }

  lifecycle {
    create_before_destroy = true
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# 접근 제어 — 베스천에서만 3306
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_security_group" "db" {
  name_prefix = "${local.name}-db-"
  description = "dbmon seed fleet: MySQL from bastion (and dbmon tasks) only"
  vpc_id      = data.aws_vpc.target.id

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_vpc_security_group_ingress_rule" "db_from_bastion" {
  security_group_id            = aws_security_group.db.id
  description                  = "MySQL from SSM bastion"
  referenced_security_group_id = aws_security_group.bastion.id
  from_port                    = 3306
  to_port                      = 3306
  ip_protocol                  = "tcp"
}

# ECS 태스크에서 붙는다. `40-compute` 를 올린 뒤에만 유효하다.
resource "aws_vpc_security_group_ingress_rule" "db_from_tasks" {
  count = var.task_security_group_id == "" ? 0 : 1

  security_group_id            = aws_security_group.db.id
  description                  = "MySQL from dbmon tasks"
  referenced_security_group_id = var.task_security_group_id
  from_port                    = 3306
  to_port                      = 3306
  ip_protocol                  = "tcp"
}

# ─────────────────────────────────────────────────────────────────────────────
# RDS 로그 그룹 — **보존기간을 미리 박는다**
# ─────────────────────────────────────────────────────────────────────────────
# RDS 가 알아서 만든 로그 그룹은 **Never Expire** 다. `long_query_time = 1` 에
# 부하 생성기까지 돌면 계속 쌓인다(서울 ingest $0.76/GB + 저장). RDS 는 기존 그룹이
# 있으면 재사용하므로 먼저 선언하면 된다. Aurora 는 클러스터 경로를 쓴다.
resource "aws_cloudwatch_log_group" "aurora" {
  for_each          = local.aurora_log_groups
  name              = each.value
  retention_in_days = var.log_retention_days
}

resource "aws_cloudwatch_log_group" "aurora84" {
  for_each          = local.aurora84_log_groups
  name              = each.value
  retention_in_days = var.log_retention_days
}

resource "aws_cloudwatch_log_group" "mysql" {
  for_each          = local.mysql_log_groups
  name              = each.value
  retention_in_days = var.log_retention_days
}

# ─────────────────────────────────────────────────────────────────────────────
# RDS MySQL 마스터 시크릿 — 우리가 만든다 (관리형 시크릿 불가, mysql.tf 참조)
# ─────────────────────────────────────────────────────────────────────────────
# 비밀번호는 write-only 로만 흐른다: TF_VAR → password_wo(RDS) + secret_string_wo(여기).
# state 에는 어느 쪽에도 남지 않는다. 두 RDS primary(prd/dev)가 같은 값을 쓴다 —
# 베스천 뒤에만 있는 테스트 플릿이라 시크릿 하나로 충분하다.
resource "aws_secretsmanager_secret" "mysql_master" {
  name_prefix = "${local.name}-mysql-master-"
  description = "dbmon seed RDS MySQL master password (shared by prd/dev primaries)"

  # 테스트 플릿: destroy 후 name_prefix 재사용이 가능하도록 유예 없이 지운다.
  recovery_window_in_days = 0

  # **부트스트랩이 이 태그를 요구한다** (M3 경로 b, docs/07 §1.2).
  #
  # 앱의 IAM 정책이 `secretsmanager:ResourceTag/dbmon = true` 로 좁혀져 있고, 앱
  # 코드도 같은 태그를 확인한다(`is_tagged_for_dbmon`). 태그가 없으면 정책과 코드
  # 양쪽에서 막힌다 — 임의 ARN 으로 계정의 다른 비밀을 읽는 것을 막기 위해서다.
  #
  # Aurora 는 `manage_master_user_password = true` 라 RDS 관리형 시크릿(`rds!*`)을
  # 쓰고, 그 경로는 이름 규칙으로 좁히므로 태그가 필요 없다.
  tags = {
    dbmon = "true"
  }
}

# 평상시 plan/apply 에 비밀번호 변수를 요구하지 않기 위해, 변수가 없으면 시크릿의
# 현재 값을 ephemeral 로 읽어 쓴다. write-only 쌍(_wo, _wo_version)은 provider 가
# **둘 다 지정**을 강제하고(requiredWith — 2026-08-21 배포 후 plan 에러로 실측),
# 버전이 그대로면 값은 전송되지 않으므로 이 읽기가 로테이션을 일으키지 않는다.
ephemeral "aws_secretsmanager_secret_version" "mysql_master_current" {
  count     = var.mysql_master_password == null ? 1 : 0
  secret_id = aws_secretsmanager_secret.mysql_master.id
}

locals {
  # ponytail: 최초 생성(시크릿에 버전이 아직 없을 때)은 TF_VAR_mysql_master_password
  # 필수다 — 없으면 여기 coalesce 가 plan 에서 실패한다. 이후에는 변수 없이 동작한다.
  mysql_master_password = coalesce(
    var.mysql_master_password,
    try(jsondecode(ephemeral.aws_secretsmanager_secret_version.mysql_master_current[0].secret_string).password, null),
  )
}

resource "aws_secretsmanager_secret_version" "mysql_master" {
  secret_id = aws_secretsmanager_secret.mysql_master.id

  # write-only: version 이 바뀔 때만 전송된다. 로테이션 절차는 variables.tf 참조.
  secret_string_wo = jsonencode({
    username = var.master_username
    password = local.mysql_master_password
  })
  secret_string_wo_version = var.mysql_master_password_version
}
