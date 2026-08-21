//! 방송 허브 ([09 §4](../../../../docs/09-frontend.md)).
//!
//! # 두 채널로 나누는 이유 — 백프레셔 정책이 다르다
//!
//! 규정은 "송신 큐가 넘치면 오래된 `status` 를 버리고 `slowq`·`alert` 는 버리지
//! 않는다" 다. 한 채널이면 그 구분이 불가능하다 — `broadcast` 는 종류를 모르고
//! 오래된 것부터 버린다. 그래서 채널을 둘 둔다:
//!
//! | 채널 | 용량 | 밀렸을 때 |
//! |---|---|---|
//! | `slowq` | 1000 | **버렸다는 사실을 알린다** (`stream_lagged`) → 클라이언트가 재조회 |
//! | `status` | 64 | 조용히 버린다. 다음 샘플이 5초 뒤에 온다 |
//!
//! `status` 를 조용히 버려도 되는 근거: 게이지의 최신값만 의미가 있고 5초 뒤
//! 갱신된다. `slowq` 는 하나가 사라지면 그 쿼리가 화면에 영구히 안 나타난다 —
//! 그래서 조용히 버리지 않고 **재조회를 유발한다.**

use tokio::sync::broadcast;

use super::view::SlowQueryBroadcast;
use crate::metrics::LiveMetrics;

/// `slowq` 채널 용량. 넘치면 클라이언트에 알린다.
const SLOWQ_CAPACITY: usize = 1000;
/// `status` 채널 용량. 넘치면 조용히 버린다 — 다음 샘플이 곧 온다.
const STATUS_CAPACITY: usize = 64;

/// 방송 한 건. `key` 는 [`super::topic::Topic::key`] 와 같은 문자열이다.
#[derive(Debug, Clone)]
pub struct Broadcast<T> {
    pub key: String,
    pub data: T,
}

/// 구독자에게 보낼 것을 모으는 허브.
///
/// **`Clone` 이 싸다** — 내부가 `broadcast::Sender` 뿐이다. 수집 태스크와 API
/// 상태가 각자 들고 있어도 된다.
#[derive(Clone)]
pub struct Hub {
    slowq: broadcast::Sender<Broadcast<SlowQueryBroadcast>>,
    status: broadcast::Sender<Broadcast<LiveMetrics>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        Self {
            slowq: broadcast::channel(SLOWQ_CAPACITY).0,
            status: broadcast::channel(STATUS_CAPACITY).0,
        }
    }

    /// 슬로우 쿼리를 방송한다.
    ///
    /// **구독자가 없으면 조용히 버린다.** `broadcast::send` 가 `Err` 를 주지만
    /// 그건 오류가 아니다 — 아무도 안 보고 있는 것이다. 여기서 경고를 찍으면
    /// 화면을 안 켠 동안 로그가 가득 찬다.
    pub fn publish_slow_query(&self, q: &dbmon_core::slow_query::SlowQuery) {
        let data = SlowQueryBroadcast::from_record(q);
        let key = super::topic::Topic::SlowQueries(q.env).key();
        let _ = self.slowq.send(Broadcast { key, data });
    }

    pub fn publish_status(&self, instance_id: &str, metrics: LiveMetrics) {
        let key = super::topic::Topic::InstanceStatus(instance_id.to_string()).key();
        let _ = self.status.send(Broadcast { key, data: metrics });
    }

    pub fn subscribe_slow_queries(&self) -> broadcast::Receiver<Broadcast<SlowQueryBroadcast>> {
        self.slowq.subscribe()
    }

    pub fn subscribe_status(&self) -> broadcast::Receiver<Broadcast<LiveMetrics>> {
        self.status.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::Env;

    #[tokio::test]
    async fn a_published_slow_query_reaches_a_subscriber_with_the_right_key() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_slow_queries();

        let mut q = crate::store::tests::sample();
        q.env = Env::Prd;
        hub.publish_slow_query(&q);

        let got = rx.recv().await.expect("수신");
        assert_eq!(got.key, "slowq:env=prd");
        assert_eq!(got.data.record_id, q.record_id.as_str());
    }

    /// **구독자가 없어도 패닉하거나 실패하지 않는다.**
    ///
    /// 화면을 아무도 안 켠 상태가 정상이다. 여기서 오류를 내면 수집 경로가
    /// 화면 유무에 의존하게 된다.
    #[tokio::test]
    async fn publishing_without_subscribers_is_not_an_error() {
        let hub = Hub::new();
        hub.publish_slow_query(&crate::store::tests::sample());
        hub.publish_status("acct/region/inst", zero_metrics());
    }

    /// `status` 채널은 밀리면 오래된 것을 버린다 — 그게 의도다.
    #[tokio::test]
    async fn status_drops_oldest_when_the_subscriber_lags() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_status();
        for _ in 0..(STATUS_CAPACITY + 10) {
            hub.publish_status("acct/region/inst", zero_metrics());
        }
        // 첫 수신은 `Lagged` 다. 그 뒤로는 최신 것들이 온다.
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        assert!(rx.recv().await.is_ok(), "밀린 뒤에도 계속 받아야 한다");
    }

    /// 두 채널이 **서로 간섭하지 않는다** — `status` 폭주가 `slowq` 를 밀어내면
    /// 백프레셔 정책이 무의미해진다.
    #[tokio::test]
    async fn a_status_flood_does_not_evict_slow_queries() {
        let hub = Hub::new();
        let mut slowq_rx = hub.subscribe_slow_queries();
        let status_rx = hub.subscribe_status();

        hub.publish_slow_query(&crate::store::tests::sample());
        for _ in 0..1_000 {
            hub.publish_status("acct/region/inst", zero_metrics());
        }
        drop(status_rx);

        assert!(slowq_rx.recv().await.is_ok(), "slowq 가 밀려났다");
    }

    fn zero_metrics() -> LiveMetrics {
        LiveMetrics {
            at_ms: 0,
            qps: None,
            slow_per_sec: None,
            threads_running: None,
            threads_connected: None,
            max_connections: None,
            lock_waits: None,
            rate_gap_reason: None,
        }
    }
}
