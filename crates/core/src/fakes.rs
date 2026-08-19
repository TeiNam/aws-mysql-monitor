//! 인메모리 페이크 구현 — **포트의 두 번째 구현**.
//!
//! 이게 있어야 `core` 의 trait 가 정당하고([ADR-002](../../../docs/03-decisions.md)),
//! 무엇보다 **AWS 없이 수집기 전체를 테스트할 수 있다.**
//! SSO 세션이 만료돼도 개발이 멈추지 않는 근거다.
//!
//! 페이크는 프로덕션 구현의 **관찰 가능한 계약**을 재현한다. 특히
//! - `upsert_merged` 는 실제로 병합한다 (덮어쓰지 않는다)
//! - `try_acquire` 는 조건부 쓰기 의미를 지킨다 (만료 전에는 실패)
//! - `put_rollups` 는 부분 실패를 주입할 수 있다

use crate::error::{DomainError, Result};
use crate::ids::{InstanceId, RecordId};
use crate::instance::Instance;
use crate::merge::merge;
use crate::ports::target_db::{
    DigestSnapshot, DigestTextRow, Excludes, ExplainOutcome, FullSqlRow, PlanFailure, ProbeResult,
    ProcessRow, StmtCurrentRow, TargetDb,
};
use crate::ports::{
    DigestStore, DigestTextEntry, InstanceRegistry, LEASE_TTL_MS, Lease, LeaseStore, SlowQueryStore,
};
use crate::rollup::DigestRollupRow;
use crate::slow_query::SlowQuery;
use crate::time::{Clock, EpochMs, TimeRange};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 주입 가능한 시계. `advance()` 로 시간을 밀어 상태 머신을 테이블 주도로 검증한다 (R5).
#[derive(Debug, Clone)]
pub struct FakeClock(Arc<AtomicI64>);

impl FakeClock {
    pub fn new(start_ms: EpochMs) -> Self {
        Self(Arc::new(AtomicI64::new(start_ms)))
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: EpochMs) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> EpochMs {
        self.0.load(Ordering::SeqCst)
    }
}

// ── SlowQueryStore ──────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FakeSlowQueryStore {
    items: Mutex<BTreeMap<String, SlowQuery>>,
    /// 이 횟수만큼 `upsert_merged` 를 실패시킨다 (재시도 경로 검증).
    fail_next: AtomicUsize,
    pub upsert_count: AtomicUsize,
}

impl FakeSlowQueryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next(&self, n: usize) {
        self.fail_next.store(n, Ordering::SeqCst);
    }

    pub fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn all(&self) -> Vec<SlowQuery> {
        self.items.lock().unwrap().values().cloned().collect()
    }

    fn take_failure(&self) -> Option<DomainError> {
        let n = self.fail_next.load(Ordering::SeqCst);
        if n == 0 {
            return None;
        }
        self.fail_next.store(n - 1, Ordering::SeqCst);
        Some(DomainError::Unavailable {
            dependency: "fake-store",
            reason: "injected".into(),
        })
    }
}

#[async_trait]
impl SlowQueryStore for FakeSlowQueryStore {
    async fn upsert_merged(&self, q: &SlowQuery) -> Result<SlowQuery> {
        if let Some(e) = self.take_failure() {
            return Err(e);
        }
        self.upsert_count.fetch_add(1, Ordering::SeqCst);
        let mut items = self.items.lock().unwrap();
        let key = q.record_id.as_str().to_string();

        // 1) record_id 직접 조회
        if let Some(existing) = items.get(&key) {
            let merged = merge(existing, q);
            items.insert(key, merged.clone());
            return Ok(merged);
        }
        // 2) ±2초 보조 조회 — 시작 시각 추정이 1초 어긋난 같은 실행을 찾는다.
        let candidate_key = items
            .values()
            .find(|e| {
                e.instance_id == q.instance_id
                    && e.thread_id == q.thread_id
                    && e.app_digest == q.app_digest
                    && (e.started_at_ms - q.started_at_ms).abs() <= 2_000
            })
            .map(|e| e.record_id.as_str().to_string());
        if let Some(ck) = candidate_key {
            let existing = items.get(&ck).unwrap().clone();
            let merged = merge(&existing, q);
            items.insert(ck, merged.clone());
            return Ok(merged);
        }
        items.insert(key, q.clone());
        Ok(q.clone())
    }

