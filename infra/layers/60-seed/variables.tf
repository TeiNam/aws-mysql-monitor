variable "region" {
  type    = string
  default = "ap-northeast-2"
}

variable "environment" {
  type = string
  validation {
    condition     = contains(["dev", "stg", "prd"], var.environment)
    error_message = "environment 는 dev, stg, prd 중 하나여야 한다."
  }
}

variable "vpc_id" {
  type = string
}

variable "db_subnet_group_name" {
  description = "기존 서브넷 그룹 이름. dev 계정에는 dev-rds-subnet-group 이 있다."
  type        = string
}

variable "engine_version" {
  description = "ADR-019 의 하한이 8.4 다. 5.7 은 EOL, 8.0 은 하한 미달."
  type        = string
  default     = "8.4"
}

variable "instance_class" {
  description = <<-EOT
    db.t4g.micro 로 시작한다. IAM DB Auth 가 수백 MiB 를 쓴다는 안내가 있어
    작은 인스턴스에서 FreeableMemory 를 관측하는 것이 M1-14 의 목적이기도 하다.
  EOT
  type        = string
  default     = "db.t4g.micro"
}

variable "master_username" {
  description = "마스터 계정명. 비밀번호는 RDS 관리형 시크릿이 관리한다."
  type        = string
  default     = "dbmonadmin"
}

variable "allowed_client_cidrs" {
  description = <<-EOT
    3306 인바운드를 허용할 CIDR 목록. **기본값을 두지 않는다** —
    빈 목록이면 아무도 붙을 수 없고, 그게 안전한 기본값이다.
    개발자 IP 를 넣는다: `curl -s https://checkip.amazonaws.com`
  EOT
  type        = list(string)
  default     = []

  validation {
    condition     = !contains(var.allowed_client_cidrs, "0.0.0.0/0")
    error_message = "0.0.0.0/0 을 허용할 수 없다. 개발자 IP 를 /32 로 넣는다."
  }
}

variable "task_security_group_id" {
  description = "40-compute 의 태스크 보안 그룹. 아직 없으면 빈 문자열."
  type        = string
  default     = ""
}

variable "publicly_accessible" {
  description = <<-EOT
    dev 는 true. dev-vpc-01 프라이빗 서브넷의 0.0.0.0/0 이 blackhole 이고 NAT 가 없어
    다른 접근 경로가 없다. 보안 그룹이 CIDR 를 좁힌다.
  EOT
  type        = bool
  default     = true
}

variable "backup_retention_days" {
  description = "0 이면 PITR 이 꺼진다. 실수 복구를 위해 최소 1을 유지한다."
  type        = number
  default     = 1

  validation {
    condition     = var.backup_retention_days >= 1
    error_message = "0 은 PITR 을 끈다. 시드 데이터 실수 복구를 위해 1 이상."
  }
}
