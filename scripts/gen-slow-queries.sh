#!/usr/bin/env bash
# 시드 DB 에 **실제로 느린 쿼리**를 흘려 넣는다. 수집기·화면을 실데이터로 확인하는 용도다.
#
#   ./scripts/gen-slow-queries.sh                 # 기본: 10분, 동시 3
#   DURATION_SEC=120 CONCURRENCY=5 ./scripts/gen-slow-queries.sh
#   TARGET=aurora ./scripts/gen-slow-queries.sh    # Aurora 라이터로
#
# # 왜 SLEEP() 을 쓰지 않는가
#
# `SELECT SLEEP(5)` 는 확실히 느리지만 **실행계획이 없다.** 이 프로젝트가 보여주려는
# 것은 계획·다이제스트·카디널리티이므로, 계획이 나오는 실제 조인을 쓴다. 소요 시간은
# 실측했다(주석의 `~Ns`).
#
# ponytail: mysql 클라이언트 + 백그라운드 서브셸. 부하 도구(sysbench 등)를 새로
# 들이지 않는다 — 이건 데이터를 흘려 넣는 것이지 벤치마크가 아니다.
set -euo pipefail

REGION="${DBMON_REGION:-ap-northeast-2}"
TARGET="${TARGET:-mysql}"
DURATION_SEC="${DURATION_SEC:-600}"
CONCURRENCY="${CONCURRENCY:-3}"
SCHEMA="${SCHEMA:-shop}"

case "$TARGET" in
  mysql)  INSTANCE="dbmon-seed-dev-mysql" ;;
  aurora) INSTANCE="dbmon-seed-dev-aurora-2" ;;
  *) echo "TARGET 은 mysql 또는 aurora 다 (받은 값: $TARGET)" >&2; exit 2 ;;
esac

host=$(aws rds describe-db-instances --region "$REGION" \
  --db-instance-identifier "$INSTANCE" \
  --query 'DBInstances[0].Endpoint.Address' --output text)

# 마스터 자격증명: RDS 관리형 시크릿이 있으면 그것, 없으면 Terraform 시크릿.
# **M3 의 자격증명 소스 두 개가 여기서도 갈린다** — 시드가 둘 다 쓴다.
#
# ⚠ **Aurora 는 클러스터에 붙어 있다.** `describe-db-instances` 의
# `MasterUserSecret` 은 Aurora 멤버에서 항상 `None` 이고, 그걸 못 보고 Terraform
# 시크릿으로 떨어지면 **다른 DB 의 자격증명**으로 접속을 시도한다(18849회 전부
# 실패로 실측했다). IAM DB 인증의 클러스터 리소스 id 문제와 같은 부류다.
cluster=$(aws rds describe-db-instances --region "$REGION" \
  --db-instance-identifier "$INSTANCE" \
  --query 'DBInstances[0].DBClusterIdentifier' --output text)
if [[ -n "$cluster" && "$cluster" != "None" ]]; then
  secret=$(aws rds describe-db-clusters --region "$REGION" \
    --db-cluster-identifier "$cluster" \
    --query 'DBClusters[0].MasterUserSecret.SecretArn' --output text)
else
  secret=$(aws rds describe-db-instances --region "$REGION" \
    --db-instance-identifier "$INSTANCE" \
    --query 'DBInstances[0].MasterUserSecret.SecretArn' --output text)
fi
if [[ -z "$secret" || "$secret" == "None" ]]; then
  secret=$(aws secretsmanager list-secrets --region "$REGION" \
    --query 'SecretList[?starts_with(Name, `dbmon-seed-mysql-master-`)].Name | [0]' --output text)
fi
[[ -n "$secret" && "$secret" != "None" ]] || { echo "마스터 시크릿을 찾을 수 없다: $INSTANCE" >&2; exit 1; }

# 비밀번호를 **파일에 쓰지 않는다.** 환경변수로만 넘긴다.
creds=$(aws secretsmanager get-secret-value --region "$REGION" --secret-id "$secret" \
  --query SecretString --output text)
