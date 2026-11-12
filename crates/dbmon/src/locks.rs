//! 락·트랜잭션 스냅샷 — **지금 누가 누구를 막고 있나** (요청 시 조회).
//!
//! # 왜 Database Insights 로 안 되는가
//!
//! Database Insights 의 락 분석은 PostgreSQL 전용이다(2026-11 문서). MySQL 에서 보이는 것은
//! 대기 이벤트와 횟수뿐이고 **쥐고 있는 쪽은 안 보인다** — 블로커는 대개 유휴라 DB 부하에도
//! 처리목록에도 나타나지 않는다(8.0.46·8.4.11 실측: `Sleep`, `INFO` NULL).
//!
//! # 왜 수집이 아니라 요청 시 조회인가
//!
//! 상태 스냅샷이고 사람이 화면을 열었을 때만 필요하다. 500대를 주기적으로 훑을 이유가 없다.
//! 대신 여러 사람이 같은 인스턴스를 보거나 화면이 몰아쳐도 대상 DB 에 연결이 쏟아지지 않게
//! **짧게 캐시한다.**
//!
//! # SQL 은 언제나 마스킹한다
//!
//! 저장 레코드의 리터럴 정책과 무관하게 **정규 텍스트만** 내보낸다. 마스킹을 믿을 수 없는
//! 문장(인용부호 미종료 — 1,024바이트 절단에서 흔하다)은 통째로 버리고 그 수를 센다.
//!
//! **보이는지는 슬로우 쿼리 SQL 과 같은 규칙이다**([`crate::api::view::sql_hidden_reason`]) —
//! 정책이 `off` 면 아무도, 원문 정책이면 리터럴 열람 권한이 있는 사람만 본다. 캐시는 사람을
//! 가리지 않으므로 응답 직전에 뺀다([`LockSnapshot::without_sql`]).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbmon_core::error::{DomainError, Result};
use dbmon_core::instance::Instance;
use dbmon_core::ports::{LockWaitRow, TargetDb};
use dbmon_core::slow_query::LiteralPolicy;
use dbmon_core::time::{Clock, EpochMs, SystemClock};
use serde::Serialize;

use crate::aws::auth_token::TargetAuth;
use crate::config::Config;
use crate::mysql::{LOCK_WAITS_LIMIT, MdlRow, TargetMysql, TrxRow};

/// 같은 인스턴스의 스냅샷을 다시 쓰는 시간. 화면 갱신 주기(5초)보다 짧다.
const SNAPSHOT_TTL_MS: EpochMs = 3_000;
/// 이 나이(초) 이상인 트랜잭션만 본다 — 1초 미만은 정상적인 OLTP 다.
const MIN_TRX_AGE_SECS: u64 = 1;
const MAX_TRANSACTIONS: u64 = 50;
const MAX_MDL_ROWS: u64 = 200;
/// 연결 수립(TLS + IAM 토큰, 최대 5초) + 조회 셋에 주는 기한. **서비스 안에 둔다** — 기한에
/// 걸린 것도 캐시해야 닿지 않는 인스턴스를 화면마다 다시 두드리지 않는다.
const COLLECT_DEADLINE: Duration = Duration::from_secs(15);

/// 화면 하나에 실리는 스냅샷.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LockSnapshot {
    pub instance_id: String,
    pub captured_at_ms: EpochMs,
    pub lock_waits: Vec<LockWaitView>,
    pub transactions: Vec<TransactionView>,
    pub metadata_locks: Vec<MetadataLockView>,
    /// 상한에 걸려 일부만 실었다.
    pub truncated: bool,
    /// 마스킹을 믿을 수 없어 **버린** SQL 수. 0 이 아니면 화면이 그 사실을 말한다.
    pub sql_withheld: usize,
    /// SQL 을 **전부 뺀** 이유(`policy_off`·`insufficient_role`). 있으면 화면이 그 사실을 말한다.
    pub sql_hidden: Option<&'static str>,
}

