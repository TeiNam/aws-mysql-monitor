# 02. 시스템 아키텍처

## 1. 전체 구조

```
                                  ┌──────────────────────────────┐
   브라우저 (SPA)                  │  Cognito User Pool           │
   React 19 + TS + Vite  ◄────────►│  - local users (ID/PW)       │
        │  ▲                       │  - SAML / OIDC IdP           │
   HTTPS│  │ WebSocket             └──────────────────────────────┘
        ▼  │
   ┌────────────────────── ALB (HTTPS, ACM) ──────────────────────┐
   └───────────────────────────────┬──────────────────────────────┘
                                   │
   ┌───────────────────────────────▼──────────────────────────────┐
   │  EC2 Auto Scaling Group  —  dbmon 단일 Rust 바이너리          │
   │                                                              │
   │  ┌────────────┐  ┌───────────┐  ┌──────────┐  ┌───────────┐  │
   │  │ api        │  │ collector │  │ scheduler│  │ evaluator │  │
   │  │ (axum)     │  │ (tokio)   │  │ (cron)   │  │ (alert)   │  │
   │  └──────┬─────┘  └─────┬─────┘  └────┬─────┘  └─────┬─────┘  │
   │         │              │              │              │       │
   │  ┌──────▼──────────────▼──────────────▼──────────────▼─────┐  │
   │  │  core: domain / ports  +  normalize / planparse (순수)   │  │
   │  └──────┬──────────────┬──────────────┬──────────────┬─────┘  │
   └─────────┼──────────────┼──────────────┼──────────────┼────────┘
             │              │              │              │
   ┌─────────▼──┐  ┌────────▼───────┐  ┌───▼────────┐  ┌──▼────────┐
   │ DynamoDB   │  │ 대상 RDS/Aurora │  │ AWS 제어면 │  │ 외부 채널  │
   │ 단일 진실  │  │  MySQL 8.4+     │  │ RDS/CW/    │  │ Slack /   │
   │ 원천(31일) │  │  Aurora 8.0.32+ │  │ Secrets/   │  │ Telegram  │
   │ PITR 활성  │  │  (읽기 전용)     │  │ Bedrock/   │  └───────────┘
   └─────┬──────┘  └─────────────────┘  │ Athena/    │
         │                              │ Cognito    │
   일 1회 증분 내보내기                  └────────────┘
         │
   ┌─────▼──────────┐   Athena MERGE INTO   ┌──────────────────────┐
   │ S3 raw (DDB    │──────────────────────►│ S3 Tables (Iceberg)  │
   │ JSON, 7일 만료)│                       │ 1년 보관             │
   └────────────────┘                       └──────┬───────────────┘
                                                   │ Athena
                                    31일 초과 조회 / 리포트 집계
```

**아카이브는 파생물이다.** DynamoDB가 단일 진실 원천이고, Iceberg는 그것에서 일 1회 파생된다.
이중 쓰기가 없으므로 두 저장소가 갈라질 구조적 여지가 없고, 정합성 검증 배치도 필요 없다.
→ [ADR-004](03-decisions.md)

## 2. 프로세스 구성

단일 바이너리, 역할은 기동 플래그로 선택한다. 개발·소규모에서는 한 프로세스가 전부 담당하고,
규모가 커지면 역할별로 분리 배포한다.

```
dbmon serve            # api + collector + scheduler + evaluator (기본, 소규모)
dbmon serve --role api          # API/WS만
dbmon serve --role collector    # 수집만 (샤딩 대상)
dbmon serve --role control      # scheduler + evaluator (단일 리더)
dbmon bootstrap --instance <id> # CLI 부트스트랩
dbmon backfill-cwlog --from ... # CLI 백필
dbmon report --month 2026-07    # CLI 리포트 생성
```

**왜 단일 바이너리인가**: 배포 단위가 하나면 버전 스큐가 없고, 도메인 타입을 프로세스 경계로
직렬화할 필요가 없다. 역할 분리는 필요할 때 플래그로 하면 된다. → [ADR-002](03-decisions.md)

## 3. 크레이트 구조 (Cargo workspace)

