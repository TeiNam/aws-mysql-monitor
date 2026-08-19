#!/usr/bin/env bash
# IAM DB Auth 토큰 교차 검증 — 우리 구현 vs `aws rds generate-db-auth-token`.
#
# 단위 테스트는 "문서대로 생겼는가" 만 답한다. 서명이 **AWS 와 같은 값인가** 는
# 독립 구현과 대조해야 알 수 있다. AWS CLI 의 토큰 생성은 네트워크를 타지 않으므로
# 자격증명이 유효하지 않아도 된다(서명만 계산한다).
#
# 시각이 서명에 들어가므로 같은 초 안에 생성된 쌍만 비교한다.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST=orders-01.abc.ap-northeast-2.rds.amazonaws.com
export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

command -v aws >/dev/null || { echo "건너뜀: aws CLI 가 없다"; exit 0; }
cargo build --quiet --package dbmon --example tokgen

sig() { grep -o 'X-Amz-Signature=[0-9a-f]*' <<<"$1"; }
date_of() { grep -o 'X-Amz-Date=[0-9TZ]*' <<<"$1"; }

for _ in $(seq 1 10); do
  MINE=$(./target/debug/examples/tokgen)
  CLI=$(aws rds generate-db-auth-token --hostname "$HOST" --port 3306 \
        --username dbmon --region ap-northeast-2)
  [ "$(date_of "$MINE")" = "$(date_of "$CLI")" ] || continue

  if [ "$(sig "$MINE")" = "$(sig "$CLI")" ]; then
    echo "OK  서명 일치 ($(date_of "$MINE"))"
    exit 0
  fi
  echo "FAIL 서명 불일치"
  echo "  우리: $MINE"
  echo "  CLI : $CLI"
  exit 1
done
echo "건너뜀: 같은 초 안에 쌍을 얻지 못했다 (다시 실행한다)"
