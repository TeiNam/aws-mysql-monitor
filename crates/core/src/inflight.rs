//! in-flight 상태 머신 ([05 §4](../../../docs/05-collector.md), R4·R5).
//!
//! ```text
//!                  detect 에서 관측됨 (TIME >= threshold)
//!                              │
//!                              ▼
//!                     ┌─────────────────┐
//!        ┌───────────►│    OBSERVED     │  첫 관측: 심층 조회 3건 발행
//!        │            └────────┬────────┘
//!        │  다음 tick 에도       │
//!        │  같은 실행           ▼
//!        │                ┌─────────────────┐
//!        └────────────────│    TRACKING     │  max_time 갱신, 플랜 재시도
//!                         └────────┬────────┘
//!                                  │ tick 에서 사라짐
//!                                  ▼
//!                              FINALIZING → 저장
//! ```
//!
//! # 스레드 ID 재사용이 이 모듈의 존재 이유다 (R5)
//!
//! MySQL 은 연결이 끊기면 `PROCESSLIST_ID` 를 **재사용한다.** 1세대는 `pid` 만으로
//! 캐시를 관리하고 `pid` 유니크 인덱스까지 걸어서, 재사용 시 두 개의 다른 실행이
//! 하나로 합쳐졌다.
//!
//! 같은 실행으로 판정하는 조건은 **전부** 만족해야 한다:
//!
//! | # | 조건 | 깨지면 |
//! |---|---|---|
//! | 1 | `thread_id` 동일 | 다른 세션이다 |
//! | 2 | `mysql_digest` 동일 (없으면 `app_digest`) | 같은 세션의 다음 쿼리다 |
//! | 3 | 관측된 `TIME` 이 감소하지 않았다 | 새 쿼리가 시작됐다 |
//! | 4 | `db_user` + `db_host` 동일 | 연결이 재사용됐다 |
//!
//! 하나라도 깨지면 기존 항목을 **즉시 확정**하고 새 항목을 시작한다.

use crate::clock_offset::ClockOffset;
use crate::time::EpochMs;
use std::collections::BTreeMap;

/// 관측된 실행의 신원. 조건 2·4 를 담는다.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    /// PS 가 준 다이제스트. 없으면 `app_digest` 로 대체한다.
    pub digest: Option<String>,
    pub db_user: Option<String>,
    pub db_host: Option<String>,
}

impl Identity {
    /// 같은 실행일 수 있는가 (조건 2·4).
    ///
    /// **다이제스트가 양쪽에 없으면 "같다"고 본다.** 첫 tick 에는 심층 조회가 아직
    /// 끝나지 않아 다이제스트가 없다. 그때 다르다고 판정하면 매 tick 마다 새 항목이
    /// 생겨 하나의 느린 쿼리가 수십 건으로 기록된다.
    fn matches(&self, other: &Identity) -> bool {
        let digest_ok = match (&self.digest, &other.digest) {
            (Some(a), Some(b)) => a == b,
            // 한쪽만 있으면 뒤늦게 채워진 것이다 → 같은 실행으로 본다.
            _ => true,
        };
        digest_ok && self.db_user == other.db_user && self.db_host == other.db_host
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedState {
    /// 첫 관측. 심층 조회를 발행해야 한다.
    Observed,
    /// 계속 관측 중.
    Tracking,
}

/// 추적 중인 하나의 실행.
#[derive(Debug, Clone, PartialEq)]
pub struct Tracked {
    pub thread_id: u64,
    pub identity: Identity,
    pub schema_name: Option<String>,
    pub state: TrackedState,
    /// 우리 시계 기준 최초 관측 시각.
    pub first_observed_at_ms: EpochMs,
    /// **DB 시계 기준** 시작 시각 추정. 파티션·병합이 이 값을 쓴다.
    pub started_at_ms: EpochMs,
    /// `TIMER_WAIT` 로 보정한 정밀 시각. 있으면 표시·정렬에 쓴다.
    pub started_at_ms_precise: Option<EpochMs>,
    /// 관측된 **최대** `TIME`(초). 보수적으로 최대값을 쓴다.
    pub max_time_secs: i64,
    /// 마지막으로 관측된 시각(우리 시계). 고아 판정 기준 (F4).
    pub last_seen_at_ms: EpochMs,
    /// 플랜 수집 시도 횟수. 상한을 넘으면 포기한다.
    pub plan_attempts: u8,
    /// 플랜을 확보했다.
    pub has_plan: bool,
}

impl Tracked {
    /// 관측된 지속시간(ms). `TIMER_WAIT` 가 있으면 그걸 쓴다.
    pub fn duration_ms(&self) -> i64 {
        self.max_time_secs * 1000
    }
}

/// 확정 사유.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeReason {
    /// tick 에서 사라졌다 — 정상 종료.
    Disappeared,
    /// 같은 `thread_id` 에서 다른 실행이 시작됐다 (R5).
    ThreadReused,
    /// 최대 추적 시간을 넘겼다.
    TooLong,
}

impl FinalizeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disappeared => "disappeared",
            Self::ThreadReused => "thread_reused",
            Self::TooLong => "too_long",
        }
    }
    /// 실제로 종료를 관측했는가. `false` 면 `duration_ms` 가 하한이다.
    pub fn observed_end(self) -> bool {
        self == Self::Disappeared
    }
}

