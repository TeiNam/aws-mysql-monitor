# 05. 수집기 설계

## 1. 수집 루프 구성

인스턴스 1대당 tokio 태스크 묶음 하나. 주기가 다른 루프를 별도 태스크로 돌린다.

| 루프 | 기본 주기 | 대상 DB 쿼리 수(정상 상태) | 목적 |
|---|---|---|---|
| `detect` | 1초 | 1 | 슬로우 쿼리 탐지 |
| `digest` | 60초 | 1 | 다이제스트 델타 |
| `status` | 5초 | 1 | 실시간 지표 |
| `health` | 30초 | 2 | 복제 상태 + 락 대기 |
| `daily` | 24시간 | 5~10 | 인덱스 위생, 테이블 통계, 스키마 지문, 파라미터 |

정상 상태의 초당 쿼리 수 = 1(detect) + 1/60(digest) + 1/5(status) + 2/30(health) ≈ **1.28 QPS**.
NFR-P-02(3 QPS 이하) 안에 든다. 슬로우 쿼리가 발생하면 건당 3~4쿼리가 추가된다.

## 2. 사용하는 SQL 전량

**모든 문장은 읽기 전용이다. DDL/DML은 하나도 없다.** `ANALYZE TABLE`, `FLUSH`,
`TRUNCATE TABLE performance_schema.*`는 금지(FR-CAP-09, FR-AI-10).

### 2.1 탐지 — `detect` 루프

```sql
/* dbmon:detect */
SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE
FROM performance_schema.processlist
WHERE INFO IS NOT NULL
  AND COMMAND NOT IN ('Sleep','Daemon','Binlog Dump','Binlog Dump GTID','Connect')
  AND TIME >= ?                      -- 임계값(초)
  AND ID <> CONNECTION_ID()          -- 자기 자신 제외
  AND (DB IS NULL OR DB NOT IN (...))
  AND USER NOT IN (...)
ORDER BY TIME DESC
LIMIT 500
```

- `performance_schema.processlist`를 쓰는 이유: `information_schema.PROCESSLIST`와 달리
  스레드 목록 뮤텍스를 잡지 않는다. 1초 주기로 도는 유일한 쿼리이므로 여기가 가장 싸야 한다.
- `INFO`는 여기서 **읽지 않는다**(1024바이트 절단본이므로 무의미).
- `TIME`은 초 단위 정수다. 임계값 2초면 실제 탐지 시점은 2.0~3.0초 사이.
- `LIMIT 500`: 대량 슬로우 쿼리 폭주 시 수집기가 폭주하지 않도록 상한. 초과분은
  `detect_overflow` 메트릭으로 노출.
- 모든 문장에 `/* dbmon:<purpose> */` 주석을 붙인다. **단 이 주석은 사람이 읽는 용도로만
  쓴다** — `SHOW PROCESSLIST`, 슬로우로그, 감사 로그에서는 주석이 살아 있다.
  **다이제스트 통계에서는 주석이 제거되므로 식별에 쓸 수 없다**([ADR-005](03-decisions.md)).
  자기 식별·자기 제외는 §10의 계정 기반 방법을 쓴다.

### 2.2 전문 SQL — 임계값 초과 스레드만

```sql
/* dbmon:fulltext */
SELECT ID, DB, USER, HOST, TIME, INFO
FROM information_schema.PROCESSLIST
WHERE ID IN (?, ?, ...)
```

**⚠ 초기 설계는 "`INFO`는 `LONGTEXT`이며 절단되지 않는다"고 적었다. 사실이 아니다**
([19 §A](19-m1-findings.md), MySQL 8.4.11 · 8.0.46 실측).

```
information_schema.PROCESSLIST.INFO   varchar(21845)  →  LENGTH() 기준 65,535바이트에서 절단
performance_schema.processlist.INFO   longtext        →  내용이 1,024바이트로 절단
```

두 컬럼의 제약 방향이 **반대**다. `information_schema` 쪽은 컬럼 타입이 좁고,
`performance_schema` 쪽은 타입은 무제한이지만 서버가 내용을 자른다
(`performance_schema_max_sql_text_length`). 21845 × 3바이트(utf8mb3) = 65535 이므로
이 상한은 컬럼 정의에서 오며 **설정으로 바뀌지 않는다.**

그래도 1,024 → 65,535 는 **64배 개선**이고 실무 SQL 길이 분포를 생각하면 사실상 전부를
커버한다. ADR-005 는 유효하다. 단 다음을 지킨다.

| 실측값 | 대응 |
|---|---|
| 65,535바이트 정확히 반환 | `sql_text_truncated = true`. 상한이 컬럼 정의라 값이 고정이므로 이 판정이 정확하다 |
| `performance_schema_max_sql_text_length` 상향 (1024 → 8192) | `events_statements_current.SQL_TEXT` 가 8,192바이트를 반환한다. **폴백은 동작한다**(재시작 필요) |
| 그 폴백으로도 65,535 초과는 불가 | 65KB 넘는 SQL 의 전문은 어느 경로로도 못 얻는다. 정직하게 표시한다 |

### 2.3 정확 지표 + 다이제스트 — 임계값 초과 스레드만

```sql
/* dbmon:stmtcurrent */
SELECT t.PROCESSLIST_ID, t.THREAD_ID,
       e.EVENT_NAME, e.CURRENT_SCHEMA,
       e.DIGEST, e.DIGEST_TEXT,
       e.TIMER_WAIT, e.LOCK_TIME,
       e.ROWS_EXAMINED, e.ROWS_SENT, e.ROWS_AFFECTED,
       e.CREATED_TMP_TABLES, e.CREATED_TMP_DISK_TABLES,
       e.SELECT_FULL_JOIN, e.SELECT_FULL_RANGE_JOIN, e.SELECT_SCAN,
       e.SORT_MERGE_PASSES, e.SORT_ROWS, e.SORT_SCAN,
       e.NO_INDEX_USED, e.NO_GOOD_INDEX_USED,
       e.NESTING_EVENT_ID, e.NESTING_EVENT_TYPE
FROM performance_schema.events_statements_current e
JOIN performance_schema.threads t USING (THREAD_ID)
WHERE t.PROCESSLIST_ID IN (?, ?, ...)
```

- `TIMER_WAIT`, `LOCK_TIME`은 **피코초**다. ms로 변환할 때 1e9로 나눈다.
  (1세대에 흔한 버그: 마이크로초로 착각)
- `NESTING_EVENT_TYPE`이 `STATEMENT`면 프로시저 내부 문장이다. 이 경우 `information_schema`의
  `INFO`는 최상위 CALL 문이고 `events_statements_current`가 내부 문장을 준다 → **양쪽을 모두
  저장**하고 `is_nested=true`로 표시한다.
- **`DIGEST_TEXT` 는 절단돼도 `...` 로 끝나지 않는다** (8.4.11 실측 — 토큰 중간에서 잘린다).
  `looks_truncated_by_server()` 가 4가지 신호를 함께 본다: `...` 접미, 홀수 개의 백틱,
  불완전한 꼬리 토큰, `max_digest_length` 근처의 길이(여유 256바이트).

### 2.4 실행계획 — 별도 연결

**⚠ 이 경로는 RDS·Aurora 에서 쓸 수 없다** ([19 §B](19-m1-findings.md) 실측).
타인 커넥션을 explain 하려면 **정적 전역 권한 전체**가 필요하고, RDS 는 마스터 유저에게도
`SUPER`·`FILE`·`SHUTDOWN` 을 주지 않는다. `PROCESS` 도 `SUPER` 도 부족하다.
→ 실질 기본 경로는 **`plan_source=rerun`**(원문 재실행)이고, DML 은
**`rerun_as_select`**(조건절을 SELECT 로 변환한 근사 플랜)다.
아래 문장은 **자체 관리 MySQL**에서만 쓰인다.

```sql
/* dbmon:plan */ EXPLAIN FORMAT=JSON FOR CONNECTION 8842119
```

- **연결 ID를 파라미터 바인딩할 수 없다.** 문장에 리터럴로 박아야 한다.
  → `u64`로 파싱된 값만 포맷팅한다. 문자열이 들어올 경로 자체를 없앤다(타입으로 강제).
- **반드시 별도 연결에서 실행한다.** `detect` 루프의 연결을 점유하면 폴링이 밀린다.
- 3초 타임아웃. 초과 시 연결을 버리고 새로 만든다(취소해도 서버 측 작업이 남을 수 있음).
- `FORMAT=TREE FOR CONNECTION` 지원 여부는 버전별로 다를 수 있다
  → [OPEN-Q-05](OPEN-QUESTIONS.md). 미지원 시 JSON만 수집.

실패 분류와 대응 — **에러 메시지 문자열이 아니라 에러 코드로 분류한다.**
`lc_messages` 설정에 따라 메시지가 번역되므로 문자열 매칭은 깨진다.

