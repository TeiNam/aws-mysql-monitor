# 04. 데이터 모델

## 0. 설계 원칙

1. **DynamoDB가 단일 진실 원천이다.** Iceberg는 그것에서 파생된다([ADR-004](03-decisions.md)).
2. **모든 레코드에 `record_id`가 있다.** 결정론적으로 계산되며, 재수집·재적재가 멱등해진다.
3. **모든 시각은 UTC epoch millis로 저장한다.** 파티션 날짜 문자열도 UTC 기준.
   단 `record_id`의 시각 성분은 **초 단위로 floor 정규화**한다 — 아래 §1.2 참조.
   표시 시각 변환은 프론트엔드에서만 한다. (1세대는 KST 문자열을 저장해서 집계가 어려웠다)
4. **압축은 용도로 갈린다.** SQL 계열 텍스트는 평문(Athena에서 `LIKE` 검색 대상),
   플랜 JSON은 zstd 바이너리(앱만 읽는 블롭), 초대형은 S3.
5. **`app_digest`가 모든 그룹핑의 축이다.** 3중 소스 조인 키([ADR-011](03-decisions.md)).
6. **스키마에 `v`(버전) 필드를 둔다.** 읽기 시 하위호환 처리.

## 1. 식별자 규약

| 식별자 | 형식 | 예 |
|---|---|---|
| `instance_id` | `<region>/<db_instance_identifier>` | `ap-northeast-2/orders-prd-01` |
| `cluster_id` | `<region>/<db_cluster_identifier>` | `ap-northeast-2/orders-prd` |
| `record_id` (슬로우 쿼리) | `<instance_id>:<thread_id>:<started_at_sec>` | `ap-northeast-2/orders-prd-01:8842119:1755500400` |
| `app_digest` | `sha256(정규화SQL)` 앞 32 hex | `9f2c1a...` (32자) |
| `mysql_digest` | MySQL이 계산한 SHA-256 64 hex | `a3f1...` (64자) |
| `plan_fingerprint` | `sha256(플랜 구조 정규화)` 앞 16 hex | `4b81c9...` |
| `schema_fingerprint` | `sha256(참조 테이블들의 DDL+인덱스 정규화)` 앞 16 hex | `77de02...` |
| `hour_bucket` | `YYYY-MM-DDTHH` (UTC) | `2026-08-18T14` |
| `date_part` | `YYYY-MM-DD` (UTC) | `2026-08-18` |
| `dur_bucket` | `b0`(2~5s) `b1`(5~15s) `b2`(15~60s) `b3`(60s+) | `b1` |

### 1.1 식별자 표

위 표가 규약이다. `instance_id`에 리전을 넣는 이유: 멀티 리전이고, 리전이 다르면 같은
identifier가 존재할 수 있다.

### 1.2 `record_id`의 시각 성분은 초 단위로 정규화한다

`started_at_ms`는 소스에 따라 값이 달라진다.

| 소스 | 산출 방식 | 오차 |
|---|---|---|
| processlist 관측 | `최초관측시각 − TIME(초)` | ±1초 |
| `events_statements_current` | `관측시각 − TIMER_WAIT/1e9` | ms 이하 |
| CloudWatch 슬로우로그 | `# Time` − `Query_time` | 초 단위 |

`record_id`에 밀리초를 넣으면 **같은 실행이 소스마다 다른 `record_id`를 갖게 된다.**
그러면 (a) 3소스 병합이 키로 성립하지 않고, (b) 아카이브 `MERGE INTO`가 중복 행을 만들고,
(c) "결정론적 `record_id`" 테스트가 통과할 수 없다.

**규칙**
```
record_id = <instance_id>:<thread_id>:<floor(started_at_ms / 1000)>
```
- 밀리초 정밀 시각은 `started_at_ms`(표시·정렬용)와 `started_at_ms_precise`
  (`TIMER_WAIT` 기반, 있을 때만) 별도 속성에 담는다.
- 초 단위 floor로도 어긋날 수 있다(경계에서 1초 차이). 그래서 [05 §8.2](05-collector.md)의
  병합은 `record_id` 동일성만 믿지 않고, `(instance_id, thread_id, ±2초 시간창, app_digest)`
  보조 조회로 후보를 찾은 뒤 병합한다. 병합 시 `record_id`는 **먼저 저장된 쪽을 유지**한다.
- 같은 스레드가 같은 초에 두 쿼리를 시작하면 충돌한다. 실행시간이 임계값(2초) 이상인
  쿼리만 저장하므로 물리적으로 발생하지 않는다.

### 1.3 식별자 안정성 문제

`instance_id`는 RDS 인스턴스 식별자를 포함하므로 **이름을 바꾸면 값이 바뀐다.**
이 값이 파티션 키에 들어가 있어 과거 데이터와의 연결이 끊긴다.

**대안 검토**

| 방안 | 장점 | 단점 |
|---|---|---|
| `region/DbiResourceId` (예: `ap-northeast-2/db-ABCDEFGHIJKL`) | 이름 변경에 불변 | 로그·URL·알림에서 사람이 읽을 수 없다. 디버깅 비용이 매일 발생 |
| **`region/identifier` (채택)** | 읽기 쉬움 | 이름 변경 시 연결 끊김 |

**채택 이유** — 이름 변경은 드물고(연 수 회), 읽기 어려운 식별자의 비용은 매일 발생한다.
드문 사건을 위해 상시 비용을 내지 않는다.

**이름 변경 대응** — `dbi_resource_id`를 인스턴스 레지스트리 속성으로 저장하고, 탐색 시
"같은 `dbi_resource_id`인데 identifier가 다름"을 감지한다.

```
감지 시:
  1. 새 instance_id 로 레지스트리 항목 생성 (renamed_from = 구 instance_id)
  2. 구 항목은 deleted_at 대신 renamed_to = 신 instance_id 로 마킹, 30일 보존
  3. 과거 데이터는 구 instance_id 아래 그대로 둔다 (이동시키지 않는다)
  4. UI는 두 항목을 연결해 표시하고, 조회 시 양쪽 키를 모두 질의한다
  5. RENAME 이벤트를 남긴다
```

이름을 바꾼 뒤 30일이 지나면 과거 데이터는 TTL로 만료되므로 연결 필요성도 사라진다.
Iceberg에는 양쪽 `instance_id`가 남아 있으므로 리포트에서는 `renamed_from` 체인을 따라
합산한다.

**Aurora 페일오버** — 라이터/리더 역할이 바뀌어도 인스턴스 식별자는 그대로다.
`is_cluster_writer` 속성만 갱신되고 `instance_id`는 불변이다. 문제없다.
단 역할 변경을 이벤트로 남겨, "이 시점 이후 이 노드가 라이터였다"를 알 수 있게 한다.

## 2. DynamoDB 테이블 `dbmon-data`

### 2.1 키 구조

```
PK   (S)  파티션 키
SK   (S)  정렬 키
GSI1PK / GSI1SK   — 다이제스트 축 + 활성 알림 (희소 인덱스)
GSI2PK / GSI2SK   — 환경 × 시간 축
ttl  (N)  epoch seconds
v    (N)  스키마 버전
```

- 과금: **온디맨드**. 쓰기 패턴이 버스트(정시 롤업 플러시)라 프로비저닝이 어렵다.
- PITR: **활성**(증분 내보내기 전제 조건, FR-STO-12)
- 암호화: KMS 고객 관리 키
- GSI 사영: `INCLUDE` — 목록 표시에 필요한 속성만. `sql_text`/`plan_json`은 제외해
  인덱스 크기와 쓰기 비용을 낮춘다(상세는 기본 테이블에서 `GetItem`).

