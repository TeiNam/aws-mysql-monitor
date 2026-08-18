# 01. 요구사항

## 1. 배경과 문제 정의

MySQL에는 Oracle AWR 같은 표준 성능 리포트가 없다. 상용 APM(Datadog DBM 등)은 대상 DB에
프로시저/이벤트를 설치해 모든 SQL을 훑기 때문에 관측 대상 자체에 부하를 주고, 필요 없는
쿼리의 플랜까지 저장한다. 1세대 PoC로 "필요한 슬로우 쿼리만" 잡는 방식은 검증했으나 다음
한계가 드러났다.

| # | 1세대에서 확인된 문제 | 2세대 대응 |
|---|---|---|
| P1 | `performance_schema.processlist.INFO`가 1024바이트에서 잘려 긴 SQL이 손실 | `information_schema.PROCESSLIST` 타깃 조회로 전문 확보 |
| P2 | 플랜을 사후에 재실행 → 통계 변동으로 실제 플랜과 불일치, UPDATE/DELETE는 수집 포기 | 탐지 시점 `EXPLAIN FOR CONNECTION` |
| P3 | 1초 폴링이라 1초 미만 쿼리는 전부 미관측 → 워크로드 전체 그림이 없음 | `events_statements_summary_by_digest` 델타 스냅샷으로 전수조사 |
| P4 | MongoDB 자체 운영 부담, 1년치 집계 쿼리가 느림 | DynamoDB(핫) + S3 Tables/Iceberg(콜드) + Athena |
| P5 | 인증 없음 → 누구나 접근 | Cognito OIDC + RBAC |
| P6 | 모니터링 DB 계정 비밀번호가 환경변수/시크릿에 상주 | IAM DB Auth (비밀번호 미존재) |
| P7 | 대상 인스턴스를 `env=prd` 태그로만 필터 → stg/dev 관측 불가, 리전 1개 고정 | 멀티 리전 + prd/stg/dev 분류 |
| P8 | 인스턴스 증가 시 단일 프로세스 한계 | 리스 기반 워커 샤딩 |

## 2. 목표 / 비목표

### 목표
- 슬로우 쿼리를 **실행 중에** 잡아 전문 SQL과 실제 실행계획을 함께 남긴다.
- 다이제스트 단위로 워크로드 전체를 집계해, "이 그룹이 몇 번 돌고 총 몇 초를 쓰는가"에
  답한다. 그리고 그 그룹의 **실행 가능한 실제 샘플 쿼리**를 제공한다.
- 관측 대상 DB에 어떤 오브젝트도 설치하지 않는다(읽기 전용 조회만).
- 500대 규모까지 수평 확장한다.
- 월간/개선 리포트를 자동 생성해 수작업 보고서를 없앤다.

### 비목표 (이번 범위 아님)
- MySQL 이외 엔진(PostgreSQL, MSSQL, Oracle) 지원 → 인터페이스만 엔진 추상화, 구현은 MySQL만
- 온프레미스 MySQL 지원 → RDS/Aurora 전용 (CloudWatch·RDS API 의존)
- 자동 DDL 적용/인덱스 자동 생성 → **권고까지만**. 적용은 사람이 한다.
- APM(애플리케이션 트레이싱), 분산 트레이싱 연동
- 쿼리 차단·킬 기능 (읽기 전용 원칙. 단 [OPEN-Q-11](OPEN-QUESTIONS.md) 참조)
- 멀티 계정(AssumeRole) — 단일 계정 확정. 단 세션 계층은 계정 파라미터를 받도록 설계

## 3. 이해관계자와 역할

| 역할 | Cognito 그룹 | 권한 |
|---|---|---|
| 플랫폼 관리자 | `admin` | AWS 설정, 부트스트랩(모니터링 계정 생성), IdP 관리, 사용자 관리, 전 환경 조회 |
| DBA / 운영자 | `operator` | 알림 규칙 CRUD, 리포트 생성, AI 어드바이저 실행, 전 환경 조회 |
| 개발자 | `viewer` | 조회만. 환경 스코프 제한 가능(예: dev/stg만) |

## 4. 기능 요구사항 (FR)

우선순위: **P0** = MVP 필수, **P1** = 1차 정식 릴리스, **P2** = 이후.

### 4.1 인스턴스 탐색 및 등록 (FR-DSC)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-DSC-01 | 설정된 리전 목록 전체에서 `rds:DescribeDBInstances` / `DescribeDBClusters`로 MySQL·Aurora MySQL 인스턴스를 자동 수집한다 | P0 |
| FR-DSC-02 | 수집 주기는 기본 5분, 설정 가능. 수동 "지금 새로고침" 버튼 제공 | P0 |
| FR-DSC-03 | 태그 키 `env` / `Environment` / `environment` / `ENV`(대소문자 무시)의 값을 정규화해 `prd`/`stg`/`dev`/`unknown`으로 분류한다. 값 매핑표는 설정 가능 | P0 |
| FR-DSC-04 | 태그가 없으면 `unknown`으로 분류하고 UI에서 수동 지정할 수 있다. 수동 지정은 태그 재수집에도 유지된다(오버라이드) | P0 |
| FR-DSC-05 | 인스턴스별로 수집 활성/비활성을 개별 토글할 수 있다. 태그 `dbmon:enabled=true|false`로도 제어 가능하며 UI 설정이 태그보다 우선 | P0 |
| FR-DSC-06 | Aurora 클러스터는 라이터/리더를 구분하고, 클러스터-멤버 관계를 표시한다 | P0 |
| FR-DSC-07 | 사라진 인스턴스는 즉시 삭제하지 않고 `deleted_at`을 찍어 30일 보존한다(과거 데이터의 인스턴스 메타 참조 유지) | P0 |
| FR-DSC-08 | 인스턴스별 수집 상태(연결 성공/실패, 마지막 수집 시각, 실패 원인)를 UI에 노출한다 | P0 |
| FR-DSC-09 | 엔진 버전 EOL, 대기 중 유지보수(`PendingMaintenanceActions`), 파라미터 그룹 `pending-reboot` 상태를 표시한다 | P1 |
| FR-DSC-10 | 수집 전제조건 자가진단: 엔진 버전 하한(C-07) 충족, `performance_schema=ON`, `events_statements_current` consumer 활성, `statements_digest` consumer 활성, 슬로우로그 CloudWatch 내보내기 여부, IAM DB Auth 활성 여부, 네트워크 도달 가능성을 인스턴스별로 점검해 미충족 항목과 해결 방법을 표시한다 | P0 |
| FR-DSC-11 | 버전 하한 미달 인스턴스는 목록에 표시하되 수집을 비활성화하고 사유를 `unsupported_version`으로 표시한다. Aurora는 `EngineVersion` 문자열 비교가 불가하므로 Aurora 버전(3.05+) 또는 `SELECT VERSION()`으로 판정한다 | P0 |
| FR-DSC-12 | `rds:DescribeEvents`로 RDS 이벤트(페일오버·재부팅·파라미터 적용·스토리지 오토스케일)를 15분마다 수집하고, 메트릭 차트·슬로우 쿼리 타임라인에 마커로 오버레이한다. 알림 소스로도 제공 | P1 |
| FR-DSC-13 | 대상 RDS의 TLS 인증서 만료(`CertificateDetails.ValidTill`)를 감시한다. 90일 미만 경고, 30일 미만 critical | P1 |
| FR-DSC-14 | 인스턴스 이름 변경을 `DbiResourceId`로 감지해 `renamed_from`/`renamed_to`로 연결한다. Aurora 라이터/리더 역할 변경을 이벤트로 남긴다 | P1 |
| FR-DSC-15 | 대상 RDS 네트워크 접근 요청 상태(`none`/`requested`/`granted`/`regressed`)를 추적한다. `requested` 상태에서는 `unreachable` 알림을 억제하고, 14일 초과 시 에스컬레이션한다 | P1 |

