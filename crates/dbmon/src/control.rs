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
//! # 프로세스 범위다
//!
//! 이 플래그는 **요청을 받은 프로세스에만** 적용된다(원자값이므로 재시작하면
//! 초기화된다). 여러 워커를 띄운 배포에서 전체를 멈추려면 설정 저장소에 상태를
//! 둬야 하고, 그건 이 구조보다 크다. 그래서 상태 응답이 `worker_id` 와
//! `scope: "process"` 를 함께 말한다 — 어느 워커를 멈췄는지 화면이 알 수 있어야 한다.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

/// 수집 제어 플래그와 런타임 사실. API 와 리더 루프가 `Arc` 로 공유한다.
#[derive(Debug, Default)]
pub struct Controls {
    paused: AtomicBool,
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
    paused_since_ms: AtomicI64,
}

impl Controls {
    pub fn new() -> Self {
        Self::default()
    }

    // ── 조작 ────────────────────────────────────────────────────────────────

    /// 수집을 멈춘다/재개한다. 이미 그 상태면 시각을 덮지 않는다.
    pub fn set_paused(&self, paused: bool, now_ms: i64) {
        let was = self.paused.swap(paused, Ordering::SeqCst);
        if was != paused {
            self.paused_since_ms
                .store(if paused { now_ms } else { 0 }, Ordering::SeqCst);
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

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
            paused: self.is_paused(),
            paused_since_ms: opt(self.paused_since_ms.load(Ordering::SeqCst)),
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ControlSnapshot {
    pub paused: bool,
    pub paused_since_ms: Option<i64>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_records_when_it_started_and_resume_clears_it() {
        let c = Controls::new();
        assert!(!c.is_paused());
        assert_eq!(c.snapshot().paused_since_ms, None);

        c.set_paused(true, 1_000);
        assert!(c.is_paused());
        assert_eq!(c.snapshot().paused_since_ms, Some(1_000));

        // 같은 상태를 다시 눌러도 시작 시각을 덮지 않는다 — 화면이 "3분 전부터
        // 멈춤" 을 말할 수 있어야 한다.
        c.set_paused(true, 9_000);
        assert_eq!(c.snapshot().paused_since_ms, Some(1_000));

        c.set_paused(false, 9_000);
        assert!(!c.is_paused());
        assert_eq!(c.snapshot().paused_since_ms, None);
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
