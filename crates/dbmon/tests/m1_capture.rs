//! M1-1 ~ M1-4, M1-17 — 캡처 경로 검증 스파이크.
//!
//! **[ADR-005](../../../../.claude/docs/03-decisions.md) 의 핵심 전제를 확인한다**:
//! `information_schema.PROCESSLIST.INFO` 는 절단되지 않고,
//! `performance_schema.processlist.INFO` 는 1024바이트에서 잘린다.
//! 이게 1세대의 실제 버그를 고치는 방법이므로 틀리면 설계가 바뀐다.
//!
//! ```
//! docker compose up -d
//! cargo test -p dbmon --test m1_capture -- --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` 이 필요하다 — 여러 테스트가 동시에 장기 실행 쿼리를 걸면
//! processlist 측정이 서로 간섭한다.

// **장기 실행 쿼리를 `ROOT` 로 만든다.** `it_collector` 의 `reset_targets` 가
// `USER <> 'root'` 인 쿼리를 모두 죽이는데, 이 바이너리는 그 락(`exclusive_target`)을
// 잡지 않는다 — 그리고 그 락은 **테스트 바이너리를 넘지 않으므로** 잡아도 소용없다.
// `ROOT` 로 붙으면 리셋 대상에서 제외된다. 측정 대상은 텍스트 길이·다이제스트이므로
// 실행 계정은 결과에 영향이 없다.
mod support;

use mysql_async::prelude::*;
use support::*;

/// M1-1 — 세 소스의 SQL 텍스트 길이. **ADR-005 의 전제.**
#[tokio::test]
async fn m1_1_information_schema_processlist_info_is_not_truncated() {
    let mut probe = conn_or_skip!(MYSQL84, ROOT);
    let ps_limit: Option<u64> = var(&mut probe, "performance_schema_max_sql_text_length")
        .await
        .and_then(|v| v.parse().ok());

    let mut rows: Vec<(String, String)> = vec![
        (
            "performance_schema_max_sql_text_length".into(),
            ps_limit.map(|v| v.to_string()).unwrap_or("?".into()),
        ),
        (
            "─ 목표길이 → (PS processlist / IS PROCESSLIST / stmt_current)".into(),
            String::new(),
        ),
    ];
    let mut is_within_ceiling = true;
    let mut is_truncated_above_ceiling = false;
    let mut ps_truncated_somewhere = false;

    for target_bytes in [4_096usize, 16_384, 65_536, 1_048_576] {
        let sql = long_running_sql(target_bytes, 3.0);
        let actual = sql.len();
        let Some(mut running) = start_long_query(MYSQL84, ROOT, sql).await else {
            eprintln!("[skip] 장기 실행 쿼리를 시작할 수 없다");
            return;
        };
        let m = measure_text_lengths(&mut probe, running.connection_id).await;
        kill_query(&mut probe, running.connection_id).await;
        running.join().await;

        let fmt = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or("없음".into());
        rows.push((
            format!("{:>9} 바이트 (실제 {actual})", target_bytes),
            format!(
                "{} / {} / {}",
                fmt(m.ps_processlist_info),
                fmt(m.is_processlist_info),
                fmt(m.stmt_current_sql_text)
            ),
        ));

        // `information_schema` 는 상한(65535바이트) 안에서는 원본과 같아야 한다.
        if let Some(len) = m.is_processlist_info {
            let expected = actual.min(IS_PROCESSLIST_INFO_MAX_BYTES);
            if len as usize != expected {
                is_within_ceiling = false;
                rows.push(("  ⚠ 예상과 다름".into(), format!("{len} != {expected}")));
            }
            if (len as usize) < actual {
                is_truncated_above_ceiling = true;
            }
        }
        if let Some(len) = m.ps_processlist_info
            && (len as usize) < actual
        {
            ps_truncated_somewhere = true;
        }
    }

    rows.push((
        "→ 결론".into(),
        format!(
            "IS PROCESSLIST.INFO 는 {}바이트까지 온전하고 그 이상은 절단된다. \
             PS processlist.INFO 는 1024바이트. 64배 개선 → ADR-005 유효(단서 있음)",
            IS_PROCESSLIST_INFO_MAX_BYTES
        ),
    ));
    report("M1-1 SQL 텍스트 절단", &rows);

    assert!(
        ps_truncated_somewhere,
        "performance_schema.processlist.INFO 가 절단되지 않았다 \
         — 1세대 버그의 전제가 이 환경에서 성립하지 않는다"
    );
    assert!(
        is_within_ceiling,
        "information_schema.PROCESSLIST.INFO 의 절단 지점이 {IS_PROCESSLIST_INFO_MAX_BYTES}바이트가 \
         아니다. 상한이 바뀌었으면 sql_text_truncated 판정 기준을 고쳐야 한다."
    );
    assert!(
        is_truncated_above_ceiling,
        "상한을 넘는 SQL 에서도 절단이 관측되지 않았다 — 측정이 잘못됐을 수 있다"
    );
}

