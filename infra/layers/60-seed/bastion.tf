# 베스천 — SSM 포트 포워딩 전용. SSH 키도, 퍼블릭 IP 도, 인바운드 규칙도 없다.
#
# 경로: 로컬 mysql 클라이언트 → aws ssm start-session(포트 포워딩)
#       → SSM 엔드포인트(VPC 내부) → 베스천 → RDS/Aurora:3306
#
# 프라이빗 서브넷의 기본 라우트가 blackhole 이어도 동작한다: SSM 인터페이스
# 엔드포인트 3종(ssm/ssmmessages/ec2messages)이 VPC 안에 있고, 그 SG
# (sg-0eb275500f0998b57)가 10.1.0.0/16:443 을 허용함을 실측했다(2026-08-21).
# 인터넷 경로가 아예 없으므로 패키지 설치는 불가하지만, 포트 포워딩은 TCP 릴레이라
# mysql 클라이언트조차 필요 없다. AL2023 은 SSM 에이전트를 내장한다.

data "aws_ssm_parameter" "al2023_arm64" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64"
}

resource "aws_iam_role" "bastion" {
  name_prefix = "${local.name}-bastion-"
  description = "dbmon seed bastion: SSM managed instance only"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

# Session Manager 등록·포트 포워딩에 필요한 전부다. 그 이상 주지 않는다.
resource "aws_iam_role_policy_attachment" "bastion_ssm" {
  role       = aws_iam_role.bastion.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "bastion" {
  name_prefix = "${local.name}-bastion-"
  role        = aws_iam_role.bastion.name
}

resource "aws_security_group" "bastion" {
  name_prefix = "${local.name}-bastion-"
  description = "dbmon seed bastion: no ingress; egress to SSM endpoints and DB only"
  vpc_id      = data.aws_vpc.target.id

  lifecycle {
    create_before_destroy = true
  }
}

# 인바운드 규칙 0개가 의도다 — SSM 세션은 에이전트의 아웃바운드 연결로 성립한다.

# SSM 인터페이스 엔드포인트(443)로 나간다. 엔드포인트 ENI 는 VPC CIDR 안에 있다.
resource "aws_vpc_security_group_egress_rule" "bastion_https" {
  security_group_id = aws_security_group.bastion.id
  description       = "HTTPS to in-VPC SSM interface endpoints"
  cidr_ipv4         = data.aws_vpc.target.cidr_block
  from_port         = 443
  to_port           = 443
  ip_protocol       = "tcp"
}

# 포워딩 대상 DB 로 나간다.
resource "aws_vpc_security_group_egress_rule" "bastion_mysql" {
  security_group_id            = aws_security_group.bastion.id
  description                  = "MySQL to seed fleet"
  referenced_security_group_id = aws_security_group.db.id
  from_port                    = 3306
  to_port                      = 3306
  ip_protocol                  = "tcp"
}

resource "aws_instance" "bastion" {
  ami           = data.aws_ssm_parameter.al2023_arm64.insecure_value
  instance_type = var.bastion_instance_type

  subnet_id                   = var.bastion_subnet_id
  vpc_security_group_ids      = [aws_security_group.bastion.id]
  associate_public_ip_address = false

  iam_instance_profile = aws_iam_instance_profile.bastion.name

  # IMDSv2 강제 — 크리덴셜 탈취 표면을 줄인다.
  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }

  root_block_device {
    volume_type = "gp3"
    volume_size = 8
    encrypted   = true
  }

  tags = {
    Name      = "${local.name}-bastion"
    DbmonRole = "bastion" # default_tags 의 seed-target 을 덮어쓴다 — DB 가 아니다.
  }

  lifecycle {
    # SSM 파라미터가 가리키는 최신 AMI 는 계속 바뀐다. 그때마다 베스천을 갈아치울
    # 이유가 없다 — 교체는 taint/replace 로 의도적으로 한다.
    ignore_changes = [ami]
  }
}
