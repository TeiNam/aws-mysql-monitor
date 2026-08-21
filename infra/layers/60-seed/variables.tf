variable "region" {
  type    = string
  default = "ap-northeast-2"
}

variable "environment" {
  description = <<-EOT
    이 레이어 자체의 배포 환경(default_tags 용). 시드 DB 의 시뮬레이션 환경은
    `seed_environments` 가 정하고, DB 리소스의 `env`/`Environment` 태그가
    이 값을 **덮어쓴다**.
  EOT
  type        = string
  default     = "dev"

  validation {
    condition     = contains(["dev", "stg", "prd"], var.environment)
    error_message = "environment 는 dev, stg, prd 중 하나여야 한다."
  }
}

variable "seed_environments" {
  description = <<-EOT
    시뮬레이션할 환경 태그 목록. 환경마다 다음 4대가 생긴다:
    Aurora MySQL 클러스터(writer+reader) + RDS MySQL(primary+read replica).

    탐색기가 태그 키 `env` → `Environment` 순으로 읽으므로([06 §태그]) 두 키 모두
    리소스에 박는다. 순서가 터널 로컬 포트 대역을 정한다: 14310 + 10×인덱스.
  EOT
  type        = list(string)
  default     = ["prd", "dev"]

  validation {
    condition     = length(var.seed_environments) > 0 && alltrue([for e in var.seed_environments : contains(["dev", "stg", "prd"], e)])
    error_message = "seed_environments 는 dev, stg, prd 의 비어 있지 않은 부분집합이어야 한다."
  }

  validation {
    condition     = length(distinct(var.seed_environments)) == length(var.seed_environments)
    error_message = "seed_environments 에 중복이 있다."
  }
}

variable "vpc_id" {
  type = string
}

variable "db_subnet_ids" {
  description = <<-EOT
    DB 서브넷 그룹에 넣을 서브넷 (서로 다른 AZ 2개 이상).
    dev: `dev-vpc-db-subnet-a/b` — 라우트가 local 뿐이라 인터넷에서 도달 불가.
    그게 의도다: 접근은 베스천 SSM 터널로만 한다.
  EOT
  type        = list(string)

  validation {
    condition     = length(var.db_subnet_ids) >= 2
    error_message = "RDS 서브넷 그룹에는 서로 다른 AZ 의 서브넷이 2개 이상 필요하다."
  }
}

variable "bastion_subnet_id" {
  description = <<-EOT
    베스천을 둘 프라이빗 서브넷. dev: `dev-vpc-priv-subnet-a`.
    기본 라우트가 blackhole 이어도 된다 — SSM 인터페이스 엔드포인트 3종
    (ssm/ssmmessages/ec2messages)이 VPC 안에 있고 그 SG 가 10.1.0.0/16:443 을
    허용한다(실측, [18 §1.2]). 퍼블릭 IP 도 NAT 도 쓰지 않는다.
  EOT
  type        = string
}

variable "mysql_engine_version" {
  description = <<-EOT
    **완전 버전으로 핀한다.** `"8.4"` 같은 두 자리는 RDS 가 prefix 매치로 해석해
    생성 시점의 기본 마이너를 고르는 **플로팅**이다.

    이 플릿의 존재 이유가 다이제스트·절단 동작 측정이고 그 동작은 마이너 버전에
    따라 갈릴 수 있다. [19](../../../docs/19-m1-findings.md) 의 실측이 8.4.11 이다.

    사용 가능한 버전: `aws rds describe-db-engine-versions --engine mysql \
      --query 'DBEngineVersions[].EngineVersion'`
  EOT
  type        = string
  default     = "8.4.11"

  validation {
    condition     = can(regex("^8\\.4\\.[0-9]+$", var.mysql_engine_version))
    error_message = "완전 버전(8.4.N)으로 지정한다. 두 자리는 플로팅이라 측정을 재현할 수 없다."
  }
}

variable "aurora_engine_version" {
  description = <<-EOT
    Aurora MySQL 3.x (MySQL 8.0 호환, ADR-019). 완전 버전으로 핀한다.
    3.12.0 은 db.t4g.medium 주문 가능을 실측으로 확인했다 (2026-08-21, 2a~2d).

    ⚠ 3.10.5 → 3.12.0 은 **인플레이스 업그레이드 경로가 없다**
    (`ValidUpgradeTarget` 빈 배열 실측). 이미 배포된 클러스터의 버전을 올리려면
    `terraform apply -replace` 로 재생성해야 한다 (시드 데이터는 loadgen 으로 재생성).
  EOT
  type        = string
  default     = "8.0.mysql_aurora.3.12.0"

  validation {
    condition     = can(regex("^8\\.0\\.mysql_aurora\\.3\\.[0-9]+\\.[0-9]+$", var.aurora_engine_version))
    error_message = "완전 버전(8.0.mysql_aurora.3.N.N)으로 지정한다."
  }
}

