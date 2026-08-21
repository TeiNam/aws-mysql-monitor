#!/usr/bin/env bash
# AWS 시드 DB 에 **샘플 스키마 + 모니터링 계정**을 넣는다. 멱등하다.
#
# 사용:
#   export AWS_PROFILE=teinam-primary-123456789012
#   scripts/seed-aws-db.sh --list
#   scripts/seed-aws-db.sh dev-mysql              # 스키마 + 데이터 + dbmon 계정
#   scripts/seed-aws-db.sh dev-aurora --user-only # 계정만 (스키마는 건드리지 않는다)
#
# 요구:
#   - Client VPN 연결 (또는 VPC 안에서 실행). 엔드포인트가 사설 IP 로 풀려야 한다.
#   - mysql 클라이언트, AWS_PROFILE
#   - infra/layers/60-seed 가 apply 된 상태 (endpoint·시크릿 ARN 을 output 에서 읽는다)
#
# # 왜 리플리카를 대상으로 두지 않는가
#
# `-ro` 인스턴스는 read-only 라 `CREATE USER` 도 `CREATE TABLE` 도 실행되지 않는다.
# 소스에 만들면 복제로 따라온다. Aurora 도 라이터 엔드포인트 하나만 대상이다.
#
# # 모니터링 계정은 비밀번호가 없다
#
# `IDENTIFIED WITH AWSAuthenticationPlugin` — IAM DB Auth 토큰으로만 붙는다(ADR-007).
# 붙는 주체마다 **호스트 패턴이 다른 별개 계정**이 필요하다: ECS 태스크는 프라이빗
# 서브넷, 개발 노트북은 VPN 클라이언트 CIDR 에서 온다.
set -euo pipefail

# **리전을 못 박는다.** 쉘 프로파일이 `AWS_REGION` 을 내보내고 있으면(us-west-2 등)
# 그 값이 프로파일 설정을 **덮는다** — 증상은 "인스턴스 0대"·"시크릿을 찾을 수 없다" 로
# 나타나고 원인이 권한처럼 보인다. 실측으로 두 번 걸렸다.
REGION="${DBMON_REGION:-ap-northeast-2}"
export AWS_REGION="$REGION"

layer="$(cd "$(dirname "$0")/../infra/layers/60-seed" && pwd)"
seed_dir="$(cd "$(dirname "$0")/../local/seed" && pwd)"

# 관측 스키마. 권한 모드 B(최소 권한)의 화이트리스트가 이것 하나다.
SCHEMA=shop
# 접속 주체의 **DB 가 보는 주소**로 호스트 패턴을 정한다.
#
# ⚠ **Client VPN 클라이언트 CIDR 이 아니다.** Client VPN 은 VPC 리소스로 갈 때 소스
# NAT 을 해서, DB 는 association 서브넷의 ENI 주소로 본다 — 실측: 클라이언트가
# 10.99.0.130 인데 MySQL 은 `10.1.24.139` 로 봤다(priv-subnet-a, 10.1.16.0/20).
# 그래서 개발 노트북과 ECS 태스크가 **같은 패턴 하나**로 덮인다.
HOST_TASKS='10.1.%'

targets_json="$(terraform -chdir="$layer" output -json 2>/dev/null)" || {
  echo "terraform output 을 읽지 못했다. 60-seed 가 apply 됐는지 확인한다." >&2
  exit 1
}
export OUT_JSON="$targets_json"