| 에러 코드 | 원인 | 대응 |
|---|---|---|
| `1094` `ER_NO_SUCH_THREAD` | 스레드가 이미 종료됨 | `plan_source=none`, `plan_error=thread_gone`. 폴백 시도 |
| `3012` `ER_EXPLAIN_NOT_SUPPORTED` | SELECT/UPDATE/INSERT/DELETE/REPLACE 이외 (CALL, SET, DDL 등) | `plan_error=not_explainable`. 폴백 없음 |
| `1044`/`1045`/`1227` (권한 계열) | **타인 커넥션 explain 권한 부족. RDS 에서는 상시 발생** | `plan_error=denied` → `rerun` 폴백. **자가진단 실패로 승격하지 않는다** (사용자가 고칠 수 없다) |
| `1142` `ER_TABLEACCESS_DENIED_ERROR` | 읽기 전용 계정으로 DML 을 `EXPLAIN` 했다 | `plan_error=dml_privilege_missing` → `rerun_as_select` 로 전환 |
| (에러 없음, 빈 결과) | 유휴 커넥션이었다 | `plan_error=no_statement`. **에러로 세지 않는다** — 정상을 실패로 센다 |
| 클라이언트 타임아웃 | 서버가 플랜 직렬화 시점에 도달 못 함 | `plan_error=timeout`. 연결 폐기 |
| 그 외 | — | `plan_error=other:<code>`. 코드를 기록해 나중에 분류에 추가 |

M1-2 스파이크에서 8.4 / 8.0.32의 **실제 에러 코드를 기록**해 이 표를 확정한다.

폴백(`plan_source=rerun`) 조건: `statement_type == SELECT` **AND** SQL 전문 확보됨
**AND** `sql_text_truncated == false` **AND** 폴백 예산 잔여. UPDATE/DELETE/INSERT는
재실행하지 않는다.

### 2.5 다이제스트 스냅샷 — `digest` 루프

#### 2.5.1 조회 쿼리

**2단 쿼리로 나눈다.** 이유는 페이로드 크기다 — 아래 §2.5.3 참조.

```sql
/* dbmon:digest */                       -- ① 지표만. 큰 텍스트 컬럼 제외
SELECT SCHEMA_NAME, DIGEST,
       COUNT_STAR,
       SUM_TIMER_WAIT, MIN_TIMER_WAIT, AVG_TIMER_WAIT, MAX_TIMER_WAIT,
       SUM_LOCK_TIME, SUM_ERRORS, SUM_WARNINGS,
       SUM_ROWS_AFFECTED, SUM_ROWS_SENT, SUM_ROWS_EXAMINED,
       SUM_CREATED_TMP_TABLES, SUM_CREATED_TMP_DISK_TABLES,
       SUM_SELECT_FULL_JOIN, SUM_SELECT_FULL_RANGE_JOIN,
       SUM_SELECT_RANGE, SUM_SELECT_RANGE_CHECK, SUM_SELECT_SCAN,
       SUM_SORT_MERGE_PASSES, SUM_SORT_RANGE, SUM_SORT_ROWS, SUM_SORT_SCAN,
       SUM_NO_INDEX_USED, SUM_NO_GOOD_INDEX_USED,
       FIRST_SEEN, LAST_SEEN,
       QUANTILE_95, QUANTILE_99,
       QUERY_SAMPLE_SEEN, QUERY_SAMPLE_TIMER_WAIT
FROM performance_schema.events_statements_summary_by_digest
WHERE DIGEST IS NOT NULL
  AND LAST_SEEN >= ?                     -- ★ 이전 스냅샷 시각. 활성 다이제스트만
  AND (SCHEMA_NAME IS NULL OR SCHEMA_NAME NOT IN
       ('mysql','sys','performance_schema','information_schema'))
```

```sql
/* dbmon:digesttext */                   -- ② 처음 본 다이제스트의 텍스트만
SELECT DIGEST, DIGEST_TEXT, QUERY_SAMPLE_TEXT
FROM performance_schema.events_statements_summary_by_digest
WHERE DIGEST IN (?, ?, ...)              -- ①에서 캐시에 없던 것만. 없으면 실행 안 함
```

- ②는 워커 메모리의 `DIGEST → app_digest` 캐시에 없는 다이제스트가 있을 때만 실행한다.
  안정 상태에서는 실행 횟수가 0에 가깝다.
- `QUERY_SAMPLE_TEXT`는 `QUERY_SAMPLE_SEEN`/`QUERY_SAMPLE_TIMER_WAIT`가 변했을 때만
  다시 가져온다(①이 그 두 값을 주므로 변화를 감지할 수 있다).

#### 2.5.2 델타 계산 규칙

```
prev = 이전 스냅샷[(SCHEMA_NAME, DIGEST)]
cur  = 현재 행

if prev 없음:
    신규 다이제스트 → NEW_DIGEST 이벤트, 델타 = 0 (기준선만 수립)
    단 cur.FIRST_SEEN 이 이전 스냅샷 시각보다 나중이면 델타 = cur 전체
elif cur.COUNT_STAR < prev.COUNT_STAR:
    리셋 감지 (서버 재시작 / 테이블 오버플로 / 수동 TRUNCATE)
    → PS_RESET 이벤트, reset_detected=true, 델타 = cur 전체 (0부터 누적된 것으로 간주)
else:
    델타 = cur - prev  (모든 SUM_/COUNT_ 컬럼)
```

- `QUANTILE_95/99`는 **누적 히스토그램에서 유도된 현재 추정값**이다. 델타를 계산하면 안 된다.
  스냅샷 시점의 값을 그대로 보관한다(`p95_ms_snapshot`).
- `MIN/MAX/AVG_TIMER_WAIT`도 누적값이다. `MAX`는 창 내 최대가 아니라 전체 기간 최대다.
  → 창 내 최대는 `델타 total / 델타 count`로 평균만 정확히 얻고, `max`는 "전체 기간 최대"로
  라벨을 다르게 붙인다. 이걸 헷갈리면 리포트 수치가 틀린다.
- `QUERY_SAMPLE_TEXT` 교체 규칙은 둘이다: (a) **더 느린 실행은 항상** 교체한다,
  (b) 저장된 샘플이 `performance_schema_max_digest_sample_age`(기본 60초)보다 오래되면
  **속도와 무관하게** 다음 실행이 교체한다. 즉 활발한 다이제스트의 샘플은 거의 매번 바뀐다.
  → §2.5 ②의 "값이 바뀌었을 때만 재조회"가 활발한 다이제스트에서 자주 트리거된다.
  `QUERY_SAMPLE_TEXT` 재조회는 별도 주기(기본 10분)로 제한한다. 우리의 실시간 캡처 샘플이
  더 좋으므로 PS 샘플은 보조 수단이다.
- 사라진 다이제스트가 많으면 `performance_schema_digests_size` 오버플로 신호다.
  `events_statements_summary_by_digest`에 `DIGEST IS NULL` 행(오버플로 집계 행)이 있는지 확인해
  경고한다.

#### 2.5.3 `LAST_SEEN` 필터와 컬럼 분리가 필수인 이유

이 테이블은 서버 시작 이후 관측된 **모든** 다이제스트를 누적한다. 바쁜 인스턴스는
`performance_schema_digests_size`(기본 10,000) 한도까지 찬다. 필터 없이 전량을 매분 가져오면:

```
행 10,000개 × (DIGEST_TEXT 평균 400B + QUERY_SAMPLE_TEXT 평균 500B + 지표 40컬럼 400B)
  ≈ 13MB / 스냅샷
× 1,440 스냅샷/일  =  약 18GB / 일 / 인스턴스
× 500 인스턴스     =  약 9TB / 일
```

- **네트워크**: 크로스 리전 인스턴스라면 데이터 전송 비용이 월 수천 달러가 된다
  ([16 §2.9](16-cost.md)).
- **대상 DB 부하**: 13MB를 직렬화하는 것 자체가 무시할 부하가 아니다.
  NFR-P-02("CPU 1% 미만")를 지킬 수 없다.
- **워커 CPU**: 파싱·해싱 대상이 100배 늘어난다. [ADR-001](03-decisions.md)의 벤치마크
  전제도 이걸 반영해야 한다.

두 가지 최적화로 해결한다.

| 최적화 | 효과 |
|---|---|
| `LAST_SEEN >= <이전 스냅샷 시각>` | 60초 창에 실제로 실행된 다이제스트만. 10,000 → 보통 100~500행 |
| 텍스트 컬럼을 2차 쿼리로 분리 + 캐시 | 행당 900B → 400B. 2차 쿼리는 신규 다이제스트가 있을 때만 |

```
최적화 후: 300행 × 400B ≈ 120KB / 스냅샷
         × 1,440 = 약 170MB / 일 / 인스턴스
         × 500  = 약 85GB / 일        (100배 축소)
```

**주의** — `LAST_SEEN` 필터에는 함정이 있다. 서버·클라이언트 시계가 다르거나 스냅샷 주기가
밀리면 경계 구간의 다이제스트를 놓칠 수 있다. 대응:
- 필터 값은 **대상 DB의 시각**을 쓴다. 매 스냅샷에서 `SELECT NOW(6)`를 함께 읽어
  다음 스냅샷의 필터 값으로 쓴다(우리 시계를 쓰지 않는다).
- 안전 여유 5초를 뺀다(`LAST_SEEN >= prev_db_now - 5s`). 중복 관측은 델타 계산이 0으로
  흡수하므로 무해하고, 누락은 복구 불가다.
- 스냅샷이 실패하거나 지연되면 다음 필터 값은 **마지막 성공 시각**이다.
  주기가 밀려도 구간이 이어진다.

### 2.6 실시간 지표 — `status` 루프

