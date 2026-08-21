#!/usr/bin/env bash
# Client VPN 설정 파일(.ovpn)을 만든다.
#
# 사용:
#   scripts/vpn.sh                  # ./dbmon-seed.ovpn 생성 (gitignore 됨)
#   scripts/vpn.sh /path/out.ovpn   # 경로 지정
#
# 생성 후:
#   - AWS VPN Client 또는 OpenVPN 호환 클라이언트에서 프로파일로 추가
#   - 연결되면 시드 DB 에 실제 호스트네임으로 직접 접속:
#     mysql -h dbmon-seed-prd-mysql.<...>.rds.amazonaws.com -udbmonadmin -p
#
# ⚠ 이 파일에는 클라이언트 개인키가 들어간다. 커밋 금지(*.ovpn 은 gitignore 됨).
set -euo pipefail

layer="$(cd "$(dirname "$0")/../infra/layers/60-seed" && pwd)"
out="${1:-dbmon-seed.ovpn}"

endpoint_id="$(terraform -chdir="$layer" output -raw vpn_endpoint_id)"

aws ec2 export-client-vpn-client-configuration \
  --client-vpn-endpoint-id "$endpoint_id" \
  --output text > "$out"

{
  echo "<cert>"
  terraform -chdir="$layer" output -raw vpn_client_cert
  echo "</cert>"
  echo "<key>"
  terraform -chdir="$layer" output -raw vpn_client_key
  echo "</key>"
} >> "$out"

chmod 600 "$out"
echo "생성: $out (개인키 포함 — 공유 금지)"
echo "AWS VPN Client (brew install --cask aws-vpn-client) 또는 OpenVPN 에 프로파일로 추가한다."