/// `information_schema.PROCESSLIST.INFO` 의 실제 상한 (바이트).
///
/// **설계는 이 컬럼이 `LONGTEXT` 라 절단되지 않는다고 적었다. 사실이 아니다.**
/// 실측(8.4.11 / 8.0.46): `varchar(21845)` 이고 `LENGTH()` 기준 65535바이트에서 잘린다
/// (21845자 × utf8mb3 3바이트 = 65535).
///
/// `performance_schema.processlist.INFO` 는 `longtext` 이지만 **내용이**
/// `performance_schema_max_sql_text_length`(기본 1024)로 잘린다 — 두 컬럼의 제약 방향이 반대다.
const IS_PROCESSLIST_INFO_MAX_BYTES: usize = 65_535;

/// `events_statements_current.SQL_TEXT` 는 `max_sql_text_length` 로 잘린다 —
/// 폴백 경로(파라미터 그룹 상향)가 실제로 효과가 있는지 확인한다.
#[tokio::test]
async fn m1_1b_sql_text_length_follows_parameter() {
    let mut narrow = conn_or_skip!(MYSQL84, ROOT);
    let mut wide = conn_or_skip!(MYSQL84_WIDE, ROOT);
    let sql = long_running_sql(16_384, 3.0);

    let mut out = Vec::new();
    for (label, target, probe) in [
        ("mysql84 (1024)", MYSQL84, &mut narrow),
        ("mysql84-wide (8192)", MYSQL84_WIDE, &mut wide),
    ] {
        let limit = var(probe, "performance_schema_max_sql_text_length")
            .await
            .unwrap_or_default();
        let Some(mut running) = start_long_query(target, ROOT, sql.clone()).await else {
            return;
        };
        let m = measure_text_lengths(probe, running.connection_id).await;
        kill_query(probe, running.connection_id).await;
        running.join().await;
        out.push((
            format!("{label} max_sql_text_length={limit}"),
            format!(
                "SQL_TEXT={} / IS.INFO={}",
                m.stmt_current_sql_text
                    .map(|v| v.to_string())
                    .unwrap_or("없음".into()),
                m.is_processlist_info
                    .map(|v| v.to_string())
                    .unwrap_or("없음".into())
            ),
        ));
    }
    report("M1-1b 폴백 경로 (max_sql_text_length 상향)", &out);
}

