# ALB — **조건부**다. dev 에서는 만들지 않는다.
#
# dev 접근 모델은 ECS Exec 또는 SSM 포트 포워딩이고, ALB($16/월) + ACM 인증서 +
# Route53 레코드를 만들지 않아 **네트워크 비용이 $0** 이다
# ([18 §4](../../../.claude/docs/18-dev-environment.md)).
#
# 그 대가로 dev 에서 검증할 수 없는 것이 있다: ALB 헬스체크·등록 해제 지연·유휴
# 타임아웃·WebSocket 업그레이드. → [OPEN-Q-21](../../../.claude/docs/OPEN-QUESTIONS.md).
# M12 에서 하루 apply 해 확인하고 destroy 한다(비용 $1 미만).

resource "aws_security_group" "alb" {
  count       = var.enable_alb ? 1 : 0
  name_prefix = "${local.name}-alb-"
  description = "dbmon ALB"
  # 인바운드는 var.alb_ingress_cidr 로 좁힌다.
  vpc_id = data.aws_vpc.target.id

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_vpc_security_group_ingress_rule" "alb_https" {
  count = var.enable_alb ? 1 : 0

  security_group_id = aws_security_group.alb[0].id
  # 허용된 CIDR 에서 HTTPS
  description = "HTTPS from allowed CIDR"
  # **`0.0.0.0/0` 를 기본값으로 두지 않는다.** 관측 도구가 인터넷에 열려 있으면
  # 인증 이전 단계에서 이미 공격 표면이다.
  cidr_ipv4   = var.alb_ingress_cidr
  from_port   = 443
  to_port     = 443
  ip_protocol = "tcp"
}

resource "aws_vpc_security_group_egress_rule" "alb_to_task" {
  count = var.enable_alb ? 1 : 0

  security_group_id = aws_security_group.alb[0].id
  # ALB → 태스크
  description                  = "to dbmon tasks"
  referenced_security_group_id = aws_security_group.task.id
  from_port                    = var.container_port
  to_port                      = var.container_port
  ip_protocol                  = "tcp"
}

resource "aws_lb" "app" {
  count = var.enable_alb ? 1 : 0

  name               = local.name
  load_balancer_type = "application"
  subnets            = var.alb_subnet_ids
  security_groups    = [aws_security_group.alb[0].id]

  # WebSocket 연결이 유휴로 끊기지 않아야 한다. 앱이 하트비트를 보내지만
  # 기본 60초는 너무 짧다.
  idle_timeout = 300

  drop_invalid_header_fields = true
  enable_deletion_protection = var.environment == "prd"
}

resource "aws_lb_target_group" "app" {
  count = var.enable_alb ? 1 : 0

  name_prefix = substr(local.name, 0, 6) # 대상 그룹 name_prefix 는 6자 이하다
  port        = var.container_port
  protocol    = "HTTP"
  target_type = "ip" # awsvpc 모드는 IP 대상이다
  vpc_id      = data.aws_vpc.target.id

  # **`/readyz` 를 본다. `/healthz` 가 아니다.**
  #
  # standby 워커는 데이터를 갖고 있지 않으므로 트래픽을 받으면 안 된다(F1).
  # `/readyz` 503 이 라우팅에서 빼 준다. 컨테이너 헬스체크는 `/healthz` 를 보므로
  # standby 가 죽지는 않는다 — 두 헬스체크의 역할이 다르다.
  health_check {
    path                = "/readyz"
    protocol            = "HTTP"
    matcher             = "200"
    interval            = 15
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 2
  }

  # 종료 시 앱이 `deregistration_wait_secs` 만큼 기다리므로 그보다 짧게 둔다.
  deregistration_delay = 20

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_lb_listener" "https" {
  count = var.enable_alb ? 1 : 0

  load_balancer_arn = aws_lb.app[0].arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = var.acm_certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.app[0].arn
  }
}

# HTTP 는 리다이렉트만 한다. 평문으로 토큰이 오갈 여지를 두지 않는다.
resource "aws_lb_listener" "http_redirect" {
  count = var.enable_alb ? 1 : 0

  load_balancer_arn = aws_lb.app[0].arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type = "redirect"
    redirect {
      port        = "443"
      protocol    = "HTTPS"
      status_code = "HTTP_301"
    }
  }
}