```sql
/* dbmon:status */
SELECT VARIABLE_NAME, VARIABLE_VALUE
FROM performance_schema.global_status
WHERE VARIABLE_NAME IN (
  'Queries','Questions','Slow_queries','Threads_running','Threads_connected',
  'Threads_created','Aborted_connects','Connection_errors_max_connections',
  'Innodb_rows_read','Innodb_rows_inserted','Innodb_rows_updated','Innodb_rows_deleted',
  'Innodb_row_lock_waits','Innodb_row_lock_time','Innodb_row_lock_current_waits',
  'Innodb_buffer_pool_read_requests','Innodb_buffer_pool_reads',
  'Innodb_data_reads','Innodb_data_writes','Innodb_log_waits','Innodb_os_log_written',
  'Created_tmp_tables','Created_tmp_disk_tables','Created_tmp_files',
  'Select_scan','Select_full_join','Select_range_check','Sort_merge_passes',
  'Table_locks_waited','Handler_read_rnd_next','Open_tables','Opened_tables',
  'Com_select','Com_insert','Com_update','Com_delete','Com_commit','Com_rollback',
  'Bytes_received','Bytes_sent','Uptime'
)
```

- 카운터형은 델타/경과초 → 초당 변화율. 게이지형(`Threads_running`,
  `Innodb_row_lock_current_waits`, `Open_tables`)은 그대로.
- `Uptime` 감소 → 재시작 감지 → 델타 스킵 + `PS_RESET` 이벤트.
- 파생 지표: 버퍼 풀 히트율 `1 - reads/read_requests`, 임시 디스크 테이블 비율,
  풀스캔 비율 `Select_scan/Com_select`.

### 2.7 복제 상태 — `health` 루프

```sql
/* dbmon:repl */ SHOW REPLICA STATUS
```

- 8.4에서 `SHOW SLAVE STATUS`는 제거됐다. 하한이 8.0.32+이므로 분기 없이 이 문장만 쓴다.
- 필요 권한: `REPLICATION CLIENT`.
- 수집 필드: `Replica_IO_Running`, `Replica_SQL_Running`, `Seconds_Behind_Source`,
  `Last_IO_Error(no)`, `Last_SQL_Error(no)`, `Retrieved_Gtid_Set`, `Executed_Gtid_Set`,
  `Source_Host`, `Auto_Position`, `Replica_SQL_Running_State`.
- 결과가 0행이면 복제 미구성(또는 소스 노드) → 정상. 에러로 취급하지 않는다.

Aurora는 위 문장이 의미 없다(스토리지 레벨 복제).

```sql
/* dbmon:aurorarepl */
SELECT SERVER_ID, SESSION_ID, LAST_UPDATE_TIMESTAMP, REPLICA_LAG_IN_MILLISECONDS,
       CPU, IS_CURRENT
FROM information_schema.replica_host_status
```

바이너리 로그 위치가 필요하면 `SHOW BINARY LOG STATUS`(8.4) — `SHOW MASTER STATUS`는 제거됨.
8.0.32~8.4 미만 인스턴스에서는 `SHOW MASTER STATUS`가 필요하므로, 이 문장만은 버전 분기한다.

### 2.8 락 대기 / 장기 트랜잭션 — `health` 루프

```sql
/* dbmon:lockwait */
SELECT waiting_pid, waiting_lock_mode, waiting_trx_age, waiting_trx_rows_locked,
       blocking_pid, blocking_lock_mode, blocking_trx_age,
       locked_type, locked_table_schema, locked_table_name, locked_index,
       locked_table, wait_age_secs, sql_kill_blocking_query
FROM sys.innodb_lock_waits
WHERE wait_age_secs >= ?
```

`sys.innodb_lock_waits`를 쓰는 이유: `performance_schema.data_lock_waits`와
`information_schema.INNODB_TRX`를 직접 조인하면 트랜잭션 ID 타입(varchar vs bigint unsigned)
캐스팅과 대기 체인 구성을 직접 해야 한다. sys 뷰가 이미 정확히 그걸 한다.
(`sql_kill_blocking_query` 컬럼은 **표시만** 한다. 우리는 KILL을 실행하지 않는다.)

**컬럼명 주의** — `locked_type` / `locked_table_schema` / `locked_table_name`이다.
`waiting_lock_type`, `waiting_table_schema` 같은 이름은 존재하지 않는다.

**⚠ `waiting_query` / `blocking_query`를 이 뷰에서 가져오지 않는다.**
이 두 컬럼은 `sys.format_statement()`를 거치며
`sys.sys_config.statement_truncate_len`(**기본 64자**, 중간 생략)으로 잘린다.
즉 이 프로젝트가 고치려는 "조용한 SQL 절단" 버그를 락 화면에서 그대로 재현하게 된다.

→ 전문 SQL은 `waiting_pid` / `blocking_pid`로
`information_schema.PROCESSLIST WHERE ID IN (...)`을 2차 조회해 얻는다(§2.2와 같은 경로).
`statement_truncate_len` 상향은 `sys.sys_config` 쓰기 권한이 필요해 모니터링 계정의
읽기 전용 원칙과 충돌하므로 채택하지 않는다.

```sql
/* dbmon:longtxn */
SELECT trx_id, trx_state, trx_started, trx_requested_lock_id, trx_wait_started,
       trx_mysql_thread_id, trx_query, trx_operation_state,
       trx_tables_in_use, trx_tables_locked, trx_rows_locked, trx_rows_modified,
       trx_isolation_level, trx_is_read_only
FROM information_schema.innodb_trx
WHERE trx_started < NOW() - INTERVAL ? SECOND
ORDER BY trx_started
```

데드락은 `SHOW ENGINE INNODB STATUS`의 `LATEST DETECTED DEADLOCK` 블록을 파싱한다.
같은 블록이 계속 반복되므로 텍스트 정규화 해시로 중복을 병합한다([04 §2.3](04-data-model.md)).
`SHOW ENGINE INNODB STATUS`는 출력이 크므로(수십 KB) `health` 루프가 아니라 5분 주기로 돌린다.

### 2.9 인덱스 위생 / 통계 — `daily` 루프

```sql
/* dbmon:unusedidx */
SELECT object_schema, object_name, index_name FROM sys.schema_unused_indexes;

/* dbmon:redundantidx */
SELECT table_schema, table_name, redundant_index_name, redundant_index_columns,
       dominant_index_name, dominant_index_columns, subpart_exists, sql_drop_index
FROM sys.schema_redundant_indexes;

/* dbmon:autoinc */
SET SESSION information_schema_stats_expiry = 0;   -- 아래 주의 2 참조
SELECT table_schema, table_name, column_name, data_type, column_type, is_signed,
       max_value, auto_increment, auto_increment_ratio
FROM sys.schema_auto_increment_columns
WHERE auto_increment_ratio > 0.5;

/* dbmon:tablestats */
SELECT TABLE_SCHEMA, TABLE_NAME, ENGINE, TABLE_ROWS, AVG_ROW_LENGTH,
       DATA_LENGTH, INDEX_LENGTH, DATA_FREE, AUTO_INCREMENT,
       CREATE_TIME, UPDATE_TIME, TABLE_COLLATION
FROM information_schema.TABLES
WHERE TABLE_SCHEMA NOT IN ('mysql','sys','performance_schema','information_schema')
  AND TABLE_TYPE = 'BASE TABLE';
```

**주의** — `sys.schema_unused_indexes`는 `performance_schema.table_io_waits_summary_by_index_usage`
기반이므로 **서버 재시작 이후 누적치**만 본다. 재시작 직후에는 모든 인덱스가 "미사용"으로
보인다. → `Uptime`이 7일 미만이면 이 스냅샷을 `low_confidence=true`로 표시하고 알림하지 않는다.
`Uptime`은 **상태 변수**이므로 `SELECT @@GLOBAL.uptime`은 `ERROR 1193`으로 실패한다
— `performance_schema.global_status`에서 읽는다.
이걸 놓치면 실제로 쓰는 인덱스를 지우게 된다. **1세대에는 이 기능이 없었으므로 새로 도입할 때
반드시 이 가드가 필요하다.**

**주의 2** — `sys.schema_auto_increment_columns`는 `information_schema.TABLES.AUTO_INCREMENT`를
읽고, 이 컬럼도 `information_schema_stats_expiry`(기본 86400초) 캐시 대상이다. 캐시된 값을 쓰면
소진 경고가 최대 24시간 낡는다. 이 쿼리만 `SET SESSION information_schema_stats_expiry = 0`으로
실시간 조회한다(세션 변수라 대상 DB 전역 설정을 바꾸지 않는다. 통계 재계산 비용이 있으므로
하루 1회 `daily` 루프에서만 쓴다).

**주의 3** — `information_schema.TABLES.TABLE_ROWS`는 InnoDB에서 추정치이고
`information_schema_stats_expiry`(기본 86400초) 캐시가 적용된다. 정확한 행수가 필요한
곳에서는 추정치임을 명시한다. `COUNT(*)`를 실행하지 않는다(풀스캔 유발).

### 2.10 어드바이저용 스키마·카디널리티 (온디맨드)

```sql
/* dbmon:ddl   */ SHOW CREATE TABLE `schema`.`table`;

/* dbmon:idxstat */
SELECT TABLE_SCHEMA, TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX,
       COLUMN_NAME, EXPRESSION, COLLATION, CARDINALITY, SUB_PART,
       NULLABLE, INDEX_TYPE, IS_VISIBLE
FROM information_schema.STATISTICS
WHERE (TABLE_SCHEMA, TABLE_NAME) IN ((?,?), ...)
ORDER BY TABLE_SCHEMA, TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX;

/* dbmon:histogram */
SELECT SCHEMA_NAME, TABLE_NAME, COLUMN_NAME, HISTOGRAM
FROM information_schema.COLUMN_STATISTICS
WHERE (SCHEMA_NAME, TABLE_NAME) IN ((?,?), ...);
```

