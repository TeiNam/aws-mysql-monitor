//! 시각 타입. **모든 시각은 UTC epoch millis 로 저장한다**
//! ([04 §0](../../../.claude/docs/04-data-model.md)).
//!
//! 1세대는 KST 문자열을 저장해서 집계가 어려웠다. 표시 시각 변환은 프론트엔드에서만 한다.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// UTC epoch milliseconds.
pub type EpochMs = i64;

/// epoch 밀리초를 **정렬 키에 쓸 수 있는 고정 폭 문자열**로 만든다.
///
/// # DynamoDB 의 `S` 정렬 키는 사전순이다
///
/// 숫자를 그대로 넣으면 자릿수가 다를 때 순서가 뒤집힌다:
///
/// ```text
/// 사전순:  "1000" < "999"      ← 틀렸다
/// 숫자순:  1000   > 999
/// ```
///
/// epoch 밀리초는 2286년까지 13자리이므로 **프로덕션 값끼리는 우연히 맞는다.** 그래서
/// 이 결함은 테스트나 백필에서만 드러나고, `SK 범위 조회`(AP-1)와 고아 스윕
/// (`GSI1SK < 임계`, AP-18)이 조용히 잘못된 집합을 반환한다.
///
/// 음수(1970 이전)는 실무에 없지만 `i64` 이므로 표현 가능하다. 부호가 붙으면 사전순이
/// 완전히 깨지므로 **0 으로 클램프**한다 — 그런 값이 오면 데이터가 이미 잘못됐다.
pub fn sort_key_ms(ms: EpochMs) -> String {
    format!("{:013}", ms.max(0))
}

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
    /// (`ttl = hour 시작 + 35일`, [04 §2.3](../../../.claude/docs/04-data-model.md)).
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
    ///
    /// 형식이 깨져 있으면 전체를 반환한다. **패닉하지 않는다** — 이 값은
    /// DynamoDB 에서 역직렬화되므로 손상된 항목 하나로 프로세스가 죽으면 안 된다.
    pub fn year_month(&self) -> &str {
        self.0.get(..7).unwrap_or(&self.0)
    }

    /// `YYYY-MM-DD`
    pub fn date_part(&self) -> DatePart {
        DatePart(self.0.get(..10).unwrap_or(&self.0).to_string())
    }

    /// 형식이 올바른가. `start_ms()` 가 성공하는 것과 같은 조건이다.
    pub fn is_valid(&self) -> bool {
        self.start_ms().is_some()
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

/// `date_parts()` 가 만들 수 있는 최대 파티션 수. 약 27년이다 —
/// 이보다 긴 조회는 아카이브 설계(400일 티어)에 존재하지 않는다.
pub const MAX_DATE_PARTS: usize = 10_000;

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
        // **상한을 둔다.** 이 구간은 API 질의 파라미터에서 오고 `TimeRange::new` 는
        // 길이를 제한하지 않는다. `to_ms = i64::MAX` 면 `t += DAY_MS` 가 오버플로하고
        // Vec 은 OOM 까지 자란다.
        while t <= self.to_ms && out.len() < MAX_DATE_PARTS {
            out.push(DatePart::from_epoch_ms(t));
            match t.checked_add(DAY_MS) {
                Some(next) => t = next,
                None => break,
            }
        }
        out
    }
}

fn to_utc(ms: EpochMs) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

/// 시각 공급자 포트. 테스트가 시간을 주입할 수 있어야 in-flight 상태 머신을
/// 테이블 주도로 검증할 수 있다 ([15 §](../../../.claude/docs/15-testing.md), R5).
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

    #[test]
    fn sort_key_ms_is_lexicographically_ordered() {
        // 이 테스트의 요점: **작은 값과 큰 값을 섞어도** 사전순 = 숫자순이어야 한다.
        let mut pairs: Vec<(EpochMs, String)> = vec![999, 1_000, 10_000, 1_755_500_400_000, 0, 42]
            .into_iter()
            .map(|ms| (ms, sort_key_ms(ms)))
            .collect();

        let mut by_number = pairs.clone();
        by_number.sort_by_key(|(ms, _)| *ms);
        pairs.sort_by(|a, b| a.1.cmp(&b.1));

        assert_eq!(
            pairs.iter().map(|(ms, _)| *ms).collect::<Vec<_>>(),
            by_number.iter().map(|(ms, _)| *ms).collect::<Vec<_>>(),
            "사전순 정렬이 숫자순과 달라졌다"
        );
        // 패딩하지 않으면 이 단정이 깨진다.
        assert!("1000" < "999", "전제 확인: 패딩 없는 사전순은 뒤집힌다");
        assert!(sort_key_ms(1_000) > sort_key_ms(999), "패딩하면 바로잡힌다");
        assert_eq!(sort_key_ms(1_755_500_400_000), "1755500400000");
        assert_eq!(sort_key_ms(42), "0000000000042");
        // 음수는 부호가 사전순을 깨뜨리므로 0 으로 클램프한다.
        assert_eq!(sort_key_ms(-5), "0000000000000");
    }

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