```
crates/
  normalize/     # SQL 정규화 + app_digest + 마스킹. 순수 함수. 의존성 0
  planparse/     # EXPLAIN JSON 파싱, 플랜 정규화(리터럴 마스킹), 지문, 참조 테이블. 의존성 0
  core/          # 도메인 타입 + 포트(trait). serde/chrono만
    domain/      #   SlowQuery, Digest, Instance, Plan, AlertRule, Advice ...
    ports/       #   SlowQueryStore, DigestStore, InstanceRegistry, MetricSource,
                 #   TargetDb, Notifier, LlmAdvisor, ArchiveQuery, SecretSource, Clock
  dbmon/         # 나머지 전부 (bin). 내부는 평범한 Rust 모듈
    aws/         #   DynamoDB(+PITR export), S3, RDS, CloudWatch, Secrets, Athena,
                 #   Bedrock, Cognito, STS, ECS
    mysql/       #   TargetDb 구현 (mysql_async). 엔진/버전별 SQL 격리
    collector/   #   수집 루프, 리스, 백오프, 버퍼
    api/         #   axum 라우터, 인증, DTO, OpenAPI 생성
    alerting/    #   규칙 평가, 상태 머신, 채널 어댑터
    advisor/     #   컨텍스트 빌더, 프롬프트, 응답 검증
    reporting/   #   Athena 쿼리, 집계, 리포트 렌더
    scheduler/   #   cron 잡, 리더 리스
    main.rs      #   플래그 파싱, 조립(wiring), 그레이스풀 셧다운, healthcheck 서브커맨드
web/             # React SPA
infra/           # Terraform
docs/            # 이 문서들
```

**크레이트는 4개로 시작한다** ([ADR-002](03-decisions.md)).
프로덕션 구현이 하나뿐인 경계에 trait·DTO·변환·mock을 만들면 작은 팀이 수집 로직보다
구조 유지에 시간을 더 쓴다.

| 크레이트 | 분리 이유 |
|---|---|
| `normalize` | 순수 함수. 골든 코퍼스 프로퍼티 테스트가 여기에만 필요. 의존성 0 |
| `planparse` | 순수 함수. fuzz 대상. 의존성 0 |
| `core` | 도메인 타입 + 포트. **테스트용 페이크가 "두 번째 구현"이므로 trait이 정당하다** |
| `dbmon` | 나머지 전부. 파일당 400줄 상한(NFR-M-01)은 모듈로도 지킬 수 있다 |

**의존 방향은 안쪽으로만**: `core`는 아무것도 모른다. `dbmon::aws`/`dbmon::mysql`이 `core`의
trait를 구현한다. 구현체 주입은 `main.rs`에서 1회.
→ 테스트에서 AWS·MySQL 없이 도메인 로직을 전부 검증할 수 있다. [15-testing.md](15-testing.md)

`dbmon`이 커지면(파일 100개 이상, 또는 컴파일 시간이 개발 속도를 해치면) 그때 크레이트로
승격한다. 독립 배포가 필요해지는 것도 승격 조건이다.

## 4. 데이터 흐름

### 4.1 인스턴스 탐색 (5분 주기)

```
scheduler
  → RDS DescribeDBInstances / DescribeDBClusters (리전별 병렬)
  → 엔진 필터(mysql, aurora-mysql)
  → 태그 정규화 → env 분류 (prd/stg/dev/unknown)
  → UI 오버라이드 병합 (오버라이드 우선)
  → DynamoDB config 테이블 upsert (사라진 것은 deleted_at 마킹)
  → 자가진단 큐에 추가 (performance_schema, IAM auth, 슬로우로그 설정 확인)
  → 수집 대상 변경분을 collector에 통지 (watch 채널)
```

### 4.2 슬로우 쿼리 캡처 (1초 주기, 인스턴스별 독립 태스크)

```
tick(1s)
  ├─ Q1: performance_schema.processlist 조회 (경량)
  │       └─ TIME >= threshold 인 행 추출 → candidates
  │
  ├─ candidates 가 비어있으면 여기서 끝 (정상 상태의 99%)
  │
  └─ candidates 각각에 대해 (병렬, 최대 동시 N개)
       ├─ Q2: information_schema.PROCESSLIST WHERE ID = ?   → 전문 SQL
       ├─ Q3: events_statements_current  (THREAD_ID 조인)   → DIGEST, rows_*, flags
       └─ Q4: EXPLAIN FORMAT=JSON FOR CONNECTION <id>       → 실행 중 옵티마이저 플랜
                (별도 연결, 3초 타임아웃)
  ↓
  ★ 정규화·마스킹은 선행 저장 전에 한다 (F2)
     normalize(sql) → app_digest, 정책에 따른 마스킹 + 후조건 검증
     planparse(plan) → plan_normalized(리터럴 마스킹), plan_fingerprint, referenced_tables
     literal_policy_at_ms = now  (이 레코드의 확정 정책을 고정)
  ↓
  최초 관측 시: state=in_flight 로 선행 저장 (정책 적용된 형태) + WS 방송
     owner_worker / owner_epoch / last_seen_at_ms 를 함께 기록 (F4 고아 정리용)
  ↓
  in-flight 캐시 갱신 (thread_id → max_time, first_seen, 수집물)
  5초마다 WS 방송 + last_seen_at_ms 갱신
  ↓
  다음 tick 에서 사라진 thread_id → 확정(finalize)
  ↓
  DynamoDB UpdateItem (state=finalized. 속성별 병합 우선순위 적용 — F5)
  ↓
  WebSocket 브로드캐스트 (state=finalized. sql_preview 는 항상 정규화 텍스트)
  ↓
  alert evaluator 에 이벤트 전달 → 조건부 상태 전이 → 발송 의도 큐
```

