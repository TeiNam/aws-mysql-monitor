# 15. 테스트 전략

## 1. 원칙

1. **도메인 로직은 AWS·MySQL 없이 전부 테스트한다.** `core`의 포트를 페이크로 구현한다.
   페이크가 "두 번째 구현"이므로 trait 존재가 정당해진다([02 §10](02-architecture.md)).
2. **관측 도구의 정확성은 골든 데이터로 검증한다.** 정규화·다이제스트·플랜 파싱·슬로우로그
   파싱은 실제 MySQL 출력을 픽스처로 커밋하고 그것에 맞춘다. 우리 기대가 아니라 실제 동작이
   기준이다.
3. **데이터 경계는 골든 페이로드로 지킨다.** Bedrock 요청 본문, 로그 출력, API 응답을
   스냅샷으로 두고 변경을 감지한다. 리팩터링으로 조용히 새는 것을 막는다.
4. **부하 특성은 측정으로 확정한다.** "Rust면 500대 된다"는 가정을 벤치마크로 검증한다
   ([OPEN-Q-01](OPEN-QUESTIONS.md)).
5. **테스트를 위해 프로덕션 스위치를 만들지 않는다.** 검증 비활성화 플래그, 인증 우회
   플래그를 만들지 않는다.

## 2. 계층

```
단위 (cargo test)            의존성 없음. 순수 함수 + 페이크 포트. 수초 내 완료
통합 (--features integration) testcontainers: MySQL 8.4, MySQL 8.0.32, DynamoDB Local
통합-실계정 (--features aws-it)  10-foundation·60-seed 가 apply 된 dev 계정 (일 1회 CI)
계약 (OpenAPI)               스펙 ↔ 실제 응답 검증
프론트 단위 (vitest)          유틸, 훅, 컴포넌트
E2E (Playwright)             dev 환경에 배포된 실물
부하 (별도 하네스)             모의 MySQL 엔드포인트 N개
```

## 3. 단위 테스트

### 3.1 `normalize` 크레이트

가장 중요한 단위다. 여기가 틀리면 모든 그룹핑이 틀린다.

```rust
// 프로퍼티 테스트 (proptest)
// P1: 멱등성 — 정규화된 텍스트를 다시 정규화해도 같다
proptest!(|(sql in arb_sql())| {
    let n1 = normalize(&sql);
    assert_eq!(n1, normalize(&n1));
});

// P2: 수렴성 — 골든 코퍼스에서 raw 와 MySQL DIGEST_TEXT 가 같은 app_digest 를 낸다
#[test]
fn digest_converges_with_mysql() {
    for case in load_golden("digest_golden_84.json") {
        assert_eq!(
            app_digest(&normalize(&case.raw_sql)),
            app_digest(&normalize(&case.mysql_digest_text)),
            "불일치: {}", case.raw_sql
        );
    }
}

// P3: 리터럴 불변성 — 리터럴만 바꾼 두 쿼리는 같은 다이제스트
proptest!(|(tmpl in arb_sql_template(), a in arb_literal(), b in arb_literal())| {
    prop_assume!(a != b);
    assert_eq!(app_digest_of(tmpl.with(a)), app_digest_of(tmpl.with(b)));
});

// P4: 구조 민감성 — 구조가 다르면 다이제스트가 다르다
// P5: 결정론적 절단 — 8192자 초과 입력의 다이제스트가 안정적
// P6: 패닉 없음 — 임의 바이트열(유효하지 않은 UTF-8 포함)에 패닉하지 않는다
```

**골든 코퍼스 생성** (`just golden`)

```
1. testcontainers 로 MySQL 8.4 / 8.0.32 기동 (performance_schema=ON)
2. tests/corpus/*.sql 의 문장 300개 실행
     - 단순 SELECT / 다중 JOIN / 서브쿼리 / 상관 서브쿼리
     - CTE / 재귀 CTE / 윈도우 함수
     - IN 절 (2개, 100개, 10000개)
     - 멀티 VALUES INSERT
     - UPDATE / DELETE / REPLACE / INSERT ... ON DUPLICATE KEY
     - 옵티마이저 힌트 /*+ */
     - 일반 주석 /* */, --, #
     - 문자열 이스케이프, 유니코드, 이모지
     - 16진수·비트 리터럴, NULL, TRUE/FALSE
     - LIMIT / OFFSET
     - 매우 긴 쿼리 (2048자, 8192자, 65536자) → 절단 케이스
     - 프로시저 호출 (내부 문장 확인)
     - 대소문자 혼용 키워드, 백틱 유무
3. events_statements_summary_by_digest 에서 DIGEST, DIGEST_TEXT 수집
4. { raw_sql, mysql_digest, mysql_digest_text, truncated, version } 을 JSON 으로 커밋
5. 규칙과 실제가 다르면 규칙([05 §3.2](05-collector.md))을 고친다
```

