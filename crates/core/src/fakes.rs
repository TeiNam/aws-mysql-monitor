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
        //
        // **같은 키라도 같은 실행인지 확인한다.** 멱등 키는 `(인스턴스, 스레드, 시작
        // 초)` 이므로 중첩 문장처럼 같은 초에 시작한 다른 문장이 같은 키를 갖는다.
        // 실제 어댑터와 같은 판정을 쓴다.
        let incoming = crate::slow_query::ExecutionSpan::of(q);
        if let Some(existing) = items.get(&key) {
            let same = existing.started_at_ms == q.started_at_ms
                || crate::slow_query::ExecutionSpan::of(existing).is_same_execution_within(
                    &incoming,
                    crate::clock_offset::LIVE_ESTIMATE_SPREAD_MS,
                );
            if same {
                let merged = merge(existing, q);
                items.insert(key, merged.clone());
                return Ok(merged);
            }
        }
        // 2) ±2초 보조 조회 — 시작 시각 추정이 1초 어긋난 같은 실행을 찾는다.
        //
        // **실제 어댑터와 같은 규칙을 쓴다**(`ExecutionSpan::is_same_execution`).
        // 계약이 갈리면 단위 테스트가 통과하면서 실제만 틀린다 — 이미 한 번 겪었다
        // (`find_merge_candidate` 에 프로덕션 호출부가 없던 일).
        let incoming = crate::slow_query::ExecutionSpan::of(q);
        let candidate_key = items
            .values()
            .find(|e| {
                e.instance_id == q.instance_id
                    && e.thread_id == q.thread_id
                    && (e.started_at_ms - q.started_at_ms).abs()
                        <= crate::clock_offset::BASE_MERGE_WINDOW_MS
                    && crate::slow_query::ExecutionSpan::of(e).is_same_execution_within(
                        &incoming,
                        crate::clock_offset::LIVE_ESTIMATE_SPREAD_MS,
                    )
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

pub struct FakeInstanceRegistry {
    items: Mutex<BTreeMap<String, Instance>>,
    /// 중복 미발견 방지 간격. **프로덕션은 탐색 주기에서 유도한다** — 페이크가 고정값을
    /// 쓰면 짧은 주기를 쓰는 배포의 테스트가 거짓말을 한다(교차 리뷰 4회차).
    /// 기본은 기본 주기(300초) 기준이고 [`Self::with_discovery_interval`] 로 바꾼다.
    missing_min_gap_ms: Mutex<i64>,
    /// 마지막으로 미발견을 센 시각. **실제 어댑터의 `missing_at_ms` 속성과 같은 역할**
    /// 이다 — 한 라운드를 두 번 세지 않기 위한 것이고, `Instance` 필드가 아니므로
    /// 여기 따로 둔다.
    missing_at: Mutex<BTreeMap<String, EpochMs>>,
}

/// **파생 `Default` 를 쓰지 않는다.** 그러면 `missing_min_gap_ms` 가 0 이 되어 중복
/// 미발견 방지가 꺼지고, 페이크로 그 규칙을 검증하는 테스트가 전부 무의미해진다.
impl Default for FakeInstanceRegistry {
    fn default() -> Self {
        Self {
            items: Mutex::new(BTreeMap::new()),
            missing_at: Mutex::new(BTreeMap::new()),
            missing_min_gap_ms: Mutex::new(crate::instance::missing_min_gap_ms(300)),
        }
    }
}

impl FakeInstanceRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    /// 어댑터와 같은 함수로 간격을 유도한다.
    pub fn with_discovery_interval(self, interval_secs: u64) -> Self {
        *self.missing_min_gap_ms.lock().unwrap() =
            crate::instance::missing_min_gap_ms(interval_secs);
        self
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
        // **한 라운드를 두 번 세지 않는다.** 실제 어댑터는 `missing_at_ms` 를 항목에
        // 쓰고 조건부 갱신으로 막는다. 페이크가 그 규칙을 빼면 페이크로 테스트하는
        // 코드가 실제와 다른 동작을 본다 — `deleted_at_ms` 계약에서 이미 겪었다.
        let last = self.missing_at.lock().unwrap().get(id.as_str()).copied();
        if let Some(prev) = last {
            // **어댑터와 같은 비교다.**
            //
            // DynamoDB 조건은 `missing_at_ms < :cutoff` 이고 `cutoff = now - gap` 이므로
            // 세는 조건은 `prev < now - gap` ⟺ `now - prev > gap` 이다. 즉 **경계(같은
            // 값)는 거부된다.** 페이크가 `<` 로 두면 경계에서 세고 계약이 갈린다 —
            // 교차 리뷰 4회차가 지적하고 계약 테스트가 실증했다.
            if now_ms - prev <= *self.missing_min_gap_ms.lock().unwrap() {
                return Ok(inst.clone());
            }
        }
        self.missing_at
            .lock()
            .unwrap()
            .insert(id.as_str().to_string(), now_ms);

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

    async fn set_state(
        &self,
        id: &InstanceId,
        state: crate::instance::InstanceState,
    ) -> Result<()> {
        let mut m = self.items.lock().unwrap();
        // 프로덕션 구현이 `attribute_exists(PK)` 를 걸므로 없는 항목은 오류다.
        let inst = m
            .get_mut(id.as_str())
            .ok_or_else(|| DomainError::NotFound {
                kind: "인스턴스",
                id: id.to_string(),
            })?;
        // **"수집하지 않는다" 는 결정을 덮지 않는다.** 실제 어댑터가 조건부 갱신으로
        // 막고 `Conflict` 를 올린다 — 규칙과 오류 종류를 함께 맞춘다.
        if crate::instance::blocks_state_write(inst.state) {
            return Err(DomainError::Conflict(format!(
                "{}: 수집 대상이 아닌 상태여서 {} 로 바꾸지 않았다",
                id.as_str(),
                state.as_str()
            )));
        }
        inst.state = state;
        Ok(())
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
        // 중복 제거 도장도 지운다 — 남으면 다시 사라졌을 때 첫 미발견이 거부된다.
        self.missing_at.lock().unwrap().remove(id.as_str());
        Ok(())
    }
}

// ── LeaseStore ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FakeLeaseStore {
    leases: Mutex<BTreeMap<String, Lease>>,
    /// 남은 갱신 실패 횟수. **저장소 장애를 재현한다** — 그 상태에서 만료 시점에
    /// 리더를 그만두는지가 중복 수집 방지의 마지막 장치다.
    renew_failures: Mutex<usize>,
}

impl FakeLeaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 다음 `n` 회 `renew` 를 저장소 오류로 실패시킨다.
    pub fn fail_next_renewals(&self, n: usize) {
        *self.renew_failures.lock().unwrap() = n;
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
        {
            let mut fails = self.renew_failures.lock().unwrap();
            if *fails > 0 {
                *fails -= 1;
                return Err(DomainError::Unavailable {
                    dependency: "dynamodb",
                    reason: "테스트 주입 실패".into(),
                });
            }
        }
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

// ── PauseStore ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FakePauseStore {
    since: Mutex<BTreeMap<String, EpochMs>>,
    /// `list` 를 실패시킨다. **저장소 장애 중에 정지가 풀리지 않는지**가 요점이다.
    fail_list: AtomicBool,
    /// `list` 호출 횟수. 캐시가 실제로 읽기를 아끼는지 센다.
    pub list_count: AtomicUsize,
}

impl FakePauseStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_failing(&self, failing: bool) {
        self.fail_list.store(failing, Ordering::SeqCst);
    }

    pub fn list_calls(&self) -> usize {
        self.list_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl crate::ports::PauseStore for FakePauseStore {
    async fn list(&self) -> Result<crate::pause::PauseSet> {
        self.list_count.fetch_add(1, Ordering::SeqCst);
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(DomainError::Unavailable {
                dependency: "pause_store",
                reason: "주입된 장애".into(),
            });
        }
        let m = self.since.lock().unwrap();
        Ok(crate::pause::PauseSet::from_entries(
            m.iter().map(|(k, v)| (k.clone(), *v)),
        ))
    }

    /// 프로덕션 구현의 `if_not_exists` 의미를 재현한다 — **시작 시각을 덮지 않는다.**
    async fn pause(
        &self,
        scope: &crate::pause::PauseScope,
        _by: &str,
        now_ms: EpochMs,
    ) -> Result<()> {
        self.since
            .lock()
            .unwrap()
            .entry(scope.as_key())
            .or_insert(now_ms);
        Ok(())
    }

    async fn resume(&self, scope: &crate::pause::PauseScope) -> Result<()> {
        self.since.lock().unwrap().remove(&scope.as_key());
        Ok(())
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
    /// `global_status` 가 돌려줄 변수들.
    pub status_vars: Mutex<BTreeMap<String, String>>,
    /// `global_status` 를 앞으로 n 번 실패시킨다.
    fail_global_status: AtomicUsize,
    /// 호출 횟수 (경로가 실제로 돌았는지 확인용).
    pub full_sql_calls: AtomicUsize,
    pub explain_calls: AtomicUsize,
    warm_calls: AtomicUsize,
}

impl FakeTargetDb {
    pub fn new() -> Self {
        Self::default()
    }

    /// `global_status` 가 이 변수들을 돌려준다.
    pub fn set_status(&self, pairs: &[(&str, &str)]) {
        *self.status_vars.lock().unwrap() = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    }

    /// `global_status` 를 앞으로 n 번 실패시킨다.
    pub fn fail_global_status(&self, n: usize) {
        self.fail_global_status.store(n, Ordering::SeqCst);
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

    async fn explain_rerun(&self, _sql: &str, _schema: Option<&str>) -> Result<ExplainOutcome> {
        self.explain_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .explain
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(ExplainOutcome::Failed(PlanFailure::NotExplainable)))
    }

    async fn explain_tree(&self, _sql: &str, _schema: Option<&str>) -> Result<Option<String>> {
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
        if Self::should_fail(&self.fail_global_status) {
            return Err(DomainError::Unavailable {
                dependency: "target-mysql",
                reason: "global_status: 연결 획득 타임아웃".into(),
            });
        }
        Ok(self.status_vars.lock().unwrap().clone())
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
