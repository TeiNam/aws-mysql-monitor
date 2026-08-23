#![allow(dead_code)] // 테스트 바이너리마다 쓰는 함수가 다르다

//! M1 검증 스파이크 공용 지원 코드.
//!
//! 로컬 `docker compose` 의 MySQL 컨테이너에 붙는다. 컨테이너가 없으면 테스트를
//! **건너뛴다** — Docker 가 없는 환경에서 `cargo test` 가 실패하면 안 된다.
//!
//! ⚠ 여기의 `with_danger_accept_invalid_certs` 는 **로컬 컨테이너 전용**이다.
//! 컨테이너가 자체 서명 인증서를 자동 생성하기 때문이다.
//! 프로덕션 경로(`dbmon::mysql`)에는 인증서 검증 비활성 옵션을 두지 않는다
//! ([07](../../../../.claude/docs/07-credentials-bootstrap.md) M3-4, NFR-S-03).

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

/// 목표 바이트 길이의 **장기 실행** SQL 을 만든다.
///
/// 1세대의 실제 버그(1024바이트 절단)를 재현하기 위한 것이다.
///
/// **형태가 중요하다.** `id = 1` 로 1행만 읽게 해서 `SLEEP(초)` 이 정확히 한 번
/// 평가되게 하고, 길이는 뒤의 `IN` 리스트로 채운다. `IN` 리스트로 행수를 만들면
/// 실행시간이 (리스트 길이 × 행당 sleep) 이 되어 목표 시간을 맞출 수 없다.
pub fn long_running_sql(target_bytes: usize, seconds: f64) -> String {
    let head =
        format!("SELECT COUNT(*) FROM orders WHERE id = 1 AND SLEEP({seconds}) = 0 AND id IN (");
    let tail = ")";
    let mut sql = String::with_capacity(target_bytes + head.len() + 8);
    sql.push_str(&head);
    let mut n = 1u32;
    while sql.len() + tail.len() < target_bytes {
        if n > 1 {
            sql.push(',');
        }
        sql.push_str(&n.to_string());
        n += 1;
    }
    sql.push_str(tail);
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
    /// `Drop` 이 `take()` 하므로 `Option` 이다.
    handle: Option<tokio::task::JoinHandle<()>>,
    /// `Drop` 이 서버 측 쿼리를 죽이는 데 쓴다.
    port: u16,
    /// 명시적으로 정리했다 — `Drop` 은 아무것도 하지 않는다.
    cleaned: bool,
}

impl RunningQuery {
    /// 이미 정리했다고 표시한다 — `Drop` 이 중복 `KILL QUERY` 를 보내지 않는다.
    pub fn mark_cleaned(&mut self) {
        self.cleaned = true;
    }

    /// 배경 태스크가 끝날 때까지 기다린다.
    pub async fn join(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

/// **단정 실패에도 서버 측 쿼리를 정리한다.**
///
/// 이전에는 성공 경로에서만 `kill_and_wait` 를 불렀다. 단정 하나가 실패하면
/// `SLEEP(10)` 이 최대 10초 동안 서버에 남아 **다음 테스트가 그걸 후보로 본다** —
/// 릴리스 빌드에서 `it_collector` 가 간헐적으로 실패한 원인 중 하나다.
///
/// `Drop` 은 async 를 쓸 수 없으므로 동기 커넥션으로 `KILL QUERY` 를 보낸다.
impl Drop for RunningQuery {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        if self.cleaned {
            return;
        }
        let id = self.connection_id;
        let port = self.port;
        // 별도 스레드에서 동기 정리. 실패는 무시한다(이미 끝났을 수 있다).
        let _ = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async move {
                let target = Target {
                    label: "cleanup",
                    port,
                    max_digest_length: 0,
                };
                if let Some(mut c) = connect(target, ROOT).await {
                    let _ = c.query_drop(format!("KILL QUERY {id}")).await;
                }
            });
        })
        .join();
    }
}

pub async fn start_long_query(
    target: Target,
    cred: (&str, &str),
    sql: impl Into<String>,
) -> Option<RunningQuery> {
    start_long_statements(target, cred, vec![sql.into()]).await
}