코퍼스는 커밋되므로 CI에서 MySQL 없이도 수렴성 테스트가 돈다.
`just golden`은 MySQL 버전 추가 시나 규칙 변경 시에만 실행한다.

### 3.2 `planparse` 크레이트

```
픽스처: 실제 EXPLAIN FORMAT=JSON 출력 40개
  - 단일 테이블 / nested_loop / hash join
  - materialized_from_subquery / attached_subqueries
  - union_result / duplicates_removal
  - ordering_operation / grouping_operation
  - <temporary>, <derived2>, <union1,2> 가상 테이블
  - json_v1 / json_v2 (explain_json_format_version)
  - MySQL 8.4 / 8.0.32 각각

검증:
  참조 테이블 추출이 정확한가 (가상 테이블 제외, 스키마 한정자 처리)
  plan_fingerprint 가 구조 변화에만 반응하는가 (rows 값 변화에는 불변)
  비용 상위 노드 순위가 맞는가
  깨진 JSON / 예상 못한 필드에 패닉하지 않는가 (fuzz)
```

### 3.3 `collector` 크레이트 (페이크 포트)

```rust
// 페이크 TargetDb: 스크립트된 processlist 시퀀스를 반환
let db = FakeTargetDb::new()
    .tick(vec![row(id=100, time=1, digest="A")])       // 임계값 미달
    .tick(vec![row(id=100, time=3, digest="A")])       // 탐지
    .tick(vec![row(id=100, time=5, digest="A")])       // 추적
    .tick(vec![]);                                     // 종료 → 확정

// 페이크 클록: 시간을 주입해 백오프·리스·타임아웃을 결정론적으로 테스트
```

검증 항목:
| 항목 | 케이스 |
|---|---|
| in-flight 상태 머신 | 정상 / 즉시 종료 / 1시간 초과 / 폴링 사이 종료 |
| 스레드 ID 재사용 | 같은 ID, 다른 다이제스트 → 별도 레코드 2건 |
| 스레드 ID 재사용 | 같은 ID, TIME 감소 → 별도 레코드 |
| 시작 시각 추정 | TIME 기반 / TIMER_WAIT 기반, `duration_source` 표기 |
| 델타 계산 | 신규 / 정상 / 리셋(COUNT_STAR 감소) / FIRST_SEEN 변화 |
| 누적값 오해 방지 | `MAX_TIMER_WAIT`를 창 내 최대로 쓰지 않는지 |
| `_other` 합산 | 상위 N + `_other` = 전체 |
| 시간 롤업 경계 | 정시 직전·직후 데이터가 올바른 버킷에 들어가는지 |
| `partial` 플래그 | 워커 재시작 시뮬레이션 |
| 백오프 | 실패 3/10/30회 시 주기·서킷 상태 |
| 실패 분류 | 연결/권한/타임아웃/파싱 각각의 처리 차이 |
| 폭주 방어 | 슬로우 쿼리 600건 → LIMIT 500, 심층 조회 50건 |
| 레이트 리밋 | 같은 다이제스트 초당 10건 → 심층 조회 3건 |
| 그레이스풀 셧다운 | 리스 반납 + 누산기 플러시 + 버퍼 플러시 순서 |

### 3.4 `alerting` 크레이트

```
상태 머신 테이블 주도 테스트:
  입력: (조건 true/false 시퀀스, for_duration, resolve_after)
  기대: (상태 전이 시각, 발송 횟수)

케이스:
  즉시 발화 (for_duration=0)
  지속 조건 미달 → 발화 안 함
  플래핑 (true,false,true,false) → resolve_after 로 흡수
  재통지 간격
  음소거 중 상태 전이는 하되 발송 안 함
  점검 창 진입·이탈
  그룹핑 (60초 내 5개 지문)
  의존 억제 (collector 발화 중 다른 규칙 억제)
  spike_ratio: 7일 미만 데이터 → 평가 스킵
```