user=$(printf '%s' "$creds" | python3 -c 'import sys,json;print(json.load(sys.stdin)["username"])')
MYSQL_PWD=$(printf '%s' "$creds" | python3 -c 'import sys,json;print(json.load(sys.stdin)["password"])')
export MYSQL_PWD
unset creds

# 쿼리 목록. 실측 소요 시간을 주석에 적어 둔다 — 임계값(기본 2초)을 넘어야 잡힌다.
QUERIES=(
  # ~7s — 자기조인 범위 스캔. 계획에 nested loop + 인덱스 range 가 나온다.
  "SELECT COUNT(*) FROM order_items a JOIN order_items b ON b.order_id BETWEEN a.order_id AND a.order_id+80"
  # ~7s — STRAIGHT_JOIN + 계산식 조건. 인덱스를 쓸 수 없어 full scan 두 번이다.
  "SELECT COUNT(*) FROM order_items a STRAIGHT_JOIN customers c ON c.id % 13 = a.id % 13"
  # ~4s — GROUP BY + filesort. 정렬이 계획에 드러난다.
  "SELECT o.customer_id, COUNT(*) n, SUM(oi.qty*oi.unit_price) amt
     FROM orders o JOIN order_items oi ON oi.order_id = o.id
     GROUP BY o.customer_id ORDER BY amt DESC"
  # ~3s — 상관 서브쿼리. DEPENDENT SUBQUERY 가 계획에 나온다.
  "SELECT c.id, c.email FROM customers c
    WHERE (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) > 2
    ORDER BY c.id DESC LIMIT 100"
  # ~5s — DISTINCT + LEFT JOIN 두 단계.
  "SELECT COUNT(DISTINCT c.email) FROM customers c
     LEFT JOIN orders o ON o.customer_id + 0 = c.id
     LEFT JOIN order_items oi ON oi.order_id + 0 = o.id"
)

# **접속을 먼저 한 번 확인한다.** 안 하면 실패가 초당 수백 번 반복되면서 로그만
# 쌓인다(실측: 18849회). 여기서 죽는 것이 낫다.
if ! mysql -h "$host" -u "$user" "$SCHEMA" --connect-timeout 10 -N -e "SELECT 1" >/dev/null 2>&1; then
  echo "접속 확인 실패: $user@$host/$SCHEMA — 시크릿·스키마·보안그룹을 확인한다" >&2
  mysql -h "$host" -u "$user" "$SCHEMA" --connect-timeout 10 -N -e "SELECT 1" 2>&1 | head -2 >&2
  exit 1
fi

echo "대상   : $INSTANCE ($host)"
echo "계정   : $user"
echo "스키마 : $SCHEMA"
echo "설정   : ${DURATION_SEC}초 동안 동시 ${CONCURRENCY}개"
echo

deadline=$(( $(date +%s) + DURATION_SEC ))

worker() {
  local id="$1" n=0
  while [[ $(date +%s) -lt $deadline ]]; do
    # 워커마다 다른 쿼리에서 시작해 다이제스트가 골고루 섞이게 한다.
    local q="${QUERIES[$(( (n + id) % ${#QUERIES[@]} ))]}"
    local start=$(date +%s%N)
    if mysql -h "$host" -u "$user" "$SCHEMA" --connect-timeout 10 -N -e "$q" >/dev/null 2>&1; then
      printf '  w%d #%02d %5dms ok\n' "$id" "$n" "$(( ($(date +%s%N) - start) / 1000000 ))"
    else
      printf '  w%d #%02d %5dms 실패\n' "$id" "$n" "$(( ($(date +%s%N) - start) / 1000000 ))" >&2
    fi
    n=$((n + 1))
  done
}

for i in $(seq 1 "$CONCURRENCY"); do
  worker "$i" &
done
wait

echo
echo "완료. 화면에서 확인: \$(./scripts/dbmon-url.sh)"
