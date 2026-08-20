//! 시계 오프셋 추정 (F14, [05 §4.2](../../../docs/05-collector.md)).
//!
//! # 왜 필요한가
//!
//! `first_observed_at_ms` 는 **우리 시계**(컨테이너)이고 슬로우로그의 시작 시각은
//! **DB 시계**에서 온다. 두 시계가 2초 이상 어긋나면:
//!
//! | 영향 | 결과 |
//! |---|---|
//! | ±2초 병합 창 ([05 §8.2](../../../docs/05-collector.md)) | **상시 실패** → 모든 레코드가 이중화된다 |
//! | `record_id` 의 초 성분 | 갈라진다 → 멱등성이 깨진다 |
//! | `hour_bucket` · `date_part` · `dur_bucket` | 경계 배정이 틀어져 파티션이 어긋난다 |
//!
//! 초기 설계는 `LAST_SEEN` 필터에 대해서만 "우리 시계를 쓰지 않는다"고 해결했는데,
//! **그 원칙이 다른 경로에는 적용되지 않았다.**
//!
//! # 지수 이동 평균을 쓰는 이유
//!
//! 단발 측정은 왕복 지연·GC·스케줄링 지터에 흔들린다. α=0.2 면 대략 최근 5회 측정의
//! 가중 평균이라 순간 튐을 흡수하면서 실제 드리프트는 몇 tick 안에 따라간다.

use crate::time::EpochMs;

/// 지수 이동 평균 계수. 작을수록 안정적이고 느리다.
const ALPHA: f64 = 0.2;

/// 이 값을 넘으면 자가진단 경고 + 병합 창 자동 확대.
pub const WARN_THRESHOLD_MS: i64 = 1_000;
/// 이 값을 넘으면 `CLOCK_SKEW` 이벤트 + 알림. 사람이 봐야 한다.
pub const ALERT_THRESHOLD_MS: i64 = 5_000;
/// 기본 병합 창 ([05 §8.2](../../../docs/05-collector.md)).
pub const BASE_MERGE_WINDOW_MS: i64 = 2_000;

/// **같은 실행의 두 시작 추정이 어긋날 수 있는 최대 폭 (1초).**
///
/// 실시간 캡처의 시작은 `now − PROCESSLIST.TIME × 1000` 이고 `TIME` 은 **정수 초**라
/// 오차가 1초 미만이다. 슬로우로그는 밀리초다. 그래서 같은 실행의 두 관측은 1초
/// 안에 들어온다.
///
/// # 왜 병합 창과 따로 두는가
///
/// 창([`BASE_MERGE_WINDOW_MS`])은 **후보를 긁어오는 범위**이고 여유가 섞여 있다.
/// 그 창을 동일성 판정에 그대로 쓰면 `long_query_time` 이 창보다 작을 때 **연속한 두
/// 실행**이 합쳐진다 — 종료를 관측하지 못한 레코드의 끝은 "사라진 것을 알아챈
/// 폴링" 까지 늘어나므로 다음 실행과 겹쳐 보이기 때문이다(10라운드 지적).
///
/// ⚠ **남은 위험**: `long_query_time` 이 1초 미만이면 이 폭 안에 연속한 두 실행이
/// 들어올 수 있다. 근본 해결은 실행마다 고유한 식별자
/// (`performance_schema` 의 `EVENT_ID`)를 레코드에 담는 것이다 — 지금은 없다.
pub const LIVE_ESTIMATE_SPREAD_MS: i64 = 1_000;

/// 대상 DB 와 우리 시계의 차이. **인스턴스별로 유지한다.**
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClockOffset {
    /// `db_now - local_now`. 양수면 DB 가 앞서 있다.
    offset_ms: f64,
    /// 측정 횟수. 0이면 아직 추정값이 없다.
    samples: u32,
}

impl ClockOffset {
    /// 이전 상태에서 복원한다. 샤드 인수 시 계승한다 (F24).
    pub fn restored(offset_ms: i64) -> Self {
        Self {
            offset_ms: offset_ms as f64,
            samples: 1,
        }
    }