    async fn get(&self, id: &RecordId) -> Result<Option<SlowQuery>> {
        Ok(self.items.lock().unwrap().get(id.as_str()).cloned())
    }

    async fn find_merge_candidate(
        &self,
        instance: &InstanceId,
        thread_id: u64,
        app_digest: &str,
        around_ms: EpochMs,
        window_ms: i64,
    ) -> Result<Option<SlowQuery>> {
        Ok(self
            .items
            .lock()
            .unwrap()
            .values()
            .find(|e| {
                e.instance_id == *instance
                    && e.thread_id == thread_id
                    && e.app_digest == app_digest
                    && (e.started_at_ms - around_ms).abs() <= window_ms
            })
            .cloned())
    }

    async fn list_by_instance(
        &self,
        instance: &InstanceId,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<SlowQuery>> {
        let mut out: Vec<SlowQuery> = self
            .items
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.instance_id == *instance && range.contains(e.started_at_ms))
            .cloned()
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.started_at_ms));
        out.truncate(limit);
        Ok(out)
    }

    async fn list_in_flight(&self, limit: usize) -> Result<Vec<SlowQuery>> {
        let mut out: Vec<SlowQuery> = self
            .items
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.state == crate::slow_query::SlowQueryState::InFlight)
            .cloned()
            .collect();
        out.sort_by_key(|e| e.last_seen_at_ms.unwrap_or(e.started_at_ms));
        out.truncate(limit);
        Ok(out)
    }
}

// ── DigestStore ─────────────────────────────────────────────────────────────

/// 저장된 롤업 한 건. clippy 의 복잡한 타입 경고를 피하고 의도를 드러낸다.
pub type StoredRollup = (InstanceId, DigestRollupRow);

pub struct FakeDigestStore {
    pub rollups: Mutex<Vec<StoredRollup>>,
    pub texts: Mutex<BTreeMap<String, DigestTextEntry>>,
    /// 배치의 앞 N 건만 성공시킨다 (부분 실패 주입, F20).
    partial_success: AtomicUsize,
}

impl Default for FakeDigestStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDigestStore {
    /// **`Default` 를 직접 derive 하지 않는 이유**: `AtomicUsize::default()` 는 0 이고
    /// 그건 `accept_only(0)`, 즉 **전부 거부**를 뜻한다. `new()` 는 전부 수락(`MAX`)이라
    /// 두 생성자가 정반대로 동작했다 — `default()` 로 만든 페이크는 아무것도 저장하지
    /// 않으면서 성공을 반환했다. 이제 둘이 같다.
    pub fn new() -> Self {
        Self {
            partial_success: AtomicUsize::new(usize::MAX),
            rollups: Default::default(),
            texts: Default::default(),
        }
    }

    /// 다음 `put_rollups` 에서 앞 `n` 건만 성공시킨다.
    pub fn accept_only(&self, n: usize) {
        self.partial_success.store(n, Ordering::SeqCst);
    }

    pub fn rollup_count(&self) -> usize {
        self.rollups.lock().unwrap().len()
    }
}

#[async_trait]
impl DigestStore for FakeDigestStore {
    async fn put_rollups(
        &self,
        instance: &InstanceId,
        rows: &[DigestRollupRow],
    ) -> Result<Vec<DigestRollupRow>> {
        let accept = self
            .partial_success
            .swap(usize::MAX, Ordering::SeqCst)
            .min(rows.len());
        let mut store = self.rollups.lock().unwrap();
        for r in &rows[..accept] {
            store.push((instance.clone(), r.clone()));
        }
        Ok(rows[accept..].to_vec())
    }