**설계 포인트**: 정상 상태에서는 tick마다 쿼리 1건(Q1)만 나간다. 비싼 조회(Q2~Q4)는 실제로
느린 쿼리가 있을 때만 발생한다. 이게 "대상 DB에 부하를 주지 않는다"의 근거다.

### 4.3 다이제스트 스냅샷 (60초 주기 계산 / 1시간 주기 영속화)

```
tick(60s)
  → ① SELECT 지표 컬럼만 FROM events_statements_summary_by_digest
        WHERE LAST_SEEN >= <이전 스냅샷의 DB 시각 − 5s>     ← 활성 다이제스트만
     ② 캐시에 없는 DIGEST 가 있으면만: DIGEST_TEXT, QUERY_SAMPLE_TEXT 조회
     (필터·컬럼 분리가 없으면 응답이 100배 커진다 → 05 §2.5.3)
  → 이전 스냅샷과 델타 계산
      · COUNT_STAR 감소 → 리셋 감지 → 델타 0 처리 + 리셋 이벤트 기록
      · 신규 DIGEST → 신규 등장 이벤트
      · 사라진 DIGEST → 다이제스트 테이블 오버플로 가능성 경고
  → normalize(DIGEST_TEXT) → app_digest  (실시간 캡처와 같은 함수)
  → 메모리 누산기에 가산: (instance, app_digest, hour) → 누적 델타
  → 메모리 링버퍼에 1분 해상도 보관 (최근 60분, 라이브 화면용)

정시 경계(hour rollover, 인스턴스별 0~60초 지터)
  → 임계값(총 100ms) 으로 후보를 먼저 걸러낸 뒤 그중 상위 N(기본 200)만 선별   ← hard cap
  → 나머지는 app_digest="_other" 1행으로 합산
     (_other = 전체 델타 − 저장분. 임계값 미만도 포함 — 04 §2.3 F6)
  → DynamoDB: (instance, hour) 파티션에 배치 쓰기
  → digest_text 사전에 신규 텍스트만 1회 upsert
  → 성공한 hour 만 누산기에서 제거

플러시 실패 처리 (F20) — 누산기를 hour 별 미완료 큐로 모델링한다
  ├ 배치 쓰기 부분 실패(UnprocessedItems) → 남은 항목만 지수 백오프 재시도 (최대 5회)
  ├ 5회 실패 → 그 hour 를 큐에 남기고 다음 정시에 함께 재시도
  │            (hour 가 키에 있으므로 두 시간분이 섞이지 않는다)
  ├ 큐에 3시간 이상 미완료가 쌓이면 → partial=true + flush_failed=true 로 강제 기록
  │            (총량이 조용히 사라지는 것보다 부분 데이터가 낫다)
  └ 메모리 상한 초과 → 오래된 hour 부터 드롭 + DroppedRecords{reason=accumulator_full}
```

**왜 시간 롤업인가**: 1분 해상도를 그대로 적재하면 500대 × 1440분 × 활성 다이제스트 300개 ≈
**일 2억 행**이다. DynamoDB 쓰기 비용만 하루 수백 달러가 되고 Athena 스캔량도 감당되지 않는다.
시간 롤업 + 상위 200개 제한이면 **일 241만 행 ≈ 720MB**로 떨어진다. 롱테일은 `_other`로 합산해
"총 실행시간 중 관측된 비율"을 항상 알 수 있게 한다. → [ADR-010](03-decisions.md)

1분 해상도가 필요한 건 "지금 무슨 쿼리가 몰리는가"를 보는 라이브 화면뿐이고, 그건 메모리
링버퍼로 충분하다.

### 4.4 실시간 지표 (5초 주기)

```
tick(5s) → SELECT VARIABLE_NAME, VARIABLE_VALUE FROM performance_schema.global_status
        → 이전 값과 델타 → 초당 변화율 산출
        → 메모리 링버퍼(인스턴스당 최근 60분)에만 보관
        → WebSocket push (구독자에게)
```

