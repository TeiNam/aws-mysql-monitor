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
        allocated_storage_gb: None,
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

/// **연결 수립은 tick 밖에서 일어나야 한다.**
///
/// `detect_total`(tick 예산 800ms) 이 `connect`(5초) 보다 작은 것이 정당한 근거는
/// "연결 수립은 `warm()` 이 tick 밖에서 한다" 는 것이다. 그 근거가 성립하려면
/// **연결 실패가 실제로 warm 을 다시 트리거해야** 한다.
///
/// 이전 판은 `fail_full_sql` 로 검증했는데, 연결 문제는 `probe` 에서 드러난다 —
/// 즉 이름이 약속한 보장을 테스트가 전혀 건드리지 않았다 (vacuous).
#[tokio::test]
async fn probe_failure_retriggers_warm() {
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

    // 기동 시 한 번 warm 한다.
    c.warm_if_needed().await;
    assert_eq!(c.db().warm_calls(), 1, "기동 warm 이 실행돼야 한다");

    // 연결이 끊겼다. probe 가 획득 타임아웃으로 5회 실패한다.
    c.db_mut().fail_probe(5);
    for i in 0..5 {
        assert!(
            c.detect_tick().await.is_err(),
            "tick{i}: probe 실패는 Err 로 올라와야 한다"
        );
        c.warm_if_needed().await;
        clock.advance(1_000);
    }

    assert!(
        c.db().warm_calls() > 1,
        "probe 실패가 warm 을 다시 트리거하지 않았다 — 콜드 풀이 영구히 회복되지 않는다"
    );

    // 복구되면 tick 이 다시 성공한다.
    let t = c.detect_tick().await.expect("복구 tick");
    assert_eq!(t.candidates, 1);
}

/// 심층 조회 실패도 warm 을 표시한다 (다른 경로, 같은 보장).
#[tokio::test]
async fn deep_probe_failure_also_marks_warm() {
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
    c.warm_if_needed().await;
    let before = c.db().warm_calls();

    c.db_mut().fail_full_sql(1);
    c.detect_tick().await.expect("tick");
    c.warm_if_needed().await;

    assert!(
        c.db().warm_calls() > before,
        "심층 조회 실패가 warm 을 표시하지 않았다"
    );
}

/// **어휘 발산이 있으면 플랜을 정확하다고 표시하면 안 된다.**
///
/// 우리 세션은 `sql_mode=''` 로 고정하는데(주입 방어) SQL 은 앱 세션에서 온다.
/// 앱이 `ANSI_QUOTES` 로 돌면 `"status"` 를 식별자로 보고 우리는 문자열로 본다 —
/// 8.4.11 실측으로 같은 쿼리가 15,000행 vs 0행이었다. 그 상태의 플랜은
/// `Zero rows (Impossible WHERE)` 이고, 그걸 정확한 플랜으로 저장하면 운영자는
/// **존재하지 않는 쿼리의 플랜**을 본다.
#[tokio::test]
async fn lexical_divergence_downgrades_plan_exactness() {
    use dbmon_core::ports::target_db::ExplainOutcome;
    use dbmon_core::slow_query::PlanSource;

    for (mode, expect) in [
        ("", PlanSource::Rerun),
        ("ANSI_QUOTES", PlanSource::RerunAsSelect),
        (
            "NO_BACKSLASH_ESCAPES,STRICT_TRANS_TABLES",
            PlanSource::RerunAsSelect,
        ),
        ("ONLY_FULL_GROUP_BY", PlanSource::Rerun),
    ] {
        let db = FakeTargetDb::new().with_threads(&[(1, 3)]);
        db.set_sql_mode(mode);
        *db.full.lock().unwrap() = vec![dbmon_core::ports::target_db::FullSqlRow {
            id: 1,
            db: Some("shop".into()),
            user: Some("app".into()),
            host: Some("10.0.3.44".into()),
            time_secs: 3,
            info: Some("SELECT a FROM t WHERE id = 1".into()),
        }];
        *db.explain.lock().unwrap() = Some(ExplainOutcome::Plan(
            r#"{"query_block":{"table":{"table_name":"t","access_type":"ALL"}}}"#.into(),
        ));

        let store = std::sync::Arc::new(FakeSlowQueryStore::default());
        let clock = FakeClock::new(1_755_500_400_000);
        let mut c = InstanceCollector::new(
            instance("orders-prd-01"),
            db,
            store.clone(),
            clock,
            params(),
        );
        c.warm_if_needed().await; // 여기서 sql_mode 를 읽는다
        c.detect_tick().await.expect("tick");

        let saved = store.all();
        let q = saved.iter().find(|q| q.thread_id == 1).expect("레코드");
        assert_eq!(
            q.plan.source, expect,
            "sql_mode={mode:?} 에서 plan_source 가 틀렸다"
        );
    }
}

