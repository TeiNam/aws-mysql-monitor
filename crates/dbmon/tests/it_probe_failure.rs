//! 심층 조회가 **실패했을 때** 수집 루프가 무엇을 잃는지 검사한다.
//!
//! 2차 리뷰가 잡은 결함: 1차의 "실패를 삼키지 않는다" 수정이 `return Ok(stats)` 였고,
//! `tracker.tick()` 은 이미 캐시에서 엔트리를 제거해 `tick.finalized` 로 넘긴 상태였다.
//! 조기 반환이 그 Vec 을 버리므로 **확정이 영구히 사라졌다** — 1차 수정이 수정 전보다
//! 나빴다.
//!
//! Docker 가 필요 없다. `FakeTargetDb` 로 권한 오류·타임아웃을 원하는 순간에 만든다.

use std::sync::Arc;

use dbmon::collector::{CollectParams, InstanceCollector};
use dbmon_core::env::{Env, EnvResolution};
use dbmon_core::fakes::{FakeClock, FakeSlowQueryStore, FakeTargetDb};
use dbmon_core::ids::InstanceId;
use dbmon_core::instance::{Engine, EngineVersion, Instance, InstanceState};
use dbmon_core::slow_query::{LiteralPolicy, SlowQueryState};

const ACCOUNT: &str = "123456789012";

fn instance(name: &str) -> Instance {
    Instance {
        id: InstanceId::new(ACCOUNT, "ap-northeast-2", name).unwrap(),
        cluster_id: None,
        engine: Engine::Mysql,
        engine_version: EngineVersion::parse("8.4.11").unwrap(),
        env: EnvResolution::resolve(Env::Dev, None),
        state: InstanceState::Collecting,
        endpoint: Some("127.0.0.1".into()),
        port: 3306,
        dbi_resource_id: "db-LOCAL".into(),
        vpc_id: None,
        availability_zone: None,
        instance_class: None,
        is_cluster_writer: true,
        iam_auth_enabled: false,
        tags: Default::default(),
        cert_valid_till_ms: None,
        first_seen_ms: 0,
        last_seen_ms: 0,
        deleted_at_ms: None,
        missing_count: 0,
        renamed_from: None,
        renamed_to: None,
    }
}

fn params() -> CollectParams {
    CollectParams {
        slow_threshold_secs: 2,
        literal_policy: LiteralPolicy::Masked,
        monitor_db_user: "dbmon".into(),
        worker_id: "w1".into(),
        ..Default::default()
    }
}

/// **확정은 심층 조회 실패와 무관하게 저장돼야 한다.**
///
/// 시나리오:
/// 1. tick1 — 스레드 1·2 관측 → 선행 저장
/// 2. tick2 — 스레드 1 사라짐 (확정 대상) + `full_sql` 이 1142 로 실패
/// 3. 확정이 저장돼야 한다. 잃으면 `duration_ms`·`ended_at_ms` 가 영구 유실된다.
#[tokio::test]
async fn deep_probe_failure_does_not_lose_finalizations() {
    let db = FakeTargetDb::new().with_threads(&[(1, 3), (2, 4)]);
    let store = Arc::new(FakeSlowQueryStore::default());
    let clock = FakeClock::new(1_755_500_400_000);
    let mut c = InstanceCollector::new(
        instance("orders-prd-01"),
        db,
        store.clone(),
        clock.clone(),
        params(),
    );

    let t1 = c.detect_tick().await.expect("tick1");
    assert_eq!(t1.candidates, 2, "두 스레드가 관측돼야 한다");

    // 스레드 1 이 끝났다. 동시에 `full_sql` 이 권한 오류로 실패한다.
    c.db_mut().with_threads_mut(&[(2, 5)]);
    c.db_mut().fail_full_sql(1);
    clock.advance(1_000);

    let t2 = c
        .detect_tick()
        .await
        .expect("tick2 는 Err 가 아니어야 한다");

    assert_eq!(t2.deep_probe_failed, 1, "실패를 세야 한다");
    assert_eq!(
        t2.finalized, 1,
        "심층 조회가 실패해도 확정은 저장돼야 한다 — 확정 경로는 그 조회를 쓰지 않는다"
    );

    // 저장된 레코드가 실제로 확정 상태인지 확인한다.
    let saved = store.all();
    let thread1 = saved
        .iter()
        .find(|q| q.thread_id == 1)
        .expect("스레드 1 레코드");
    assert_eq!(
        thread1.state,
        SlowQueryState::Finalized,
        "스레드 1 이 in_flight 고아로 남았다"
    );
    assert!(
        thread1.ended_at_ms.is_some(),
        "종료 시각이 유실됐다 — 다시는 얻을 수 없다"
    );
    assert!(
        thread1.duration_ms >= 3_000,
        "실행시간이 유실됐다 (실제 {}ms)",
        thread1.duration_ms
    );
}

/// `stmt_current` 실패도 같아야 한다.
#[tokio::test]
async fn stmt_current_failure_does_not_lose_finalizations() {
    let db = FakeTargetDb::new().with_threads(&[(7, 9)]);
    let store = Arc::new(FakeSlowQueryStore::default());
    let clock = FakeClock::new(1_755_500_400_000);
    let mut c = InstanceCollector::new(
        instance("orders-prd-01"),
        db,
        store.clone(),
        clock.clone(),
        params(),
    );

    c.detect_tick().await.expect("tick1");
    c.db_mut().with_threads_mut(&[]);
    c.db_mut().fail_stmt_current(1);
    clock.advance(1_000);

    let t2 = c.detect_tick().await.expect("tick2");
    assert_eq!(t2.finalized, 1, "확정을 잃었다");
}

/// 심층 조회가 계속 실패해도 tick 은 계속 돌아야 한다 (서킷이 잘못 열리면 안 된다).
#[tokio::test]
async fn repeated_probe_failure_keeps_the_loop_alive() {
    let db = FakeTargetDb::new().with_threads(&[(1, 3)]);
    let store = Arc::new(FakeSlowQueryStore::default());
    let clock = FakeClock::new(1_755_500_400_000);
    let mut c = InstanceCollector::new(
        instance("orders-prd-01"),
        db,
        store,
        clock.clone(),
        params(),
    );

    c.db_mut().fail_full_sql(5);
    for i in 0..5 {
        let t = c
            .detect_tick()
            .await
            .unwrap_or_else(|e| panic!("tick{i} 이 Err 를 반환했다: {e}"));
        assert_eq!(t.deep_probe_failed, 1, "tick{i}");
        assert_eq!(t.prefetch_saved, 0, "tick{i} — 선행 저장은 건너뛴다");
        clock.advance(1_000);
    }
    // 복구되면 다시 저장된다.
    let t = c.detect_tick().await.expect("복구 tick");
    assert_eq!(t.deep_probe_failed, 0);
}
