# Cognito User Pool · App Client · Groups (M5, .claude/docs/08-security-auth.md §2)
#
# # 무엇을 만들고 무엇을 안 만드는가
#
# | 만든다 | 안 만든다 | 이유 |
# |---|---|---|
# | User Pool + 그룹 3개 | 사용자 계정 | 초대는 사람이 한다 (자가 가입 비활성) |
# | App Client (퍼블릭, PKCE) | 클라이언트 시크릿 | SPA 는 시크릿을 숨길 수 없다 |
# | Hosted UI 도메인 | IdP 페더레이션 | SAML/OIDC 는 조직마다 다르고, 화면에서 등록한다 |
# | | Pre Token Generation Lambda | 서버 `USER` 레코드만으로 동작한다 (아래 참조) |
#
# ## Pre Token Generation Lambda 를 만들지 않는 이유
#
# 문서(08 §2.3)는 IdP 그룹을 `cognito:groups` 로 주입하는 Lambda 를 규정했다.
#
# **네이티브 Cognito 사용자에게는 필요 없다** — Cognito 가 그룹 멤버십을
# `cognito:groups` 클레임에 자동으로 넣는다. 여기서 만드는 그룹 3개가 그대로 동작한다.
#
# ⚠ **IdP 페더레이션을 붙이면 필요해진다.** `AuthContext::intersect` 는 그룹 클레임이
# 비면 권한을 주지 않으므로(fail-closed, `an_empty_group_claim_grants_nothing` 테스트),
# 트리거 없이 IdP 로 들어온 사용자는 아무것도 못 본다. 그때 이 파일에 Lambda 를
# 추가하거나 IdP 사용자를 Cognito 그룹에 직접 넣는다.
#
# 한동안 반대로 동작했다: 빈 그룹을 "정보 없음" 으로 읽어 서버 역할을 그대로 썼고,
# 그러면 **Cognito 그룹에서 빼도 여전히 admin** 이었다(교차 리뷰 31라운드).

locals {
  pool_name = "dbmon-${var.environment}"

  # 역할 → Cognito 그룹. 이름의 `dbmon-` 접두어는 코드가 벗긴다
  # (`Role::highest_from_groups`) — 사용자 풀이 다른 앱과 공유될 수 있어서다.
  groups = {
    "dbmon-admin"    = { precedence = 10, description = "전체 권한" }
    "dbmon-operator" = { precedence = 20, description = "조회 + 알림·리포트·어드바이저·수집 설정" }
    "dbmon-viewer"   = { precedence = 30, description = "조회만" }
  }
}

resource "aws_cognito_user_pool" "main" {
  count = var.enable_cognito ? 1 : 0

  name = local.pool_name

  # **자가 가입 비활성** (FR-AUT-01) — 관리자 초대만.
  admin_create_user_config {
    allow_admin_create_user_only = true
    invite_message_template {
      email_subject = "dbmon 접속 초대"
      email_message = "dbmon 모니터링 콘솔 계정이 생성되었습니다.\n\n사용자 이름: {username}\n임시 비밀번호: {####}\n\n첫 로그인 시 비밀번호를 변경해야 합니다."
      sms_message   = "dbmon 사용자 {username}, 임시 비밀번호 {####}"
    }
  }

  username_attributes      = ["email"]
  auto_verified_attributes = ["email"]

  # 계정 열거 방지 — 존재하지 않는 사용자와 틀린 비밀번호를 같은 오류로 답한다.
  username_configuration {
    case_sensitive = false
  }

  password_policy {
    minimum_length                   = 12 # FR-AUT-02
    require_lowercase                = true
    require_uppercase                = true
    require_numbers                  = true
    require_symbols                  = true
    temporary_password_validity_days = 3
  }

  # **MFA 는 OPTIONAL 로 시작한다** (FR-AUT-05).
  #
  # `ON` 으로 만들면 첫 관리자가 TOTP 를 등록하기 전에 잠긴다 — 초대 메일의 임시
  # 비밀번호로 로그인하는 순간 MFA 를 요구받고, 등록 화면에 들어갈 방법이 없는
  # 구성이 있다. 운영 시작 후 `var.mfa_required = true` 로 올린다.
  mfa_configuration = var.mfa_required ? "ON" : "OPTIONAL"
  software_token_mfa_configuration {
    enabled = true
  }

  # **SMS 복구를 쓰지 않는다** — SIM 스와핑 위험 (08 §2.1).
  account_recovery_setting {
    recovery_mechanism {
      name     = "verified_email"
      priority = 1
    }
  }

  # 삭제 방지. 사용자 풀을 지우면 **모든 `sub` 가 사라지고** `USER#<sub>` 레코드가
  # 전부 고아가 된다 — 되돌릴 수 없다.
  deletion_protection = var.environment == "dev" ? "INACTIVE" : "ACTIVE"

  # 고급 보안(자격증명 유출 탐지·적응형 인증). **추가 비용이 있다** — dev 는 끈다.
  user_pool_add_ons {
    advanced_security_mode = var.environment == "dev" ? "OFF" : "ENFORCED"
  }

  # `custom:groups` — IdP 가 보내는 그룹 클레임을 받는 자리.
  #
  # **`mutable = false` 다** (08 §3). 사용자가 `UpdateUserAttributes` 로 자기 그룹
  # 소스를 바꿀 수 없어야 한다.
  schema {
    name                     = "groups"
    attribute_data_type      = "String"
    mutable                  = false
    required                 = false
    developer_only_attribute = false
    string_attribute_constraints {
      min_length = 0
      max_length = 2048
    }
  }

  lifecycle {
    # 스키마는 **추가만 가능하고 수정이 불가능**하다. Terraform 이 변경을 감지하면
    # 풀을 재생성하려 하고, 그건 모든 사용자를 지운다.
    ignore_changes = [schema]
  }
}