### 4.2 자격증명 부트스트랩 (FR-CRD)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-CRD-01 | 마스터 자격증명 소스를 3가지 중 선택할 수 있다: (a) RDS 관리형 마스터 시크릿(`MasterUserSecret.SecretArn`) 자동 사용, (b) 사용자가 지정한 Secrets Manager 시크릿 ARN, (c) UI 1회 수동 입력 | P0 |
| FR-CRD-02 | (c) 수동 입력한 마스터 자격증명은 **디스크·DB·로그에 절대 기록하지 않는다.** 메모리에만 보관하고 부트스트랩 종료 시 즉시 폐기(zeroize) | P0 |
| FR-CRD-03 | 마스터 자격증명은 **모니터링 계정 생성/권한 부여 시점에만** 사용한다. 상시 수집에는 사용하지 않는다 | P0 |
| FR-CRD-04 | 모니터링 계정을 IAM DB Auth(`AWSAuthenticationPlugin`) 방식으로 생성하는 것을 기본으로 한다. 계정 이름은 설정 가능(기본 `dbmon`) | P0 |
| FR-CRD-05 | IAM DB Auth를 쓸 수 없는 경우(인스턴스 미지원/미활성) 비밀번호 방식으로 폴백하고, 생성한 비밀번호를 Secrets Manager에 저장(30일 자동 로테이션 설정)한다 | P0 |
| FR-CRD-06 | 부트스트랩은 실행 전 **실행될 SQL 전문과 영향 범위를 화면에 보여주고 명시적 확인**을 받는다. dry-run 모드를 제공한다 | P0 |
| FR-CRD-07 | 부트스트랩은 멱등하다. 이미 계정이 있으면 권한만 비교해 부족한 GRANT만 추가한다 | P0 |
| FR-CRD-08 | 여러 인스턴스에 대해 일괄 부트스트랩할 수 있고, 인스턴스별 성공/실패를 개별 보고한다 | P1 |
| FR-CRD-09 | IAM DB Auth 활성화(`ModifyDBInstance --enable-iam-database-authentication`)는 RDS 변경 작업이므로, prd 대상일 때 **별도의 2차 확인**을 받는다 | P0 |
| FR-CRD-10 | 부트스트랩 이력(누가·언제·어떤 인스턴스에·어떤 SQL을)을 감사 로그로 남긴다 | P0 |
| FR-CRD-11 | 모니터링 계정 권한 점검 화면: 현재 부여된 권한 vs 필요 권한 diff를 인스턴스별로 표시 | P1 |

### 4.3 슬로우 쿼리 캡처 (FR-CAP)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-CAP-01 | 인스턴스별 폴링 주기(기본 1초)와 슬로우 임계값(기본 2초)을 개별 설정할 수 있다 | P0 |
| FR-CAP-02 | 탐지는 `performance_schema.processlist`(경량, 뮤텍스 없음)로 하고, 임계값 초과 스레드에 대해서만 `information_schema.PROCESSLIST`를 타깃 조회해 **절단되지 않은 전문 SQL**을 가져온다 | P0 |
| FR-CAP-03 | 같은 시점에 `events_statements_current`에서 해당 스레드의 `DIGEST`, `DIGEST_TEXT`, `ROWS_EXAMINED`, `ROWS_SENT`, `CREATED_TMP_DISK_TABLES`, `NO_INDEX_USED`, `NO_GOOD_INDEX_USED`, `SORT_MERGE_PASSES`, `SELECT_FULL_JOIN`을 함께 수집한다 | P0 |
| FR-CAP-04 | 쿼리의 시작·종료를 추적한다. 종료 시 관측된 최대 실행시간을 확정값으로 저장한다 | P0 |
| FR-CAP-05 | 폴링 사이에 끝난 쿼리는 미관측을 허용하되, 미관측 구간의 존재를 **다이제스트 스냅샷으로 보정**한다(FR-DGS) | P0 |
| FR-CAP-06 | 제외 규칙: 스키마·계정·호스트·SQL 정규식 패턴별로 수집 제외를 설정할 수 있다. 기본 제외는 `mysql`/`information_schema`/`performance_schema`/`sys` 스키마와 `rdsadmin`/`system user`/`event_scheduler`/모니터링 계정. 1세대 태그 `real_time_slow_sql`은 `dbmon:enabled`로 대체하되, 기존 태그도 읽어 마이그레이션을 돕는다 | P0 |
| FR-CAP-07 | 리터럴 값 저장 정책을 인스턴스별로 설정할 수 있다: `full` / `full_restricted`(저장하되 조회를 operator 이상 + 감사) / `masked` / `off`. **기본값은 prd → `full_restricted`, stg·dev → `full`.** 저장 시점 마스킹은 되돌릴 수 없으므로 노출 통제를 우선한다([08 §6.1](08-security-auth.md)) | P0 |
| FR-CAP-08 | 동일 스레드ID 재사용에 의한 오탐을 방지한다(스레드ID + 시작시각 + 다이제스트 조합으로 식별) | P0 |
| FR-CAP-09 | 대상 DB에 어떤 스키마 오브젝트도 생성하지 않는다. 실행하는 문장은 SELECT / SHOW / EXPLAIN 계열로 제한한다 | P0 |
| FR-CAP-10 | 수집기가 대상 DB에 주는 부하를 스스로 측정해 노출한다(초당 쿼리 수, 평균 응답시간, 연결 수). **자기 식별은 모니터링 계정 이름으로 한다** — MySQL 다이제스트는 주석을 제거하므로 `/* dbmon: */` 주석으로는 식별할 수 없다([ADR-005](03-decisions.md)) | P1 |
| FR-CAP-11 | 대상 DB 응답 지연/장애 시 지수 백오프로 폴링 주기를 늘리고, 회복되면 원복한다. 실패가 임계 횟수를 넘으면 알림 | P0 |

### 4.4 실행계획 수집 (FR-PLN)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-PLN-01 | 슬로우 쿼리 탐지 시점에 별도 연결에서 `EXPLAIN FORMAT=JSON FOR CONNECTION <id>`를 실행해 **실행 중 캡처한 옵티마이저 플랜**을 확보한다. 실행 결과(실제 행수·단계별 시간)가 아니므로 "실제 실행 플랜"이라고 표기하지 않는다([ADR-006](03-decisions.md)) | P0 |
| FR-PLN-02 | in-flight 수집이 실패하면(쿼리 종료, EXPLAIN 불가 문장 등) 실패 사유를 기록하고 폴백 경로로 넘긴다 | P0 |
| FR-PLN-03 | 폴백: SELECT 계열에 한해 사후 `EXPLAIN FORMAT=JSON` / `FORMAT=TREE`를 재실행한다. 재실행 플랜임을 `plan_source`로 명확히 구분 저장한다 | P0 |
| FR-PLN-04 | 플랜 수집에는 타임아웃(기본 3초)을 걸고, `EXPLAIN` 자체가 대상 DB를 붙잡지 않도록 한다 | P0 |
| FR-PLN-05 | 같은 다이제스트에 대해 동일 플랜 지문(plan fingerprint)이면 플랜 본문을 중복 저장하지 않고 참조한다 | P1 |
| FR-PLN-06 | 플랜 변화를 추적한다: 같은 다이제스트의 플랜 지문이 바뀌면 `PLAN_CHANGE` 이벤트를 남긴다. **알림 소스로도 제공한다**(접근 방식이 나빠진 경우만 발화해 오탐을 줄인다) | P1 |
| FR-PLN-09 | 플랜의 조건식 필드(`attached_condition`, `index_condition` 등)에는 리터럴이 들어 있다. 정규화 단계에서 리터럴을 마스킹한 `plan_normalized`를 만들고, 리터럴 정책에 따라 원본 보관 여부를 결정한다. **Bedrock에는 항상 정규화본만 보낸다** | P0 |
| FR-PLN-10 | 수집 제외 규칙(FR-CAP-06)에 걸린 세션의 플랜은 수집하지 않는다. `EXPLAIN FOR CONNECTION`은 `PROCESS` 권한으로 다른 계정 세션의 플랜을 읽으므로, 제외하지 않으면 제외 규칙이 무의미해진다 | P0 |
| FR-PLN-07 | UPDATE/DELETE/INSERT...SELECT도 in-flight 경로로 플랜을 수집한다(재실행 폴백은 하지 않음) | P0 |
| FR-PLN-08 | 플랜 JSON이 DynamoDB 항목 한도에 가까우면 S3에 저장하고 포인터만 남긴다 | P0 |

