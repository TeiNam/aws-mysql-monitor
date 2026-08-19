# 19. M1 검증 스파이크 실측 결과

측정 환경 — 로컬 `docker compose` ([docker-compose.yml](../docker-compose.yml))

| 컨테이너 | 버전 | 특이 설정 |
|---|---|---|
| `mysql84` | 8.4.11 | 기본값 (`max_digest_length=1024`, `max_sql_text_length=1024`) |
| `mysql84-wide` | 8.4.11 | `max_digest_length=4096`, `performance_schema_max_sql_text_length=8192` |
| `mysql80` | 8.0.46 | 기본값. Aurora MySQL 3.x 의 커뮤니티 기반 |

측정 코드는 `cargo test` 로 재현 가능하고, **결과를 단정문으로 고정**해 두었다.
MySQL 동작이 바뀌면 테스트가 깨지고 그때 설계를 되돌린다.

```
docker compose up -d
cargo test -p dbmon --test m1_digest  -- --nocapture --test-threads=1
cargo test -p dbmon --test m1_capture -- --nocapture --test-threads=1
```

---

## A. SQL 텍스트 절단 (M1-1, [OPEN-Q-07](OPEN-QUESTIONS.md)) — **부분 확인 + 사실 정정**

| 목표 SQL 길이 | 실제 | `performance_schema.processlist.INFO` | `information_schema.PROCESSLIST.INFO` | `events_statements_current.SQL_TEXT` |
|---|---|---|---|---|
| 4 KB | 4,097 | 1,024 | **4,097** | 1,024 |
| 16 KB | 16,387 | 1,024 | **16,387** | 1,024 |
| 64 KB | 65,541 | 1,024 | **65,535** ⚠ | 1,024 |
| 1 MB | 1,048,577 | 1,024 | **65,535** ⚠ | 1,024 |

### 정정: `information_schema.PROCESSLIST.INFO` 는 `LONGTEXT` 가 아니다

[05 §2.2](05-collector.md)는 "`INFO` 는 `LONGTEXT` 이며 절단되지 않는다"고 적었다. **사실이 아니다.**

```
information_schema.PROCESSLIST.INFO   varchar(21845)   → LENGTH() 기준 65,535바이트
performance_schema.processlist.INFO   longtext         → 내용이 1,024바이트로 잘린다
```

두 컬럼의 제약 방향이 **반대**다. `information_schema` 쪽은 컬럼 타입이 좁고,
`performance_schema` 쪽은 타입은 무제한이지만 내용을 서버가 자른다
(`performance_schema_max_sql_text_length`).

21845 × 3바이트(utf8mb3) = 65535 이므로 이 상한은 컬럼 정의에서 온 것이며 설정으로 바뀌지 않는다.

### ADR-005 는 유효하다 — 단서를 달아서

1세대 버그(1,024바이트 절단)를 고치는 방법으로서 **64배 개선**이며, 실무 SQL 길이 분포를
생각하면 사실상 전부를 커버한다. 다만:

- **65,535바이트를 넘으면 잘린다** → `sql_text_truncated = true` 를 반드시 세운다.
  판정은 "받은 길이가 정확히 65,535" 로 한다(상한이 컬럼 정의라 값이 고정이다).
- 폴백(파라미터 그룹에서 `performance_schema_max_sql_text_length` 상향)은 **동작한다**:
  1024 → 8192 로 올린 컨테이너에서 `SQL_TEXT` 가 8,192바이트를 반환했다.
  단 이 경로도 `information_schema` 의 65,535 상한을 넘지는 못한다.

**반영 대상**: [05 §2.2](05-collector.md), [ADR-005](03-decisions.md),
[04 §2.3](04-data-model.md)(`sql_text_truncated` 판정 기준), [OPEN-Q-07](OPEN-QUESTIONS.md).

---

## B. `EXPLAIN ... FOR CONNECTION` (M1-2/3/4) — **ADR-006 이 깨진다**

### 권한 요구가 문서보다 훨씬 세다

타인 커넥션을 explain 할 때의 실측 결과 (8.4.11 · 8.0.46 동일):

