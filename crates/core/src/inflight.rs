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
    /// 이 항목의 레코드가 **저장소에서 실제로 놓인 자리**. 한 번도 못 썼으면 `None`.
    ///
    /// 고아 판정은 저장된 `last_seen_at_ms` 의 침묵으로 한다. 그런데 저장은 심층 조회
    /// 대상일 때만 일어나고, 플랜을 확보하면 그 대상에서 빠진다 — **살아 있는 쿼리의
    /// 저장된 갱신 시각이 굳는다.** 그러면 임계를 넘겨 고아로 확정된다(교차 리뷰 22라운드).
    /// 그래서 주기적으로 그 자리에 생존 신호만 올린다([`InFlightTracker::needs_heartbeat`]).
    ///
    /// **자리를 계산으로 복원할 수 없다.** 물리 키는 시작 시각 추정에서 나오고 그 추정은
    /// 관측자마다 다르며, 병합이 시작 시각을 앞당기면 필드와 키가 어긋난다. 그래서 쓰기가
    /// 알려 준 값을 그대로 들고 있는다 — 24·25라운드의 블로커 셋이 전부 이 값을 추측한
    /// 결과였다.
    pub storage_key: Option<crate::ports::StoredKey>,
    /// 생존 신호를 **마지막으로 시도한** 시각(성공·실패 무관).
    ///
    /// 성공만 기록하면 실패한 항목이 매 tick 다시 후보가 되어 상한을 채운다. 반대로
    /// 실패를 성공처럼 기록하면 그 항목이 **줄의 앞자리를 영구히 차지**해 진짜 갱신이
    /// 필요한 항목이 굶는다(25라운드). 그래서 성공과 시도를 나눠 둔다.
    pub touch_attempt_ms: Option<EpochMs>,
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
    /// 이번 tick 에 관측된 엔트리만으로 상한을 넘긴 횟수.
    ///
    /// 관측 중인 실행을 버리지 않기로 했으므로 이 경우 상한을 일시적으로 넘는다.
    /// 0 이 아니면 `max_entries` 나 `detect_limit` 설정을 재검토해야 한다.
    pub over_cap_ticks: u64,
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
            over_cap_ticks: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// 엔트리 수가 상한을 넘었는가. **관측 중인 실행을 버리지 않기로 한 결과**다.
    pub fn is_over_cap(&self) -> bool {
        self.entries.len() > self.max_entries
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
        // **`BTreeSet` 이다.** `Vec` + `contains` 는 O(엔트리 × 관측) 이고, 크기 상한이
        // 50,000 이라 그 곱이 커졌다 — 릴리스 실측으로 50,000 × 10,000 tick 이 48ms 였다.
        // 정렬 집합이면 O(엔트리 × log 관측) 이다.
        let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();

        for obs in observations {
            // **DB 출력은 신뢰할 수 없는 입력이다.** 같은 tick 에 동일 `thread_id` 가
            // 두 번 오면 방금 만든 엔트리를 `ThreadReused` 로 즉시 확정해 쓰레기
            // 레코드를 만든다. 첫 관측만 쓴다.
            if !seen.insert(obs.thread_id) {
                continue;
            }
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
        //
        // # 이번 tick 에 관측된 것은 축출하지 않는다
        //
        // 처음 구현은 "가장 오래 추적한 것부터" 였는데 두 가지가 깨졌다:
        //
        // 1. **`min_by_key` 를 `while` 안에서 돌려 O(n²)** 였다. 릴리스 실측으로
        //    50,000 엔트리에서 10,000건 축출이 **2.6초** — tick 예산 800ms 의 3배다.
        //    동기 코드라 같은 tokio 워커의 다른 인스턴스까지 굶는다.
        // 2. 아직 실행 중인 스레드를 축출하면 다음 tick 에 **새 엔트리로 다시 생기고**
        //    `started_at_ms` 가 재계산돼 `record_id` 가 달라진다 → 한 실행이 여러
        //    레코드로 쪼개진다(1차 H5 재발). 실측: tick 지터 ±120ms 에서 11.1%.
        //
        // 그래서 **이번 tick 에 보이지 않은 것만** 축출 대상으로 삼는다. 그것들은 이미
        // 사라졌거나 `LIMIT` 컷 아래로 밀린 것이고, 어느 쪽이든 다음 tick 에 다시
        // 관측되면 새 실행으로 보는 것이 맞다.
        //
        // 한 번의 정렬로 필요한 만큼만 고른다 — O(n log n) 이고 tick 당 한 번이다.
        if self.entries.len() > self.max_entries {
            let excess = self.entries.len() - self.max_entries;
            let mut candidates: Vec<(EpochMs, u64)> = self
                .entries
                .iter()
                .filter(|(id, _)| !seen.contains(id))
                .map(|(id, t)| (t.first_observed_at_ms, *id))
                .collect();
            candidates.sort_unstable();
            for (_, id) in candidates.into_iter().take(excess) {
                if let Some(t) = self.entries.remove(&id) {
                    self.evicted_total += 1;
                    result.finalized.push((t, FinalizeReason::Evicted));
                }
            }
            // 이번 tick 에 관측된 것만으로 상한을 넘었다면 줄일 수 없다. 관측 중인 실행을
            // 버리는 것보다 상한을 일시적으로 넘기는 편이 낫다 — 관측 가능하게 남긴다.
            if self.entries.len() > self.max_entries {
                self.over_cap_ticks += 1;
            }
        }

        result
    }

    /// 심층 조회 결과를 반영한다.
    /// 이 항목을 저장소에 **성공적으로 썼다**고 표시한다. 자리도 함께 기록한다.
    ///
    /// 실패했을 때는 부르지 않는다 — 다음 tick 이 다시 고르게 둔다.
    pub fn record_saved(&mut self, thread_id: u64, now_ms: EpochMs, key: crate::ports::StoredKey) {
        if let Some(t) = self.entries.get_mut(&thread_id) {
            t.storage_key = Some(key);
            t.touch_attempt_ms = Some(now_ms);
        }
    }

    /// 생존 신호를 **시도했다**고만 표시한다(결과 무관). 페이스 전용이다.
    pub fn record_touch_attempt(&mut self, thread_id: u64, now_ms: EpochMs) {
        if let Some(t) = self.entries.get_mut(&thread_id) {
            t.touch_attempt_ms = Some(now_ms);
        }
    }

    /// **저장된 갱신 시각이 굳은** 항목들. 오래 안 쓴 것부터 준다.
    ///
    /// # 왜 필요한가
    ///
    /// 고아 스윕은 저장된 `last_seen_at_ms` 의 침묵으로 판정한다. 그런데 저장은 심층
    /// 조회 대상일 때만 일어나고, 플랜을 확보하면(또는 시도 상한에 걸리면) 그 대상에서
    /// 빠진다 — 살아 있는 긴 쿼리의 저장된 값이 그 시점에 굳어 임계를 넘고, **살아 있는
    /// 쿼리가 `abandoned` 로 확정된다**(교차 리뷰 22라운드의 배포 차단 항목).
    ///
    /// 그래서 추적 중인 항목은 주기적으로 다시 쓴다. 그러면 "침묵" 이 실제로 "아무도
    /// 관측하지 않는다" 를 뜻하게 되고, 스윕은 시각 비교만으로 옳아진다 — 소유를
    /// 추론할 필요가 없다(그 추론이 20~22라운드에서 양방향으로 틀렸다).
    ///
    /// # 자리를 아는 항목만 대상이다
    ///
    /// 하트비트는 **쓰기가 알려 준 자리**에만 신호를 올린다(추측하면 다른 실행의 행을
    /// 갱신한다 — 교차 리뷰 25라운드). 그래서 아직 한 번도 저장되지 않은 항목은 올릴
    /// 자리가 없다.
    ///
    /// 그게 인수인계에서 구멍이 되지 않는 이유: **새 추적 항목은 첫 tick 에 반드시 심층
    /// 조회 대상**이고(`needs_deep_probe`), 그 선행 저장이 이전 리더가 만든 행에 병합되며
    /// 그때 자리를 알려 준다. 심층 조회 상한에 걸린 항목은 다음 tick 에 다시 후보가 된다.
    ///
    /// **줄은 마지막 시도 순이다** — 계급을 두지 않는다. 실패한 시도를 성공처럼 기록해
    /// 앞자리에 두면 진짜 갱신이 필요한 항목이 굶는다(25라운드가 201개로 재현했다).
    ///
    /// 상한은 두지 않는다. **호출부가 자르고 자른 사실을 기록한다** — 조용히 자르면
    /// 잘린 항목이 고아로 확정되는데 그 이유가 어디에도 남지 않는다.
    pub fn needs_heartbeat(&self, now_ms: EpochMs, interval_ms: i64) -> Vec<u64> {
        let mut stale: Vec<(EpochMs, u64)> = self
            .entries
            .values()
            .filter(|t| t.storage_key.is_some())
            .filter_map(|t| {
                // 시도한 적이 없으면 최초 관측을 기준으로 잰다.
                let last = t.touch_attempt_ms.unwrap_or(t.first_observed_at_ms);
                (now_ms.saturating_sub(last) >= interval_ms).then_some((last, t.thread_id))
            })
            .collect();
        stale.sort_unstable();
        stale.into_iter().map(|(_, id)| id).collect()
    }

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
            // **조밀 추정보다 늦을 수 없다.** 두 값은 서로 다른 tick 의 `ClockOffset`
            // EMA 를 쓰므로 오프셋이 걸으면 `precise > started_at_ms` 가 될 수 있고,
            // 그러면 레코드가 "종료가 시작보다 이르다" 로 보인다
            // (`display_started_at_ms()` 가 정밀값을 반환한다).
            //
            // 발생 지점에서 막는 것이 요점이다. 병합에서 막으려면 "내 정밀값이 **남의**
            // 종료보다 이른가" 를 물어야 하는데 그건 지역적으로 판정할 수 없고,
            // 병합 후 판정하면 결합법칙이 깨진다(4차 H1).
            let precise = offset.to_db_time(observed_at_ms) - elapsed_ms;
            t.started_at_ms_precise = Some(precise.min(t.started_at_ms));
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
        storage_key: None,
        touch_attempt_ms: None,
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

    /// 축출 대상은 **이번 tick 에 보이지 않은 것 중 가장 오래된 것**이다.
    #[test]
    fn eviction_removes_the_oldest_unseen_entry() {
        let mut t = InFlightTracker::default().with_max_entries(2);
        t.tick(&[obs(1, 3, Some("a"))], 1_000, &no_offset(), true);
        t.tick(&[obs(2, 3, Some("b"))], 2_000, &no_offset(), true);
        // 3번이 새로 보인다. 1·2 는 이번 tick 에 없다 → 오래된 1번이 축출된다.
        let r = t.tick(&[obs(3, 3, Some("c"))], 3_000, &no_offset(), true);

        let evicted: Vec<u64> = r
            .finalized
            .iter()
            .filter(|(_, reason)| *reason == FinalizeReason::Evicted)
            .map(|(tr, _)| tr.thread_id)
            .collect();
        assert_eq!(evicted, vec![1], "보이지 않는 것 중 가장 오래된 1번");
        assert!(t.get(3).is_some(), "방금 관측된 것을 버리면 안 된다");
    }

    /// **관측 중인 실행은 축출하지 않는다.**
    ///
    /// 축출하면 다음 tick 에 새 엔트리로 다시 생기고 `started_at_ms` 가 재계산돼
    /// `record_id` 가 달라진다 → 한 실행이 여러 레코드로 쪼개진다 (1차 H5 재발).
    #[test]
    fn currently_observed_entries_are_never_evicted() {
        let mut t = InFlightTracker::default().with_max_entries(2);
        // 상한 2인데 이번 tick 에 3개가 모두 관측된다.
        let obs3 = [
            obs(1, 3, Some("a")),
            obs(2, 3, Some("b")),
            obs(3, 3, Some("c")),
        ];
        let r = t.tick(&obs3, 1_000, &no_offset(), true);

        assert!(
            r.finalized.is_empty(),
            "관측 중인 실행을 축출했다 — record_id 가 흔들린다"
        );
        assert_eq!(t.len(), 3, "상한을 일시적으로 넘기는 것이 맞다");
        assert_eq!(t.over_cap_ticks, 1, "넘긴 사실이 관측 가능해야 한다");

        // 같은 스레드가 계속 보이면 record_id 근거(started_at_ms)가 불변이어야 한다.
        let started_before: Vec<i64> = (1..=3).map(|i| t.get(i).unwrap().started_at_ms).collect();
        t.tick(&obs3, 2_100, &no_offset(), true);
        let started_after: Vec<i64> = (1..=3).map(|i| t.get(i).unwrap().started_at_ms).collect();
        assert_eq!(
            started_before, started_after,
            "started_at_ms 가 재계산되면 record_id 가 달라진다"
        );
    }

    /// 축출이 **한 번의 정렬**로 끝나야 한다. `min_by_key` 를 루프 안에서 돌리면
    /// 50,000 엔트리에서 10,000건 축출이 릴리스 빌드로 2.6초 — tick 예산의 3배다.
    #[test]
    fn eviction_is_not_quadratic() {
        let mut t = InFlightTracker::default().with_max_entries(2_000);
        // 4,000개를 채운다 (아무도 이번 tick 에 보이지 않게 만든다).
        for chunk in 0..8 {
            let obs_batch: Vec<Observation> = (0..500)
                .map(|i| obs(chunk * 500 + i, 3, Some("d")))
                .collect();
            t.tick(&obs_batch, 1_000 + chunk as i64 * 1_000, &no_offset(), true);
        }
        assert!(t.len() >= 2_000);

        let start = std::time::Instant::now();
        let r = t.tick(&[obs(99_999, 3, Some("z"))], 100_000, &no_offset(), true);
        let elapsed = start.elapsed();

        assert!(t.len() <= 2_000, "상한이 지켜지지 않았다 ({})", t.len());
        assert!(!r.finalized.is_empty(), "축출이 일어나야 하는 상황이다");
        // 디버그 빌드라 넉넉히 잡는다. O(n²) 면 여기서 몇 초가 걸린다.
        assert!(
            elapsed.as_millis() < 500,
            "축출이 {}ms 걸렸다 — O(n²) 로 돌아갔을 수 있다",
            elapsed.as_millis()
        );
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

    /// **정밀 시작이 조밀 추정보다 늦을 수 없다.**
    ///
    /// 두 값은 서로 다른 tick 의 `ClockOffset` EMA 를 쓰므로 오프셋이 걸으면 역전된다.
    /// 그러면 `display_started_at_ms()` 가 종료보다 늦은 시각을 반환해 운영자가
    /// "종료가 시작보다 이른" 레코드를 본다.
    #[test]
    fn precise_start_never_exceeds_the_coarse_estimate() {
        let mut t = InFlightTracker::default();
        t.tick(&[obs(1, 4, None)], 10_000, &no_offset(), false);
        let coarse = t.get(1).unwrap().started_at_ms;

        // 오프셋이 +8초로 걸었다 — 정밀값이 조밀 추정보다 훨씬 늦어진다.
        let stepped = ClockOffset::restored(8_000);
        t.record_deep_probe(1, Some("d".into()), Some(1_000_000_000), 10_000, &stepped);

        let precise = t.get(1).unwrap().started_at_ms_precise.expect("정밀값");
        assert!(
            precise <= coarse,
            "정밀 시작({precise})이 조밀 추정({coarse})보다 늦다 — 레코드가 자기모순이 된다"
        );
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

    fn key(n: u64) -> crate::ports::StoredKey {
        crate::ports::StoredKey::new(format!("row-{n}"))
    }

    /// **플랜을 확보한 뒤에도 하트비트 대상이다.**
    ///
    /// 심층 조회 대상은 "플랜이 없는 것" 이므로 플랜을 확보하면 저장이 멈춘다. 그때
    /// 하트비트가 없으면 저장된 `last_seen_at_ms` 가 굳고, 살아 있는 쿼리가 임계를 넘겨
    /// `abandoned` 로 확정된다 — 되돌릴 수 없는 실패다(교차 리뷰 22라운드).
    #[test]
    fn a_tracked_entry_still_needs_heartbeats_after_its_plan_is_captured() {
        const INTERVAL: EpochMs = 15_000;
        let mut t = InFlightTracker::default();
        let r = t.tick(&[obs(1, 2, Some("a"))], 1_000, &no_offset(), false);
        assert_eq!(r.needs_deep_probe, vec![1], "새 항목은 심층 조회 대상이다");

        // 심층 조회로 플랜을 확보하고 저장했다 — 그때 저장소가 자리를 알려 준다.
        t.record_plan_attempt(1, true);
        t.record_saved(1, 1_000, key(1));

        // 이제 심층 조회 대상이 아니다 — 저장을 유발하는 경로가 없다.
        let r = t.tick(&[obs(1, 30, Some("a"))], 31_000, &no_offset(), false);
        assert!(
            r.needs_deep_probe.is_empty(),
            "플랜이 있으면 다시 조회하지 않는다 — 그래서 하트비트가 필요하다"
        );

        // 관측은 계속되고 있다(메모리 값은 올라간다).
        assert_eq!(t.get(1).unwrap().last_seen_at_ms, 31_000);
        // **주기가 지났으므로 다시 써야 한다.**
        assert_eq!(t.needs_heartbeat(31_000, INTERVAL), vec![1]);

        // 시도하면 주기 안에는 다시 고르지 않는다 — 쓰기 비용에 상한이 생긴다.
        t.record_touch_attempt(1, 31_000);
        assert!(t.needs_heartbeat(40_000, INTERVAL).is_empty());
        assert_eq!(t.needs_heartbeat(46_000, INTERVAL), vec![1]);
    }

    /// **자리를 모르는 항목은 대상이 아니다.**
    ///
    /// 하트비트는 쓰기가 알려 준 자리에만 신호를 올린다. 자리를 추측하면 같은 스레드의
    /// 옛 실행을 살려 두고 산 실행을 버릴 수 있다(25라운드). 자리를 모르는 항목은
    /// 첫 tick 의 심층 조회로 곧 자리를 얻는다.
    #[test]
    fn entries_without_a_known_row_are_not_candidates() {
        const INTERVAL: EpochMs = 15_000;
        let mut t = InFlightTracker::default();
        t.tick(
            &[obs(1, 2, Some("a")), obs(2, 3, Some("b"))],
            1_000,
            &no_offset(),
            false,
        );
        // 둘 다 아직 저장되지 않았다 → 올릴 자리가 없다.
        assert_eq!(t.needs_heartbeat(40_000, INTERVAL), Vec::<u64>::new());
        // 선행 저장이 자리를 알려 주면 대상이 된다.
        t.record_saved(1, 2_000, key(1));
        assert_eq!(t.needs_heartbeat(40_000, INTERVAL), vec![1]);
    }

    /// **실패한 시도가 줄의 앞자리를 차지하지 않는다.**
    ///
    /// 25라운드가 201개로 재현했다: 조건이 깨진 항목을 "저장했다" 로 기록하면 그 항목들이
    /// 매 tick 앞자리를 채워, 상한(호출부 200) 밖으로 밀린 **진짜 갱신이 필요한 항목**이
    /// 영구히 굶는다. 줄을 마지막 시도 순으로만 세우면 그 상태가 만들어지지 않는다.
    #[test]
    fn a_failed_attempt_does_not_starve_the_queue() {
        const INTERVAL: EpochMs = 15_000;
        const CAP: usize = 200;
        let mut t = InFlightTracker::default();
        let obs_all: Vec<Observation> = (1..=201).map(|i| obs(i, 2, Some("a"))).collect();
        t.tick(&obs_all, 1_000, &no_offset(), false);
        // 전부 자리를 안다(선행 저장이 됐다).
        for i in 1..=201u64 {
            t.record_saved(i, 1_000, key(i));
        }

        // 1차: 상한만큼 시도한다. 조건이 깨져도 **시도**로 기록된다.
        let first = t.needs_heartbeat(20_000, INTERVAL);
        assert_eq!(first.len(), 201);
        for id in first.iter().take(CAP) {
            t.record_touch_attempt(*id, 20_000);
        }

        // 2차: 밀렸던 201번이 **맨 앞**이다 — 그게 공평한 줄이다.
        let second = t.needs_heartbeat(36_000, INTERVAL);
        assert_eq!(second[0], 201, "밀린 항목이 다시 뒤로 갔다 — 영구히 굶는다");
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
