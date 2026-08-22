# 앱 설정에 넣을 값. **전부 공개값이다** — 사용자 풀 ID·클라이언트 ID·도메인은
# 로그인 화면이 브라우저에서 읽는 값이므로 비밀이 아니다(08 §1 위협 모델).

output "user_pool_id" {
  description = "설정 화면의 `auth.cognito.user_pool_id` 에 넣는다."
  value       = one(aws_cognito_user_pool.main[*].id)
}

output "client_id" {
  description = "설정 화면의 `auth.cognito.client_id` 에 넣는다."
  value       = one(aws_cognito_user_pool_client.spa[*].id)
}

output "hosted_ui_domain" {
  description = "로그인 화면 주소. 설정의 `auth.cognito.domain` 에 넣는다."
  value = one([
    for d in aws_cognito_user_pool_domain.main[*].domain :
    "https://${d}.auth.${var.region}.amazoncognito.com"
  ])
}

output "issuer" {
  description = "토큰 검증기가 대조하는 `iss`. 앱이 풀 ID 에서 유도하므로 참고용이다."
  value = one([
    for id in aws_cognito_user_pool.main[*].id :
    "https://cognito-idp.${var.region}.amazonaws.com/${id}"
  ])
}

output "groups" {
  description = "만들어진 그룹 이름. 사용자를 여기 넣어야 역할이 생긴다."
  value       = keys(aws_cognito_user_group.roles)
}

output "next_steps" {
  description = "apply 후 사람이 해야 하는 일."
  value       = <<-EOT
    ${var.enable_cognito ? "" : "⚠ enable_cognito = false — 아무것도 만들지 않았다.\n"}
    1) 관리자 초대:
       aws cognito-idp admin-create-user --user-pool-id ${one(aws_cognito_user_pool.main[*].id)} \
         --username <email> --user-attributes Name=email,Value=<email> Name=email_verified,Value=true
    2) 그룹 배정:
       aws cognito-idp admin-add-user-to-group --user-pool-id ${one(aws_cognito_user_pool.main[*].id)} \
         --username <email> --group-name dbmon-admin
    3) **서버 `USER` 레코드 생성** — 이게 없으면 로그인해도 권한이 없다 (fail-closed).
       docs/08-security-auth.md §3 의 형식을 따른다. `sub` 는 admin-get-user 로 확인한다.
    4) 설정 화면에 user_pool_id·client_id·domain 을 넣고 방식을 `cognito` 로 바꾼다.
  EOT
}