/// M1-2 / M1-3 — `EXPLAIN ... FOR CONNECTION` 이 SELECT·UPDATE·DELETE 에서 동작하는가.
///
/// [ADR-006](../../../../.claude/docs/03-decisions.md): 1세대는 사후 `EXPLAIN` 재실행이라 DML 플랜을
/// 포기했다. 실행 중 플랜 수집이 되면 DML 까지 커버한다.
#[tokio::test]
async fn m1_2_explain_for_connection_covers_dml() {
    let mut probe = conn_or_skip!(MYSQL84, ROOT);
    let cases: Vec<(&str, Vec<String>)> = vec![
        (
            "SELECT",
            vec!["SELECT COUNT(*) FROM orders WHERE id = 1 AND SLEEP(4) = 0".into()],
        ),
        (
            "UPDATE",
            vec![
                "START TRANSACTION".into(),
                "UPDATE orders SET memo = 'spike' WHERE id = 1 AND SLEEP(4) = 0".into(),
            ],
        ),
        (
            "DELETE",
            vec![
                "START TRANSACTION".into(),
                "DELETE FROM lock_arena WHERE id = 4 AND SLEEP(4) = 0".into(),
            ],
        ),
        (
            "INSERT ... SELECT",
            vec![
                "START TRANSACTION".into(),
                "INSERT INTO lock_arena (id, val) SELECT 90 + id, val FROM lock_arena \
                 WHERE id = 1 AND SLEEP(4) = 0"
                    .into(),
            ],
        ),
    ];

    let mut rows = Vec::new();
    for (label, statements) in cases {
        let Some(mut running) = start_long_statements(MYSQL84, ROOT, statements).await else {
            return;
        };
        let id = running.connection_id;

        let json: Result<Option<String>, _> = probe
            .query_first(format!("EXPLAIN FORMAT=JSON FOR CONNECTION {id}"))
            .await;
        let tree: Result<Option<String>, _> = probe
            .query_first(format!("EXPLAIN FORMAT=TREE FOR CONNECTION {id}"))
            .await;
        let traditional: Result<Vec<mysql_async::Row>, _> =
            probe.query(format!("EXPLAIN FOR CONNECTION {id}")).await;

        // 실제로 파싱까지 되는지 확인한다 — 플랜 문자열만 받아도 쓸모없다.
        let parsed = json
            .as_ref()
            .ok()
            .and_then(|o| o.as_deref())
            .map(|s| dbmon_planparse::parse(s, Some("shop")));

        rows.push((
            format!("{label} FORMAT=JSON"),
            match (&json, &parsed) {
                (Ok(Some(_)), Some(Ok(p))) => format!(
                    "성공. 형식={} 노드={} 참조테이블={:?}",
                    p.format.as_str(),
                    p.nodes.len(),
                    p.referenced_tables
                ),
                (Ok(Some(_)), Some(Err(e))) => format!("플랜은 왔지만 파싱 실패: {e}"),
                (Ok(None), _) => "행 없음".into(),
                (Err(e), _) => format!("실패: {}", first_line(&e.to_string())),
                _ => "?".into(),
            },
        ));
        rows.push((
            format!("{label} FORMAT=TREE"),
            match &tree {
                Ok(Some(t)) => format!("지원. {}자", t.len()),
                Ok(None) => "행 없음".into(),
                Err(e) => format!("미지원: {}", first_line(&e.to_string())),
            },
        ));
        rows.push((
            format!("{label} FORMAT=TRADITIONAL"),
            match &traditional {
                Ok(r) => format!("{}행", r.len()),
                Err(e) => format!("미지원: {}", first_line(&e.to_string())),
            },
        ));

        kill_query(&mut probe, id).await;
        running.join().await;
    }
    report("M1-2 / M1-3 EXPLAIN FOR CONNECTION", &rows);
}