`STATISTICS.CARDINALITY`도 추정치이며 `information_schema_stats_expiry` 캐시 대상이다.
`ANALYZE TABLE`로 갱신하지 않는다(FR-AI-10). 대신 `TABLES.UPDATE_TIME`과 카디널리티/행수 비율의
이상(예: 카디널리티 > 행수)을 감지해 "통계가 낡았을 가능성"을 권고에 포함한다.

**히스토그램은 대부분의 인스턴스에서 비어 있다.**
`COLUMN_STATISTICS`의 히스토그램은 `ANALYZE TABLE ... UPDATE HISTOGRAM ON <col>`을 **명시적으로
실행한 컬럼에만** 생성되고 자동 생성은 없다. 우리는 `ANALYZE TABLE`을 실행하지 않으므로
이 쿼리는 "누군가 이미 히스토그램을 만들어 둔" 컬럼에서만 결과가 나온다.
→ 히스토그램은 **있으면 활용, 없으면 그만**인 부가 정보다. 어드바이저 품질의 전제로 삼지 않고,
[07 §2.3](07-credentials-bootstrap.md)의 권한 모드 비교에서도 차별 요소로 쓰지 않는다.

## 3. SQL 정규화와 `app_digest`

### 3.1 목표

세 소스를 같은 키로 묶는다.

```
소스 A: information_schema.PROCESSLIST.INFO   (리터럴 살아있는 원문)
소스 B: events_statements_*.DIGEST_TEXT       (MySQL이 이미 ? 로 치환한 텍스트)
소스 C: CloudWatch 슬로우로그                  (리터럴 살아있는 원문)

목표: canonical(A) == canonical(B) == canonical(C)
      app_digest = sha256(canonical)[..32]
```

### 3.2 정규화 규칙

목표는 `DIGEST_TEXT` 를 **바이트 단위로 재현하는 것이 아니다.** 그건 불가능하고 필요도 없다.
필요한 것은 **수렴**이다:

```
normalize(원문 SQL) == normalize(DIGEST_TEXT)
```

양쪽을 같은 함수에 통과시켜 같아지면 된다. 이 완화 덕분에 토큰을 **공백 한 칸으로 균일하게
잇는** 단순한 구현이 성립한다 — MySQL 의 문맥별 간격 규칙을 흉내낼 필요가 없다.

아래는 M1-6 골든 코퍼스(131건, MySQL 8.4.11 / 8.0.x)로 **확정된** 규칙이다
([19 §C](19-m1-findings.md)). 이전 판의 규칙표는 가설이었고 실측으로 교체됐다.

| 단계 | 규칙 | 비고 |
|---|---|---|
| 1 | 토큰화 후 **공백 한 칸으로 균일하게 잇는다** | 원문의 공백·개행·탭은 전부 버린다 |
| 2 | 블록 주석 `/* … */`, 라인 주석 `-- …`, `# …` 제거 | |
| 2a | 옵티마이저 힌트 `/*+ … */` 는 보존하되 **내부를 재귀적으로 정규화한다** | `MAX_EXECUTION_TIME(1000)` → `MAX_EXECUTION_TIME (?)`. 원문 그대로 보존하면 수렴하지 않는다 |
| 2b | 버전 조건 주석 `/*!NNNNN … */` 를 만나면 **플랜 재실행을 거부한다** | 렉서는 버리지만 서버는 실행한다 (§2.4a) |
| 3 | 문자열 리터럴(`'…'`, `"…"`) → `?`. 이스케이프(`\'`, `''`) 처리 | |
| 4 | 수치 리터럴(정수·실수·과학표기·`0x…`·`b'…'`) → `?` | 크기 접미(`16M`)를 함께 흡수한다 |
| 4a | 문자셋 도입자 `_utf8mb4'…'` → `( _charset ) ?` | |
| 5 | **리터럴만 있는 괄호 그룹** → `( ... )` | `IN` 과 `VALUES` 만이 아니다 — 함수 인자도 해당한다 |
| 5a | 괄호 밖의 **연속 리터럴 2개 이상** → `? , ...` | `LIMIT 5, 10` |
| 5b | 다중 행 `VALUES (…),(…),…` → 연속 그룹을 흡수 | 행 수가 달라도 같은 다이제스트다 |
| 6 | **식별자를 소문자로 접는다** (키워드도 소문자) | ⚠ 아래 설명 |
| 7 | 동의어를 통일한다 | `DISTINCTROW`→`DISTINCT`, `CHARACTER`→`CHAR`, `INTEGER`→`INT`, `REGEXP`→`RLIKE`, `SUBSTR`→`SUBSTRING`, `DATABASE`→`SCHEMA`, `CURRENT_TIMESTAMP`→`NOW`, `<>`→`!=`, `sql_tsi_` 접두어 제거 |
| 8 | 백틱은 유지 | |
| 9 | 마지막 세미콜론 제거. **중간 세미콜론이 있으면 멀티문장이므로 재실행을 거부한다** | |
| 10 | 결과를 **고정 8192자**에서 절단 (문자 경계 안전) | 인스턴스 설정과 무관한 결정론적 절단 |

**규칙 6 이 이 표에서 가장 중요하다.** 이전 판은 "식별자는 원본 대소문자 유지" 였는데
그 규칙으로는 수렴하지 않는다:

| 입력 | 원문 SQL | `DIGEST_TEXT` | 소문자로 접지 않으면 |
|---|---|---|---|
| 날짜 리터럴 | `date '2026-01-01'` | `DATE ?` | `date` vs `DATE` → **갈라진다** |
| 함수명 | `Count(*)` | `COUNT (*)` | 갈라진다 |

MySQL 은 예약어를 대문자로 정규화하지만 우리는 어느 토큰이 예약어인지 **완벽히 알 수 없다.**
양쪽을 다 소문자로 접으면 그 판정이 필요 없어진다.

**예약어 집합의 오류는 비대칭이다** ([19 §C-6](19-m1-findings.md)):

| 오류 | 결과 |
|---|---|
| 예약어를 **빠뜨렸다** | 무해하다 — 양쪽 다 식별자로 취급되어 여전히 수렴한다 |
| 예약어가 **아닌 것을 넣었다** | 치명적이다 — `status`, `date`, `count` 같은 흔한 컬럼명이 깨진다 |

그래서 목록에 없는 단어를 추가할 때는 실측 근거가 필요하다.
`crates/normalize/src/keywords.rs` 의 `rejects_non_reserved` 테스트가 이걸 지킨다.

**골든 코퍼스** (M1-6, 완료):

```
docker compose 로 MySQL 8.4 / 8.4-wide(max_digest_length=4096) / 8.0 기동
  → tests/fixtures/digest_corpus.sql 의 131건 실행
  → events_statements_summary_by_digest 에서 실제 DIGEST_TEXT 수집
  → tests/fixtures/digest_golden_84.json 으로 커밋
  → normalize(raw) == normalize(DIGEST_TEXT) 를 전 코퍼스에 대해 assert
```

재현: `cargo test -p dbmon --test m1_digest -- --nocapture`

### 3.3 절단 문제와 `mysql_digest` 매핑

`DIGEST_TEXT`는 `performance_schema_max_digest_length`(기본 1024바이트)에서 잘리고,
잘린다. **초기 설계는 "잘렸을 때 `...`로 끝난다"고 적었는데 사실이 아니다** —
토큰 중간에서 그냥 끝난다([19 §E](19-m1-findings.md)):

```
max_digest_length=1024 → 끝: "… `id` AS `a_00047` ,"
max_digest_length=4096 → 끝: "… AS `a_00193` , `id`"
```

그래서 절단 판정을 4신호로 한다: ① `...` 접미 ② 백틱 개수 홀수 ③ 완결 불가 토큰으로 끝남
④ 길이가 `max_digest_length` 에 근접. **오탐은 무해**(매핑 경로를 타는 것뿐)하고 누락은
조용한 오분류이므로 넓게 잡는다.

**⚠ 읽어야 할 변수는 `max_digest_length` 다.** `performance_schema_max_digest_length` 는
저장 길이만 바꾸고 해시를 바꾸지 않는다 — 그것만 올려서 측정하면 "해시가 같다"는 잘못된
결론이 나온다(실제로 첫 측정에서 그렇게 됐다).

파라미터 그룹이 다른 인스턴스 사이에서는 같은 쿼리의
`DIGEST_TEXT`가 다르게 잘릴 수 있고, 그러면 `app_digest`도 달라진다.

**해결 — 학습된 매핑 테이블**

```
실시간 캡처는 원문 SQL과 mysql_digest 를 동시에 갖는다.
  → app_digest_from_raw ↔ mysql_digest 대응을 DigestText.mysql_digests 에 기록

다이제스트 스냅샷 처리 시:
  if DIGEST_TEXT 절단 안 됨:
      app_digest = sha256(normalize(DIGEST_TEXT))        [신뢰도 high]
  elif mysql_digest 가 매핑에 있음:
      app_digest = 매핑값                                 [신뢰도 high]
  else:
      app_digest = sha256(normalize(DIGEST_TEXT))        [신뢰도 low, digest_truncated=true]
      → 나중에 그 쿼리가 실시간 캡처에 걸리면 매핑이 학습되고,
        이후 스냅샷부터 올바른 그룹으로 합류한다
```