**메모리에만 두는 이유**: 500대 × 5초 × 20지표 = 초당 2000포인트. 이걸 영속화할 이유가 없다.
실시간 패널은 최근 60분만 보면 되고, 장기 추세는 CloudWatch가 담당한다.
→ 시계열 DB를 도입하지 않는다. [ADR-009](03-decisions.md)

### 4.5 아카이브 (일 1회, 스케줄러 리더)

```
02:00 KST
  ├─ 체크포인트 조회: CKPT / archive#<table>  (테이블별로 분리 — F11)
  │    가장 오래된 체크포인트를 기준으로 내보내기 구간을 정한다
  │    구간이 24시간을 넘으면 24시간 미만 청크로 분할 (C-16)
  ├─ ExportTableToPointInTime(
  │      ExportType=INCREMENTAL_EXPORT,
  │      IncrementalExportSpecification={ ExportFromTime, ExportToTime },
  │      S3Bucket=raw, S3Prefix=exports/<table>/<date>/)
  ├─ 상태 폴링 → COMPLETED
  ├─ Athena: MERGE INTO s3tables.dbmon.<table> AS t
  │            USING (SELECT ... FROM raw_export_view WHERE ...) AS s
  │            ON t.record_id = s.record_id
  │            WHEN MATCHED THEN UPDATE ...
  │            WHEN NOT MATCHED THEN INSERT ...
  ├─ 테이블별로 MERGE 실행 → 성공한 테이블만 체크포인트 전진
  └─ 실패한 테이블은 체크포인트 유지 → 다음 실행이 그 테이블만 재시도
```

**체크포인트를 테이블별로 분리하는 이유 (F11)** — 초기 설계는 잡 전체에 체크포인트가
하나였다. 그러면 테이블 9개 중 하나가 스키마 불일치로 계속 실패할 때 **나머지 8개도 영구히
전진하지 못한다.** 멱등이라 데이터가 깨지지는 않지만 진행이 막힌다.

그리고 그 상태가 지속되면 체크포인트가 **PITR 창(35일)을 벗어난다.**
그 순간 [14 §8.3](14-infrastructure.md)의 "체크포인트를 창 안으로 조정"은
**곧 영구 데이터 손실**이다(창 밖 구간은 내보낼 방법이 없고 DynamoDB TTL로도 사라졌다).

→ 별도 알람을 둔다: **"가장 오래된 아카이브 체크포인트가 PITR 창의 25일에 도달"**
(35일 창에서 10일 여유). 3회 실패 알림만으로는 이 진행 차단을 표현하지 못한다.

- **멱등**: `MERGE INTO`가 `record_id` 기준으로 병합하므로 같은 구간을 두 번 적재해도 행수가
  변하지 않는다. 실패 후 재시도가 안전하다.
- **삭제 반영**: 증분 내보내기에는 TTL 삭제도 delete 마커로 포함된다. 아카이브는 삭제를
  적용하지 않는다(TTL 만료는 핫 티어 정책일 뿐 아카이브에서 지울 이유가 없다).
- S3 raw는 7일 Lifecycle로 만료시킨다.
- 1년 초과분은 월 1회 `DELETE FROM ... WHERE event_date < date_add('year', -1, current_date)`.

### 4.6 조회 라우팅

```
GET /api/slow-queries?from=..&to=..
  ↓
QueryRouter (core)
  from >= now-31d                  → DynamoDB 단독
  to   <  now-31d                  → Athena 단독 (비동기)
  걸쳐 있음                        → DynamoDB + Athena 병합
                                     (경계 중복은 record_id 로 dedup)
  ↓
Athena 경로: Athena QueryExecutionId 를 그대로 반환 → 클라이언트 HTTP 폴링
              → 결과 캐시는 **워크그룹 result reuse** ([ADR-015](03-decisions.md))
              자체 잡 테이블·자체 캐시·WS 완료 통지를 만들지 않는다
```

### 4.7 AI 어드바이저

```
POST /api/digests/{app_digest}/advice
  ↓
캐시 확인: (app_digest, instance_id, schema_fingerprint, engine_version,
            plan_fingerprint, prompt_version)   ← ADR-016
  hit  → 반환
  miss ↓
ContextBuilder
  ├─ digest_text + 집계 지표 (DynamoDB/Athena)
  ├─ 대표 플랜 JSON (가장 최근 in-flight 플랜)
  ├─ 참조 테이블 = planparse::referenced_tables(plan)
  ├─ 대상 DB 조회 (읽기 전용):
  │    SHOW CREATE TABLE / information_schema.STATISTICS(카디널리티)
  │    / information_schema.TABLES(행수·크기) / COLUMN_STATISTICS(히스토그램, 옵션)
  └─ 리터럴 정책 확인 → 옵트인 인스턴스만 샘플 리터럴 포함
  ↓
Bedrock Converse (Guardrail 적용, tool-use 로 출력 스키마 강제)
  ↓
스키마 검증 → 실패 시 1회 재시도 → 저장(DynamoDB + Iceberg) → 반환
```

