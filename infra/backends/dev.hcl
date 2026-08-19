# `terraform init -backend-config=../../backends/dev.hcl` 로 주입한다.
#
# 버킷은 `00-bootstrap` 이 만든다: `dbmon-tfstate-<account_id>`.
# 계정 ID 확인: `aws sts get-caller-identity --query Account --output text`
bucket = "dbmon-tfstate-123456789012"
region = "ap-northeast-2"
