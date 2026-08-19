# aws-mysql-monitor

AWS RDS/Aurora MySQL 슬로우 쿼리 캡처 · 실행계획 수집 · 성능 분석 플랫폼.

기존 PoC 2종(`../my_slow_query_scraper`, `../my_slow_query_dashboard`)의 운영 경험을 반영한
2세대 재설계입니다. 이 저장소는 현재 **설계 단계**이며, 아래 문서가 산출물입니다.

## 한 줄 요약

RDS를 자동 탐색해 환경(prd/stg/dev)별로 분류하고, MySQL 워크로드를 3중 소스로 수집해
다이제스트 단위로 묶어 보여주며, 실행계획·카디널리티·스키마를 근거로 Bedrock이 개선안을
제시하고, 월간/개선 리포트를 자동 생성한다.

## 확정된 기술 선택

| 영역 | 선택 | 근거 |
|---|---|---|
| 백엔드 | Rust (axum + tokio), 단일 바이너리 | [ADR-001](docs/03-decisions.md), [ADR-002](docs/03-decisions.md) |
| 프론트 | React 19 + TS + Vite, 차트는 uPlot 단독 | [ADR-013](docs/03-decisions.md) |
| 핫 저장소 | DynamoDB 31일 (단일 진실 원천) | [ADR-003](docs/03-decisions.md) |
| 콜드 저장소 | S3 Tables(Iceberg) 1년. 적재는 DDB 증분 내보내기 → Athena `MERGE INTO` | [ADR-004](docs/03-decisions.md) |
| 조회 엔진 | Athena 단독. Spark 미도입 | [ADR-020](docs/03-decisions.md) |
| 인증 | Cognito OIDC (PKCE) + 서버측 RBAC | [ADR-012](docs/03-decisions.md) |
| DB 계정 | IAM DB Auth (비밀번호 없음) | [ADR-007](docs/03-decisions.md) |
| 배포 | **ECS Fargate (ARM64)** — active 1 + standby 1 | [ADR-022](docs/03-decisions.md), [14](docs/14-infrastructure.md) |
| IaC | **Terraform 7개 레이어, 레이어별 독립 state** — 앱 코드와 무관하게 apply 가능 | [14 §1](docs/14-infrastructure.md) |
| 개발계 | 계정 `123456789012` / ap-northeast-2. **네트워크 비용 $0** (SSM 포트 포워딩) | [18-dev-environment.md](docs/18-dev-environment.md) |
| 지원 버전 | RDS MySQL 8.4+, Aurora MySQL 3.05+ | [ADR-019](docs/03-decisions.md) |

## 설계 문서

| # | 문서 | 내용 |
|---|---|---|
| 01 | [요구사항](docs/01-requirements.md) | 기능(FR)·비기능(NFR) 요구사항, 범위, 제약, 가정 |
| 02 | [시스템 아키텍처](docs/02-architecture.md) | 컴포넌트 경계, 데이터 흐름, 배치 구조, 실패 모드 |
| 03 | [기술 결정 기록(ADR)](docs/03-decisions.md) | 22개 결정의 근거·대가·재검토 조건 |
| 04 | [데이터 모델](docs/04-data-model.md) | DynamoDB 단일테이블, Iceberg 스키마, 증분 적재 SQL |
| 05 | [수집기 설계](docs/05-collector.md) | 3중 소스 캡처, 사용 SQL 전량, 정규화, 샤딩 |
| 06 | [RDS 탐색 & 메트릭](docs/06-discovery-metrics.md) | 자동 탐색, 환경 분류, CloudWatch 비용 통제 |
| 07 | [자격증명 부트스트랩](docs/07-credentials-bootstrap.md) | 마스터 계정 소스, 모니터링 계정 생성, IAM DB Auth |
| 08 | [보안 & 인증](docs/08-security-auth.md) | 위협 모델 39개, Cognito, RBAC, IAM 정책, 감사 |
| 09 | [프론트엔드 설계](docs/09-frontend.md) | 화면 명세, 실시간 렌더링, 플랜 시각화 |
| 10 | [알림](docs/10-alerting.md) | 규칙 모델, 평가/발송 분리, Slack/Telegram/인앱 |
| 11 | [AI 어드바이저](docs/11-ai-advisor.md) | 규칙 엔진 + Bedrock, 응답 검증, 인젝션 방어 |
| 12 | [리포팅](docs/12-reporting.md) | 월간·개선 리포트, typed template, 스냅샷 고정 |
| 13 | [API 명세](docs/13-api-spec.md) | REST/WS 엔드포인트, 권한, 커서, 에러 |
| 14 | [인프라 & 운영](docs/14-infrastructure.md) | Terraform, 배포, 관측성, 런북, 재해복구 |
| 15 | [테스트 전략](docs/15-testing.md) | 골든 코퍼스, 계약 테스트, 부하, 회귀 대장 53개 |
| 16 | [비용 모델](docs/16-cost.md) | 100/500대 월비용, 비용 폭발 시나리오와 방어 |
| 17 | [로드맵 & 할일 목록](docs/17-roadmap-tasks.md) | M0~M12, 태스크 300여 개, 수용 기준 |
| 18 | [개발계(dev) 배포 설계](docs/18-dev-environment.md) | **실제 계정 실측 기반.** Terraform 레이어, dev 접근 모델, prd 혼재 격리, 시드 MySQL, dev 비용 |
| 19 | [M1 검증 스파이크 실측 결과](docs/19-m1-findings.md) | **실제 MySQL 8.4.11/8.0.46 측정.** ADR-006 무효화, 정규화 규칙 11건 정정, 절단 상한 |
| — | [미해결 이슈](docs/OPEN-QUESTIONS.md) | 검증 필요 + 결정 대기 21건 (3건 실측으로 해소) |