### 3.5 `advisor` 크레이트

```
DDL 안전성 검증:
  허용 목록 통과: CREATE INDEX, ALTER TABLE ADD INDEX, DROP INDEX, ANALYZE TABLE
  차단: DROP TABLE, TRUNCATE, UPDATE, DELETE, GRANT, SET GLOBAL
  우회 시도 차단:
    "CREATE INDEX i ON t(c); DROP TABLE t"        (다중 문장)
    "CREATE INDEX /* DROP TABLE t */ i ON t(c)"    (주석)
    "create index i on t(c) /*! ; drop table t */" (버전 주석)
    대소문자 혼용, 개행 삽입, 유니코드 동형 문자

식별자 검증:
  존재하지 않는 컬럼 참조 → hallucinated=true
  존재하지 않는 테이블 → hallucinated=true
  존재하는 것만 → 정상

수치 검증:
  입력에 없는 숫자가 서술문에 등장 → unverified_numbers 검출
  백분율 계산 결과는 허용
  날짜·순위는 허용

리터럴 미전송 (골든 페이로드):
  literals_included=false 로 요청 생성 → 페이로드 스냅샷 비교
  스냅샷에 리터럴 패턴(이메일/전화/숫자 리터럴)이 없는지 정규식 검사
  코드 변경으로 페이로드가 바뀌면 테스트 실패 → 의도적 변경이면 스냅샷 갱신
```

## 4. 통합 테스트

### 4.1 testcontainers 구성

```rust
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn capture_slow_query_end_to_end() {
    let mysql = MySql::default()
        .with_version("8.4")
        .with_arg("--performance_schema=ON")
        .with_arg("--slow_query_log=ON")
        .with_arg("--long_query_time=1")
        .start().await;
    let ddb = DynamoDbLocal::default().start().await;

    seed_schema(&mysql).await;           // 인덱스 없는 100만 행 테이블
    let handle = spawn_slow_query(&mysql, Duration::from_secs(4)).await;

    let collector = Collector::new(real_mysql(&mysql), real_dynamo(&ddb));
    collector.run_for(Duration::from_secs(8)).await;

    let records = query_slow_queries(&ddb).await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert!(r.duration_ms >= 3500);
    assert_eq!(r.plan_source, "for_connection");        // in-flight 성공
    assert!(r.sql_text.len() > 1024);                    // 절단 지점이 65,535바이트임 ★
    assert!(r.rows_examined > 900_000);
    assert!(r.no_index_used);
}
```

`assert!(r.sql_text.len() > 1024)`가 ADR-005의 핵심 회귀 테스트다.
1세대의 절단 버그가 다시 들어오면 여기서 잡힌다. 시드 쿼리를 1024바이트보다 길게 만든다.

### 4.2 통합 테스트 항목