| 권한 조합 | 결과 |
|---|---|
| `PROCESS` + 스키마 `SELECT` (권한 모드 B) | ✗ `ERROR 1045` |
| `PROCESS` + `SELECT ON *.*` | ✗ 1045 |
| `PROCESS` + `SUPER` | ✗ 1045 |
| `PROCESS` + `SUPER` + `SELECT ON *.*` | ✗ 1045 |
| RDS 마스터 유사 (ALL − SUPER/FILE/SHUTDOWN/CREATE TABLESPACE) | ✗ 1045 |
| **`GRANT ALL ON *.*`** (정적 전역 권한 전체) | ✓ |
| `GRANT ALL` − 정적 권한 1개 (`EVENT`·`SHUTDOWN`·`FILE`…) | ✗ 1045 |
| `GRANT ALL` − 동적 권한 1개 (`AUDIT_ADMIN`) | ✓ |
| **자기 커넥션**을 explain | ✓ (권한 무관) |

→ **정적 전역 권한 전체**가 필요하다. 동적 권한은 무관하다.
대상 테이블이 없는 문장(`SELECT SLEEP(20)`)에서도 같은 결과이므로 **테이블 권한 문제가 아니다.**

MySQL 버그 [#95850](https://bugs.mysql.com/bug.php?id=95850) 에서 개발자가
"`PROCESS` 만으로는 부족하다"고 인정했다 — 그 버그의 요지가 "문서가 부정확하다"는 것이다.

### 결과: RDS 에서는 어떤 계정으로도 불가능하다

RDS 는 마스터 유저에게도 `SUPER`·`FILE`·`SHUTDOWN` 을 주지 않는다. 위 표의 5행이 그 조건이고
실패한다. **[ADR-006](03-decisions.md)의 "탐지 시점 `EXPLAIN FOR CONNECTION`" 은
RDS·Aurora 에서 채택할 수 없다.**

### 폴백은 동작한다 (M1-4b)

읽기 전용 계정(`PROCESS` + 스키마 `SELECT`)으로 측정:

| 경로 | 결과 |
|---|---|
| `EXPLAIN FORMAT=JSON SELECT …` (원문 재실행) | ✓ `access_type=ALL, rows=60023` |
| `EXPLAIN FORMAT=JSON UPDATE/DELETE/INSERT …` | ✗ `ERROR 1142` (해당 DML 권한 필요) |
| `UPDATE … WHERE c` → `EXPLAIN SELECT * FROM t WHERE c` | ✓ 같은 접근 경로 (`ALL`, 60023행) |
| `DELETE FROM t WHERE c` → `EXPLAIN SELECT * FROM t WHERE c` | ✓ (`ref`, 2행) |

`EXPLAIN UPDATE` 는 데이터를 **변경하지 않는다**(실측: `memo` 가 NULL 그대로).
그래도 `UPDATE` 권한을 요구하므로, 읽기 전용 원칙을 지키려면 **DML 을 SELECT 로 변환**해야 한다.

### 설계 변경

`plan_source` 를 4단계로 바꾸고 우선순위를 뒤집었다
([`core::slow_query::PlanSource`](../crates/core/src/slow_query.rs)):

```
none  <  rerun_as_select  <  rerun  <  for_connection
                              ↑ 실질 기본 경로 (RDS)
                                           ↑ 자체 관리 MySQL 에서만
```

- `rerun_as_select` 는 **근사 플랜**이다. 행을 찾아가는 접근 경로는 같지만 쓰기 단계
  (보조 인덱스 갱신·트리거·외래키 검사)는 나타나지 않는다. UI 에 "근사" 배지를 붙인다
  (`PlanSource::is_exact() == false`).
- `plan_error=denied` 를 **자가진단 실패로 승격하지 않는다.** RDS 에서는 상시 발생하며
  사용자가 고칠 수 없다. 초기 설계는 이걸 "`PROCESS` 미부여"로 해석해 승격시켰는데,
  그러면 **모든 RDS 인스턴스가 영구 자가진단 실패**로 표시된다.

### 잃는 것을 정확히 적는다

[README](../README.md)의 "1세대 대비 개선" 표는 "실행 중 플랜 확보, DML 커버"라고 적었다.
이제 정확히는:

| 항목 | 1세대 | 2세대 (실측 후) |
|---|---|---|
| SELECT 플랜 | 사후 재실행 | 사후 재실행 (**같다**) |
| DML 플랜 | 없음 | `WHERE` 절을 SELECT 로 변환한 **근사** 플랜 |
| 실행 중 실제 플랜 | 없음 | 자체 관리 MySQL 에서만 |

DML 플랜은 여전히 1세대보다 낫지만 "실행 중 실제 플랜"은 RDS 에서 얻을 수 없다.

**반영 대상**: [ADR-006](03-decisions.md), [05 §2.4](05-collector.md),
[04 §2.3](04-data-model.md)(`plan_source` enum), [07 §2.3](07-credentials-bootstrap.md),
[09](09-frontend.md)(플랜 출처 배지에 "근사" 추가), [OPEN-Q-06](OPEN-QUESTIONS.md).

### 부수 확인

| 항목 | 결과 |
|---|---|
| `EXPLAIN FORMAT=TREE FOR CONNECTION` ([OPEN-Q-05](OPEN-QUESTIONS.md)) | **지원됨** (8.4.11). SELECT·UPDATE·DELETE 모두 |
| `EXPLAIN FOR CONNECTION` (TRADITIONAL) | 지원됨 |
| 연결 ID 를 식(`CONNECTION_ID()`)으로 | ✗ `ERROR 1064` 구문 오류 → **리터럴 정수만** (05 §2.4 확인) |
| 존재하지 않는 커넥션 | `1094` → `ThreadGone` |
| `EXPLAIN` 불가 문장 (`DO SLEEP(4)`) | `3012` → `NotExplainable` |
| **유휴 커넥션** | **에러가 아니라 빈 결과** → `NoStatement` 로 별도 분류. 에러로 세면 정상을 실패로 센다 |
| DML 플랜 파싱 | `planparse` 가 노드·참조 테이블 추출 성공 (`shop.orders`, `shop.lock_arena`) |

---

## C. 다이제스트 정규화 (M1-6) — 규칙표 11건 정정

골든 코퍼스 **131건 전량 수렴** (`normalize(원문) == normalize(DIGEST_TEXT)`, 불일치 0).
가설이었던 [05 §3.2](05-collector.md) 규칙표를 실측으로 고쳤다.

### C-1. 식별자 대소문자 접기 (규칙 8 정정)

초기 규칙은 "식별자는 원본 대소문자 유지"였다. 그러면 **비예약어 키워드가 갈라진다**:

| 입력 | 원문 정규화 | `DIGEST_TEXT` 정규화 |
|---|---|---|
| `date '2026-01-01'` | `date` | `DATE` |
| `count(*)` | `count` | `COUNT` |

MySQL 은 문법 키워드를 대문자로 올리고 식별자에는 백틱을 붙인다. 그 구분은 **파스 위치**에
달렸으므로 렉서로는 불가능하다. → **식별자를 소문자로 접는다.**

대가: `app_digest` 가 `mysql_digest` 보다 거칠어진다(§D).

### C-2. 괄호 축약이 `IN`/`VALUES` 전용이 아니다

MySQL 실측:

| 입력 | `DIGEST_TEXT` |
|---|---|
| `f(1,2)` · `IN (1)` · `VALUES (1,2)` | `f (...)` — 괄호 안이 **전부** 리터럴 |
| `f(memo,1,2)` | `f ( \`memo\` , ?, ... )` — 리터럴 **2개 이상 연속** |
| `LIMIT 5, 10` | `LIMIT ?, ...` — 괄호 밖에도 적용 |
| `VALUES (1,2),(3,4)` | `VALUES (...) /* , ... */` — **주석**으로 표시 |

→ 두 규칙을 구현했다: ① 리터럴만인 괄호 그룹 → `( ... )`, ② 리터럴 2개 이상 연속 → `? , ...`.
다중 행 `VALUES` 는 MySQL 이 주석으로 표시하고 우리는 주석을 제거하므로 자연히 수렴한다.

### C-3. 힌트 내부도 토큰화된다

| 입력 | `DIGEST_TEXT` |
|---|---|
| `/*+ MAX_EXECUTION_TIME(1000) */` | `/*+ MAX_EXECUTION_TIME (?) */` |
| `/*+ NO_ICP(orders) */` | `/*+ NO_ICP ( \`orders\` ) */` |
| `/*+ SET_VAR(sort_buffer_size = 16M) */` | `/*+ SET_VAR ( \`sort_buffer_size\` = ? ) */` |

초기 설계는 힌트를 원문 그대로 보존했다. → 힌트 내부를 재귀 정규화하고,
크기 접미(`16M`)를 하나의 리터럴로 흡수한다.

### C-4. 캐릭터셋 introducer

`_utf8mb4'한글'` → `( _charset ) ?`. `N'..'` · `X'..'` · `b'..'` 는 그냥 `?` 다.

### C-5. 동의어 통일

| 입력 | `DIGEST_TEXT` |
|---|---|
| `DISTINCT` | `DISTINCTROW` |
| `<>` | `!=` |
| `CHAR` (CAST) | `CHARACTER` |
| `INT` (CAST) | `INTEGER` |
| `REGEXP` | `RLIKE` |
| `SUBSTR` · `MID` | `SUBSTRING` |
| `DATABASE` | `SCHEMA` |
| `CURRENT_TIMESTAMP` · `LOCALTIME` · `LOCALTIMESTAMP` | `NOW` |
| `CURRENT_DATE` | `CURDATE` |
| `CURRENT_TIME` | `CURTIME` |
| `INTERVAL ? DAY` | `INTERVAL ? SQL_TSI_DAY` |

`sql_tsi_` 는 **접두를 벗기는** 쪽으로 통일했다. `DAY(date)` 함수는 `day` 이고
`INTERVAL ? DAY` 는 `sql_tsi_day` 인데 이 구분도 문맥 의존이다.

### C-6. 예약어 집합의 오류는 비대칭이다

| 실수 | 결과 |
|---|---|
| 예약어 **누락** | 양쪽이 똑같이 소문자로 접히므로 **수렴한다.** 무해 |
| 비예약어 **과잉** 포함 | 원문 `status` → `STATUS`, `DIGEST_TEXT` `` `status` `` → `status` → **갈라진다** |

→ 확신하는 예약어만 넣는다. `STATUS`·`DATE`·`TIMESTAMP`·`COUNT`·`USER`·`SOURCE` 같은
비예약어는 절대 넣지 않는다.

**반영 대상**: [05 §3.2](05-collector.md) 규칙표 전면 교체, [ADR-011](03-decisions.md).

---

## D. `app_digest` 와 `mysql_digest` 의 관계 (M1-6b) — **`app_digest` 1 : `mysql_digest` N**

[04 §2.3](04-data-model.md)의 `DigestText.mysql_digests` 는 `{instance_id: mysql_digest}`,
즉 인스턴스당 값 **하나**를 가정했다. 실측에서 MySQL 은 다음을 **다른 다이제스트**로 본다:

| 차이 | MySQL | 우리 |
|---|---|---|
| 공백·주석·키워드 대소문자·백틱 | 같은 다이제스트 | 같음 |
| **식별자 대소문자** (`orders` vs `ORDERS`) | **다른 다이제스트** | 같음 |
| **후행 세미콜론** | **다른 다이제스트** | 같음 |

`lower_case_table_names=0`(리눅스 기본)에서 식별자 대소문자는 실제로 다른 테이블일 수 있으므로
MySQL 이 구분하는 것이 맞다. 우리는 수렴성을 위해 접는다.

→ **`mysql_digests` 는 인스턴스당 집합이어야 한다**: `{instance_id: Set<mysql_digest>}`.
M4-16 의 매핑 학습도 1:N 을 다뤄야 한다 — 하나의 `app_digest` 아래에 여러 `mysql_digest` 를
**집합으로 누적**한다. 방향을 헷갈리면 값 하나짜리 슬롯을 만들고 덮어쓰게 된다.

**반영 대상**: [04 §2.3](04-data-model.md), [17](17-roadmap-tasks.md) M4-16.

---

## E. 다이제스트 절단과 해시 (M1-13, [OPEN-Q-16](OPEN-QUESTIONS.md)) — **`app_digest` 필수**

3,330바이트 SQL(서로 다른 식별자 300개)을 두 설정에서 비교:

| `max_digest_length` | `DIGEST_TEXT` 길이 | `DIGEST` |
|---|---|---|
| 1,024 | 958 | `61416d954598164e…` |
| 4,096 | 3,826 | `033f75ac13c2b45a…` |

**해시가 다르다.** 파라미터 그룹이 다른 인스턴스 사이에서 `mysql_digest` 를 크로스 인스턴스
그룹핑 키로 쓸 수 없다 → [ADR-011](03-decisions.md)의 `app_digest` 가 필수다. **설계 유지.**

### 두 변수를 혼동하면 잘못된 결론이 나온다

첫 측정에서 `performance_schema_max_digest_length` 만 4096으로 올렸더니
**해시가 같았다**(→ "app_digest 불필요"라는 잘못된 결론). 두 변수는 다르다:

| 변수 | 역할 |
|---|---|
| `max_digest_length` | 다이제스트 **계산** 버퍼 → `DIGEST` 해시를 바꾼다 |
| `performance_schema_max_digest_length` | 다이제스트 텍스트 **저장** 길이 |

→ **[05 §9](05-collector.md) 자가진단의 "다이제스트 길이" 점검이 읽는 변수가 틀렸다.**
`@@max_digest_length` 를 읽어야 하고, 두 값을 함께 표시하는 것이 좋다.

### 절단된 `DIGEST_TEXT` 는 `...` 로 끝나지 않는다

[05 §3.3](05-collector.md)은 "잘렸을 때 `...` 로 끝난다"고 적었다. 실측:

```
max_digest_length=1024 → 끝: "… `id` AS `a_00047` ,"
max_digest_length=4096 → 끝: "… AS `a_00193` , `id`"
```

토큰 중간에서 그냥 끝난다. → `looks_truncated_by_server()` 를 4신호 판정으로 바꿨다:
① `...` 접미 ② 백틱 개수 홀수 ③ 완결 불가 토큰으로 끝남 ④ 길이가 상한에 근접.
**오탐은 무해**(매핑 경로를 타는 것뿐)하고 누락은 조용한 오분류이므로 넓게 잡았다.

**반영 대상**: [05 §3.3](05-collector.md), [05 §9](05-collector.md) 자가진단 점검 항목,
[OPEN-Q-16](OPEN-QUESTIONS.md).

---

## F. 조회 비용 (M1-16) — ADR-005 의 대가는 싸다

스레드 88개 환경, 호출당 100회 평균:

| 쿼리 | 비용 |
|---|---|
| `performance_schema.processlist` (임계값 필터) | 306~604 µs |
| `information_schema.PROCESSLIST WHERE ID IN (…)` | 267~471 µs |
| `information_schema.PROCESSLIST` (전체) | 347~448 µs |

타깃 조회가 PS 폴링보다 **더 싸다**(0.8~0.9배). [ADR-005](03-decisions.md)가 우려한
"타깃 조회 비용"은 이 규모에서 문제가 아니다.

**단서** — 스레드 88개는 적다. `information_schema.PROCESSLIST` 는 스레드 목록 뮤텍스를
잡으므로 수천 스레드에서는 달라질 수 있다. M12 부하 테스트에서 재측정한다.

---

## G. 락 대기 (M1-17) — 설계 확인, 절단 경고도 확인

| 항목 | 결과 |
|---|---|
| `sys.innodb_lock_waits` 컬럼 수 | 30 |
| 설계가 쓰는 컬럼 14개 (`locked_type`·`locked_table_schema`·`wait_age_secs`…) | **전부 존재** |
| `waiting_query` | 85자로 **중간 생략 절단** (`sys.format_statement`) |
| `information_schema.PROCESSLIST` 2차 조회 | 124자 **전문 확보** |

[05 §2.8](05-collector.md)의 "전문 SQL 은 `waiting_pid` 로 2차 조회한다"가 옳다.
`waiting_query` 를 그대로 쓰면 이 프로젝트가 고치려는 절단 버그를 락 화면에서 재현한다.

---

## H. 남은 검증 항목

| 항목 | 왜 아직 못 했는가 |
|---|---|
| M1-5 / M1-14 IAM DB Auth (재부팅 필요 여부, `FreeableMemory`) | 실제 RDS 필요 → `60-seed` apply 후 |
| M1-7 Athena `MERGE INTO` 멱등성 | `20-data` apply 후 |
| M1-8 DynamoDB 증분 내보내기 언네스팅 | 실제 내보내기 산출물 필요 |
| M1-9 수집 루프 벤치마크 (125/250/500대) | 모의 엔드포인트 하네스 구현 후 (M12-7) |
| M1-11 다이제스트 스냅샷 실측 | 시드 RDS 필요 |
| M1-15 Cognito `cognito:groups` | `30-identity` apply 후 |
| 슬로우로그 `# Time` 이 완료 시각인가 ([05 §8.1](05-collector.md)) | 로컬에서 가능. 다음 차례 |
