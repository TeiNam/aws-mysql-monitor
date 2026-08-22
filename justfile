# dbmon 개발 명령. 'just' (https://github.com/casey/just)
#
# 설계 원칙: **AWS 없이 전부 돌아간다.** SSO 세션이 만료돼도 개발이 멈추지 않는다.
# AWS 가 필요한 명령은 이름에 'aws-' 를 붙여 구분한다.

set shell := ["bash", "-uc"]

# 로컬 MySQL 접속 정보 (docker-compose.yml 과 맞춘다)
mysql84 := "-h127.0.0.1 -P13306"
mysql84_wide := "-h127.0.0.1 -P13307"
mysql80 := "-h127.0.0.1 -P13308"
root_pw := "dbmon-local-root"

_default:
    @just --list --unsorted

# ── 로컬 환경 ────────────────────────────────────────────────────────────────

# 로컬 개발 환경을 띄운다 (MySQL 8.4 / 8.4-wide / 8.0 / DynamoDB Local)
dev:
    docker compose up -d
    @just wait

# 컨테이너가 준비될 때까지 기다린다
wait:
    #!/usr/bin/env bash
    set -uo pipefail
    echo "컨테이너 준비 대기..."
    for i in $(seq 1 60); do
      ready=$(docker compose ps --format '{{{{.Health}}}}' 2>/dev/null | grep -c healthy || true)
      if [ "$ready" -ge 3 ]; then echo "준비 완료 (healthy $ready)"; exit 0; fi
      sleep 2
    done
    echo "시간 초과. docker compose logs 를 확인한다." >&2
    exit 1

# 로컬 환경을 내린다 (데이터 유지)
down:
    docker compose down

# 로컬 환경을 완전히 지운다 (볼륨까지)
clean:
    docker compose down -v

# 컨테이너 상태
ps:
    docker compose ps

# 로컬 MySQL 셸 (8.4)
sql *ARGS:
    docker compose exec mysql84 mysql -uroot -p{{root_pw}} -D shop {{ARGS}}

# ── 부하 생성 ────────────────────────────────────────────────────────────────

# 검증 시나리오 9종을 생성한다 (M0-13d). Ctrl-C 로 중단
seed:
    ./local/loadgen.sh all

# 시나리오 하나만 실행. 'just seed-one lock' 처럼 쓴다
seed-one SCENARIO:
    ./local/loadgen.sh {{SCENARIO}}

# 사용 가능한 시나리오 목록
seed-list:
    ./local/loadgen.sh --list

# ── 테스트 ──────────────────────────────────────────────────────────────────

# 단위 테스트 + 인프라 일관성 (Docker 불필요, 1초 이내)
test:
    cargo test --workspace --lib
    cargo test -p dbmon --test it_infra_consistency

# 통합 테스트까지 (Docker 필요)
test-all:
    cargo test --workspace

# M1 검증 스파이크 — 실측 수치를 출력한다
spike:
    cargo test -p dbmon --test m1_digest -- --nocapture --test-threads=1
    cargo test -p dbmon --test m1_capture -- --nocapture --test-threads=1

# 수집 파이프라인 통합 테스트
test-collector:
    cargo test -p dbmon --test it_collector -- --nocapture

# 저장소 통합 테스트 — DynamoDB Local 에 실제로 쓰고 읽는다 (AWS 불필요)
test-store:
    cargo test -p dbmon --test it_store -- --nocapture

# ── 품질 게이트 ──────────────────────────────────────────────────────────────

# 커밋 전에 돌리는 것 전부
check: fmt-check lint test docs-check tf-check

fmt:
    cargo fmt --all
    terraform fmt -recursive infra

fmt-check:
    cargo fmt --all -- --check
    terraform fmt -recursive -check infra

lint:
    cargo clippy --workspace --all-targets -- -D warnings

# 문서 링크 검사
docs-check:
    ./scripts/check-docs.py