`digest_confidence`를 레코드에 저장하고, UI에서 `low`인 그룹에 배지를 표시한다.
근본 완화책은 `performance_schema_max_digest_length`를 올리는 것(재시작 필요)이며,
자가진단에서 이 값을 읽어 4096 미만이면 권고를 표시한다.

### 3.4 마스킹 후조건 검증 (T-36)

`masked` / `off` 정책에서 저장 직전에 **리터럴 잔존 0**을 assert한다.

```
postcondition(normalized) -> Result<()>
  다음이 남아 있으면 실패:
    - 인용부호 안의 문자열 리터럴 ('...', "...")
    - ? 로 치환되지 않은 숫자 리터럴 (정수·실수·과학표기)
    - 16진수(0x41), 비트(b'0101'), introducer 리터럴(X'41', N'..', _utf8mb4'..')
    - 날짜/시간 리터럴 (DATE '..', TIMESTAMP '..', INTERVAL n UNIT)
실패 시: sql_text 를 저장하지 않고 off 로 강등
        masking_postcondition_failed 카운터 증가 + 해시만 로깅
```

정규화 규칙표(§3.2)가 MySQL 동작 에뮬레이션 정확도에 의존하므로, "DIGEST_TEXT와 같아지는가"
(등가성)만 검증하면 **"리터럴이 없는가"(후조건)는 아무도 확인하지 않는다.**
`masked`는 보안 통제이므로 실패 시 저장하지 않는 쪽이 맞다.

규칙표에 위 리터럴 형태를 추가하고 골든 코퍼스에 포함한다.

### 3.5 `statement_type` 판정

정규화된 텍스트의 첫 키워드로 판정한다. `WITH`로 시작하면 CTE 본체의 첫 DML 키워드를 찾는다.
`/*+ */` 힌트가 앞에 오는 경우를 처리한다.

## 4. in-flight 상태 머신

```
                  detect 에서 관측됨 (TIME >= threshold)
                              │
                              ▼
                     ┌─────────────────┐
        ┌───────────►│    OBSERVED     │  (첫 관측: 심층 조회 3건 발행)
        │            └────────┬────────┘
        │  다음 tick 에도       │
        │  같은 thread_id       ▼
        │  + 같은 digest  ┌─────────────────┐
        └────────────────│    TRACKING     │  max_time 갱신, 플랜 미확보 시 재시도(최대 2회)
                         └────────┬────────┘
                                  │ tick 에서 사라짐
                                  ▼
                         ┌─────────────────┐
                         │   FINALIZING    │  ended_at 기록, 정규화, 저장
                         └────────┬────────┘
                                  ▼
                              저장 완료
```

### 4.1 스레드 ID 재사용 오탐 방지

MySQL은 연결이 끊기면 `PROCESSLIST_ID`를 재사용한다. 1세대는 `pid`만으로 캐시를 관리했고,
`pid` 유니크 인덱스까지 걸어서 재사용 시 충돌·오탐 여지가 있었다.

동일 실행으로 판정하는 조건 (**전부** 만족):
1. `thread_id` 동일
2. `mysql_digest` 동일 (없으면 `app_digest`)
3. 관측된 `TIME`이 감소하지 않았음 (감소 = 새 쿼리)
4. `db_user` + `db_host` 동일

하나라도 깨지면 기존 항목을 즉시 `FINALIZING`으로 넘기고 새 항목을 시작한다.

### 4.2 시작 시각 추정

**시계 오차를 먼저 보정한다 (F14)** — `first_observed_at_ms`는 **EC2 시계**이고
슬로우로그의 시작 시각은 **DB 시계**에서 온다. 두 시계가 2초 이상 어긋나면:
- ±2초 병합 창(§8.2)이 **상시 실패**해 모든 레코드가 이중화된다
- `record_id`가 갈라진다
- `hour_bucket` / `date_part` / `dur_bucket` 경계 배정이 틀어져 파티션이 어긋난다

§2.5.3에서 `LAST_SEEN` 필터에 대해서만 "우리 시계를 쓰지 않는다"고 해결했는데,
**그 원칙이 다른 경로에는 적용되지 않았다.**

```
매 detect tick 에 SELECT NOW(6) 을 함께 읽어 오프셋을 추정한다
  (detect 쿼리에 컬럼으로 붙이면 추가 왕복이 없다)

clock_offset_ms = db_now_ms - local_now_ms      (지수 이동 평균, α=0.2)

적용 대상:
  started_at_ms 추정        → local 시각을 DB 시각으로 환산한 뒤 계산
  병합 창 (±2초)             → 양쪽을 DB 시각으로 통일해 비교
  hour_bucket / date_part   → DB 시각 기준 (파티션이 대상 DB의 시간축과 일치)
  ttl                       → local 시각 (DynamoDB 가 판단하므로)

|clock_offset_ms| > 1000  → 자가진단 경고 + 병합 창을 |offset| + 2초로 자동 확대
|clock_offset_ms| > 5000  → CLOCK_SKEW 이벤트 + 알림 (EC2 chrony / RDS 시각 확인)
```

오프셋을 인스턴스별로 저장해 UI 자가진단에 표시한다. 왕복 지연(RTT)의 절반을 보정하면
정확도가 올라가지만, 1초 단위 판단에는 단순 차이로 충분하다.

```
started_at_ms = (first_observed_at_ms + clock_offset_ms) - (observed_TIME_sec * 1000)
```

`TIME`이 초 단위라 최대 1초 오차가 있다. `events_statements_current.TIMER_WAIT`(피코초)를
얻었으면 그걸로 보정한다:

```
started_at_ms = observed_at_ms - (TIMER_WAIT / 1_000_000_000)
```

`duration_source`에 어느 쪽을 썼는지 기록한다.

### 4.3 종료 확정과 유실

- tick에서 사라지면 `ended_at_ms = 현재 tick 시각`. 실제 종료는 직전 tick과 현재 tick 사이다
  → 최대 폴링 주기만큼의 오차. `duration_ms`는 **관측된 최대 `TIME`** 을 쓴다(보수적).
- 워커가 죽으면 `TRACKING` 상태 항목이 유실된다. → 진행 중 항목은 최초 관측 시점에
  `state=in_flight`로 **선행 저장**하고, 확정 시 갱신한다. 이러면 워커 장애에도 "느린 쿼리가
  있었다"는 사실은 남는다. 대가: 쓰기 2회. 슬로우 쿼리 건수가 적으므로 수용.

**⚠ 선행 저장에도 리터럴 정책을 적용한다 (F2)** — 초기 설계는 파이프라인 순서가
`선행 저장 → (확정 시) 정규화·마스킹`이었다. 그러면 `masked`/`off` 정책 인스턴스에서도
**선행 저장 시점에 원문이 DynamoDB에 들어간다.** 확정되지 않은 고아 레코드는 영구히
마스킹되지 않은 상태로 남고, 그 사이 증분 내보내기가 아카이브로 옮긴다.
[08 §6.1](08-security-auth.md)의 "소급 마스킹은 불가"와 결합되면 되돌릴 수 없다.

→ **정규화·마스킹·후조건 검증을 선행 저장 전에** 수행한다. 마스킹 후조건 검증은
"확정 경로"가 아니라 **모든 쓰기 경로**에 적용된다.

**정책 전환 중의 확정 정책 (A3-2)** — 진행 중 레코드가 있는 상태에서 관리자가
`full` → `masked`로 바꾸면 어느 정책으로 확정되는가?
→ **선행 저장 시점의 정책을 `literal_policy_at_ms`와 함께 레코드에 고정**하고, 확정 시에도
그 정책을 쓴다. 이유: 확정 시점 정책을 쓰면 이미 저장된 원문을 마스킹본으로 덮어써야 하는데,
그건 "소급 마스킹"이고 부분적으로만 동작한다(플랜은 이미 아카이브에 갔을 수 있다).
- 정책을 강화하는 관리자에게 "진행 중 레코드 N건은 이전 정책으로 확정됩니다"를 표시한다.
- 즉시 반영이 필요하면 [08 §6.1](08-security-auth.md)의 "기존 데이터 삭제" 옵션을 쓴다.

- `TRACKING`이 최대 지속시간(기본 1시간)을 넘으면 강제 확정하고 `long_running=true`.

### 4.5 고아 `in_flight` 레코드 정리 (F4)

"1시간 초과 시 강제 확정"은 **살아있는 워커의 메모리 캐시에 의존**한다. 워커 급사·샤드 이동
시 캐시가 사라지므로 강제 확정 주체도 사라지고, `state=in_flight` 레코드가 **35일 TTL까지
남는다.** 그러면 [09 §3.4](09-frontend.md)의 "현재 실행 중" 목록에 **며칠째 실행 중인 유령
쿼리**가 표시된다(클라이언트가 1초마다 경과 시간을 증가시키므로).