### 2.2 접근 패턴 → 키 매핑

| # | 접근 패턴 | 인덱스 | 키 |
|---|---|---|---|
| AP-1 | 특정 인스턴스의 기간별 슬로우 쿼리(최신순) | 기본 | `PK = SQ#<instance_id>#<date>`, SK 범위 |
| AP-2 | 슬로우 쿼리 단건 상세 | 기본 | `record_id`에서 PK/SK 유도 → `GetItem` |
| AP-3 | 특정 다이제스트의 최근 실행 샘플(크로스 인스턴스) | GSI1 | `GSI1PK = DG#<app_digest>`, `begins_with(GSI1SK,'Q#')`, 역순 |
| AP-4 | 환경별 슬로우 쿼리 목록(최신순, 실행시간 구간 필터) | GSI2 | `GSI2PK = ENV#<env>#<dur_bucket>#<hour_bucket>` |
| AP-5 | 인스턴스-시간별 다이제스트 롤업 | 기본 | `PK = DR#<instance_id>#<yyyy-mm>`, SK 범위 |
| AP-6 | 특정 다이제스트의 시간별 추이(크로스 인스턴스) | GSI1 | `GSI1PK = DG#<app_digest>`, `begins_with(GSI1SK,'R#')` |
| AP-7 | 다이제스트 정규화 텍스트·메타 조회 | 기본 | `PK = DT#<app_digest>`, `SK = META` |
| AP-8 | 다이제스트의 "가장 느렸던" 고정 샘플 | 기본 | `PK = DT#<app_digest>`, `SK = SLOWEST` |
| AP-9 | 활성 알림 목록 | GSI1 | `GSI1PK = ALS#firing` (희소) |
| AP-10 | 기간별 알림 이력 | 기본 | `PK = AL#<date>`, SK 범위 |
| AP-11 | 어드바이저 결과 캐시 조회 | 기본 | `PK = AD#<app_digest>`, `SK = <schema_fp>#<ver>#<prompt_ver>` |
| AP-12 | 인스턴스 이벤트(락/데드락/복제/플랜변경) | 기본 | `PK = EV#<instance_id>#<date>`, SK 범위 |
| AP-13 | 환경·종류별 이벤트 | GSI2 | `GSI2PK = EVK#<env>#<kind>#<date>` |
| AP-14 | 일별 스냅샷(인덱스 위생/스키마/테이블 크기) | 기본 | `PK = SNAP#<kind>#<instance_id>`, `SK = <date>#<obj>` |
| AP-15 | 인스턴스 자체 지표 시간 롤업 | 기본 | `PK = MR#<instance_id>#<yyyy-mm>`, `SK = <hour_bucket>` |
| AP-16 | 계정별 워크로드 시간 롤업 ([ADR-021](03-decisions.md)) | 기본 | `PK = UR#<instance_id>#<yyyy-mm>`, `SK = <hour_bucket>#<db_user>` |
| AP-17 | 환경 전체 Top 다이제스트 (사전 집계, [ADR-003](03-decisions.md)) | 기본 | `PK = TOP#<env>#<yyyy-mm-dd>`, `SK = <hour_bucket>#<rank>` |
| AP-18 | 진행 중(`in_flight`) 레코드 조회 (고아 정리, F4) | GSI1 | `GSI1PK = SQS#in_flight` (희소) |
| AP-19 | 히스토그램 롤업 (관찰 등록 다이제스트, [12 §3.2.1](12-reporting.md)) | 기본 | `PK = HG#<instance_id>#<yyyy-mm>`, `SK = <hour_bucket>#<app_digest>` |
| AP-20 | 발송 의도 큐 ([10 §2.0](10-alerting.md)) | 기본 | `PK = NOTIFY`, `SK = <epoch_ms>#<fingerprint>` |
| AP-21 | AI 토큰 사용량 누적 (F30) | 기본 | `PK = USAGE#<env>`, `SK = <yyyy-mm>` |

### 2.3 엔티티 상세

#### SlowQuery — 실시간 캡처된 개별 슬로우 쿼리

```
PK        SQ#<instance_id>#<date_part>
SK        <started_at_ms>#<thread_id>
GSI1PK    DG#<app_digest>
GSI1SK    Q#<started_at_ms>#<instance_id>
GSI2PK    ENV#<env>#<dur_bucket>#<hour_bucket>
GSI2SK    <started_at_ms>#<thread_id>
ttl       started_at_ms/1000 + 35일
v         1
```

| 속성 | 타입 | 설명 |
|---|---|---|
| `record_id` | S | 멱등 키 |
| `instance_id` / `cluster_id` / `region` / `env` | S | 인스턴스 메타(비정규화 — 조회 시 조인 회피) |
| `engine` / `engine_version` | S | `mysql` / `aurora-mysql`, `8.4.5` |
| `thread_id` | N | `information_schema.PROCESSLIST.ID` |
| `schema_name` | S | `DB` 컬럼. NULL 가능 |
| `db_user` / `db_host` | S | 접속 계정 / 클라이언트 호스트:포트 |
| `started_at_ms` / `ended_at_ms` | N | 시작 추정 = 최초 관측시각 − 관측 TIME |
| `duration_ms` | N | 관측된 최대 실행시간(확정값) |
| `duration_source` | S | `polled`(초 단위 근사) / `slowlog`(정확) / `merged` |
| `sql_text` | S | **평문**. 리터럴 정책에 따라 원문·마스킹·생략 |
| `sql_text_truncated` | BOOL | `information_schema` 타깃 조회 실패 시 true |
| `literal_policy` | S | `full` / `masked` / `off` |
| `app_digest` / `digest_algo_version` | S / N | 자체 정규화 해시 |
| `mysql_digest` | S | PS에서 읽은 값. 없으면 미존재 |
| `statement_type` | S | `SELECT`/`UPDATE`/`DELETE`/`INSERT`/`DDL`/`OTHER` |
| `rows_examined` / `rows_sent` / `rows_affected` | N | `events_statements_current` |
| `tmp_tables` / `tmp_disk_tables` / `sort_merge_passes` | N | 동일 |
| `no_index_used` / `no_good_index_used` / `full_join` | BOOL | 동일 |
| `lock_time_ms` | N | 동일 (`LOCK_TIME`은 피코초 → ms 변환) |
| `plan_json` | B | **zstd 압축** EXPLAIN JSON. 300KB 초과 시 미존재 |
| `plan_s3_key` | S | 오프로드된 경우의 S3 키. **키에 만료 티어를 포함한다** (F27) |
| `plan_format_version` | S | `json_v1` / `json_v2` |
| `plan_tree` | S | `FORMAT=TREE` 텍스트(수집 가능한 경우) |
| `plan_source` | S | `for_connection` / `rerun` / `rerun_as_select` / `none`. **`rerun` 이 실질 기본값**이고 `for_connection` 은 RDS 에서 불가하다 ([19 §B](19-m1-findings.md)) |
| `plan_approximate` | BOOL | `plan_source=rerun_as_select` 면 true. DML 의 조건절만 SELECT 로 바꿔 얻은 플랜이므로 쓰기 단계가 빠져 있다 |
| `plan_error` | S | 실패 사유 |
| `plan_fingerprint` | S | 플랜 구조 해시 |
| `referenced_tables` | L(S) | 플랜에서 추출한 `schema.table` 목록 |
| `capture_source` | S | `processlist` / `slowlog` / `merged` |
| `captured_at_ms` | N | 우리가 저장한 시각(수집 지연 계산용) |

