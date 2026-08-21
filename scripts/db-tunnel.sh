#!/usr/bin/env bash
# 시드 DB 로 SSM 포트 포워딩 터널을 연다. 베스천에 SSH 도 퍼블릭 IP 도 없다 — SSM 만 쓴다.
#
# 사용:
#   scripts/db-tunnel.sh --list                 # 타깃 목록과 기본 로컬 포트
#   scripts/db-tunnel.sh prd-aurora-writer      # 기본 포트로 터널 (prd 1431x, dev 1432x)
#   scripts/db-tunnel.sh dev-mysql 23306        # 로컬 포트 지정
#
# 요구:
#   - aws cli + session-manager-plugin (brew install --cask session-manager-plugin)
#   - AWS_PROFILE 환경변수 (예: export AWS_PROFILE=teinam-primary-123456789012)
#   - infra/layers/60-seed 가 apply 된 상태 (terraform output 을 읽는다)
#
# 마스터 암호는 Secrets Manager 에 있다 (aurora/aurora84 는 RDS 관리형, mysql 은 자체 시크릿):
#   terraform -chdir=infra/layers/60-seed output aurora    # 또는 aurora84 / mysql → master_secret_arn
#   aws secretsmanager get-secret-value --secret-id <arn> --query SecretString --output text
set -euo pipefail

layer="$(cd "$(dirname "$0")/../infra/layers/60-seed" && pwd)"

targets_json="$(terraform -chdir="$layer" output -json tunnel_targets)" || {
  echo "terraform output 을 읽지 못했다. 60-seed 가 apply 됐는지 확인한다." >&2
  exit 1
}
export TARGETS_JSON="$targets_json"

if [[ "${1:-}" == "--list" || -z "${1:-}" ]]; then
  echo "타깃                      호스트 → 기본 로컬 포트"
  python3 - <<'PY'
import json, os
d = json.loads(os.environ["TARGETS_JSON"])
for k in sorted(d):
    print(f"  {k:24s} {d[k]['host']} -> {d[k]['local_port']}")
PY
  exit 0
fi

export TARGET="$1"
read -r host default_port < <(python3 - <<'PY'
import json, os, sys
d = json.loads(os.environ["TARGETS_JSON"])
t = d.get(os.environ["TARGET"])
if not t:
    sys.exit("알 수 없는 타깃: %s (--list 로 확인)" % os.environ["TARGET"])
print(t["host"], t["local_port"])
PY
)

local_port="${2:-$default_port}"
bastion="$(terraform -chdir="$layer" output -raw bastion_instance_id)"

echo "▶ ${TARGET}: 127.0.0.1:${local_port} → ${host}:3306 (베스천 ${bastion})"
echo "  접속: mysql -h127.0.0.1 -P${local_port} -udbmonadmin -p"
exec aws ssm start-session \
  --target "$bastion" \
  --document-name AWS-StartPortForwardingSessionToRemoteHost \
  --parameters "host=${host},portNumber=3306,localPortNumber=${local_port}"