resource "aws_cognito_user_group" "roles" {
  for_each = var.enable_cognito ? local.groups : {}

  name         = each.key
  user_pool_id = aws_cognito_user_pool.main[0].id
  description  = each.value.description
  # 낮은 숫자가 우선이다. 코드는 `highest_from_groups` 로 가장 높은 역할을 뽑으므로
  # 이 값에 의존하지 않는다 — Cognito 콘솔 표시용이다.
  precedence = each.value.precedence
}

resource "aws_cognito_user_pool_client" "spa" {
  count = var.enable_cognito ? 1 : 0

  name         = "${local.pool_name}-spa"
  user_pool_id = aws_cognito_user_pool.main[0].id

  # **퍼블릭 클라이언트** — SPA 는 시크릿을 숨길 수 없다.
  generate_secret = false

  # Authorization Code + PKCE 만. **Implicit 을 켜지 않는다** — 액세스 토큰이
  # URL 프래그먼트로 오면 브라우저 히스토리·리퍼러에 남는다.
  allowed_oauth_flows                  = ["code"]
  allowed_oauth_flows_user_pool_client = true
  allowed_oauth_scopes                 = ["openid", "email", "profile"]

  supported_identity_providers = ["COGNITO"]

  callback_urls = var.callback_urls
  logout_urls   = var.logout_urls

  # **`ALLOW_USER_PASSWORD_AUTH` 를 켜지 않는다** (08 §2.2).
  #
  # 앱이 비밀번호를 직접 받으면 그 코드 경로가 피싱·로깅 위험을 만든다. 비밀번호는
  # Hosted UI 에서만 입력한다.
  explicit_auth_flows = ["ALLOW_REFRESH_TOKEN_AUTH", "ALLOW_USER_SRP_AUTH"]

  # **액세스 토큰을 15분으로 둔다** (문서 08 §2.2 는 60분을 적었다).
  #
  # # 왜 줄였나 (교차 리뷰 2차)
  #
  # 권한 변경이 기존 토큰에 즉시 반영되지 않는다. 두 수단이 있는데 둘 다 반쪽이다:
  #
  # | 수단 | 상태 |
  # |---|---|
  # | `claims_version` 대조 | Pre Token Generation 트리거가 없어 토큰이 버전을 주장하지 않는다 → 검사를 건너뛴다 |
  # | `revoked_after_ms` | 동작하지만 **사람이 DynamoDB 에 직접 세워야 한다** (앱은 그 키에 쓰기 Deny 다) |
  #
  # 그래서 그룹에서 빼거나 비활성화해도 **기존 액세스 토큰은 만료까지 산다.**
  # 60분이면 그 창이 한 시간이다. 15분으로 줄이면 4배 좁아지고, 프론트의 자동
  # 갱신(`useTokenRefresh`, 만료 5분 전)이 그 주기를 흡수한다.
  #
  # 더 줄일 수도 있지만(Cognito 최소 5분) 갱신 호출이 그만큼 늘고, 갱신 자체가
  # 실패 표면이다. 15분이 균형점이다.
  access_token_validity  = 15 # 분
  id_token_validity      = 15
  refresh_token_validity = var.refresh_token_hours
  token_validity_units {
    access_token  = "minutes"
    id_token      = "minutes"
    refresh_token = "hours"
  }

  # **리프레시 토큰 회전 + 재사용 감지.**
  #
  # # `enable_token_revocation` 은 이게 아니다 (교차 리뷰 6차)
  #
  # 한때 주석이 "리프레시 토큰 재사용 감지. 훔친 토큰이 한 번 쓰이면 그 계보 전체가
  # 무효화된다" 고 적고 `enable_token_revocation = true` 만 켰다. **그 둘은 다른
  # 기능이다**: `enable_token_revocation` 은 `RevokeToken` API 로 **명시적으로** 폐기할
  # 수 있게 하는 것이고, 회전·재사용 감지가 아니다.
  #
  # 회전을 켜면 갱신마다 새 리프레시 토큰이 나오고 이전 것이 무효가 된다. 탈취된
  # 토큰을 정상 사용자가 한 번 갱신하면 공격자의 토큰이 죽는다 — 주석이 약속한
  # 동작이 그것이다.
  #
  # `retry_grace_period_seconds` 는 **네트워크 재시도를 위한 창**이다. 0 이면 응답을
  # 못 받고 재시도한 정상 요청이 "재사용" 으로 판정돼 세션이 죽는다. 프론트가
  # 만료 5분 전부터 갱신하므로 짧게 둘 수 있지만, 0 은 위험하다.
  refresh_token_rotation {
    feature                    = "ENABLED"
    retry_grace_period_seconds = 60
  }

  # `RevokeToken` API 를 쓸 수 있게 한다 (로그아웃 시 명시적 폐기).
  enable_token_revocation       = true
  prevent_user_existence_errors = "ENABLED"

  # 앱이 쓸 수 있는 속성. **`custom:groups` 는 넣지 않는다** — 위 스키마의
  # `mutable = false` 와 이중 방어다.
  read_attributes  = ["email", "email_verified", "name"]
  write_attributes = ["email", "name"]
}

# Hosted UI 도메인. `https://<prefix>.auth.<region>.amazoncognito.com`.
resource "aws_cognito_user_pool_domain" "main" {
  count = var.enable_cognito ? 1 : 0

  domain       = var.hosted_ui_prefix != "" ? var.hosted_ui_prefix : "dbmon-${var.environment}-${data.aws_caller_identity.current.account_id}"
  user_pool_id = aws_cognito_user_pool.main[0].id
}

data "aws_caller_identity" "current" {}
