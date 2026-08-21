//! 화면에서 조작하는 수집 제어.
//!
//! # 무엇을 위한 것인가
//!
//! 참조 구현(`my_slow_query_scraper`)은 `POST /mysql/start|stop` 으로 수집 루프를
//! 켜고 껐고, `POST /collectors/rds-instances` 로 탐색을 즉시 돌렸다. 그 화면을
//! 쓰던 사람은 **"지금 멈춰" 와 "지금 훑어"** 를 버튼으로 한다.
//!
//! # 왜 리스를 놓지 않는가
//!
//! 이 수집기는 리더 선출로 돈다. "멈춤" 을 리스 반납으로 구현하면 **다른 워커가
//! 즉시 리더가 되어 수집을 계속한다** — 멈춘 게 아니다. 그래서 리스는 그대로 쥐고
//! 수집 태스크만 멈춘다.
//!
//! # 두 종류의 제어가 있다
//!
//! | 종류 | 어디 사는가 | 왜 |
//! |---|---|---|
//! | **정지 스코프** (전체·환경·인스턴스) | 저장소 ([`PauseState`]) | 사람의 의도다. 재시작·리더 교체에도 남아야 한다 |
//! | **"지금 훑어"·"지금 백필"** | 프로세스 원자값 ([`Controls`]) | 리더에게 한 번 찌르는 것이다. 남을 이유가 없다 |
//!
//! 정지가 프로세스 원자값이었을 때는 재배포에 풀렸고, 리더가 바뀌면 플래그가 없는
//! 워커가 수집을 이어갔다 — **화면에는 "멈춤" 인데 기록은 계속 쌓인다.** 그래서
//! 정지만 저장소로 옮겼다. 반대로 즉시 실행 요청을 저장소에 두면 "한 번 소비" 를
//! 위해 조건부 쓰기가 필요해지는데, 그 요청은 리더에게만 의미가 있어 이득이 없다.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dbmon_core::pause::{PauseScope, PauseSet};
use dbmon_core::ports::PauseStore;
use dbmon_core::time::EpochMs;

/// 즉시 실행 요청과 런타임 사실. API 와 리더 루프가 `Arc` 로 공유한다.
#[derive(Debug, Default)]
pub struct Controls {
    /// 다음 tick 에 탐색을 강제한다. **한 번 소비되면 내려간다.**
    discover_now: AtomicBool,
    /// 다음 tick 에 슬로우로그 백필을 강제한다.
    backfill_now: AtomicBool,

    // ── 루프가 게시하는 사실 (화면 표시용) ───────────────────────────────────
    is_leader: AtomicBool,
    collecting: AtomicUsize,
    last_tick_ms: AtomicI64,
    last_discovery_ms: AtomicI64,
    last_backfill_ms: AtomicI64,
}

impl Controls {
    pub fn new() -> Self {
        Self::default()
    }

    // ── 조작 ────────────────────────────────────────────────────────────────

    pub fn request_discovery(&self) {
        self.discover_now.store(true, Ordering::SeqCst);
    }

    pub fn request_backfill(&self) {
        self.backfill_now.store(true, Ordering::SeqCst);
    }

    /// 요청이 있었으면 `true` 를 돌려주고 **내린다.** 두 번 돌지 않는다.
    pub fn take_discovery_request(&self) -> bool {
        self.discover_now.swap(false, Ordering::SeqCst)
    }

    pub fn take_backfill_request(&self) -> bool {
        self.backfill_now.swap(false, Ordering::SeqCst)
    }

    // ── 루프가 게시하는 사실 ─────────────────────────────────────────────────

    pub fn publish_tick(&self, now_ms: i64, is_leader: bool, collecting: usize) {
        self.last_tick_ms.store(now_ms, Ordering::Relaxed);
        self.is_leader.store(is_leader, Ordering::Relaxed);
        self.collecting.store(collecting, Ordering::Relaxed);
    }