```
선행 저장 시 함께 기록:
  owner_worker    : 워커 인스턴스 ID
  owner_epoch     : 샤드 리스 epoch (2단계 펜싱용)
  last_seen_at_ms : tick마다 갱신 (5초 주기)

스케줄러 리더가 5분마다:
  GSI 로 state=in_flight 레코드를 조회
  last_seen_at_ms < now - (3 × poll_interval + 30초 grace) 인 항목을
    → state=abandoned 로 확정
    → duration_ms = 마지막 관측값, abandoned_reason = "owner_lost"
    → ABANDONED 이벤트 기록

워커 기동 시:
  자기 샤드의 state=in_flight 레코드를 조회
    owner_worker == 자기 자신 → 재수화 시도 (같은 thread_id 가 아직 살아 있으면 추적 재개)
    그 외 → 즉시 abandoned 처리
```

- `state=in_flight` 조회를 위해 **희소 GSI**를 쓴다(`GSI1PK = SQS#in_flight`).
  확정 시 이 속성을 제거하면 인덱스에서 자동 탈락한다([04 §2.3](04-data-model.md)).
- `abandoned` 레코드는 UI에서 "추적 중단(수집기 장애)"으로 표시한다. 유령 쿼리가 아니라
  **관측이 끊긴 사실**을 보여주는 것이 맞다.
- [02 §6](02-architecture.md)의 "유실되면 진행 중 1건만 놓친다"는 선행 저장 도입 이후
  사실이 아니다 — 그 표를 갱신했다.

### 4.4 폭주 방어

| 상황 | 방어 |
|---|---|
| 슬로우 쿼리 수백 건 동시 발생 | `detect` LIMIT 500. 심층 조회는 느린 순 상위 50건만. 나머지는 지표만 |
| 같은 다이제스트가 초당 수십 건 | 인스턴스+다이제스트별 심층 조회 레이트 리밋(기본 초당 3건). 초과분은 카운트만 |
| 플랜 수집 큐 적체 | 큐 상한(인스턴스당 20). 초과 시 플랜 없이 저장 |
| 저장 버퍼 적체 | 상한 초과 시 오래된 것부터 드롭 + `dropped_records` 메트릭 |

## 5. 연결 관리

**초기 설계는 풀 크기를 잘못 계산했다 (F7)** — "최대 3 (detect 1 + 심층 1 + 플랜 1)"이라고
했지만 루프는 **5개**다(`detect`, `digest`, `status`, `health`, `daily`).
`daily`(쿼리 5~10건, 타임아웃 30초)가 도는 동안 `health`(2건)와 `digest`(1~2건)가 겹치면
**`detect`가 커넥션 대기에 들어가** 1초 케이던스와 "tick 예산 = 주기의 80%"
([02 §7](02-architecture.md))가 무너진다. 이건 조용한 탐지 해상도 저하로만 나타난다.

**풀을 2개로 분리한다.**

| 풀 | 용도 | 크기 | 근거 |
|---|---|---|---|
| `hot` | `detect` **전용 예약** + 심층 조회 + 플랜 | 최소 2, 최대 4 | `detect`는 절대 대기하지 않아야 한다. 심층 조회 병렬 + 플랜 전용 1 |
| `bulk` | `digest`, `status`, `health`, `daily` | 최소 1, 최대 2 | 주기가 길고 지연에 관대하다. 세마포어로 동시 1건 제한 |

인스턴스당 총 연결 = 최대 6. 500대면 최대 3,000 연결이지만, 실제로는 유휴 시 최소값
(2+1=3)만 유지되므로 정상 상태 1,500 연결이다. 대상 DB의 `max_connections`에 영향을 주므로
자가진단에서 `max_connections` 대비 우리 몫의 비율을 표시한다.

| 항목 | 값 | 근거 |
|---|---|---|
| 인스턴스당 풀 크기 | `hot` 2~4 + `bulk` 1~2 | 위 표 |
| **`detect` 커넥션 예약** | `hot` 풀에서 1개를 `detect` 전용으로 홀드 | 다른 루프가 절대 점유하지 못한다 |
| 연결 타임아웃 | 5초 | |
| 쿼리 타임아웃 | 3초 (`daily` 루프는 30초) | |
| 유휴 연결 유지 | 무제한(핑으로 유지) | IAM Auth 연결 생성 비용 회피 |
| 세션 설정 | `SET SESSION wait_timeout=..., max_execution_time=3000, transaction_isolation='READ-COMMITTED', autocommit=1, sql_mode=''` | `max_execution_time`으로 서버 측에서도 보호 |
| TLS | 필수. RDS 글로벌 CA 번들로 검증 | NFR-S-03 |
| IAM 토큰 | 15분 유효. 만료 5분 전 갱신 | |

**IAM 토큰 갱신 주의** — 토큰은 **연결 수립 시점**에만 쓰인다. 기존 연결은 토큰이 만료돼도
유지된다. 따라서 갱신이 필요한 건 새 연결을 만들 때뿐이다. 풀 전체가 동시에 재생성되면
초당 연결 제한(C-08)에 걸릴 수 있으므로, 인스턴스별로 0~30초 지터를 준다.

**`max_execution_time`은 SELECT에만 적용된다.** `SHOW`, `EXPLAIN FOR CONNECTION`에는 안 걸리므로
클라이언트 타임아웃이 최후 방어선이다.

## 6. 서킷 브레이커와 백오프

```
연속 실패 3회      → 폴링 주기 × 2 (최대 30초)
연속 실패 10회     → 서킷 오픈: 폴링 중단, 60초 후 half-open 1회 시도
half-open 성공     → 클로즈, 원래 주기 복원
연속 실패 30회     → 인스턴스 상태 unreachable, COLLECT_FAIL 이벤트 + 알림
```

**이 상태를 샤드 인수 시 계승해야 한다 (F24)** — 초기 설계는 백오프 주기와 서킷 상태를
워커 메모리에만 뒀다. 그러면 워커 교체·샤드 재분배 시 새 소유자가 **기본 1초 주기와
클로즈 상태에서 시작**한다. 이미 과부하로 응답이 느린 대상 DB에 즉시 최대 빈도로 붙는
**장애 증폭**이다. 리스 TTL 60초마다 재분배가 반복되면 진동한다.

```
dbmon-config / CSTATE / <instance_id>
  current_poll_interval_ms   현재 주기 (백오프 적용된 값)
  circuit_state              closed | open | half_open
  consecutive_failures       연속 실패 수
  last_failure_reason        분류
  clock_offset_ms            시계 오프셋 (F14)
  updated_at_ms

쓰기 빈도: 상태가 변할 때만 (매 tick 쓰지 않는다)
읽기: 샤드 인수 직후 1회 → 그 상태로 시작
TTL: 7일 (인스턴스가 사라지면 자연 정리)
```

인수 시 서킷이 `open`이면 **half-open 시도부터 시작**한다. 클로즈로 리셋하지 않는다.

실패 종류별 구분:

| 분류 | 예 | 처리 | 버퍼 |
|---|---|---|---|
| **연결 실패** | SG·네트워크·DB 다운 | 서킷 브레이커 대상 (백오프 → 오픈) | 유지 |
| **권한 실패** | `PROCESS` 미부여, GRANT 회수 | 서킷 즉시 오픈 + 자가진단 실패로 승격. 재시도 무의미 | 유지 |
| **쿼리 타임아웃** | DB가 바쁨 | 백오프 대상. 서킷은 열지 않는다 | 유지 |
| **스로틀** (DynamoDB) | `ProvisionedThroughputExceeded` | 지수 백오프. 버퍼 초과 시 드롭 | 초과 시 드롭 |
| **권한·암호화 거부 (재시도 무의미)** | KMS `AccessDeniedException`, 키 비활성, `kms:ViaService` 조건 오설정 | **즉시 서킷 오픈 + `/readyz` 503 + 즉시 알림** | **유지. 드롭 금지** |
| **파싱 실패** | 예상 못한 형식 | 해당 레코드만 버리고 계속 | — |

**KMS 거부를 스로틀로 오분류하면 데이터가 조용히 버려진다 (F25)** — 초기 설계의 오류 분류에는
이 항목이 없었다. DynamoDB·S3는 정상인데 CMK 정책 변경·키 비활성·`kms:ViaService` 조건
오설정([ADR-012](03-decisions.md)가 지적한 그 오류)으로 `AccessDeniedException`이 나면
모든 쓰기·읽기가 실패한다. 그런데 "DynamoDB 스로틀"로 분류되면 지수 백오프 → 버퍼 초과 →
**드롭 경로**를 탄다. 재시도가 무의미한 오류이므로 백오프는 시간만 낭비하고 데이터를 버린다.

→ AWS SDK 오류 코드로 판정한다: `AccessDenied*`, `KMSAccessDenied*`,
`KMSInvalidState*`, `KMSNotFound*`, `UnrecognizedClient*`, `InvalidSignature*`.
이들은 **버퍼를 유지하고 사람이 고칠 때까지 기다린다.**

**파싱 실패 시 원본을 로그에 남기지 않는다.** 마스킹을 신뢰할 수 없는 상황(파싱 실패)에서
마스킹을 신뢰하는 것은 모순이다. 길이·SHA-256 해시·실패 지점 바이트 오프셋만 기록한다 (T-36).

## 7. 샤드 리스

