//! `global_status` 두 스냅샷에서 실시간 지표를 유도한다 (FR-MET-01, [06 §2.3]).
//!
//! # 왜 순수 함수인가
//!
//! `SHOW GLOBAL STATUS` 의 값 대부분은 **누적 카운터**다. 화면이 원하는 것은
//! QPS 처럼 **비율**이므로 두 시점의 차를 시간으로 나눠야 하고, 그 뺄셈이
//! 위험하다:
//!
//! - 서버가 재시작하면 카운터가 0 으로 돌아간다 → 음수 델타 → 거대한 음수 QPS
//! - `u64` 뺄셈이 언더플로하면 천문학적 QPS 가 나온다
//! - 샘플 간격이 0 이면 0 으로 나눈다
//!
//! 이 셋 다 **AWS 없이 검증할 수 있는 판정**이다. 그래서 유도를 여기 순수 함수로
//! 두고 어댑터는 값을 읽어 오는 일만 한다.
//!
//! # 재시작을 어떻게 아는가
//!
//! `Uptime` 이 감소하면 재시작이다 — 이게 가장 직접적인 신호다. 개별 카운터
//! 비교만으로 판정하면 `FLUSH STATUS` 처럼 일부만 초기화되는 경우를 놓친다.
//! 그래서 **`Uptime` 감소 또는 카운터 감소** 중 하나라도 있으면 비율을 내지 않는다.
//! 비율 없이 게이지만 주는 편이, 틀린 비율을 주는 것보다 낫다.

use std::collections::BTreeMap;

use dbmon_core::time::EpochMs;
use serde::Serialize;

/// `global_status` 원본 스냅샷 한 장.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSample {
    pub at_ms: EpochMs,
    /// 변수명 → 값. MySQL 은 대소문자를 섞어 주므로 조회는 [`Self::counter`] 로 한다.
    pub vars: BTreeMap<String, String>,
}

impl StatusSample {
    pub fn new(at_ms: EpochMs, vars: BTreeMap<String, String>) -> Self {
        Self { at_ms, vars }
    }

    /// 정수 변수를 읽는다. **대소문자를 구분하지 않는다** — MySQL 8.0 은
    /// `Threads_running`, 일부 도구는 `THREADS_RUNNING` 으로 준다.
    pub fn counter(&self, name: &str) -> Option<u64> {
        self.vars
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.trim().parse::<u64>().ok())
    }
}

/// 화면이 쓰는 실시간 지표 한 점.
///
/// 비율 항목이 `Option` 인 이유: **첫 샘플이거나 재시작 직후에는 비율이 없다.**
/// `0.0` 으로 채우면 "쿼리가 없다" 로 읽히는데 그건 사실이 아니다.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LiveMetrics {
    pub at_ms: EpochMs,
    /// 초당 질의 수. 첫 샘플·재시작 직후에는 `None`.
    pub qps: Option<f64>,
    /// 초당 슬로우 쿼리 수.
    pub slow_per_sec: Option<f64>,
    /// 실행 중 스레드 (게이지) — 항상 있다.
    pub threads_running: Option<u64>,
    pub threads_connected: Option<u64>,
    /// 연결 수의 **모수** (`@@max_connections`).
    ///
    /// 이게 없으면 `threads_connected` 로 포화를 판정할 수 없다 — `10` 이 정상인지
    /// 위험인지는 모수에 달렸다. CloudWatch 에는 이 값의 메트릭이 없어서 자체 수집으로
    /// 읽는다(`sql::GLOBAL_STATUS` 의 UNION).
    pub max_connections: Option<u64>,
    /// InnoDB 행 락 대기 중 (게이지).
    pub lock_waits: Option<u64>,
    /// 비율을 내지 못한 이유. 화면이 "—" 와 "0" 을 구분하게 한다.
    pub rate_gap_reason: Option<&'static str>,
}

/// 비율을 유도한다.
///
/// `prev` 가 `None` 이면 첫 샘플이므로 게이지만 채운다.
pub fn derive(prev: Option<&StatusSample>, cur: &StatusSample) -> LiveMetrics {
    let gauges = LiveMetrics {
        at_ms: cur.at_ms,
        qps: None,
        slow_per_sec: None,
        threads_running: cur.counter("Threads_running"),
        threads_connected: cur.counter("Threads_connected"),
        // 변수도 같은 맵에 담겨 온다(`sql::GLOBAL_STATUS` 의 UNION).
        max_connections: cur.counter("max_connections"),
        lock_waits: cur.counter("Innodb_row_lock_current_waits"),
        rate_gap_reason: None,
    };

    let Some(prev) = prev else {
        return LiveMetrics {
            rate_gap_reason: Some("first_sample"),
            ..gauges
        };
    };

    // **간격이 0 이거나 거꾸로 가면 나누지 않는다.** 시계 역행은 실제로 일어난다
    // (NTP 보정). 나누면 부호가 뒤집힌 값이 화면에 그려진다.
    let elapsed_ms = cur.at_ms - prev.at_ms;
    if elapsed_ms <= 0 {
        return LiveMetrics {
            rate_gap_reason: Some("non_monotonic_clock"),
            ..gauges
        };
    }

    if restarted(prev, cur) {
        return LiveMetrics {
            rate_gap_reason: Some("counters_reset"),
            ..gauges
        };
    }

    let per_sec = |name: &str| rate(prev, cur, name, elapsed_ms);
    LiveMetrics {
        qps: per_sec("Queries"),
        slow_per_sec: per_sec("Slow_queries"),
        ..gauges
    }
}