| 항목 | 검증 |
|---|---|
| 전문 SQL 확보 | 1024바이트 초과 SQL이 절단되지 않는지 (**ADR-005 회귀**) |
| in-flight 플랜 | SELECT / UPDATE / DELETE 각각 성공하는지 (**ADR-006**) |
| in-flight 실패 분류 | 종료된 스레드 / EXPLAIN 불가 문장 |
| ~~`FORMAT=TREE FOR CONNECTION`~~ | **해소됨** — RDS 에서 `FOR CONNECTION` 자체가 불가하다. `FORMAT=TREE` 는 재실행 경로로만 쓴다 ([19 §B](19-m1-findings.md)) |
| ~~최소 권한으로 플랜 수집~~ | **해소됨** — 재실행 경로는 대상 테이블 `SELECT` 권한이 **필수**다. 권한 모드 C 는 폐기 |
| 다이제스트 스냅샷 | 실행 → 델타가 정확한지 |
| `QUERY_SAMPLE_TEXT` | 수집되는지, 절단 여부 |
| 다이제스트 절단 | 매우 긴 쿼리에서 **4가지 신호**로 감지 (`...` 는 붙지 않는다) + 매핑 학습 |
| 권한 부트스트랩 | `CREATE USER` + `GRANT` 후 필요한 모든 쿼리가 동작하는지 |
| 권한 diff | 일부 권한만 준 상태에서 부족분을 정확히 찾는지 |
| 멱등 부트스트랩 | 2회 실행 후 상태 동일 |
| 복제 상태 | 소스-레플리카 컨테이너 2개로 `SHOW REPLICA STATUS` 파싱 |
| 락 대기 | 의도적 락 경합 생성 후 `sys.innodb_lock_waits` 파싱 |
| 데드락 | 의도적 데드락 생성 후 `SHOW ENGINE INNODB STATUS` 파싱 + 중복 병합 |
| 슬로우로그 파싱 | 컨테이너의 실제 슬로우로그 파일 파싱 |
| 3소스 병합 | 같은 실행이 processlist + 슬로우로그에서 하나로 합쳐지는지 |
| 샤드 리스 | DynamoDB Local로 워커 2개 경합 → 분배·인수·반납 |
| 리더 리스 | 워커 2개 중 하나만 cron 실행 |
| TTL 계산 | 정책 변경이 TTL에 반영 |
| S3 오프로드 | 300KB 초과 플랜의 S3 저장·복원 |
| 버전 하한 | 8.0.31 컨테이너 → `unsupported_version` 판정 |
| 자가진단 | `performance_schema=OFF` 컨테이너 → 정확한 진단 |

### 4.3 아카이브 경로 (실제 AWS — dev 계정)

DynamoDB 증분 내보내기와 S3 Tables는 LocalStack 지원이 불확실하므로
**dev 계정의 실제 리소스**로 테스트한다(별도 CI 잡, 일 1회).
`10-foundation` + `20-data` 레이어가 apply되어 있어야 한다([18 §3](18-dev-environment.md)).

**시드 MySQL을 활용한 실계정 통합 테스트** — testcontainers는 로컬 MySQL이므로
RDS 고유 동작(IAM DB Auth, RDS 파라미터 그룹, CloudWatch 슬로우로그 내보내기,
`rds_kill_query` 프로시저 부재)을 검증할 수 없다.
`60-seed`의 실제 RDS 인스턴스에 대고 다음을 확인한다.

| 항목 | testcontainers로 불가한 이유 |
|---|---|
| IAM DB Auth 연결·토큰 갱신 | RDS 전용 기능 |
| `information_schema.PROCESSLIST` 절단 여부 (OPEN-Q-07) | RDS 파라미터 그룹 조건에서 확인해야 의미가 있다 |
| IAM auth 활성화 시 대상 메모리 증가 (OPEN-Q-17) | RDS 인스턴스 클래스가 필요 |
| CloudWatch 슬로우로그 내보내기 → `FilterLogEvents` 파싱 | RDS → CloudWatch 경로 |
| `SHOW ENGINE INNODB STATUS` 권한 (RDS는 `SUPER`를 주지 않는다) | 로컬 MySQL은 root라 통과해버린다 |

| 항목 | 검증 |
|---|---|
| 증분 내보내기 | 지정 구간의 변경분만 나오는지 |
| 언네스팅 SQL | DynamoDB JSON → Iceberg 컬럼 매핑 정확성 (모든 타입: S, N, B, BOOL, NULL) |
| `MERGE INTO` 멱등성 | 같은 구간 2회 적재 → 행수 불변 (**FR-STO-06 수용 기준**) |
| 삭제 마커 필터 | TTL 만료 항목이 아카이브에 반영되지 않는지 |
| 체크포인트 재시도 | 실패 후 재실행이 누락 구간을 포함하는지 |
| 조회 라우팅 | 경계 교차 기간 조회 시 dedup 병합 정확성 |
| 보관 만료 | `DELETE` 후 스냅샷 만료로 스토리지가 실제로 줄어드는지 |

## 5. API 계약 테스트

```
1. 서버를 테스트 모드로 기동 (OpenAPI 스키마 검증 미들웨어 활성)
2. 모든 엔드포인트에 대해:
   - 토큰 없이 호출 → 401 (예외: /healthz, /readyz, /api/auth/config, /api/openapi.json)
   - 역할별 호출 → 권한 매트릭스와 일치
   - 응답이 OpenAPI 스키마를 만족
3. 라우터에서 경로 목록을 추출해 테스트가 자동으로 순회
   → 새 엔드포인트를 추가하면 테스트가 자동으로 포함한다 (빠뜨릴 수 없다)
```