### 4.5 다이제스트 집계 (FR-DGS)

> **이 절이 "바인드 변수화된 그룹 쿼리 + 실제 샘플" 요구사항의 구현체다.**

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-DGS-01 | 인스턴스별로 `performance_schema.events_statements_summary_by_digest`를 주기적으로(기본 60초) 스냅샷하고, 이전 스냅샷과의 **델타**를 계산한다 | P0 |
| FR-DGS-02 | 델타 항목: 실행 횟수, 총/평균/최소/최대 실행시간, 총/평균 락 시간, rows_examined/sent/affected, tmp table(메모리/디스크), no_index_used, no_good_index_used, sort_merge_passes, full_join, full_range_join, 에러/경고 수 | P0 |
| FR-DGS-01a | **1분 해상도 델타는 메모리 링버퍼(인스턴스당 최근 60분)에만 보관한다.** 영속화는 **시간 단위 롤업**으로 한다 | P0 |
| FR-DGS-01b | 영속화 대상은 인스턴스-시간별로 **임계값(기본 100ms) 이상인 후보 중 총 실행시간 상위 N개(기본 200)** 로 한정한다(`OR`가 아니다 — FR-DGS-01f). 제외된 롱테일은 `_other` 합계 1행으로 집계해 총량 정보를 잃지 않는다 | P0 |
| FR-DGS-01c | 상위 N / 임계값은 환경별로 설정 가능하다. 설정 변경이 볼륨에 미치는 영향을 UI에서 추정 표시한다 | P1 |
| FR-DGS-01d | 스냅샷 조회는 `LAST_SEEN >= <이전 스냅샷 시각>` 으로 활성 다이제스트만 가져온다. 필터 값은 **대상 DB의 시각**을 기준으로 하고 5초 안전 여유를 둔다. 전량 조회는 허용하지 않는다 | P0 |
| FR-DGS-01e | `DIGEST_TEXT` / `QUERY_SAMPLE_TEXT` 는 지표 조회와 분리된 2차 쿼리로 가져오고, 워커 캐시에 없는 다이제스트에 대해서만 실행한다 | P0 |
| FR-DGS-01f | 영속화 상한은 **hard cap**이다. 먼저 임계값으로 후보를 걸러낸 뒤 그중 상위 N개만 저장한다(`OR` 조건이 아니다). 잘려나간 후보 수를 `truncated_candidates`로 기록한다 | P0 |
| FR-DGS-02a | 누적 지표(`MIN`/`MAX`/`AVG_TIMER_WAIT`, `QUANTILE_95/99`)는 **델타를 계산하지 않는다.** 스냅샷 값을 그대로 보관하고 컬럼명에 `_cumulative`를 붙여 오용을 막는다 | P0 |
| FR-DGS-02b | 시간창 백분위수가 필요한 경우(개선 리포트, 어드바이저) `events_statements_histogram_by_digest`의 버킷 델타로 계산한다. 대상 다이제스트를 한정(`WHERE DIGEST = ?`)해 조회한다 | P1 |
| FR-DGS-03 | `TRUNCATE TABLE events_statements_summary_by_digest`는 실행하지 않는다. 카운터 리셋(서버 재시작, 다이제스트 테이블 full)을 감지해 델타를 음수로 만들지 않는다 | P0 |
| FR-DGS-04 | 스냅샷 저장 시 `DIGEST_TEXT`(정규화·바인드 변수화된 SQL)를 함께 보관한다. 동일 텍스트는 별도 테이블에 1회만 저장하고 참조한다 | P0 |
| FR-DGS-05 | 인스턴스·MySQL 버전에 무관한 **자체 정규화 해시(`app_digest`)** 를 모든 레코드에 부여해, 크로스 인스턴스·크로스 버전 그룹핑 키로 사용한다 | P0 |
| FR-DGS-06 | 실시간 캡처 레코드(FR-CAP)와 다이제스트 집계를 `app_digest`로 조인해, 다이제스트 상세 화면에서 **집계 통계 + 실제 실행 가능한 샘플 쿼리 N개(리터럴 포함) + 각 샘플의 실행계획**을 함께 보여준다 | P0 |
| FR-DGS-07 | 실시간 캡처에 한 번도 걸리지 않은 다이제스트는 `QUERY_SAMPLE_TEXT`를 샘플로 사용한다. 출처를 `sample_source`로 구분 표시한다 | P0 |
| FR-DGS-08 | 다이제스트별 샘플은 최대 N개(기본 5) 유지하고, 선택 기준은 "가장 느린 것" + "가장 최근 것" + "rows_examined가 가장 큰 것"을 포함하도록 다양화한다 | P1 |
| FR-DGS-09 | 다이제스트 목록을 총 실행시간 / 실행 횟수 / 평균 시간 / rows_examined 기준으로 정렬·필터할 수 있다 | P0 |
| FR-DGS-12 | 다이제스트 사전에 `seen_users` / `seen_hosts` 집합을 유지한다(상한 20개, 초과 시 절단 플래그) | P1 |
| FR-DGS-13 | `mysql_digest`가 있는 소스에서는 인스턴스 내 그룹핑에 **서버 `DIGEST`를 권위값으로** 쓴다. 슬로우로그 레코드에는 `STATEMENT_DIGEST(sql)` 서버 함수로 다이제스트를 부여한다. 자체 `app_digest`는 **크로스 인스턴스 그룹핑과 마스킹**에만 쓴다([ADR-011](03-decisions.md)) | P1 |
| FR-DGS-10 | 신규 등장 다이제스트, 사라진 다이제스트, 급증한 다이제스트를 자동 식별한다 | P1 |
| FR-DGS-11 | `performance_schema_max_digest_length` / `performance_schema_digests_size` 값을 읽어 다이제스트 절단·오버플로 가능성을 경고한다 | P1 |

### 4.6 CloudWatch 슬로우로그 수집 (FR-CWL)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-CWL-01 | RDS 슬로우 쿼리 로그가 CloudWatch Logs로 내보내지는 인스턴스에 대해, 로그를 파싱해 완료된 슬로우 쿼리의 정확한 지표(Query_time, Lock_time, Rows_sent, Rows_examined)를 수집한다 | P1 |
| FR-CWL-02 | 기간을 지정해 과거 로그를 백필할 수 있다(UI + CLI). 인스턴스별 병렬 처리 | P1 |
| FR-CWL-03 | 파싱된 레코드에 `app_digest`를 부여해 실시간 캡처·다이제스트 집계와 조인한다 | P1 |
| FR-CWL-04 | 수집 진행률과 재개 지점(체크포인트)을 저장해, 중단 후 이어서 수집할 수 있다 | P1 |
| FR-CWL-05 | 3소스 중복 제거: 같은 실행을 실시간 캡처와 슬로우로그가 모두 잡았을 때 하나로 병합한다(시각±허용오차 + 스레드ID + app_digest) | P1 |

