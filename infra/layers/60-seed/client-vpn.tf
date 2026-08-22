# Client VPN — 로컬 개발기가 시드 플릿 9대 전부에 **실제 엔드포인트 호스트네임으로**
# 직접 붙는 경로. SSM 터널(scripts/db-tunnel.sh)은 타깃당 1개라 수집기 개발엔
# 불편하고, VPN 은 DNS 를 VPC 리졸버로 푸시해 RDS 프라이빗 호스트네임이 그대로 풀린다.
#
# 비용: subnet association 이 과금 단위다(시간당, 연결 없어도). endpoint 자체는 무료.
# → `vpn_enabled=false` 로 association/인가만 내리면 비용 0, 설정은 보존된다.
#
# 인증: 1인 dev 환경이라 mutual TLS. 인증서 체인을 TF tls 프로바이더로 만든다.
# ⚠ **개인키가 state 에 남는다** (ponytail: dev 전용 타협 — state 는 암호화된 S3 +
# 계정 접근 통제. prd 급으로 가려면 easy-rsa 외부 생성 + ARN 변수 주입으로 전환).

# ─────────────────────────────────────────────────────────────────────────────
# 인증서 체인: CA → 서버 / 클라이언트
# ─────────────────────────────────────────────────────────────────────────────
resource "tls_private_key" "vpn_ca" {
  algorithm = "RSA"
  rsa_bits  = 2048
}

resource "tls_self_signed_cert" "vpn_ca" {
  private_key_pem = tls_private_key.vpn_ca.private_key_pem

  subject {
    common_name  = "dbmon-seed VPN CA"
    organization = "dbmon"
  }

  is_ca_certificate     = true
  validity_period_hours = 87600 # 10년 — dev 인프라 수명보다 길다
  allowed_uses          = ["cert_signing", "crl_signing"]
}

resource "tls_private_key" "vpn_server" {
  algorithm = "RSA"
  rsa_bits  = 2048
}

resource "tls_cert_request" "vpn_server" {
  private_key_pem = tls_private_key.vpn_server.private_key_pem

  subject {
    common_name  = "vpn.dbmon-seed.internal"
    organization = "dbmon"
  }
}

resource "tls_locally_signed_cert" "vpn_server" {
  cert_request_pem   = tls_cert_request.vpn_server.cert_request_pem
  ca_private_key_pem = tls_private_key.vpn_ca.private_key_pem
  ca_cert_pem        = tls_self_signed_cert.vpn_ca.cert_pem

  validity_period_hours = 87600
  allowed_uses          = ["key_encipherment", "digital_signature", "server_auth"]
}

resource "tls_private_key" "vpn_client" {
  algorithm = "RSA"
  rsa_bits  = 2048
}

resource "tls_cert_request" "vpn_client" {
  private_key_pem = tls_private_key.vpn_client.private_key_pem

  subject {
    common_name  = "client.dbmon-seed.internal"
    organization = "dbmon"
  }
}

resource "tls_locally_signed_cert" "vpn_client" {
  cert_request_pem   = tls_cert_request.vpn_client.cert_request_pem
  ca_private_key_pem = tls_private_key.vpn_ca.private_key_pem
  ca_cert_pem        = tls_self_signed_cert.vpn_ca.cert_pem

  validity_period_hours = 87600
  allowed_uses          = ["key_encipherment", "digital_signature", "client_auth"]
}

resource "aws_acm_certificate" "vpn_server" {
  private_key       = tls_private_key.vpn_server.private_key_pem
  certificate_body  = tls_locally_signed_cert.vpn_server.cert_pem
  certificate_chain = tls_self_signed_cert.vpn_ca.cert_pem

  tags = { Name = "${local.name}-vpn-server" }
}

# 클라이언트 쪽 신뢰 체인. 같은 CA 로 발급하므로 이 인증서의 체인이 곧 클라이언트 CA 다.
resource "aws_acm_certificate" "vpn_client" {
  private_key       = tls_private_key.vpn_client.private_key_pem
  certificate_body  = tls_locally_signed_cert.vpn_client.cert_pem
  certificate_chain = tls_self_signed_cert.vpn_ca.cert_pem

  tags = { Name = "${local.name}-vpn-client" }
}

# ─────────────────────────────────────────────────────────────────────────────
# 접근 제어 — VPN 클라이언트에서만 3306
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_security_group" "vpn" {
  name_prefix = "${local.name}-vpn-"
  description = "dbmon seed Client VPN endpoint ENI: egress to DB and VPC DNS only"
  vpc_id      = data.aws_vpc.target.id

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_vpc_security_group_egress_rule" "vpn_mysql" {
  security_group_id            = aws_security_group.vpn.id
  description                  = "MySQL to seed fleet"
  referenced_security_group_id = aws_security_group.db.id
  from_port                    = 3306
  to_port                      = 3306
  ip_protocol                  = "tcp"
}

# 클라이언트 DNS 질의가 VPC 리졸버(VPC+2)로 나간다.
resource "aws_vpc_security_group_egress_rule" "vpn_dns_udp" {
  security_group_id = aws_security_group.vpn.id
  description       = "DNS to VPC resolver"
  cidr_ipv4         = "${cidrhost(data.aws_vpc.target.cidr_block, 2)}/32"
  from_port         = 53
  to_port           = 53
  ip_protocol       = "udp"
}

resource "aws_vpc_security_group_egress_rule" "vpn_dns_tcp" {
  security_group_id = aws_security_group.vpn.id
  description       = "DNS to VPC resolver (TCP fallback)"
  cidr_ipv4         = "${cidrhost(data.aws_vpc.target.cidr_block, 2)}/32"
  from_port         = 53
  to_port           = 53
  ip_protocol       = "tcp"
}

