# **부분 설정(partial configuration).** `bucket` 과 `region` 은 init 시점에 주입한다:
#
#     terraform init -backend-config=../../backends/dev.hcl
#
# 이렇게 두는 이유:
#
# - `terraform init -backend=false` 로 백엔드 없이 `validate` 가 돌아간다(레이어 독립성 검증).
# - 계정/환경마다 버킷이 달라도 코드를 고치지 않는다.
# - 주석 처리해 두면 아무도 해제하지 않아서 40-compute 의 `terraform_remote_state` 가
#   존재하지 않는 객체를 읽으려다 **plan 단계에서** 죽는다. 그게 실제로 일어난 상태였다.
#
# ⚠ `terraform workspace` 를 쓰지 않는다. 워크스페이스는 state 키를
#   `env:/<name>/<key>` 로 바꿔 버려서 다른 레이어의 `terraform_remote_state.key` 와
#   어긋난다. 환경 분리는 **버킷/계정**으로 한다.
terraform {
  backend "s3" {
    key = "40-compute/terraform.tfstate"

    encrypt = true

    # S3 네이티브 락. 예전 `dynamodb_table` 인자를 대체한다 (Terraform 1.10+).
    use_lockfile = true
  }
}
