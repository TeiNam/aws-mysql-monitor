# `00-bootstrap` 은 **state 버킷 자체를 만든다** — 그 버킷에 자기 state 를 둘 수 없다.
# 그래서 여기만 로컬 state 이고, apply 후 아래 순서로 이관한다:
#
#     terraform init -backend-config=../../backends/dev.hcl -migrate-state
#
# 이관하지 않으면 부트스트랩 state 가 **한 사람의 노트북에만** 존재한다
# (`.gitignore` 가 `*.tfstate` 를 무시한다).
# terraform {
#   backend "s3" {
#     key          = "00-bootstrap/terraform.tfstate"
#     encrypt      = true
#     use_lockfile = true
#   }
# }
