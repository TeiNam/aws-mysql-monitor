//! 대상 DB 에 보내는 **문장 전량**.
//!
//! 한곳에 모으는 이유는 리뷰와 정적 검사다. "이 도구가 대상 DB 에 무엇을 하는가"를
//! 한 파일로 확인할 수 있어야 한다 — DBA 가 승인해야 하는 목록이기도 하다.
//!
//! **읽기 전용 불변식**: 이 파일에 `INSERT`·`UPDATE`·`DELETE`·`ALTER`·`DROP`·
//! `TRUNCATE`·`ANALYZE`·`FLUSH`·`SET GLOBAL` 이 하나도 없다.
//! [`tests::no_write_statements`] 가 이걸 강제한다 (FR-CAP-09, FR-AI-10).
//!
//! 모든 문장에 `/* dbmon:<purpose> */` 주석을 붙인다. 이건 **사람이 읽는 용도**다 —
//! `SHOW PROCESSLIST`·슬로우로그·감사 로그에서는 주석이 살아 있다.
//! **다이제스트 통계에서는 주석이 제거되므로 자기 식별에 쓸 수 없다**
//! ([ADR-005](../../../../docs/03-decisions.md)) — 그건 계정 기반으로 한다.

/// 탐지 — 1초 주기. **정상 상태에서 tick 당 이 쿼리 1건만 나간다.**
///
/// `performance_schema.processlist` 를 쓰는 이유: `information_schema.PROCESSLIST` 와 달리
/// 스레드 목록 뮤텍스를 잡지 않는다.
///
/// `INFO` 를 읽지 않는다 — 1,024바이트 절단본이라 무의미하다.
/// `UNIX_TIMESTAMP(NOW(6))` 를 컬럼으로 붙여 시계 오프셋을 같은 왕복에서 얻는다(F14).
/// **0행이면 시각도 오지 않는다** — 그때는 [`DB_NOW`] 로 따로 읽는다.
pub fn detect(schema_count: usize, user_count: usize) -> String {
    let schema_filter = if schema_count == 0 {
        String::new()
    } else {
        format!(
            "  AND (DB IS NULL OR DB NOT IN ({}))\n",
            placeholders(schema_count)
        )
    };
    let user_filter = if user_count == 0 {
        String::new()
    } else {
        format!(
            "  AND (USER IS NULL OR USER NOT IN ({}))\n",
            placeholders(user_count)
        )
    };
    format!(
        "/* dbmon:detect */
SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, UNIX_TIMESTAMP(NOW(6)) AS db_now
FROM performance_schema.processlist
WHERE INFO IS NOT NULL
  AND COMMAND NOT IN ('Sleep','Daemon','Binlog Dump','Binlog Dump GTID','Connect')
  AND TIME >= ?
  AND ID <> CONNECTION_ID()
{schema_filter}{user_filter}ORDER BY TIME DESC
LIMIT ?"
    )
}

/// 전문 SQL — 임계값 초과 스레드만.
///
/// `INFO` 는 `varchar(21845)` 이고 65,535바이트에서 잘린다([19 §A](../../../../docs/19-m1-findings.md)).
///
/// # 이 컬럼은 `utf8mb3` 다 — 4바이트 문자가 `?` 로 손실된다 (19 §A-2)
///
/// 실측: `SELECT '📊emoji'` 를 실행하면
/// `information_schema.PROCESSLIST.INFO` 는 `27 3F 65...`(`'?emoji'`),
/// `performance_schema.events_statements_current.SQL_TEXT` 는 `27 F0 9F 93 8A 65...` 다.
/// 손실은 서버가 IS 테이블을 채울 때 일어나므로 `CONVERT(... USING utf8mb4)` 로 복구할 수 없다.
/// 그래서 `stmt_current` 가 무손실 `SQL_TEXT` 를 함께 읽고 `pick_sql_text` 가 고른다.
pub fn full_sql(id_count: usize) -> String {
    format!(
        "/* dbmon:fulltext */
SELECT ID, DB, USER, HOST, TIME, INFO
FROM information_schema.PROCESSLIST
WHERE ID IN ({})",
        placeholders(id_count)
    )
}