    async fn upsert_digest_text(&self, entry: &DigestTextEntry) -> Result<()> {
        let mut texts = self.texts.lock().unwrap();
        match texts.get_mut(&entry.app_digest) {
            // 맵·집합 원자 갱신을 흉내낸다 — 두 경로가 서로의 엔트리를 잃지 않아야 한다 (F8).
            Some(existing) => {
                if let Some(md) = &entry.mysql_digest {
                    let already = matches!(&existing.mysql_digest, Some((i, _)) if *i == md.0);
                    if !already {
                        existing.mysql_digest = Some(md.clone());
                    }
                }
                existing.last_seen_merge(entry);
            }
            None => {
                texts.insert(entry.app_digest.clone(), entry.clone());
            }
        }
        Ok(())
    }
}

impl DigestTextEntry {
    /// 페이크가 쓰는 병합 규칙. 프로덕션은 DynamoDB 경로 갱신으로 같은 효과를 낸다.
    fn last_seen_merge(&mut self, other: &DigestTextEntry) {
        self.observed_at_ms = self.observed_at_ms.max(other.observed_at_ms);
        if self.ps_sample_text.is_none() {
            self.ps_sample_text = other.ps_sample_text.clone();
        }
        for t in &other.referenced_tables {
            if !self.referenced_tables.contains(t) {
                self.referenced_tables.push(t.clone());
            }
        }
    }
}

// ── InstanceRegistry ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FakeInstanceRegistry {
    items: Mutex<BTreeMap<String, Instance>>,
}

impl FakeInstanceRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn seed(&self, instances: Vec<Instance>) {
        let mut m = self.items.lock().unwrap();
        for i in instances {
            m.insert(i.id.as_str().to_string(), i);
        }
    }
}

#[async_trait]
impl InstanceRegistry for FakeInstanceRegistry {
    async fn list(&self) -> Result<Vec<Instance>> {
        Ok(self.items.lock().unwrap().values().cloned().collect())
    }

    async fn get(&self, id: &InstanceId) -> Result<Option<Instance>> {
        Ok(self.items.lock().unwrap().get(id.as_str()).cloned())
    }

    async fn upsert(&self, instance: &Instance) -> Result<()> {
        self.items
            .lock()
            .unwrap()
            .insert(instance.id.as_str().to_string(), instance.clone());
        Ok(())
    }

    async fn mark_missing(&self, id: &InstanceId, now_ms: EpochMs) -> Result<Instance> {
        let mut m = self.items.lock().unwrap();
        let inst = m
            .get_mut(id.as_str())
            .ok_or_else(|| DomainError::NotFound {
                kind: "인스턴스",
                id: id.to_string(),
            })?;
        inst.missing_count += 1;
        // 1회 API 실패로 삭제되지 않는다 (FR-DSC-07).
        // **술어를 여기서 다시 적지 않는다** — 상수만 공유하면 규칙의 드리프트를 못 막는다.
        if crate::instance::should_mark_deleted(inst.missing_count) && inst.deleted_at_ms.is_none()
        {
            inst.deleted_at_ms = Some(now_ms);
            inst.state = crate::instance::InstanceState::Deleted;
        }
        Ok(inst.clone())
    }

    async fn mark_seen(&self, id: &InstanceId, now_ms: EpochMs) -> Result<()> {
        let mut m = self.items.lock().unwrap();
        if let Some(inst) = m.get_mut(id.as_str()) {
            inst.missing_count = 0;
            inst.last_seen_ms = now_ms;
            // **실제 어댑터와 계약을 맞춘다.** 페이크가 삭제 도장을 남기면, 페이크로
            // 단위 테스트하는 코드는 실제 동작과 다른 상태를 본다(2차 리뷰가 지적).
            inst.deleted_at_ms = None;
        }
        Ok(())
    }
}

// ── LeaseStore ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FakeLeaseStore {
    leases: Mutex<BTreeMap<String, Lease>>,
}