/// 한 tick 의 결과.
#[derive(Debug, Default, PartialEq)]
pub struct TickResult {
    /// 심층 조회(전문 SQL·정확 지표·플랜)를 발행할 대상.
    pub needs_deep_probe: Vec<u64>,
    /// 확정해 저장할 항목.
    pub finalized: Vec<(Tracked, FinalizeReason)>,
}

/// 탐지 루프에 들어오는 한 행. 포트의 `ProcessRow` 를 도메인 형태로 좁힌 것.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub thread_id: u64,
    pub time_secs: i64,
    pub identity: Identity,
    pub schema_name: Option<String>,
}

/// 인스턴스 하나의 in-flight 캐시. **워커 메모리에만 있다.**
#[derive(Debug)]
pub struct InFlightTracker {
    entries: BTreeMap<u64, Tracked>,
    max_tracking_ms: i64,
    max_plan_attempts: u8,
}

/// 기본 최대 추적 시간. 이걸 넘기면 강제 확정한다.
pub const DEFAULT_MAX_TRACKING_MS: i64 = 3_600_000;
/// 플랜 수집 재시도 상한.
pub const DEFAULT_MAX_PLAN_ATTEMPTS: u8 = 3;

impl Default for InFlightTracker {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_TRACKING_MS, DEFAULT_MAX_PLAN_ATTEMPTS)
    }
}

impl InFlightTracker {
    pub fn new(max_tracking_ms: i64, max_plan_attempts: u8) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_tracking_ms,
            max_plan_attempts,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn get(&self, thread_id: u64) -> Option<&Tracked> {
        self.entries.get(&thread_id)
    }
    pub fn iter(&self) -> impl Iterator<Item = &Tracked> {
        self.entries.values()
    }

    /// 한 tick 을 처리한다.
    ///
    /// `now_ms` 는 우리 시계, `offset` 은 DB 와의 차이. 시작 시각은 **DB 시계**로 계산한다.
    pub fn tick(
        &mut self,
        observations: &[Observation],
        now_ms: EpochMs,
        offset: &ClockOffset,
    ) -> TickResult {
        let mut result = TickResult::default();
        let mut seen: Vec<u64> = Vec::with_capacity(observations.len());

        for obs in observations {
            seen.push(obs.thread_id);
            match self.entries.get_mut(&obs.thread_id) {
                Some(existing) => {
                    // 조건 2·3·4 를 확인한다. 하나라도 깨지면 다른 실행이다.
                    let same_identity = existing.identity.matches(&obs.identity);
                    let time_not_decreased = obs.time_secs >= existing.max_time_secs;
                    if same_identity && time_not_decreased {
                        existing.state = TrackedState::Tracking;
                        existing.max_time_secs = existing.max_time_secs.max(obs.time_secs);
                        existing.last_seen_at_ms = now_ms;
                        // 다이제스트가 뒤늦게 채워지면 반영한다.
                        if existing.identity.digest.is_none() {
                            existing.identity.digest = obs.identity.digest.clone();
                        }
                        // 플랜이 없으면 재시도 대상이다.
                        if !existing.has_plan && existing.plan_attempts < self.max_plan_attempts {
                            result.needs_deep_probe.push(obs.thread_id);
                        }
                    } else {
                        // **스레드 재사용** — 이전 실행을 즉시 확정하고 새로 시작한다.
                        let old = self
                            .entries
                            .remove(&obs.thread_id)
                            .expect("직전에 존재했다");
                        result.finalized.push((old, FinalizeReason::ThreadReused));
                        self.entries
                            .insert(obs.thread_id, new_tracked(obs, now_ms, offset));
                        result.needs_deep_probe.push(obs.thread_id);
                    }
                }
                None => {
                    self.entries
                        .insert(obs.thread_id, new_tracked(obs, now_ms, offset));
                    result.needs_deep_probe.push(obs.thread_id);
                }
            }
        }

        // 사라진 항목을 확정한다.
        let disappeared: Vec<u64> = self
            .entries
            .keys()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();
        for id in disappeared {
            if let Some(t) = self.entries.remove(&id) {
                result.finalized.push((t, FinalizeReason::Disappeared));
            }
        }

        // 너무 오래 추적한 항목을 강제 확정한다. **제거하지 않으면** 캐시가 무한히 자란다.
        let too_long: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, t)| now_ms - t.first_observed_at_ms > self.max_tracking_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in too_long {
            if let Some(t) = self.entries.remove(&id) {
                result.finalized.push((t, FinalizeReason::TooLong));
            }
        }

