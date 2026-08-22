output "data_table" {
  value = aws_dynamodb_table.data.name
}

output "config_table" {
  value = aws_dynamodb_table.config.name
}

output "data_table_arn" {
  value = aws_dynamodb_table.data.arn
}

output "config_table_arn" {
  value = aws_dynamodb_table.config.arn
}

output "kms_key_arn" {
  value = aws_kms_key.data.arn
}

output "plan_bucket" {
  value = aws_s3_bucket.plans.id
}

# ⚠ **앱에 없는 설정을 내보내지 않는다.**
#
# `StorageConfig` 는 `deny_unknown_fields` 다 — 여기서 내보낸 환경변수 하나가 앱에
# 없으면 기동이 **실패한다.** `DBMON__STORAGE__PLAN_BUCKET` 이 정확히 그 상태였다
# (앱에서 `plan_bucket` 을 지웠는데 이 출력에 남아 있었다, 교차 리뷰 2회차).
# 여기에 줄을 추가하려면 `crates/dbmon/src/config.rs` 에 그 필드가 있어야 한다.
output "app_env" {
  description = "로컬 개발에서 export 할 환경변수. `terraform output -raw app_env` 로 쓴다."
  value       = <<-EOT
    export DBMON__AWS__REGION=${var.region}
    export DBMON__AWS__ACCOUNT_ID=${data.aws_caller_identity.current.account_id}
    export DBMON__DEPLOYMENT_ENV=${var.environment}
    export DBMON__STORAGE__DATA_TABLE=${aws_dynamodb_table.data.name}
    export DBMON__STORAGE__CONFIG_TABLE=${aws_dynamodb_table.config.name}
    export DBMON__DISCOVERY__ALLOWED_VPC_IDS=${var.vpc_id}
  EOT
}

output "s3_gateway_endpoint_id" {
  description = "새로 만든 것이든 기존 것이든 하나로 노출한다."
  value = var.create_s3_gateway_endpoint ? (
    aws_vpc_endpoint.s3[0].id
  ) : data.aws_vpc_endpoint.s3_existing[0].id
}

output "dynamodb_gateway_endpoint_id" {
  value = aws_vpc_endpoint.dynamodb.id
}