impl FakeLeaseStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LeaseStore for FakeLeaseStore {
    async fn try_acquire(&self, key: &str, owner: &str, now_ms: EpochMs) -> Result<Option<Lease>> {
        let mut m = self.leases.lock().unwrap();
        let next_epoch = match m.get(key) {
            // 조건부 쓰기 의미: 만료 전에는 남의 리스를 가져갈 수 없다.
            Some(l) if l.expires_at_ms > now_ms => return Ok(None),
            Some(l) => l.epoch + 1,
            None => 1,
        };
        let lease = Lease {
            key: key.to_string(),
            owner: owner.to_string(),
            expires_at_ms: now_ms + LEASE_TTL_MS,
            epoch: next_epoch,
        };
        m.insert(key.to_string(), lease.clone());
        Ok(Some(lease))
    }

    async fn renew(&self, lease: &Lease, now_ms: EpochMs) -> Result<Option<Lease>> {
        let mut m = self.leases.lock().unwrap();
        match m.get(&lease.key) {
            // owner 와 epoch 가 모두 일치해야 한다. 하나라도 다르면 이미 빼앗겼다.
            Some(cur) if cur.owner == lease.owner && cur.epoch == lease.epoch => {
                let renewed = Lease {
                    expires_at_ms: now_ms + LEASE_TTL_MS,
                    ..lease.clone()
                };
                m.insert(lease.key.clone(), renewed.clone());
                Ok(Some(renewed))
            }
            _ => Ok(None),
        }
    }

    async fn release(&self, lease: &Lease) -> Result<()> {
        let mut m = self.leases.lock().unwrap();
        if m.get(&lease.key)
            .is_some_and(|c| c.owner == lease.owner && c.epoch == lease.epoch)
        {
            m.remove(&lease.key);
        }
        Ok(())
    }