        result
    }

    /// 심층 조회 결과를 반영한다.
    pub fn record_deep_probe(
        &mut self,
        thread_id: u64,
        digest: Option<String>,
        timer_wait_ps: Option<u64>,
        observed_at_ms: EpochMs,
        offset: &ClockOffset,
    ) {
        let Some(t) = self.entries.get_mut(&thread_id) else {
            return;
        };
        if t.identity.digest.is_none() {
            t.identity.digest = digest;
        }
        // `TIMER_WAIT` 는 피코초. `TIME` 의 초 단위 오차(최대 1초)를 없앤다.
        if let Some(ps) = timer_wait_ps {
            let elapsed_ms = (ps / 1_000_000_000) as i64;
            t.started_at_ms_precise = Some(offset.to_db_time(observed_at_ms) - elapsed_ms);
        }
    }

    pub fn record_plan_attempt(&mut self, thread_id: u64, succeeded: bool) {
        if let Some(t) = self.entries.get_mut(&thread_id) {
            t.plan_attempts = t.plan_attempts.saturating_add(1);
            t.has_plan |= succeeded;
        }
    }

    /// 그레이스풀 셧다운·리더 상실 시 전부 확정한다.
    pub fn drain(&mut self) -> Vec<(Tracked, FinalizeReason)> {
        std::mem::take(&mut self.entries)
            .into_values()
            .map(|t| (t, FinalizeReason::TooLong))
            .collect()
    }
}

