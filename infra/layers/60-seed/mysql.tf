# RDS MySQL — 환경(prd/dev)마다 primary 1 + read replica 1.
#
# 리플리카가 있어야 replica lag·read-only 인스턴스의 다이제스트 스냅샷·
# 리플리카 전용 슬로우로그 같은 실전 토폴로지를 관측할 수 있다.

resource "aws_db_instance" "mysql" {
  for_each = local.envs

  identifier     = "${local.name}-${each.key}-mysql"
  engine         = "mysql"
  engine_version = var.mysql_engine_version
  instance_class = var.mysql_instance_class

  allocated_storage = 20
  storage_type      = "gp3"
  storage_encrypted = true

  db_name  = "shop"
  username = var.master_username

  # **관리형 시크릿을 쓰지 않는다** — RDS MySQL 은 관리형 시크릿이 켜진 원본으로
  # 리드 리플리카를 만들 수 없다(2026-08-21 배포에서 실측한 400 에러, variables.tf).
  # 대신 write-only 인자로 주입한다: 비밀번호는 state 에 남지 않고, 값의 정본은
  # 우리가 만든 Secrets Manager 시크릿(main.tf)에 있다. 변수가 없으면 그 시크릿을
  # ephemeral 로 읽어 쌍을 채운다(버전 불변 → 전송 없음). Aurora 는 관리형 유지.
  password_wo         = local.mysql_master_password
  password_wo_version = var.mysql_master_password_version

  iam_database_authentication_enabled = true

  db_subnet_group_name   = aws_db_subnet_group.seed.name
  vpc_security_group_ids = [aws_security_group.db.id]
  parameter_group_name   = aws_db_parameter_group.mysql.name

  # 전부 프라이빗. 접근은 베스천 SSM 터널로만 한다.
  publicly_accessible = false

  enabled_cloudwatch_logs_exports = ["slowquery", "error"]
  depends_on                      = [aws_cloudwatch_log_group.mysql]

  # 리플리카 생성에 원본 백업이 필수다 (variables.tf 에서 1 이상 강제).
  backup_retention_period = var.backup_retention_days

  # 시뮬레이션 prd — 테스트 플릿이므로 destroy 를 막지 않는다.
  skip_final_snapshot = true
  deletion_protection = false

  # 다중 AZ 스탠바이는 MySQL 프로토콜로는 보이지 않고 비용만 2배다.
  # 토폴로지 다양성은 리플리카가 담당한다.
  multi_az = false

  # **끈다.** 켜면 유지보수 창에서 마이너가 올라가 측정 기준선이 조용히 바뀐다.
  # 업그레이드는 `mysql_engine_version` 을 바꾸는 의도적 커밋으로 한다.
  auto_minor_version_upgrade = false
  apply_immediately          = true

  # Enhanced Monitoring 은 OS 지표(M12-11)에 필요하지만 월 비용이 있다. dev 는 끈다.
  monitoring_interval = 0

  tags = local.env_tags[each.key]
}

resource "aws_db_instance" "mysql_replica" {
  for_each = local.envs

  identifier          = "${local.name}-${each.key}-mysql-ro"
  replicate_source_db = aws_db_instance.mysql[each.key].identifier
  instance_class      = var.mysql_instance_class

  # 스토리지·계정은 원본에서 상속된다. 같은 리전 리플리카는
  # db_subnet_group_name 을 지정하면 안 된다(원본 것을 쓴다).
  #
  # 암호화도 원본에서 상속되지만 **값을 생략하면 안 된다**: provider 는 미지정을
  # null 로 읽어 실제값 true 와 diff 를 만들고, 이 속성은 ForceNew 라
  # 다음 apply 에서 리플리카가 조용히 재생성된다(2026-08-21 사후 plan 에서 실측).
  storage_encrypted = true

  vpc_security_group_ids = [aws_security_group.db.id]
  parameter_group_name   = aws_db_parameter_group.mysql.name

  publicly_accessible = false

  iam_database_authentication_enabled = true

  # 리플리카 자신의 슬로우로그도 소스 C 로 내보낸다 (read 워크로드 관측).
  enabled_cloudwatch_logs_exports = ["slowquery", "error"]
  depends_on                      = [aws_cloudwatch_log_group.mysql]

  # 리플리카에는 백업을 두지 않는다 — 원본 PITR 이 복구를 담당한다.
  backup_retention_period = 0

  skip_final_snapshot = true
  deletion_protection = false
  multi_az            = false

  auto_minor_version_upgrade = false
  apply_immediately          = true
  monitoring_interval        = 0

  tags = local.env_tags[each.key]
}