    /// 측정을 반영한다.
    ///
    /// `rtt_ms` 를 주면 왕복 지연의 절반을 보정한다 — DB 시각은 요청과 응답 사이
    /// 어딘가에 찍히므로 응답 시점과 비교하면 RTT/2 만큼 과소 추정된다.
    pub fn observe(&mut self, db_now_ms: EpochMs, local_now_ms: EpochMs, rtt_ms: Option<i64>) {
        let raw = (db_now_ms - local_now_ms) as f64 + rtt_ms.unwrap_or(0) as f64 / 2.0;
        self.offset_ms = if self.samples == 0 {
            // 첫 측정은 그대로 받는다. EMA 로 0에서 출발하면 수렴이 느리다.
            raw
        } else {
            ALPHA * raw + (1.0 - ALPHA) * self.offset_ms
        };
        self.samples = self.samples.saturating_add(1);
    }

    pub fn as_ms(&self) -> i64 {
        self.offset_ms.round() as i64
    }

    pub fn has_estimate(&self) -> bool {
        self.samples > 0
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// 우리 시계의 시각을 DB 시계로 환산한다.
    ///
    /// **`started_at_ms` 추정·병합 창·파티션 배정에 모두 이걸 쓴다.**
    /// 하나라도 빠뜨리면 그 경로만 어긋나 진단이 어려워진다.
    pub fn to_db_time(&self, local_ms: EpochMs) -> EpochMs {
        local_ms + self.as_ms()
    }

    /// DB 시계의 시각을 우리 시계로 환산한다. TTL 계산에 쓴다
    /// (TTL 은 DynamoDB 가 판단하므로 우리 시계여야 한다).
    pub fn to_local_time(&self, db_ms: EpochMs) -> EpochMs {
        db_ms - self.as_ms()
    }

    pub fn severity(&self) -> OffsetSeverity {
        let abs = self.as_ms().abs();
        if !self.has_estimate() {
            OffsetSeverity::Unknown
        } else if abs > ALERT_THRESHOLD_MS {
            OffsetSeverity::Alert
        } else if abs > WARN_THRESHOLD_MS {
            OffsetSeverity::Warn
        } else {
            OffsetSeverity::Ok
        }
    }

    /// 병합 창. 오프셋이 크면 **자동으로 넓힌다** — 좁은 창이 상시 실패하는 것보다
    /// 넓은 창이 드물게 오병합하는 것이 낫다(오병합은 `app_digest` 와 `thread_id` 가
    /// 같아야 하므로 실제로는 거의 없다).
    pub fn merge_window_ms(&self) -> i64 {
        let abs = self.as_ms().abs();
        if abs > WARN_THRESHOLD_MS {
            abs + BASE_MERGE_WINDOW_MS
        } else {
            BASE_MERGE_WINDOW_MS
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetSeverity {
    /// 아직 측정하지 않았다.
    Unknown,
    Ok,
    /// 1초 초과 — 자가진단 경고 + 병합 창 확대.
    Warn,
    /// 5초 초과 — `CLOCK_SKEW` 이벤트 + 알림.
    Alert,
}

impl OffsetSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Alert => "alert",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_without_estimate() {
        let c = ClockOffset::default();
        assert!(!c.has_estimate());
        assert_eq!(c.severity(), OffsetSeverity::Unknown);
        assert_eq!(c.as_ms(), 0);
        // 추정값이 없어도 환산이 패닉하지 않아야 한다 (오프셋 0으로 동작).
        assert_eq!(c.to_db_time(1_000), 1_000);
    }

    #[test]
    fn first_sample_is_taken_directly() {
        // EMA 로 0에서 출발하면 3초 오차를 따라잡는 데 여러 tick 이 걸린다.
        let mut c = ClockOffset::default();
        c.observe(10_000, 7_000, None);
        assert_eq!(c.as_ms(), 3_000);
        assert_eq!(c.samples(), 1);
    }

    #[test]
    fn ema_absorbs_a_single_spike() {
        let mut c = ClockOffset::default();
        for _ in 0..10 {
            c.observe(10_000, 10_000, None); // 오프셋 0
        }
        assert_eq!(c.as_ms(), 0);

        // 한 번 크게 튄다 (GC·스케줄링 지터).
        c.observe(10_000 + 5_000, 10_000, None);
        let after_spike = c.as_ms();
        assert!(
            (900..=1_100).contains(&after_spike),
            "단발 스파이크가 그대로 반영됐다: {after_spike}"
        );

        // 정상 측정이 이어지면 다시 0으로 돌아온다.
        for _ in 0..20 {
            c.observe(10_000, 10_000, None);
        }
        assert!(c.as_ms().abs() <= 20, "{}", c.as_ms());
    }

    #[test]
    fn converges_to_real_drift_within_a_few_ticks() {
        let mut c = ClockOffset::default();
        for _ in 0..10 {
            c.observe(10_000, 10_000, None);
        }
        // 실제로 DB 가 2초 앞서게 됐다.
        for _ in 0..15 {
            c.observe(12_000, 10_000, None);
        }
        assert!((1_900..=2_000).contains(&c.as_ms()), "{}", c.as_ms());
    }

    #[test]
    fn rtt_compensation_shifts_estimate() {
        let mut a = ClockOffset::default();
        a.observe(10_000, 10_000, None);
        let mut b = ClockOffset::default();
        b.observe(10_000, 10_000, Some(100));
        assert_eq!(a.as_ms(), 0);
        assert_eq!(b.as_ms(), 50, "RTT 의 절반을 보정한다");
    }

    #[test]
    fn severity_thresholds() {
        let cases = [
            (0, OffsetSeverity::Ok),
            (999, OffsetSeverity::Ok),
            (1_000, OffsetSeverity::Ok),
            (1_001, OffsetSeverity::Warn),
            (-1_001, OffsetSeverity::Warn),
            (5_000, OffsetSeverity::Warn),
            (5_001, OffsetSeverity::Alert),
            (-9_000, OffsetSeverity::Alert),
        ];
        for (offset, want) in cases {
            let c = ClockOffset::restored(offset);
            assert_eq!(c.severity(), want, "offset={offset}");
        }
    }

    #[test]
    fn merge_window_widens_with_offset() {
        assert_eq!(ClockOffset::restored(0).merge_window_ms(), 2_000);
        assert_eq!(ClockOffset::restored(500).merge_window_ms(), 2_000);
        // 3초 어긋났으면 창이 5초가 된다 — 좁은 창이 상시 실패하는 것보다 낫다.
        assert_eq!(ClockOffset::restored(3_000).merge_window_ms(), 5_000);
        assert_eq!(ClockOffset::restored(-3_000).merge_window_ms(), 5_000);
    }

    #[test]
    fn conversions_are_inverse() {
        let c = ClockOffset::restored(1_500);
        assert_eq!(c.to_db_time(1_000_000), 1_001_500);
        assert_eq!(c.to_local_time(1_001_500), 1_000_000);
        assert_eq!(c.to_local_time(c.to_db_time(42)), 42);
    }

    #[test]
    fn restored_offset_is_inherited_on_shard_takeover() {
        // F24 — 인수 시 계승해야 한다. 0에서 다시 추정하면 몇 tick 동안 시각이 틀린다.
        let c = ClockOffset::restored(-2_500);
        assert!(c.has_estimate());
        assert_eq!(c.as_ms(), -2_500);
        assert_eq!(c.severity(), OffsetSeverity::Warn);
    }

    #[test]
    fn sample_counter_saturates() {
        let mut c = ClockOffset {
            offset_ms: 0.0,
            samples: u32::MAX,
        };
        c.observe(1, 1, None);
        assert_eq!(
            c.samples(),
            u32::MAX,
            "오버플로로 0이 되면 첫 측정 취급이 된다"
        );
    }
}