### 4.7 확장 DB 관측 (FR-OBS)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-OBS-01 | **락 대기·블로킹**: `performance_schema.data_lock_waits` + `data_locks` + `information_schema.INNODB_TRX`로 대기 체인(누가 누구를 막는가)을 수집한다. 임계 시간 초과 시 알림 | P1 |
| FR-OBS-02 | **장기 트랜잭션**: `INNODB_TRX`에서 N초 이상 열린 트랜잭션과 그 트랜잭션이 실행한 문장을 수집 | P1 |
| FR-OBS-03 | **데드락**: `SHOW ENGINE INNODB STATUS`의 LATEST DETECTED DEADLOCK 블록을 파싱해 이력화(같은 데드락 중복 저장 방지) | P1 |
| FR-OBS-04 | **복제 상태**: `SHOW REPLICA STATUS` + `performance_schema.replication_*`로 지연·에러·GTID 갭을 수집. `SHOW SLAVE STATUS`는 사용하지 않는다(8.4에서 제거됨). 바이너리 로그 위치는 `SHOW BINARY LOG STATUS`. Aurora는 `information_schema.replica_host_status` + CloudWatch `AuroraReplicaLag` | P0 |
| FR-OBS-05 | **글로벌 상태 델타**: `SHOW GLOBAL STATUS` / `global_status`를 5초 주기로 스냅샷해 QPS, Threads_running/connected, Innodb_row_lock_waits, Innodb_buffer_pool_hit, Created_tmp_disk_tables, Select_scan, Slow_queries 등의 초당 변화율을 산출한다 (**실시간 패널의 데이터 소스**) | P0 |
| FR-OBS-06 | **인덱스 위생**: `sys.schema_unused_indexes`, `sys.schema_redundant_indexes`로 미사용·중복 인덱스를 주기적으로(기본 1일) 수집 | P1 |
| FR-OBS-07 | **AUTO_INCREMENT 고갈**: `sys.schema_auto_increment_columns`로 사용률 80%/90% 초과 컬럼을 탐지해 알림 | P1 |
| FR-OBS-08 | **풀스캔 쿼리**: `sys.statements_with_full_table_scans` 상위 목록 수집 | P2 |
| FR-OBS-09 | **스키마 변경 추적**: 테이블/인덱스 정의 지문을 일 1회 스냅샷해 변경 이력(추가/삭제/변경)을 남긴다 | P2 |
| FR-OBS-10 | **파라미터 드리프트**: 인스턴스별 주요 파라미터를 기준선과 비교해 차이를 표시 | P2 |
| FR-OBS-11 | **테이블 크기 추이**: `information_schema.TABLES`의 DATA_LENGTH/INDEX_LENGTH/TABLE_ROWS를 일 1회 수집해 증가 추이와 예상 고갈 시점을 산출 | P2 |
| FR-OBS-12 | **메타데이터 락(MDL) 대기**: `performance_schema.metadata_locks`로 "Waiting for table metadata lock" 상태를 수집한다. **InnoDB 행 락(`sys.innodb_lock_waits`)에는 잡히지 않는 별개의 대기**이며, 현실에서 가장 흔한 전면 장애 원인이다(ALTER TABLE이 전체 쿼리를 막는 상황) | P1 |
| FR-OBS-13 | **InnoDB 퍼지 지연**: `information_schema.INNODB_METRICS`의 `trx_rseg_history_len`을 수집한다. Aurora는 CloudWatch `RollbackSegmentHistoryListLength`로 커버되지만 **RDS MySQL에는 동등 경로가 없다.** 장기 트랜잭션의 누적 피해를 보는 유일한 지표 | P1 |
| FR-OBS-14 | **계정·호스트별 워크로드 귀속**: `sys.user_summary_by_statement_type`, `performance_schema.events_statements_summary_by_account_by_event_name`을 수집해 계정별 시간 롤업을 만든다. 임계값 미만 워크로드까지 계정별로 커버한다([ADR-021](03-decisions.md)) | P1 |
| FR-OBS-15 | **파티션 관리**: `information_schema.PARTITIONS`로 RANGE 파티션 테이블의 미래 파티션 부족·오래된 파티션 잔존을 탐지한다. 미래 파티션이 없으면 삽입이 실패하는 전형적 장애 | P2 |
| FR-OBS-16 | **콜레이션 불일치**: 조인에 쓰이는 컬럼 쌍의 문자셋·콜레이션이 다르면 인덱스가 무효화된다. 스키마 스냅샷에서 탐지해 어드바이저 `TYPE_MISMATCH` 규칙과 연결 | P2 |

### 4.8 CloudWatch 메트릭 (FR-MET)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-MET-01 | RDS/Aurora 핵심 메트릭을 조회해 표시한다. 메트릭 세트는 엔진(MySQL/Aurora/Serverless v2)별로 다르게 정의한다 | P0 |
| FR-MET-02 | `GetMetricData`를 배치로 사용한다(단일 호출 최대 500 쿼리). `GetMetricStatistics`는 사용하지 않는다 | P0 |
| FR-MET-03 | 플릿 개요 화면은 **자체 수집 지표(FR-OBS-05)** 를 기본 소스로 쓰고, CloudWatch는 상세 화면 조회 시 온디맨드로만 호출한다. 응답은 캐시한다(기본 60초) | P0 |
| FR-MET-04 | CloudWatch 응답 지연(1~3분)을 UI에 명시한다. "실시간" 패널과 "CloudWatch 추세" 패널을 시각적으로 구분한다 | P0 |
| FR-MET-05 | 기간 선택(1h/3h/6h/12h/1d/3d/1w/1mo/커스텀)에 따라 period를 자동 조정한다(60s/300s/3600s) | P0 |
| FR-MET-06 | 여러 인스턴스의 동일 메트릭을 겹쳐 비교할 수 있다(최대 10개) | P1 |
| FR-MET-07 | 슬로우 쿼리 발생 시점을 메트릭 차트 위에 마커로 오버레이한다 | P1 |
| FR-MET-08 | Enhanced Monitoring(OS 지표, `RDSOSMetrics` 로그 그룹)은 활성 인스턴스에 대해 선택적으로 조회한다 | P2 |
| FR-MET-09 | 월 CloudWatch API 비용 상한을 설정하고, 초과 시 온디맨드 조회를 제한(캐시 강제)한다 | P1 |

### 4.9 저장 및 조회 (FR-STO)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-STO-01 | 최근 31일 데이터는 DynamoDB에서 조회한다. p95 응답 500ms 이내 | P0 |
| FR-STO-02 | 31일 초과 데이터는 S3 Tables(Iceberg)에서 Athena로 조회한다. 최대 1년 보관 | P0 |
| FR-STO-03 | API는 요청 기간을 보고 **DynamoDB / Athena / 양쪽 병합**을 자동 선택한다. 호출자는 저장소를 알 필요가 없다 | P0 |
| FR-STO-04 | Athena 조회는 비동기다. 쿼리 실행 ID를 반환하고 폴링/푸시로 결과를 전달한다. 결과는 캐시한다 | P0 |
| FR-STO-05 | DynamoDB는 **단일 진실 원천(source of truth)** 이다. 아카이브는 DynamoDB에서 파생된다(이중 쓰기 금지) | P0 |
| FR-STO-06 | 아카이브 경로: DynamoDB PITR **증분 내보내기**를 일 1회 실행 → S3 raw → Athena `MERGE INTO`로 S3 Tables(Iceberg)에 적재한다. 재실행해도 중복이 생기지 않는다(멱등) | P0 |
| FR-STO-07 | DynamoDB TTL은 35일(보관 정책 31일 + 여유 4일). 아카이브 적재 성공을 확인한 이후에만 만료되도록 여유를 둔다 | P0 |
| FR-STO-08 | 아카이브 잡 실패 시 알림하고, 다음 실행에서 누락 구간을 포함해 재시도한다(마지막 성공 시각을 체크포인트로 관리) | P0 |
| FR-STO-09 | Iceberg 1년 초과 데이터는 월 1회 `DELETE`로 삭제한다 | P0 |
| FR-STO-10 | 압축 정책: **SQL 계열 텍스트는 평문**(Athena에서 검색 가능해야 함), **플랜 JSON은 zstd 압축 바이너리**(앱만 읽는 불투명 블롭), 항목이 300KB를 넘으면 플랜을 S3로 오프로드하고 포인터만 남긴다 | P0 |
| FR-STO-11 | 보관 기간(핫/콜드)은 환경별로 다르게 설정할 수 있다(예: dev는 7일/90일) | P1 |
| FR-STO-12 | DynamoDB 테이블에 PITR을 활성화한다(증분 내보내기 전제 조건이며 복구 수단이기도 하다) | P0 |

