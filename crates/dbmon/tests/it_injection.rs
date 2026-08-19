//! 주입 방어 회귀 테스트 — **실제 MySQL 에 실행해서** 확인한다.
//!
//! 이 파일의 존재 이유: 2차 리뷰에서 확인된 주입이 **읽기만으로는 보이지 않았다.**
//! `plan_query` 게이트를 통과한 문자열이 서버에서 몇 개의 문장으로 파싱되는지는
//! 서버에 물어봐야 알 수 있다. `Com_select` 델타가 그 답이다.
//!
//! Docker 가 없으면 건너뛴다.
//!
//! # `exclusive_target` 을 쓰지 않는다
//!
//! 그 락은 **테스트 바이너리 안의** `static Mutex` 라서 프로세스를 넘지 않는다.
//! `cargo test` 는 바이너리를 병렬로 돌리므로, 여기서 `reset_targets` 를 부르면
//! `it_collector` 의 장기 실행 쿼리를 죽여 그쪽을 플래키하게 만든다(실제로 그랬다).
//!
//! 이 테스트는 애초에 독점이 필요 없다: `SHOW SESSION STATUS` 는 세션 범위이고,
//! 문장 이력도 우리 스레드로 한정해 조회한다.

mod support;

use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, Value};
use support::{MYSQL84, ROOT, Target};

/// 세션의 `Com_select` 값.
///
/// **`performance_schema.session_status` 에는 `Com_*` 가 없다** (8.4 실측: 336행 중
/// `Com_select` 부재). `SHOW SESSION STATUS` 만이 세션 단위 명령 카운터를 준다.
async fn com_select(conn: &mut Conn) -> u64 {
    conn.query_first::<(String, String), _>("SHOW SESSION STATUS LIKE 'Com_select'")
        .await
        .ok()
        .flatten()
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0)
}

/// **프로덕션과 같은 경로**로 연결한다: `dbmon::mysql::sql::session_init` 을 `init` 에 넣는다.
///
/// 여기서 `session_init` 을 쓰지 않으면 이 테스트는 통제를 검증하지 못한다.
async fn conn_like_production(target: Target, pin_sql_mode: bool) -> Option<Conn> {
    let init = if pin_sql_mode {
        dbmon::mysql::sql::session_init(3_000)
    } else {
        // 수정 이전 상태를 재현한다 — `sql_mode` 를 건드리지 않는다.
        "SET SESSION max_execution_time = 3000, transaction_isolation = 'READ-COMMITTED', \
         autocommit = 1"
            .to_string()
    };
    let opts: Opts = OptsBuilder::from_opts(support::opts(target, ROOT))
        .init(vec![init])
        .into();
    match Conn::new(opts).await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("건너뜀: {} 에 붙을 수 없다 ({e})", target.label);
            None
        }
    }
}

/// 렉서는 `\'` 를 이스케이프로 보고 문자열 하나로 삼킨다. 서버가
/// `NO_BACKSLASH_ESCAPES` 로 동작하면 그렇게 보지 않는다 — 그 틈으로 문장이 실행된다.
const BACKSLASH_PAYLOAD: &str =
    r"SELECT * FROM orders WHERE memo = '\'; SELECT 31337 AS pwned; SELECT 1 -- '";

/// **게이트를 통과한다는 것 자체는 결함이 아니다.** 통과한 문자열이 서버에서
/// 문장 하나로만 실행되는지가 요점이다.
#[test]
fn gate_accepts_the_payload_as_a_single_statement() {
    let q = dbmon_normalize::plan_query(BACKSLASH_PAYLOAD)
        .expect("렉서는 이걸 단일 SELECT 로 본다 — 그게 문제의 전제다");
    assert!(q.is_exact);
    assert_eq!(
        q.sql, BACKSLASH_PAYLOAD,
        "원문 부분문자열이 그대로 서버로 간다"
    );
}

