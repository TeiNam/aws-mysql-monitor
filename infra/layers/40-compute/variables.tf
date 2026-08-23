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

# T-37 게이트의 Terraform 측 짝. 앱은 기동 시 거부하지만 `terraform apply` 는
# steady state 를 기다리지 않으므로 **성공으로 표시된다** — 실패를 알려면
# `aws ecs describe-services` 를 봐야 한다. plan 시점에 잡는 편이 낫다.
variable "allowed_vpc_ids" {
  description = <<-EOT
    앱의 탐색 필터 (T-37). **dev 에서는 비어 있으면 앱이 기동을 거부한다.**
    IAM 의 rds:DescribeDBInstances 는 리소스 수준 권한을 지원하지 않아
    prd 인스턴스도 보이므로, 이 필터가 마지막 방어선이다.
  EOT
  type        = list(string)
  default     = []

  validation {
    condition     = var.environment == "prd" || length(var.allowed_vpc_ids) > 0
    error_message = "prd 가 아니면 allowed_vpc_ids 가 필수다. 비우면 앱이 기동을 거부해 배포가 FAILED 로 끝난다 (T-37)."
  }
}

# **fail-open 이었다.** 비어 있으면 `iam.tf` 가 `dbuser:*/<user>` 로 폴백해, 아무 값도
# 주지 않은 apply 가 계정 내 **모든 RDS(prd 포함)** 에 대한 IAM DB 인증을 허용했다.
# 주석은 "dev 에서는 반드시 열거한다"고 했지만 코드가 그걸 강제하지 않았다.
variable "db_auth_resource_ids" {
  description = <<-EOT
    IAM DB 인증을 허용할 리소스 id 목록.

    ⚠ **엔진에 따라 다른 id 를 쓴다.** 이걸 틀리면 계정·플러그인·보안그룹이 모두 정상인데
    접속만 `1045 Access denied` 로 실패한다 — 실측으로 걸렸다.

    | 대상 | 넣을 값 | 얻는 방법 |
    |---|---|---|
    | RDS 단독 인스턴스 | `db-…` (`DbiResourceId`) | `aws rds describe-db-instances --query 'DBInstances[].DbiResourceId'` |
    | **Aurora** | `cluster-…` (`DbClusterResourceId`) | `aws rds describe-db-clusters --query 'DBClusters[].DbClusterResourceId'` |

    Aurora 는 인스턴스가 아니라 **클러스터 단위**로 `rds-db:connect` 를 판정한다. 인스턴스
    id 를 넣으면 그 ARN 은 어떤 요청과도 매치되지 않는다.

    비어 있으면 `dbuser:*/<user>` 로 전체를 허용한다 — dev 에서는 반드시 열거한다.
  EOT
  type        = list(string)
  default     = []

  validation {
    condition     = var.environment == "prd" || length(var.db_auth_resource_ids) > 0
    error_message = "prd 가 아니면 db_auth_resource_ids 를 명시해야 한다. 비우면 계정 내 모든 RDS 에 IAM DB 인증이 허용된다 (T-37)."
  }
}

variable "monitor_db_user" {
  description = "모니터링 DB 계정명. rds-db:connect 리소스에 들어간다."
  type        = string
  default     = "dbmon"
}

