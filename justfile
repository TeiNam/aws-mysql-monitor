# dbmon 개발 명령. `just` (https://github.com/casey/just)
#
# 설계 원칙: **AWS 없이 전부 돌아간다.** SSO 세션이 만료돼도 개발이 멈추지 않는다.
# AWS 가 필요한 명령은 이름에 `aws-` 를 붙여 구분한다.

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

# 시나리오 하나만 실행. `just seed-one lock` 처럼 쓴다
seed-one SCENARIO:
    ./local/loadgen.sh {{SCENARIO}}

# 사용 가능한 시나리오 목록
seed-list:
    ./local/loadgen.sh --list

# ── 테스트 ──────────────────────────────────────────────────────────────────

# 단위 테스트만 (Docker 불필요, 1초 이내)
test:
    cargo test --workspace --lib

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
    cargo run -p dbmon -- --config local/dbmon.toml --log-pretty serve {{ARGS}}

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
    # just 는 `{{` 를 보간으로 해석한다. 셸에 리터럴 `{`를 넘기려면 두 배로 쓴다.
    echo "/healthz: $(curl -s -o /dev/null -w '%{{{{http_code}}}}' http://127.0.0.1:18080/healthz)"
    start=$(date +%s); docker stop -t 60 dbmon-smoke >/dev/null
    echo "SIGTERM 정지: $(( $(date +%s) - start ))초 (PID 1 이 dbmon 이면 즉시)"
    docker rm -f dbmon-smoke >/dev/null

# ── AWS 가 필요한 것 (SSO 세션 필요) ─────────────────────────────────────────

# 현재 자격증명을 확인한다
aws-whoami:
    aws sts get-caller-identity

# 레이어를 plan 한다. `just aws-plan 10-foundation`
aws-plan LAYER:
    cd infra/layers/{{LAYER}} && terraform init -input=false && terraform plan