### 4.10 웹 UI (FR-UI)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-UI-01 | 플릿 개요: 환경별 인스턴스 카드, 자체 수집 실시간 지표, 활성 알림, 수집 상태 | P0 |
| FR-UI-02 | 인스턴스 목록: 환경/리전/엔진/수집상태 필터, 태그 표시, 수집 토글 | P0 |
| FR-UI-03 | 인스턴스 상세: 실시간 지표 + CloudWatch 추세 + 최근 슬로우 쿼리 + 복제 상태 + 락 대기 | P0 |
| FR-UI-04 | 슬로우 쿼리 목록: 기간·환경·인스턴스·스키마·계정·실행시간·다이제스트 필터, 가상 스크롤 | P0 |
| FR-UI-05 | 슬로우 쿼리 상세: 전문 SQL(포맷팅), 실행계획 트리 시각화 + TREE 텍스트 + 원본 JSON, 수집 지표, 동일 다이제스트 이력 링크 | P0 |
| FR-UI-06 | 다이제스트 목록: 집계 통계 정렬·필터. 신규/급증 배지 | P0 |
| FR-UI-07 | 다이제스트 상세: 시간대별 추이 차트 + `DIGEST_TEXT` + **실제 샘플 쿼리 목록(리터럴 포함, 클립보드 복사, 각 샘플의 플랜 링크)** + 참조 테이블/인덱스 정보 + AI 어드바이저 결과 | P0 |
| FR-UI-08 | 실시간 슬로우 쿼리 스트림: **탐지 즉시 `in_flight` 상태로 표시하고, 종료 시 확정값으로 갱신한다.** "현재 실행 중" 섹션과 "최근 완료" 섹션을 분리한다(WebSocket) | P0 |
| FR-UI-09 | 설정 - AWS: 리전 목록, 탐색 주기, 태그 매핑 규칙, IAM 진단(현재 Role이 필요한 권한을 갖췄는지 점검) | P0 |
| FR-UI-10 | 설정 - 부트스트랩: 마스터 자격증명 소스 선택, dry-run, 실행, 결과 | P0 |
| FR-UI-11 | 설정 - 수집: 인스턴스별 폴링 주기·임계값·제외 규칙·리터럴 정책 | P0 |
| FR-UI-12 | 설정 - 알림: 규칙 CRUD, 채널 설정, 테스트 발송 | P1 |
| FR-UI-13 | 설정 - 인증: Cognito IdP(SAML/OIDC) 등록·수정, 사용자·그룹 관리 | P1 |
| FR-UI-14 | 알림 센터: 활성/해소 알림 목록, 확인(ack), 음소거 | P1 |
| FR-UI-15 | 리포트: 월간 리포트 목록·조회·생성, 개선 리포트 생성 | P1 |
| FR-UI-16 | 락 대기 화면: 블로킹 체인 트리 시각화 | P1 |
| FR-UI-17 | 인덱스 위생 화면: 미사용/중복 인덱스, AUTO_INCREMENT 고갈 | P1 |
| FR-UI-18 | 다크/라이트 테마, 전 화면 키보드 내비게이션, 폭 좁은 화면 대응 | P1 |
| FR-UI-19 | 조회 조건이 URL에 반영되어 링크 공유가 가능하다 | P0 |

