variable "region" {
  type    = string
  default = "ap-northeast-2"
}

variable "environment" {
  description = "dev | stg | prd"
  type        = string

  validation {
    condition     = contains(["dev", "stg", "prd"], var.environment)
    error_message = "environment 는 dev, stg, prd 중 하나여야 한다."
  }
}

variable "state_bucket" {
  description = "00-bootstrap 이 만든 state 버킷. terraform_remote_state 가 읽는다."
  type        = string
}

variable "vpc_id" {
  type = string
}

variable "subnet_ids" {
  description = <<-EOT
    태스크를 둘 서브넷.
    dev: 퍼블릭 서브넷 + assign_public_ip=true → IGW 로 나간다 (NAT 비용 $0)
    prd: 프라이빗 서브넷 + NAT 또는 인터페이스 엔드포인트
  EOT
  type        = list(string)

  validation {
    condition     = length(var.subnet_ids) >= 2
    error_message = "AZ 이중화를 위해 서브넷이 2개 이상이어야 한다."
  }
}

variable "allowed_vpc_ids" {
  description = <<-EOT
    앱의 탐색 필터 (T-37). **dev 에서는 비어 있으면 앱이 기동을 거부한다.**
    IAM 의 rds:DescribeDBInstances 는 리소스 수준 권한을 지원하지 않아
    prd 인스턴스도 보이므로, 이 필터가 마지막 방어선이다.
  EOT
  type        = list(string)
  default     = []
}

variable "db_auth_resource_ids" {
  description = <<-EOT
    IAM DB 인증을 허용할 DbiResourceId 목록 (예: db-ABCDEFGH...).
    비어 있으면 `dbuser:*/<user>` 로 전체를 허용한다 — dev 에서는 반드시 열거한다.
  EOT
  type        = list(string)
  default     = []
}

variable "monitor_db_user" {
  description = "모니터링 DB 계정명. rds-db:connect 리소스에 들어간다."
  type        = string
  default     = "dbmon"
}

variable "role" {
  description = "all | api | collector | control"
  type        = string
  default     = "all"

  validation {
    condition     = contains(["all", "api", "collector", "control"], var.role)
    error_message = "role 은 all, api, collector, control 중 하나여야 한다."
  }
}

variable "image_tag" {
  description = <<-EOT
    ECR 이미지 태그. **`latest` 를 쓰지 않는다** — 어느 리비전이 돌고 있는지
    알 수 없고 롤백 대상을 특정할 수 없다. 커밋 SHA 를 쓴다.
  EOT
  type        = string

  validation {
    condition     = var.image_tag != "latest"
    error_message = "image_tag 로 latest 를 쓸 수 없다. 커밋 SHA 를 쓴다."
  }
}

variable "task_cpu" {
  description = "Fargate CPU 단위. 512=0.5vCPU, 1024=1vCPU"
  type        = number
  default     = 1024
}

variable "task_memory" {
  description = "MiB. Fargate 는 CPU 별 허용 조합이 정해져 있다."
  type        = number
  default     = 2048
}

variable "desired_count" {
  description = "active 1 + standby 1 이 기본 (ADR-018). dev 는 1로 줄여도 된다."
  type        = number
  default     = 2
}

variable "container_port" {
  type    = number
  default = 8080
}

variable "shutdown_grace_secs" {
  description = "앱의 정리 예산. stop_timeout_secs 보다 작아야 한다."
  type        = number
  default     = 45
}

variable "stop_timeout_secs" {
  description = <<-EOT
    ECS 가 SIGTERM 후 SIGKILL 까지 기다리는 시간.
    **앱의 shutdown_grace_secs 보다 커야 한다** — 작으면 누산기와 버퍼가 유실된다.
    Fargate 상한은 120초다.
  EOT
  type        = number
  default     = 60

  validation {
    condition     = var.stop_timeout_secs <= 120
    error_message = "Fargate 의 stopTimeout 상한은 120초다."
  }
}

variable "deregistration_wait_secs" {
  description = "종료 시 로드밸런서 등록 해제를 기다리는 시간. ALB 가 없으면 무시된다."
  type        = number
  default     = 20
}

variable "use_spot" {
  description = "FARGATE_SPOT 사용. dev 는 true(약 70% 저렴), prd 는 false."
  type        = bool
  default     = true
}

variable "assign_public_ip" {
  description = "퍼블릭 서브넷에서 IGW 로 나갈 때 true. NAT 를 쓰면 false."
  type        = bool
  default     = true
}

variable "enable_alb" {
  description = "ALB 를 만든다. dev 는 false (네트워크 비용 $0)."
  type        = bool
  default     = false
}

variable "alb_subnet_ids" {
  description = "ALB 를 둘 퍼블릭 서브넷. enable_alb=true 일 때만 쓴다."
  type        = list(string)
  default     = []
}

variable "alb_ingress_cidr" {
  description = <<-EOT
    ALB 인바운드를 허용할 CIDR. **0.0.0.0/0 을 기본값으로 두지 않는다** —
    관측 도구가 인터넷에 열려 있으면 인증 이전에 이미 공격 표면이다.
  EOT
  type        = string
  default     = "10.0.0.0/8"
}

variable "acm_certificate_arn" {
  description = "HTTPS 리스너용 인증서. enable_alb=true 면 필수."
  type        = string
  default     = ""
}

variable "enable_ecs_exec" {
  description = "ECS Exec(컨테이너 셸 접속). dev 만 true."
  type        = bool
  default     = false
}

variable "enable_container_insights" {
  description = "Container Insights. 태스크당 월 몇 달러이므로 dev 는 false."
  type        = bool
  default     = false
}

variable "log_retention_days" {
  type    = number
  default = 30
}

variable "log_level" {
  description = "DBMON_LOG 환경변수 (tracing EnvFilter 형식)."
  type        = string
  default     = "info"
}
