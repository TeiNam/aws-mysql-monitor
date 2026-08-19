//! M1 검증 스파이크 공용 지원 코드.
//!
//! 로컬 `docker compose` 의 MySQL 컨테이너에 붙는다. 컨테이너가 없으면 테스트를
//! **건너뛴다** — Docker 가 없는 환경에서 `cargo test` 가 실패하면 안 된다.
//!
//! ⚠ 여기의 `with_danger_accept_invalid_certs` 는 **로컬 컨테이너 전용**이다.
//! 컨테이너가 자체 서명 인증서를 자동 생성하기 때문이다.
//! 프로덕션 경로(`dbmon::mysql`)에는 인증서 검증 비활성 옵션을 두지 않는다
//! ([07](../../../../docs/07-credentials-bootstrap.md) M3-4, NFR-S-03).

use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};

/// 로컬 MySQL 대상.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub label: &'static str,
    pub port: u16,
    /// 기대하는 `performance_schema_max_digest_length`.
    pub max_digest_length: u32,
}

/// 기본 파라미터. 주 개발 대상.
pub const MYSQL84: Target = Target {
    label: "mysql84",
    port: 13306,
    max_digest_length: 1024,
};
/// `max_digest_length=4096` 비교군 (M1-13).
pub const MYSQL84_WIDE: Target = Target {
    label: "mysql84-wide",
    port: 13307,
    max_digest_length: 4096,
};
/// Aurora MySQL 3.x 의 커뮤니티 기반.
pub const MYSQL80: Target = Target {
    label: "mysql80",
    port: 13308,
    max_digest_length: 1024,
};

pub const ROOT: (&str, &str) = ("root", "dbmon-local-root");
pub const MONITOR: (&str, &str) = ("dbmon", "dbmon-local-monitor");
pub const MONITOR_MINIMAL: (&str, &str) = ("dbmon_minimal", "dbmon-local-minimal");
pub const MONITOR_BROKEN: (&str, &str) = ("dbmon_broken", "dbmon-local-broken");
pub const LOADGEN: (&str, &str) = ("loadgen", "dbmon-local-loadgen");

pub fn opts(target: Target, cred: (&str, &str)) -> Opts {
    OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(target.port)
        .user(Some(cred.0.to_string()))
        .pass(Some(cred.1.to_string()))
        .db_name(Some("shop"))
        // 로컬 컨테이너의 자체 서명 인증서를 받아들인다. 테스트 전용.
        .ssl_opts(Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ))
        .into()
}

/// 연결한다. 컨테이너가 없으면 `None` 을 반환하고 사유를 출력한다.
pub async fn connect(target: Target, cred: (&str, &str)) -> Option<Conn> {
    match Conn::new(opts(target, cred)).await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!(
                "[skip] {} ({}@{}) 에 연결할 수 없다: {e}\n       `docker compose up -d` 를 먼저 실행한다.",
                target.label, cred.0, target.port
            );
            None
        }
    }
}

/// 연결하거나 테스트를 건너뛴다. `None` 이면 호출자가 `return` 한다.
#[macro_export]
macro_rules! conn_or_skip {
    ($target:expr, $cred:expr) => {
        match $crate::support::connect($target, $cred).await {
            Some(c) => c,
            None => return,
        }
    };
}

/// 골든 코퍼스를 읽는다. `#` 주석과 빈 줄을 버린다.
pub fn corpus() -> Vec<String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/digest_corpus.sql"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("코퍼스를 읽을 수 없다 ({path}): {e}"));
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// `STATEMENT_DIGEST_TEXT()` / `STATEMENT_DIGEST()` 로 서버의 다이제스트를 얻는다.
///
/// **문장을 실행하지 않는다.** 실행 후 `events_statements_summary_by_digest` 를 뒤지는
/// 방식은 우리 조회 쿼리 자체가 다이제스트에 기록되어 상관관계를 맺기 어렵다.
/// 이 함수 두 개가 정확히 같은 계산을 해 주므로 스파이크가 결정론적이 된다.
pub async fn server_digest(conn: &mut Conn, sql: &str) -> Option<(String, String)> {
    let row: Option<(Option<String>, Option<String>)> = conn
        .exec_first(
            "SELECT STATEMENT_DIGEST_TEXT(?), STATEMENT_DIGEST(?)",
            (sql, sql),
        )
        .await
        .ok()?;
    match row {
        Some((Some(text), Some(hash))) => Some((text, hash)),
        _ => None,
    }
}