# Terraform 전 레이어 validate
tf-check:
    #!/usr/bin/env bash
    set -uo pipefail
    fail=0
    for d in infra/layers/*/; do
      ( cd "$d" && terraform init -backend=false -input=false >/dev/null 2>&1
        if terraform validate 2>&1 | grep -q Success; then
          printf '  ✓ %s\n' "$(basename "$d")"
        else
          printf '  ✗ %s\n' "$(basename "$d")"; terraform validate 2>&1 | head -20; exit 1
        fi ) || fail=1
    done
    exit $fail

# ── 실행 ────────────────────────────────────────────────────────────────────

# 로컬 설정으로 서버를 띄운다
run *ARGS:
    # **'DBMON_TARGET_PASSWORD' 가 없으면 dev 도 IAM DB Auth 폴백을 탄다.**
    # 그러면 로컬 MySQL 접속에서 'caching_sha2_password' 가 ~1000바이트 토큰을
    # RSA 로 암호화하려다 드라이버가 패닉한다. 값은 local/seed/03-monitor-users.sql
    # 의 'dbmon' 계정 비밀번호다.
    DBMON_TARGET_PASSWORD=dbmon-local-monitor \
      cargo run -p dbmon -- --config local/dbmon.toml --log-pretty serve {{ARGS}}

# 로컬 저장소를 준비한다 — DynamoDB Local 테이블 + 로컬 MySQL 을 감시 대상으로 등록
#
# **왜 등록이 필요한가**: RDS 탐색은 AWS 를 호출하므로 로컬에서는 아무것도 찾지 못한다.
# 등록부가 비면 수집 태스크가 0개이므로, 로컬 MySQL 을 직접 등록해 그 뒤 경로
# (수집 → 정규화 → 저장 → 백필 병합)를 전부 돌린다.
local-init:
    #!/usr/bin/env bash
    set -euo pipefail
    export AWS_ACCESS_KEY_ID=local AWS_SECRET_ACCESS_KEY=local AWS_REGION=ap-northeast-2
    ddb() { aws dynamodb --endpoint-url http://127.0.0.1:18000 "$@"; }

    if ! ddb describe-table --table-name dbmon-data-local >/dev/null 2>&1; then
      # 스키마는 JSON 파일로 둔다. CLI 단축 문법은 중첩 JSON 을 받지 않고,
      # just 는 이중 중괄호를 보간으로 해석하므로 인라인으로 쓰면 양쪽에서 깨진다.
      ddb create-table --cli-input-json file://local/table.json >/dev/null
      echo "테이블 생성: dbmon-data-local"
    else
      echo "테이블 있음: dbmon-data-local"
    fi

    # 설정 테이블. **정지 스코프가 여기 산다** — 없으면 리더 루프가 매 tick
    # 'ResourceNotFoundException' 을 찍고 정지가 동작하지 않는다.
    if ! ddb describe-table --table-name dbmon-config-local >/dev/null 2>&1; then
      ddb create-table --cli-input-json file://local/config-table.json >/dev/null
      echo "테이블 생성: dbmon-config-local"
    else
      echo "테이블 있음: dbmon-config-local"
    fi

    # 로컬 MySQL 을 감시 대상으로 등록한다. 'endpoint' 가 루프백이므로 평문 접속
    # 경로를 탄다('dev' + 루프백일 때만 허용된다 — 'mysql::connect' 참고).
    ddb put-item --table-name dbmon-data-local --item file://local/instance.json >/dev/null
    echo "인스턴스 등록: mysql84-local (127.0.0.1:13306)"
    ddb delete-item --table-name dbmon-data-local       --key '{"PK":{"S":"LEASE#LEADER#collect"},"SK":{"S":"L"}}' >/dev/null 2>&1 || true

# 로컬 상태를 지운다 (테이블 삭제)
local-reset:
    #!/usr/bin/env bash
    set -euo pipefail
    export AWS_ACCESS_KEY_ID=local AWS_SECRET_ACCESS_KEY=local AWS_REGION=ap-northeast-2
    aws dynamodb --endpoint-url http://127.0.0.1:18000       delete-table --table-name dbmon-data-local >/dev/null 2>&1 || true
    aws dynamodb --endpoint-url http://127.0.0.1:18000       delete-table --table-name dbmon-config-local >/dev/null 2>&1 || true
    echo "테이블 삭제: dbmon-data-local, dbmon-config-local"

# 저장된 슬로우 쿼리를 본다
local-show:
    #!/usr/bin/env bash
    set -euo pipefail
    export AWS_ACCESS_KEY_ID=local AWS_SECRET_ACCESS_KEY=local AWS_REGION=ap-northeast-2
    aws dynamodb --endpoint-url http://127.0.0.1:18000 scan       --table-name dbmon-data-local       --filter-expression 'begins_with(PK, :p)'       --expression-attribute-values '{":p":{"S":"SQ#"}}'       --query 'Items[].{dur_ms:duration_ms.N,src:duration_source.S,cap:capture_source.S,rows:stats.M.rows_examined.N,state:state.S,sql:sql_text.S}'       --output table

# 느린 쿼리를 하나 만든다 (관측 대상)
local-slow SECS="5":
    docker exec dbmon-dev-mysql84-1 mysql -uloadgen -pdbmon-local-loadgen -D shop       -e "SELECT /* demo */ COUNT(*) FROM orders o JOIN order_items i ON i.order_id=o.id WHERE SLEEP({{SECS}})=0;"