/// 실패 코드 분류표를 실측으로 확정한다 ([05 §2.4](../../../../.claude/docs/05-collector.md)).
#[tokio::test]
async fn m1_2b_explain_failure_error_codes() {
    let mut probe = conn_or_skip!(MYSQL84, ROOT);
    let mut rows = Vec::new();

    // ① 존재하지 않는 커넥션 → ER_NO_SUCH_THREAD (1094 기대)
    let err = probe
        .query_drop("EXPLAIN FORMAT=JSON FOR CONNECTION 999999999")
        .await
        .unwrap_err();
    let code = mysql_error_code(&err);
    rows.push((
        "존재하지 않는 커넥션".into(),
        format!(
            "code={code:?} → {:?}",
            code.map(dbmon_core::ports::PlanFailure::from_mysql_error_code)
        ),
    ));

    // ② 유휴 커넥션(Sleep 상태) → 실행 중 문장이 없다
    let Some(idle) = connect(MYSQL84, LOADGEN).await else {
        return;
    };
    let mut idle = idle;
    let idle_id: u64 = idle
        .query_first("SELECT CONNECTION_ID()")
        .await
        .unwrap()
        .unwrap();
    let r = probe
        .query_drop(format!("EXPLAIN FORMAT=JSON FOR CONNECTION {idle_id}"))
        .await;
    rows.push((
        "유휴 커넥션".into(),
        match &r {
            Ok(()) => "성공(빈 결과)".into(),
            Err(e) => format!(
                "code={:?} msg={}",
                mysql_error_code(e),
                first_line(&e.to_string())
            ),
        },
    ));

    // ③ EXPLAIN 불가 문장 실행 중 → ER_EXPLAIN_NOT_SUPPORTED (3012 기대)
    let Some(mut running) = start_long_query(MYSQL84, ROOT, "DO SLEEP(4)").await else {
        return;
    };
    let r = probe
        .query_drop(format!(
            "EXPLAIN FORMAT=JSON FOR CONNECTION {}",
            running.connection_id
        ))
        .await;
    rows.push((
        "EXPLAIN 불가 문장 (DO SLEEP)".into(),
        match &r {
            Ok(()) => "성공".into(),
            Err(e) => format!(
                "code={:?} → {:?}",
                mysql_error_code(e),
                mysql_error_code(e).map(dbmon_core::ports::PlanFailure::from_mysql_error_code)
            ),
        },
    ));
    kill_query(&mut probe, running.connection_id).await;
    running.join().await;
    let _ = idle.disconnect().await;

    report("M1-2b EXPLAIN 실패 에러 코드", &rows);
}