이 "라우터 순회" 방식이 중요하다. 엔드포인트별로 테스트를 손으로 쓰면 새로 추가한 것을
반드시 빠뜨린다.

## 6. 프론트엔드 테스트

| 대상 | 도구 | 항목 |
|---|---|---|
| 유틸 | Vitest | 시간 포맷, 숫자 축약, 필터 직렬화 |
| 필터 ↔ URL | Vitest | 왕복 무손실 |
| 플랜 트리 | Vitest + 스냅샷 | v1/v2 픽스처 렌더 |
| 권한별 렌더 | Testing Library | 역할별 버튼 노출 |
| WS 훅 | Vitest + 모의 WS | 재연결, 구독 복원, 백프레셔 |
| 가상 스크롤 | Vitest + 성능 측정 | 1만 행 렌더 시간 임계 |
| 접근성 | axe-core | 전 라우트 자동 검사 |

## 7. E2E (Playwright)

핵심 시나리오 6개만 유지한다. E2E는 비싸고 깨지기 쉬우므로 넓히지 않는다.

```
S1. 로그인 → 플릿 개요 → 인스턴스 목록 → 상세
S2. 슬로우 쿼리 목록 → 필터 → 상세 → 플랜 트리 확인
S3. 다이제스트 목록 → 상세 → 샘플 쿼리 복사 → 플랜 링크    ★ 핵심 요구사항
S4. 부트스트랩 dry-run → 계획에 SQL 표시 확인 (apply 는 하지 않음)
S5. 알림 규칙 생성 → 테스트 발송 → 규칙 삭제
S6. viewer 로 로그인 → 변경 버튼 미노출 + 직접 API 호출 시 403
```

S3와 S6이 가장 중요하다. S3은 이 제품의 핵심 가치, S6은 권한 회귀 방지.

## 8. 부하 테스트

### 8.1 목표

NFR-P-01(워커 4대로 500대), NFR-P-02(대상 DB CPU 1% 미만) 검증.

### 8.2 하네스

인스턴스 500대를 실제로 띄울 수 없으므로 두 단계로 나눈다.

**단계 1: 모의 MySQL 엔드포인트**

```
MySQL 와이어 프로토콜을 말하는 최소 서버를 N개 포트에 띄운다.
  - 핸드셰이크 응답 (mysql_clear_password 플러그인 협상)
  - 우리가 던지는 쿼리 패턴별로 합성 결과 반환
      processlist  → 설정된 분포로 랜덤 행 생성 (슬로우 쿼리 발생률 조절)
      digest 스냅샷 → 300~2000개 행, 카운터를 단조 증가
      global_status → 카운터 증가
  - 응답 지연을 주입 (크로스 리전 시뮬레이션: 0ms / 50ms / 200ms / 500ms)

측정: 워커의 CPU·메모리·tick 지연 p50/p95/p99, DynamoDB 쓰기 처리량
```

목적은 **우리 쪽 확장성**을 재는 것이다. 대상 DB 부하는 이걸로 못 잰다.

**단계 2: 실제 MySQL 1대에 대한 부하 측정**

```
실제 MySQL 8.4 인스턴스(dev RDS) 1대에 수집기를 붙이고
동시에 sysbench 로 워크로드를 준다.

측정: 수집기 있을 때 vs 없을 때의
  - 대상 DB CPU 사용률 차이
  - sysbench QPS·지연 차이
  - 우리 쿼리의 SUM_TIMER_WAIT 비중

폴링 주기 1초 / 0.5초 / 5초, 슬로우 쿼리 발생률 0 / 1/s / 10/s 조합으로 측정
```

### 8.3 통과 기준

| 항목 | 기준 |
|---|---|
| 워커 1대(c7g.xlarge)가 커버하는 인스턴스 수 | ≥ 125 |
| tick 지연 p99 | < 폴링 주기의 50% |
| 워커 메모리 (125대 기준) | < 1GB |
| 대상 DB CPU 증가 (정상 상태) | < 1% |
| 대상 DB QPS 영향 | < 1% |
| 우리 쿼리의 대상 DB 총 실행시간 비중 | < 0.5% |
| 슬로우 쿼리 폭주(초당 100건) 시 | 워커 생존, 유실은 메트릭에 기록 |
| in-flight 플랜 성공률 (2초 이상 쿼리) | ≥ 80% |