# 전체 데모: 컨테이너 → 준비 → 기동. 다른 터미널에서 'just local-slow' 를 쏜다.
demo: dev wait local-init
    @echo ""
    @echo "  준비됐다. 이 창은 서버가 점유한다."
    @echo "  다른 터미널에서:  just local-slow   → 느린 쿼리 발생"
    @echo "                    just local-show   → 저장된 레코드 확인"
    @echo ""
    just run

# 설정만 검증한다
config-check:
    cargo run -q -p dbmon -- --config local/dbmon.toml check

# ── 컨테이너 ────────────────────────────────────────────────────────────────

# arm64 이미지를 빌드한다 (ECS 태스크가 Graviton 이다)
image:
    docker build --platform linux/arm64 -t dbmon:dev .

# 이미지 스모크 테스트 — 헬스체크와 SIGTERM 반응까지 본다
image-test: image
    #!/usr/bin/env bash
    set -euo pipefail
    docker rm -f dbmon-smoke >/dev/null 2>&1 || true
    docker run -d --name dbmon-smoke -p 18080:8080 \
      -e DBMON__DEPLOYMENT_ENV=prd -e DBMON__AWS__REGION=ap-northeast-2 \
      -e DBMON__AWS__ACCOUNT_ID=000000000000 \
      -e DBMON__STORAGE__DATA_TABLE=d -e DBMON__STORAGE__CONFIG_TABLE=c \
      -e DBMON__HTTP__DEREGISTRATION_WAIT_SECS=0 \
      dbmon:dev serve >/dev/null
    sleep 6
    echo "HEALTHCHECK: $(docker inspect dbmon-smoke --format '{{{{.State.Health.Status}}}}')"
    echo "healthcheck 종료코드: $(docker exec dbmon-smoke dbmon healthcheck; echo $?)"
    # just 는 이중 중괄호를 보간으로 해석한다. 셸에 리터럴 중괄호를 넘기려면
    # 두 배로 쓴다. 주석에도 이중 중괄호를 쓰면 안 된다 — just 가 주석 안에서도
    # 그걸 보간 시작으로 읽고, 닫히지 않으면 한참 뒤 줄에서 파싱이 죽는다.
    echo "/healthz: $(curl -s -o /dev/null -w '%{{{{http_code}}}}' http://127.0.0.1:18080/healthz)"
    start=$(date +%s); docker stop -t 60 dbmon-smoke >/dev/null
    echo "SIGTERM 정지: $(( $(date +%s) - start ))초 (PID 1 이 dbmon 이면 즉시)"
    docker rm -f dbmon-smoke >/dev/null

# 모니터를 컨테이너로 띄우고 화면 접속 URL 을 찍는다
#
# 컨테이너는 루프백에 바인드할 수 없으므로 인증 우회가 적용되지 않는다.
# 기동 시 발급된 토큰이 있어야 들어간다 — URL 을 그대로 브라우저에 붙인다.
docker-up:
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose --profile monitor up -d --build dbmon
    echo "토큰 URL 을 기다린다…"
    for i in $(seq 1 30); do
      url=$(docker compose logs dbmon 2>/dev/null | grep -oE 'http://127\.0\.0\.1:8080/\?token=[a-f0-9]+' | tail -1 || true)
      if [ -n "$url" ]; then
        echo ""
        echo "  화면:  ${url/:8080/:18080}"
        echo ""
        exit 0
      fi
      sleep 1
    done
    echo "토큰이 나오지 않았다. docker compose logs dbmon 을 확인한다." >&2
    exit 1

# 모니터 컨테이너를 내린다
docker-down:
    docker compose --profile monitor rm -sf dbmon

# ── AWS 가 필요한 것 (SSO 세션 필요) ─────────────────────────────────────────

# 현재 자격증명을 확인한다
aws-whoami:
    aws sts get-caller-identity

# 주석 규칙: 백틱과 이중 중괄호를 쓰지 않는다. just 1.58 은 주석 안에서도 둘을
# 렉싱하고, 닫히지 않으면 파일 전체 파싱이 (한참 뒤 줄에서) 실패한다.

# 레이어를 plan 한다 (예: just aws-plan 10-foundation)
aws-plan LAYER:
    cd infra/layers/{{LAYER}} && terraform init -input=false && terraform plan