/// M1-4 — 모니터링 계정으로 `EXPLAIN FOR CONNECTION` 이 되는가.
///
/// # 답: **안 된다. ADR-006 이 깨진다.**
///
/// 실측(MySQL 8.4.11 / 8.0.46)에서 확인한 것:
///
/// | 조건 | `EXPLAIN ... FOR CONNECTION <타인 커넥션>` |
/// |---|---|
/// | `PROCESS` + 스키마 `SELECT` (모드 B) | ✗ `ERROR 1045` |
/// | `PROCESS` + `SELECT ON *.*` | ✗ 1045 |
/// | `PROCESS` + `SUPER` | ✗ 1045 |
/// | `PROCESS` + `SUPER` + `SELECT ON *.*` | ✗ 1045 |
/// | RDS 마스터 유사(ALL − SUPER/FILE/SHUTDOWN) | ✗ 1045 |
/// | `GRANT ALL ON *.*` (정적 전역 권한 **전체**) | ✓ |
/// | `GRANT ALL` − 정적 권한 1개 (EVENT·SHUTDOWN 등) | ✗ 1045 |
/// | `GRANT ALL` − 동적 권한 1개 (AUDIT_ADMIN) | ✓ |
/// | **자기 커넥션**을 explain | ✓ (권한 무관) |
///
/// 즉 **정적 전역 권한 전체**가 필요하다. RDS 는 마스터 유저에게도 `SUPER`·`FILE`·
/// `SHUTDOWN` 을 주지 않으므로 **RDS 에서는 어떤 계정으로도 불가능하다.**
/// MySQL 버그 [#95850](https://bugs.mysql.com/bug.php?id=95850) 에서 개발자도
/// "`PROCESS` 만으로는 부족하다"고 인정했다(문서가 부정확하다는 것이 그 버그의 요지다).
///
/// → 이 테스트는 **불가능하다는 사실을 고정**한다. 나중에 MySQL 이 이걸 완화하면
/// 테스트가 실패하고, 그때 `for_connection` 경로를 되살리면 된다.
#[tokio::test]
async fn m1_4_for_connection_is_denied_to_least_privilege_accounts() {
    let mut root = conn_or_skip!(MYSQL84, ROOT);
    let Some(mut running) = start_long_query(
        MYSQL84,
        ROOT,
        "SELECT COUNT(*) FROM orders o JOIN customers c ON o.customer_id = c.id \
         WHERE o.status = 'PENDING' AND SLEEP(5) = 0",
    )
    .await
    else {
        return;
    };
    let id = running.connection_id;

    let mut rows = Vec::new();
    for (label, cred) in [
        ("모드 B (least, shop SELECT 있음)", MONITOR),
        ("모드 C (minimal, shop SELECT 없음)", MONITOR_MINIMAL),
    ] {
        let Some(mut c) = connect(MYSQL84, cred).await else {
            continue;
        };
        // 전문 SQL 을 볼 수 있는가 (PROCESS 권한)
        let info: Option<Option<String>> = c
            .exec_first(
                "SELECT INFO FROM information_schema.PROCESSLIST WHERE ID = ?",
                (id,),
            )
            .await
            .ok()
            .flatten();
        let sees_sql = info.flatten().is_some();

        let plan: Result<Option<String>, _> = c
            .query_first(format!("EXPLAIN FORMAT=JSON FOR CONNECTION {id}"))
            .await;
        let (ok, detail) = match &plan {
            Ok(Some(p)) => {
                let parsed = dbmon_planparse::parse(p, Some("shop"));
                match parsed {
                    Ok(pp) => (
                        true,
                        format!(
                            "성공. 조건식 노출={}",
                            pp.nodes.iter().any(|n| n.attached_condition.is_some())
                        ),
                    ),
                    Err(e) => (true, format!("플랜은 왔지만 파싱 실패: {e}")),
                }
            }
            Ok(None) => (false, "행 없음".into()),
            Err(e) => (
                false,
                format!(
                    "code={:?} → {:?}",
                    mysql_error_code(e),
                    mysql_error_code(e).map(dbmon_core::ports::PlanFailure::from_mysql_error_code)
                ),
            ),
        };
        rows.push((
            label.into(),
            format!(
                "전문SQL={sees_sql} EXPLAIN={} — {detail}",
                if ok { "가능" } else { "불가" }
            ),
        ));
        let _ = c.disconnect().await;
    }

    kill_query(&mut root, id).await;
    running.join().await;
    rows.push((
        "→ 결론".into(),
        "정적 전역 권한 전체가 필요하다. RDS 에서는 불가 → for_connection 경로를 쓸 수 없다".into(),
    ));
    report("M1-4 최소 권한으로 플랜 수집", &rows);

    // 이 단정이 실패하면 MySQL 이 요구 권한을 완화한 것이다 — 그때 설계를 되돌린다.
    assert!(
        !rows
            .iter()
            .any(|(k, v)| k.starts_with("모드 B") && v.contains("EXPLAIN=가능")),
        "모드 B 계정으로 FOR CONNECTION 이 성공했다. MySQL 동작이 바뀌었으니 \
         ADR-006 을 재검토하고 이 테스트를 갱신한다."
    );
}

