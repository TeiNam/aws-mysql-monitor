output "state_bucket" {
  description = "다른 레이어의 backend.tf 에 넣을 버킷 이름."
  value       = aws_s3_bucket.state.id
}

output "lock_table" {
  value = aws_dynamodb_table.locks.name
}

output "backend_snippet" {
  description = "다른 레이어의 backend.tf 에 그대로 붙여넣을 블록."
  value       = <<-EOT
    terraform {
      backend "s3" {
        bucket         = "${aws_s3_bucket.state.id}"
        key            = "<레이어 이름>/terraform.tfstate"
        region         = "${var.region}"
        dynamodb_table = "${aws_dynamodb_table.locks.name}"
        encrypt        = true
      }
    }
  EOT
}