**파티션 검토** — DynamoDB의 10GB item collection 한도는 **LSI가 있는 테이블에만**
적용된다. 이 설계는 GSI만 쓰므로 파티션 키당 크기 한도가 없다.
실제 제약은 **파티션당 처리량**(약 3,000 RCU / 1,000 WCU)과 **핫키 편중**이다.

`SQ#<instance_id>#<date>` PK는 바쁜 인스턴스 1대의 쓰기가 하루 동안 한 파티션에 집중된다.
슬로우 쿼리는 초당 수 건 수준이라 정상 상태에서는 문제없지만, 폭주 시(초당 수백 건)
스로틀이 발생할 수 있다.
→ 시간 샤드 추가(`SQ#<instance_id>#<date>#<hour/6>`)의 트리거는 **건수가 아니라
CloudWatch `ThrottledRequests` 지표**로 정의한다. 조회 시 샤드 4개를 병합해야 하므로
필요해질 때까지 도입하지 않는다.

**`dur_bucket`을 GSI2PK에 넣는 이유** — 사용자가 실제로 보는 건 "느린 것"이다.
버킷을 파티션 키에 넣으면 (a) "15초 이상만" 조회가 해당 버킷만 읽어 스캔량이 급감하고,
(b) 쓰기가 4배 많은 파티션에 분산된다. 대가: "전체 최신순"은 4개 버킷을 각각 조회해
애플리케이션에서 병합해야 한다(각 `Limit=25`, 힙 병합). 4회 병렬 Query면 비용·지연 모두 무시 가능.

#### DigestText — 다이제스트 사전 + 고정 샘플

```
PK        DT#<app_digest>
SK        META  |  SLOWEST
```

`SK=META`:

| 속성 | 타입 | 설명 |
|---|---|---|
| `digest_text` | S | **평문**. 바인드 변수화된 정규화 SQL (MySQL `DIGEST_TEXT` 또는 자체 정규화 결과) |
| `digest_text_source` | S | `mysql` / `app` |
| `statement_type` | S | |
| `mysql_digests` | M | `{instance_id: Set<mysql_digest>}` — PS 다이제스트와의 대응표. **인스턴스당 여러 개다** (§아래 N:1) |
| `first_seen_ms` / `last_seen_ms` | N | |
| `seen_instances` | SS | 이 다이제스트가 관측된 인스턴스 집합 |
| `ps_sample_text` | S | `QUERY_SAMPLE_TEXT` (실시간 캡처에 안 걸린 다이제스트의 유일한 실행 가능 샘플) |
| `ps_sample_at_ms` | N | |
| `referenced_tables` | L(S) | |
| `digest_algo_version` | N | |
| `ttl` | N | `last_seen_ms/1000 + 400일` (쓰기마다 갱신) |
| `item_size_bytes` | N | 항목 크기 추정 (400KB 한도 감시용) |

**`app_digest` : `mysql_digest` 는 1:N 이다 (M1-6b 실측, [19 §D](19-m1-findings.md)).**
초기 설계는 인스턴스당 `mysql_digest` **하나**를 가정했다. MySQL 은 다음을 서로 다른
다이제스트로 본다.

| 차이 | MySQL | 우리 |
|---|---|---|
| 공백·주석·키워드 대소문자·백틱 | 같음 | 같음 |
| **식별자 대소문자** (`orders` vs `ORDERS`) | **다름** | 같음 |
| **후행 세미콜론** | **다름** | 같음 |

`lower_case_table_names=0`(리눅스 기본)에서 식별자 대소문자는 실제로 다른 테이블일 수 있으므로
MySQL 이 구분하는 것이 맞다. 우리는 수렴성을 위해 접는다(불가피하다 — [19 §C-1](19-m1-findings.md)).
→ 매핑 학습(M4-16)이 **집합에 추가**하는 연산이어야 한다. 덮어쓰면 학습이 소실된다.

**맵·집합에 정리 주체가 필요하다 (F28)** — 활성 다이제스트의 `META`는 TTL이 계속 갱신되어
사실상 **영구 항목**인데, 인스턴스가 삭제되거나 이름이 바뀌어도(`renamed_from` 체인)
`mysql_digests` / `seen_instances` / `seen_users` / `seen_hosts`에서 항목이 제거되지 않았다.
수년 운영하면 삭제된 인스턴스·계정·호스트가 누적되어 **400KB 항목 한도**와 GSI 쓰기 비용에
접근하고, UI의 `seen_instances`가 존재하지 않는 인스턴스를 표시한다.

```
각 엔트리에 last_seen_ms 를 함께 저장한다
  mysql_digests  : { "<instance_id>": { "d": "<digest>", "t": <last_seen_ms> } }
  seen_users     : SS 대신 M { "<user>": <last_seen_ms> }   ← 정리를 위해 맵으로
  seen_instances : M { "<instance_id>": <last_seen_ms> }

정리 잡 (아카이브 잡에 편승, 일 1회):
  ├ last_seen_ms < now - 90일 인 엔트리를 제거 (REMOVE 경로 갱신)
  ├ 인스턴스 삭제 이벤트 수신 시 해당 키를 즉시 제거
  └ item_size_bytes > 300KB 인 항목을 알림 (한도 접근 감시)
```

`seen_users`/`seen_instances`를 `SS`가 아니라 `M`으로 두는 이유: `SS`는 `ADD`로 원자 추가가
되지만 **개별 엔트리의 시각을 저장할 수 없어 정리 기준이 없다.** 맵 경로 갱신
(`SET seen_users.#u = :now`)도 원자적이므로 F8의 요구를 만족한다.

`SK=SLOWEST`: 지금까지 관측된 **가장 느린 실제 실행 1건**을 고정 보관한다.
`duration_ms > 기존값` 조건부 업데이트로 갱신하고, TTL은 400일.
→ SlowQuery 본체가 35일 TTL로 사라진 뒤에도 **실행 가능한 샘플 한 개는 항상 남는다.**

**S3 오프로드 플랜의 수명주기를 항목 TTL과 맞춘다 (F27)** — 두 방향의 불일치가 있었다.

| 문제 | 초기 설계 | 결과 |
|---|---|---|
| (a) 본체는 35일에 사라지는데 오프로드 플랜은 400일 Lifecycle | 삭제 주체 없음 | 리터럴을 포함할 수 있는 원본 JSON이 **365일 더 잔존**. 보관 정책 위반 + 순수 비용 |
| (b) `SLOWEST`는 갱신 시 TTL이 밀리는데 S3 객체 만료는 **생성 시점 기준** | 어긋남 | "35일 후에도 남는 실행 가능 샘플"의 **플랜 참조가 끊긴다** |

→ **오프로드 키에 만료 티어를 넣어 Lifecycle을 분리한다.**

```
s3://dbmon-plans-<acct>/ttl35/<record_id>.zst    Lifecycle 40일   (일반 SlowQuery)
s3://dbmon-plans-<acct>/ttl400/<app_digest>.zst  Lifecycle 405일  (SLOWEST 고정 샘플)
```

- `SLOWEST` 갱신 시 플랜 객체를 `ttl400/` 티어로 **복사**한다(원본은 `ttl35/`에 그대로).
  `CopyObject` 1회이고 SLOWEST 갱신은 드물다.