fn new_tracked(obs: &Observation, now_ms: EpochMs, offset: &ClockOffset) -> Tracked {
    // **DB 시계로 환산한 뒤** TIME 을 뺀다. 우리 시계로 계산하면 오프셋만큼 어긋난다 (F14).
    let started_at_ms = offset.to_db_time(now_ms) - obs.time_secs * 1000;
    Tracked {
        thread_id: obs.thread_id,
        identity: obs.identity.clone(),
        schema_name: obs.schema_name.clone(),
        state: TrackedState::Observed,
        first_observed_at_ms: now_ms,
        started_at_ms,
        started_at_ms_precise: None,
        max_time_secs: obs.time_secs,
        last_seen_at_ms: now_ms,
        plan_attempts: 0,
        has_plan: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(thread_id: u64, time_secs: i64, digest: Option<&str>) -> Observation {
        Observation {
            thread_id,
            time_secs,
            identity: Identity {
                digest: digest.map(str::to_string),
                db_user: Some("app".into()),
                db_host: Some("10.0.3.44".into()),
            },
            schema_name: Some("shop".into()),
        }
    }

    fn no_offset() -> ClockOffset {
        ClockOffset::restored(0)
    }

    #[test]
    fn first_observation_requests_deep_probe() {
        let mut t = InFlightTracker::default();
        let r = t.tick(&[obs(100, 2, None)], 10_000, &no_offset());
        assert_eq!(r.needs_deep_probe, vec![100]);
        assert!(r.finalized.is_empty());
        assert_eq!(t.get(100).unwrap().state, TrackedState::Observed);
        // 시작 시각 = 관측 시각 − TIME
        assert_eq!(t.get(100).unwrap().started_at_ms, 8_000);
    }

    #[test]
    fn continued_observation_updates_max_time() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(100, 2, Some("d1"))], 10_000, &no_offset());
        t.record_plan_attempt(100, true);
        let r = t.tick(&[obs(100, 5, Some("d1"))], 13_000, &no_offset());
        assert!(
            r.needs_deep_probe.is_empty(),
            "플랜을 이미 얻었으면 재시도하지 않는다"
        );
        assert!(r.finalized.is_empty());
        let e = t.get(100).unwrap();
        assert_eq!(e.state, TrackedState::Tracking);
        assert_eq!(e.max_time_secs, 5);
        assert_eq!(e.duration_ms(), 5_000);
        // 시작 시각은 **첫 관측에서 정하고 바꾸지 않는다** — 매 tick 재계산하면 흔들린다.
        assert_eq!(e.started_at_ms, 8_000);
    }

    /// R4 — 4초 쿼리가 `duration_ms ≈ 4000` 이어야 한다.
    #[test]
    fn r4_duration_matches_observed_time() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(1, 2, Some("d"))], 0, &no_offset());
        t.tick(&[obs(1, 3, Some("d"))], 1_000, &no_offset());
        t.tick(&[obs(1, 4, Some("d"))], 2_000, &no_offset());
        let r = t.tick(&[], 3_000, &no_offset());
        assert_eq!(r.finalized.len(), 1);
        let (tracked, reason) = &r.finalized[0];
        assert_eq!(tracked.duration_ms(), 4_000);
        assert_eq!(*reason, FinalizeReason::Disappeared);
        assert!(reason.observed_end());
    }

    /// R5 — 같은 `thread_id` 로 다른 다이제스트가 나타나면 두 건이어야 한다.
    #[test]
    fn r5_thread_reuse_with_different_digest_splits() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(100, 3, Some("digest-A"))], 10_000, &no_offset());
        let r = t.tick(&[obs(100, 2, Some("digest-B"))], 13_000, &no_offset());

        assert_eq!(r.finalized.len(), 1, "이전 실행이 확정돼야 한다");
        assert_eq!(r.finalized[0].1, FinalizeReason::ThreadReused);
        assert_eq!(
            r.finalized[0].0.identity.digest.as_deref(),
            Some("digest-A")
        );
        assert_eq!(
            r.needs_deep_probe,
            vec![100],
            "새 실행은 심층 조회가 필요하다"
        );
        assert_eq!(
            t.get(100).unwrap().identity.digest.as_deref(),
            Some("digest-B")
        );
        assert_eq!(t.len(), 1, "같은 thread_id 는 하나만 추적한다");
    }

    /// R5 — `TIME` 이 감소하면 다이제스트가 같아도 새 쿼리다.
    #[test]
    fn r5_decreasing_time_splits_even_with_same_digest() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(100, 10, Some("d"))], 10_000, &no_offset());
        let r = t.tick(&[obs(100, 2, Some("d"))], 20_000, &no_offset());
        assert_eq!(r.finalized.len(), 1);
        assert_eq!(r.finalized[0].1, FinalizeReason::ThreadReused);
        assert_eq!(r.finalized[0].0.max_time_secs, 10);
        assert_eq!(t.get(100).unwrap().max_time_secs, 2);
    }

    /// R5 — 계정·호스트가 바뀌면 연결이 재사용된 것이다.
    #[test]
    fn r5_different_user_or_host_splits() {
        for (user, host) in [
            (Some("other"), Some("10.0.3.44")),
            (Some("app"), Some("10.0.9.9")),
        ] {
            let mut t = InFlightTracker::default();
            t.tick(&[obs(100, 3, Some("d"))], 10_000, &no_offset());
            let mut second = obs(100, 4, Some("d"));
            second.identity.db_user = user.map(str::to_string);
            second.identity.db_host = host.map(str::to_string);
            let r = t.tick(&[second], 11_000, &no_offset());
            assert_eq!(
                r.finalized.len(),
                1,
                "계정/호스트가 바뀌면 분리해야 한다: user={user:?} host={host:?}"
            );
        }
    }

    /// 첫 tick 에는 다이제스트가 없다. 그때 "다르다"고 판정하면 매 tick 새 항목이 생긴다.
    #[test]
    fn missing_digest_does_not_split() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(100, 2, None)], 10_000, &no_offset());
        let r = t.tick(&[obs(100, 3, Some("d1"))], 11_000, &no_offset());
        assert!(
            r.finalized.is_empty(),
            "뒤늦게 채워진 다이제스트로 분리하면 안 된다"
        );
        assert_eq!(t.get(100).unwrap().identity.digest.as_deref(), Some("d1"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn clock_offset_shifts_start_time_to_db_clock() {
        // DB 가 3초 앞서 있다. 시작 시각은 **DB 시계**여야 파티션이 맞는다 (F14).
        let offset = ClockOffset::restored(3_000);
        let mut t = InFlightTracker::default();
        t.tick(&[obs(1, 2, None)], 10_000, &offset);
        assert_eq!(t.get(1).unwrap().started_at_ms, 10_000 + 3_000 - 2_000);
        // 우리 시계 기준 최초 관측 시각은 그대로 보존한다 (고아 판정에 쓴다).
        assert_eq!(t.get(1).unwrap().first_observed_at_ms, 10_000);
    }

    #[test]
    fn timer_wait_refines_start_time() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(1, 4, None)], 10_000, &no_offset());
        // TIMER_WAIT = 4.213초 (피코초)
        t.record_deep_probe(
            1,
            Some("d".into()),
            Some(4_213_000_000_000),
            10_000,
            &no_offset(),
        );
        let e = t.get(1).unwrap();
        assert_eq!(e.started_at_ms, 6_000, "TIME 기반 추정은 그대로 둔다");
        assert_eq!(
            e.started_at_ms_precise,
            Some(5_787),
            "TIMER_WAIT 기반 정밀값"
        );
    }

    #[test]
    fn plan_attempts_are_capped() {
        let mut t = InFlightTracker::new(DEFAULT_MAX_TRACKING_MS, 2);
        t.tick(&[obs(1, 2, Some("d"))], 0, &no_offset());
        t.record_plan_attempt(1, false);
        let r = t.tick(&[obs(1, 3, Some("d"))], 1_000, &no_offset());
        assert_eq!(r.needs_deep_probe, vec![1], "1회 실패 후에는 재시도한다");
        t.record_plan_attempt(1, false);
        let r = t.tick(&[obs(1, 4, Some("d"))], 2_000, &no_offset());
        assert!(r.needs_deep_probe.is_empty(), "상한에 도달하면 포기한다");
        assert_eq!(t.get(1).unwrap().plan_attempts, 2);
    }

    #[test]
    fn too_long_tracking_is_force_finalized_and_removed() {
        let mut t = InFlightTracker::new(1_000, 3);
        t.tick(&[obs(1, 2, Some("d"))], 0, &no_offset());
        let r = t.tick(&[obs(1, 3, Some("d"))], 5_000, &no_offset());
        assert_eq!(r.finalized.len(), 1);
        assert_eq!(r.finalized[0].1, FinalizeReason::TooLong);
        assert!(!r.finalized[0].1.observed_end(), "종료를 관측하지 못했다");
        assert!(t.is_empty(), "제거하지 않으면 캐시가 무한히 자란다");
    }

    #[test]
    fn multiple_threads_are_independent() {
        let mut t = InFlightTracker::default();
        let r = t.tick(
            &[
                obs(1, 2, Some("a")),
                obs(2, 3, Some("b")),
                obs(3, 9, Some("c")),
            ],
            0,
            &no_offset(),
        );
        assert_eq!(r.needs_deep_probe, vec![1, 2, 3]);
        assert_eq!(t.len(), 3);

        // 2번만 사라진다.
        let r = t.tick(
            &[obs(1, 3, Some("a")), obs(3, 10, Some("c"))],
            1_000,
            &no_offset(),
        );
        assert_eq!(r.finalized.len(), 1);
        assert_eq!(r.finalized[0].0.thread_id, 2);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn empty_tick_finalizes_everything() {
        let mut t = InFlightTracker::default();
        t.tick(
            &[obs(1, 2, Some("a")), obs(2, 3, Some("b"))],
            0,
            &no_offset(),
        );
        let r = t.tick(&[], 1_000, &no_offset());
        assert_eq!(r.finalized.len(), 2);
        assert!(t.is_empty());
    }

    #[test]
    fn drain_finalizes_all_for_shutdown() {
        let mut t = InFlightTracker::default();
        t.tick(
            &[obs(1, 2, Some("a")), obs(2, 3, Some("b"))],
            0,
            &no_offset(),
        );
        let drained = t.drain();
        assert_eq!(drained.len(), 2);
        assert!(t.is_empty());
        assert!(drained.iter().all(|(_, r)| !r.observed_end()));
    }

    #[test]
    fn last_seen_is_updated_for_orphan_detection() {
        // F4 — 스케줄러 리더가 `last_seen_at_ms` 로 고아를 판정한다.
        let mut t = InFlightTracker::default();
        t.tick(&[obs(1, 2, Some("a"))], 1_000, &no_offset());
        assert_eq!(t.get(1).unwrap().last_seen_at_ms, 1_000);
        t.tick(&[obs(1, 3, Some("a"))], 5_000, &no_offset());
        assert_eq!(t.get(1).unwrap().last_seen_at_ms, 5_000);
    }

    #[test]
    fn deep_probe_on_unknown_thread_is_ignored() {
        // 심층 조회 응답이 확정 이후에 도착할 수 있다. 패닉하지 않아야 한다.
        let mut t = InFlightTracker::default();
        t.record_deep_probe(999, Some("d".into()), Some(1), 0, &no_offset());
        t.record_plan_attempt(999, true);
        assert!(t.is_empty());
    }
}