# **컨테이너에는 설정 파일이 없다.** 런타임 이미지는 바이너리와 `web/dist` 만 담으므로
# (`Dockerfile`) 앱은 `Config::load(None)` 으로 기동하고, 여기서 주지 않는 값은 코드
# 기본값이 된다. `local/dbmon-aws.toml` 의 `literal_policy` 는 로컬 실행에만 적용된다 —
# 그래서 dev 배포가 `masked` 로 돌며 `?` 만 저장하고 있었다.
variable "literal_policy" {
  description = <<-EOT
    SQL 리터럴 저장 정책 (OPEN-Q-15, [08 §6.1]).
    masked = `?` 치환 저장 (**되돌릴 수 없다**), full_restricted = 원문 저장 + operator 이상 노출.
    코드 기본값과 같은 `masked` 를 기본으로 둔다 — 잘못된 방향의 되돌리기 비용이 비대칭이다.
  EOT
  type        = string
  default     = "masked"

  validation {
    condition     = contains(["full", "full_restricted", "masked", "off"], var.literal_policy)
    error_message = "literal_policy 는 full, full_restricted, masked, off 중 하나여야 한다."
  }
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
  description = <<-EOT
    태스크 수. **기본값은 1이다.**

    수집은 리스로 **한 태스크만** 한다(ADR-018: "1단계는 단일 워커로 시작"). 두 번째
    태스크가 하는 일은 조회 API 서빙과 웜 스탠바이뿐이고, 한 워커가 인스턴스 500대를
    보도록 설계했으므로(01 의 NFR) dev 에서 API 용량이 부족할 이유가 없다.

    기본값이 2였는데 그건 위 ADR 과 어긋났다 — 실측으로 드러났다(ECS 태스크 2개 중
    하나만 리더가 되고 나머지는 기동 로그만 남기고 조용했다).

    | 값 | 대가 |
    |---|---|
    | 1 | Spot 회수·AZ 장애 시 ECS 가 교체하는 1~2분 동안 **수집 공백** |
    | 2 | 그 공백이 리스 인수 시간(≤60초)으로 줄고, 비용은 두 배 |

    prd 는 2 이상을 쓴다. 안 쓰는 기간에는 0 으로 두면 컴퓨트 비용이 0 이다.
  EOT
  type        = number
  default     = 1
}

variable "admin_ingress_security_group_ids" {
  description = <<-EOT
    컨테이너 포트로 **인바운드를 허용할 보안 그룹**(예: Client VPN 의 SG).

    `enable_alb = false` 인 dev 에서 화면을 보는 경로다 — 태스크는 VPC 사설 IP 를 가지므로
    VPN 으로 들어오면 `http://<사설 IP>:8080` 으로 바로 붙는다. ALB·ACM·공개 DNS 가 필요
    없다.

    ⚠ 이 목록이 비어 있지 않으면 **인증이 유일한 방어선**이다. ECS 에서는 dev 토큰 발급이
    꺼지므로(`api::auth`) `auth_token_secret_arn` 을 함께 넣어야 한다. 안 넣으면 모든
    요청이 401 이라 규칙만 켠 의미가 없다.
  EOT
  type        = list(string)
  default     = []
}