/// 정확 지표 + 다이제스트 — 임계값 초과 스레드만.
///
/// `TIMER_WAIT`·`LOCK_TIME` 은 **피코초**다. ms 변환은 1e9 로 나눈다
/// (1세대의 흔한 버그: 마이크로초로 착각 → 1000배 오차).
pub fn stmt_current(id_count: usize) -> String {
    format!(
        "/* dbmon:stmtcurrent */
SELECT t.PROCESSLIST_ID, t.THREAD_ID,
       e.EVENT_NAME, e.CURRENT_SCHEMA,
       e.DIGEST, e.DIGEST_TEXT, e.SQL_TEXT,
       e.TIMER_WAIT, e.LOCK_TIME,
       e.ROWS_EXAMINED, e.ROWS_SENT, e.ROWS_AFFECTED,
       e.CREATED_TMP_TABLES, e.CREATED_TMP_DISK_TABLES,
       e.SELECT_FULL_JOIN, e.SORT_MERGE_PASSES,
       e.NO_INDEX_USED, e.NO_GOOD_INDEX_USED,
       e.NESTING_EVENT_TYPE
FROM performance_schema.events_statements_current e
JOIN performance_schema.threads t USING (THREAD_ID)
WHERE t.PROCESSLIST_ID IN ({})",
        placeholders(id_count)
    )
}

/// 다이제스트 스냅샷 ① — **지표 컬럼만**. 큰 텍스트 컬럼을 제외한다.
///
/// `LAST_SEEN` 필터와 컬럼 분리가 없으면 응답이 100배 커진다
/// ([05 §2.5.3](../../../../docs/05-collector.md)): 필터 없이 전량을 매분 가져오면
/// 인스턴스당 일 18GB, 500대면 일 9TB 다.
///
/// `last_seen_gte` 가 `None` 이면 첫 스냅샷이다(기준선 수립).
pub fn digest_snapshot(has_last_seen: bool) -> String {
    let filter = if has_last_seen {
        "  AND LAST_SEEN >= FROM_UNIXTIME(? / 1000)\n"
    } else {
        ""
    };
    format!(
        "/* dbmon:digest */
SELECT SCHEMA_NAME, DIGEST,
       COUNT_STAR,
       SUM_TIMER_WAIT, MIN_TIMER_WAIT, AVG_TIMER_WAIT, MAX_TIMER_WAIT,
       SUM_LOCK_TIME, SUM_ERRORS, SUM_WARNINGS,
       SUM_ROWS_AFFECTED, SUM_ROWS_SENT, SUM_ROWS_EXAMINED,
       SUM_CREATED_TMP_TABLES, SUM_CREATED_TMP_DISK_TABLES,
       SUM_SELECT_FULL_JOIN, SUM_SELECT_SCAN,
       SUM_SORT_MERGE_PASSES,
       SUM_NO_INDEX_USED, SUM_NO_GOOD_INDEX_USED,
       UNIX_TIMESTAMP(FIRST_SEEN) * 1000 AS first_seen_ms,
       UNIX_TIMESTAMP(LAST_SEEN) * 1000 AS last_seen_ms,
       QUANTILE_95, QUANTILE_99,
       UNIX_TIMESTAMP(NOW(6)) AS db_now
FROM performance_schema.events_statements_summary_by_digest
WHERE DIGEST IS NOT NULL
{filter}  AND (SCHEMA_NAME IS NULL OR SCHEMA_NAME NOT IN
       ('mysql','sys','performance_schema','information_schema'))"
    )
}

/// 다이제스트 스냅샷 ② — 처음 본 다이제스트의 **텍스트만**.
///
/// 워커 메모리의 `DIGEST → app_digest` 캐시에 없을 때만 실행한다.
/// 안정 상태에서는 실행 횟수가 0에 가깝다.
pub fn digest_texts(count: usize) -> String {
    format!(
        "/* dbmon:digesttext */
SELECT DIGEST, DIGEST_TEXT, QUERY_SAMPLE_TEXT
FROM performance_schema.events_statements_summary_by_digest
WHERE DIGEST IN ({})",
        placeholders(count)
    )
}

/// 다이제스트 테이블 오버플로 감지. `DIGEST IS NULL` 집계 행이 있으면 한도에 찼다.
pub const DIGEST_OVERFLOW: &str = "/* dbmon:digestoverflow */
SELECT COUNT_STAR FROM performance_schema.events_statements_summary_by_digest
WHERE DIGEST IS NULL";

/// 실시간 지표 — 5초 주기.
pub const GLOBAL_STATUS: &str = "/* dbmon:status */
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
)";

/// 대상 DB 시각. `probe` 가 0행일 때 시계 오프셋을 갱신한다.
pub const DB_NOW: &str = "/* dbmon:dbnow */ SELECT UNIX_TIMESTAMP(NOW(6))";

