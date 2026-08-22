//! 저장과 방송을 묶는 데코레이터.
//!
//! # 왜 호출부마다 방송을 붙이지 않는가
//!
//! 수집기는 세 곳에서 저장한다 — 선행 저장(`in_flight`), 확정, 고아 정리.
//! 각 호출부에 `hub.publish(...)` 를 넣으면 **빠뜨릴 기회가 셋** 생기고, 새
//! 저장 경로가 추가될 때마다 하나 더 늘어난다. 이 프로젝트에서 "구현은 있고
//! 호출부가 없다" 가 아홉 번 반복된 실패 유형이다.
//!
//! 그래서 저장소를 감싼다. **저장된 것은 반드시 방송된다** — 구조가 보장한다.
//!
//! # 실패는 방송하지 않는다
//!
//! `upsert_merged` 가 실패하면 방송도 하지 않는다. 저장되지 않은 레코드를
//! 화면에 띄우면 새로고침 때 사라진다 — 사용자에게는 그게 버그로 보인다.
//!
//! # 방송하는 값은 **병합 결과**다
//!
//! `upsert_merged` 는 병합된 레코드를 돌려준다. 입력을 방송하면 실시간 캡처의
//! 부분 정보(예: `rows_examined` 없음)가 나가고, 슬로우로그 백필이 채운 값이
//! 화면에 반영되지 않는다.

use std::sync::Arc;

use async_trait::async_trait;
use dbmon_core::Result;
use dbmon_core::ids::{InstanceId, RecordId};
use dbmon_core::ports::SlowQueryStore;
use dbmon_core::slow_query::SlowQuery;
use dbmon_core::time::{EpochMs, TimeRange};

use crate::api::hub::Hub;

/// 저장 성공 시 [`Hub`] 로 방송하는 래퍼.
pub struct BroadcastingStore<S> {
    /// `Arc` 를 안에 둔다 — 감싸기 전의 저장소를 다른 곳과 공유해야 하고,
    /// 래퍼 자체도 `Arc` 로 공유되기 때문이다.
    inner: Arc<S>,
    hub: Hub,
}

impl<S> BroadcastingStore<S> {
    pub fn new(inner: Arc<S>, hub: Hub) -> Self {
        Self { inner, hub }
    }

    /// 포트에 없는 메서드(`probe` 등)에 닿기 위한 통로.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

#[async_trait]
impl<S: SlowQueryStore> SlowQueryStore for BroadcastingStore<S> {
    async fn upsert_merged(&self, q: &SlowQuery) -> Result<SlowQuery> {
        let merged = self.inner.upsert_merged(q).await?;
        // 병합 결과를 방송한다 — 입력이 아니라.
        self.hub.publish_slow_query(&merged);
        Ok(merged)
    }

    async fn get(&self, id: &RecordId) -> Result<Option<SlowQuery>> {
        self.inner.get(id).await
    }

    async fn find_merge_candidate(
        &self,
        instance: &InstanceId,
        thread_id: u64,
        app_digest: &str,
        around_ms: EpochMs,
        window_ms: i64,
    ) -> Result<Option<SlowQuery>> {
        self.inner
            .find_merge_candidate(instance, thread_id, app_digest, around_ms, window_ms)
            .await
    }

    async fn list_by_instance(
        &self,
        instance: &InstanceId,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<SlowQuery>> {
        self.inner.list_by_instance(instance, range, limit).await
    }

    async fn list_in_flight(&self, limit: usize) -> Result<Vec<SlowQuery>> {
        self.inner.list_in_flight(limit).await
    }

    /// **방송하지 않는다.** 하트비트는 새 사실이 아니다 — 이미 화면에 있는 레코드가
    /// 아직 돌고 있다는 것뿐이고, 경과 시간은 클라이언트가 센다. 방송하면 브라우저가
    /// 15초마다 목록 전체를 무효화한다(교차 리뷰 23라운드).
    async fn touch_in_flight(
        &self,
        instance: &InstanceId,
        thread_id: u64,
        started_at_ms: EpochMs,
        last_seen_at_ms: EpochMs,
    ) -> Result<bool> {
        self.inner
            .touch_in_flight(instance, thread_id, started_at_ms, last_seen_at_ms)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::Env;
    use dbmon_core::fakes::FakeSlowQueryStore;

    /// **저장하면 방송된다.** 호출부가 방송을 잊을 수 없다는 것이 이 래퍼의 전부다.
    #[tokio::test]
    async fn a_stored_record_is_broadcast() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_slow_queries();
        let store = BroadcastingStore::new(Arc::new(FakeSlowQueryStore::new()), hub);

        let mut q = crate::store::tests::sample();
        q.env = Env::Stg;
        store.upsert_merged(&q).await.expect("저장");

        let got = rx.recv().await.expect("방송");
        assert_eq!(got.key, "slowq:env=stg");
        assert_eq!(got.data.record_id, q.record_id.as_str());
    }

    /// **저장이 실패하면 방송하지 않는다.**
    ///
    /// 저장되지 않은 레코드를 띄우면 새로고침 때 사라진다.
    #[tokio::test]
    async fn a_failed_store_is_not_broadcast() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_slow_queries();
        let store = BroadcastingStore::new(Arc::new(FakeSlowQueryStore::new()), hub);
        store.inner().fail_next(1);

        assert!(
            store
                .upsert_merged(&crate::store::tests::sample())
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err(), "실패한 저장이 방송됐다");
    }

    /// 읽기 경로는 방송하지 않는다 — 조회가 스트림을 만들면 안 된다.
    #[tokio::test]
    async fn reads_do_not_broadcast() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_slow_queries();
        let store = BroadcastingStore::new(Arc::new(FakeSlowQueryStore::new()), hub);

        let q = crate::store::tests::sample();
        let _ = store.get(&q.record_id).await;
        let _ = store.list_in_flight(10).await;
        assert!(rx.try_recv().is_err(), "조회가 방송됐다");
    }
}
