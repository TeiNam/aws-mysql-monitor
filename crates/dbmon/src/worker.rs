//! 수집 워커 루프 — 리스·리더 게이트·tick 을 조립한다 (M4-21 배선).
//!
//! # 이 파일이 F1 을 실제로 강제하는 지점이다
//!
//! [`crate::store::lease::target_shard_count`] 가 불변식을 계산하지만, **아무도 부르지
//! 않으면 무효다** — 이 프로젝트에서 "고쳤는데 호출부가 없다" 가 네 번 재발했다.
//! 이 루프가 그 유일한 호출부이고, [`LeaderGate`] 가 상태를 들고 있다.
//!
//! # 왜 리더가 아니면 아무것도 안 하는가
//!
//! `desiredCount=2` 로 띄우면 두 워커가 뜬다. 둘 다 수집하면 같은 인스턴스를 중복
//! 수집하고 다이제스트 누산기가 last-writer-wins 로 조용히 손상된다. 그래서
//! **리더만 수집한다.** 리더가 없는 순간(재분배 중)에는 아무도 수집하지 않는다 —
//! 최대 80초 공백(TTL 60 + 스캔 20)이고, 그게 중복 수집보다 낫다.

use std::sync::Arc;
use std::time::Duration;

use dbmon_core::ports::{COLLECT_LEADER_KEY, LEASE_RENEW_INTERVAL_MS, Lease, LeaseStore};
use dbmon_core::time::{Clock, EpochMs};

use crate::store::lease::target_shard_count;

/// 수집 리더 리스를 잡고 유지한다.
///
/// **상태를 한 곳에 모으는 것이 요점이다.** "리더인가" 를 여러 곳에서 각자 판단하면
/// 한쪽만 갱신 실패를 눈치채고 다른 쪽은 계속 수집한다.
pub struct LeaderGate<L: LeaseStore, C: Clock> {
    lease_store: Arc<L>,
    clock: C,
    worker_id: String,
    /// 현재 보유한 리스. `None` 이면 **리더가 아니다** → 수집하지 않는다.
    held: Option<Lease>,
    /// 마지막 갱신 시각. 갱신 주기를 지키는 데 쓴다.
    last_renew_ms: EpochMs,
}

impl<L: LeaseStore, C: Clock> LeaderGate<L, C> {
    pub fn new(lease_store: Arc<L>, clock: C, worker_id: impl Into<String>) -> Self {
        Self {
            lease_store,
            clock,
            worker_id: worker_id.into(),
            held: None,
            last_renew_ms: 0,
        }
    }

    /// 이 워커가 지금 수집 리더인가.
    pub fn is_leader(&self) -> bool {
        self.held.is_some()
    }

    /// 소유해야 하는 샤드 수 (F1). 리더가 아니면 0 이다.
    pub fn shards_owned(&self) -> u32 {
        target_shard_count(self.is_leader())
    }

    /// 현재 리스의 `epoch`. 저장 레코드의 `owner_epoch` 로 쓴다(펜싱 근거).
    pub fn epoch(&self) -> Option<u64> {
        self.held.as_ref().map(|l| l.epoch)
    }

    /// 리스를 잡거나 갱신한다. **매 tick 전에 부른다.**
    ///
    /// 갱신에 실패하면 `held` 를 비워 즉시 수집을 멈춘다 — 다른 워커가 이미 리더일 수
    /// 있으므로 계속 수집하면 중복이다.
    pub async fn refresh(&mut self) {
        let now = self.clock.now_ms();

        if let Some(lease) = self.held.clone() {
            // 갱신 주기가 아니면 그대로 둔다.
            if now - self.last_renew_ms < LEASE_RENEW_INTERVAL_MS {
                return;
            }
            match self.lease_store.renew(&lease, now).await {
                Ok(Some(renewed)) => {
                    self.held = Some(renewed);
                    self.last_renew_ms = now;
                }
                Ok(None) => {
                    // **빼앗겼다.** 즉시 멈춘다.
                    tracing::warn!(
                        worker = %self.worker_id,
                        epoch = lease.epoch,
                        "수집 리더 리스를 잃었다 — 수집을 멈춘다"
                    );
                    self.held = None;
                }
                Err(e) => {
                    // 저장소 오류로는 리스를 포기하지 않는다 — TTL 이 만료를 처리한다.
                    // 여기서 포기하면 일시적 오류가 수집 공백을 만든다.
                    tracing::warn!(error = %e, "리스 갱신 실패 — TTL 까지 유지한다");
                }
            }
            return;
        }

        // 리더가 아니다. 잡아 본다.
        match self
            .lease_store
            .try_acquire(COLLECT_LEADER_KEY, &self.worker_id, now)
            .await
        {
            Ok(Some(lease)) => {
                tracing::info!(
                    worker = %self.worker_id,
                    epoch = lease.epoch,
                    shards = target_shard_count(true),
                    "수집 리더가 됐다"
                );
                self.last_renew_ms = now;
                self.held = Some(lease);
            }
            // 다른 워커가 리더다. 정상이다 — standby 로 대기한다.
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "리스 획득 실패"),
        }
    }

    /// 그레이스풀 셧다운 — 리스를 명시적으로 반납해 재분배를 앞당긴다.
    ///
    /// 반납하지 않으면 다음 리더가 TTL(60초)을 기다린다.
    pub async fn release(&mut self) {
        if let Some(lease) = self.held.take() {
            match self.lease_store.release(&lease).await {
                Ok(()) => tracing::info!(worker = %self.worker_id, "수집 리더 리스 반납"),
                Err(e) => tracing::warn!(error = %e, "리스 반납 실패 — TTL 로 만료된다"),
            }
        }
    }
}

