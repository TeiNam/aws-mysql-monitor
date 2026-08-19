#!/usr/bin/env bash
# 부하 생성기 — 검증 시나리오 9종 (M0-13d, [18 §5.1](../docs/18-dev-environment.md)).
#
# 각 시나리오는 **수집기가 다뤄야 하는 구체적인 상황**을 만든다. 랜덤 부하가 아니다 —
# 설계가 전제하는 동작을 하나씩 유발해 확인할 수 있어야 한다.
#
#   just seed              # 전부 (Ctrl-C 로 중단)
#   just seed-one lock     # 하나만
#   just seed-list         # 목록
#
# 의존성은 `docker compose` 와 `bash` 뿐이다. MySQL 클라이언트도 컨테이너 것을 쓴다.

set -uo pipefail

SERVICE="${DBMON_LOADGEN_SERVICE:-mysql84}"
DB="${DBMON_LOADGEN_DB:-shop}"
USER="${DBMON_LOADGEN_USER:-loadgen}"
PASS="${DBMON_LOADGEN_PASS:-dbmon-local-loadgen}"

# 배경 프로세스를 추적해 종료 시 정리한다. 정리하지 않으면 다음 실행이 이전 부하를 본다.
declare -a BG_PIDS=()

cleanup() {
  local n=${#BG_PIDS[@]}
  [[ $n -eq 0 ]] && return
  echo
  echo "정리 중 (배경 세션 ${n}개)..."
  for pid in "${BG_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  # 서버 측에 남은 장기 실행 쿼리도 죽인다.
  mysql_root -e "
    SELECT ID INTO @dummy FROM information_schema.PROCESSLIST LIMIT 1;
  " >/dev/null 2>&1 || true
  mysql_root -N -B -e "
    SELECT CONCAT('KILL QUERY ', ID, ';') FROM information_schema.PROCESSLIST
    WHERE USER = '${USER}' AND COMMAND <> 'Sleep'
  " 2>/dev/null | mysql_root >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

mysql_as() { docker compose exec -T "$SERVICE" mysql -u"$USER" -p"$PASS" -D "$DB" "$@" 2>/dev/null; }
mysql_root() { docker compose exec -T "$SERVICE" mysql -uroot -p"dbmon-local-root" -D "$DB" "$@" 2>/dev/null; }

# 배경에서 SQL 을 실행하고 PID 를 기록한다.
bg() {
  mysql_as -e "$1" >/dev/null &
  BG_PIDS+=("$!")
}

require_up() {
  if ! mysql_as -e "SELECT 1" >/dev/null; then
    echo "오류: ${SERVICE} 에 접속할 수 없다. 'just dev' 를 먼저 실행한다." >&2
    exit 1
  fi
}

# ── 시나리오 ────────────────────────────────────────────────────────────────

# ① 1024바이트를 넘는 SQL — `performance_schema.processlist.INFO` 절단을 유발한다.
#    `information_schema.PROCESSLIST` 는 65,535바이트까지 온전해야 한다 (19 §A).
scenario_longsql() {
  echo "① 긴 SQL (약 8KB) — PS 는 1024 로 절단, IS 는 온전해야 한다"
  local list
  list=$(seq 1 1200 | paste -sd, -)
  bg "SELECT COUNT(*) FROM orders WHERE id = 1 AND SLEEP(6) = 0 AND id IN ($list)"
}

# ② 장기 실행 DML — 1세대가 플랜을 포기한 경우다.
#    읽기 전용 계정은 `EXPLAIN UPDATE` 를 못 하므로 SELECT 변환 경로를 타야 한다 (19 §B).
scenario_dml() {
  echo "② 장기 실행 UPDATE — rerun_as_select 근사 플랜 경로"
  bg "START TRANSACTION; UPDATE orders SET memo = 'loadgen' WHERE status = 'PAID' AND SLEEP(8) = 0; ROLLBACK"
}

# ③ 락 경합 — `sys.innodb_lock_waits` 와 2차 조회로 전문 SQL 확보를 확인한다.
scenario_lock() {
  echo "③ 락 경합 — 한 세션이 행을 잡고 다른 세션이 기다린다"
  bg "START TRANSACTION; UPDATE lock_arena SET val = val + 1 WHERE id = 1; SELECT SLEEP(9); ROLLBACK"
  sleep 1
  bg "START TRANSACTION; UPDATE lock_arena SET val = val + 100 /* 이 주석은 SQL 을 64자보다 길게 만들어 sys 뷰의 절단을 관찰하기 위한 것이다 */ WHERE id = 1; ROLLBACK"
}

# ④ 데드락 — `SHOW ENGINE INNODB STATUS` 파싱과 중복 병합을 확인한다.
scenario_deadlock() {
  echo "④ 데드락 — 두 세션이 반대 순서로 잠근다"
  bg "START TRANSACTION; UPDATE lock_arena SET val=val+1 WHERE id=2; SELECT SLEEP(2); UPDATE lock_arena SET val=val+1 WHERE id=3; COMMIT"
  bg "START TRANSACTION; UPDATE lock_arena SET val=val+1 WHERE id=3; SELECT SLEEP(2); UPDATE lock_arena SET val=val+1 WHERE id=2; COMMIT"
}

# ⑤ 다이제스트 1000종 — `performance_schema_digests_size` 압박과 상위 N 선별을 확인한다.
scenario_digests() {
  echo "⑤ 서로 다른 다이제스트 1000종 — 상위 N + _other 집계"
  # 컬럼 별칭을 바꾸면 다이제스트가 달라진다. 리터럴만 바꾸면 같은 다이제스트다.
  local sql=""
  for i in $(seq 1 1000); do
    sql+="SELECT id AS c_${i} FROM orders WHERE id = ${i} LIMIT 1; "
  done
  mysql_as -e "$sql" >/dev/null &
  BG_PIDS+=("$!")
}

# ⑥ 풀 테이블 스캔 — `access_type=ALL` 경고와 낮은 선택도를 확인한다.
scenario_fullscan() {
  echo "⑥ 풀스캔 — status 에 인덱스가 없다"
  bg "SELECT COUNT(*) FROM orders WHERE status = 'PENDING' AND SLEEP(0.0001) = 0"
}

# ⑦ filesort + 임시 테이블 — 이 플래그는 테이블 노드가 아니라 연산 노드에 붙는다.
scenario_sort() {
  echo "⑦ filesort + 임시 테이블 — ordering_operation 노드에 플래그가 붙는다"
  bg "SELECT status, COUNT(*) c FROM orders WHERE SLEEP(0.00005) = 0 GROUP BY memo, status ORDER BY c DESC"
}

# ⑧ sub-second 고빈도 — 슬로우 쿼리 임계값 아래라 실시간 캡처에 안 걸린다.
#    다이제스트 스냅샷만이 이 워크로드를 관측한다 (1세대의 사각지대).
scenario_subsecond() {
  echo "⑧ sub-second 고빈도 — 다이제스트 스냅샷만 관측할 수 있다"
  local sql=""
  for i in $(seq 1 300); do
    sql+="SELECT COUNT(*) FROM order_items WHERE order_id = $(( (i * 37) % 60000 + 1 )); "
  done
  mysql_as -e "$sql" >/dev/null &
  BG_PIDS+=("$!")
}

# ⑨ 다중 행 INSERT — MySQL 이 `VALUES (...) /* , ... */` 로 축약한다 (19 §C-2).
scenario_multiinsert() {
  echo "⑨ 다중 행 INSERT — 행 수가 달라도 같은 다이제스트여야 한다"
  mysql_as -e "
    START TRANSACTION;
    INSERT INTO lock_arena (id, val) VALUES (901,1),(902,2) ON DUPLICATE KEY UPDATE val=val+1;
    INSERT INTO lock_arena (id, val) VALUES (903,1),(904,2),(905,3),(906,4) ON DUPLICATE KEY UPDATE val=val+1;
    ROLLBACK;
  " >/dev/null
}

SCENARIOS=(longsql dml lock deadlock digests fullscan sort subsecond multiinsert)

usage() {
  cat <<EOF
사용법: $0 <시나리오|all|--list>

시나리오:
  longsql      1024바이트 초과 SQL (PS 절단 / IS 비절단 확인)
  dml          장기 실행 UPDATE (rerun_as_select 경로)
  lock         락 경합 (sys.innodb_lock_waits + 2차 조회)
  deadlock     데드락 (SHOW ENGINE INNODB STATUS 파싱)
  digests      서로 다른 다이제스트 1000종 (상위 N + _other)
  fullscan     풀 테이블 스캔 (access_type=ALL 경고)
  sort         filesort + 임시 테이블 (연산 노드 플래그)
  subsecond    sub-second 고빈도 (다이제스트 스냅샷 전용)
  multiinsert  다중 행 INSERT (VALUES 축약)

환경변수:
  DBMON_LOADGEN_SERVICE  대상 compose 서비스 (기본 mysql84)
EOF
}

main() {
  case "${1:-}" in
    ""|-h|--help) usage; exit 0 ;;
    --list) printf '%s\n' "${SCENARIOS[@]}"; exit 0 ;;
  esac

  require_up

  if [[ "$1" == "all" ]]; then
    echo "전 시나리오 실행. Ctrl-C 로 중단한다."
    echo
    for s in "${SCENARIOS[@]}"; do
      "scenario_$s"
      sleep 1
    done
    echo
    echo "부하 생성 중. 다른 창에서 확인한다:"
    echo "  just spike"
    echo "  just test-collector"
    echo "  just sql -e \"SELECT ID,USER,TIME,LEFT(INFO,60) FROM information_schema.PROCESSLIST WHERE INFO IS NOT NULL\""
    wait
  else
    local fn="scenario_$1"
    if ! declare -F "$fn" >/dev/null; then
      echo "알 수 없는 시나리오: $1" >&2
      usage >&2
      exit 1
    fi
    "$fn"
    wait
  fi
}

main "$@"
