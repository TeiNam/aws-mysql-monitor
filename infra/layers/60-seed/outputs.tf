output "endpoint" {
  value = aws_db_instance.seed.address
}

output "port" {
  value = aws_db_instance.seed.port
}

output "identifier" {
  value = aws_db_instance.seed.identifier
}

output "dbi_resource_id" {
  description = "IAM DB Auth 리소스에 넣을 값. 40-compute 의 db_auth_resource_ids."
  value       = aws_db_instance.seed.resource_id
}

output "master_secret_arn" {
  description = "RDS 관리형 마스터 시크릿. 부트스트랩(07)이 이걸 읽는다."
  value       = try(aws_db_instance.seed.master_user_secret[0].secret_arn, null)
}

output "instance_id" {
  description = "우리 키 포맷의 instance_id (M0-2a)."
  value       = "${data.aws_caller_identity.current.account_id}/${var.region}/${aws_db_instance.seed.identifier}"
}

output "reboot_reminder" {
  description = "정적 파라미터는 재부팅해야 적용된다."
  value       = <<-EOT
    ⚠ 파라미터 그룹의 정적 항목(max_digest_length, performance_schema*)은
      재부팅 후에 적용된다. apply 직후 확인하면 기본값이 보인다.

      aws rds reboot-db-instance --db-instance-identifier ${aws_db_instance.seed.identifier}

      확인:
      SELECT @@max_digest_length, @@performance_schema_max_digest_length,
             @@performance_schema_max_sql_text_length, @@performance_schema_digests_size;
  EOT
}
