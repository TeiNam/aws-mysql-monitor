#!/usr/bin/env bash
# IAM DB Auth 토큰 교차 검증 — 우리 구현 vs `aws rds generate-db-auth-token`.
#
# **토큰 문자열 전체**를 비교한다. 서명 16진수만 비교하면 퍼센트 인코딩 차이가
# 보이지 않는다 — 실제로 그렇게 놓쳤고, ECS 태스크 롤(임시 자격증명)의 base64
# 세션 토큰에 `+`·`/`·`=` 가 있어 프로덕션 경로가 전부 깨져 있었다.
#
# 그래서 **세션 토큰이 있는 케이스를 반드시 포함**한다. 장기 자격증명만 비교하면
# 같은 결함이 다시 통과한다.
#
# AWS CLI 의 토큰 생성은 네트워크를 타지 않으므로 자격증명이 유효하지 않아도 된다.
# 시각이 서명에 들어가므로 같은 초 안에 생성된 쌍만 비교한다.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST=orders-01.abc.ap-northeast-2.rds.amazonaws.com
export AWS_REGION=ap-northeast-2

command -v aws >/dev/null || { echo "건너뜀: aws CLI 가 없다"; exit 0; }
cargo build --quiet --package dbmon --example tokgen

date_of() { grep -o 'X-Amz-Date=[0-9TZ]*' <<<"$1"; }

# 토큰을 "호스트 + 정렬된 파라미터" 로 정규화한다.
#
# **파라미터 순서는 계약이 아니다.** 서명은 정렬된 canonical query 위에서 계산되고
# 서버는 이름으로 파싱한다. AWS CLI 는 삽입 순서를 그대로 내보내므로(정렬하지 않는다)
# 순서까지 맞추려 들면 CLI 의 구현 세부를 코드에 새기게 된다. 대신 **값과 서명이
# 전부 같은지**를 본다 — 그게 실제로 중요한 것이다.
normalize() {
  local host="${1%%/\?*}"
  echo "$host"
  tr '&' '\n' <<<"${1#*\?}" | LC_ALL=C sort
}

# 두 가지 자격증명 형태를 모두 확인한다.
run_case() {
  local name="$1"
  for _ in $(seq 1 12); do
    MINE=$(./target/debug/examples/tokgen)
    CLI=$(aws rds generate-db-auth-token --hostname "$HOST" --port 3306 \
          --username dbmon --region "$AWS_REGION")
    [ "$(date_of "$MINE")" = "$(date_of "$CLI")" ] || continue

    if [ "$(normalize "$MINE")" = "$(normalize "$CLI")" ]; then
      echo "OK  $name — 호스트·파라미터·서명 전부 일치 ($(date_of "$MINE"))"
      return 0
    fi
    echo "FAIL $name — 토큰이 AWS CLI 와 다르다"
    diff <(normalize "$MINE") <(normalize "$CLI") | head -12 || true
    return 1
  done
  echo "건너뜀: $name — 같은 초 안에 쌍을 얻지 못했다"
  return 0
}

# ① 장기 자격증명 (AWS 공개 문서 예시 키)
export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
unset AWS_SESSION_TOKEN
run_case "장기 자격증명"

# ② 임시 자격증명 — **base64 세션 토큰에 +, /, = 를 넣는다.**
export AWS_ACCESS_KEY_ID=ASIAIOSFODNN7EXAMPLE
export AWS_SESSION_TOKEN='FwoGZXIvYXdzEBYaDG+abc/def=ghi+jkl/mno='
run_case "임시 자격증명 (세션 토큰)"