/// 연결 확인.
pub const PING: &str = "/* dbmon:ping */ SELECT 1";

/// 우리 문장의 다이제스트를 서버에 계산시킨다 — 자기 제외 화이트리스트용
/// ([05 §10](../../../../docs/05-collector.md)).
pub const STATEMENT_DIGEST: &str = "/* dbmon:selfdigest */ SELECT STATEMENT_DIGEST(?)";

/// 대상 인스턴스의 **전역** `sql_mode`. 앱 세션이 상속하는 값이다.
///
/// 세션 값(`@@session.sql_mode`)이 아니라 전역 값을 본다 — 우리 세션은 `''` 로
/// 고정했으므로 세션 값은 항상 비어 있다.
pub const GLOBAL_SQL_MODE: &str = "/* dbmon:sqlmode */ SELECT @@global.sql_mode";

/// 세션 초기화. **커넥션 수립 직후와 풀 반납 후 재사용 시** 실행된다(`setup`).
///
/// `max_execution_time` 은 `EXPLAIN` 에도 적용된다(8.4.11 실측: 50ms 상한에서 ERROR 3024).
/// `transaction_isolation='READ-COMMITTED'` 는 우리 조회가 다른 세션의 잠금·스냅샷에
/// 영향을 주지 않게 한다. `SET SESSION` 이므로 대상 DB 전역 설정을 바꾸지 않는다.
pub fn session_init(query_timeout_ms: u64) -> String {
    // **`sql_mode` 를 고정한다. 이것은 보안 통제다.**
    //
    // 고정하지 않으면 대상 인스턴스의 global `sql_mode` 를 그대로 상속한다. 거기에
    // `NO_BACKSLASH_ESCAPES` 가 있으면 서버는 `\` 를 이스케이프로 보지 않는데
    // 우리 렉서는 항상 이스케이프로 처리한다 — **검증기와 실행기가 문자열 경계를 다르게
    // 본다.** 그 틈으로 주입된 문장이 실제로 실행된다. 8.4.11 실측:
    //
    // ```text
    // 입력 (PROCESSLIST.INFO 에서 온 신뢰 불가 텍스트):
    //   SELECT * FROM orders WHERE memo = '\'; SELECT 31337 AS pwned; SELECT 1 -- '
    //
    // 우리 렉서: '\'…' 를 문자열 하나로 삼킨다 → `;` 토큰 0개 → 멀티문장 검사 통과
    //
    // sql_mode=''                    → Com_select +1  (EXPLAIN 만)
    // sql_mode=NO_BACKSLASH_ESCAPES  → SELECT 31337 AS pwned 가 **실행된다**
    // ```
    //
    // `mysql_async` 는 `CLIENT_MULTI_STATEMENTS` 를 무조건 켜고 끌 방법이 없으므로
    // 이 고정이 유일한 차단점이다. `ANSI_QUOTES` 도 같은 부류라 함께 닫힌다.
    //
    // 빈 문자열로 두는 이유: 특정 모드를 열거하면 새 MySQL 버전이 기본값에 모드를
    // 추가할 때 다시 벌어진다. **아무 모드도 없는 상태**가 렉서의 전제와 일치한다.
    format!(
        "SET SESSION sql_mode = '', max_execution_time = {query_timeout_ms}, \
         transaction_isolation = 'READ-COMMITTED', autocommit = 1"
    )
}

/// 계획 JSON 형식을 v2 로 올린다 (MySQL 8.3+).
///
/// # 왜 세션 초기화에 넣지 않는가
///
/// `session_init` 은 한 문장에 여러 변수를 세운다. 거기에 이 변수를 넣으면 8.3 미만에서
/// **초기화 전체가 1193 으로 실패**해 그 인스턴스의 모든 연결이 죽는다. 버전 판정이
/// 틀리는 경우(관리형 엔진이 커뮤니티 버전만 8.4 로 보고하는 등)까지 감당하려면
/// 재실행 직전에 **따로** 보내고 실패는 v1 로 흘려야 한다.
pub const EXPLAIN_JSON_V2: &str =
    "/* dbmon:explainv2 */ SET SESSION explain_json_format_version = 2";

/// `EXPLAIN ... FOR CONNECTION <id>`.
///
/// **연결 ID 를 파라미터 바인딩할 수 없다** — 실측에서 `FOR CONNECTION CONNECTION_ID()` 가
/// `ERROR 1064` 였다. 리터럴 정수만 받는다. 그래서 인자를 `u64` 로 받아 **타입으로 강제**한다.
pub fn explain_for_connection(connection_id: u64, format: ExplainFormat) -> String {
    format!(
        "/* dbmon:plan */ EXPLAIN {} FOR CONNECTION {connection_id}",
        format.clause()
    )
}