/// **핵심 회귀 테스트.** `session_init` 이 `sql_mode` 를 고정하므로 주입 문장이
/// 실행되지 않아야 한다.
#[tokio::test]
async fn pinned_sql_mode_prevents_injected_statement_execution() {
    let Some(mut conn) = conn_like_production(MYSQL84, true).await else {
        return;
    };

    // 고정이 실제로 적용됐는지 먼저 확인한다. 아니면 이 테스트는 아무것도 증명하지 않는다.
    let mode: Option<String> = conn
        .query_first("SELECT @@session.sql_mode")
        .await
        .expect("sql_mode 조회");
    assert_eq!(
        mode.as_deref(),
        Some(""),
        "session_init 이 sql_mode 를 고정하지 못했다"
    );

    let before = com_select(&mut conn).await;
    let stmt =
        dbmon::mysql::sql::explain_rerun(BACKSLASH_PAYLOAD, dbmon::mysql::sql::ExplainFormat::Json);
    // `explain_rerun` 과 같은 호출 방식(COM_QUERY 텍스트 프로토콜).
    let _ = conn.query_first::<String, _>(stmt).await;
    let after = com_select(&mut conn).await;

    assert_eq!(
        after - before,
        1,
        "EXPLAIN 하나만 실행돼야 한다. {}건이면 주입 문장이 실행됐다",
        after - before
    );

    // 주입 문장이 실행됐다면 그 결과가 남는다.
    // 함정 셋을 모두 피해야 한다:
    //  ① 전체 테이블을 보면 병렬 테스트의 문장에 걸린다 → 우리 스레드로 한정한다.
    //  ② 패턴을 그대로 쓰면 **이 쿼리 자신의 텍스트가 매칭된다** → 리터럴을 쪼갠다.
    //  ③ `explain_rerun` 은 `/* dbmon:planrerun */ EXPLAIN …` 이므로 `LIKE 'EXPLAIN%'`
    //     로는 우리 EXPLAIN 을 제외하지 못한다 → `%EXPLAIN%` 으로 본다.
    let pwned: Option<i64> = conn
        .query_first(
            "SELECT 1 FROM performance_schema.events_statements_history h \
             JOIN performance_schema.threads t USING (THREAD_ID) \
             WHERE t.PROCESSLIST_ID = CONNECTION_ID() \
               AND h.SQL_TEXT LIKE CONCAT('%313', '37 AS pw', 'ned%') \
               AND h.SQL_TEXT NOT LIKE '%EXPLAIN%' LIMIT 1",
        )
        .await
        .unwrap_or(None);
    assert!(pwned.is_none(), "주입된 문장이 별도 문장으로 실행됐다");
}

/// **통제가 없으면 실제로 뚫린다**는 것을 같은 방법으로 확인한다.
///
/// 이 테스트가 없으면 위 테스트가 "고정이 효과가 있어서" 통과하는지
/// "애초에 뚫리지 않아서" 통과하는지 구분할 수 없다.
#[tokio::test]
async fn without_pinning_the_injection_actually_executes() {
    let Some(mut conn) = conn_like_production(MYSQL84, false).await else {
        return;
    };
    // 대상 인스턴스가 이 모드로 설정된 상황을 재현한다(우리는 상속만 한다).
    conn.query_drop("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'")
        .await
        .expect("모드 설정");

    let before = com_select(&mut conn).await;
    let stmt =
        dbmon::mysql::sql::explain_rerun(BACKSLASH_PAYLOAD, dbmon::mysql::sql::ExplainFormat::Json);
    let _ = conn.query_first::<String, _>(stmt).await;
    let after = com_select(&mut conn).await;

    assert!(
        after - before > 1,
        "NO_BACKSLASH_ESCAPES 에서는 주입이 성립해야 한다 (증가 {}건). \
         성립하지 않는다면 위 테스트의 근거가 사라진 것이므로 둘 다 재검토한다",
        after - before
    );
    eprintln!(
        "  통제 없음: Com_select +{} (주입 문장이 실행됐다)",
        after - before
    );
}

/// 바인딩 자리표가 남은 텍스트(`DIGEST_TEXT`)로 EXPLAIN 을 실행하면 **항상 1064** 다.
/// 그런 문장을 대상 DB 로 보내면 안 된다 — 실패가 보장된 부하다.
#[test]
fn digest_text_with_placeholders_is_not_replayable() {
    for sql in [
        "SELECT `a` FROM `t` WHERE `id` = ?",
        "SELECT `a` FROM `t` WHERE `id` IN (...)",
        "INSERT INTO `t` (`a`) VALUES (...)",
    ] {
        assert!(
            dbmon_normalize::plan_query(sql).is_none(),
            "자리표가 남은 텍스트는 재실행 대상이 아니다: {sql}"
        );
    }
}

/// 자리표 거부가 **실제 SQL 을 막지 않는지** 확인한다.
/// 과하게 막으면 플랜 수집이 전부 죽는다.
#[test]
fn placeholder_rejection_does_not_block_real_sql() {
    for sql in [
        "SELECT a FROM t WHERE id = 1",
        "SELECT a FROM t WHERE memo = 'why? really?'",
        "SELECT a FROM t WHERE memo LIKE '%(...)%'",
        "SELECT JSON_EXTRACT(doc, '$.a?b') FROM t",
    ] {
        assert!(
            dbmon_normalize::plan_query(sql).is_some(),
            "실제 SQL 을 막으면 안 된다: {sql}"
        );
    }
}

/// `explain_rerun` 이 만드는 문장에 우리가 만든 것 외의 문장 구분자가 없어야 한다.
#[test]
fn explain_rerun_emits_exactly_one_statement_for_validated_input() {
    let q = dbmon_normalize::plan_query("SELECT a FROM t WHERE id = 1").expect("통과");
    let stmt = dbmon::mysql::sql::explain_rerun(&q.sql, dbmon::mysql::sql::ExplainFormat::Json);
    assert_eq!(
        stmt.matches(';').count(),
        0,
        "생성된 문장에 세미콜론이 있다: {stmt}"
    );
    let _ = Value::from(0); // mysql_async import 사용 표시
}