/// M1-4b — `for_connection` 이 막혔을 때의 **폴백 경로**가 최소 권한으로 동작하는가.
///
/// `EXPLAIN` 재실행은 `SELECT` 권한만으로 된다. DML 은 해당 DML 권한을 요구하므로
/// (`ERROR 1142`) 읽기 전용 계정은 **DML 을 SELECT 로 바꿔** 플랜을 얻어야 한다.
/// 행을 찾아가는 접근 경로는 같으므로 튜닝에 필요한 정보는 보존된다.
#[tokio::test]
async fn m1_4b_readonly_fallback_paths() {
    let mut c = conn_or_skip!(MYSQL84, MONITOR);
    let cases = [
        (
            "SELECT 재실행",
            "EXPLAIN FORMAT=JSON SELECT COUNT(*) FROM orders WHERE status = 'PAID'",
        ),
        (
            "UPDATE 재실행",
            "EXPLAIN FORMAT=JSON UPDATE orders SET memo = 'x' WHERE status = 'PAID'",
        ),
        (
            "DELETE 재실행",
            "EXPLAIN FORMAT=JSON DELETE FROM orders WHERE status = 'PAID'",
        ),
        (
            "INSERT 재실행",
            "EXPLAIN FORMAT=JSON INSERT INTO lock_arena (id, val) VALUES (1, 1)",
        ),
        (
            "UPDATE → SELECT 변환",
            "EXPLAIN FORMAT=JSON SELECT * FROM orders WHERE status = 'PAID'",
        ),
        (
            "DELETE → SELECT 변환",
            "EXPLAIN FORMAT=JSON SELECT * FROM order_items WHERE order_id = 1",
        ),
    ];
    let mut rows = Vec::new();
    let mut select_ok = false;
    let mut dml_denied = false;
    let mut converted_ok = false;

    for (label, sql) in cases {
        let r: Result<Option<String>, _> = c.query_first(sql).await;
        let detail = match &r {
            Ok(Some(p)) => match dbmon_planparse::parse(p, Some("shop")) {
                Ok(pp) => {
                    let node = pp.nodes.first();
                    format!(
                        "성공. access_type={:?} rows={:?} 경고={}",
                        node.and_then(|n| n.access_type.clone()),
                        node.and_then(|n| n.rows_examined_per_scan),
                        pp.warnings.len()
                    )
                }
                Err(e) => format!("플랜은 왔지만 파싱 실패: {e}"),
            },
            Ok(None) => "행 없음".into(),
            Err(e) => format!("실패 code={:?}", mysql_error_code(e)),
        };
        if label.starts_with("SELECT 재실행") && r.as_ref().is_ok_and(|o| o.is_some()) {
            select_ok = true;
        }
        if label.ends_with("재실행") && !label.starts_with("SELECT") {
            // 1142 = ER_TABLEACCESS_DENIED_ERROR
            if mysql_error_code_of(&r) == Some(1142) {
                dml_denied = true;
            }
        }
        if label.contains("변환") && r.as_ref().is_ok_and(|o| o.is_some()) {
            converted_ok = true;
        }
        rows.push((label.into(), detail));
    }
    rows.push((
        "→ 결론".into(),
        "SELECT 재실행이 기본 경로. DML 은 SELECT 로 변환해 근사 플랜을 얻는다".into(),
    ));
    report("M1-4b 읽기 전용 폴백 경로", &rows);

    assert!(
        select_ok,
        "SELECT 재실행이 안 되면 플랜 수집 경로가 하나도 없다"
    );
    assert!(
        dml_denied,
        "읽기 전용 계정으로 EXPLAIN UPDATE/DELETE 가 성공했다 — 그렇다면 변환이 불필요하다. \
         설계를 단순화할 수 있으니 확인한다."
    );
    assert!(
        converted_ok,
        "DML → SELECT 변환 경로도 막히면 DML 플랜을 얻을 방법이 없다"
    );
}

