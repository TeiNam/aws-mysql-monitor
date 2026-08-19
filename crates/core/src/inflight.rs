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
    /// **엔트리 수 상한**에 걸려 강제 확정했다.
    ///
    /// `TooLong` 과 구분하는 이유: 이 값이 레코드에 남으면 "동시 슬로우 쿼리가 상한을
    /// 넘었다" 는 사실이 사후에도 보인다. `TooLong` 으로 합치면 1시간 실행 쿼리와
    /// 구분할 수 없고 `long_running` 이 잘못 붙는다.
    Evicted,
    /// 그레이스풀 셧다운·리더 상실로 강제 확정했다.
    ///
    /// `TooLong` 과 구분하는 이유: 저장된 레코드의 `abandoned_reason` 이 사후 분석의
    /// 유일한 단서다. 둘을 합치면 "쿼리가 1시간을 넘겼다" 와 "우리가 재배포했다" 를
    /// 구분할 수 없고, `long_running` 이 잘못 붙는다.
    Shutdown,
}

impl FinalizeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disappeared => "disappeared",
            Self::ThreadReused => "thread_reused",
            Self::TooLong => "too_long",
            Self::Evicted => "evicted",
            Self::Shutdown => "shutdown",
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
    max_entries: usize,
    /// 상한 때문에 강제 확정된 누적 건수. **관측 가능해야 한다** — 조용히 버리면
    /// 왜 레코드가 사라지는지 알 수 없다.
    pub evicted_total: u64,
}

/// 기본 최대 추적 시간. 이걸 넘기면 강제 확정한다.
pub const DEFAULT_MAX_TRACKING_MS: i64 = 3_600_000;
/// 플랜 수집 재시도 상한.
pub const DEFAULT_MAX_PLAN_ATTEMPTS: u8 = 3;

/// 추적 엔트리 수 상한.
///
/// # `max_tracking_ms` 는 **크기** 상한이 아니다
///
/// 잘린 tick(`list_truncated`)에서는 사라짐 판정을 건너뛰므로, 절단이 지속되면
/// 엔트리가 `max_tracking_ms`(1시간) 동안 **한 건도 제거되지 않는다.** 기본값
/// (1초 tick, 500행/tick, 스레드 절반 회전)으로 계산하면:
///
/// ```text
/// tick   600 (10분): 약 150,000 엔트리
/// tick  3600 (60분): 약 900,000 엔트리 → 인스턴스당 약 225MB
/// ```
///
/// 그리고 1시간 뒤에 250건/tick 씩 `TooLong` 으로 쏟아져 tick 안에서 순차 쓰기가 되고,
/// 전부 `long_running=true` 오탐이 된다. 절단은 정의상 "장애 중" 이므로 이 경로가
/// 가장 나쁠 때 터진다.
///
/// `detect_limit` 최대(10,000)의 몇 배로 잡는다 — 정상 운영에서는 절대 닿지 않고,
/// 병리적 상황에서만 메모리를 묶는다.
pub const DEFAULT_MAX_ENTRIES: usize = 50_000;

impl Default for InFlightTracker {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_TRACKING_MS, DEFAULT_MAX_PLAN_ATTEMPTS)
    }
}

impl InFlightTracker {
    /// 엔트리 수 상한을 바꾼다 (테스트용).
    pub fn with_max_entries(mut self, n: usize) -> Self {
        self.max_entries = n;
        self
    }