    pub fn publish_discovery(&self, now_ms: i64) {
        self.last_discovery_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn publish_backfill(&self, now_ms: i64) {
        self.last_backfill_ms.store(now_ms, Ordering::Relaxed);
    }

    /// 화면에 그대로 내보내는 스냅샷. **`0` 은 "아직 없다" 이므로 `None` 으로 바꾼다.**
    pub fn snapshot(&self) -> ControlSnapshot {
        let opt = |v: i64| if v == 0 { None } else { Some(v) };
        ControlSnapshot {
            is_leader: self.is_leader.load(Ordering::Relaxed),
            collecting: self.collecting.load(Ordering::Relaxed),
            last_tick_ms: opt(self.last_tick_ms.load(Ordering::Relaxed)),
            last_discovery_ms: opt(self.last_discovery_ms.load(Ordering::Relaxed)),
            last_backfill_ms: opt(self.last_backfill_ms.load(Ordering::Relaxed)),
            discovery_requested: self.discover_now.load(Ordering::SeqCst),
            backfill_requested: self.backfill_now.load(Ordering::SeqCst),
        }
    }
}

/// 수집기 상태. 참조 대시보드의 `Status: running` 카드에 대응한다.
///
/// ⚠ **정지 여부는 여기 없다.** 그건 프로세스가 아니라 저장소의 사실이므로
/// API 응답이 [`PauseState`] 에서 읽어 함께 내보낸다.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ControlSnapshot {
    /// 이 프로세스가 수집 리더인가. **아니면 멈추고 있는 것이 정상이다.**
    pub is_leader: bool,
    /// 지금 수집 중인 인스턴스 수.
    pub collecting: usize,
    pub last_tick_ms: Option<i64>,
    pub last_discovery_ms: Option<i64>,
    pub last_backfill_ms: Option<i64>,
    /// 눌렀지만 아직 소비되지 않은 요청. 버튼이 "대기 중" 을 보여줄 수 있다.
    pub discovery_requested: bool,
    pub backfill_requested: bool,
}

/// 정지 스코프 집합의 **공유 캐시**.
///
/// # 왜 캐시인가
///
/// 리더 루프는 1초마다 돈다(`detect_interval_ms`). 매 tick 저장소를 읽으면 조회
/// 비용은 사소하지만 **tick 안에 네트워크 왕복이 하나 늘어난다** — 이 루프가 늦으면
/// `gate.refresh()` 가 늦고, 리스 TTL(60초)을 놓치면 리더가 바뀐다. 그래서 짧게
/// 캐시한다. 정지 반영이 최대 [`CACHE_TTL_MS`] 늦지만, 사람이 버튼을 누르는 일에
/// 5초는 보이지 않는다.
///
/// # 왜 실패하면 마지막 값을 쓰는가
///
/// 조회 실패를 "아무것도 안 멈춰 있다" 로 접으면 **저장소가 흔들릴 때 멈춰 둔 prd 가
/// 다시 수집된다.** 사람이 관측을 멈춘 이유(유지보수·부하)는 그때도 유효하다.
/// 그래서 마지막으로 성공한 집합을 유지하고 경고만 남긴다.
pub struct PauseState {
    store: Arc<dyn PauseStore>,
    cache: Mutex<Cached>,
}

/// 캐시 유효 기간. 사람 손으로 누르는 조작에 대해 보이지 않는 지연이다.
pub const CACHE_TTL_MS: i64 = 5_000;

#[derive(Default)]
struct Cached {
    set: PauseSet,
    /// 마지막으로 **성공한** 조회 시각. `None` 이면 아직 한 번도 못 읽었다.
    loaded_ms: Option<EpochMs>,
}

impl PauseState {
    pub fn new(store: Arc<dyn PauseStore>) -> Self {
        Self {
            store,
            cache: Mutex::new(Cached::default()),
        }
    }

    /// 캐시가 유효하면 그대로, 아니면 저장소에서 읽는다.
    pub async fn load(&self, now_ms: EpochMs) -> PauseSet {
        if let Some(fresh) = self.fresh(now_ms) {
            return fresh;
        }
        self.refresh(now_ms).await
    }

    /// 캐시를 무시하고 읽는다. **쓰기 직후**에 부른다 — 같은 요청의 응답이
    /// 방금 누른 결과를 말해야 한다.
    pub async fn refresh(&self, now_ms: EpochMs) -> PauseSet {
        match self.store.list().await {
            Ok(set) => {
                let mut c = self.cache.lock().expect("pause cache");
                c.set = set.clone();
                c.loaded_ms = Some(now_ms);
                set
            }
            Err(e) => {
                // **마지막 값을 유지한다.** 빈 집합으로 접으면 멈춰 둔 수집이 재개된다.
                let last = self.cached();
                tracing::warn!(
                    error = %crate::telemetry::Scrubbed(&e),
                    paused_scopes = last.len(),
                    "정지 스코프를 읽을 수 없다 — 마지막으로 읽은 집합을 유지한다"
                );
                last
            }
        }
    }

    /// 지금 들고 있는 값. **I/O 를 하지 않는다.**
    pub fn cached(&self) -> PauseSet {
        self.cache.lock().expect("pause cache").set.clone()
    }

    fn fresh(&self, now_ms: EpochMs) -> Option<PauseSet> {
        let c = self.cache.lock().expect("pause cache");
        let loaded = c.loaded_ms?;
        (now_ms - loaded < CACHE_TTL_MS).then(|| c.set.clone())
    }

    pub async fn pause(
        &self,
        scope: &PauseScope,
        by: &str,
        now_ms: EpochMs,
    ) -> dbmon_core::Result<PauseSet> {
        self.store.pause(scope, by, now_ms).await?;
        Ok(self.refresh(now_ms).await)
    }

