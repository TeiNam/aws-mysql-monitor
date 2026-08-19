//! `global_status` 를 주기적으로 읽어 실시간 지표를 만든다.
//!
//! # 왜 별도 케이던스인가
//!
//! 탐지 tick 은 1초다(슬로우 쿼리 해상도). 지표는 5초로 충분하고
//! ([09 §3.3] "자체 수집 · 5초"), 매초 `SHOW GLOBAL STATUS` 를 돌리면 감시 대상
//! DB 에 쓸데없는 부하를 준다 — 우리가 관측하는 대상을 우리가 느리게 만들면 안 된다.
//!
//! # 실패해도 수집을 멈추지 않는다
//!
//! 지표 조회가 실패하면 그 샘플만 버린다. 슬로우 쿼리 탐지가 지표 때문에 멈추면
//! 우선순위가 뒤집힌다 — 탐지가 본업이다.

use std::time::Duration;

use dbmon_core::ports::TargetDb;
use dbmon_core::time::EpochMs;

use super::derive::{LiveMetrics, StatusSample, derive};

/// 지표 샘플 주기 ([09 §3.3]).
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// 직전 샘플을 들고 비율을 유도하는 샘플러.
///
/// **인스턴스마다 하나**여야 한다 — 공유하면 다른 인스턴스의 카운터를 뺀다.
#[derive(Debug, Default)]
pub struct MetricsSampler {
    prev: Option<StatusSample>,
    /// 마지막 성공 시각. **`0` 을 센티널로 쓰지 않는다** — `0` 은 유효한
    /// `EpochMs` 이고, 실제로 테스트가 `now_ms = 0` 에서 그 충돌을 잡았다.
    last_sampled_at_ms: Option<EpochMs>,
}

impl MetricsSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 지금 샘플링해야 하는가. 호출부가 매 tick 물어본다.
    pub fn is_due(&self, now_ms: EpochMs) -> bool {
        // 첫 호출은 항상 샘플링한다 — 그래야 화면이 기동 직후 게이지를 볼 수 있다.
        let Some(last) = self.last_sampled_at_ms else {
            return true;
        };
        // 시계가 거꾸로 가면 즉시 샘플링한다. 기다리면 보정 폭만큼 지표가 멈춘다.
        now_ms < last || now_ms - last >= SAMPLE_INTERVAL.as_millis() as i64
    }

    /// 한 번 샘플링한다. **`is_due` 를 통과했을 때만 부른다.**
    ///
    /// 실패하면 직전 샘플을 **유지한다** — 버리면 다음 성공이 "첫 샘플" 이 되어
    /// 비율이 한 번 더 비어 버린다.
    pub async fn sample<D: TargetDb>(
        &mut self,
        db: &D,
        now_ms: EpochMs,
    ) -> dbmon_core::Result<LiveMetrics> {
        let vars = db.global_status().await?;
        let cur = StatusSample::new(now_ms, vars);
        let metrics = derive(self.prev.as_ref(), &cur);
        self.prev = Some(cur);
        self.last_sampled_at_ms = Some(now_ms);
        Ok(metrics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::fakes::FakeTargetDb;

    #[test]
    fn the_first_sample_is_always_due() {
        assert!(MetricsSampler::new().is_due(0));
        assert!(MetricsSampler::new().is_due(1_787_000_000_000));
    }

    /// **주기보다 자주 샘플링하지 않는다.** 대상 DB 부하를 우리가 만들면 안 된다.
    #[tokio::test]
    async fn sampling_respects_the_interval() {
        let db = FakeTargetDb::new();
        db.set_status(&[("Queries", "100"), ("Uptime", "10")]);
        let mut s = MetricsSampler::new();

        s.sample(&db, 10_000).await.expect("샘플");
        assert!(!s.is_due(11_000), "1초 뒤에 또 샘플링하려 한다");
        assert!(!s.is_due(14_999));
        assert!(s.is_due(15_000), "5초가 지났는데 샘플링하지 않는다");
    }

    /// **실패가 직전 샘플을 지우지 않는다.**
    ///
    /// 지우면 다음 성공이 `first_sample` 이 되어 QPS 가 한 번 더 비어 버린다 —
    /// 일시적 연결 오류가 화면에 두 번 구멍을 낸다.
    #[tokio::test]
    async fn a_failed_sample_keeps_the_previous_one() {
        let db = FakeTargetDb::new();
        db.set_status(&[("Queries", "100"), ("Uptime", "10")]);
        let mut s = MetricsSampler::new();
        s.sample(&db, 10_000).await.expect("첫 샘플");
        assert!(s.prev.is_some());

        db.fail_global_status(1);
        assert!(s.sample(&db, 15_000).await.is_err());
        assert!(s.prev.is_some(), "실패가 직전 샘플을 지웠다");

        // 다음 성공은 비율을 낼 수 있어야 한다 (첫 샘플이 아니다).
        let m = s.sample(&db, 20_000).await.expect("복구");
        assert_ne!(m.rate_gap_reason, Some("first_sample"));
    }

    /// **시계가 거꾸로 가면 기다리지 않는다.** 기다리면 보정 폭(최대 수 초)만큼
    /// 지표가 멈추고, 화면에서는 그게 "DB 가 멈췄다" 로 보인다.
    #[tokio::test]
    async fn a_backwards_clock_triggers_an_immediate_sample() {
        let db = FakeTargetDb::new();
        db.set_status(&[("Queries", "100"), ("Uptime", "10")]);
        let mut s = MetricsSampler::new();
        s.sample(&db, 20_000).await.expect("샘플");
        assert!(s.is_due(19_000), "시계 역행 뒤 샘플링을 미뤘다");
    }

    /// 실패는 `last_sampled_at_ms` 도 올리지 않는다 — 다음 tick 에 즉시 재시도한다.
    #[tokio::test]
    async fn a_failed_sample_does_not_consume_the_interval() {
        let db = FakeTargetDb::new();
        let mut s = MetricsSampler::new();
        db.fail_global_status(1);
        assert!(s.sample(&db, 10_000).await.is_err());
        assert!(s.is_due(10_000), "실패한 샘플이 주기를 소모했다");
    }
}