/// **축출은 `TickStats` 로 관측 가능해야 한다.**
///
/// 카운터가 `InFlightTracker` 안에만 있으면 아무도 읽지 않는다 — 문서가 "관측 가능해야
/// 한다" 고 적어도 그건 주장일 뿐이다. 읽히지 않는 카운터는 관측성이 아니다.
#[tokio::test]
async fn eviction_is_visible_in_tick_stats() {
    let db = FakeTargetDb::new();
    let store = std::sync::Arc::new(FakeSlowQueryStore::default());
    let clock = FakeClock::new(1_755_500_400_000);
    let mut c = InstanceCollector::new(
        instance("orders-prd-01"),
        db,
        store,
        clock.clone(),
        params(),
    );
    c.set_max_tracked_entries(2);

    // 상한 2. 매 tick 새 스레드 3개가 나타나고 이전 것은 사라진다(잘린 목록).
    let mut saw_evicted = 0usize;
    for tick in 0..6i64 {
        let ids: Vec<(u64, i64)> = (0..3).map(|i| ((tick * 3 + i) as u64, 3)).collect();
        c.db_mut().with_threads_mut(&ids);
        c.db_mut().set_truncated(true);
        let st = c.detect_tick().await.expect("tick");
        saw_evicted += st.evicted;
        clock.advance(1_000);
    }
    assert!(
        saw_evicted > 0,
        "축출이 TickStats.evicted 로 올라오지 않았다"
    );
}

/// **확정 시점에 끝난 문장의 지표를 실제로 채운다.**
///
/// # 왜 이게 필요한가
///
/// MySQL 은 실행 중인 문장의 `ROWS_EXAMINED` 를 **0 으로** 준다(실측: 12만 행을 훑는
/// 조인이 완료 후 120,005). 실시간 캡처만으로는 그 값을 영구히 알 수 없고, 슬로우로그가
/// 병합되지 않으면 그대로 남는다 — 실측 4,814건 중 2,016건(42%)이 그 상태였다.
///
/// 확정 시점 주석은 "스레드가 이미 사라졌으므로 새로 조회할 수 없다" 고 했지만, 그건
/// 커넥션이 닫혔을 때만 참이다. 풀링 커넥션의 `events_statements_history` 는 방금 끝난
/// 문장을 실제 값으로 들고 있다.
#[tokio::test]
async fn finalization_fills_metrics_from_statement_history() {
    let db = FakeTargetDb::new()
        .with_threads(&[(1, 3)])
        // 심층 조회는 **실행 중** 값을 준다 — 카운터가 0 이다.
        .with_running_stmt(1, "d-abc");
    // 확정 시점에는 끝난 문장이 히스토리에 있다.
    db.with_finished_history(1, "d-abc", 120_005, 5);

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
    // 선행 저장 시점에는 아직 모른다 — 0 을 사실로 저장하지 않는다.
    let mid = store.all();
    let running = mid
        .iter()
        .find(|q| q.thread_id == 1)
        .expect("선행 저장 레코드");
    assert_eq!(
        running.stats.rows_examined, None,
        "실행 중 값 0 을 '0행을 훑었다' 로 저장했다"
    );

    // 스레드가 끝났다 → 확정.
    c.db_mut().with_threads_mut(&[]);
    clock.advance(1_000);
    let t2 = c.detect_tick().await.expect("tick2");

    assert_eq!(t2.finalized, 1);
    assert_eq!(
        t2.finalized_with_history, 1,
        "히스토리를 못 읽었다 — 조용히 예전 동작으로 돌아간 것과 구분되지 않는다"
    );

    let saved = store.all();
    let done = saved
        .iter()
        .find(|q| q.thread_id == 1)
        .expect("확정 레코드");
    assert_eq!(done.state, SlowQueryState::Finalized);
    assert_eq!(
        done.stats.rows_examined,
        Some(120_005),
        "확정 시점에 실제 검사 행수를 채우지 못했다"
    );
    assert_eq!(done.stats.rows_sent, Some(5));
}

/// **다이제스트를 모르면 히스토리를 붙이지 않는다.**
///
/// 같은 커넥션의 히스토리에는 최근 문장 5개가 남는다. 가릴 근거 없이 아무 행을 붙이면
/// **엉뚱한 문장의 지표**가 이 레코드의 사실이 된다 — 지표가 없는 것보다 나쁘다.
/// "가장 가까운 행을 고른다" 는 교차 리뷰 24·25라운드에 블로커였다.
#[tokio::test]
async fn history_is_not_applied_without_a_digest_to_match_on() {
    // 심층 조회 결과가 없다 → 다이제스트를 모른다.
    let db = FakeTargetDb::new().with_threads(&[(1, 3)]);
    db.with_finished_history(1, "d-somebody-else", 999_999, 7);

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
    clock.advance(1_000);
    let t2 = c.detect_tick().await.expect("tick2");

    assert_eq!(t2.finalized, 1, "확정 자체는 되어야 한다");
    assert_eq!(
        t2.finalized_with_history, 0,
        "다이제스트 없이 히스토리를 붙였다 — 남의 지표를 이 레코드의 사실로 만들었다"
    );
    let saved = store.all();
    let done = saved
        .iter()
        .find(|q| q.thread_id == 1)
        .expect("확정 레코드");
    assert_eq!(done.stats.rows_examined, None);
}
