# Aurora MySQL 클러스터 — 환경(prd/dev)마다 writer 1 + reader 1.
#
# reader 엔드포인트 캡처·Aurora 특유의 다이제스트 동작(ADR-019: 3.05+)을 관측하려면
# 리플리카가 실제로 있어야 한다. 인스턴스 2대 중 promotion_tier 가 낮은 쪽이
# 초기 writer 가 되고, 어느 쪽이든 클러스터의 writer/reader 엔드포인트가 추상화한다.

resource "aws_rds_cluster" "aurora" {
  for_each = local.envs

  cluster_identifier = "${local.name}-${each.key}-aurora"
  engine             = "aurora-mysql"
  engine_version     = var.aurora_engine_version

  database_name   = "shop"
  master_username = var.master_username

  # **비밀번호를 Terraform state 에 두지 않는다.** RDS 관리형 마스터 시크릿을 쓰면
  # AWS 가 Secrets Manager 에 만들고 로테이션까지 관리한다.
  manage_master_user_password = true

  # **IAM DB 인증** — ADR-007 의 전제.
  iam_database_authentication_enabled = true

  db_subnet_group_name   = aws_db_subnet_group.seed.name
  vpc_security_group_ids = [aws_security_group.db.id]

  storage_encrypted = true

  # 슬로우로그를 CloudWatch 로 내보낸다. 3소스 병합의 소스 C 다.
  # Aurora 는 /aws/rds/cluster/<id>/<type> 경로를 쓴다 — 로그 그룹을 먼저 만든다.
  enabled_cloudwatch_logs_exports = ["slowquery", "error"]
  depends_on                      = [aws_cloudwatch_log_group.aurora]

  backup_retention_period = var.backup_retention_days

  # 시뮬레이션 prd 다 — 실 프로덕션이 아니라 태그만 prd 인 테스트 플릿이므로
  # destroy 를 막지 않는다. (실 prd 라면 둘 다 반대여야 한다.)
  skip_final_snapshot = true
  deletion_protection = false

  apply_immediately = true

  tags = local.env_tags[each.key]
}

resource "aws_rds_cluster_instance" "aurora" {
  # "prd-1"(writer 후보), "prd-2"(reader), "dev-1", "dev-2"
  for_each = {
    for p in setproduct(var.seed_environments, ["1", "2"]) :
    "${p[0]}-${p[1]}" => { env = p[0], idx = tonumber(p[1]) }
  }

  identifier         = "${local.name}-${each.value.env}-aurora-${each.value.idx}"
  cluster_identifier = aws_rds_cluster.aurora[each.value.env].id
  engine             = aws_rds_cluster.aurora[each.value.env].engine
  engine_version     = aws_rds_cluster.aurora[each.value.env].engine_version
  instance_class     = var.aurora_instance_class

  db_parameter_group_name = aws_db_parameter_group.aurora.name

  # -1 이 초기 writer 가 되도록 승격 우선순위를 준다 (0 이 최우선).
  promotion_tier = each.value.idx - 1

  # 버전 핀과 충돌하는 자동 업그레이드를 끈다 (main.tf 의 핀 근거 참조).
  auto_minor_version_upgrade = false
  apply_immediately          = true

  # Performance Insights·Enhanced Monitoring 은 비용이 있어 dev 는 끈다.
  monitoring_interval = 0

  # 탐색기는 DescribeDBInstances 로 보므로 **인스턴스에도** 환경 태그를 박는다.
  tags = local.env_tags[each.value.env]
}

# ─────────────────────────────────────────────────────────────────────────────
# Aurora MySQL 8.4 (신 메이저) — 기본 prd 1곳, writer 1대
# ─────────────────────────────────────────────────────────────────────────────
# 수집기의 Aurora 8.4 대응(다이제스트·PS 동작이 8.0 계열과 갈리는지)을 검증하는
# 대상이다. 토폴로지 다양성은 8.0 클러스터가 담당하므로 여기는 writer 만 둔다.
resource "aws_rds_cluster" "aurora84" {
  for_each = toset(var.aurora84_environments)

  cluster_identifier = "${local.name}-${each.key}-aurora84"
  engine             = "aurora-mysql"
  engine_version     = var.aurora84_engine_version

  database_name   = "shop"
  master_username = var.master_username

  manage_master_user_password         = true
  iam_database_authentication_enabled = true

  db_subnet_group_name   = aws_db_subnet_group.seed.name
  vpc_security_group_ids = [aws_security_group.db.id]

  storage_encrypted = true

  enabled_cloudwatch_logs_exports = ["slowquery", "error"]
  depends_on                      = [aws_cloudwatch_log_group.aurora84]

  backup_retention_period = var.backup_retention_days

  # 시뮬레이션 prd — 테스트 플릿이므로 destroy 를 막지 않는다.
  skip_final_snapshot = true
  deletion_protection = false

  apply_immediately = true

  tags = local.env_tags[each.key]
}

resource "aws_rds_cluster_instance" "aurora84" {
  for_each = toset(var.aurora84_environments)

  identifier         = "${local.name}-${each.key}-aurora84-1"
  cluster_identifier = aws_rds_cluster.aurora84[each.key].id
  engine             = aws_rds_cluster.aurora84[each.key].engine
  engine_version     = aws_rds_cluster.aurora84[each.key].engine_version
  instance_class     = var.aurora_instance_class

  # 패밀리가 aurora-mysql8.4 라 PG 를 분리한다 (main.tf).
  db_parameter_group_name = aws_db_parameter_group.aurora84[0].name

  promotion_tier = 0

  auto_minor_version_upgrade = false
  apply_immediately          = true
  monitoring_interval        = 0

  tags = local.env_tags[each.key]
}