    pub fn new(max_tracking_ms: i64, max_plan_attempts: u8) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_tracking_ms,
            max_plan_attempts,
            max_entries: DEFAULT_MAX_ENTRIES,
            evicted_total: 0,
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
    /// `list_truncated` 는 detect 조회가 `LIMIT` 에 걸렸는지다.
    ///
    /// # 잘린 목록으로 사라짐 판정을 하면 실행 중인 쿼리에 거짓 종료 시각이 박힌다
    ///
    /// detect 는 `ORDER BY TIME DESC LIMIT 500` 이다. 동시 슬로우 쿼리가 500개를 넘으면
    /// 컷 아래의 스레드는 **아직 실행 중인데도** 이번 tick 결과에 없다. 그걸 사라짐으로
    /// 보면 `Disappeared` → `observed_end() == true` → 거짓 `ended_at_ms` 로 확정되고,
    /// 다음 tick 에 다시 보이면 같은 실행이 여러 레코드로 쪼개진다.
    ///
    /// 동시 슬로우 쿼리 500개 초과는 정확히 "장애 중"이며, 이 도구가 가장 정확해야 하는
    /// 순간이다. 그래서 잘린 tick 에서는 **사라짐 판정을 건너뛴다.** 실제로 끝난 항목은
    /// 다음 온전한 tick 에서 확정된다. `too_long` 상한이 여전히 캐시를 제한한다.
    pub fn tick(
        &mut self,
        observations: &[Observation],
        now_ms: EpochMs,
        offset: &ClockOffset,
        list_truncated: bool,
    ) -> TickResult {
        let mut result = TickResult::default();
        let mut seen: Vec<u64> = Vec::with_capacity(observations.len());

        for obs in observations {
            // **DB 출력은 신뢰할 수 없는 입력이다.** 같은 tick 에 동일 `thread_id` 가
            // 두 번 오면 방금 만든 엔트리를 `ThreadReused` 로 즉시 확정해 쓰레기
            // 레코드를 만든다. 첫 관측만 쓴다.
            if seen.contains(&obs.thread_id) {
                continue;
            }
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

        // 사라진 항목을 확정한다. **목록이 잘렸으면 판정할 수 없다.**
        let disappeared: Vec<u64> = if list_truncated {
            Vec::new()
        } else {
            self.entries
                .keys()
                .copied()
                .filter(|id| !seen.contains(id))
                .collect()
        };
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

        // **크기 상한.** 시간 상한만으로는 잘린 tick 이 지속될 때 캐시를 묶지 못한다.
        // 가장 오래 추적한 것부터 확정한다 — 새 관측보다 오래된 것이 끝났을 확률이 높다.
        while self.entries.len() > self.max_entries {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, t)| t.first_observed_at_ms)
                .map(|(id, _)| *id);
            let Some(id) = oldest else { break };
            if let Some(t) = self.entries.remove(&id) {
                self.evicted_total += 1;
                result.finalized.push((t, FinalizeReason::Evicted));
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
            .map(|t| (t, FinalizeReason::Shutdown))
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

    /// **잘린 tick 이 지속되면 시간 상한만으로는 캐시를 묶지 못한다.**
    ///
    /// 절단은 정의상 "동시 슬로우 쿼리가 상한을 넘었다 = 장애 중" 이다. 그때
    /// 사라짐 판정을 건너뛰므로 1시간 동안 한 건도 제거되지 않고 메모리가 수백 MB 로 자란다.
    #[test]
    fn entry_cap_bounds_the_cache_when_truncation_persists() {
        let mut t = InFlightTracker::default().with_max_entries(10);
        let mut evicted = 0usize;

        // 매 tick 새 스레드 5개가 나타나고 아무도 사라지지 않는다(목록이 잘렸다).
        for tick in 0..20i64 {
            let obs: Vec<Observation> = (0..5)
                .map(|i| obs((tick * 5 + i) as u64, 3, Some("d")))
                .collect();
            let r = t.tick(&obs, tick * 1_000, &no_offset(), true);
            evicted += r
                .finalized
                .iter()
                .filter(|(_, reason)| *reason == FinalizeReason::Evicted)
                .count();
            assert!(
                t.len() <= 10,
                "tick {tick}: 엔트리가 상한을 넘었다 ({}건)",
                t.len()
            );
        }
        assert!(evicted > 0, "축출이 한 번도 일어나지 않았다");
        assert_eq!(
            t.evicted_total as usize, evicted,
            "축출 수가 관측되지 않는다"
        );
    }

    /// 축출은 **가장 오래 추적한 것부터**여야 한다. 새로 관측된 것을 버리면
    /// 방금 시작한 쿼리가 즉시 확정된다.
    #[test]
    fn eviction_removes_the_oldest_entry_first() {
        let mut t = InFlightTracker::default().with_max_entries(2);
        t.tick(&[obs(1, 3, Some("a"))], 1_000, &no_offset(), true);
        t.tick(&[obs(2, 3, Some("b"))], 2_000, &no_offset(), true);
        let r = t.tick(&[obs(3, 3, Some("c"))], 3_000, &no_offset(), true);

        let evicted: Vec<u64> = r
            .finalized
            .iter()
            .filter(|(_, reason)| *reason == FinalizeReason::Evicted)
            .map(|(tr, _)| tr.thread_id)
            .collect();
        assert_eq!(evicted, vec![1], "가장 먼저 관측된 1번이 축출돼야 한다");
        assert!(t.get(3).is_some(), "방금 관측된 것을 버리면 안 된다");
    }

    /// `Evicted` 는 `TooLong` 과 구분돼야 한다 — `long_running` 오탐을 막는다.
    #[test]
    fn evicted_is_not_reported_as_too_long() {
        assert_ne!(
            FinalizeReason::Evicted.as_str(),
            FinalizeReason::TooLong.as_str()
        );
        assert!(!FinalizeReason::Evicted.observed_end());
    }

    /// detect 목록이 `LIMIT` 에 잘렸으면 실행 중인 쿼리를 "정상 종료"로 확정하면 안 된다.
    #[test]
    fn truncated_list_does_not_finalize_missing_entries() {
        let mut t = InFlightTracker::default();
        t.tick(
            &[obs(1, 5, Some("a")), obs(2, 3, Some("b"))],
            0,
            &no_offset(),
            false,
        );
        // 2번이 LIMIT 컷 아래로 밀려 목록에서 빠졌다 — 아직 실행 중이다.
        let r = t.tick(&[obs(1, 6, Some("a"))], 1_000, &no_offset(), true);
        assert!(
            r.finalized.is_empty(),
            "잘린 목록으로는 사라짐을 판정할 수 없다"
        );
        assert_eq!(t.len(), 2, "두 항목 모두 추적을 유지한다");

        // 온전한 tick 이 오면 그때 확정한다.
        let r = t.tick(&[obs(1, 7, Some("a"))], 2_000, &no_offset(), false);
        assert_eq!(r.finalized.len(), 1);
        assert_eq!(r.finalized[0].0.thread_id, 2);
        assert_eq!(r.finalized[0].1, FinalizeReason::Disappeared);
    }

    /// 잘린 tick 에서도 `too_long` 상한은 살아 있어야 한다 — 아니면 캐시가 무한히 자란다.
    #[test]
    fn truncated_list_still_enforces_max_tracking() {
        let mut t = InFlightTracker::new(1_000, 3);
        t.tick(&[obs(1, 2, Some("d"))], 0, &no_offset(), false);
        let r = t.tick(&[], 5_000, &no_offset(), true);
        assert_eq!(r.finalized.len(), 1);
        assert_eq!(r.finalized[0].1, FinalizeReason::TooLong);
        assert!(t.is_empty());
    }

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
        let r = t.tick(&[obs(100, 2, None)], 10_000, &no_offset(), false);
        assert_eq!(r.needs_deep_probe, vec![100]);
        assert!(r.finalized.is_empty());
        assert_eq!(t.get(100).unwrap().state, TrackedState::Observed);
        // 시작 시각 = 관측 시각 − TIME
        assert_eq!(t.get(100).unwrap().started_at_ms, 8_000);
    }

    #[test]
    fn continued_observation_updates_max_time() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(100, 2, Some("d1"))], 10_000, &no_offset(), false);
        t.record_plan_attempt(100, true);
        let r = t.tick(&[obs(100, 5, Some("d1"))], 13_000, &no_offset(), false);
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
        t.tick(&[obs(1, 2, Some("d"))], 0, &no_offset(), false);
        t.tick(&[obs(1, 3, Some("d"))], 1_000, &no_offset(), false);
        t.tick(&[obs(1, 4, Some("d"))], 2_000, &no_offset(), false);
        let r = t.tick(&[], 3_000, &no_offset(), false);
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
        t.tick(
            &[obs(100, 3, Some("digest-A"))],
            10_000,
            &no_offset(),
            false,
        );
        let r = t.tick(
            &[obs(100, 2, Some("digest-B"))],
            13_000,
            &no_offset(),
            false,
        );

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
        t.tick(&[obs(100, 10, Some("d"))], 10_000, &no_offset(), false);
        let r = t.tick(&[obs(100, 2, Some("d"))], 20_000, &no_offset(), false);
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
            t.tick(&[obs(100, 3, Some("d"))], 10_000, &no_offset(), false);
            let mut second = obs(100, 4, Some("d"));
            second.identity.db_user = user.map(str::to_string);
            second.identity.db_host = host.map(str::to_string);
            let r = t.tick(&[second], 11_000, &no_offset(), false);
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
        t.tick(&[obs(100, 2, None)], 10_000, &no_offset(), false);
        let r = t.tick(&[obs(100, 3, Some("d1"))], 11_000, &no_offset(), false);
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
        t.tick(&[obs(1, 2, None)], 10_000, &offset, false);
        assert_eq!(t.get(1).unwrap().started_at_ms, 10_000 + 3_000 - 2_000);
        // 우리 시계 기준 최초 관측 시각은 그대로 보존한다 (고아 판정에 쓴다).
        assert_eq!(t.get(1).unwrap().first_observed_at_ms, 10_000);
    }

    #[test]
    fn timer_wait_refines_start_time() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(1, 4, None)], 10_000, &no_offset(), false);
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
        t.tick(&[obs(1, 2, Some("d"))], 0, &no_offset(), false);
        t.record_plan_attempt(1, false);
        let r = t.tick(&[obs(1, 3, Some("d"))], 1_000, &no_offset(), false);
        assert_eq!(r.needs_deep_probe, vec![1], "1회 실패 후에는 재시도한다");
        t.record_plan_attempt(1, false);
        let r = t.tick(&[obs(1, 4, Some("d"))], 2_000, &no_offset(), false);
        assert!(r.needs_deep_probe.is_empty(), "상한에 도달하면 포기한다");
        assert_eq!(t.get(1).unwrap().plan_attempts, 2);
    }

    #[test]
    fn too_long_tracking_is_force_finalized_and_removed() {
        let mut t = InFlightTracker::new(1_000, 3);
        t.tick(&[obs(1, 2, Some("d"))], 0, &no_offset(), false);
        let r = t.tick(&[obs(1, 3, Some("d"))], 5_000, &no_offset(), false);
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
            false,
        );
        assert_eq!(r.needs_deep_probe, vec![1, 2, 3]);
        assert_eq!(t.len(), 3);

        // 2번만 사라진다.
        let r = t.tick(
            &[obs(1, 3, Some("a")), obs(3, 10, Some("c"))],
            1_000,
            &no_offset(),
            false,
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
            false,
        );
        let r = t.tick(&[], 1_000, &no_offset(), false);
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
            false,
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
        t.tick(&[obs(1, 2, Some("a"))], 1_000, &no_offset(), false);
        assert_eq!(t.get(1).unwrap().last_seen_at_ms, 1_000);
        t.tick(&[obs(1, 3, Some("a"))], 5_000, &no_offset(), false);
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
