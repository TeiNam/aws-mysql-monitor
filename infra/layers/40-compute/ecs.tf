# ECS 클러스터 · 태스크 정의 · 서비스.

resource "aws_cloudwatch_log_group" "app" {
  name              = "/dbmon/${var.environment}/app"
  retention_in_days = var.log_retention_days
  kms_key_id        = local.foundation.kms_key_arn
}

resource "aws_ecs_cluster" "main" {
  name = local.name

  setting {
    # Container Insights 는 태스크당 월 몇 달러다. dev 에서는 끈다.
    name  = "containerInsights"
    value = var.container_insights_mode
  }
}

resource "aws_ecs_cluster_capacity_providers" "main" {
  cluster_name       = aws_ecs_cluster.main.name
  capacity_providers = ["FARGATE", "FARGATE_SPOT"]

  default_capacity_provider_strategy {
    # dev 는 Spot(약 70% 저렴). 중단되면 ECS 가 새 태스크를 띄우고,
    # 샤드 리스가 TTL 60초 안에 재분배되므로 수집 공백이 짧다.
    #
    # prd 는 On-Demand. 리스 재분배 중 최대 80초의 수집 공백이 생기고,
    # Spot 중단은 그 공백을 예측 불가하게 만든다.
    capacity_provider = var.use_spot ? "FARGATE_SPOT" : "FARGATE"
    weight            = 1
    base              = 0
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# 보안 그룹 — **인바운드는 ALB 에서만**
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_security_group" "task" {
  name = "${local.name}-task"
  # AWS 는 보안 그룹 description 에 ASCII 만 허용한다. 한국어는 주석으로 둔다.
  description = "dbmon ECS tasks"
  vpc_id      = data.aws_vpc.target.id

  # 인바운드 규칙을 여기에 인라인으로 두지 않는다 — 별도 리소스로 두면
  # ALB 유무에 따라 조건부로 만들 수 있다.

  egress {
    # 대상 RDS · AWS API · 외부 채널(Slack/Telegram)
    description = "target RDS, AWS APIs, notification channels"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_vpc_security_group_ingress_rule" "from_alb" {
  count = var.enable_alb ? 1 : 0

  security_group_id = aws_security_group.task.id
  # ALB → 컨테이너 포트
  description                  = "from ALB to container port"
  referenced_security_group_id = aws_security_group.alb[0].id
  from_port                    = var.container_port
  to_port                      = var.container_port
  ip_protocol                  = "tcp"
}

# **ALB 없이 화면을 보는 경로.**
#
# dev 는 `enable_alb = false` 다(네트워크 비용 0). 그러면 인바운드가 하나도 없어서 배포한
# 화면을 볼 방법이 없다 — 로그만 남는다. 그런데 이 VPC 에는 Client VPN 이 있고 태스크는
# VPC 사설 IP 를 갖는다. VPN 쪽 보안 그룹에서 컨테이너 포트를 열면 **ALB 없이 사설 IP 로
# 바로 접속**된다(`http://<태스크 사설 IP>:8080`).
#
# ⚠ 인바운드가 열리면 **인증이 유일한 방어선**이다. ECS 에서는 dev 토큰 발급도 꺼지므로
# (`api::auth` 의 세 번째 조건) `auth_token_secret_arn` 을 반드시 넣어야 한다 — 안 넣으면
# 모든 요청이 401 이고, 넣지 않은 채로 이 규칙만 켜는 것은 의미가 없다.
resource "aws_vpc_security_group_ingress_rule" "from_admin_cidr" {
  for_each = toset(var.admin_ingress_cidrs)

  security_group_id = aws_security_group.task.id
  description       = "from admin CIDR to container port"
  cidr_ipv4         = each.value
  from_port         = var.container_port
  to_port           = var.container_port
  ip_protocol       = "tcp"
}

resource "aws_vpc_security_group_ingress_rule" "from_admin_sg" {
  for_each = toset(var.admin_ingress_security_group_ids)

  security_group_id            = aws_security_group.task.id
  description                  = "from admin/VPN security group to container port"
  referenced_security_group_id = each.value
  from_port                    = var.container_port
  to_port                      = var.container_port
  ip_protocol                  = "tcp"
}

# ─────────────────────────────────────────────────────────────────────────────
# 태스크 정의
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_ecs_task_definition" "app" {
  family                   = local.name
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.task_cpu
  memory                   = var.task_memory
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  runtime_platform {
    operating_system_family = "LINUX"
    # **ARM64 (Graviton).** 같은 성능에 약 20% 저렴하다. 이미지도 arm64 로 빌드한다.
    cpu_architecture = "ARM64"
  }

  container_definitions = jsonencode([{
    name      = "dbmon"
    image     = "${aws_ecr_repository.app.repository_url}:${var.image_tag}"
    essential = true

    # `exec` 형식이라 PID 1 이 dbmon 이다 → SIGTERM 이 셸을 거치지 않고 바로 온다.
    command = ["serve"]

    portMappings = [{
      containerPort = var.container_port
      protocol      = "tcp"
      name          = "http"
    }]

    environment = [
      for k, v in local.app_env : { name = k, value = v }
    ]

    # **토큰은 `environment` 가 아니라 `secrets` 로 넣는다.**
    #
    # `environment` 에 두면 값이 태스크 정의에 평문으로 남고, 태스크 정의는
    # `ecs:DescribeTaskDefinition` 을 가진 누구나 읽는다. `secrets` 는 ARN 만 남기고
    # ECS 에이전트가 기동 시점에 주입한다 — 실행 롤이 그 비밀을 읽을 권한을 갖는다
    # (아래 `execution_secrets` 정책).
    secrets = var.auth_token_secret_arn == "" ? [] : [{
      name      = "DBMON__HTTP__AUTH_TOKEN"
      valueFrom = var.auth_token_secret_arn
    }]

    # **`/healthz` 를 본다. `/readyz` 가 아니다.**
    #
    # standby 워커는 `/readyz` 가 503 이지만 살아 있다(F1). 헬스체크가 `/readyz` 를 보면
    # ECS 가 standby 를 계속 죽이고 다시 띄우는 무한 루프에 빠진다.
    # 라우팅에서 빼는 것은 로드밸런서의 대상 그룹 헬스체크가 담당한다.
    #
    # 이미지에 `curl` 이 없으므로 바이너리의 서브커맨드를 쓴다.
    healthCheck = {
      command     = ["CMD", "/usr/local/bin/dbmon", "healthcheck"]
      interval    = 15
      timeout     = 3
      retries     = 3
      startPeriod = 20
    }

    # **앱의 `shutdown_grace_secs` 보다 커야 한다.** 작으면 SIGKILL 이 먼저 와서
    # 다이제스트 누산기와 쓰기 버퍼가 유실된다.
    stopTimeout = var.stop_timeout_secs

    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.app.name
        "awslogs-region"        = var.region
        "awslogs-stream-prefix" = "dbmon"
        # JSON 로그를 그대로 넘긴다. Logs Insights 가 파싱한다.
        "mode"            = "non-blocking"
        "max-buffer-size" = "4m"
      }
    }

    linuxParameters = {
      # 코어 덤프를 남기지 않는다 — 메모리에 자격증명·리터럴이 있을 수 있다.
      initProcessEnabled = var.enable_ecs_exec
    }

    # **ECS Exec 과 양립하지 않는다.** SSM 에이전트가 컨테이너 파일시스템에 써야 하고,
    # AWS 문서는 "읽기 전용 루트 파일시스템은 어떤 방법으로도 지원하지 않는다"고 명시한다.
    # 앱 자체는 디스크를 쓰지 않으므로(로그는 stdout, zstd 는 메모리) prd 에서는 true 다.
    readonlyRootFilesystem = !var.enable_ecs_exec
    user                   = "10001:10001"
  }])

  lifecycle {
    # 이미지 태그가 바뀌면 새 리비전이 생긴다. 그게 정상 배포 경로다.
    create_before_destroy = true
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# 서비스
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_ecs_service" "app" {
  name            = local.name
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.app.arn

  # **active 1 + standby 1** ([ADR-018](../../../.claude/docs/03-decisions.md)).
  # 리스만으로는 split-brain 을 막지 못하므로 리더 1대만 수집하고 나머지는 standby 다.
  desired_count = var.desired_count

  enable_execute_command = var.enable_ecs_exec

  # **전략을 서비스에 명시한다.** 클러스터 기본 전략에만 의존하면
  # `aws_ecs_cluster_capacity_providers` 와 이 리소스가 형제 노드라 Terraform 이 병렬로
  # 만들고, 기본 전략이 아직 없는 순간에 ECS 가 `launchType=EC2` 로 해석해
  # `No Container Instances were found` 로 apply 가 실패한다. 재실행하면 통과하는
  # 종류라 CI 에서 간헐적으로 터진다.
  capacity_provider_strategy {
    capacity_provider = var.use_spot ? "FARGATE_SPOT" : "FARGATE"
    weight            = 1
    # `base` 는 0 이다. 공급자가 하나뿐이면 1 과 결과가 같지만, 나중에 On-Demand+Spot
    # 혼합으로 갈 때 Spot 항목에 `base=1` 이 남아 있으면 "prd 는 On-Demand" 원칙과
    # 정면으로 충돌한다.
    base = 0
  }

  # 배포 중 최소 가용 태스크. active 가 내려가는 순간 다른 태스크가 리더를 인수한다.
  deployment_minimum_healthy_percent = 50
  deployment_maximum_percent         = 200

  deployment_circuit_breaker {
    enable = true
    # 새 리비전이 헬스체크를 통과하지 못하면 자동 롤백한다.
    rollback = true
  }

  network_configuration {
    subnets         = var.subnet_ids
    security_groups = [aws_security_group.task.id]
    # dev 는 퍼블릭 서브넷 + 퍼블릭 IP 로 IGW 를 통해 나간다 → **NAT 비용 $0**.
    # prd 는 프라이빗 서브넷 + NAT 이므로 false.
    assign_public_ip = var.assign_public_ip
  }

  dynamic "load_balancer" {
    for_each = var.enable_alb ? [1] : []
    content {
      target_group_arn = aws_lb_target_group.app[0].arn
      container_name   = "dbmon"
      container_port   = var.container_port
    }
  }

  # 대상 그룹 의존은 `load_balancer` 블록의 ARN 참조가 암시적으로 만든다.
  # 리스너 의존은 `count=0` 일 때 무해하게 사라진다(리소스 블록 주소를 참조하므로).
  depends_on = [aws_ecs_cluster_capacity_providers.main, aws_lb_listener.https]

  lifecycle {
    # 배포 파이프라인이 task_definition 을 갱신하는 경우 Terraform 이 되돌리지 않게 한다.
    ignore_changes = [desired_count]
  }
}