```
shard_key = hash(instance_id) % SHARD_COUNT     (SHARD_COUNT = 64 고정)

워커 루프 (20초마다 — 리스 TTL 60초 ÷ 3회 갱신 기회):
  0. ★ 수집 리더 게이트
       LEADER#collect 리스를 조건부로 획득 시도 (TTL 60초)
       보유하지 못했으면:
         - 목표 샤드 수 = 0
         - 보유 중인 샤드가 있으면 전부 반납하고 인스턴스 태스크 정지
         - /readyz 를 503 으로 응답 (standby)
         - 이후 단계를 건너뛰고 다음 루프까지 대기
  1. 내 리스 갱신: ConditionExpression = "owner = :me AND expires_at > :now
                                          AND lease_epoch = :my_epoch"
     실패 → 해당 샤드 포기
  2. 만료된 리스 탐색 → 조건부 획득 시도
     ConditionExpression = "attribute_not_exists(owner) OR expires_at < :now"
     획득 시 lease_epoch += 1 (2단계 펜싱용. 1단계에서는 기록만)
  3. 목표 샤드 수 = 리더면 SHARD_COUNT(64), 아니면 0
       (2단계에서 펜싱 토큰이 구현되면 ceil(64 / 활성_리더_수) 로 바뀐다)
  4. 획득/상실된 샤드에 대해 인스턴스 태스크 시작/정지
  5. ShardsOwned 메트릭 발행 (리더는 64, standby는 0)
```

### 7.1 수집 리더 게이트가 필수인 이유 (F1)

**초기 설계에는 이 게이트가 없었다.** 그래서 [ADR-018](03-decisions.md)이 "펜싱 토큰이
없으므로 절대 하지 말라"고 한 **다중 active 상태가 1단계에서 기본값으로 발생**했다.

```
초기 설계의 실제 동작:
  ECS desiredCount=2 → 워커 2대 기동
  둘 다 리스 획득 조건(attribute_not_exists(owner) OR expires_at < now)을 만족
  → 목표 샤드 수 = ceil(64/2) = 32 → 각자 32샤드를 수집
  → /readyz 503 은 ALB 라우팅만 막고 수집 소유권과 무관하다
  → 다이제스트 누산기가 last-writer-wins 로 손상 (ADR-018 표의 피해가 그대로 발생)
```

**리더 리스와 샤드 리스를 연결하는 규칙이 어디에도 없었다.** 이제 0단계가 그 연결이다.

| 상태 | `LEADER#collect` | 목표 샤드 | `/readyz` | ALB |
|---|---|---|---|---|
| active | 보유 | 64 | 200 | 등록 |
| standby | 미보유 | **0** | 503 | 제외 |
| 리더 상실 직후 | 상실 | 0 (반납 중) | 503 | 제외 진행 |

- **리더 상실 시 태스크 정지 절차**: 진행 중 tick 완료 대기 → 다이제스트 누산기 플러시
  (`partial=true`) → 샤드 리스 명시적 반납 → 인스턴스 태스크 취소. 그레이스풀 셧다운과
  같은 경로를 쓴다(§4.3, [14 §6.2](14-infrastructure.md)).
- **검증**: `ShardsOwned` 합계가 항상 64 또는 0이어야 한다. 두 워커의 합이 64를 넘으면
  즉시 알람([14 §5.3](14-infrastructure.md)의 샤드 커버리지 알람에 "합계 > 64" 조건 추가).
- 리더 리스는 스케줄러 리더(`LEADER#cron`)와 **별도**다. 수집과 cron의 스케일 축이 다르므로
  2단계에서 분리 배포하면 서로 다른 노드가 잡는다.

- 리스 TTL 60초, 갱신 20초 주기 → 3회 갱신 실패 시 상실.
- 그레이스풀 셧다운 시 모든 리스를 명시적으로 반납한다(즉시 재분배).
- 중복 수집이 잠깐 발생해도 안전하다: 저장 키가 `record_id`로 동일하므로 덮어쓰기다.
- 1단계(단일 워커)에서도 이 코드를 쓴다. 워커가 1대면 64샤드를 다 가져간다.
  → 확장 시 코드 변경이 없다.
- 인스턴스 수가 크게 불균형해질 수 있다(샤드당 인스턴스 수가 다름).
  → `SHARD_COUNT=64`는 500대 기준 샤드당 평균 8대. 극단적 불균형은
  [OPEN-Q-09](OPEN-QUESTIONS.md)에서 가중 할당으로 재검토.

## 8. CloudWatch 슬로우로그 수집

### 8.1 파싱

RDS 슬로우 쿼리 로그 형식:

```
# Time: 2026-08-18T14:23:15.336787Z          <- 문장 완료 시각
# User@Host: app[app] @  [10.0.3.44]  Id: 8842119
# Query_time: 4.213331  Lock_time: 0.000112 Rows_sent: 1  Rows_examined: 8213445
use shop;
SET timestamp=1786026191;                    <- 문장 시작 시각 (2026-08-18T14:23:11Z)
SELECT ... ;
```

- **`# Time:`은 문장 완료 시점**이다(MySQL은 문장이 끝난 뒤 로그를 쓴다). 시작 시각으로 쓰면
  안 된다. 시작 시각은 `SET timestamp=<epoch>`(문장 시작, 초 단위) 또는
  `Time − Query_time`으로 유도한다. 이 전제가 틀리면 §8.2의 ±2초 매칭이 2초 이상 걸린 모든
  쿼리에서 실패한다 → M1 스파이크에서 실측 확인한다.
- `use <db>;`와 `SET timestamp=...;`는 헤더의 일부이며 실제 SQL이 아니다 → SQL 본문에서 제거하되
  `SET timestamp` 값은 시작 시각으로 보관한다.
- 여러 줄 SQL, 세미콜론이 문자열 안에 있는 경우를 처리해야 한다 → 다음 `# Time:` 또는
  `# User@Host:`까지를 한 엔트리로 본다.
- `Thread_id`(=`Id:`)를 얻을 수 있어 실시간 캡처와의 병합 키가 된다.
- `log_output=FILE` + CloudWatch 내보내기 전제. `log_output=TABLE`(`mysql.slow_log`)은
  지원하지 않는다(대상 DB에 부하).

### 8.2 3소스 병합

```
같은 실행으로 판정:
  instance_id 동일
  AND thread_id 동일
  AND |slowlog.start_time − capture.started_at| <= 2초
  AND app_digest 동일

병합 규칙:
  duration_ms, rows_examined, rows_sent, lock_time → 슬로우로그 값 채택 (정확)
  sql_text                                        → 더 긴 쪽 채택
  plan_*                                          → 실시간 캡처 값 유지 (슬로우로그엔 없음)
  capture_source = "merged"
  duration_source = "slowlog"
```

**병합은 양방향이어야 한다 (F5)** — 초기 설계는 슬로우로그 수집 경로에만 병합 로직을 뒀고,
실시간 캡처 확정 경로는 그냥 `PutItem`이었다. 백필로 슬로우로그가 **먼저** 적재된 뒤 같은
실행의 캡처가 확정되면:
- 시작 시각 추정이 1초 어긋나면 → 레코드 2건
- 같은 키로 떨어지면 → `PutItem`이 슬로우로그의 정확 지표(`rows_examined`,
  `duration_source=slowlog`)를 **덮어쓴다**

→ **양쪽 경로 모두** `record_id` 조회 + ±2초 보조 조회를 거친 뒤 `UpdateItem`으로 속성별
병합한다. `PutItem`을 쓰지 않는다.

```
공통 병합 절차 (양방향):
  1. record_id 로 GetItem
  2. 없으면 (instance_id, thread_id, ±2초, app_digest) 보조 조회
     → GSI1(DG#<app_digest>) 에서 시간 범위로 후보 탐색
  3. 후보를 찾으면 그 record_id 를 유지하고 UpdateItem (먼저 저장된 키가 이긴다)
  4. 없으면 새 레코드로 PutItem

속성별 병합 우선순위 (양방향 동일):
  duration_ms, rows_examined, rows_sent, lock_time_ms  → slowlog > polled
  sql_text                                             → 더 긴 쪽 (단 정책 위반 시 마스킹본)
  plan_*                                               → for_connection > rerun > none
  started_at_ms                                        → 더 이른 쪽
  capture_source                                       → 병합됐으면 "merged"
  duration_source                                      → 채택한 duration 의 출처
  literal_policy / literal_policy_at_ms                → 먼저 기록된 쪽 유지 (F2)
```

이 절차를 `SlowQueryStore::upsert_merged()` 하나로 구현해 두 경로가 같은 코드를 쓰게 한다.
경로마다 따로 구현하면 다시 비대칭이 된다.

### 8.3 백필

```
FilterLogEvents(logGroupName='/aws/rds/instance/<id>/slowquery',
                startTime, endTime, nextToken)
```
- 인스턴스별 병렬(동시 8개). 리전별 API 레이트 리밋 주의.
- 체크포인트: `CKPT / cwlog#<instance_id>` 에 마지막 처리 타임스탬프 저장.
- 백필된 레코드는 TTL을 원래 이벤트 시각 기준으로 계산한다. 31일보다 오래된 구간을
  백필하면 DynamoDB에 넣어도 즉시 만료되므로, **31일 초과 구간은 Iceberg에 직접 적재**한다
  (별도 경로: 파싱 결과를 Parquet으로 S3에 쓰고 `MERGE INTO`).

**⚠ 이 경로는 "아카이브는 파생물" 불변식의 유일한 예외다 (F12)**
[02 §1](02-architecture.md)과 [ADR-004](03-decisions.md)는 "이중 쓰기가 없으므로 두 저장소가
갈라질 구조적 여지가 없다"고 단언한다. 31일 초과 백필은 **DynamoDB를 거치지 않는 두 번째
쓰기 경로**이므로 그 주장이 이 경로에는 적용되지 않는다.