/// 여러 문장을 순서대로 실행하고 **마지막 문장이 실행 중인 상태**로 돌려준다.
///
/// DML 플랜 검증(ADR-006)에 필요하다: `START TRANSACTION` 후 `UPDATE`/`DELETE` 를 걸고
/// 커밋하지 않는다. 커넥션이 드롭되면 롤백되므로 시드 데이터가 오염되지 않는다.
pub async fn start_long_statements(
    target: Target,
    cred: (&str, &str),
    statements: Vec<String>,
) -> Option<RunningQuery> {
    let mut conn = connect(target, cred).await?;
    let connection_id: u64 = conn.query_first("SELECT CONNECTION_ID()").await.ok()??;
    let handle = tokio::spawn(async move {
        for s in statements {
            // 실패해도 무해하다 — 측정이 끝난 뒤 KILL 당할 수 있다.
            if conn.query_drop(s).await.is_err() {
                break;
            }
        }
        // 커밋하지 않는다. 드롭 시 롤백된다.
        let _ = conn.disconnect().await;
    });
    // 서버가 문장을 접수하고 processlist 에 나타날 시간을 준다.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    Some(RunningQuery {
        connection_id,
        handle: Some(handle),
        port: target.port,
        cleaned: false,
    })
}

/// 문장을 끝까지 실행하고 **커넥션을 유휴 상태로 살려 둔다.**
///
/// # 왜 필요한가 — 풀링 커넥션을 재현한다
///
/// `start_long_statements` 는 문장이 끝나면 곧바로 `disconnect` 한다. 그러면 서버가 스레드를
/// 파괴하고 그 스레드의 `events_statements_history` 도 함께 사라진다 — 확정 시점에 정확
/// 지표를 읽는 경로를 테스트할 수 없다.
///
/// 실제 애플리케이션은 커넥션을 풀에 돌려주므로 스레드가 살아 있고, 히스토리에 방금 끝난
/// 문장이 실제 값으로 남는다(실측: `ROWS_EXAMINED` 0 → 120,005). 그 상황을 만든다.
pub async fn run_then_idle(
    target: Target,
    cred: (&str, &str),
    sql: impl Into<String>,
    idle: std::time::Duration,
) -> Option<RunningQuery> {
    let sql = sql.into();
    let mut conn = connect(target, cred).await?;
    let connection_id: u64 = conn.query_first("SELECT CONNECTION_ID()").await.ok()??;
    let handle = tokio::spawn(async move {
        let _ = conn.query_drop(sql).await;
        // **문장은 끝났고 커넥션은 살아 있다.** 여기서 SQL 을 더 보내면 그 문장이
        // `events_statements_current` 를 차지하고 히스토리 자리도 밀려난다.
        tokio::time::sleep(idle).await;
        let _ = conn.disconnect().await;
    });
    Some(RunningQuery {
        connection_id,
        handle: Some(handle),
        port: target.port,
        cleaned: false,
    })
}

/// 실행 중인 커넥션의 문장을 세 소스에서 각각 읽어 길이를 잰다 (M1-1).
pub struct TextLengths {
    /// `performance_schema.processlist.INFO` — 1024바이트 절단이 예상된다.
    pub ps_processlist_info: Option<u64>,
    /// `information_schema.PROCESSLIST.INFO` — `LONGTEXT`. 절단되지 않아야 한다.
    pub is_processlist_info: Option<u64>,
    /// `events_statements_current.SQL_TEXT` — `max_sql_text_length` 로 잘린다.
    pub stmt_current_sql_text: Option<u64>,
}

pub async fn measure_text_lengths(conn: &mut Conn, connection_id: u64) -> TextLengths {
    let ps: Option<u64> = conn
        .exec_first(
            "SELECT LENGTH(INFO) FROM performance_schema.processlist WHERE ID = ?",
            (connection_id,),
        )
        .await
        .ok()
        .flatten();
    let is: Option<u64> = conn
        .exec_first(
            "SELECT LENGTH(INFO) FROM information_schema.PROCESSLIST WHERE ID = ?",
            (connection_id,),
        )
        .await
        .ok()
        .flatten();
    let cur: Option<u64> = conn
        .exec_first(
            "SELECT LENGTH(e.SQL_TEXT) FROM performance_schema.events_statements_current e \
             JOIN performance_schema.threads t USING (THREAD_ID) WHERE t.PROCESSLIST_ID = ?",
            (connection_id,),
        )
        .await
        .ok()
        .flatten();
    TextLengths {
        ps_processlist_info: ps,
        is_processlist_info: is,
        stmt_current_sql_text: cur,
    }
}