### 4.8 알림 평가

```
이벤트 소스 (슬로우쿼리 확정 / 다이제스트 델타 / 메트릭 폴 / 복제 상태 / 락 대기 / 수집 상태)
  ↓
RuleEvaluator (스코프 매칭 → 조건 평가 → 지속시간 확인)
  ↓
AlertState 머신: pending → firing → resolved
  · 지문 = hash(rule_id, scope_key)
  · 음소거/점검창 확인
  · 재통지 간격 확인
  ↓
채널 팬아웃 (Slack / Telegram / 인앱) — 재시도 3회 지수 백오프
  ↓
DynamoDB 알림 이력 + WebSocket push
```

## 5. 컴포넌트 책임 경계

| 컴포넌트 | 하는 일 | 하지 않는 일 |
|---|---|---|
| `core` | 도메인 타입, 포트 정의, 조회 라우팅 규칙, 보관 정책 계산 | I/O 일체 |
| `normalize` | SQL → 정규화 텍스트 → `app_digest` | DB 접근, 저장 |
| `planparse` | EXPLAIN JSON 파싱, 플랜 지문, 참조 테이블 추출, 비용 노드 트리 | EXPLAIN 실행 |
| `dbmon::mysql` | 대상 DB 연결·풀·버전 분기 SQL 실행 | 도메인 판단, 저장, **IAM 토큰 발급**(→ `AuthTokenProvider` 포트로 분리) |
| `dbmon::aws` | AWS SDK 호출, 직렬화, 재시도, 압축/오프로드, IAM 토큰 발급 | 도메인 판단 |
| `dbmon::collector` | 폴링 루프, 샤드 리스, 백오프, in-flight 상태 추적, 버퍼 | SQL 문장 작성(`dbmon::mysql`), 저장 형식(`dbmon::aws`) |
| `dbmon::api` | HTTP/WS, 인증·인가, DTO 변환, 입력 검증, OpenAPI | 도메인 규칙 |
| `dbmon::alerting` | 규칙 평가, 상태 머신, 억제 | 채널 프로토콜 세부(어댑터로 분리) |
| `dbmon::advisor` | 컨텍스트 조립, 프롬프트, 응답 검증, 캐시 키 | Bedrock SDK 호출(`dbmon::aws`) |
| `dbmon::reporting` | 집계 쿼리 정의, 리포트 조립, 렌더 | Athena 실행(`dbmon::aws`) |
| `dbmon::scheduler` | cron 정의, 리더 선출, 잡 실행·재시도 | 잡의 내용 |

