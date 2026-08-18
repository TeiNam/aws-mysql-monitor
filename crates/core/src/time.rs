//! 시각 타입. **모든 시각은 UTC epoch millis 로 저장한다**
//! ([04 §0](../../../docs/04-data-model.md)).
//!
//! 1세대는 KST 문자열을 저장해서 집계가 어려웠다. 표시 시각 변환은 프론트엔드에서만 한다.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// UTC epoch milliseconds.
pub type EpochMs = i64;

/// `YYYY-MM-DDTHH` (UTC). 시간 롤업의 축.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HourBucket(String);

impl HourBucket {
    pub fn from_epoch_ms(ms: EpochMs) -> Self {
        let dt = to_utc(ms);
        Self(format!(
            "{:04}-{:02}-{:02}T{:02}",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 이 버킷이 시작하는 시각. TTL 계산의 기준
    /// (`ttl = hour 시작 + 35일`, [04 §2.3](../../../docs/04-data-model.md)).
    pub fn start_ms(&self) -> Option<EpochMs> {
        let b = self.0.as_bytes();
        if b.len() != 13 {
            return None;
        }
        let y: i32 = self.0.get(0..4)?.parse().ok()?;
        let mo: u32 = self.0.get(5..7)?.parse().ok()?;
        let d: u32 = self.0.get(8..10)?.parse().ok()?;
        let h: u32 = self.0.get(11..13)?.parse().ok()?;
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0)
            .single()
            .map(|dt| dt.timestamp_millis())
    }

    /// `YYYY-MM` — `DR#<instance_id>#<yyyy-mm>` 파티션 키에 쓴다.
    pub fn year_month(&self) -> &str {
        &self.0[..7]
    }

    /// `YYYY-MM-DD`
    pub fn date_part(&self) -> DatePart {
        DatePart(self.0[..10].to_string())
    }
}

impl fmt::Display for HourBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `YYYY-MM-DD` (UTC).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DatePart(String);

impl DatePart {
    pub fn from_epoch_ms(ms: EpochMs) -> Self {
        let dt = to_utc(ms);
        Self(format!(
            "{:04}-{:02}-{:02}",
            dt.year(),
            dt.month(),
            dt.day()
        ))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 조회 기간. **항상 `from <= to`** 가 보장된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    from_ms: EpochMs,
    to_ms: EpochMs,
}

impl TimeRange {
    /// 뒤집힌 구간은 거부한다. `None` 을 무한 구간으로 해석하는 경로를 만들지 않는다
    /// (아카이브 쿼리가 전체 스캔이 되는 가장 흔한 원인).
    pub fn new(from_ms: EpochMs, to_ms: EpochMs) -> Option<Self> {
        (from_ms <= to_ms).then_some(Self { from_ms, to_ms })
    }
    pub fn from_ms(&self) -> EpochMs {
        self.from_ms
    }
    pub fn to_ms(&self) -> EpochMs {
        self.to_ms
    }
    pub fn duration_ms(&self) -> i64 {
        self.to_ms - self.from_ms
    }
    pub fn contains(&self, ms: EpochMs) -> bool {
        (self.from_ms..=self.to_ms).contains(&ms)
    }

    /// 구간이 걸치는 날짜 파티션 목록 (UTC). 조회 팬아웃 대상.
    pub fn date_parts(&self) -> Vec<DatePart> {
        const DAY_MS: i64 = 86_400_000;
        let start = self.from_ms.div_euclid(DAY_MS) * DAY_MS;
        let mut out = Vec::new();
        let mut t = start;
        while t <= self.to_ms {
            out.push(DatePart::from_epoch_ms(t));
            t += DAY_MS;
        }
        out
    }
}

fn to_utc(ms: EpochMs) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

/// 시각 공급자 포트. 테스트가 시간을 주입할 수 있어야 in-flight 상태 머신을
/// 테이블 주도로 검증할 수 있다 ([15 §](../../../docs/15-testing.md), R5).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> EpochMs;
}

/// 실제 시계.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> EpochMs {
        Utc::now().timestamp_millis()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-08-20T05:23:11.500Z
    const T: EpochMs = 1_787_203_391_500;

    #[test]
    fn hour_bucket_formats_and_roundtrips() {
        let h = HourBucket::from_epoch_ms(T);
        assert_eq!(h.as_str(), "2026-08-20T05");
        assert_eq!(h.year_month(), "2026-08");
        assert_eq!(h.date_part().as_str(), "2026-08-20");
        // start_ms 는 TTL 계산 기준이므로 정확해야 한다.
        let start = h.start_ms().unwrap();
        assert_eq!(HourBucket::from_epoch_ms(start), h);
        assert_eq!(start, 1_787_202_000_000);
    }

    #[test]
    fn hour_bucket_start_rejects_malformed() {
        assert!(HourBucket("2026-08-19".into()).start_ms().is_none());
        assert!(HourBucket("bogus--bogus".into()).start_ms().is_none());
        assert!(HourBucket("2026-13-20T05".into()).start_ms().is_none());
    }

    #[test]
    fn time_range_rejects_inverted() {
        assert!(TimeRange::new(100, 99).is_none());
        assert!(TimeRange::new(100, 100).is_some());
    }

    #[test]
    fn date_parts_cover_boundaries() {
        let start = DatePart::from_epoch_ms(T);
        let r = TimeRange::new(T, T + 86_400_000 * 2).unwrap();
        let parts = r.date_parts();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], start);
        assert_eq!(parts[2].as_str(), "2026-08-22");

        // 같은 날 안의 짧은 구간은 파티션 1개.
        let short = TimeRange::new(T, T + 1000).unwrap();
        assert_eq!(short.date_parts().len(), 1);
    }

    #[test]
    fn epoch_boundaries_do_not_panic() {
        for ms in [0, -1, i64::MIN, i64::MAX] {
            let _ = HourBucket::from_epoch_ms(ms);
            let _ = DatePart::from_epoch_ms(ms);
        }
    }

    #[test]
    fn fake_clock_is_injectable() {
        struct Fixed(EpochMs);
        impl Clock for Fixed {
            fn now_ms(&self) -> EpochMs {
                self.0
            }
        }
        let c: Box<dyn Clock> = Box::new(Fixed(T));
        assert_eq!(c.now_ms(), T);
    }
}