**단계 1의 결과가 기준 미달이면 [ADR-001](03-decisions.md)의 전제가 틀린 것이다.**
그 경우 워커 수를 늘리거나 폴링 주기를 조정하는 것으로 대응하고 문서를 갱신한다.

## 9. CI 게이트

```
PR 필수 (머지 차단) — arm64 러너
  cargo fmt --check
  cargo clippy -- -D warnings
  cargo test --workspace                     (단위, 목표 3분 이내)
  cargo audit / cargo deny
  gitleaks
  web: tsc --noEmit / eslint / vitest
  web: openapi 타입 diff 없음
  terraform fmt / validate / plan (dev)
  API 계약 테스트 (라우터 순회 인증·권한)
  정적 검사 (아래 §9.1)
  Playwright: 컴포넌트·모킹만  ← 실 E2E는 dev 배포 후여야 한다

main 머지 후 — arm64 러너
  AL2023 arm64 컨테이너에서 release 빌드
  스모크: 산출물을 AL2023에서 `dbmon --version` 실행   ← glibc 스큐 게이트
  cargo test --features integration          (testcontainers, arm64 이미지)
  dev 배포 → Playwright 실 E2E

일 1회 (실패 시 알림, 머지는 차단하지 않음)
  아카이브 경로 테스트 (dev AWS 실제 리소스, GitHub OIDC Role)
  axe-core 접근성
  부하 테스트 단계 1 (축소판: 인스턴스 100대)

릴리스 전 (수동)
  부하 테스트 전체 (500대 + 실제 DB 부하 측정)
  보안 검증 항목 전량 (08 §11)
  DR 리허설 (연 1회)
```

**빌드는 arm64 러너에서 한다.** x86 러너에서 크로스 컴파일하면 (a) 크로스 링커·cmake
툴체인이 필요하고(`zstd-sys`, TLS 백엔드가 C를 빌드한다), (b) **glibc 스큐**로 배포 당일에
터지고(ubuntu glibc 2.39로 링크 → AL2023 glibc 2.34에서 `GLIBC_2.3x not found`),
(c) **통합 테스트가 x86에서 돌아 출하 바이너리(arm64)는 한 번도 검증되지 않는다.**

### 9.1 정적 검사 (CI 게이트)

코드 레벨 방어를 자동으로 확인한다. 문서에만 "하지 말라"고 쓰면 언젠가 한다.

| 검사 | 방법 |
|---|---|
| 다이제스트 쿼리에 `LAST_SEEN` 필터 존재 | 쿼리 상수를 정규식 검사 (R21) |
| 다이제스트 쿼리에 `DIGEST_TEXT`/`QUERY_SAMPLE_TEXT` 부재 | 동일 |
| `MERGE INTO`에 파티션 조건 존재 | 생성 SQL 스냅샷 검사 (R27) |
| 리포트 쿼리에 시간 범위 조건 존재 | `TimeRange` 필수 인자로 타입 강제 + 생성 SQL 검사 |
| SQL 문자열 결합 함수 부재 | clippy custom lint 또는 `grep` 금지 패턴 |
| `minijinja` `\|safe` 사용 부재 | 템플릿 grep |
| EMF 차원 화이트리스트 | 방출 함수의 차원 상수 검증 (R26) |
| `@@GLOBAL.uptime` 부재 | grep (R30) |
| `SHOW SLAVE STATUS` / `SHOW MASTER STATUS` 부재 | grep |
| `FLUSH PRIVILEGES` 부재 | grep |
| `ANALYZE TABLE` 실행 코드 부재 (권고 문자열은 허용) | grep + 리뷰 |
| presigned URL 반환 부재 (리포트) | 계약 테스트 (R38) |

**커버리지 목표를 숫자로 정하지 않는다.** 대신 다음을 필수로 요구한다:
- `normalize` / `planparse`: 골든 코퍼스 전량 통과
- 상태 머신(in-flight, 알림): 정의된 전이 전량 커버
- 권한 매트릭스: 전 엔드포인트 × 전 역할
- DDL 안전성: 금지 목록 전량 + 우회 시도

