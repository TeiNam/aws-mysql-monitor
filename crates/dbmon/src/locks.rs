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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dbmon_core::error::Result;
use dbmon_core::instance::Instance;
use dbmon_core::ports::{LockWaitRow, TargetDb};
use dbmon_core::time::EpochMs;
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
    }
}

/// 요청 시 스냅샷 서비스.
pub struct LockService {
    auth: Arc<TargetAuth>,
    config: Arc<Config>,
    /// 인스턴스 → (찍은 시각, 스냅샷).
    ///
    /// shortcut: 같은 인스턴스를 동시에 처음 여는 요청은 둘 다 조회한다(단일 비행 없음).
    /// 보는 사람이 많아져 대상 DB 연결이 문제 되면 인스턴스별 비행 락을 둔다.
    cache: Mutex<HashMap<String, (EpochMs, Arc<LockSnapshot>)>>,
}

impl LockService {
    pub fn new(auth: Arc<TargetAuth>, config: Arc<Config>) -> Self {
        Self {
            auth,
            config,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn snapshot(
        &self,
        instance: &Instance,
        now_ms: EpochMs,
    ) -> Result<Arc<LockSnapshot>> {
        let key = instance.id.as_str().to_string();
        if let Some(hit) = self.cached(&key, now_ms) {
            return Ok(hit);
        }
        let db = TargetMysql::on_demand(&self.auth, &self.config, instance, "locks").await?;
        let collected = collect(&db).await;
        // **닫는다.** 화면이 5초마다 묻는데 버리면 대상 DB 에 `Aborted_clients` 가 쌓인다.
        db.disconnect().await;
        let (waits, trx, mdl) = collected?;
        let truncated = waits.len() as u64 >= LOCK_WAITS_LIMIT
            || trx.len() as u64 >= MAX_TRANSACTIONS
            || mdl.len() as u64 >= MAX_MDL_ROWS;
        let snap = Arc::new(build_snapshot(&key, now_ms, waits, trx, mdl, truncated));
        self.store(key, now_ms, Arc::clone(&snap));
        Ok(snap)
    }

    fn cached(&self, key: &str, now_ms: EpochMs) -> Option<Arc<LockSnapshot>> {
        let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache
            .get(key)
            .filter(|(at, _)| now_ms >= *at && now_ms - at < SNAPSHOT_TTL_MS)
            .map(|(_, snap)| Arc::clone(snap))
    }

    fn store(&self, key: String, now_ms: EpochMs, snap: Arc<LockSnapshot>) {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        // 만료된 것은 버린다 — 한 번 본 인스턴스가 영영 메모리에 남지 않게.
        cache.retain(|_, (at, _)| now_ms >= *at && now_ms - *at < SNAPSHOT_TTL_MS);
        cache.insert(key, (now_ms, snap));
    }
}

async fn collect(db: &TargetMysql) -> Result<(Vec<LockWaitRow>, Vec<TrxRow>, Vec<MdlRow>)> {
    let waits = db.lock_waits(&[]).await?;
    let trx = db.transactions(MIN_TRX_AGE_SECS, MAX_TRANSACTIONS).await?;
    let mdl = db.metadata_lock_waits(MAX_MDL_ROWS).await?;
    Ok((waits, trx, mdl))
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

    #[test]
    fn hints_and_comments_do_not_leak() {
        let masked =
            masked_sql("SELECT /*+ QB_NAME(secret_qb) */ * FROM t /* api-key=abc */ WHERE a = 1")
                .expect("마스킹된다");
        assert!(!masked.contains("secret_qb"), "{masked}");
        assert!(!masked.contains("abc"), "{masked}");
    }
}