따라서 이 경로에만 적용되는 규칙을 명시한다.

| 규칙 | 내용 |
|---|---|
| 출처 표시 | `capture_source = "backfill"`. 재파생 불가한 행임을 데이터로 남긴다 |
| 중복 방지 | `record_id` 기준 `MERGE INTO`. 같은 구간 재백필이 멱등 |
| 병합 주체 없음 | 이후 같은 실행의 다른 소스가 도착해도 DynamoDB에 없으므로 병합할 수 없다. 백필 레코드는 **슬로우로그 단독 정보**로 확정된다 |
| 리포트 영향 | 백필로 과거 파티션이 바뀌면 **이미 생성된 월간 리포트가 낡는다.** 영향받는 리포트(대상 기간이 겹치는 것)를 `stale=true`로 표시하고 재생성을 제안한다 |
| 리터럴 정책 | 백필 시점의 인스턴스 정책을 적용한다. 인스턴스가 삭제됐으면 `masked`로 강제 |
| 아카이브 잡과의 경합 | 백필은 아카이브 잡과 **같은 리더 리스**를 요구한다(동시에 같은 파티션을 MERGE하지 않게) |

[ADR-017](03-decisions.md)의 스냅샷 고정은 **재현만** 보장하고 최신화는 보장하지 않는다.
그래서 `stale` 표시가 필요하다.

## 9. 자가진단 (FR-DSC-10)

인스턴스 등록 시와 이후 1시간마다 실행한다.

| 점검 | 쿼리/방법 | 실패 시 |
|---|---|---|
| 엔진 버전 하한 | `SELECT VERSION()` + RDS API `EngineVersion` | `unsupported_version`, 수집 비활성 |
| 네트워크 도달 | TCP 연결 | `unreachable` + SG 확인 안내 |
| TLS 검증 | 연결 시 CA 검증 | `tls_failed` + CA 번들 안내 |
| IAM DB Auth | RDS API `IAMDatabaseAuthenticationEnabled` | 폴백(비밀번호) 사용 안내 |
| `performance_schema` | `SELECT @@performance_schema` | `ps_off` + 파라미터 그룹 변경 안내 |
| consumer 활성 | `SELECT NAME,ENABLED FROM performance_schema.setup_consumers WHERE NAME IN ('events_statements_current','statements_digest','global_instrumentation','thread_instrumentation')` | 어느 소스가 불가한지 명시 |
| 계정 권한 | `SHOW GRANTS FOR CURRENT_USER()` | 부족한 GRANT 목록 표시 |
| 다이제스트 길이 | `SELECT @@max_digest_length, @@performance_schema_max_digest_length` | 4096 미만이면 권고. **해시를 바꾸는 것은 앞쪽 변수다** ([19 §E](19-m1-findings.md)) |
| SQL 텍스트 길이 | `SELECT @@performance_schema_max_sql_text_length` | 참고 정보 |
| 다이제스트 테이블 크기 | `SELECT @@performance_schema_digests_size` + 오버플로 행 존재 | 오버플로 시 상향 권고 |
| 슬로우로그 설정 | RDS API `EnabledCloudwatchLogsExports` + `SELECT @@slow_query_log, @@long_query_time` | FR-CWL 미적용 표시 |
| sys 스키마 | `SELECT 1 FROM sys.version` | 인덱스 위생 기능 비활성 |
| 서버 Uptime | `performance_schema.global_status` 의 `Uptime` (§2.6에서 이미 수집) | 7일 미만이면 인덱스 위생 신뢰도 낮음 표시 |

### 9.1 조용한 0데이터 감지 (F15)

**자가진단 1시간 주기로는 부족하다.** `setup_consumers`는 **재시작 없이** 바뀐다.
`events_statements_current` 또는 `statements_digest` consumer가 꺼지면:
- 쿼리는 **성공하고 0행을 반환**한다
- 서킷 브레이커(실패 카운트)는 발화하지 않는다
- `CollectStaleness`(tick 성공 여부)도 발화하지 않는다
- → 최악 1시간 동안 **"슬로우 쿼리 없음 = 건강함"**으로 표시된다

이게 관측 도구의 가장 나쁜 실패 모드다. `performance_schema` 자체가 꺼지는 경우만
[02 §9](02-architecture.md)에 있었다.

**소스별 "마지막 유효 데이터 수신 시각"을 분리해 관측한다.**

```
digest_last_nonempty_at   : digest 쿼리가 1행 이상 반환한 마지막 시각
stmtcur_last_nonempty_at  : events_statements_current 조회가 성공한 마지막 시각
status_last_nonempty_at   : global_status 조회가 값을 반환한 마지막 시각

digest 쿼리가 0행을 5회 연속(5분) 반환하면:
  → 즉시 setup_consumers 재확인 (자가진단 주기를 기다리지 않는다)
  → 비활성이면 인스턴스 상태 = degraded + 사유 표시 + 알림
  → 활성인데 계속 0행이면 정상일 수 있다 (유휴 인스턴스) → 경고만
```

**"유휴 인스턴스"와 "consumer 꺼짐"을 구분하는 방법**: `global_status`의 `Queries` 델타가
0이 아니면(쿼리가 실행되고 있으면) 다이제스트가 0행일 수 없다. 이 조합으로 판정한다.

| `Queries` 델타 | `digest` 행 수 | 판정 |
|---|---|---|
| > 0 | 0 | **consumer 꺼짐 의심** → 즉시 재확인 |
| 0 | 0 | 유휴 인스턴스 → 정상 |
| > 0 | > 0 | 정상 |

알림 소스에 `collector.silent_zero_data`를 추가한다.

결과는 `dbmon-config / DIAG / <instance_id>`에 캐시하고 UI에 그대로 노출한다.
**"왜 이 인스턴스가 수집되지 않는가"에 화면이 스스로 답해야 한다.** 1세대에서 가장 자주 물은
질문이 이거였다.

## 10. 부하 자기 계측 (FR-CAP-10)

수집기가 대상 DB에 준 부하를 스스로 측정한다.

- 인스턴스별 초당 발행 쿼리 수, 평균/p95 응답시간, 현재 연결 수 (우리 쪽 계측)
- **대상 DB 관점의 우리 부하는 계정 기반으로 얻는다.**
  ```sql
  /* dbmon:selfload */
  SELECT event_name, total, total_latency, max_latency
  FROM sys.user_summary_by_statement_type
  WHERE user = 'dbmon';
  ```
  또는 `performance_schema.events_statements_summary_by_account_by_event_name
  WHERE USER = 'dbmon'`. 계정 단위 집계는 다이제스트 정규화의 영향을 받지 않는다.
  → 우리 쿼리의 총 지연이 서버 전체(`sys.user_summary` 전 계정 합)의 몇 %인지 계산해 표시.

**자기 관측 제외 (반드시 필요)** — 우리 `detect` 쿼리는 1초 주기로 돌므로 **대상 DB의
다이제스트 통계에서 실행 횟수 상위권**에 든다. 제외하지 않으면 상위 N 후보·`_other`·
관측 커버리지·계정 롤업이 모두 오염된다.

주석 기반 제외(`DIGEST_TEXT LIKE '/* dbmon:%'`)는 **작동하지 않는다** — 다이제스트가 주석을
제거한다. 두 가지로 대체한다.

1. **다이제스트 화이트리스트** — 기동 시 우리가 쓰는 문장들을
   `SELECT STATEMENT_DIGEST(?)`로 계산해 인스턴스별 `self_digests` 집합을 만든다.
   스냅샷 처리 시 이 집합에 속한 `DIGEST`를 별도 집계로 분리한다.
   ([ADR-011](03-decisions.md)의 `STATEMENT_DIGEST()` 활용)
2. **폴백** — `STATEMENT_DIGEST()`를 쓸 수 없으면, 우리 문장의 정규화 텍스트로 `app_digest`를
   계산해 그 집합과 대조한다. 다이제스트 스냅샷의 `DIGEST_TEXT`를 정규화하면 우리 것과
   같은 `app_digest`가 나온다.

분리된 자기 집계는 UI의 "수집기 부하" 패널에 표시하고, 일반 워크로드 통계에서는 제외한다.

## 11. 테스트 전략 요약

| 대상 | 방법 |
|---|---|
| 정규화 수렴 | 골든 코퍼스 프로퍼티 테스트 (testcontainers MySQL 8.4 + 8.0.32) |
| 델타 계산 | 리셋·신규·감소 케이스 단위 테스트 |
| in-flight 상태 머신 | 시간 주입 가능한 페이크 클록 + 스크립트된 processlist 시퀀스 |
| 스레드 재사용 오탐 | 같은 ID로 다른 다이제스트가 나타나는 시퀀스 |
| 플랜 실패 분류 | 실패 메시지 → 분류 매핑 테이블 테스트 |
| 슬로우로그 파싱 | 실제 RDS 로그 샘플 + 엣지 케이스(멀티라인, 유니코드, 세미콜론 in string) |
| 3소스 병합 | 같은 실행을 3소스로 만들어 하나로 합쳐지는지 |
| 샤드 리스 | DynamoDB Local로 워커 2개 경합 시뮬레이션 |
| 백오프/서킷 | 실패 주입 |
| 부하 | 모의 MySQL 엔드포인트 500개 |

상세는 [15-testing.md](15-testing.md).
