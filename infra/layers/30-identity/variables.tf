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

# ─────────────────────────────────────────────────────────────────────────────
# Cognito (M5)
# ─────────────────────────────────────────────────────────────────────────────

variable "enable_cognito" {
  description = <<-EOT
    Cognito 리소스를 만들까.

    **기본 `false`.** 이 레이어는 공유 토큰 배포에서 필요하지 않고, 사용자 풀은
    한번 만들면 지우기 어렵다(`deletion_protection`, 그리고 지우면 모든 `sub` 가
    사라져 `USER#<sub>` 레코드가 전부 고아가 된다).

    켤 준비가 됐다는 것은: 로그인시킬 사람이 둘 이상이고, 그들에게 서로 다른
    권한을 줘야 한다는 뜻이다. 한 명이면 공유 토큰이 맞다.
  EOT
  type        = bool
  default     = false
}

variable "callback_urls" {
  description = <<-EOT
    Hosted UI 로그인 후 돌아올 주소.

    ⚠ **`localhost` 를 prd 에 넣지 않는다.** 콜백 URL 은 인가 코드를 받는 자리이고,
    목록에 있는 주소면 어디로든 코드가 전달된다.
  EOT
  type        = list(string)
  default     = []

  validation {
    condition = alltrue([
      for u in var.callback_urls :
      startswith(u, "https://") || startswith(u, "http://localhost:")
    ])
    error_message = "콜백 URL 은 https:// 이거나 http://localhost:<port> 여야 한다."
  }
}

variable "logout_urls" {
  description = "로그아웃 후 돌아올 주소."
  type        = list(string)
  default     = []

  validation {
    condition = alltrue([
      for u in var.logout_urls :
      startswith(u, "https://") || startswith(u, "http://localhost:")
    ])
    error_message = "로그아웃 URL 은 https:// 이거나 http://localhost:<port> 여야 한다."
  }
}

variable "hosted_ui_prefix" {
  description = <<-EOT
    Hosted UI 도메인 접두어. 비어 있으면 `dbmon-<env>-<account>` 를 쓴다.

    **전역 고유**해야 한다 — 다른 AWS 고객이 쓰고 있으면 apply 가 실패한다.
    그래서 기본값에 계정 번호를 붙인다.
  EOT
  type        = string
  default     = ""

  validation {
    condition     = var.hosted_ui_prefix == "" || can(regex("^[a-z0-9-]{1,63}$", var.hosted_ui_prefix))
    error_message = "도메인 접두어는 소문자·숫자·하이픈 63자 이내여야 한다."
  }
}

variable "mfa_required" {
  description = <<-EOT
    MFA(TOTP)를 필수로 할까.

    **기본 `false`(OPTIONAL).** `true` 로 만들면 첫 관리자가 TOTP 를 등록하기 전에
    잠길 수 있다 — 임시 비밀번호로 로그인하는 순간 MFA 를 요구받는다.
    운영 시작 후 올린다.
  EOT
  type        = bool
  default     = false
}

variable "refresh_token_hours" {
  description = "리프레시 토큰 유효 시간. 문서 기본값은 8시간이다 (08 §2.2)."
  type        = number
  default     = 8

  validation {
    condition     = var.refresh_token_hours >= 1 && var.refresh_token_hours <= 24 * 30
    error_message = "1시간 ~ 30일 사이여야 한다."
  }
}