### 4.11 인증·인가 (FR-AUT)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-AUT-01 | Cognito User Pool 기반 로그인. 자가 가입 비활성, 관리자 초대 방식 | P0 |
| FR-AUT-02 | ID/비밀번호 로그인 지원. 비밀번호 정책(최소 12자, 대소문자·숫자·특수문자) 강제 | P0 |
| FR-AUT-03 | SAML 2.0 및 OIDC IdP 연동 지원. 복수 IdP 동시 등록 가능 | P0 |
| FR-AUT-04 | IdP 등록·수정을 관리자 UI에서 할 수 있다 | P1 |
| FR-AUT-05 | TOTP MFA를 선택적/필수로 설정할 수 있다 | P1 |
| FR-AUT-06 | SPA는 Authorization Code + PKCE 플로우를 사용한다. 클라이언트 시크릿을 쓰지 않는다 | P0 |
| FR-AUT-07 | 백엔드는 모든 요청의 JWT를 JWKS로 검증한다(`iss`/`exp`/`token_use`/**`client_id`** — Cognito access token에는 `aud`가 없다). JWKS는 캐시하고 회전에 대응한다 | P0 |
| FR-AUT-08 | 최종 권한 = **토큰의 `cognito:groups`로 유도한 역할 ∩ 서버 측 `USER` 레코드의 역할**. 토큰 클레임만으로 권한이 결정되지 않는다. `USER` 레코드가 없으면 권한 없음(승인 대기) | P0 |
| FR-AUT-12 | 토큰 폐기 경로: `USER` 레코드의 `revoked_after_ms`·`claims_version`을 검증 단계에서 대조한다. 권한 변경 시 기존 토큰이 즉시 무효화된다 | P0 |
| FR-AUT-13 | WebSocket은 5분마다 토큰·권한을 재검증한다. 권한 변경 시 서버가 연결을 강제 종료한다 | P0 |
| FR-AUT-14 | `admin` 그룹에는 MFA를 필수로 강제한다 | P1 |
| FR-AUT-09 | `viewer`는 환경 스코프(예: dev/stg만)를 제한할 수 있다 | P1 |
| FR-AUT-10 | 변경 작업(부트스트랩, 설정 변경, 규칙 변경, IdP 변경)은 감사 로그에 사용자·시각·before/after를 남긴다 | P0 |
| FR-AUT-11 | 세션 만료 시 자동 갱신(refresh token), 실패 시 재로그인 유도 | P0 |

### 4.12 알림 (FR-ALT)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-ALT-01 | 규칙 정의: 소스(슬로우쿼리/다이제스트/메트릭/복제/락/수집상태), 조건, 임계값, 지속시간, 스코프(환경·태그·인스턴스), 심각도, 채널 | P1 |
| FR-ALT-02 | 채널: Slack(Incoming Webhook 및 Bot token), Telegram(Bot API), 인앱 | P1 |
| FR-ALT-03 | 채널 자격증명은 Secrets Manager에 저장한다. UI에는 다시 표시하지 않는다 | P1 |
| FR-ALT-04 | 중복 억제: 지문(rule+scope) 기준 상태 머신(firing→resolved), 재통지 간격, 그룹핑 | P1 |
| FR-ALT-05 | 음소거: 인스턴스·규칙·환경 단위, 기간 지정. 정기 점검 창(maintenance window) 설정 | P1 |
| FR-ALT-06 | 해소(resolved) 알림도 발송한다 | P1 |
| FR-ALT-07 | 알림 메시지에 해당 슬로우 쿼리/다이제스트 상세 화면 딥링크를 포함한다 | P1 |
| FR-ALT-08 | 규칙별 테스트 발송 기능 | P1 |
| FR-ALT-09 | 알림 발송 실패를 재시도(지수 백오프 3회)하고, 최종 실패는 인앱 알림으로 남긴다 | P1 |
| FR-ALT-10 | 기본 제공 규칙 템플릿: 슬로우쿼리 급증, 복제 지연, 락 대기 장기화, 연결 포화, 디스크 여유 부족, AUTO_INCREMENT 고갈, 수집 실패 | P1 |

### 4.13 AI 어드바이저 (FR-AI)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-AI-01 | 다이제스트 단위로 개선 가이드를 생성한다. 실행 트리거는 (a) 사용자 버튼, (b) 조건 충족 시 자동(옵션) | P1 |
| FR-AI-02 | 컨텍스트 수집: `DIGEST_TEXT`, 실행계획 JSON, 참조 테이블의 `SHOW CREATE TABLE`, 인덱스 목록과 카디널리티(`information_schema.STATISTICS`), 테이블 행수·크기(`TABLES`), 집계 지표(실행수·평균시간·rows_examined/sent 비율), MySQL 버전·주요 파라미터 | P1 |
| FR-AI-03 | 참조 테이블 목록은 **실행계획 JSON에서 추출**한다(SQL 파싱 아님). 플랜이 없으면 SQL 파서 폴백 | P1 |
| FR-AI-04 | 기본적으로 리터럴 값을 Bedrock에 보내지 않는다(`DIGEST_TEXT` 사용). 인스턴스별 옵트인 설정 시에만 샘플 리터럴을 포함한다 | P0 |
| FR-AI-05 | Bedrock Guardrail을 적용해 PII 유출을 차단한다 | P1 |
| FR-AI-06 | 출력은 구조화된 스키마로 강제한다: 원인 분석, 인덱스 권고(DDL 문장 포함), 쿼리 재작성안, 예상 효과, 리스크, 검증 방법, 신뢰도 | P1 |
| FR-AI-07 | 결과는 `(app_digest, 스키마지문, MySQL버전, 프롬프트버전)` 키로 캐시한다. 동일 조건 재요청은 캐시 반환 | P1 |
| FR-AI-08 | 월 토큰 예산을 환경별로 설정하고, 초과 시 자동 실행을 중단한다. 사용량을 UI에 표시 | P1 |
| FR-AI-09 | **DDL을 자동 적용하지 않는다.** 복사 가능한 문장 제시까지만 | P0 |
| FR-AI-10 | 카디널리티 수집은 읽기 전용이다. `ANALYZE TABLE`을 실행하지 않는다(통계 갱신은 부하·락 유발) | P0 |
| FR-AI-11 | 권고에 대한 사용자 피드백(적용함/보류/부적절)을 기록해 개선 리포트에 연결한다 | P2 |
| FR-AI-12 | 프롬프트 버전을 코드에 고정하고, 결과에 사용된 모델 ID·프롬프트 버전을 함께 저장한다 | P1 |

### 4.14 리포팅 (FR-RPT)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-RPT-01 | 월간 리포트를 매월 1일 자동 생성한다(EventBridge Scheduler). 수동 생성도 가능 | P1 |
| FR-RPT-02 | 월간 리포트 내용: 환경별 요약, 인스턴스별 슬로우 쿼리 추이, 총 실행시간 상위 다이제스트 Top20, 신규/사라진 다이제스트, 전월 대비 변화, 락/데드락 요약, 복제 지연 요약, CloudWatch 메트릭 요약, 미해결 권고 목록 | P1 |
| FR-RPT-03 | 개선 리포트: 다이제스트와 두 기간(before/after)을 지정하면 실행수·평균/P95 시간·rows_examined·플랜 지문 변화를 비교하고 개선 여부를 판정한다 | P1 |
| FR-RPT-04 | 수치를 포함한 문장은 **typed template**으로 생성한다. Bedrock은 **숫자 없는 해설**만 담당하고, 응답에 숫자가 있으면 그 문장을 제거한다([ADR-017](03-decisions.md)) | P1 |
| FR-RPT-08 | 리포트 생성 시 사용한 Iceberg 스냅샷 ID를 메타에 기록하고, 재현 시 `FOR VERSION AS OF`로 고정한다. 원본 집계 JSON도 S3에 저장한다 | P1 |
| FR-RPT-09 | 리포트 HTML은 **앱이 프록시해 제공한다.** presigned URL을 직접 노출하지 않는다(인증·환경 스코프·감사·CSP 헤더 부여) | P0 |
| FR-RPT-10 | 리포트의 "월"은 `report_timezone`(기본 `Asia/Seoul`) 기준이다. UTC 경계를 메타에 기록한다 | P0 |
| FR-RPT-05 | 리포트는 S3에 저장하고 웹에서 조회한다. 브라우저 인쇄로 PDF 출력 | P1 |
| FR-RPT-06 | 리포트 생성 이력과 실패 사유를 기록한다 | P1 |
| FR-RPT-07 | 리포트를 Slack/Telegram으로 요약+링크 형태로 발송할 수 있다 | P2 |

### 4.15 운영 (FR-OPS)

| ID | 요구사항 | 우선순위 |
|---|---|---|
| FR-OPS-01 | 워커 샤딩: 인스턴스를 워커에 분배한다. 워커 추가/제거 시 자동 재분배 | P1 |
| FR-OPS-02 | `/healthz`(프로세스 생존), `/readyz`(의존성 확인) 엔드포인트 | P0 |
| FR-OPS-03 | 구조화 JSON 로그. 요청 ID·사용자·인스턴스 ID를 컨텍스트로 전파 | P0 |
| FR-OPS-04 | 자체 메트릭을 EMF로 CloudWatch에 내보낸다(수집 지연, 실패율, DynamoDB 쓰기, Bedrock 토큰, Athena 스캔량) | P0 |
| FR-OPS-05 | 설정 변경은 프로세스 재시작 없이 반영한다(폴링 주기, 임계값, 제외 규칙) | P1 |
| FR-OPS-06 | 무중단 배포(ALB + 롤링). 배포 중 수집 중단 최대 30초 | P1 |
| FR-OPS-07 | 그레이스풀 셧다운: ASG Lifecycle Hook으로 시간을 확보한 뒤 `/readyz` 503 전환 → WS 재연결 요청 → 진행 중 요청 완료 → 리스 반납 → 누산기·버퍼 플러시 → `CompleteLifecycleAction` | P0 |
| FR-OPS-08 | 1단계에서는 **active collector 1대 + standby**로 운영한다. standby는 `/readyz` 503으로 ALB에서 빠진다. 샤딩(다중 active)은 펜싱 토큰 구현 이후에만 활성화한다([ADR-018](03-decisions.md)) | P0 |
| FR-OPS-09 | collector·control 역할은 ALB에 연결되지 않으므로, 리스 보유 실패·`CollectStaleness` 초과가 5분 지속되면 `SetInstanceHealth`로 스스로 Unhealthy를 선언한다 | P1 |
| FR-OPS-10 | 각 cron 잡은 성공 시 `JobHeartbeat` 메트릭을 발행한다. CloudWatch 알람이 `TreatMissingData=breaching`으로 침묵을 감지한다 | P0 |
| FR-OPS-11 | 전역 수집 긴급 정지(kill switch): `CFG/GLOBAL/collection.enabled = false`로 모든 인스턴스 수집을 즉시 중단할 수 있다(admin). 대상 DB 부하 의심 시의 1차 대응 수단 | P0 |

## 5. 비기능 요구사항 (NFR)

### 5.1 성능·확장성

| ID | 요구사항 | 검증 방법 |
|---|---|---|
| NFR-P-01 | 인스턴스 500대를 워커 4대 이하로 1초 주기 수집 | 부하 테스트(모의 MySQL 엔드포인트 500개) |
| NFR-P-02 | 대상 DB에 주는 부하: 인스턴스당 CPU 1% 미만, 초당 쿼리 3건 이하(정상 상태) | 실 인스턴스 계측 |
| NFR-P-03 | 슬로우 쿼리 탐지 → 저장 → 화면 표시 지연 p95 3초 이내 | E2E 계측 |
| NFR-P-04 | in-flight 플랜 수집 성공률 80% 이상(2초 이상 실행 쿼리 대상) | 스테이징 실측 |
| NFR-P-05 | DynamoDB 조회 p95 500ms, p99 1s 이내(31일 범위) | 부하 테스트 |
| NFR-P-06 | Athena 조회 p95 30초 이내(1개월 범위 집계) | 실측 |
| NFR-P-07 | 프론트엔드 초기 로드 LCP 2.5초 이내, 슬로우 쿼리 1만 행 목록 스크롤 60fps | Lighthouse + 프로파일 |
| NFR-P-08 | 워커 1대당 메모리 1GB 이하(인스턴스 125대 기준) | 계측 |

### 5.2 신뢰성

| ID | 요구사항 |
|---|---|
| NFR-R-01 | 대상 DB 1대의 장애가 다른 인스턴스 수집에 영향을 주지 않는다(격리) |
| NFR-R-02 | 워커 프로세스 비정상 종료 시 **80초 이내**(리스 TTL 60초 + 스캔 주기 20초)에 해당 샤드가 다른 워커에 재할당된다. 명시적 반납(그레이스풀 셧다운) 시에는 20초 이내. 타이밍의 단일 출처는 [05 §7](05-collector.md) |
| NFR-R-03 | DynamoDB 쓰기 실패는 버퍼 후 재시도(지수 백오프). 버퍼 한도 초과 시 오래된 것부터 버리고 유실 건수를 메트릭으로 노출 |
| NFR-R-04 | 아카이브는 DynamoDB에서 파생되므로 원천 대비 누락이 구조적으로 발생하지 않는다. 잡 실패는 체크포인트 기반 재시도로 복구하고, TTL(35일)보다 훨씬 짧은 주기로 실행해 복구 여유를 둔다 |
| NFR-R-05 | 어떤 외부 의존성(Bedrock, Slack, Athena) 장애도 수집을 멈추지 않는다 |
| NFR-R-06 | 수집 데이터는 유실 허용(관측 데이터). 단 유실은 반드시 관측 가능해야 한다 |

### 5.3 보안

| ID | 요구사항 |
|---|---|
| NFR-S-01 | 마스터 자격증명은 저장하지 않는다. 메모리 보관 시 `zeroize`, 로그·에러 메시지에 절대 노출 금지 |
| NFR-S-02 | 모니터링 계정은 IAM DB Auth로 비밀번호를 없앤다. 폴백 시에도 Secrets Manager + 로테이션 |
| NFR-S-03 | MySQL 연결은 TLS 필수(`REQUIRE SSL` + RDS CA 검증). 인증서 검증을 끄지 않는다 |
| NFR-S-04 | 모니터링 계정에 DML/DDL 권한을 주지 않는다 |
| NFR-S-05 | 저장 데이터는 KMS 암호화(DynamoDB, S3, Secrets Manager, CloudWatch Logs) |
| NFR-S-06 | SQL 텍스트에는 운영 데이터 리터럴이 포함될 수 있다. `masked` 정책과 접근 권한 분리를 제공하고 기본값을 안전한 쪽으로 둔다 |
| NFR-S-07 | 모든 API는 인증 필수. 인증 없이 접근 가능한 것은 `/healthz`뿐 |
| NFR-S-08 | 사용자 입력(기간, 인스턴스 ID, 정렬 키, 필터)은 화이트리스트 검증. Athena SQL은 파라미터 바인딩 또는 엄격한 식별자 검증 |
| NFR-S-09 | IAM 정책은 최소 권한. 와일드카드 리소스는 불가피한 API에만 사용하고 문서에 근거를 남긴다 |
| NFR-S-10 | 시크릿·토큰·SQL 리터럴이 로그에 남지 않도록 로그 마스킹 레이어를 둔다 |
| NFR-S-11 | 프로덕션 RDS를 변경하는 작업(IAM DB Auth 활성화 등)은 명시적 2차 확인 + 감사 로그 |
| NFR-S-12 | 감사 로그는 애플리케이션이 수정·삭제할 수 없다. IAM 정책의 키 범위 Deny + 해시 체인 + 외부 앵커로 3중 보호 |
| NFR-S-13 | 리터럴이 조직 밖으로 나갈 수 있는 경로를 전부 열거하고 각각 통제한다(알림 채널, 리포트, Athena 결과, Bedrock, 내보내기, 브라우저) |
| NFR-S-14 | 마스킹이 실패하면 저장하지 않는다. `masked`/`off` 정책에서 리터럴 잔존 후조건을 assert하고 위반 시 해당 필드를 버린다 |

### 5.4 유지보수성

| ID | 요구사항 |
|---|---|
| NFR-M-01 | 파일당 400줄, 함수당 50줄을 기본 상한으로 한다(불가피하면 근거 주석) |
| NFR-M-02 | 도메인 로직은 AWS SDK·DB 드라이버에 의존하지 않는다(포트/어댑터 분리) |
| NFR-M-03 | 엔진별 SQL은 한 모듈에 격리해 MySQL 버전 분기를 한 곳에서 관리한다 |
| NFR-M-04 | API는 OpenAPI 3.1 스펙을 코드에서 생성하고, 프론트 타입을 스펙에서 생성한다 |
| NFR-M-05 | 설정은 환경변수 > 파일 > DynamoDB 설정 > 기본값 우선순위. 기동 시 필수값 검증 후 실패 시 즉시 종료 |
| NFR-M-06 | 데이터 스키마 변경은 버전 필드를 두고 읽기 시 하위호환 처리 |

### 5.5 관측성

| ID | 요구사항 |
|---|---|
| NFR-O-01 | 인스턴스별 수집 지연·성공률·마지막 성공 시각을 항상 조회할 수 있다 |
| NFR-O-02 | 자체 플랫폼의 오류율·지연이 CloudWatch 대시보드로 제공된다 |
| NFR-O-03 | 비용 관련 사용량(Athena 스캔 바이트, Bedrock 토큰, CloudWatch API 호출 수)을 일별로 집계해 노출한다 |
| NFR-O-04 | 데이터 유실(버퍼 드롭, 아카이브 누락, 플랜 수집 실패)은 각각 별도 메트릭으로 노출한다 |

## 6. 제약 조건

| ID | 제약 | 영향 |
|---|---|---|
| C-01 | 단일 AWS 계정, 멀티 리전 | 크로스 계정 AssumeRole 불필요. 리전별 클라이언트 팬아웃 필요 |
| C-02 | 앱은 EC2에 배포 (IAM Instance Profile 사용) | 로컬 개발은 AWS SSO 프로파일로 동일 코드 경로 사용 |
| C-03 | 관측 대상 DB에 오브젝트 설치 불가 | 프로시저·이벤트·트리거 방식 배제. 폴링 + 읽기 전용 조회만 |
| C-04 | CloudWatch 메트릭 최소 granularity 60초, 수집 지연 1~3분 | "실시간"은 자체 수집으로 별도 제공 |
| C-05 | `performance_schema.threads.PROCESSLIST_INFO`는 1024바이트 절단 | 전문 SQL은 `information_schema.PROCESSLIST` 또는 파라미터 조정 필요 |
| C-06 | `performance_schema_max_sql_text_length`, `performance_schema_max_digest_length`는 read-only 변수 → 파라미터 그룹 변경 + 재시작 필요 | 기본값(1024) 전제로 설계. 조정은 선택적 최적화로만 |
| C-07 | **지원 버전 하한: RDS for MySQL 8.4+, Aurora MySQL 3.x(MySQL 8.0.32+).** MySQL 5.7 및 Aurora MySQL 2.x는 지원하지 않는다 | 5.7 MD5 다이제스트·`SHOW SLAVE STATUS` 분기 코드 없음. 하한 미달 인스턴스는 탐색 목록에 표시하되 수집 비활성 + 업그레이드 안내 |
| C-07a | 다이제스트는 `performance_schema_max_digest_length`에 따라 절단되므로, 파라미터 그룹이 다른 인스턴스 간에는 긴 쿼리의 `mysql_digest`가 달라질 수 있다 | 크로스 인스턴스 그룹핑 키는 `app_digest` 필수 |
| C-08 | IAM DB Auth는 MySQL 신규 연결 초당 약 200개 제한, TLS 필수 | 연결 풀 유지로 회피. 풀 재생성 폭주 방지 로직 필요 |
| C-09 | DynamoDB 항목 최대 400KB | 큰 플랜/SQL은 압축 + S3 오프로드 |
| C-10 | Athena는 비동기, 콜드 스타트 수 초 | UI는 비동기 조회 UX 전제 |
| C-11 | Bedrock 모델 가용 리전 제한 | 크로스 리전 추론 프로파일 사용, 리전 설정 분리 |
| C-12 | RDS 슬로우로그 CloudWatch 내보내기는 인스턴스별 옵션 | 미활성 인스턴스는 FR-CWL 미적용, UI에 표시 |
| C-13 | DynamoDB 증분 내보내기는 PITR 활성화가 전제이며, 내보내기 포맷은 DynamoDB JSON(타입 서술자 중첩 구조) | Athena에서 언네스팅하는 뷰/CTAS SQL을 1회 작성해야 함 |
| C-14 | 다이제스트 스냅샷의 원시 볼륨(1분 해상도)은 500대 기준 일 2억 행 수준 | 시간 롤업 + 상위 N 제한 필수(FR-DGS-01a/b). 미적용 시 비용·성능 모두 파탄 |
| C-15 | S3 Tables의 리전 가용성은 리전마다 다름 | 배포 리전에서 사전 확인. 미지원 시 [OPEN-Q-03](OPEN-QUESTIONS.md)의 플레인 S3 대안 |
| C-16 | DynamoDB 증분 내보내기의 창은 **최소 15분 / 최대 24시간** | 누락 구간은 24시간 미만 청크로 쪼개 순차 실행 |
| C-17 | 증분 내보내기 데이터 파일은 실행별 폴더가 아니라 **공유 폴더(`AWSDynamoDB/data/`)에 누적** | 매니페스트(`manifest-files.json`)를 읽어 이번 실행 파일만 골라야 한다 |
| C-18 | 같은 내보내기 창 안에서 생성 후 삭제된 항목은 출력에 나타나지 않는다 | **하루 안에 쓰고 지우는 데이터를 만들지 않는다** |
| C-19 | EMF 메트릭은 커스텀 메트릭으로 메트릭당 월 과금 | 차원에 `instance_id`를 넣지 않는다. 인스턴스별 값은 로그 필드 |
| C-20 | Bedrock Converse는 `toolSpec.inputSchema`를 검증하지 않고 자동 재시도도 하지 않는다 | 앱이 검증하고 `toolResult status=error`로 재요청 |
| C-21 | S3 버킷 이름은 **전역 네임스페이스** | IAM 리소스에 와일드카드를 쓰면 타 계정 버킷에 접근 가능. ARN 열거 + `aws:ResourceAccount` |
| C-22 | S3는 `Content-Security-Policy` 헤더를 오브젝트 메타데이터로 반환할 수 없다 | HTML을 presigned로 직접 노출하지 않고 앱이 프록시 |
| C-23 | MySQL 다이제스트 정규화는 **주석을 제거한다** | `/* dbmon: */` 주석으로 자기 쿼리를 식별할 수 없다. 계정 이름으로 식별 |
| C-24 | `sys` 뷰의 `waiting_query`/`blocking_query`는 `statement_truncate_len`(기본 64자)으로 절단된다 | 락 화면의 전문 SQL은 `information_schema.PROCESSLIST` 2차 조회로 얻는다 |
| C-25 | `COLUMN_STATISTICS` 히스토그램은 `ANALYZE TABLE ... UPDATE HISTOGRAM`을 실행한 컬럼에만 존재 | 대부분 비어 있다. 어드바이저 품질의 전제로 삼지 않는다 |
| C-26 | RDS 슬로우로그의 `# Time:`은 **문장 완료 시각** | 시작 시각은 `SET timestamp` 또는 `Time − Query_time`으로 유도 |
| C-27 | Cognito Pre Token Generation V1은 **ID 토큰에만** `cognito:groups`를 주입한다 | access token으로 인가하려면 실제 그룹 멤버십(`AdminAddUserToGroup`) 또는 트리거 V2(상위 요금제) 필요 → [OPEN-Q-18](OPEN-QUESTIONS.md) |
| C-28 | **개발계 계정(123456789012)에 프로덕션 워크로드가 함께 있다** — `prd-lla-vpc`(10.3.0.0/16)에 실행 중 EC2 1대 | 계정 전역 설정 변경 금지. 리소스 이름·태그로 구분. IAM 스코핑을 dev에서 좁힌다 ([18 §6](18-dev-environment.md)) |
| C-29 | `dev-vpc-01` 프라이빗 서브넷의 `0.0.0.0/0` 라우트가 **blackhole**이다 (NAT 삭제, 라우트 잔존) | dev 앱은 퍼블릭 서브넷 + 퍼블릭 IP + 인바운드 0 SG로 배치. NAT를 만들지 않는다 |
| C-30 | Cognito 콜백 URL은 HTTPS 필수. 예외는 `http://localhost` | dev는 SSM 포트 포워딩 + `http://localhost:8080/auth/callback` → ALB·ACM·Route53 불필요 |
| C-31 | 인프라 배포는 애플리케이션 코드와 **독립적으로 동작해야 한다** | Terraform을 7개 레이어로 나누고 각 레이어에 독립 state를 둔다 ([14 §1](14-infrastructure.md)) |

## 7. 가정

| ID | 가정 | 틀렸을 때 영향 |
|---|---|---|
| A-00 | 대상 인스턴스가 RDS MySQL 8.4+ 또는 Aurora MySQL 8.0.32+ 이다 | 미달 인스턴스는 수집 대상에서 제외(FR-DSC-11). Aurora 3.x 비중이 높으면 그대로 커버되지만, Aurora 2.x가 남아 있으면 업그레이드가 선행 조건 |
| A-01 | 대상 인스턴스에서 `performance_schema=ON`이다 | OFF면 FR-CAP-03/FR-DGS 전체 불가 → 파라미터 그룹 변경 + 재시작 필요. FR-DSC-10이 이를 탐지 |
| A-02 | 부트스트랩 시점에 마스터 자격증명을 확보할 수 있다 | 불가하면 DBA가 수동으로 GRANT SQL 실행(문서 제공) |
| A-03 | 앱이 대상 RDS에 네트워크로 접근 가능하다(VPC/SG) | 불가하면 SG 규칙 추가 필요. FR-DSC-10이 연결 실패로 표시 |
| A-04 | 슬로우 쿼리 발생량은 인스턴스당 일 수천 건 이하 | 훨씬 많으면 DynamoDB 비용·쓰기 스로틀 재검토 |
| A-05 | 500대 목표는 2년 내 도달 | 더 빠르면 샤딩(FR-OPS-01)을 M2로 앞당김 |
| A-06 | 리터럴 포함 SQL을 저장해도 되는 조직 정책이다 | 아니면 `masked`/`off` 정책을 기본값으로 → [OPEN-Q-15](OPEN-QUESTIONS.md) |
| A-07 | 대상 RDS의 태그를 소유한 팀이 `env` 값을 정확히 유지한다 | 태그가 신뢰 경계 밖이므로 env 변경 시 리터럴 정책을 자동 완화하지 않고 `unknown`은 prd로 취급(T-29) |
| A-08 | 워커 1대가 500대를 커버한다 | 못 하면 [ADR-018](03-decisions.md)의 펜싱 토큰이 M12가 아니라 선행 조건이 된다. M1-9가 결정 |
| A-09 | ~~대상 인스턴스가 이미 존재한다~~ | **거짓으로 확인됨.** 전 리전 RDS 0개 → 시드 인스턴스를 만들어야 개발이 가능하다 ([18 §5](18-dev-environment.md)) |
| A-10 | dev/prd 토폴로지 차이로 검증할 수 없는 항목이 있다 | ALB 동작·WAF·VPC 엔드포인트 IAM 조건·프라이빗 서브넷 지연 → [OPEN-Q-21](OPEN-QUESTIONS.md) |

## 8. 수용 기준 (릴리스 게이트)

### MVP (M0~M3)
- 리전 2개 이상에서 인스턴스가 자동 탐색되고 prd/stg/dev로 분류된다.
- 부트스트랩으로 IAM DB Auth 모니터링 계정이 생성되고, 이후 비밀번호 없이 수집이 동작한다.
- 2초 이상 실행되는 SELECT/UPDATE에 대해 전문 SQL과 in-flight 실행계획이 저장된다.
- 다이제스트 목록에서 그룹을 선택하면 집계 통계와 실제 샘플 쿼리 + 플랜이 함께 보인다.
- Cognito 로그인 없이는 어떤 데이터도 조회되지 않는다.
- 인스턴스 100대에 대해 워커 1대로 1초 주기 수집이 안정적으로 유지된다.
- 다이제스트 스냅샷 쿼리에 `LAST_SEEN` 필터가 없으면 컴파일되지 않는다(타입 강제).
- `masked` 정책 인스턴스의 저장된 `sql_text`와 `plan_normalized`에 리터럴 토큰이 0개다.
- 실시간 스트림에 **진행 중인 쿼리**가 표시되고, 종료 시 같은 행이 확정값으로 갱신된다.

### 1차 정식 (M4~M8)
- CloudWatch 메트릭 화면이 동작하고, 월 API 비용이 상한 내에 있다.
- 32일 전 데이터가 Athena 경로로 조회된다. 아카이브 잡을 2회 연속 실행해도 Iceberg 행수가 변하지 않는다(멱등성 확인).
- Slack/Telegram 알림이 발송·해소되고 중복 억제가 동작한다.
- AI 어드바이저가 인덱스 권고 DDL을 포함한 구조화 결과를 반환한다.
- 월간 리포트가 자동 생성되고 모든 수치가 Athena 결과와 일치한다.