variable "aurora84_environments" {
  description = <<-EOT
    Aurora MySQL 8.4 (신 메이저) 클러스터를 둘 환경 목록. 기본은 prd 1곳.
    수집기의 Aurora 8.4 대응을 검증하는 대상이라 writer 1대만 만든다.
    빈 목록이면 만들지 않는다. seed_environments 의 부분집합이어야 한다.
  EOT
  type        = list(string)
  default     = ["prd"]

  validation {
    condition     = alltrue([for e in var.aurora84_environments : contains(var.seed_environments, e)])
    error_message = "aurora84_environments 는 seed_environments 의 부분집합이어야 한다."
  }
}

variable "aurora84_engine_version" {
  description = "Aurora MySQL 8.4 계열. 완전 버전으로 핀한다. 8.4.7 은 t4g.medium 주문 가능 실측 (2026-08-21)."
  type        = string
  default     = "8.4.mysql_aurora.8.4.7"

  validation {
    condition     = can(regex("^8\\.4\\.mysql_aurora\\.8\\.4\\.[0-9]+$", var.aurora84_engine_version))
    error_message = "완전 버전(8.4.mysql_aurora.8.4.N)으로 지정한다."
  }
}

variable "mysql_instance_class" {
  description = "RDS MySQL 인스턴스 클래스. IAM DB Auth 메모리 영향 관측(M1-14)을 위해 작게 유지."
  type        = string
  default     = "db.t4g.micro"
}

variable "aurora_instance_class" {
  description = "Aurora 인스턴스 클래스. 프로비저닝 Aurora 의 최소 버스터블이 t4g.medium 이다."
  type        = string
  default     = "db.t4g.medium"
}

variable "bastion_instance_type" {
  description = "베스천은 SSM 포트 포워딩 TCP 릴레이만 한다. nano 면 충분하다."
  type        = string
  default     = "t4g.nano"
}

variable "master_username" {
  description = "마스터 계정명. 비밀번호 관리: Aurora 는 RDS 관리형 시크릿, RDS MySQL 은 write-only 주입(아래)."
  type        = string
  default     = "dbmonadmin"
}

variable "mysql_master_password" {
  description = <<-EOT
    RDS MySQL 마스터 비밀번호 (write-only — state 에 저장되지 않는다).

    **관리형 시크릿(manage_master_user_password)을 쓰지 않는 이유**: RDS MySQL 은
    관리형 시크릿이 켜진 원본으로 리드 리플리카를 만들 수 없다
    (`InvalidParameterValue: ... ManageMasterUserPassword is enabled is not
    supported`, 2026-08-21 실측). Aurora 는 클러스터가 리플리카를 만들므로
    관리형 시크릿을 유지한다.

    값은 우리가 만든 Secrets Manager 시크릿(outputs 의 master_secret_arn)에도
    저장된다(역시 write-only 라 state 에 없다).

    **언제 필요한가**: 최초 생성(시크릿이 비어 있을 때)과 로테이션 때만.
    평상시 plan/apply 는 생략한다 — main.tf 가 시크릿의 현재 값을 ephemeral 로
    읽어 write-only 쌍을 채우고, 버전이 그대로면 아무것도 전송되지 않는다.

    로테이션: 새 값과 함께 `mysql_master_password_version` 을 1 올린다.
      export TF_VAR_mysql_master_password=$(openssl rand -hex 16)
      terraform apply ... -var mysql_master_password_version=2
    규칙: 8~41자, '/', '@', '"', 공백 금지 (hex 는 항상 안전).
  EOT
  type        = string
  default     = null
  nullable    = true
  # ephemeral 로 두지 않는다: 이 변수의 유무가 ephemeral 시크릿 읽기의 count 조건인데,
  # Terraform 은 ephemeral 값으로 count 를 정할 수 없다(실측 에러). sensitive 로 충분하다 —
  # 값은 write-only 인자로만 흘러 state 에 남지 않는다. 단 이 변수를 넣은 채
  # `plan -out=<file>` 로 플랜 파일을 저장하지는 말 것(플랜 파일에는 변수가 담긴다).
  sensitive = true

  validation {
    condition     = var.mysql_master_password == null || can(regex("^[^/@\" ]{8,41}$", var.mysql_master_password))
    error_message = "8~41자, '/', '@', '\"', 공백 금지."
  }
}

variable "mysql_master_password_version" {
  description = "mysql_master_password 를 바꿀 때 1씩 올린다. 이 값이 바뀔 때만 비밀번호가 전송된다."
  type        = number
  default     = 1
}

variable "task_security_group_id" {
  description = "40-compute 의 태스크 보안 그룹. 아직 없으면 빈 문자열."
  type        = string
  default     = ""
}

variable "backup_retention_days" {
  description = <<-EOT
    0 이면 PITR 이 꺼지고 **RDS 리드 리플리카 생성도 불가**하다(원본에 백업 필수).
    시드 데이터 실수 복구를 위해서도 최소 1을 유지한다.
  EOT
  type        = number
  default     = 1

  validation {
    condition     = var.backup_retention_days >= 1
    error_message = "리플리카 생성과 실수 복구를 위해 1 이상이어야 한다."
  }
}

variable "log_retention_days" {
  description = "RDS 로그 그룹 보존기간. 미지정 시 RDS 가 만드는 그룹은 Never Expire 다."
  type        = number
  default     = 14
}