**이름은 [§3](#3-크레이트-구조-cargo-workspace)의 모듈 경로와 일치시킨다 (F31).**
초기 설계는 이 표에 `mysqlsrc`·`awsinfra` 같은 (존재하지 않는) 크레이트 이름을 써서
"무엇이 무엇을 몰라야 하는가"를 코드에 대응시킬 수 없었다.

**두 가지 경계 위반을 정정했다.**
1. `dbmon::mysql`이 "IAM 토큰 발급 연동"을 한다고 적어 **MySQL 어댑터가 AWS SDK를 알아야
   한다고 스스로 인정**하고 있었다(F19의 원인 중 하나).
   → `core::ports::AuthTokenProvider` 포트로 분리한다. `dbmon::mysql`은 "비밀번호 문자열을
   주는 무언가"만 알고, 그게 IAM 토큰인지 Secrets Manager 값인지 모른다.
2. `core::ports::ArchiveQuery`가 Athena 실행 모델(`execution_id`, `next_token`,
   `FOR VERSION AS OF`)을 그대로 노출해 "core는 아무것도 모른다"가 성립하지 않았다.
   → 도메인 언어로 정의한다:
   ```rust
   pub trait ArchiveQuery {
       async fn start(&self, q: ArchiveQuerySpec, at: Option<AsOf>) -> QueryHandle;
       async fn status(&self, h: &QueryHandle) -> QueryStatus;
       async fn page(&self, h: &QueryHandle, cur: Option<Cursor>) -> (Rows, Option<Cursor>);
       async fn cancel(&self, h: &QueryHandle);
   }
   ```
   `QueryHandle`·`Cursor`·`AsOf`는 불투명 타입이고, Athena의 `QueryExecutionId`·`NextToken`·
   스냅샷 ID는 `dbmon::aws` 안에 갇힌다.

## 6. 상태와 리더십

| 상태 | 위치 | 이유 |
|---|---|---|
| 인스턴스 in-flight 캐시 | 워커 메모리 | 초당 갱신. **선행 저장 이후로는 유실 범위가 다르다** — 관측된 후보는 이미 저장돼 있고, 유실되면 그 항목들이 고아로 남아 스케줄러 리더의 정리(F4)를 기다린다 |
| 실시간 지표 링버퍼 | 워커 메모리 | 대량·단기. 유실 허용 |
| 다이제스트 이전 스냅샷 + `LAST_SEEN` 체크포인트 | 워커 메모리 | 재시작 시 첫 델타 1회 스킵(기준선 재수립). 영속화 불필요 |
| `DIGEST → app_digest` 캐시 | 워커 메모리 | 2차 텍스트 쿼리를 생략하기 위한 캐시. 유실 시 재조회 |
| 알림 상태 (`AlertState`) | DynamoDB 조건부 쓰기 | 여러 워커가 동시 평가해도 전이는 1회 ([10 §2.0](10-alerting.md)) |
| 발송 의도 큐 | DynamoDB (`NOTIFY#`) | 평가(collector)와 발송(control) 분리 |
| 다이제스트 시간 누산기 | 워커 메모리 | 정시에 플러시. 재시작 시 해당 시간의 부분 데이터 유실 → `partial=true` 플래그로 표시 |
| 아카이브 체크포인트 | DynamoDB (`dbmon-config`, 테이블별) | 잡 재시도 구간 결정 (F11) |
| **인스턴스 수집 상태** (현재 폴링 주기, 서킷 상태, 연속 실패 수) | **DynamoDB (`dbmon-config`)** | 샤드 인수 시 **계승**해야 한다 (F24) |
| 시계 오프셋 (`clock_offset_ms`) | 워커 메모리 + DynamoDB 백업 | 인수 시 계승. 재추정에 몇 tick 걸림 |
| 부트스트랩 상태 | DynamoDB (`dbmon-config`) | 부분 실패 추적 (F23) |
| AI 토큰 사용량 | DynamoDB (`ADD` 원자 증가) | 동시 실행 예산 초과 방지 (F30) |
| 샤드 리스 | DynamoDB (`dbmon-config`) | 워커 간 조정 |
| 스케줄러 리더 | DynamoDB 조건부 쓰기 리스(30초 TTL) | cron 중복 실행 방지 |
| 설정 | DynamoDB | 재시작 없이 반영, 워커 간 공유 |
| WebSocket 구독 | 워커 메모리 | ALB 스티키 세션. 워커 재시작 시 클라이언트 재연결 |

**WS 팬아웃** — 1단계에서는 active 노드가 1대이므로 문제가 없다(클라이언트가 항상 active에
붙고, standby는 `/readyz` 503으로 ALB에서 빠져 있다). 2단계에서 API·collector를 분리하면
워커 간 이벤트 전파가 필요해진다. → [OPEN-Q-04](OPEN-QUESTIONS.md)

## 7. 격리 전략

- **인스턴스별 태스크 격리**: 인스턴스 1대 = tokio 태스크 1개(+ 플랜 수집용 서브태스크). 한 대의
  패닉이 다른 대에 전파되지 않도록 `JoinHandle` 감시 + 재시작.
- **연결 풀 격리**: 인스턴스별 **풀 2개** — hot(탐지·심층 조회, 2~4) + bulk(다이제스트 스냅샷·일일 집계, 1~2). 최대 6이다 (F7, [05 §5](05-collector.md)). 자기 계측이 대상 DB `max_connections` 대비 점유율을 보고할 때 이 값을 쓴다.
- **서킷 브레이커**: 연속 실패 N회 → open(폴링 중단) → 지수 백오프로 half-open 재시도.
- **타임아웃 계층**: 연결 5초 / 쿼리 3초 / 플랜 3초 / tick 전체 예산 = 폴링 주기의 80%.
- **외부 의존성 격리**: Bedrock·Slack·Athena 호출은 별도 태스크 + 큐. 실패해도 수집 루프 무영향.

## 8. 배포 토폴로지

### 1단계 (인스턴스 ~150대)
```
ECS Fargate 서비스 (desiredCount=2, ARM64 1 vCPU / 2 GB)
  → 1 태스크 active (리더 리스 보유, 수집·API 담당)
  → 1 태스크 standby (/readyz 503 → 대상 그룹에서 제외)
ALB → 대상 그룹 (스티키 세션 불필요, WS 지원, 헬스체크 /readyz)

컨테이너 헬스체크는 `/healthz`(무조건 200), 대상 그룹 헬스체크는 `/readyz` 다.
**둘을 바꾸면 standby 가 계속 재시작된다** — standby 는 정상적으로 503 을 낸다.
```

**active 1대 원칙** ([ADR-018](03-decisions.md)) — 리스만으로는 split-brain을 막지 못한다
(펜싱 토큰이 없다). 그래서 1단계에서는 리더 1대만 수집하고, 나머지는 standby로 둔다.
이렇게 하면 WS 팬아웃 문제도 함께 사라진다(클라이언트가 항상 active에 붙는다).

### 2단계 (~500대) — **선행 조건 2개**
```
service-api        1 vCPU / 2 GB  × 2  (--role api, ALB 대상)
service-collector  4 vCPU / 8 GB  × 4  (--role collector, 리스 샤딩, ALB 미연결)
service-control    0.5 vCPU / 1 GB × 1 (--role control, 리더 리스)
```

이 단계로 가려면 **먼저 해결해야 하는 것**이 둘이다.
1. **펜싱 토큰** ([ADR-018](03-decisions.md)) — 다중 active collector가 안전해야 한다.
   없으면 다이제스트 누산기가 last-writer-wins로 손상된다.
2. **워커 간 실시간 이벤트 팬아웃** ([OPEN-Q-04](OPEN-QUESTIONS.md)) — API 노드와
   collector 노드가 분리되면 실시간 지표·슬로우 쿼리 이벤트를 전달할 경로가 필요하다.

**M1-9 벤치마크에서 워커 1대가 500대를 커버하지 못하면 이 둘이 M12가 아니라 선행 조건이 된다.**

리전은 앱 배포 리전 1곳. 대상 RDS는 멀티 리전을 크로스 리전 연결로 수집한다.
크로스 리전 MySQL 연결 지연(수십~수백 ms)이 1초 폴링에 영향을 주므로, 지연이 큰 리전은
폴링 주기를 늘리거나 해당 리전에 collector를 추가 배치한다. → [OPEN-Q-02](OPEN-QUESTIONS.md)

## 9. 실패 모드와 대응

| 실패 | 감지 | 대응 | 사용자 영향 |
|---|---|---|---|
| 대상 DB 연결 불가 | 연결 타임아웃 | 서킷 오픈, 백오프, 인스턴스 상태 `unreachable` | 해당 인스턴스만 수집 중단, UI에 표시 |
| 대상 DB 응답 지연 | 쿼리 타임아웃 | 폴링 주기 2배씩 증가(최대 30초), 회복 시 원복 | 탐지 해상도 저하 |
| `performance_schema` OFF | 자가진단 | 수집 비활성 + 해결 가이드 표시 | 해당 인스턴스 미수집 |
| in-flight 플랜 실패 | EXPLAIN 에러 | 폴백(재실행) 또는 플랜 없이 저장, 사유 기록 | 플랜 없음 표시 |
| DynamoDB 스로틀 | SDK 에러 | 지수 백오프 + 버퍼. 버퍼 초과 시 드롭 + 메트릭 | 일부 유실(관측 가능) |
| 아카이브 잡 실패 | 잡 상태 + `JobHeartbeat` 결측 알람 | 체크포인트 미갱신 → 다음 실행이 누락 구간을 **24시간 미만 청크로 쪼개** 재시도. 3회 연속 실패 시 알림 | 과거(31일 초과) 조회에 최대 며칠 공백. TTL 35일이라 복구 여유 있음 |
| cron 잡 침묵 (리더가 멈춤) | `JobHeartbeat` 결측 (`TreatMissingData=breaching`) | 리더 리스 TTL 만료 → 다른 워커 승계 | 잡 최대 1주기 지연 |
| standby 승격 실패 | `/readyz` 503 지속 + ALB 비정상 대상 알람 | ECS 가 태스크를 교체 | 최대 2분 수집·조회 중단 |
| Athena MERGE 충돌 | 커밋 실패 | 재시도(Iceberg 낙관적 커밋). 아카이브 잡은 단일 리더만 실행하므로 경합 자체가 드묾 | 없음 |
| Athena 실패/느림 | 쿼리 상태 | 사용자에게 실행 상태·경과 시간 표시, 취소 가능 | 과거 조회 지연 |
| Bedrock 스로틀/에러 | SDK 에러 | 재시도 후 실패 반환. 캐시된 이전 결과 있으면 함께 표시 | 어드바이저만 실패 |
| 워커 다운 | 리스 TTL 만료 | 리스 TTL 60초 + 스캔 20초 = 최대 80초 내 인수 ([05 §7](05-collector.md)) | 최대 80초 수집 공백. 명시적 반납 시 20초 |
| 스케줄러 리더 다운 | 리스 TTL 만료 | 다른 워커가 리더 승계, 미실행 잡은 다음 tick에 실행 | 잡 최대 1주기 지연 |
| Cognito 장애 | 토큰 검증 실패 | 기존 유효 토큰은 JWKS 캐시로 계속 동작. 신규 로그인만 실패 | 로그인 불가 |
| **시계 오차** (EC2 ↔ 대상 DB) | `SELECT NOW(6)` 대비 오프셋 추정 ([05 §4.2](05-collector.md)) | 1초 초과 시 병합 창 자동 확대 + 자가진단 경고. 5초 초과 시 알림. 시각 계산에 오프셋 보정 | 보정 전이라면 레코드 이중화·파티션 어긋남 |
| **`performance_schema` consumer 런타임 비활성** | `digest` 쿼리가 0행을 N회 연속 반환 ([05 §9.1](05-collector.md)) | 즉시 `setup_consumers` 재확인 → 비활성이면 인스턴스 상태 `degraded` + 알림 | 조용한 0데이터 (가장 나쁜 실패 모드) |
| **KMS 접근 거부** | `AccessDeniedException` (재시도 무의미) | 즉시 서킷 오픈 + **버퍼 유지(드롭 금지)** + `/readyz` 503 + 즉시 알림 | 전면 중단이지만 데이터 유실 없음 |
| **Iceberg 스키마 ↔ 항목 구조 불일치** (배포 중) | 아카이브 잡 시작 시 `schema_version` 최대값 vs Iceberg 컬럼 집합 비교 | 조기 실패 + 알림. 그 회차 건너뜀 | 아카이브 1일 지연 |

## 10. 확장 지점 (지금 만들지 않고 자리만 비워둠)

| 확장 | 비워둔 자리 |
|---|---|
| PostgreSQL 지원 | `core::ports::TargetDb` trait만으로는 **부족하다.** 도메인 재모델링 수준 — 아래 참조 |
| 멀티 계정 | **키 포맷은 이미 준비됐다** (M0-2a): `instance_id = <account>/<region>/<identifier>` 이므로 마이그레이션이 필요 없다. 남은 것은 IAM `dbuser:*/dbmon` 리소스, 크로스 계정 로그 그룹 접근, KMS 키 정책, 그리고 계정별 자격증명 취득(STS AssumeRole)이다 |
| 다른 LLM | `core::ports::LlmAdvisor` trait |
| 다른 알림 채널 | `core::ports::Notifier` trait + 어댑터 |
| ~~ECS 배포~~ | **현재 배포 방식이다** ([ADR-022](03-decisions.md)). EKS 로 가려면 리스·헬스체크는 그대로 쓰고 Deployment + Service 로 바꾸면 된다 |

**멀티 계정 완화책은 적용됐다 (F18, M0-2a, 2026-08-19).**
`instance_id` 를 `<account>/<region>/<identifier>` 로 정했고 단일 계정에서도 계정 ID를 넣는다.
키가 ≈45자로 길어지는 것 외에 비용이 없고, 멀티 계정으로 갈 때
**키 포맷 변경과 전 데이터 마이그레이션이 불필요**하다.
`InstanceId::parse` 가 2성분 형식을 거부하므로 옛 형식이 섞여 들어올 수 없다.

**PostgreSQL 지원도 trait 하나로 되지 않는다 (F19).** 도메인 타입과 규약이 MySQL 전제로 굳어
있다: `mysql_digest`, `thread_id`, `plan_format_version=json_v1`, `record_id`의 `thread_id`
성분, `TIME` 초 해상도 기반 시작 시각 추정, `LAST_SEEN` 델타 모델, 자가진단 13개 항목,
`EXPLAIN ... FOR CONNECTION`(PostgreSQL에 등가물 없음).
필요한 변경: 도메인 식별자 중립화(`server_digest`, `session_id`), 플랜 포맷 추상화,
진단 항목 엔진별 분리, in-flight 플랜 부재 시 폴백 정의. **도메인 재모델링 수준이다.**
확장 지점 표가 난이도를 낮게 표현하지 않도록 이 문단을 남긴다.

이 trait들은 **구현이 2개 이상 필요해질 때** 실제 다형성을 쓴다. 지금은 구현 1개 + 테스트용
페이크 1개라 trait 존재가 정당하다(테스트가 두 번째 구현이다).