# 타깃 키 → (writer 엔드포인트, 마스터 시크릿 ARN)
read_target() {
  TARGET="$1" python3 - <<'PY'
import json, os, sys
o = json.loads(os.environ["OUT_JSON"])
t = os.environ["TARGET"]
val = lambda k: o[k]["value"]
table = {}
for env in ("dev", "prd"):
    a = val("aurora").get(env)
    if a:
        table[f"{env}-aurora"] = (a["writer_endpoint"], a["master_secret_arn"])
    m = val("mysql").get(env)
    if m:
        table[f"{env}-mysql"] = (m["primary_address"], m["master_secret_arn"])
a84 = val("aurora84").get("prd")
if a84:
    table["prd-aurora84"] = (a84["writer_endpoint"], a84["master_secret_arn"])
if t == "--list":
    for k in sorted(table):
        print(f"  {k:14s} {table[k][0]}")
    sys.exit(0)
if t not in table:
    print(f"모르는 타깃: {t}. --list 로 확인한다.", file=sys.stderr)
    sys.exit(2)
print(f"{table[t][0]}\t{table[t][1]}")
PY
}

if [[ "${1:-}" == "--list" || -z "${1:-}" ]]; then
  echo "타깃            writer 엔드포인트"
  read_target --list
  exit 0
fi

target="$1"
user_only="${2:-}"
read -r endpoint secret_arn < <(read_target "$target")

secret="$(aws secretsmanager get-secret-value --region "$REGION" --secret-id "$secret_arn" \
  --query SecretString --output text)"
export SECRET_JSON="$secret"
master_user="$(python3 -c 'import json,os;print(json.loads(os.environ["SECRET_JSON"])["username"])')"
# **비밀번호는 환경변수로만 넘긴다.** 명령줄에 두면 `ps` 에 보인다.
MYSQL_PWD="$(python3 -c 'import json,os;print(json.loads(os.environ["SECRET_JSON"])["password"])')"
export MYSQL_PWD
unset SECRET_JSON

run_sql() {
  mysql -h "$endpoint" -u "$master_user" --ssl-mode=REQUIRED --connect-timeout=15 "$@"
}

echo "대상: $target ($endpoint), 마스터: $master_user"

if [[ "$user_only" != "--user-only" ]]; then
  echo "→ 스키마 생성"
  run_sql -e "CREATE DATABASE IF NOT EXISTS \`$SCHEMA\` DEFAULT CHARSET utf8mb4;"
  run_sql "$SCHEMA" < "$seed_dir/01-schema.sql"
  echo "→ 데이터 적재 (서버측 재귀 CTE — 20k/60k/120k 행)"
  run_sql "$SCHEMA" < "$seed_dir/02-data.sql"
fi

echo "→ 모니터링 계정 (IAM DB Auth, 비밀번호 없음)"
for host in "$HOST_TASKS"; do
  # `IF NOT EXISTS` 는 **기존 계정의 잘못된 상태를 고치지 않는다**(docs/03 §멱등성).
  # 그래서 ALTER 로 플러그인·SSL 요구를 매번 강제한다 — 예전에 비밀번호 계정으로
  # 만들어 뒀으면 그 상태가 조용히 남아 IAM 인증만 계속 실패한다.
  run_sql -e "
    CREATE USER IF NOT EXISTS 'dbmon'@'$host'
      IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL;
    ALTER USER 'dbmon'@'$host'
      IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL;
    GRANT PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO 'dbmon'@'$host';
    GRANT SELECT ON \`performance_schema\`.* TO 'dbmon'@'$host';
    GRANT SELECT ON \`sys\`.*                TO 'dbmon'@'$host';
    GRANT SELECT ON \`$SCHEMA\`.*            TO 'dbmon'@'$host';
  "
  echo "   dbmon@$host"
done

echo "→ 확인"
run_sql -N -e "
SELECT CONCAT('테이블 ', COUNT(*), '개') FROM information_schema.tables WHERE table_schema='$SCHEMA';
SELECT CONCAT('행수 customers=', (SELECT COUNT(*) FROM \`$SCHEMA\`.customers),
              ' orders=',        (SELECT COUNT(*) FROM \`$SCHEMA\`.orders),
              ' order_items=',   (SELECT COUNT(*) FROM \`$SCHEMA\`.order_items));
SELECT CONCAT(user,'@',host,' → ',plugin) FROM mysql.user WHERE user='dbmon';
"