/// `EXPLAIN ... <문장>` 재실행. **RDS 의 기본 플랜 수집 경로다.**
///
/// `sql` 은 `dbmon_normalize::plan_query()` 를 통과한 것이어야 한다 — 멀티문장과
/// 잘린 SQL 을 거기서 거부한다.
pub fn explain_rerun(sql: &str, format: ExplainFormat) -> String {
    format!("/* dbmon:planrerun */ EXPLAIN {} {sql}", format.clause())
}

/// 기본 스키마를 지정하는 문장. **식별자를 엄격히 검증한다.**
///
/// 스키마 이름은 **대상 DB 에서 온 값**이다(처리목록의 `db` 열) — L1 위협 모델에서
/// 공격자 통제 하에 있을 수 있으므로 값으로 신뢰하지 않는다. MySQL 식별자 규칙보다
/// 더 좁게 `[A-Za-z0-9_$]` 와 64자로 제한하고, 벗어나면 **문장을 만들지 않는다**
/// (`None`). 백틱 이스케이프에 기대는 대신 애초에 통과시키지 않는 쪽이다.
pub fn use_schema(schema: &str) -> Option<String> {
    let ok = !schema.is_empty()
        && schema.len() <= 64
        && schema
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$');
    ok.then(|| format!("/* dbmon:useschema */ USE `{schema}`"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainFormat {
    Json,
    /// 사람이 읽기 쉽다. 8.4 에서 `FOR CONNECTION` 과 함께도 동작한다(M1-3 실측).
    Tree,
}

impl ExplainFormat {
    fn clause(self) -> &'static str {
        match self {
            Self::Json => "FORMAT=JSON",
            Self::Tree => "FORMAT=TREE",
        }
    }
}

/// `?, ?, ?` — 바인딩 자리표. **값을 문자열로 이어붙이지 않는다** (T-18).
fn placeholders(n: usize) -> String {
    if n == 0 {
        // 빈 IN 절은 구문 오류다. 호출자가 0을 넘기지 않아야 하지만, 방어적으로
        // 아무것도 매칭하지 않는 값을 둔다.
        return "NULL".to_string();
    }
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 이 파일에 쓰기 문장이 하나도 없어야 한다 (FR-CAP-09).
    ///
    /// 소스를 직접 읽어 검사한다 — 상수 목록을 따로 관리하면 새 상수가 빠진다.
    #[test]
    fn no_write_statements() {
        // **테스트 모듈 이전만** 본다 — 아래 금지어 목록 자체가 걸리면 안 된다.
        let src = include_str!("sql.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("소스가 비어 있을 수 없다");
        // 문서·주석에는 금지어가 나올 수 있으므로 **문장처럼 쓰인 형태**만 본다.
        let forbidden = [
            "INSERT INTO",
            "UPDATE ",
            "DELETE FROM",
            "ALTER TABLE",
            "DROP TABLE",
            "TRUNCATE TABLE",
            "ANALYZE TABLE",
            "FLUSH ",
            "SET GLOBAL",
            "CREATE TABLE",
            "GRANT ",
            "REVOKE ",
            "KILL ",
        ];
        for line in src.lines() {
            let code = line.trim();
            // 주석·문서 줄은 건너뛴다.
            if code.starts_with("//") || code.starts_with("///") || code.starts_with("*") {
                continue;
            }
            let upper = code.to_uppercase();
            for f in forbidden {
                assert!(
                    !upper.contains(f),
                    "쓰기 문장이 들어왔다: {f:?}\n  줄: {line}"
                );
            }
        }
    }

    #[test]
    fn placeholders_are_bound_not_interpolated() {
        assert_eq!(placeholders(3), "?, ?, ?");
        assert_eq!(placeholders(1), "?");
        // 0개를 넘겨도 구문 오류가 나지 않아야 한다.
        assert_eq!(placeholders(0), "NULL");
    }

    #[test]
    fn detect_omits_empty_filters() {
        let none = detect(0, 0);
        assert!(!none.contains("DB NOT IN"), "{none}");
        assert!(!none.contains("USER NOT IN"), "{none}");
        // 임계값과 LIMIT 은 항상 바인딩이다.
        assert_eq!(none.matches('?').count(), 2, "{none}");

        let both = detect(4, 1);
        assert!(both.contains("DB NOT IN (?, ?, ?, ?)"), "{both}");
        assert!(both.contains("USER NOT IN (?)"), "{both}");
        assert_eq!(both.matches('?').count(), 2 + 4 + 1);
    }

    #[test]
    fn detect_excludes_self_connection_and_reads_db_time() {
        let q = detect(0, 0);
        assert!(
            q.contains("ID <> CONNECTION_ID()"),
            "자기 자신을 후보로 넣으면 안 된다"
        );
        assert!(
            q.contains("NOW(6)"),
            "시계 오프셋 추정용 시각이 필요하다 (F14)"
        );
        assert!(!q.contains("INFO,"), "PS 의 INFO 는 절단본이라 읽지 않는다");
    }

    #[test]
    fn digest_snapshot_filter_is_optional_but_present_when_asked() {
        assert!(!digest_snapshot(false).contains("LAST_SEEN >="));
        let filtered = digest_snapshot(true);
        assert!(
            filtered.contains("LAST_SEEN >= FROM_UNIXTIME(? / 1000)"),
            "{filtered}"
        );
        // 텍스트 컬럼이 ① 쿼리에 없어야 한다 — 페이로드가 100배 커진다.
        assert!(!filtered.contains("DIGEST_TEXT"), "{filtered}");
        assert!(!filtered.contains("QUERY_SAMPLE_TEXT"), "{filtered}");
    }

    #[test]
    fn digest_texts_only_fetches_text_columns() {
        let q = digest_texts(2);
        assert!(q.contains("DIGEST_TEXT"));
        assert!(q.contains("QUERY_SAMPLE_TEXT"));
        assert!(!q.contains("COUNT_STAR"), "지표는 ① 쿼리가 가져온다");
    }

    #[test]
    fn explain_for_connection_takes_typed_integer() {
        let q = explain_for_connection(8842119, ExplainFormat::Json);
        assert!(q.ends_with("FOR CONNECTION 8842119"), "{q}");
        assert!(q.contains("FORMAT=JSON"));
        assert!(explain_for_connection(1, ExplainFormat::Tree).contains("FORMAT=TREE"));
    }

    /// **스키마 이름은 대상 DB 에서 온 값이다.** 식별자로 쓸 수 없는 값이 오면
    /// 문장을 만들지 않는다 — 백틱 이스케이프의 정확성에 기대지 않는다.
    #[test]
    fn use_schema_rejects_anything_that_is_not_a_plain_identifier() {
        assert_eq!(
            use_schema("shop").as_deref(),
            Some("/* dbmon:useschema */ USE `shop`")
        );
        assert!(use_schema("app_1$").is_some());
        for bad in [
            "",
            "shop`; DROP DATABASE x; --",
            "shop-1",
            "shop schema",
            "샵",
            "shop.orders",
            "`shop`",
            &"x".repeat(65),
        ] {
            assert!(use_schema(bad).is_none(), "거부해야 한다: {bad}");
        }
    }

    #[test]
    fn explain_rerun_prefixes_the_statement() {
        let q = explain_rerun("SELECT 1", ExplainFormat::Json);
        assert!(q.ends_with("EXPLAIN FORMAT=JSON SELECT 1"), "{q}");
    }

    #[test]
    fn session_init_sets_only_session_scope() {
        let q = session_init(3000);
        assert!(q.starts_with("SET SESSION"));
        assert!(q.contains("max_execution_time = 3000"));
        // **보안 통제**: 고정하지 않으면 NO_BACKSLASH_ESCAPES 를 상속해
        // 렉서와 서버가 문자열 경계를 다르게 보고 주입 문장이 실행된다.
        assert!(
            q.contains("sql_mode = ''"),
            "sql_mode 를 빈 문자열로 고정해야 한다: {q}"
        );
        assert!(
            !q.to_uppercase().contains("GLOBAL"),
            "전역 설정을 바꾸면 안 된다"
        );
    }

    #[test]
    fn every_statement_is_tagged_for_human_readers() {
        for q in [
            detect(1, 1),
            full_sql(1),
            stmt_current(1),
            digest_snapshot(true),
            digest_texts(1),
            DIGEST_OVERFLOW.into(),
            GLOBAL_STATUS.into(),
            DB_NOW.into(),
            PING.into(),
            STATEMENT_DIGEST.into(),
            explain_for_connection(1, ExplainFormat::Json),
            explain_rerun("SELECT 1", ExplainFormat::Json),
        ] {
            assert!(q.contains("/* dbmon:"), "태그가 없다: {q}");
        }
    }
}