/// 카운터가 초기화됐는가.
///
/// `Uptime` 감소가 재시작이다. 그것만 보면 `FLUSH STATUS` 를 놓치므로 주요
/// 카운터의 감소도 함께 본다.
fn restarted(prev: &StatusSample, cur: &StatusSample) -> bool {
    if let (Some(p), Some(c)) = (prev.counter("Uptime"), cur.counter("Uptime"))
        && c < p
    {
        return true;
    }
    ["Queries", "Slow_queries"].iter().any(|name| {
        matches!(
            (prev.counter(name), cur.counter(name)),
            (Some(p), Some(c)) if c < p
        )
    })
}

/// 누적 카운터의 초당 비율. **`checked_sub` 로 언더플로를 막는다.**
fn rate(prev: &StatusSample, cur: &StatusSample, name: &str, elapsed_ms: i64) -> Option<f64> {
    let delta = cur.counter(name)?.checked_sub(prev.counter(name)?)?;
    Some(delta as f64 * 1000.0 / elapsed_ms as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at_ms: EpochMs, pairs: &[(&str, &str)]) -> StatusSample {
        StatusSample::new(
            at_ms,
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn pair(
        t0: EpochMs,
        v0: &[(&str, &str)],
        t1: EpochMs,
        v1: &[(&str, &str)],
    ) -> (StatusSample, StatusSample) {
        (sample(t0, v0), sample(t1, v1))
    }

    #[test]
    fn qps_is_the_counter_delta_over_elapsed_seconds() {
        let (a, b) = pair(
            1_000,
            &[("Queries", "1000"), ("Uptime", "100")],
            6_000,
            &[("Queries", "2500"), ("Uptime", "105")],
        );
        let m = derive(Some(&a), &b);
        // 1500 질의 / 5초 = 300 QPS
        assert_eq!(m.qps, Some(300.0));
        assert_eq!(m.rate_gap_reason, None);
    }

    /// **첫 샘플에는 비율이 없다.** `0.0` 을 주면 "쿼리가 없다" 로 오해된다.
    #[test]
    fn the_first_sample_has_gauges_but_no_rates() {
        let s = sample(1_000, &[("Queries", "1000"), ("Threads_running", "7")]);
        let m = derive(None, &s);
        assert_eq!(m.qps, None);
        assert_eq!(m.threads_running, Some(7));
        assert_eq!(m.rate_gap_reason, Some("first_sample"));
    }

    /// **재시작을 비율로 착각하지 않는다.**
    ///
    /// 이게 없으면 `u64` 뺄셈이 언더플로해 천문학적 QPS 가 화면에 그려진다.
    #[test]
    fn a_restart_does_not_produce_a_rate() {
        let (a, b) = pair(
            1_000,
            &[("Queries", "1000000"), ("Uptime", "86400")],
            6_000,
            &[("Queries", "12"), ("Uptime", "3")],
        );
        let m = derive(Some(&a), &b);
        assert_eq!(m.qps, None, "재시작 후 비율이 나왔다");
        assert_eq!(m.rate_gap_reason, Some("counters_reset"));
    }

    /// `FLUSH STATUS` 는 `Uptime` 을 건드리지 않지만 카운터를 되돌린다.
    #[test]
    fn flush_status_is_also_treated_as_a_reset() {
        let (a, b) = pair(
            1_000,
            &[("Queries", "1000000"), ("Uptime", "86400")],
            6_000,
            &[("Queries", "5"), ("Uptime", "86405")],
        );
        assert_eq!(derive(Some(&a), &b).rate_gap_reason, Some("counters_reset"));
    }

    /// **시계가 거꾸로 가도 나누지 않는다.** NTP 보정으로 실제로 일어난다.
    #[test]
    fn a_backwards_clock_does_not_produce_a_rate() {
        for (t0, t1) in [(6_000, 1_000), (1_000, 1_000)] {
            let (a, b) = pair(t0, &[("Queries", "10")], t1, &[("Queries", "20")]);
            let m = derive(Some(&a), &b);
            assert_eq!(m.qps, None, "t0={t0} t1={t1} 에서 비율이 나왔다");
            assert_eq!(m.rate_gap_reason, Some("non_monotonic_clock"));
        }
    }

    /// 변수명 대소문자에 의존하지 않는다.
    #[test]
    fn variable_lookup_is_case_insensitive() {
        let s = sample(1_000, &[("THREADS_RUNNING", "3"), ("queries", "9")]);
        assert_eq!(s.counter("Threads_running"), Some(3));
        assert_eq!(s.counter("Queries"), Some(9));
    }

    /// 없는 변수·깨진 값에 패닉하지 않는다.
    #[test]
    fn missing_or_malformed_values_are_absent_not_zero() {
        let (a, b) = pair(
            1_000,
            &[("Queries", "10")],
            6_000,
            &[("Queries", "not-a-number"), ("Threads_running", "")],
        );
        let m = derive(Some(&a), &b);
        assert_eq!(m.qps, None);
        assert_eq!(m.threads_running, None);
        // 값이 깨진 것은 재시작이 아니므로 사유를 붙이지 않는다.
        assert_eq!(m.rate_gap_reason, None);
    }

    /// 음수 문자열도 `u64` 파싱 실패로 흡수한다 — 게이지가 음수가 될 수 없다.
    #[test]
    fn negative_strings_do_not_parse() {
        let s = sample(1_000, &[("Threads_running", "-1")]);
        assert_eq!(s.counter("Threads_running"), None);
    }
}