# **VPN → ECS 태스크(화면).**
#
# `40-compute` 를 `enable_alb = false` 로 올리면 인바운드 경로가 없어서 배포한 화면을 볼
# 방법이 없다. 태스크는 VPC 사설 IP 를 가지므로 VPN 에서 컨테이너 포트로 나갈 수 있으면
# `http://<태스크 사설 IP>:8080` 으로 바로 붙는다.
#
# ⚠ 이 이그레스가 없으면 **태스크 쪽 인바운드를 열어도 안 된다.** Client VPN 은 클라이언트
# 트래픽을 VPN ENI 로 NAT 하므로 출처가 이 SG 이고, 이 SG 의 이그레스가 DB·DNS 로만
# 열려 있었다 — 실측으로 그 조합에 걸렸다(태스크 SG 를 열었는데도 연결이 안 됐다).
resource "aws_vpc_security_group_egress_rule" "vpn_to_tasks" {
  count = var.task_security_group_id == "" ? 0 : 1

  security_group_id            = aws_security_group.vpn.id
  description                  = "to dbmon tasks (web UI)"
  referenced_security_group_id = var.task_security_group_id
  from_port                    = var.task_container_port
  to_port                      = var.task_container_port
  ip_protocol                  = "tcp"
}

resource "aws_vpc_security_group_ingress_rule" "db_from_vpn" {
  security_group_id            = aws_security_group.db.id
  description                  = "MySQL from Client VPN"
  referenced_security_group_id = aws_security_group.vpn.id
  from_port                    = 3306
  to_port                      = 3306
  ip_protocol                  = "tcp"
}

# ─────────────────────────────────────────────────────────────────────────────
# 엔드포인트
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_cloudwatch_log_group" "vpn" {
  name              = "/aws/clientvpn/${local.name}"
  retention_in_days = var.log_retention_days
}

resource "aws_ec2_client_vpn_endpoint" "seed" {
  description            = "dbmon seed fleet access"
  server_certificate_arn = aws_acm_certificate.vpn_server.arn
  client_cidr_block      = var.vpn_client_cidr

  authentication_options {
    type                       = "certificate-authentication"
    root_certificate_chain_arn = aws_acm_certificate.vpn_client.arn
  }

  # split tunnel: VPC 행 트래픽만 VPN 을 탄다. 로컬 인터넷은 그대로.
  split_tunnel = true

  # RDS 프라이빗 호스트네임 해석 — 이게 VPN 을 쓰는 이유의 절반이다.
  dns_servers = [cidrhost(data.aws_vpc.target.cidr_block, 2)]

  vpc_id             = data.aws_vpc.target.id
  security_group_ids = [aws_security_group.vpn.id]

  session_timeout_hours = 12

  transport_protocol = "udp"

  connection_log_options {
    enabled              = true
    cloudwatch_log_group = aws_cloudwatch_log_group.vpn.name
  }

  tags = { Name = "${local.name}-vpn" }
}

# ─────────────────────────────────────────────────────────────────────────────
# association + 인가 — `vpn_enabled` 로 켜고 끈다 (과금 단위)
# ─────────────────────────────────────────────────────────────────────────────
# association 이 생기면 VPC 전체 라우트가 자동 추가된다. **인가 규칙이 실제 게이트다**
# — DB 서브넷 2개 + DNS 리졸버 /32 만 연다. VPC 의 나머지(베스천 포함)는 닫힌 채다.
resource "aws_ec2_client_vpn_network_association" "seed" {
  count = var.vpn_enabled ? 1 : 0

  client_vpn_endpoint_id = aws_ec2_client_vpn_endpoint.seed.id
  subnet_id              = var.bastion_subnet_id # priv-a. association 은 AZ 당 과금이라 1개만
}

resource "aws_ec2_client_vpn_authorization_rule" "db_subnets" {
  for_each = var.vpn_enabled ? toset(var.db_subnet_cidrs) : toset([])

  client_vpn_endpoint_id = aws_ec2_client_vpn_endpoint.seed.id
  target_network_cidr    = each.value
  authorize_all_groups   = true
  description            = "seed DB subnets"
}

# **ECS 태스크 서브넷 인가.**
#
# 인가 규칙은 목적지 CIDR 로 판정한다 — DB 서브넷만 인가하면 같은 VPC 라도 태스크
# 서브넷으로는 못 간다(실측: 3306 은 되는데 태스크 8080 이 막혔고, 태스크·VPN 양쪽
# 보안 그룹을 다 열어도 그대로였다). 화면을 VPN 으로 보려면 이 인가가 필요하다.
#
# 비워 두면 규칙이 생기지 않는다 — `40-compute` 를 올리지 않은 환경에서는 그게 맞다.
resource "aws_ec2_client_vpn_authorization_rule" "task_subnets" {
  for_each = var.vpn_enabled ? toset(var.task_subnet_cidrs) : toset([])

  client_vpn_endpoint_id = aws_ec2_client_vpn_endpoint.seed.id
  target_network_cidr    = each.value
  authorize_all_groups   = true
  description            = "dbmon ECS task subnets (web UI)"
}

# DNS 리졸버 인가 — 이게 없으면 인가 규칙이 DNS 질의를 막아 호스트네임 해석이 죽는다.
resource "aws_ec2_client_vpn_authorization_rule" "dns" {
  count = var.vpn_enabled ? 1 : 0

  client_vpn_endpoint_id = aws_ec2_client_vpn_endpoint.seed.id
  target_network_cidr    = "${cidrhost(data.aws_vpc.target.cidr_block, 2)}/32"
  authorize_all_groups   = true
  description            = "VPC DNS resolver"
}