variable "admin_ingress_cidrs" {
  description = <<-EOT
    컨테이너 포트로 인바운드를 허용할 **CIDR** 목록.

    `admin_ingress_security_group_ids` 와 같은 목적인데 출처가 SG 가 아닌 경우다 — 사내
    VPN·TGW·피어링으로 들어오면 트래픽에 우리 VPC 의 SG 가 붙지 않고 클라이언트 CIDR 로
    도착한다(실측: dbmon Client VPN 이 아니라 다른 VPN 으로 들어오는 환경이 그랬다).

    ⚠ 인터넷 대역(`0.0.0.0/0`)을 넣지 않는다. 태스크는 퍼블릭 서브넷에 있고 퍼블릭 IP 를
    가지므로 그 순간 공개된다 — 그때는 ALB + WAF 를 쓰는 것이 맞다.
  EOT
  type        = list(string)
  default     = []

  validation {
    condition     = !contains(var.admin_ingress_cidrs, "0.0.0.0/0")
    error_message = "0.0.0.0/0 은 허용하지 않는다 — 퍼블릭 IP 태스크가 인터넷에 열린다."
  }
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

  # **이 검증이 없으면 조용히 통과하고 프로덕션 배포·Spot 중단 때만 데이터가 유실된다.**
  # 앱은 `stopTimeout` 값을 알 수 없으므로(환경변수로 주지 않는다) 여기서 막아야 한다.
  # ADR-022 는 "설정 검증으로 강제한다"고 적었지만 앱은 deregistration_wait < grace
  # 만 검증한다 — 그 짝이 여기 없었다.
  validation {
    condition     = var.stop_timeout_secs > var.shutdown_grace_secs
    error_message = "stop_timeout_secs 는 shutdown_grace_secs 보다 커야 한다. 작으면 SIGKILL 이 먼저 도착해 다이제스트 누산기와 쓰기 버퍼가 유실된다."
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

  validation {
    condition     = !var.enable_alb || length(var.alb_subnet_ids) >= 2
    error_message = "ALB 는 서로 다른 AZ 의 서브넷 2개 이상이 필요하다."
  }
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

  # 없으면 리스너에서 ValidationError 로 실패하는데, ALB·SG 는 이미 만들어진 뒤라
  # 부분 적용 상태로 남는다.
  validation {
    condition     = !var.enable_alb || var.acm_certificate_arn != ""
    error_message = "enable_alb=true 면 acm_certificate_arn 이 필요하다."
  }
}

variable "enable_ecs_exec" {
  description = "ECS Exec(컨테이너 셸 접속). dev 만 true."
  type        = bool
  default     = false
}

variable "container_insights_mode" {
  description = <<-EOT
    `disabled` | `enabled` | `enhanced`.

    이전 이름은 `enable_container_insights`(bool) 였는데, `true` 가 옛 `enabled` 가 아니라
    **Enhanced observability**(태스크당 과금이 훨씬 크다)로 갔다. "예전 그거" 로 오인해
    켤 위험이 있어 값을 그대로 받는다.
  EOT
  type        = string
  default     = "disabled"

  validation {
    condition     = contains(["disabled", "enabled", "enhanced"], var.container_insights_mode)
    error_message = "container_insights_mode 는 disabled, enabled, enhanced 중 하나여야 한다."
  }
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

variable "slowlog_log_group_arns" {
  description = <<-EOT
    슬로우로그를 읽을 CloudWatch 로그 그룹 ARN 목록.

    비우면 `arn:aws:logs:<region>:<account>:log-group:/aws/rds/*` 로 폴백한다 —
    계정 내 **모든** RDS 슬로우로그를 읽을 수 있고, 슬로우로그에는 SQL 리터럴이 들어간다.
    이 계정에는 프로덕션 워크로드가 함께 있으므로(T-37) prd 가 아니면 열거해야 한다.

    `rds:Describe*` 의 `Resource:"*"` 는 API 제약이라 불가피하지만 이건 아니다.
  EOT
  type        = list(string)
  default     = []

  validation {
    condition     = var.environment == "prd" || length(var.slowlog_log_group_arns) > 0
    error_message = "prd 가 아니면 slowlog_log_group_arns 를 명시해야 한다."
  }
}

# ── 크로스 계정 탐색 ─────────────────────────────────────────────────────────

variable "discovery_account_ids" {
  description = <<-EOT
    다른 계정의 RDS 도 탐색할 때 그 계정 번호들.

    비어 있으면 `sts:AssumeRole` 정책을 **만들지 않는다** — 멀티 계정을 쓰지 않는
    배포가 기본이고, 쓰지 않는 권한을 두지 않는다.

    화면 설정(`설정 → 탐색 범위`)의 계정 목록과 **같아야 한다.** 여기 없는 계정을
    화면에 넣으면 탐색이 AccessDenied 로 실패하고 목록에 뜨지 않는다.
  EOT
  type        = list(string)
  default     = []

  validation {
    condition     = alltrue([for id in var.discovery_account_ids : can(regex("^[0-9]{12}$", id))])
    error_message = "계정 번호는 숫자 12자리다."
  }
}

variable "discovery_role_name" {
  description = "대상 계정에서 맡을 역할 이름. 계정마다 같은 이름으로 만든다."
  type        = string
  default     = "dbmon-discovery"
}

# ── AI 튜닝 ─────────────────────────────────────────────────────────────────

variable "enable_ai_tuning" {
  description = "Bedrock 호출 권한을 붙이는가. 화면 설정(`설정 → AI 튜닝`)과 함께 켜야 동작한다."
  type        = bool
  default     = false
}

variable "bedrock_model_ids" {
  description = <<-EOT
    허용할 모델·추론 프로파일 ID.

    **열거한다.** `*` 로 열면 계정의 모든 모델(단가가 전혀 다른 이미지·비디오 모델
    포함)을 부를 수 있다. 화면에서 고른 모델이 여기 없으면 호출이 AccessDenied 다.

    기본값은 설정 화면의 모델 바로가기(`web/src/components/settings/AiSection.tsx` 의
    `KNOWN_MODELS`)와 **같은 목록**이다. 하나라도 빠지면 그 칩은 눌리는데 호출이 403 이고,
    그건 항상 실패하는 선택지다. 두 목록이 갈리는 것은
    `it_infra_consistency` 의 `bedrock_iam_covers_the_models_the_ui_offers` 가 막는다.
  EOT
  type        = list(string)
  default = [
    "global.anthropic.claude-sonnet-5",
    "global.anthropic.claude-opus-5",
    "global.anthropic.claude-haiku-4-5-20251001-v1:0",
  ]
}

# ── 알림 채널 ───────────────────────────────────────────────────────────────

variable "enable_notifications" {
  description = "알림 채널 비밀 읽기 권한을 붙이는가."
  type        = bool
  default     = false
}

variable "channel_secret_prefix" {
  description = <<-EOT
    알림 채널 비밀의 이름 접두어([10 §3.4] 의 `dbmon/channel/`).

    접두어로 좁히는 이유: `*` 로 열면 이 태스크가 **DB 마스터 암호를 포함해** 계정의
    모든 비밀을 읽을 수 있다.
  EOT
  type        = string
  default     = "dbmon/channel/"
}

# ─────────────────────────────────────────────────────────────────────────────
# API 인증 — 공유 토큰
# ─────────────────────────────────────────────────────────────────────────────

# **이 값 없이는 화면과 API 에 들어갈 수 없다.**
#
# 앱의 인증 수단 넷 중 셋(`local-dev`·`local-token`·`off`)은 ECS 에서 쓸 수 없거나
# 두 곳의 명시적 허용을 요구한다. 남는 것은 공유 토큰이고, 그 값은 배포 설정
# (`DBMON__HTTP__AUTH_TOKEN`)으로만 들어간다.
#
# 비워 두면 태스크가 뜨긴 하지만 **모든 API 요청이 401** 이고 화면이
# "이 배포에는 인증 수단이 없다" 를 보여준다. 기동 로그도 경고를 남긴다.
variable "auth_token_secret_arn" {
  description = <<-EOT
    공유 접속 토큰을 담은 Secrets Manager 비밀의 ARN.

    평문 토큰을 변수로 받지 않는다 — 그러면 값이 tfstate 와 plan 출력에 남는다.
    비밀을 먼저 만들고 ARN 만 넘긴다:

      aws secretsmanager create-secret --name dbmon/api-token \
        --secret-string "$(openssl rand -hex 32)"

    32자 미만이거나 공백이 섞이면 앱이 기동을 거부한다.
  EOT
  type        = string
  default     = ""

  validation {
    # 오타를 배포 시점에 잡는다. 빈 값은 "인증 없이 띄운다" 는 의도로 허용한다.
    condition     = var.auth_token_secret_arn == "" || can(regex("^arn:aws[a-z-]*:secretsmanager:", var.auth_token_secret_arn))
    error_message = "Secrets Manager 비밀의 ARN 이어야 한다 (평문 토큰이 아니다)."
  }
}


variable "enable_bootstrap" {
  description = <<-EOT
    모니터링 계정 부트스트랩(M3)을 켤까.

    **기본 `false`.** 켜면 태스크 롤이 **대상 DB 의 마스터 비밀번호를 읽을 수 있다** —
    이 배포에서 가장 큰 권한이다. 켜지 않으면 화면이 수동 스크립트를 보여주고
    사람이 실행한다(문서 07 §5 가 "가장 안전한 기본값" 이라고 지목한 경로다).

    켜기 전에 확인할 것:
    - 마스터 시크릿이 RDS 관리형(`rds!*`)이거나 `dbmon=true` 태그가 붙어 있다
    - `bootstrap.monitor_host` 설정에 앱 서브넷 CIDR 이 들어 있다 (`%` 가 아니다)
  EOT
  type        = bool
  default     = false
}