    pub async fn resume(
        &self,
        scope: &PauseScope,
        now_ms: EpochMs,
    ) -> dbmon_core::Result<PauseSet> {
        self.store.resume(scope).await?;
        Ok(self.refresh(now_ms).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::Env;
    use dbmon_core::fakes::FakePauseStore;

    fn state() -> (Arc<FakePauseStore>, PauseState) {
        let store = Arc::new(FakePauseStore::new());
        (Arc::clone(&store), PauseState::new(store))
    }

    /// **캐시가 유효한 동안에는 저장소를 다시 읽지 않는다.** 리더 루프가 1초마다
    /// 부르므로 여기가 새면 tick 마다 왕복이 하나 붙는다.
    #[tokio::test]
    async fn a_fresh_cache_does_not_hit_the_store() {
        let (store, state) = state();
        state.load(1_000).await;
        assert_eq!(store.list_calls(), 1);

        state.load(1_000 + CACHE_TTL_MS - 1).await;
        assert_eq!(store.list_calls(), 1, "캐시가 유효하면 읽지 않는다");

        state.load(1_000 + CACHE_TTL_MS).await;
        assert_eq!(store.list_calls(), 2, "만료되면 다시 읽는다");
    }

    /// **저장소가 흔들려도 멈춰 둔 것은 멈춰 있어야 한다.**
    ///
    /// 빈 집합으로 접으면 사람이 멈춰 둔 prd 가 조용히 다시 수집된다 — 관측을 멈춘
    /// 이유는 저장소 장애와 무관하게 유효하다.
    #[tokio::test]
    async fn a_store_failure_keeps_the_last_known_set() {
        let (store, state) = state();
        state
            .pause(&PauseScope::Env(Env::Prd), "tester", 1_000)
            .await
            .expect("정지");
        assert!(
            state.cached().is_paused_id(
                &dbmon_core::InstanceId::new("123456789012", "ap-northeast-2", "orders-prd-01")
                    .expect("id"),
                Env::Prd
            )
        );

        store.set_failing(true);
        let after = state.load(1_000 + CACHE_TTL_MS).await;
        assert!(
            after.contains(&PauseScope::Env(Env::Prd)),
            "조회 실패가 정지를 풀어서는 안 된다"
        );
    }

    /// 같은 스코프를 다시 눌러도 **시작 시각을 덮지 않는다** — 화면이 "3분 전부터
    /// 멈춤" 을 말할 수 있어야 한다. (저장소가 `if_not_exists` 로 지키는 계약이다.)
    #[tokio::test]
    async fn pausing_twice_keeps_the_first_timestamp() {
        let (_store, state) = state();
        let scope = PauseScope::Env(Env::Dev);
        state.pause(&scope, "a", 1_000).await.expect("정지");
        let set = state.pause(&scope, "b", 9_000).await.expect("재정지");
        assert_eq!(set.since_ms(&scope), Some(1_000));

        state.resume(&scope, 9_500).await.expect("재개");
        assert!(state.cached().is_empty());
        // 재개 후 다시 멈추면 그때가 시작이다.
        let set = state.pause(&scope, "a", 10_000).await.expect("정지");
        assert_eq!(set.since_ms(&scope), Some(10_000));
    }

    /// **요청은 한 번만 소비된다.** 안 그러면 매 tick 탐색이 돌아 핫 루프가 된다.
    #[test]
    fn a_request_is_consumed_once() {
        let c = Controls::new();
        assert!(!c.take_discovery_request());

        c.request_discovery();
        assert!(c.snapshot().discovery_requested);
        assert!(c.take_discovery_request());
        assert!(!c.take_discovery_request());
        assert!(!c.snapshot().discovery_requested);

        c.request_backfill();
        assert!(c.take_backfill_request());
        assert!(!c.take_backfill_request());
    }

    /// `0` 은 유효한 epoch 이지만 **"아직 없다" 로 쓰지 않는다** — 이 프로젝트가
    /// 같은 실수를 이미 두 번 했다(`last_sampled_at_ms`, 합계 타일).
    #[test]
    fn unset_timestamps_are_none_not_zero() {
        let c = Controls::new();
        let s = c.snapshot();
        assert_eq!(s.last_tick_ms, None);
        assert_eq!(s.last_discovery_ms, None);
        assert_eq!(s.last_backfill_ms, None);

        c.publish_tick(1_700_000_000_000, true, 3);
        c.publish_discovery(1_700_000_000_001);
        c.publish_backfill(1_700_000_000_002);
        let s = c.snapshot();
        assert_eq!(s.last_tick_ms, Some(1_700_000_000_000));
        assert_eq!(s.last_discovery_ms, Some(1_700_000_000_001));
        assert_eq!(s.last_backfill_ms, Some(1_700_000_000_002));
        assert!(s.is_leader);
        assert_eq!(s.collecting, 3);
    }
}