impl LockSnapshot {
    /// SQL 을 전부 뺀 사본. 이 사람에게 보일 수 없는 정책·역할일 때 응답 직전에 쓴다.
    pub fn without_sql(&self, reason: &'static str) -> Self {
        Self {
            lock_waits: self
                .lock_waits
                .iter()
                .map(|w| LockWaitView {
                    waiting_sql: None,
                    blocking_statements: Vec::new(),
                    ..w.clone()
                })
                .collect(),
            transactions: self
                .transactions
                .iter()
                .map(|t| TransactionView {
                    current_sql: None,
                    statements: Vec::new(),
                    ..t.clone()
                })
                .collect(),
            metadata_locks: self
                .metadata_locks
                .iter()
                .map(|m| MetadataLockView {
                    sql: None,
                    ..m.clone()
                })
                .collect(),
            sql_withheld: 0,
            sql_hidden: Some(reason),
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LockWaitView {
    pub waiting_pid: u64,
    pub waiting_time_s: Option<u64>,
    pub waiting_sql: Option<String>,
    /// `스키마.테이블`.
    pub locked_table: Option<String>,
    pub locked_index: Option<String>,
    pub lock_mode: Option<String>,
    pub blocking_pid: Option<u64>,
    pub blocking_user: Option<String>,
    pub blocking_host: Option<String>,
    pub blocking_command: Option<String>,
    pub blocking_time_s: Option<u64>,
    /// 블로커가 **유휴(`Sleep`)로 트랜잭션을 쥐고 있다** — 처리목록에서는 아무것도 하지 않는
    /// 세션으로 보인다. 커밋을 잊은 코드 경로가 대개 이것이다.
    pub blocking_idle: bool,
    /// 블로커 트랜잭션이 실행한 문장, 오래된 순.
    pub blocking_statements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransactionView {
    pub pid: Option<u64>,
    pub state: Option<String>,
    pub age_s: Option<u64>,
    pub rows_locked: Option<u64>,
    pub rows_modified: Option<u64>,
    pub user: Option<String>,
    pub host: Option<String>,
    pub db: Option<String>,
    pub command: Option<String>,
    pub time_s: Option<u64>,
    /// 문장은 끝났는데 커밋하지 않았다.
    pub idle: bool,
    pub current_sql: Option<String>,
    pub statements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetadataLockView {
    /// `스키마.테이블`.
    pub table: Option<String>,
    pub lock_type: Option<String>,
    pub lock_duration: Option<String>,
    pub lock_status: Option<String>,
    pub pending: bool,
    pub pid: Option<u64>,
    pub user: Option<String>,
    pub host: Option<String>,
    pub command: Option<String>,
    pub time_s: Option<u64>,
    pub sql: Option<String>,
}

/// 화면에 보낼 SQL. **정규 텍스트만** — 마스킹을 믿을 수 없으면 `None`.
///
/// `tuning::model_safe_sql` 과 같은 판정이다. 인용부호가 닫히지 않았거나 후조건이 실패하면
/// 버린다. 힌트·주석도 지운다 — 정규화는 그 안을 마스킹하지 않는다.
pub fn masked_sql(raw: &str) -> Option<String> {
    let n = dbmon_normalize::normalize(raw);
    if n.unterminated_quote || n.check_no_literals().is_err() {
        return None;
    }
    let text = dbmon_core::tuning::strip_hints_and_comments(&n.canonical);
    (!text.trim().is_empty()).then_some(text)
}

fn is_idle(command: Option<&str>) -> bool {
    command == Some("Sleep")
}

pub(crate) fn qualified(schema: Option<String>, table: Option<String>) -> Option<String> {
    match (schema, table) {
        (Some(s), Some(t)) => Some(format!("{s}.{t}")),
        (None, t) => t,
        (Some(_), None) => None,
    }
}

/// 원문 행 → 화면 뷰. **SQL 이 나가는 유일한 길이다** — 여기서 마스킹한다.
pub fn build_snapshot(
    instance_id: &str,
    captured_at_ms: EpochMs,
    waits: Vec<LockWaitRow>,
    trx: Vec<TrxRow>,
    mdl: Vec<MdlRow>,
    truncated: bool,
) -> LockSnapshot {
    let mut withheld = 0usize;
    let mut one = |raw: Option<String>| -> Option<String> {
        let raw = raw?;
        let masked = masked_sql(&raw);
        withheld += usize::from(masked.is_none());
        masked
    };
    let lock_waits: Vec<LockWaitView> = waits
        .into_iter()
        .map(|w| LockWaitView {
            waiting_pid: w.waiting_pid,
            waiting_time_s: w.waiting_time_s,
            waiting_sql: one(w.waiting_sql),
            locked_table: qualified(w.locked_schema, w.locked_table),
            locked_index: w.locked_index,
            lock_mode: w.lock_mode,
            blocking_pid: w.blocking_pid,
            blocking_user: w.blocking_user,
            blocking_host: w.blocking_host,
            blocking_idle: is_idle(w.blocking_command.as_deref()),
            blocking_command: w.blocking_command,
            blocking_time_s: w.blocking_time_s,
            blocking_statements: w
                .blocking_statements
                .into_iter()
                .filter_map(|s| one(Some(s)))
                .collect(),
        })
        .collect();
    let transactions: Vec<TransactionView> = trx
        .into_iter()
        .map(|t| TransactionView {
            pid: t.pid,
            state: t.state,
            age_s: t.age_s,
            rows_locked: t.rows_locked,
            rows_modified: t.rows_modified,
            user: t.user,
            host: t.host,
            db: t.db,
            idle: is_idle(t.command.as_deref()),
            command: t.command,
            time_s: t.time_s,
            current_sql: one(t.current_sql),
            statements: t
                .statements
                .into_iter()
                .filter_map(|s| one(Some(s)))
                .collect(),
        })
        .collect();
    let metadata_locks: Vec<MetadataLockView> = mdl
        .into_iter()
        .map(|m| MetadataLockView {
            table: qualified(m.schema, m.table),
            pending: m.lock_status.as_deref() == Some("PENDING"),
            lock_type: m.lock_type,
            lock_duration: m.lock_duration,
            lock_status: m.lock_status,
            pid: m.pid,
            user: m.user,
            host: m.host,
            command: m.command,
            time_s: m.time_s,
            sql: one(m.sql),
        })
        .collect();
    LockSnapshot {
        instance_id: instance_id.to_string(),
        captured_at_ms,
        lock_waits,
        transactions,
        metadata_locks,
        truncated,
        sql_withheld: withheld,
        sql_hidden: None,
    }
}

/// 스냅샷을 못 만든 이유. **화면이 할 일을 고를 만큼만** 나눈다 — 원인은 로그로 간다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockFailure {
    /// 대상 DB 가 모니터링 계정을 거부했다(로그인·GRANT). 재시도로는 낫지 않는다.
    Forbidden,
    /// 닿지 못했거나 조회가 실패했다.
    Unavailable,
    /// 기한([`COLLECT_DEADLINE`]) 안에 끝나지 않았다.
    Timeout,
}

pub type LockOutcome = std::result::Result<Arc<LockSnapshot>, LockFailure>;

/// 요청 시 스냅샷 서비스.
pub struct LockService {
    auth: Arc<TargetAuth>,
    config: Arc<Config>,
    flights: Flights<LockOutcome>,
}

impl LockService {
    pub fn new(auth: Arc<TargetAuth>, config: Arc<Config>) -> Self {
        Self {
            auth,
            config,
            flights: Flights::new(SNAPSHOT_TTL_MS),
        }
    }

    /// SQL 을 보일지 가르는 정책 — 수집기가 저장에 쓰는 것과 같은 배포 설정이다.
    pub fn literal_policy(&self) -> LiteralPolicy {
        self.config.collector.literal_policy
    }

    /// 지금 누가 누구를 막고 있나. **같은 인스턴스는 한 번에 한 연결만** 대상 DB 에 붙고,
    /// 결과는 실패까지 잠깐 다시 쓴다([`Flights`]).
    ///
    /// **요청에서 떼어 내 돌린다.** 브라우저가 탭을 닫아 요청이 끊겨도 대상 DB 작업은 끝까지
    /// 가서 연결을 정상으로 닫고 결과를 캐시에 남긴다. 요청과 함께 버리면 수립 중이던 연결이
    /// 끊긴다 — 실측(8.4.11): 끊긴 요청 5건에 `Aborted_clients` 4.
    pub async fn snapshot(self: &Arc<Self>, instance: &Instance) -> LockOutcome {
        let svc = Arc::clone(self);
        let instance = instance.clone();
        let task = tokio::spawn(async move {
            svc.flights
                .run(instance.id.as_str(), &SystemClock, || {
                    svc.fetch(&instance, &SystemClock)
                })
                .await
        });
        task.await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "락 스냅샷 작업이 끝나지 못했다");
            Err(LockFailure::Unavailable)
        })
    }

    async fn fetch(&self, instance: &Instance, clock: &impl Clock) -> LockOutcome {
        let work = async {
            let db = TargetMysql::on_demand(&self.auth, &self.config, instance, "locks").await?;
            // 닫기는 `OnDemandConn` 이 맡는다 — 기한에 걸려 이 future 가 버려져도 닫힌다.
            collect(&db).await
        };
        let (waits, trx, mdl) = match tokio::time::timeout(COLLECT_DEADLINE, work).await {
            Ok(Ok(rows)) => rows,
            Ok(Err(e)) => {
                // 원인은 로그로만 — 서버 오류 메시지에는 문장 조각이 들어 있다.
                tracing::warn!(
                    instance = %instance.id.as_str(),
                    error = %crate::telemetry::Scrubbed(&e),
                    "락 스냅샷 조회 실패"
                );
                return Err(match e {
                    DomainError::Forbidden { .. } => LockFailure::Forbidden,
                    _ => LockFailure::Unavailable,
                });
            }
            Err(_) => {
                tracing::warn!(
                    instance = %instance.id.as_str(),
                    deadline_secs = COLLECT_DEADLINE.as_secs(),
                    "락 스냅샷 조회가 기한을 넘겼다"
                );
                return Err(LockFailure::Timeout);
            }
        };
        let truncated = waits.len() as u64 >= LOCK_WAITS_LIMIT
            || trx.len() as u64 >= MAX_TRANSACTIONS
            || mdl.len() as u64 >= MAX_MDL_ROWS;
        Ok(Arc::new(build_snapshot(
            instance.id.as_str(),
            clock.now_ms(),
            waits,
            trx,
            mdl,
            truncated,
        )))
    }
}

async fn collect(db: &TargetMysql) -> Result<(Vec<LockWaitRow>, Vec<TrxRow>, Vec<MdlRow>)> {
    let waits = db.lock_waits(&[]).await?;
    let trx = db.transactions(MIN_TRX_AGE_SECS, MAX_TRANSACTIONS).await?;
    let mdl = db.metadata_lock_waits(MAX_MDL_ROWS).await?;
    Ok((waits, trx, mdl))
}

/// 키(인스턴스)별 **단일 비행 + 짧은 캐시.** 실패도 같은 기간 캐시한다.
///
/// 장애 때는 여러 사람이 같은 인스턴스의 락 탭을 한꺼번에 연다 — 대상 DB 가 가장 약할 때다.
/// 같은 키는 한 번에 하나만 일하고, 나머지는 줄을 섰다가 그 결과를 받는다. 닿지 않는
/// 인스턴스도 화면마다·재시도마다 두드리지 않는다.
///
/// shortcut: 줄 선 요청이 끊기면 앞 비행의 결과를 기다리지 않고 다음 차례가 다시 일한다(작업을
/// 떼어 내 돌리지 않는다). 그래도 한 인스턴스에 붙는 연결은 언제나 하나다.
struct Flights<T> {
    ttl_ms: EpochMs,
    done: Mutex<HashMap<String, (EpochMs, T)>>,
    turns: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl<T: Clone> Flights<T> {
    fn new(ttl_ms: EpochMs) -> Self {
        Self {
            ttl_ms,
            done: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
        }
    }

    async fn run<F>(&self, key: &str, clock: &impl Clock, work: impl FnOnce() -> F) -> T
    where
        F: Future<Output = T>,
    {
        if let Some(hit) = self.fresh(key, clock.now_ms()) {
            return hit;
        }
        let turn = self.turn(key);
        let _held = turn.lock().await;
        // 줄을 서는 동안 앞 비행이 채웠을 수 있다.
        if let Some(hit) = self.fresh(key, clock.now_ms()) {
            return hit;
        }
        let out = work().await;
        // **끝난 시각**으로 적는다 — 시작 시각이면 오래 걸린 결과가 적히자마자 만료된다.
        self.put(key, clock.now_ms(), out.clone());
        out
    }

    fn fresh(&self, key: &str, now_ms: EpochMs) -> Option<T> {
        let done = self.done.lock().unwrap_or_else(|p| p.into_inner());
        done.get(key)
            .filter(|(at, _)| self.alive(*at, now_ms))
            .map(|(_, v)| v.clone())
    }

    fn put(&self, key: &str, now_ms: EpochMs, value: T) {
        let mut done = self.done.lock().unwrap_or_else(|p| p.into_inner());
        // 만료된 것은 버린다 — 한 번 본 인스턴스가 영영 메모리에 남지 않게.
        done.retain(|_, (at, _)| self.alive(*at, now_ms));
        done.insert(key.to_string(), (now_ms, value));
    }

    fn turn(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut turns = self.turns.lock().unwrap_or_else(|p| p.into_inner());
        // 아무도 잡거나 기다리지 않는 줄은 치운다 — 맵만 들고 있는 것이다.
        turns.retain(|_, t| Arc::strong_count(t) > 1);
        Arc::clone(turns.entry(key.to_string()).or_default())
    }

    fn alive(&self, at: EpochMs, now_ms: EpochMs) -> bool {
        now_ms >= at && now_ms - at < self.ttl_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **리터럴이 화면으로 나가면 안 된다.** 락 화면은 저장 정책과 무관하게 항상 마스킹한다.
    #[test]
    fn statements_are_masked_and_unmaskable_ones_are_withheld() {
        let wait = LockWaitRow {
            waiting_pid: 34,
            waiting_sql: Some("UPDATE orders SET memo = 'kim@example.com' WHERE id = 1".into()),
            locked_schema: Some("shop".into()),
            locked_table: Some("orders".into()),
            blocking_pid: Some(31),
            blocking_command: Some("Sleep".into()),
            blocking_statements: vec![
                "UPDATE orders SET status = 'PAID' WHERE id = 1".into(),
                // 1,024바이트에서 잘려 인용부호가 닫히지 않았다 — 마스킹을 믿을 수 없다.
                "SELECT * FROM users WHERE ssn = '900101-12".into(),
            ],
            ..Default::default()
        };
        let snap = build_snapshot("i", 1, vec![wait], vec![], vec![], false);
        let json = serde_json::to_string(&snap).expect("직렬화");
        for secret in ["kim@example.com", "PAID", "900101"] {
            assert!(!json.contains(secret), "{secret} 가 나갔다: {json}");
        }
        let w = &snap.lock_waits[0];
        assert!(w.waiting_sql.as_deref().is_some_and(|s| s.contains('?')));
        assert_eq!(w.blocking_statements.len(), 1, "못 믿는 문장은 버린다");
        assert_eq!(snap.sql_withheld, 1, "버린 수를 말해야 한다");
        assert!(w.blocking_idle, "Sleep 블로커는 유휴다");
        assert_eq!(w.locked_table.as_deref(), Some("shop.orders"));
    }

    #[test]
    fn idle_transactions_and_pending_metadata_locks_are_flagged() {
        let trx = vec![
            TrxRow {
                pid: Some(31),
                command: Some("Sleep".into()),
                ..Default::default()
            },
            TrxRow {
                pid: Some(34),
                command: Some("Query".into()),
                ..Default::default()
            },
        ];
        let mdl = vec![
            MdlRow {
                pid: Some(33),
                lock_status: Some("PENDING".into()),
                ..Default::default()
            },
            MdlRow {
                pid: Some(30),
                lock_status: Some("GRANTED".into()),
                ..Default::default()
            },
        ];
        let snap = build_snapshot("i", 1, vec![], trx, mdl, false);
        assert!(snap.transactions[0].idle);
        assert!(!snap.transactions[1].idle);
        assert!(snap.metadata_locks[0].pending);
        assert!(!snap.metadata_locks[1].pending);
        assert_eq!(snap.sql_withheld, 0, "SQL 이 없으면 버린 것도 없다");
    }

    /// 응답 직전에 빼는 사본에 **문장이 하나도 남지 않는다.** 하나라도 남으면 정책·역할 경계가 샌다.
    #[test]
    fn without_sql_drops_every_statement() {
        let wait = LockWaitRow {
            waiting_pid: 34,
            waiting_sql: Some("UPDATE orders SET memo = ? WHERE id = 1".into()),
            blocking_pid: Some(31),
            blocking_statements: vec!["UPDATE orders SET v = 1 WHERE id = 1".into()],
            ..Default::default()
        };
        let trx = TrxRow {
            pid: Some(31),
            current_sql: Some("SELECT 1".into()),
            statements: vec!["SELECT 1".into()],
            ..Default::default()
        };
        let mdl = MdlRow {
            pid: Some(30),
            sql: Some("ALTER TABLE t ADD c INT".into()),
            ..Default::default()
        };
        let snap = build_snapshot("i", 1, vec![wait], vec![trx], vec![mdl], false);
        let hidden = snap.without_sql("insufficient_role");
        let json = serde_json::to_string(&hidden).expect("직렬화");
        for sql in ["UPDATE", "SELECT", "ALTER"] {
            assert!(!json.contains(sql), "{sql} 가 남았다: {json}");
        }
        assert_eq!(hidden.sql_hidden, Some("insufficient_role"));
        // 누가 누구를 막는지는 그대로다.
        assert_eq!(hidden.lock_waits[0].blocking_pid, Some(31));
        assert_eq!(hidden.transactions[0].pid, Some(31));
    }

    /// **같은 인스턴스는 한 번만 일한다** — 장애 때 여러 사람이 동시에 열어도 대상 DB 에는 하나다.
    #[tokio::test]
    async fn concurrent_callers_share_one_flight() {
        let flights = Flights::<u32>::new(SNAPSHOT_TTL_MS);
        let clock = dbmon_core::fakes::FakeClock::new(1_000_000);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let work = || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // 다른 호출이 줄을 설 틈을 준다.
            tokio::task::yield_now().await;
            7
        };
        let (a, b) = tokio::join!(
            flights.run("i", &clock, work),
            flights.run("i", &clock, work)
        );
        assert_eq!((a, b), (7, 7));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 다른 인스턴스는 줄을 나누지 않는다.
        flights.run("other", &clock, work).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// **실패도 잠깐 다시 쓴다** — 닿지 않는 인스턴스를 화면마다·재시도마다 두드리지 않게.
    #[tokio::test]
    async fn failures_are_reused_until_the_ttl_passes() {
        let flights = Flights::<std::result::Result<u32, LockFailure>>::new(SNAPSHOT_TTL_MS);
        let clock = dbmon_core::fakes::FakeClock::new(1_000_000);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let failing = || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(LockFailure::Unavailable)
        };

        assert_eq!(
            flights.run("i", &clock, failing).await,
            Err(LockFailure::Unavailable)
        );
        clock.advance(SNAPSHOT_TTL_MS - 1);
        assert_eq!(
            flights.run("i", &clock, failing).await,
            Err(LockFailure::Unavailable)
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "기한 안에서는 다시 묻지 않는다"
        );

        clock.advance(1);
        flights.run("i", &clock, failing).await.ok();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "기한이 지나면 다시 묻는다"
        );
    }

    #[test]
    fn hints_and_comments_do_not_leak() {
        let masked =
            masked_sql("SELECT /*+ QB_NAME(secret_qb) */ * FROM t /* api-key=abc */ WHERE a = 1")
                .expect("마스킹된다");
        assert!(!masked.contains("secret_qb"), "{masked}");
        assert!(!masked.contains("abc"), "{masked}");
    }
}