이 4개가 통과하면 나머지 커버리지 숫자는 부차적이다.

## 10. 회귀 테스트 대장

1세대에서 실제로 겪은 문제와, 설계 중 발견한 함정을 회귀 테스트로 고정한다.

| # | 문제 | 테스트 |
|---|---|---|
| R1 | `performance_schema.processlist.INFO` 1024바이트 절단 | 통합: 1024자 초과 SQL 캡처 후 길이 검증 |
| R2 | 사후 EXPLAIN 재실행으로 플랜 불일치 | 통합: `plan_source == for_connection` |
| R3 | UPDATE/DELETE 플랜 미수집 | 통합: UPDATE 플랜 수집 성공 |
| R4 | `TIMER_WAIT` 단위 오해 (피코초를 마이크로초로) | 단위: 4초 쿼리의 `duration_ms ≈ 4000` |
| R5 | 스레드 ID 재사용 오탐 | 단위: 같은 ID·다른 다이제스트 → 2건 |
| R6 | KST 문자열 저장으로 집계 불가 | 단위: 저장된 값이 UTC epoch millis |
| R7 | `MAX_TIMER_WAIT`를 창 내 최대로 오해 | 단위: 누적 최대와 창 최대 구분 |
| R8 | 다이제스트 카운터 리셋 시 음수 델타 | 단위: 리셋 케이스 |
| R9 | 상위 N 제한으로 총량 왜곡 | 단위: `_other` 합산 검증 |
| R10 | `sys.schema_unused_indexes` 재시작 직후 오탐 | 단위: uptime < 7일 → `low_confidence` |
| R11 | `TABLE_ROWS`를 정확한 행수로 오해 | 코드 리뷰 + 문서. UI에 "추정" 표기 검증 |
| R12 | 마스터 자격증명 로그 유출 | 통합: 부트스트랩 후 로그 전문 패턴 검사 |
| R13 | 리터럴이 Bedrock으로 전송 | 단위: 골든 페이로드 |
| R14 | 아카이브 이중 적재로 중복 | 통합: `MERGE` 2회 → 행수 불변 |
| R15 | 다이제스트 절단으로 크로스 인스턴스 그룹 분리 | 통합: 긴 쿼리 + 매핑 학습 |
| R16 | 권한 없는 사용자의 데이터 접근 | 계약: 권한 매트릭스 |
| R17 | Athena SQL 인젝션 | 단위: 정렬 키에 페이로드 주입 |
| R18 | 웹훅 SSRF | 단위: 사설 IP·리다이렉트·DNS 리바인딩 |
| R19 | CloudWatch API 비용 폭주 | 단위: 캐시 히트 시 호출 없음 검증 |
| R20 | 알림 폭풍 (의존 억제 미작동) | 단위: 수집 실패 중 억제 |
| R21 | 다이제스트 스냅샷 전량 조회 (`LAST_SEEN` 필터 누락) | 단위: 생성된 SQL에 `LAST_SEEN` 조건 존재 확인 + 텍스트 컬럼 부재 확인 (정적 검사) |
| R22 | 리포트 월 경계 시간대 오류 | 단위: KST 2026-07 → UTC `2026-06-30T15:00Z ~ 2026-07-31T15:00Z` |
| R23 | 워커 2개 동시 평가로 알림 중복 발화 | 통합: 같은 지문을 두 워커가 동시 평가 → 발송 의도 1건 |
| R24 | 인스턴스 이름 변경으로 과거 데이터 유실 | 통합: 이름 변경 후 `renamed_from` 체인으로 조회 성공 |
| R25 | 설정 값으로 자기 파괴 | 단위: `poll_interval_ms=0`, `query_timeout > poll_interval` 거부/경고 |
| R26 | EMF 차원에 `instance_id` → 월 $2,100 | 단위: EMF 방출 함수의 차원 화이트리스트 검증 + `EmfMetricCount` 상한 |
| R27 | `MERGE INTO` 파티션 프루닝 누락 | 단위: 생성 SQL에 파티션 조건 존재 확인 / 통합: 스캔 바이트가 증분 구간에 비례 |
| R28 | 플랜 `attached_condition` 리터럴 유출 | 단위: `plan_normalized`에 리터럴 0개 / 골든 페이로드 |
| R29 | `sys` 뷰의 64자 절단본을 전문으로 오인 | 통합: 락 대기의 SQL이 64자를 넘고 `...`로 끝나지 않음 |
| R30 | `SELECT @@GLOBAL.uptime` 사용 | 단위: 자가진단 SQL 목록에 해당 문장 부재 (정적 검사) |
| R31 | Aurora 버전 문자열 비교 | 단위: `8.0.mysql_aurora.3.05.2` 파싱 → 3.05 판정. `8.0.32`와 문자열 비교하지 않음 |
| R32 | 증분 내보내기 `Item` 오인 | 통합: `NewImage` 구조 파싱 + 매니페스트 기반 파일 선택 |
| R33 | 24시간 초과 내보내기 창 | 통합: 48시간 누락 → 24시간 청크 2개로 분할 실행 |
| R34 | `/* dbmon: */` 주석으로 자기 쿼리 식별 시도 | 통합: 대상 DB 다이제스트에 주석이 남지 않음을 확인 → 계정 기반 식별로 대체 |
| R35 | CloudWatch period 15~63일 구간 제약 | 단위: 30일 전 조회 시 300초 이상으로 조정 |
| R36 | `Deadlocks`/`FreeStorageSpace`를 RDS/Aurora에 잘못 요청 | 단위: 엔진별 메트릭 카탈로그 검증 |
| R37 | 리포트 서술문에 LLM이 만든 숫자 | 단위: LLM 응답의 숫자 토큰이 0개 |
| R38 | presigned URL로 리포트 직접 노출 | 계약: `/api/reports/{id}` 응답에 presigned URL 부재. `/render`가 CSP 헤더 포함 |
| R39 | 알림 본문에 리터럴 | 단위: 발송 페이로드에 정규화 텍스트만 |
| R40 | `Type=notify` 인데 `sd_notify` 미구현 | 통합: 유닛 기동 후 `systemctl is-active` 확인 (또는 CI 스모크) |
| R41 | `in_flight` 선행 저장이 마스킹 이전 | 통합: `masked` 인스턴스의 `in_flight` 레코드에 리터럴 0개 (F2) |
| R42 | 고아 `in_flight` 레코드 (유령 쿼리) | 통합: 워커 강제 종료 → 5분 내 `abandoned` (F4) |
| R43 | 3소스 병합 비대칭 (백필 선착 시 중복·덮어쓰기) | 통합: 슬로우로그 먼저 적재 → 캡처 확정 → 레코드 1건, 정확 지표 보존 (F5) |
| R44 | 다중 active collector (리더 게이트 부재) | 통합: 워커 2개 기동 → `ShardsOwned` 합계 64, standby 0 (F1) |
| R45 | 샤드 인수 시 서킷 상태 유실 → 장애 증폭 | 통합: 서킷 오픈 상태에서 샤드 이동 → 새 소유자가 half-open 부터 시작 (F24) |
| R46 | 시계 오차로 병합 창 상시 실패 | 통합: DB 시각을 3초 앞당김 → 병합 정상, 파티션 정상 (F14) |
| R47 | consumer 런타임 비활성 → 조용한 0데이터 | 통합: `statements_digest` 를 끄고 부하 유지 → 5분 내 `degraded` (F15) |
| R48 | 맵·집합 JSON 문자열 저장으로 lost update | 통합: 두 경로가 동시에 `DT#` upsert → 양쪽 엔트리 보존 (F8) |
| R49 | 어드바이저 캐시 키에 인스턴스 부재 | 단위: 다른 인스턴스 조회 → 캐시 미스 (F10) |
| R50 | S3 오프로드 플랜이 항목보다 오래 잔존 | 단위: 항목 TTL ≤ 객체 Lifecycle 불변식 (F27) |
| R51 | KMS 거부를 스로틀로 오분류 → 데이터 드롭 | 단위: `AccessDeniedException` 주입 → 버퍼 유지, 드롭 0 (F25) |
| R52 | 개선 리포트 p95 를 누적 스냅샷으로 계산 | 단위: 미등록 다이제스트 → "p95 판정 없음" (F3) |
| R53 | `partial` 시간 버킷이 총량 집계를 왜곡 | 단위: partial 20% 구간 → 정규화값·실측값 병기 (F13) |