- Lifecycle을 항목 TTL보다 **5일 길게** 둔다(TTL 삭제와 Lifecycle 삭제의 시차 흡수).
- **불변식**: 항목의 TTL이 참조하는 S3 객체의 Lifecycle보다 길지 않다.
  → [§7](#7-검증-항목-테스트로-강제) 검증 항목에 추가.

**요구사항 대응** — "실시간 캡처한 쿼리를 PS의 바인드 변수화된 그룹 쿼리와 묶어 샘플로"는
이 엔티티 + AP-3 + AP-8의 조합이다:

```
다이제스트 상세 화면
  ├─ digest_text          ← DT#<digest> / META            (바인드 변수화된 그룹 쿼리)
  ├─ 시간별 집계 추이      ← GSI1 R# prefix                (전체 워크로드 통계)
  ├─ 최근 실행 샘플 5개    ← GSI1 Q# prefix, Limit=5, 역순  (리터럴 살아있는 실제 쿼리 + 플랜)
  ├─ 최악 실행 샘플 1개    ← DT#<digest> / SLOWEST          (35일 이후에도 생존)
  └─ 샘플 없을 때          ← ps_sample_text                (PS가 준 샘플, 출처 표시)
```

#### DigestRollup — 시간 단위 다이제스트 집계

```
PK        DR#<instance_id>#<yyyy-mm>
SK        <hour_bucket>#<app_digest>
GSI1PK    DG#<app_digest>
GSI1SK    R#<hour_bucket>#<instance_id>
ttl       hour 시작 + 35일
```

| 속성 | 타입 | 설명 |
|---|---|---|
| `exec_count` | N | 시간 내 실행 횟수 델타 |
| `total_time_ms` / `avg_time_ms` / `min_time_ms` / `max_time_ms` | N | |
| `total_lock_time_ms` | N | |
| `rows_examined_sum` / `rows_sent_sum` / `rows_affected_sum` | N | |
| `tmp_tables_sum` / `tmp_disk_tables_sum` / `sort_merge_sum` | N | |
| `no_index_used_count` / `no_good_index_count` / `full_join_count` | N | |
| `errors_sum` / `warnings_sum` | N | |
| `schema_name` | S | |
| `env` | S | |
| `partial` | BOOL | 워커 재시작 등으로 시간 일부만 관측했으면 true |
| `partial_minutes` | N | 그 시간 중 실제 관측된 분 수 (0~60). 총량 정규화용 (F13) |
| `flush_failed` | BOOL | 재시도 소진 후 강제 기록됐으면 true (F20) |
| `reset_detected` | BOOL | 시간 내 카운터 리셋 감지 시 true |
| `top_n` / `threshold_ms` | N | **이 버킷을 만들 때 적용된 설정값** (F21) |
| `digest_algo_version` | N | 정규화 알고리즘 버전 (F29) |

**설정값을 버킷마다 저장하는 이유 (F21)** — `digest_top_n`은 10~2000 사이에서 변경 가능하다
([14 §8.9](14-infrastructure.md)). 200 → 100으로 줄이면 `_other`의 의미가 시점마다 달라지는데,
어떤 행이 어떤 설정으로 만들어졌는지 사후 판별이 불가능하면 **커버리지 추세와 순위 추세가
설정 변경과 워크로드 변화를 구분하지 못한다.**
설정 변경 시각도 `Event`(`kind=CONFIG_CHANGE`)로 남겨 리포트가 경계를 표시하게 한다.

**`_other` 행의 정의 (F6)** — 초기 설계는 "상위 N 외 롱테일"이라고만 적었고,
**임계값(100ms) 미만 다이제스트가 포함되는지가 정의되지 않았다.** 그러면 커버리지 계산의
분모가 이미 필터링된 부분집합이 되어 **커버리지가 체계적으로 과대평가**된다.

```
_other.total_time_ms = (스냅샷 전체 SUM_TIMER_WAIT 델타)
                       − (저장된 상위 N개의 total_time_ms 합)
```

즉 **임계값 미만도 반드시 `_other`에 포함**된다. 임계값은 "상위 N 후보를 고르는 필터"이고
`_other`는 "저장되지 않은 전부"다. 이렇게 정의하면 `상위 N 합 + _other = 전체`가 항상 참이다
(회귀 테스트 R9).

`_other` 행에 함께 담는 속성:

| 속성 | 의미 |
|---|---|
| `other_digest_count` | `_other`에 합산된 다이제스트 수 |
| `truncated_candidates` | 임계값은 넘었지만 상위 N에서 잘린 수 (FR-DGS-01f) |
| `total_time_all_ms` | 그 시간의 전체 총 실행시간 (분모. 검증용 중복 저장) |
| `top_n` / `threshold_ms` | 이 버킷을 만들 때 적용된 설정값 (F21) |
| `digest_algo_version` | 정규화 알고리즘 버전 (F29) |
| `partial_minutes` | 그 시간 중 실제 관측된 분 수 (F13) |

`top_n`·`threshold_ms`를 저장하는 이유: 설정을 200 → 100으로 바꾸면 `_other`의 의미가
시점마다 달라진다. 어떤 행이 어떤 설정으로 만들어졌는지 사후 판별이 가능해야
커버리지 추세와 설정 변경을 구분할 수 있다.

**볼륨** — 500대 × 24시간 × (200 + 1) ≈ **일 241만 항목**, 항목 ~300B → 일 720MB.
온디맨드 쓰기 약 $3/일. → [16-cost.md](16-cost.md)

#### AlertEvent / AlertState

```
AlertEvent   PK = AL#<date>        SK = <epoch_ms>#<fingerprint>
             GSI1PK = ALS#firing   GSI1SK = <epoch_ms>      ← 해소되면 속성 제거(희소 인덱스에서 탈락)
AlertState   PK = AS#<fingerprint> SK = STATE
```

`fingerprint = sha256(rule_id + scope_key)` 앞 16 hex.
`scope_key`는 규칙 스코프가 인스턴스면 `instance_id`, 다이제스트면 `instance_id|app_digest`.

#### Event — 인스턴스 이벤트

```
PK        EV#<instance_id>#<date_part>
SK        <kind>#<epoch_ms>#<dedup_hash>       (기본형)
          DEADLOCK#<dedup_hash>                 (데드락: 시각 제외 → 중복 자동 병합)
GSI2PK    EVK#<env>#<kind>#<date_part>
GSI2SK    <epoch_ms>#<instance_id>
```

`kind`: `LOCK_WAIT`, `DEADLOCK`, `LONG_TXN`, `REPL_LAG`, `REPL_ERROR`, `PLAN_CHANGE`,
`NEW_DIGEST`, `DIGEST_SPIKE`, `PS_RESET`, `COLLECT_FAIL`, `DIAG_FAIL`, `AI_LIMIT`.

데드락은 `SHOW ENGINE INNODB STATUS`를 읽을 때마다 같은 블록이 반복되므로,
SK에서 시각을 빼고 `dedup_hash`(데드락 텍스트 정규화 해시)만 쓴다.
`ADD count 1, SET last_at_ms = :now, if_not_exists(first_at_ms, :now)` 업데이트로 병합한다.

#### AdvisorResult

```
PK  AD#<app_digest>
SK  <instance_id>#<schema_fingerprint>#<engine_version>#<plan_fingerprint>#<prompt_version>
```

**SK에 `instance_id`와 `plan_fingerprint`가 반드시 들어간다 (F10).**
[ADR-016](03-decisions.md)은 "권고는 데이터 분포·인덱스 통계·옵티마이저 설정에 의존하므로
인스턴스가 다르면 권고도 다르다. 히트율을 50%로 낮추더라도 정확성을 택한다"고 결론냈다.
초기 데이터 모델의 SK에는 둘 다 없어서, **같은 다이제스트를 다른 인스턴스에서 조회하면
첫 인스턴스의 권고가 반환**됐다. ADR이 막으려던 오진이 데이터 모델 때문에 그대로 발생한다.

SK가 길어지는 대가는 있지만 DynamoDB SK 한도(1024바이트) 안에 충분히 들어간다.

| 속성 | 설명 |
|---|---|
| `findings` | B (zstd) — 구조화된 권고 JSON. 스키마는 [11-ai-advisor.md](11-ai-advisor.md) |
| `model_id` / `prompt_version` / `guardrail_version` | 재현성 |
| `input_tokens` / `output_tokens` / `cost_usd_estimate` | 비용 추적 |
| `literals_included` | BOOL — 리터럴을 보냈는지 (감사용) |
| `created_at_ms` / `created_by` | |
| `feedback` | `applied` / `deferred` / `rejected` + 코멘트 |
| `ttl` | 400일 |

#### Snapshot — 일별 스냅샷

```
PK  SNAP#<kind>#<instance_id>
SK  <date_part>#<object_key>
```

`kind`: `UNUSED_INDEX`, `REDUNDANT_INDEX`, `AUTOINC`, `TABLE_STATS`, `SCHEMA_FP`, `PARAM`.
`object_key`: `<schema>.<table>` 또는 `<schema>.<table>.<index>` 또는 파라미터명.
TTL 400일(추이 분석용).

#### UserRollup — 계정별 워크로드 시간 롤업 (ADR-021)

```
PK  UR#<instance_id>#<yyyy-mm>
SK  <hour_bucket>#<db_user>
ttl hour 시작 + 35일
```

| 속성 | 타입 | 설명 |
|---|---|---|
| `exec_count` / `total_time_ms` | N | 계정별 실행수·총시간 |
| `slow_count` | N | 임계값 초과 건수 |
| `by_statement_type` | M | `{SELECT: n, UPDATE: n, ...}` — 맵 경로 갱신으로 원자적 증가 |
| `rows_examined_sum` / `rows_sent_sum` | N | |
| `distinct_hosts` | SS | 그 시간에 관측된 클라이언트 호스트 (상한 20) |
| `env` | S | |

계정 수는 인스턴스당 수십 수준이므로 볼륨이 `DigestRollup`의 1% 미만이다.
계정 수가 인스턴스당 2개 이하면 이 롤업을 자동 비활성한다(설정).

#### TopDigest — 환경 전체 Top 사전 집계 (ADR-003)

```
PK  TOP#<env>#<date_part>
SK  <hour_bucket>#<rank_padded>
ttl hour 시작 + 35일
```

"환경 전체 Top 다이제스트"는 UI에서 자주 쓰이지만 500개 인스턴스 파티션 fan-out이 필요하다.
그래서 **시간 롤업 플러시 시점에 미리 집계**해 둔다.

**소유권 문제 (F9)** — 인스턴스별 지터 플러시 때문에 여러 시점에 같은 `(env, hour)` 항목을
갱신한다. 1단계는 active collector가 1대이므로 경합이 없지만, 2단계에서는 필요하다.
→ **리더 단독 재계산** 방식을 쓴다: 정시 + 90초(모든 인스턴스 지터 0~60초 완료 후)에
스케줄러 리더가 `DR#` 파티션들을 읽어 Top N을 계산하고 `TOP#`을 **덮어쓴다**.
누산 방식(`ADD`)이 아니라 재계산이므로 중복 실행이 무해하다.

#### HistogramRollup — 관찰 등록 다이제스트의 지연 분포

```
PK  HG#<instance_id>#<yyyy-mm>
SK  <hour_bucket>#<app_digest>
ttl hour 시작 + 400일   (개선 리포트가 과거 구간을 비교하므로 길게)
```

| 속성 | 타입 | 설명 |
|---|---|---|
| `bucket_counts` | S (JSON) | 비어있지 않은 버킷만 sparse 저장 `[[bucket_no, count], ...]` |
| `bucket_timer_low_ps` | S (JSON) | 버킷 경계 (서버 설정에 따라 다를 수 있어 함께 저장) |
| `exec_count` | N | 그 시간의 총 실행수 (검증용) |

`events_statements_histogram_by_digest`는 누적이므로 **사후 재구성이 불가능하다.**
그래서 관찰 등록 시점부터 영속화한다([12 §3.2.1](12-reporting.md) F3).
등록 다이제스트 수 상한: 인스턴스당 20개.

#### NotifyIntent — 발송 의도 큐

```
PK  NOTIFY
SK  <epoch_ms>#<fingerprint>
ttl 1시간   (오래된 의도는 발송 가치가 없다)
```

| 속성 | 설명 |
|---|---|
| `rule_id` / `scope_key` / `severity` / `transition` | 발화·해소 구분 |
| `payload` | S (JSON) — 렌더에 필요한 값 (리터럴 제외, T-23) |
| `claimed_by` / `claimed_at_ms` | control 워커가 조건부로 클레임 |

**소비 규칙** — control 리더가 10초마다 Query로 미클레임 항목을 읽고,
`ConditionExpression = "attribute_not_exists(claimed_by)"`로 클레임한 뒤 발송한다.
발송 성공 시 항목을 삭제한다. 실패 시 `claimed_by`를 제거해 다음 드레인에서 재시도한다
(TTL 1시간이 최종 방어선).

#### AiUsage — 토큰 사용량 누적 (F30)

```
PK  USAGE#<env>
SK  <yyyy-mm>
```

| 속성 | 설명 |
|---|---|
| `input_tokens` / `output_tokens` | `ADD`로 원자 증가 |
| `call_count` / `cost_usd_estimate` | 동일 |
| `budget_input_tokens` | 환경별 예산 (설정에서 복사) |

**`AdvisorResult`를 스캔해 합산하면 안 된다.** `PK = AD#<app_digest>`이므로
"이번 달 환경별 합계"가 전체 스캔이 되고, [ADR-003](03-decisions.md)이 금지한 접근이다.
→ 실행 전 조건부 검사(`input_tokens + est <= budget`)로 동시 실행의 예산 초과도 막는다.

#### MetricRollup — 자체 수집 지표 시간 롤업

```
PK  MR#<instance_id>#<yyyy-mm>
SK  <hour_bucket>
```

`metrics`: M — `{qps: {avg, max}, threads_running: {avg, max}, repl_lag_s: {avg, max}, ...}`
CloudWatch에 없고 리포트에 필요한 것만 담는다(FR-OBS-05 델타 지표 중 선별).
500대 × 24 × 30 = 월 36만 항목. TTL 400일.

## 3. DynamoDB 테이블 `dbmon-config`

작고(수천 항목) 읽기 편중이며, 데이터 테이블과 수명주기·백업 정책이 다르므로 분리한다.

| 엔티티 | PK | SK | 비고 |
|---|---|---|---|
| Instance | `INST` | `<region>#<instance_id>` | 단일 파티션 Query로 전체 목록. 500항목이라 문제없음 |
| InstanceOverride | `INSTOVR` | `<instance_id>` | 사용자가 수동 지정한 env·수집여부 (태그 재수집에도 유지) |
| Setting | `CFG` | `GLOBAL` / `ENV#<env>` / `INST#<instance_id>` | 3계층 병합 |
| AlertRule | `RULE` | `<rule_id>` | |
| Mute | `MUTE` | `<mute_id>` | ttl |
| Channel | `CHAN` | `<channel_id>` | Secrets Manager ARN **참조만** 저장 |
| ShardLease | `LEASE` | `SHARD#<shard_key>` | ttl 60s, owner, epoch |
| LeaderLease | `LEASE` | `LEADER#<job_group>` | ttl 30s |
| Checkpoint | `CKPT` | `<job_name>` | 아카이브·백필 재개 지점 |
| UserScope | `USER` | `<cognito_sub>` | 환경 스코프 제한 |
| Audit | `AUDIT#<yyyy-mm>` | `<epoch_ms>#<uuid>` | ttl 400일. 아카이브 대상 |
| DiagResult | `DIAG` | `<instance_id>` | 자가진단 결과 캐시 |
| BootstrapState | `BSTATE` | `<instance_id>` | 부트스트랩 상태 (F23) |

**`BootstrapState` (F23)** — 10대 중 5대 성공 후 실패하거나 브라우저가 닫히거나 ALB 유휴
타임아웃(300초)을 넘기면, 초기 설계에서는 **감사 로그와 사라진 WS 스트림만 남았다.**
[09 §3.5](09-frontend.md)의 "미부트스트랩만 보기" 필터가 참조할 상태 필드가 없었고,
`bootstrap_progress.job_id`는 존재하지 않는 잡 레코드를 가리켰다.

| 속성 | 값 |
|---|---|
| `state` | `none` / `ok` / `failed` / `partial` |
| `checked_at_ms` | 마지막 검증 시각 |
| `last_error_code` | `denied` / `iam_auth_disabled` / `plugin_mismatch` / `excess_privileges` / `unreachable` |
| `auth_method` | `iam` / `password` |
| `privilege_mode` | `broad` / `least` / `minimal` |
| `granted_schemas` | SS |
| `last_job_id` / `last_actor` | 추적용 |

**자격증명을 담지 않는다.** 상태 판정은 권한 diff 조회 결과로만 갱신하므로
"비밀을 저장하지 않는다"는 원칙과 충돌하지 않는다.
`ok` 판정은 자가진단이 통과했을 때만 내린다(부트스트랩 SQL 성공과 다르다).

**설정 3계층 병합** — `GLOBAL` → `ENV#<env>` → `INST#<id>` 순서로 얕은 병합.
워커는 30초마다 폴링해 `watch` 채널로 반영한다(재시작 없이 적용, FR-OPS-05).

## 4. S3 Tables (Iceberg) — 네임스페이스 `dbmon`

### 4.1 테이블 목록

| 테이블 | 파티셔닝 | 정렬 | 보관 |
|---|---|---|---|
| `slow_queries` | `day(started_at)` | `instance_id, app_digest` | 1년 |
| `digest_rollups` | `day(hour_ts)` | `instance_id, app_digest` | 1년 |
| `digest_texts` | 없음(작은 차원 테이블) | `app_digest` | 무제한 |
| `advisor_results` | `month(created_at)` | `app_digest` | 1년 |
| `alert_events` | `day(event_ts)` | `env, rule_id` | 1년 |
| `instance_events` | `day(event_ts)` | `instance_id, kind` | 1년 |
| `metric_rollups` | `day(hour_ts)` | `instance_id` | 1년 |
| `daily_snapshots` | `day(snapshot_date)` | `instance_id, kind` | 1년 |
| `audit_log` | `month(event_ts)` | `actor` | 3년 |

`digest_texts`는 조인 대상 차원 테이블이라 파티션을 두지 않는다(전체가 수십 MB).

### 4.2 DDL 예 — `slow_queries`

```sql
CREATE TABLE "s3tablescatalog/dbmon-tables-<acct>"."dbmon"."slow_queries" (
  record_id            string,
  started_at           timestamp,
  ended_at             timestamp,
  captured_at          timestamp,
  instance_id          string,
  cluster_id           string,
  region               string,
  env                  string,
  engine               string,
  engine_version       string,
  thread_id            bigint,
  schema_name          string,
  db_user              string,
  db_host              string,
  duration_ms          bigint,
  duration_source      string,
  sql_text             string,          -- 평문. LIKE 검색 대상
  sql_text_truncated   boolean,
  literal_policy       string,
  app_digest           string,
  digest_algo_version  int,
  mysql_digest         string,
  statement_type       string,
  rows_examined        bigint,
  rows_sent            bigint,
  rows_affected        bigint,
  lock_time_ms         bigint,
  tmp_tables           bigint,
  tmp_disk_tables      bigint,
  sort_merge_passes    bigint,
  no_index_used        boolean,
  no_good_index_used   boolean,
  full_join            boolean,
  plan_zstd            binary,          -- 불투명 블롭. 앱만 해제
  plan_s3_key          string,
  plan_format_version  string,
  plan_tree            string,
  plan_source          string,
  plan_error           string,
  plan_fingerprint     string,
  referenced_tables    array<string>,
  capture_source       string,
  schema_version       int
)
PARTITIONED BY (day(started_at))
TBLPROPERTIES (
  'table_type' = 'ICEBERG',
  'write_compression' = 'zstd',
  'format' = 'parquet'
);
```

`digest_rollups`:

```sql
CREATE TABLE "s3tablescatalog/dbmon-tables-<acct>"."dbmon"."digest_rollups" (
  hour_ts              timestamp,
  instance_id          string,
  cluster_id           string,
  env                  string,
  app_digest           string,
  schema_name          string,
  exec_count           bigint,
  total_time_ms        bigint,
  avg_time_ms          double,
  min_time_ms          bigint,
  max_time_ms          bigint,
  total_lock_time_ms   bigint,
  rows_examined_sum    bigint,
  rows_sent_sum        bigint,
  rows_affected_sum    bigint,
  tmp_tables_sum       bigint,
  tmp_disk_tables_sum  bigint,
  sort_merge_sum       bigint,
  no_index_used_count  bigint,
  no_good_index_count  bigint,
  full_join_count      bigint,
  errors_sum           bigint,
  warnings_sum         bigint,
  other_digest_count   int,      -- app_digest='_other' 행에만 채움
  truncated_candidates int,      -- 임계값은 넘었지만 상위 N에서 잘린 수 (FR-DGS-01f)
  total_time_all_ms    bigint,   -- 그 시간의 전체 총 실행시간 (커버리지 분모, F6)
  partial              boolean,
  partial_minutes      int,      -- 실제 관측된 분 수 (F13)
  flush_failed         boolean,  -- (F20)
  reset_detected       boolean,
  top_n                int,      -- 이 버킷 생성 시 설정값 (F21)
  threshold_ms         bigint,
  digest_algo_version  int,      -- (F29)
  schema_version       int
)
PARTITIONED BY (day(hour_ts))
TBLPROPERTIES ('table_type'='ICEBERG', 'write_compression'='zstd', 'format'='parquet');
```

나머지 테이블 DDL은 DynamoDB 엔티티 속성과 1:1 대응하므로 생략한다.

**스키마 변경은 3단계 순서를 지킨다 (F26)** — 롤링 배포 중에는 구·신 워커가 동시에 쓴다.
신 워커가 추가한 속성은 (a) 언네스팅 뷰가 열거하지 않으면 **조용히 버려지고**,
(b) `MERGE`의 컬럼 목록과 어긋나면 **잡이 실패한다.**

```
1단계: Iceberg 테이블에 컬럼 추가 (nullable 필수)
         ALTER TABLE ... ADD COLUMN new_col bigint
       → 구 파티션은 NULL. 구 워커의 쓰기에 영향 없음

2단계: 언네스팅 뷰와 MERGE 문을 옵셔널로 확장
         뷰: element_at(NewImage, 'new_col') 로 결측 허용
             (map 접근이므로 키가 없어도 NULL 반환)
         MERGE: 새 컬럼을 INSERT/UPDATE 목록에 추가
       → 이 시점에는 아무도 그 속성을 쓰지 않으므로 항상 NULL. 무해

3단계: 앱 배포 (새 속성 쓰기 시작 + schema_version 상향)
       → 이미 뷰·MERGE가 준비돼 있으므로 롤링 배포 중에도 안전
```

**아카이브 잡이 시작 시 검증한다** — 해당 구간 항목의 `schema_version` 최대값과 Iceberg
컬럼 집합을 비교해 불일치하면 **조기 실패 + 알림**한다. 조용히 버리는 것보다 낫다.

**컬럼 삭제·타입 변경은 하지 않는다.** 롤백이 불가능해진다([14 §6.5](14-infrastructure.md)).

### 4.3 적재 SQL — 증분 내보내기 언네스팅

**증분 내보내기의 포맷은 전체 내보내기와 다르다.** 전체 내보내기는 `{"Item":{...}}` 한 줄이지만,
증분 내보내기는 변경 레코드 형태다.

```json
{"Metadata":{"WriteTimestampMicros":"1755527391000000"},
 "Keys":{"PK":{"S":"SQ#ap-northeast-2/orders-prd-01#2026-08-18"},
         "SK":{"S":"1755500591000#8842119"}},
 "NewImage":{"PK":{"S":"..."},"SK":{"S":"..."},
             "duration_ms":{"N":"4213"},
             "plan_json":{"B":"KLUv..."},
             "no_index_used":{"BOOL":true},
             "referenced_tables":{"S":"[\"shop.orders\"]"}}}
```

| 변경 유형 | 출력 |
|---|---|
| insert / update | `Keys` + `NewImage` (+ `OldImage`, view type 설정에 따라) |
| delete | `Keys` (+ `OldImage`) — **`NewImage` 없음** |
| 같은 구간 안에서 insert 후 delete | **출력 없음** |

내보내기 `ExportViewType`은 `NEW_IMAGE`로 고정한다. `NEW_AND_OLD_IMAGES`는 페이로드가 2배가
되고 아카이브에 필요하지 않다.

#### 4.3.1 매니페스트를 반드시 읽어야 한다

증분 내보내기의 **데이터 파일은 실행별 폴더에 들어가지 않는다.**

```
s3://dbmon-raw/exports/AWSDynamoDB/
  ├── <ExportId-1>/manifest-summary.json
  ├── <ExportId-1>/manifest-files.json      ← 이번 실행의 파일 목록이 여기에만 있다
  ├── <ExportId-2>/manifest-summary.json
  ├── <ExportId-2>/manifest-files.json
  └── data/                                  ← 모든 실행이 공유하며 누적된다
        ├── abc123.json.gz
        ├── def456.json.gz
        └── ...
```

프리픽스를 통째로 외부 테이블로 걸면 **매일 과거 전량을 재스캔**한다(비용이 누적 선형으로
증가) 그리고 `manifest-*.json`·`_started` 같은 비데이터 파일까지 파싱 대상이 된다.

**적재 잡 절차**

```
1. ExportTableToPointInTime(ExportType=INCREMENTAL_EXPORT,
                            ExportViewType=NEW_IMAGE,
                            IncrementalExportSpecification={ExportFromTime, ExportToTime},
                            S3Bucket=dbmon-raw, S3Prefix=exports/)
2. DescribeExport 폴링 → COMPLETED → ExportManifest 경로 획득
3. manifest-files.json 을 읽어 dataFileS3Key 목록 추출
4. 그 파일들을 s3://dbmon-raw/staging/<run_date>/ 로 복사 (CopyObject, 병렬)
5. Athena 외부 테이블의 파티션을 staging/<run_date> 로 등록
6. MERGE INTO 실행
7. 성공 시 체크포인트 갱신 + staging/<run_date> 삭제
```

- 4단계 복사는 파일 수가 수십~수백 개이므로 비용·시간이 무시할 수준이다.
  복사 대신 매니페스트 기반 `WHERE "$path" IN (...)` 필터도 가능하지만, 경로 목록이 길어지면
  쿼리 크기 한도에 걸린다. 복사가 단순하다.
- `AWSDynamoDB/data/`는 **7일 Lifecycle로 만료시킬 수 없다** — 여러 실행이 공유하는 폴더이고
  DynamoDB가 관리한다. `staging/`만 잡이 직접 삭제하고, `AWSDynamoDB/`는 90일 Lifecycle로
  둔다(매니페스트 감사 추적 겸용).

#### 4.3.2 외부 테이블과 뷰

```sql
-- 1) staging 외부 테이블 (Glue, JSON SerDe)
CREATE EXTERNAL TABLE dbmon_raw.ddb_incremental (
  Metadata  map<string,string>,
  Keys      map<string, map<string,string>>,
  NewImage  map<string, map<string,string>>
)
PARTITIONED BY (run_date string)
ROW FORMAT SERDE 'org.openx.data.jsonserde.JsonSerDe'
WITH SERDEPROPERTIES ('ignore.malformed.json'='true')
LOCATION 's3://dbmon-raw/staging/';

-- 2) 슬로우 쿼리만 골라 타입 캐스팅
CREATE OR REPLACE VIEW dbmon_raw.v_slow_queries AS
SELECT
  NewImage['record_id']['S']                                        AS record_id,
  from_unixtime(CAST(NewImage['started_at_ms']['N'] AS bigint)/1000) AS started_at,
  NewImage['instance_id']['S']                                      AS instance_id,
  NewImage['env']['S']                                              AS env,
  CAST(NewImage['duration_ms']['N'] AS bigint)                       AS duration_ms,
  NewImage['sql_text']['S']                                         AS sql_text,
  NewImage['app_digest']['S']                                       AS app_digest,
  from_base64(NewImage['plan_json']['B'])                            AS plan_zstd,
  CAST(NewImage['no_index_used']['BOOL'] AS boolean)                 AS no_index_used,
  CAST(json_parse(NewImage['referenced_tables']['S']) AS array(varchar)) AS referenced_tables
  -- ... 나머지 컬럼 동일 패턴
FROM dbmon_raw.ddb_incremental
WHERE run_date = ?                       -- 이번 실행 파티션만
  AND NewImage IS NOT NULL               -- 삭제 레코드 제외
  AND Keys['PK']['S'] LIKE 'SQ#%';

-- 3) 멱등 적재
MERGE INTO "s3tablescatalog/dbmon-tables-<acct>"."dbmon"."slow_queries" AS t
USING dbmon_raw.v_slow_queries AS s
ON t.record_id = s.record_id
WHEN MATCHED THEN UPDATE SET
  duration_ms = s.duration_ms, plan_zstd = s.plan_zstd /* ... */
WHEN NOT MATCHED THEN INSERT (record_id, started_at, /* ... */)
                      VALUES (s.record_id, s.started_at, /* ... */);
```

#### 4.3.3 주의점

1. **`Item` 맵의 값 타입이 이종이다.** `map<string, map<string,string>>`로 받으면
   `L`(리스트)·`M`(맵) 같은 중첩 타입을 문자열로 못 받는다.
   → **불변 배열은 JSON 문자열(`S`)로 저장한다.** 대상: `referenced_tables`, `metrics`,
   `findings`, `bucket_counts`. 이들은 레코드 생성 시 한 번 쓰고 변하지 않는다.
   Athena에서 `json_parse`로 되살린다.

   **⚠ 누적 갱신되는 맵·집합은 JSON 문자열로 저장하면 안 된다 (F8).**
   `DigestText.mysql_digests`, `seen_instances`, `seen_users`, `seen_hosts`는 여러 경로가
   **동시에 항목을 추가**한다(`detect` 경로와 `digest` 경로가 같은 `DT#<app_digest>`를
   upsert하는 것이 정상 동작이다). JSON 문자열이면 갱신이 read-modify-write가 되어
   DynamoDB의 `ADD`(SS)·맵 경로 업데이트를 쓸 수 없고, **서로의 엔트리를 잃는다.**
   `mysql_digest ↔ app_digest` 학습 매핑(M4-16)이 조용히 소실된다.
   2단계 다중 워커에서는 상시 발생한다.

   → 이들은 **네이티브 타입으로 저장**한다.

   | 속성 | 타입 | 갱신 방식 |
   |---|---|---|
   | `seen_instances`, `seen_users`, `seen_hosts` | `SS` (String Set) | `ADD` — 원자적 |
   | `mysql_digests` | `M` (Map) | `SET mysql_digests.#inst = :digest` — 경로 갱신, 원자적 |
   | `referenced_tables`, `metrics`, `findings`, `bucket_counts` | `S` (JSON) | 생성 시 1회 쓰기 |

   대가: 아카이브 언네스팅 SQL에서 `SS`/`M`을 다뤄야 한다. raw 외부 테이블 정의에서
   해당 키만 별도 타입으로 명시한다(`map<string, struct<SS:array<string>>>` 등).
   **정확성이 SQL 편의보다 우선한다.**
2. **삭제 레코드 필터** — TTL 만료·삭제 항목은 `NewImage`가 없다. `NewImage IS NOT NULL`로
   걸러 아카이브에 반영하지 않는다(핫 티어의 TTL은 아카이브의 보관 정책과 무관하다).
3. **`WHEN MATCHED THEN UPDATE`가 필요한 이유** — 슬로우 쿼리 레코드는 확정 후에도 갱신될 수
   있다(진행 중 선행 저장 → 확정, 사후 플랜 폴백, 슬로우로그 병합). 같은 `record_id`가 여러
   내보내기 구간에 나타나므로 UPDATE 분기가 필요하다.
4. **S3 Tables의 Athena 카탈로그 이름은 테이블 버킷 이름을 포함한다.**
   `"s3tablescatalog/<table-bucket-name>"."<namespace>"."<table>"` 형태이며,
   3파트 `s3tablescatalog.dbmon.slow_queries`로는 해석되지 않는다.
   → [OPEN-Q-03](OPEN-QUESTIONS.md)에서 실제 형태를 확정한다.
5. **최초 1회는 전체 내보내기**로 기준선을 만든다. 그때는 `{"Item":{...}}` 포맷이므로
   별도 뷰(`v_slow_queries_full`)가 필요하다. 1회성이므로 잡에 두지 않고 수동 SQL로 처리한다.

### 4.4 보관 만료

```sql
-- 월 1회
DELETE FROM "s3tablescatalog/dbmon-tables-<acct>"."dbmon"."slow_queries"
WHERE started_at < date_add('year', -1, current_timestamp);
```

S3 Tables의 관리형 유지보수(컴팩션, 스냅샷 만료, 미참조 파일 정리)를 활성화한다.
Iceberg `DELETE`는 삭제 파일을 만들 뿐이므로, 스냅샷 만료가 켜져 있어야 실제 스토리지가 준다.

S3 raw 버킷은 Lifecycle 7일 만료.

## 5. 조회 라우팅 규칙

```
requested_range = [from, to]
hot_boundary    = now - 31d          (환경별 설정 가능, FR-STO-11)

to   >= hot_boundary  &&  from >= hot_boundary   → DynamoDB 단독
to   <  hot_boundary                             → Athena 단독
그 외 (경계 교차)                                 → 둘 다 조회 후 record_id 로 dedup 병합
```

경계 교차 시 중복이 나오는 이유: 아카이브는 파생이므로 최근 며칠 데이터가 양쪽에 모두 있다.
`record_id`로 dedup하고, 충돌 시 DynamoDB 쪽을 우선한다(더 최신 상태).

## 6. 데이터 크기 추정 (500 인스턴스 기준)

| 데이터 | 일 건수 | 항목 크기 | 일 볼륨 | Iceberg 연간(압축) |
|---|---|---|---|---|
| SlowQuery | 50,000 | 8KB | 400MB | ~30GB |
| DigestRollup | 2,410,000 | 300B | 720MB | ~120GB |
| DigestText | 5,000(신규) | 2KB | 10MB | ~1GB |
| MetricRollup | 12,000 | 1KB | 12MB | ~2GB |
| Event | 10,000 | 1KB | 10MB | ~2GB |
| Snapshot | 250,000 | 300B | 75MB | ~12GB |
| AlertEvent | 500 | 1KB | 0.5MB | ~0.1GB |
| **합계** | | | **~1.2GB/일** | **~170GB/년** |

DynamoDB 저장량은 TTL 35일 기준 **항목 데이터 약 42GB**이며, 항목당 오버헤드(약 100B)와
GSI 사영을 더하면 **약 60GB**다([16 §2.2](16-cost.md)).
슬로우 쿼리 5만 건/일은 보수적 상한이다(인스턴스당 100건). 실제로는 이보다 훨씬 적을 것이며,
다이제스트 롤업이 볼륨의 대부분을 차지한다 → 상위 N 값이 비용의 주 조절 손잡이다.

## 7. 검증 항목 (테스트로 강제)

| 항목 | 테스트 |
|---|---|
| `record_id` 결정론성 | 같은 입력 → 같은 ID (프로퍼티 테스트) |
| `app_digest` 수렴성 | `app_digest(raw) == app_digest(DIGEST_TEXT(raw))` (프로퍼티 테스트) |
| 키 인코딩 왕복 | `record_id` → PK/SK → `record_id` 복원 |
| `dur_bucket` 경계 | 2000ms, 5000ms, 15000ms, 60000ms 정확히 어느 버킷인지 |
| 다중 버킷 병합 정렬 | 4개 버킷 결과 병합이 전역 시간 정렬과 일치 |
| 리셋 감지 | `COUNT_STAR` 감소 입력 시 델타가 음수가 되지 않음 |
| `_other` 총량 보존 | 상위 N + `_other` 합 == 전체 합 |
| MERGE 멱등성 | 같은 구간 2회 적재 후 행수 불변 (통합 테스트) |
| TTL 계산 | 보관 정책 변경이 TTL에 정확히 반영 |
| 300KB 오프로드 임계 | 경계 크기에서 S3 오프로드 분기 동작 |
| **오프로드 수명주기 불변식** | 항목 TTL ≤ 참조 S3 객체 Lifecycle. `SLOWEST` 갱신 시 `ttl400/` 복사 확인 (F27) |
| **맵·집합 원자 갱신** | 두 경로가 동시에 `DT#` 를 upsert → 양쪽 엔트리 모두 보존 (F8) |
| **`_other` 총량 보존** | `상위 N 합 + _other = total_time_all_ms` (임계값 미만 포함, F6) |
| **`in_flight` 고아 정리** | `last_seen_at_ms` 초과 → `abandoned` 확정 (F4) |