/// 수집 루프의 한 tick 주기를 계산한다.
pub fn tick_interval(detect_interval_ms: u64) -> Duration {
    Duration::from_millis(detect_interval_ms.max(100))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::fakes::{FakeClock, FakeLeaseStore};

    fn gate(
        store: Arc<FakeLeaseStore>,
        clock: FakeClock,
        id: &str,
    ) -> LeaderGate<FakeLeaseStore, FakeClock> {
        LeaderGate::new(store, clock, id)
    }

    /// **F1 — 두 워커 중 하나만 수집한다.**
    ///
    /// 둘 다 수집하면 다이제스트 누산기가 last-writer-wins 로 조용히 손상된다.
    #[tokio::test]
    async fn only_one_of_two_workers_becomes_leader() {
        let store = Arc::new(FakeLeaseStore::default());
        let clock = FakeClock::new(1_000);
        let mut a = gate(store.clone(), clock.clone(), "worker-a");
        let mut b = gate(store.clone(), clock.clone(), "worker-b");

        a.refresh().await;
        b.refresh().await;

        assert!(a.is_leader() ^ b.is_leader(), "정확히 하나만 리더여야 한다");
        // **불변식**: 합계가 SHARD_COUNT 다.
        assert_eq!(
            a.shards_owned() + b.shards_owned(),
            dbmon_core::ports::SHARD_COUNT
        );
    }

    /// 비리더는 **0 샤드**를 소유한다 — 그게 중복 수집을 막는 유일한 장치다.
    #[tokio::test]
    async fn non_leader_owns_zero_shards() {
        let store = Arc::new(FakeLeaseStore::default());
        let clock = FakeClock::new(1_000);
        let mut a = gate(store.clone(), clock.clone(), "worker-a");
        let mut b = gate(store, clock, "worker-b");
        a.refresh().await;
        b.refresh().await;

        let follower = if a.is_leader() { &b } else { &a };
        assert!(!follower.is_leader());
        assert_eq!(follower.shards_owned(), 0);
        assert_eq!(follower.epoch(), None, "비리더는 epoch 가 없다");
    }

    /// **리스를 잃으면 즉시 멈춘다.** 계속 수집하면 새 리더와 중복된다.
    #[tokio::test]
    async fn losing_the_lease_stops_collection_immediately() {
        let store = Arc::new(FakeLeaseStore::default());
        let clock = FakeClock::new(1_000);
        let mut a = gate(store.clone(), clock.clone(), "worker-a");
        a.refresh().await;
        assert!(a.is_leader());

        // 다른 워커가 만료 후 가져간다.
        clock.advance(dbmon_core::ports::LEASE_TTL_MS + 1);
        let mut b = gate(store.clone(), clock.clone(), "worker-b");
        b.refresh().await;
        assert!(b.is_leader(), "만료됐으므로 B 가 잡아야 한다");

        // A 가 갱신을 시도하면 실패하고 즉시 멈춰야 한다.
        a.refresh().await;
        assert!(
            !a.is_leader(),
            "빼앗긴 뒤에도 리더라고 믿는다 — 중복 수집이 된다"
        );
        assert_eq!(a.shards_owned(), 0);
    }

    /// 갱신 주기 안에서는 저장소를 다시 부르지 않는다 — 매 tick 왕복은 낭비다.
    #[tokio::test]
    async fn renew_respects_the_interval() {
        let store = Arc::new(FakeLeaseStore::default());
        let clock = FakeClock::new(1_000);
        let mut a = gate(store.clone(), clock.clone(), "worker-a");
        a.refresh().await;
        let epoch = a.epoch();

        // 주기 미달 — 갱신하지 않는다.
        clock.advance(LEASE_RENEW_INTERVAL_MS - 1);
        a.refresh().await;
        assert_eq!(
            a.epoch(),
            epoch,
            "epoch 가 바뀌면 안 된다(갱신은 올리지 않는다)"
        );
        assert!(a.is_leader());

        // 주기 도달 — 갱신한다. 여전히 리더다.
        clock.advance(2);
        a.refresh().await;
        assert!(a.is_leader());
    }

    /// 반납하면 다른 워커가 **TTL 을 기다리지 않고** 리더가 된다.
    #[tokio::test]
    async fn release_lets_the_next_worker_take_over_immediately() {
        let store = Arc::new(FakeLeaseStore::default());
        let clock = FakeClock::new(1_000);
        let mut a = gate(store.clone(), clock.clone(), "worker-a");
        a.refresh().await;
        assert!(a.is_leader());

        a.release().await;
        assert!(!a.is_leader());

        // TTL 을 기다리지 않는다.
        let mut b = gate(store, clock, "worker-b");
        b.refresh().await;
        assert!(
            b.is_leader(),
            "반납 후에도 TTL 을 기다려야 했다 — 수집 공백이 길어진다"
        );
    }

    #[test]
    fn tick_interval_has_a_floor() {
        assert_eq!(tick_interval(1_000), Duration::from_millis(1_000));
        // 0 을 주면 핫 루프가 된다. 설정 검증이 막지만 여기도 하한을 둔다.
        assert_eq!(tick_interval(0), Duration::from_millis(100));
    }
}