    async fn list(&self, key_prefix: &str) -> Result<Vec<Lease>> {
        Ok(self
            .leases
            .lock()
            .unwrap()
            .values()
            .filter(|l| l.key.starts_with(key_prefix))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::COLLECT_LEADER_KEY;

    #[test]
    fn fake_clock_is_shared_and_advances() {
        let c = FakeClock::new(1_000);
        let c2 = c.clone();
        c.advance(500);
        assert_eq!(c2.now_ms(), 1_500);
        c2.set(9);
        assert_eq!(c.now_ms(), 9);
    }

    #[tokio::test]
    async fn lease_is_exclusive_until_expiry() {
        let store = FakeLeaseStore::new();
        let a = store
            .try_acquire(COLLECT_LEADER_KEY, "worker-a", 0)
            .await
            .unwrap();
        assert!(a.is_some(), "빈 리스는 획득 가능");
        assert!(
            store
                .try_acquire(COLLECT_LEADER_KEY, "worker-b", 0)
                .await
                .unwrap()
                .is_none(),
            "만료 전에는 두 번째 워커가 획득하지 못한다 — 이게 F1 방어의 기반이다"
        );

        // TTL 이 지나면 인수 가능하고 epoch 가 올라간다.
        let b = store
            .try_acquire(COLLECT_LEADER_KEY, "worker-b", LEASE_TTL_MS + 1)
            .await
            .unwrap()
            .expect("만료 후에는 획득 가능");
        assert_eq!(b.epoch, 2, "epoch 는 획득마다 증가한다 (펜싱 토큰 기반)");
    }

    #[tokio::test]
    async fn renew_fails_after_being_taken_over() {
        let store = FakeLeaseStore::new();
        let old = store
            .try_acquire("SHARD#01", "a", 0)
            .await
            .unwrap()
            .unwrap();
        let _new = store
            .try_acquire("SHARD#01", "b", LEASE_TTL_MS + 1)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store.renew(&old, LEASE_TTL_MS + 2).await.unwrap().is_none(),
            "이전 소유자의 갱신은 실패해야 한다 — 실패하면 그 샤드를 포기한다"
        );
    }

    #[tokio::test]
    async fn release_only_affects_own_lease() {
        let store = FakeLeaseStore::new();
        let a = store
            .try_acquire("SHARD#02", "a", 0)
            .await
            .unwrap()
            .unwrap();
        let stale = Lease {
            owner: "c".into(),
            ..a.clone()
        };
        store.release(&stale).await.unwrap();
        assert_eq!(
            store.list("SHARD#").await.unwrap().len(),
            1,
            "남의 리스를 지우면 안 된다"
        );
        store.release(&a).await.unwrap();
        assert!(store.list("SHARD#").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn partial_rollup_failure_returns_leftovers() {
        use crate::digest::DigestDelta;
        use crate::rollup::{HourAccumulator, RollupConfig};

        let store = FakeDigestStore::new();
        let inst = InstanceId::new("123456789012", "ap-northeast-2", "t").unwrap();
        let mut acc = HourAccumulator::new(crate::HourBucket::from_epoch_ms(0), 1);
        for i in 0..5 {
            acc.add(
                &format!("d{i}"),
                None,
                &DigestDelta {
                    exec_count: 1,
                    total_time_ms: 1_000,
                    ..Default::default()
                },
                0,
                false,
            );
        }
        let rows = acc.build_rows(RollupConfig::default());
        store.accept_only(2);
        let leftover = store.put_rollups(&inst, &rows).await.unwrap();
        assert_eq!(
            leftover.len(),
            rows.len() - 2,
            "남은 항목을 반환해 재시도할 수 있어야 한다"
        );
        assert_eq!(store.rollup_count(), 2);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TargetDb
// ─────────────────────────────────────────────────────────────────────────────

/// 대상 DB 페이크. **실패 주입이 요점이다.**
///
/// 실패 경로는 Docker 로 재현하기 어렵다(권한 오류·타임아웃을 원하는 순간에 만들 수 없다).
/// 그런데 이 코드베이스에서 가장 위험한 결함들이 전부 그 경로에 있었다 — 조회 실패가
/// 확정을 영구히 잃거나, 빈 레코드를 저장하거나, 실패를 성공으로 세는 것들이다.
#[derive(Default)]
pub struct FakeTargetDb {
    /// `probe` 가 돌려줄 행.
    pub rows: Mutex<Vec<ProcessRow>>,
    /// `probe.truncated`.
    pub truncated: AtomicBool,
    /// `full_sql` 이 돌려줄 행.
    pub full: Mutex<Vec<FullSqlRow>>,
    /// `stmt_current` 가 돌려줄 행.
    pub stmts: Mutex<Vec<StmtCurrentRow>>,
    /// `probe` 를 앞으로 n 번 실패시킨다. **연결 실패를 재현한다.**
    fail_probe: AtomicUsize,
    /// `full_sql` 을 앞으로 n 번 실패시킨다.
    fail_full_sql: AtomicUsize,
    /// `stmt_current` 를 앞으로 n 번 실패시킨다.
    fail_stmt_current: AtomicUsize,
    /// `explain_*` 결과.
    pub explain: Mutex<Option<ExplainOutcome>>,
    /// 대상의 전역 `sql_mode`. 어휘 발산 테스트가 여기에 값을 넣는다.
    pub sql_mode: Mutex<String>,
    /// 호출 횟수 (경로가 실제로 돌았는지 확인용).
    pub full_sql_calls: AtomicUsize,
    pub explain_calls: AtomicUsize,
    warm_calls: AtomicUsize,
}

impl FakeTargetDb {
    pub fn new() -> Self {
        Self::default()
    }

    /// `probe` 가 이 스레드들을 돌려준다.
    pub fn with_threads(self, ids: &[(u64, i64)]) -> Self {
        *self.rows.lock().unwrap() = ids
            .iter()
            .map(|(id, time_secs)| ProcessRow {
                id: *id,
                user: Some("app".into()),
                host: Some("10.0.3.44:5000".into()),
                db: Some("shop".into()),
                command: Some("Query".into()),
                time_secs: *time_secs,
                state: Some("executing".into()),
            })
            .collect();
        self
    }

    /// `probe` 결과를 바꾼다 (스레드가 나타나고 사라지는 상황).
    pub fn with_threads_mut(&self, ids: &[(u64, i64)]) {
        *self.rows.lock().unwrap() = ids
            .iter()
            .map(|(id, time_secs)| ProcessRow {
                id: *id,
                user: Some("app".into()),
                host: Some("10.0.3.44:5000".into()),
                db: Some("shop".into()),
                command: Some("Query".into()),
                time_secs: *time_secs,
                state: Some("executing".into()),
            })
            .collect();
    }

    /// `probe.truncated` 를 설정한다.
    pub fn set_truncated(&self, v: bool) {
        self.truncated.store(v, Ordering::SeqCst);
    }

    /// 대상의 전역 `sql_mode` 를 설정한다.
    pub fn set_sql_mode(&self, mode: &str) {
        *self.sql_mode.lock().unwrap() = mode.to_string();
    }

    /// `warm` 이 몇 번 호출됐는가. **표시만 하고 아무도 부르지 않는 실수**를 잡는다.
    pub fn warm_calls(&self) -> usize {
        self.warm_calls.load(Ordering::SeqCst)
    }

    /// `probe` 를 앞으로 n 번 실패시킨다. `acquire()` 의 연결 획득 타임아웃과 같은 형태다.
    pub fn fail_probe(&self, n: usize) {
        self.fail_probe.store(n, Ordering::SeqCst);
    }

    pub fn fail_full_sql(&self, n: usize) {
        self.fail_full_sql.store(n, Ordering::SeqCst);
    }

    pub fn fail_stmt_current(&self, n: usize) {
        self.fail_stmt_current.store(n, Ordering::SeqCst);
    }

    fn should_fail(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                n.checked_sub(1).or(Some(0))
            })
            .is_ok_and(|prev| prev > 0)
    }
}

#[async_trait]
impl TargetDb for FakeTargetDb {
    async fn probe(&self, _threshold_secs: u32, _excludes: &Excludes) -> Result<ProbeResult> {
        if Self::should_fail(&self.fail_probe) {
            return Err(DomainError::Unavailable {
                dependency: "target-mysql",
                reason: "probe: 연결 획득 타임아웃".into(),
            });
        }
        Ok(ProbeResult {
            rows: self.rows.lock().unwrap().clone(),
            truncated: self.truncated.load(Ordering::SeqCst),
            db_now_ms: None,
        })
    }

    async fn full_sql(&self, _ids: &[u64]) -> Result<Vec<FullSqlRow>> {
        self.full_sql_calls.fetch_add(1, Ordering::SeqCst);
        if Self::should_fail(&self.fail_full_sql) {
            return Err(DomainError::Forbidden {
                action: "full_sql: injected 1142".into(),
            });
        }
        Ok(self.full.lock().unwrap().clone())
    }

    async fn stmt_current(&self, _ids: &[u64]) -> Result<Vec<StmtCurrentRow>> {
        if Self::should_fail(&self.fail_stmt_current) {
            return Err(DomainError::Unavailable {
                dependency: "target-mysql",
                reason: "stmt_current: injected timeout".into(),
            });
        }
        Ok(self.stmts.lock().unwrap().clone())
    }

    async fn explain_for_connection(&self, _connection_id: u64) -> Result<ExplainOutcome> {
        // RDS 의 정상 동작을 재현한다.
        Ok(ExplainOutcome::Failed(PlanFailure::Denied))
    }

    async fn explain_rerun(&self, _sql: &str) -> Result<ExplainOutcome> {
        self.explain_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .explain
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(ExplainOutcome::Failed(PlanFailure::NotExplainable)))
    }

    async fn explain_tree(&self, _sql: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn digest_snapshot(&self, _last_seen_gte_ms: Option<EpochMs>) -> Result<DigestSnapshot> {
        Ok(DigestSnapshot {
            rows: Vec::new(),
            db_now_ms: 0,
            overflow_detected: false,
        })
    }

    async fn digest_texts(&self, _digests: &[String]) -> Result<Vec<DigestTextRow>> {
        Ok(Vec::new())
    }

    async fn global_status(&self) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    async fn statement_digest(&self, _sql: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn db_now_ms(&self) -> Result<EpochMs> {
        Ok(0)
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn warm(&self) -> Result<()> {
        self.warm_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn target_sql_mode(&self) -> Result<String> {
        Ok(self.sql_mode.lock().unwrap().clone())
    }
}
