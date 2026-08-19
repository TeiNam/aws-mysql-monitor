# `00-bootstrap` apply 후 이 블록을 활성화하고 `terraform init -migrate-state` 를 실행한다.
#
# 처음에는 주석 상태로 두어 **bootstrap 없이도 plan 이 돌게** 한다 — 레이어가 독립이라는
# 성질을 검증(`terraform validate`)할 때 백엔드가 없어도 되어야 한다.
#
# terraform {
#   backend "s3" {
#     bucket         = "dbmon-tfstate-123456789012"
#     key            = "10-foundation/terraform.tfstate"
#     region         = "ap-northeast-2"
#     dynamodb_table = "dbmon-tfstate-locks"
#     encrypt        = true
#   }
# }