/// M1-17 — `sys.innodb_lock_waits` 의 컬럼명과 절단을 실측한다.
///
/// 설계는 컬럼명을 `locked_type` / `locked_table_schema` 로 적었고,
/// `waiting_query` 는 **64자로 잘린다**고 경고했다([05 §2.8](../../../../.claude/docs/05-collector.md)).
#[tokio::test]
async fn m1_17_innodb_lock_waits_columns_and_truncation() {
    let mut probe = conn_or_skip!(MYSQL84, ROOT);

    // 컬럼명을 먼저 확인한다 — 이름이 틀리면 쿼리 자체가 실패한다.
    let cols: Vec<String> = probe
        .query(
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = 'sys' AND TABLE_NAME = 'innodb_lock_waits' \
             ORDER BY ORDINAL_POSITION",
        )
        .await
        .unwrap_or_default();
    let expected = [
        "waiting_pid",
        "waiting_lock_mode",
        "waiting_trx_age",
        "waiting_trx_rows_locked",
        "blocking_pid",
        "blocking_lock_mode",
        "blocking_trx_age",
        "locked_type",
        "locked_table_schema",
        "locked_table_name",
        "locked_index",
        "locked_table",
        "wait_age_secs",
        "sql_kill_blocking_query",
    ];
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|c| !cols.contains(&c.to_string()))
        .collect();

    // 락 경합을 만든다: A 가 행을 잡고, B 가 같은 행을 기다린다.
    // SQL 을 64자보다 길게 만들어 절단을 관찰한다.
    let blocker = start_long_statements(
        MYSQL84,
        ROOT,
        vec![
            "START TRANSACTION".into(),
            "UPDATE lock_arena SET val = val + 1 WHERE id = 1".into(),
            "SELECT SLEEP(8)".into(),
        ],
    )
    .await;
    let waiter = start_long_statements(
        MYSQL84,
        ROOT,
        vec![
            "START TRANSACTION".into(),
            "UPDATE lock_arena SET val = val + 100 /* 이 주석은 SQL 을 64자보다 길게 만들기 위한 것이다 */ WHERE id = 1".into(),
        ],
    )
    .await;

    let mut rows: Vec<(String, String)> = vec![
        ("컬럼 수".into(), cols.len().to_string()),
        (
            "설계가 쓰는 컬럼 중 없는 것".into(),
            if missing.is_empty() {
                "없음".into()
            } else {
                format!("{missing:?}")
            },
        ),
    ];

    /// `waiting_pid, blocking_pid, locked_table_name, locked_type, waiting_query`
    type LockWaitRow = (u64, u64, Option<String>, Option<String>, Option<String>);
    let waits: Vec<LockWaitRow> = probe
        .query(
            "SELECT waiting_pid, blocking_pid, locked_table_name, locked_type, waiting_query \
             FROM sys.innodb_lock_waits",
        )
        .await
        .unwrap_or_default();

    rows.push(("관측된 대기".into(), waits.len().to_string()));
    for (wp, bp, table, ty, q) in &waits {
        let qlen = q.as_ref().map(|s| s.len()).unwrap_or(0);
        rows.push((
            format!("  대기 {wp} ← 차단 {bp}"),
            format!(
                "table={:?} type={:?} waiting_query 길이={qlen} 절단추정={}",
                table,
                ty,
                q.as_deref()
                    .is_some_and(|s| s.contains("...") || s.len() <= 64)
            ),
        ));
        if let Some(q) = q {
            rows.push(("  waiting_query".into(), q.clone()));
        }
    }

    // 전문 SQL 은 2차 조회로 얻어야 한다 (sys 뷰의 64자 절단 회피).
    if let Some((wp, ..)) = waits.first() {
        let full: Option<Option<String>> = probe
            .exec_first(
                "SELECT INFO FROM information_schema.PROCESSLIST WHERE ID = ?",
                (*wp,),
            )
            .await
            .ok()
            .flatten();
        rows.push((
            "  2차 조회한 전문 SQL 길이".into(),
            full.flatten()
                .map(|s| s.len().to_string())
                .unwrap_or("없음".into()),
        ));
    }

    report("M1-17 sys.innodb_lock_waits", &rows);

    for mut r in [blocker, waiter].into_iter().flatten() {
        kill_query(&mut probe, r.connection_id).await;
        r.join().await;
    }

    assert!(
        missing.is_empty(),
        "설계가 참조하는 컬럼이 이 버전에 없다: {missing:?} — 05 §2.8 의 쿼리를 고쳐야 한다"
    );
}

// ── 유틸 ─────────────────────────────────────────────────────────────────────

fn mysql_error_code_of<T>(r: &Result<T, mysql_async::Error>) -> Option<u16> {
    r.as_ref().err().and_then(mysql_error_code)
}

fn mysql_error_code(e: &mysql_async::Error) -> Option<u16> {
    match e {
        mysql_async::Error::Server(s) => Some(s.code),
        _ => None,
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(120).collect()
}