/// 서버 변수를 읽는다.
pub async fn var(conn: &mut Conn, name: &str) -> Option<String> {
    let q = format!("SELECT @@{name}");
    conn.query_first(q).await.ok().flatten()
}

/// 리터럴이 아주 많은 `IN` 절로 목표 바이트 길이에 가까운 SQL 을 만든다.
///
/// 1세대의 실제 버그(1024바이트 절단)를 재현하기 위한 것이다. `id IN (...)` 은
/// PK 범위 스캔이 되므로 행수를 통제할 수 있고, `SLEEP()` 으로 실행시간을 만든다.
pub fn long_running_sql(target_bytes: usize, sleep_per_row: f64) -> String {
    let head = "SELECT COUNT(*) FROM orders WHERE id IN (";
    let tail = format!(") AND SLEEP({sleep_per_row}) = 0");
    let mut sql = String::with_capacity(target_bytes + tail.len() + 16);
    sql.push_str(head);
    let mut n = 1u32;
    while sql.len() < target_bytes.saturating_sub(tail.len()) {
        if n > 1 {
            sql.push(',');
        }
        sql.push_str(&n.to_string());
        n += 1;
    }
    sql.push_str(&tail);
    sql
}

/// **다이제스트 텍스트 자체가** 목표 길이를 넘는 SQL 을 만든다.
///
/// `long_running_sql` 로는 M1-13 을 검증할 수 없다 — MySQL 이 `IN (1,2,...)` 를 `IN (...)` 로
/// 축약해 버려서 4KB 쿼리의 `DIGEST_TEXT` 가 72바이트가 된다(첫 실행에서 발견).
/// 절단을 유발하려면 **서로 다른 식별자**가 많아야 한다.
pub fn wide_digest_sql(column_count: usize) -> String {
    let cols: Vec<String> = (0..column_count).map(|i| format!("col_{i:05}")).collect();
    format!("SELECT {} FROM orders WHERE id = 1", cols.join(", "))
}

/// 다른 커넥션에서 장기 실행 쿼리를 시작한다. 반환된 핸들의 `connection_id` 로
/// `EXPLAIN FOR CONNECTION` 을 걸 수 있다.
pub struct RunningQuery {
    pub connection_id: u64,
    pub handle: tokio::task::JoinHandle<()>,
}

pub async fn start_long_query(
    target: Target,
    cred: (&str, &str),
    sql: String,
) -> Option<RunningQuery> {
    let mut conn = connect(target, cred).await?;
    let connection_id: u64 = conn.query_first("SELECT CONNECTION_ID()").await.ok()??;
    let handle = tokio::spawn(async move {
        // 실패해도 무해하다 — 측정이 끝난 뒤 KILL 당할 수 있다.
        let _ = conn.query_drop(sql).await;
        let _ = conn.disconnect().await;
    });
    // 서버가 문장을 접수하고 processlist 에 나타날 시간을 준다.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    Some(RunningQuery {
        connection_id,
        handle,
    })
}

/// 측정 결과를 사람이 읽을 표로 출력한다. 스파이크의 산출물은 pass/fail 이 아니라
/// **문서에 옮길 수치**다 ([17](../../../../docs/17-roadmap-tasks.md) M1 완료 기준).
pub fn report(title: &str, rows: &[(String, String)]) {
    let width = rows
        .iter()
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0)
        .max(title.len());
    println!("\n┌─ {title} ─");
    for (k, v) in rows {
        println!("│ {k:<width$}  {v}");
    }
    println!("└─");
}
