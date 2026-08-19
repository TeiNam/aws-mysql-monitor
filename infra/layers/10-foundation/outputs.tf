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

output "app_env" {
  description = "로컬 개발에서 export 할 환경변수. `terraform output -raw app_env` 로 쓴다."
  value       = <<-EOT
    export DBMON__AWS__REGION=${var.region}
    export DBMON__AWS__ACCOUNT_ID=${data.aws_caller_identity.current.account_id}
    export DBMON__DEPLOYMENT_ENV=${var.environment}
    export DBMON__STORAGE__DATA_TABLE=${aws_dynamodb_table.data.name}
    export DBMON__STORAGE__CONFIG_TABLE=${aws_dynamodb_table.config.name}
    export DBMON__STORAGE__PLAN_BUCKET=${aws_s3_bucket.plans.id}
    export DBMON__DISCOVERY__ALLOWED_VPC_IDS=${var.vpc_id}
  EOT
}
