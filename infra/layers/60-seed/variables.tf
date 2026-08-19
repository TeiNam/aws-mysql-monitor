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
  description = <<-EOT
    기존 서브넷 그룹 이름. `publicly_accessible = false` 일 때만 쓴다.

    `dev-rds-subnet-group` 은 `dev-vpc-db-subnet-a/b` 이고 그 라우트 테이블에는
    **라우트가 하나도 없다**("없음 (DB 전용)", [18 §1](../../../docs/18-dev-environment.md)).
    퍼블릭으로 만들면 RDS 는 정상 생성되고 퍼블릭 DNS 도 나오지만 **응답 경로가 없어**
    개발자 IP 에서 3306 이 타임아웃한다. RDS 는 생성 시 라우트 테이블을 검증하지 않는다.
    그래서 퍼블릭일 때는 `public_subnet_ids` 위에 우리 서브넷 그룹을 만든다.
  EOT
  type        = string
  default     = ""
}

variable "public_subnet_ids" {
  description = <<-EOT
    IGW 라우트가 있는 서브넷 2개 이상 (`publicly_accessible = true` 일 때 필수).
    dev: `dev-vpc-pub-subnet-a/b` — 이쪽만 IGW ✓ 다.
  EOT
  type        = list(string)
  default     = []

  validation {
    condition     = !var.publicly_accessible || length(var.public_subnet_ids) >= 2
    error_message = "publicly_accessible=true 면 IGW 라우트가 있는 서브넷 2개 이상이 필요하다. db 전용 서브넷은 라우트가 없어 응답 경로가 생기지 않는다."
  }
}

variable "engine_version" {
  description = <<-EOT
    **완전 버전으로 핀한다.** `"8.4"` 같은 두 자리는 RDS 가 prefix 매치로 해석해
    생성 시점의 기본 마이너를 고르는 **플로팅**이다. 여기에 `auto_minor_version_upgrade`
    까지 켜면 유지보수 창에서 버전이 올라간다.

    이 인스턴스의 존재 이유가 다이제스트·절단 동작 측정이고 그 동작은 마이너 버전에
    따라 갈릴 수 있다. 언제 무엇으로 측정했는지 코드에 남지 않으면
    [19](../../../docs/19-m1-findings.md) 의 수치를 나중에 재현할 수 없다.

    사용 가능한 버전: `aws rds describe-db-engine-versions --engine mysql \
      --query 'DBEngineVersions[?starts_with(EngineVersion,`8.4`)].EngineVersion'`
  EOT
  type        = string
  default     = "8.4.6"

  validation {
    condition     = can(regex("^8\\.4\\.[0-9]+$", var.engine_version))
    error_message = "완전 버전(8.4.N)으로 지정한다. 두 자리는 플로팅이라 측정을 재현할 수 없다."
  }
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

  # **리터럴 denylist 는 fail-open 이다.** `0.0.0.0/0` 만 막으면 의미가 같은 값들이
  # 통과한다 (HCL 만으로 격리 검증한 결과):
  #
  #   ["0.0.0.0/0"]                      → BLOCKED
  #   ["0.0.0.0/1", "128.0.0.0/1"]       → ACCEPTED  (IPv4 전체)
  #   ["0.0.0.0/4"], ["0.0.0.0/8"]       → ACCEPTED
  #
  # `publicly_accessible` 기본값이 `true` 이므로 이 값들은 **인터넷에 열린 MySQL** 이다.
  # 접두 길이 하한이 fail-closed 형태다.
  validation {
    condition = alltrue([
      for c in var.allowed_client_cidrs :
      can(regex("^[0-9.]+/[0-9]+$", c)) && tonumber(split("/", c)[1]) >= 24
    ])
    error_message = "각 CIDR 의 접두 길이는 /24 이상이어야 한다. /0~/23 은 너무 넓다 — 개발자 IP 를 /32 로 넣는다."
  }

  # 빈 목록이면 인바운드 규칙이 0개다. "안전한 기본값" 은 맞지만 그 결과가
  # **아무것도 붙을 수 없는 DB 에 월 $25** 다. 태스크 SG 도 없으면 열거를 강제한다.
  validation {
    condition     = length(var.allowed_client_cidrs) > 0 || var.task_security_group_id != ""
    error_message = "allowed_client_cidrs 또는 task_security_group_id 중 하나는 있어야 한다. 둘 다 비면 인바운드 규칙이 없어 아무도 접속할 수 없다."
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

variable "log_retention_days" {
  description = "RDS 로그 그룹 보존기간. 미지정 시 RDS 가 만드는 그룹은 Never Expire 다."
  type        = number
  default     = 14
}