## 개발 시작하기

**AWS 없이 전부 돌아간다.** SSO 세션이 만료돼도 개발이 멈추지 않는다.

```bash
docker compose up -d                 # MySQL 8.4 / 8.4-wide / 8.0 / DynamoDB Local
cargo test --workspace               # 단위 + 통합 테스트 302건
./local/loadgen.sh all               # 검증 시나리오 9종 생성
cargo run -p dbmon -- --config local/dbmon.toml --log-pretty serve
```

[`just`](https://github.com/casey/just) 가 있으면 `just dev` · `just test` · `just check` ·
`just spike` 로 줄여 쓸 수 있다([justfile](justfile) 에 전 명령이 있다). 없어도 된다.

| 확인하고 싶은 것 | 명령 |
|---|---|
| M1 실측 수치 재현 | `cargo test -p dbmon --test m1_digest -- --nocapture --test-threads=1` |
| 캡처 파이프라인 E2E | `cargo test -p dbmon --test it_collector -- --nocapture` |
| 컨테이너 스모크 | `docker build --platform linux/arm64 -t dbmon:dev . && docker run --rm dbmon:dev --version` |
| Terraform 전 레이어 | `for d in infra/layers/*/; do (cd $d && terraform init -backend=false >/dev/null && terraform validate); done` |
| 문서 링크 | `./scripts/check-docs.py` |

## 읽는 순서

- **처음 보는 사람**: 01 → 02 → 03 → 17
- **구현 시작하는 사람**: 17(할일) → 담당 영역 문서 → 13(API) → 15(테스트)
- **인프라 담당**: **18(개발계)** → 14 → 07 → 08 → 16
- **리뷰어**: 03(결정 근거) → [OPEN-QUESTIONS](docs/OPEN-QUESTIONS.md)

## 구현 전에 반드시 해야 할 것

**M1 검증 스파이크**([17](docs/17-roadmap-tasks.md#m1-검증-스파이크-매우-중요))를 건너뛰면
M4~M7에서 재작업이 발생한다. 설계가 전제하는 17가지 동작을 실측으로 확인한다.
특히 다음 4개는 실패 시 설계가 바뀐다.

| 항목 | 실패 시 |
|---|---|
| ~~`information_schema.PROCESSLIST.INFO` 비절단~~ → **해소**. 65,535바이트 상한 확인 ([19 §A](docs/19-m1-findings.md)) | 해당 없음 |
| Athena `MERGE INTO` 멱등성 ([OPEN-Q-03](docs/OPEN-QUESTIONS.md)) | 플레인 S3 Parquet + 자체 컴팩션으로 전환 |
| 워커 1대가 500대를 커버 ([OPEN-Q-01](docs/OPEN-QUESTIONS.md)) | 펜싱 토큰·팬아웃이 M12가 아니라 선행 조건이 된다 |

**실측으로 해소된 것** (2026-08-19, 계정 123456789012)

| 항목 | 결과 |
|---|---|
| S3 Tables 리전 가용성 + Glue 카탈로그 형식 | **사용 가능.** `"s3tablescatalog/<bucket>"."<ns>"."<table>"` 확정. Athena engine v3 |
| 플릿 능력 인벤토리 (M1-12) | **전 리전 RDS 0개** → 8.4+ 고정이 업그레이드 프로젝트가 될 위험 없음. 시드 인스턴스를 만들면 된다 |
| 크로스 리전 요구 ([OPEN-Q-02](docs/OPEN-QUESTIONS.md)) | 타 리전 RDS 0개 → 현재 불필요. 코드 경로만 유지 |
| Bedrock 모델 가용성 | Claude 다수 + `global`/`apac` 추론 프로파일 |

**⚠ 개발계 계정에 프로덕션 워크로드가 함께 있다** — `prd-lla-vpc`(10.3.0.0/16)에 실행 중
EC2 1대. VPC 피어링·TGW가 없어 네트워크는 격리되어 있지만, 우리 IAM 설계의
`rds:DescribeDBInstances` `Resource:"*"`와 `rds-db:connect` on `dbuser:*/dbmon`는
prd 인스턴스까지 커버한다(T-37). 3중 격리를 [18 §6](docs/18-dev-environment.md)에 설계했다.

**사용자 결정이 필요한 것**: prd 리터럴 저장 정책([OPEN-Q-15](docs/OPEN-QUESTIONS.md)).
이 결정이 "복사해서 바로 실행할 수 있는 샘플 쿼리" 기능이 prd에서 켜지는지를 정한다.

## 1세대에서 이어받는 것 / 버리는 것

| 항목 | 1세대 | 2세대 | 이유 |
|---|---|---|---|
| 캡처 소스 | `performance_schema.processlist` | PS 탐지 + `information_schema.PROCESSLIST` 타깃 조회 + PS 다이제스트 + CW 슬로우로그 | PS `processlist.INFO`는 1024바이트 절단 ([ADR-005](docs/03-decisions.md)) |
| 실행계획 (SELECT) | 사후 `EXPLAIN` 재실행 | **사후 재실행 (같다)** | `FOR CONNECTION` 이 RDS 에서 불가 ([19 §B](docs/19-m1-findings.md)) |
| 실행계획 (DML) | 없음 | 조건절을 SELECT 로 바꾼 **근사** 플랜 | `EXPLAIN UPDATE` 는 DML 권한 필요(1142) |
| 워크로드 전수조사 | 없음 (1초 미만 쿼리 미관측) | 다이제스트 델타 스냅샷 + 시간 롤업 | sub-second 워크로드 커버 ([ADR-010](docs/03-decisions.md)) |
| 저장소 | MongoDB | DynamoDB(31일) + S3 Tables/Iceberg(1년) | 운영 부담 제거, 장기 SQL 집계 |
| 백엔드 | Python/FastAPI | Rust/axum | 단일 바이너리·메모리 예측성·팀 역량 |
| 인증 | 없음 | Cognito OIDC + RBAC + 감사 | 접근 통제 |
| DB 계정 | 전 인스턴스 공통 비밀번호 | IAM DB Auth (비밀번호 없음) | 상시 자격증명 제거 |
| 계정별 통계 | 있었음 | **유지 + 확장** (`sys.user_summary_*`) | 1세대 대비 회귀 방지 ([ADR-021](docs/03-decisions.md)) |
| 환경 필터 | `env=prd` 태그 고정, 리전 1개 | 멀티 리전 + prd/stg/dev 분류 + 수동 오버라이드 | |
| 프론트 | React18/Vite/Recharts | React19 유지 + uPlot | 자산 재사용, 밀집 시계열 성능 |

## 설계 리뷰 이력

초안 작성 후 독립 리뷰를 거쳐 개정했다. **완료된 리뷰**와 **진행/미실시**를 구분해 적는다.

| 리뷰 관점 | 상태 | 발견 | 주요 반영 |
|---|---|---|---|
| MySQL/AWS 기술 사실 검증 | 완료 | 24건 | 증분 내보내기 포맷(`NewImage`), `sys.innodb_lock_waits` 컬럼명·64자 절단, `@@GLOBAL.uptime` 부재, Aurora 버전 비교 불가, CloudWatch period 규칙, `COLUMN_STATISTICS` 공백 |
| 보안 설계 | 완료 | 25건 | **플랜 JSON 리터럴 유출**(T-16), Athena 경로 RBAC 우회(T-17), `monitor_user` 인젝션(T-18), 감사 로그 변조(T-21), WS 마스킹 우회(T-22), 리포트 XSS(T-24) |
| 인프라·비용 현실성 | 완료 | 25건 | **EMF 커스텀 메트릭 월 $2,100**, EC2 단가 2.5배 과소, `MERGE INTO` 전량 스캔, NAT 시간당 요금, 크로스 AZ 양방향, 침묵 감지 알람 부재 |
| 요구사항 완결성 (원 요청 대비) | 완료 | 21건 | 첫 admin 부트스트랩 순환, prd `masked` 기본값이 핵심 기능 무력화, `in_flight` 미방송, Spark ADR 부재, 1세대 계정별 통계 회귀 |
| Codex 독립 비판 (교차 모델) | 완료 | 19건 | **`/* dbmon: */` 주석 자기식별 불가**(다이제스트가 주석 제거), 시간창 P95 계산 불가, 펜싱 토큰 부재, WS 팬아웃 모순, tool-use 자동검증 부재 |
| 문서 간 수치·설정값 일관성 | 완료 | 22건 | 다이제스트 영속화 조건이 `OR` vs `hard cap`으로 **정반대**, ADR-009 EMF 수치 stale, `dbmon-batch` 쿼리당 한도(100GB) > 일 한도(50GB) **도달 불가 설정**, 샤드 재할당 60초 vs 80초, 리스 루프 10초 vs 20초, 회귀 게이트가 R1~R20만 |
| 논리적 공백·아키텍처 결함 | 완료 | 31건 | **active 1대 불변식이 샤드 리스로 강제되지 않음**(F1), **`in_flight` 선행 저장이 마스킹 이전**(F2), **개선 리포트 p95 계산 불가**(F3), 고아 `in_flight` 유령 쿼리(F4), 3소스 병합 비대칭(F5), 커버리지 분모 결함(F6), 연결 풀 3개 vs 루프 5개(F7), 맵 JSON 저장으로 lost update(F8) |

자체 발견(리뷰 대기 중 직접 찾은 것): 다이제스트 스냅샷 페이로드 100배 문제(`LAST_SEEN` 필터
부재), EMF 메트릭 수 과소 계산(150 → 228), 비용 문서 절 제목과 소계 불일치, 첫 admin 부트스트랩
순환, 스키마 마이그레이션 절차 부재, 인스턴스 이름 변경 시 데이터 연결 끊김, 리포트 월 경계
시간대, 알림 평가/발송 분리.

리뷰에서 나온 사실 오류와 비용 오류는 문서 본문에 **"초기 설계는 ~였다"** 형태로 정정 이력을
남겼다. 같은 함정을 다시 밟지 않기 위한 것이다.

**리뷰가 정정한 제 오류 중 가장 위험했던 것 3개**

1. **`/* dbmon: */` 주석으로 자기 쿼리를 식별**하려 했다. MySQL 다이제스트는 주석을 제거하므로
   작동하지 않는다. 그리고 자기 관측 제외가 실패하면 1초 주기 쿼리가 상위 N 후보·`_other`·
   커버리지·계정 롤업을 전부 오염시킨다. ADR-005에서 고친 뒤에도 수집기 문서와 런북에
   그대로 남아 있었다.
2. **ASG desired=2인데 샤드 리스에 리더 게이트가 없어**, ADR-018이 "펜싱 토큰 없이는 절대
   하지 말라"고 한 다중 active 상태가 1단계 기본값으로 발생했다. `/readyz` 503은 ALB
   라우팅만 막고 수집 소유권과 무관하다.
3. **`in_flight` 선행 저장이 마스킹보다 앞**에 있어, `masked` 정책 인스턴스에서도 원문이
   DynamoDB에 들어갔다. 고아 레코드는 마스킹되지 않은 채 35일 남고 그 사이 아카이브로 간다.

## 구현 착수 전 마지막 결정

**`instance_id`에 계정 ID를 포함할 것인가** ([OPEN-Q-20](docs/OPEN-QUESTIONS.md)).
이 값은 모든 파티션 키에 들어가므로, 나중에 멀티 계정으로 확장하려면 **키 포맷 변경 + 전
데이터 마이그레이션**이 필요하다(`account_id` 파라미터를 추가하는 문제가 아니다).
지금 `<account>/<region>/<identifier>`로 정하면 비용은 "키가 13자 길어진다"뿐이고 나중 비용이
0이 된다. **M0-2a 태스크 이후에는 바꿀 수 없다.**