/// 실행 중인 문장을 강제 종료한다. 측정이 끝난 뒤 정리용.
pub async fn kill_query(conn: &mut Conn, connection_id: u64) {
    let _ = conn.query_drop(format!("KILL QUERY {connection_id}")).await;
}

/// 측정 결과를 사람이 읽을 표로 출력한다. 스파이크의 산출물은 pass/fail 이 아니라
/// **문서에 옮길 수치**다 ([17](../../../../.claude/docs/17-roadmap-tasks.md) M1 완료 기준).
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

/// 실행 중인 쿼리를 죽이고 태스크가 끝나기를 기다린다.
/// 명시적으로 죽이고 기다린다.
///
/// `Drop` 이 한 번 더 `KILL QUERY` 를 보내지 않도록 `handle` 을 비워 둔다 —
/// 두 번째 kill 은 `ER_NO_SUCH_THREAD`(1094)로 무해하지만 왕복 35ms 를 낭비한다.
pub async fn kill_and_wait(mut r: RunningQuery) {
    if let Some(mut probe) = connect(MYSQL84, ROOT).await {
        kill_query(&mut probe, r.connection_id).await;
        let _ = probe.disconnect().await;
    }
    r.join().await;
    // `Drop` 이 중복 kill 을 보내지 않게 한다 (이미 죽였다).
    r.mark_cleaned();
    // 서버가 스레드를 정리할 시간을 준다 — 바로 tick 하면 아직 processlist 에 있다.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
}

/// 대상 DB 에 남아 있는 장기 실행 쿼리를 전부 죽인다.
///
/// 테스트가 순차로 돌더라도 `SLEEP(6)` 이 다음 테스트까지 살아남으면 후보가 섞인다.
/// 각 테스트 시작 시 호출해 **격리를 보장한다.**
pub async fn reset_targets(target: Target) {
    let Some(mut root) = connect(target, ROOT).await else {
        return;
    };
    let ids: Vec<u64> = root
        .query(
            "SELECT ID FROM information_schema.PROCESSLIST              WHERE COMMAND <> 'Sleep' AND USER <> 'root' AND INFO IS NOT NULL",
        )
        .await
        .unwrap_or_default();
    for id in ids {
        kill_query(&mut root, id).await;
    }
    let _ = root.disconnect().await;
    // 서버가 스레드를 정리할 시간을 준다.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

/// 대상 DB 를 쓰는 테스트를 **직렬화**한다.
///
/// `reset_targets` 가 다른 테스트의 장기 실행 쿼리까지 죽이므로 병렬 실행은 불안정하다.
///
/// # ⚠ 이 락은 **테스트 바이너리를 넘지 않는다**
///
/// `static Mutex` 이므로 프로세스 안에서만 유효하다. 같은 대상에 장기 실행 쿼리를 만드는
/// **다른 바이너리**가 이 락을 잡아도 서로를 막지 못한다.
///
/// (`cargo test` 가 바이너리를 동시에 하나만 돌린다는 관측도 있지만, 그건 보장이 아니라
/// 현재 동작이다. `--jobs` 나 향후 변경에 의존하지 않는다.)
///
/// 규칙:
/// - 장기 실행 쿼리를 만들고 그게 살아 있길 기대하는 테스트는 **`ROOT` 로 붙는다**
///   (`reset_targets` 가 `USER <> 'root'` 로 제외한다).
/// - 세션 범위 상태만 보는 테스트는 락이 필요 없다.
///
/// `--test-threads=1` 을 잊어도 안전하게 만든다.
/// 실행 방법에 의존하는 테스트는 언젠가 CI 에서 깨진다.
pub static TARGET_DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 락을 잡고 대상 DB 를 초기화한다. 가드가 살아 있는 동안 다른 테스트가 끼어들지 않는다.
pub async fn exclusive_target(target: Target) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = TARGET_DB_LOCK.lock().await;
    reset_targets(target).await;
    guard
}
