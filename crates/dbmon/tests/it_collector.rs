//! 수집 파이프라인 통합 테스트 — **실제 MySQL + 페이크 저장소**.
//!
//! AWS 없이 캡처 경로 전체를 검증한다: 탐지 → 전문 SQL → 정확 지표 → 실행계획 →
//! 정규화·마스킹 → 저장. SSO 세션이 만료돼도 이 테스트가 돈다.
//!
//! ```
//! docker compose up -d
//! cargo test -p dbmon --test it_collector -- --nocapture --test-threads=1
//! ```
//!
//! 테스트는 [`support::TARGET_DB_LOCK`] 으로 **직렬화**된다. `reset_targets` 가 다른
//! 테스트의 장기 실행 쿼리까지 죽이므로 병렬이면 불안정하다.
//! `--test-threads=1` 을 잊어도 안전하다 — 실행 방법에 의존하는 테스트는 언젠가 깨진다.

mod support;

use std::sync::Arc;
use std::time::Duration;

use dbmon::collector::{CollectParams, InstanceCollector};
use dbmon::mysql::{TargetMysql, Timeouts};
use dbmon_core::env::{Env, EnvResolution};
use dbmon_core::fakes::FakeSlowQueryStore;
use dbmon_core::ids::InstanceId;
use dbmon_core::instance::{Engine, EngineVersion, Instance, InstanceState};
use dbmon_core::slow_query::{
    CaptureSource, DurationSource, LiteralPolicy, PlanSource, SlowQueryState,
};
use dbmon_core::time::SystemClock;
use support::*;

const ACCOUNT: &str = "000000000000";

fn local_instance(name: &str) -> Instance {
    Instance {
        id: InstanceId::new(ACCOUNT, "ap-northeast-2", name).unwrap(),
        cluster_id: None,
        engine: Engine::Mysql,
        engine_version: EngineVersion::parse("8.4.11").unwrap(),
        env: EnvResolution::resolve(Env::Dev, None),
        state: InstanceState::Collecting,
        endpoint: Some("127.0.0.1".into()),
        port: MYSQL84.port,
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

/// 모니터링 계정으로 붙은 수집기를 만든다. 컨테이너가 없으면 `None`.
fn collector(
    policy: LiteralPolicy,
    params: CollectParams,
) -> Option<(
    InstanceCollector<TargetMysql, FakeSlowQueryStore, SystemClock>,
    Arc<FakeSlowQueryStore>,
)> {
    let opts = opts(MYSQL84, MONITOR);
    let timeouts = Timeouts {
        // 로컬 컨테이너는 빠르지만 CI 지터를 감당할 여유를 둔다.
        detect: Duration::from_secs(2),
        ..Default::default()
    };
    let db = TargetMysql::connect(opts, timeouts, "local-mysql84").ok()?;
    let store = Arc::new(FakeSlowQueryStore::new());
    let c = InstanceCollector::new(
        local_instance("local-mysql84"),
        db,
        store.clone(),
        SystemClock,
        CollectParams {
            literal_policy: policy,
            // 자기 제외 대상. `dbmon` 계정의 쿼리는 후보가 되지 않아야 한다.
            monitor_db_user: MONITOR.0.into(),
            worker_id: "test-worker".into(),
            ..params
        },
    );
    Some((c, store))
}

/// 컨테이너가 살아 있는지 확인한다. 없으면 테스트를 건너뛴다.
async fn containers_up() -> bool {
    connect(MYSQL84, MONITOR).await.is_some()
}

/// 후보가 나타날 때까지 tick 을 돌린다.
async fn tick_until<F>(
    c: &mut InstanceCollector<TargetMysql, FakeSlowQueryStore, SystemClock>,
    max_ticks: usize,
    done: F,
) -> dbmon::collector::TickStats
where
    F: Fn(&dbmon::collector::TickStats) -> bool,
{
    let mut last = Default::default();
    for _ in 0..max_ticks {
        match c.detect_tick().await {
            Ok(s) => {
                if done(&s) {
                    return s;
                }
                last = s;
            }
            Err(e) => panic!("detect_tick 실패: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    last
}

/// 전체 캡처 경로 — 느린 SELECT 하나를 끝까지 따라간다.
#[tokio::test]
async fn captures_slow_select_end_to_end() {
    if !containers_up().await {
        eprintln!("[skip] docker compose up -d 를 먼저 실행한다");
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let Some((mut c, store)) = collector(LiteralPolicy::Full, CollectParams::default()) else {
        return;
    };
    let _ = c.detect_tick().await;

    let sql = "SELECT COUNT(*) FROM orders WHERE status = 'PENDING' AND SLEEP(8) = 0";
    let Some(running) = start_long_query(MYSQL84, LOADGEN, sql).await else {
        return;
    };

    let probed = tick_until(&mut c, 6, |s| s.deep_probed > 0).await;
    assert!(
        probed.candidates > 0,
        "느린 쿼리를 탐지하지 못했다: {probed:?}"
    );
    assert!(
        probed.deep_probed > 0,
        "심층 조회가 발행되지 않았다: {probed:?}"
    );
    assert!(
        probed.prefetch_saved > 0,
        "선행 저장이 되지 않았다: {probed:?}"
    );

    // 선행 저장된 레코드를 확인한다.
    let saved = store.all();
    let rec = saved
        .iter()
        .find(|q| q.sql_text.as_deref().is_some_and(|t| t.contains("PENDING")))
        .unwrap_or_else(|| panic!("전문 SQL 이 저장되지 않았다. 저장된 것: {saved:#?}"));

    assert_eq!(
        rec.state,
        SlowQueryState::InFlight,
        "확정 전에는 in_flight 다"
    );
    assert_eq!(rec.capture_source, CaptureSource::Processlist);
    assert!(!rec.sql_text_truncated);
    assert_eq!(rec.db_user.as_deref(), Some(LOADGEN.0));
    assert_eq!(rec.schema_name.as_deref(), Some("shop"));
    assert!(
        rec.duration_ms >= 1_000,
        "관측된 실행시간이 너무 짧다: {}",
        rec.duration_ms
    );
    assert!(
        rec.owner_worker.is_some() && rec.last_seen_at_ms.is_some(),
        "고아 정리 필드 (F4)"
    );

    // **실행계획을 얻었어야 한다.** RDS 에서는 재실행 경로다.
    assert_ne!(
        rec.plan.source,
        PlanSource::None,
        "플랜을 못 얻었다: {:?}",
        rec.plan
    );
    assert_eq!(
        rec.plan.source,
        PlanSource::Rerun,
        "SELECT 는 원문 재실행이어야 한다 (FOR CONNECTION 은 권한이 없다)"
    );
    let plan_json = rec.plan.normalized_json.as_deref().expect("마스킹된 플랜");
    assert!(
        !plan_json.contains("PENDING"),
        "플랜에 리터럴이 남았다 (T-16): {plan_json}"
    );
    assert!(rec.plan.fingerprint.is_some());
    assert!(
        rec.plan
            .referenced_tables
            .iter()
            .any(|t| t.ends_with("orders")),
        "참조 테이블: {:?}",
        rec.plan.referenced_tables
    );

    // 쿼리가 끝나면 확정된다.
    kill_and_wait(running).await;
    let finalized = tick_until(&mut c, 6, |s| s.finalized > 0).await;
    assert!(finalized.finalized > 0, "확정되지 않았다: {finalized:?}");

    let after = store.all();
    let rec = after
        .iter()
        .find(|q| q.record_id == rec.record_id)
        .expect("같은 record_id 로 병합돼야 한다");
    assert_eq!(rec.state, SlowQueryState::Finalized);
    assert!(
        rec.ended_at_ms.is_some(),
        "종료를 관측했으므로 ended_at 이 있어야 한다"
    );
    assert!(
        rec.sql_text
            .as_deref()
            .is_some_and(|t| t.contains("PENDING")),
        "확정이 선행 저장의 텍스트를 덮어썼다 — upsert_merged 가 병합해야 한다"
    );
    assert_ne!(rec.plan.source, PlanSource::None, "확정이 플랜을 지웠다");
    assert_eq!(
        after
            .iter()
            .filter(|q| q.record_id == rec.record_id)
            .count(),
        1,
        "레코드가 중복 생성됐다"
    );
}

/// `masked` 정책에서 **선행 저장 시점부터** 리터럴이 없어야 한다 (F2).
#[tokio::test]
async fn masked_policy_never_stores_literals_even_in_flight() {
    if !containers_up().await {
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let Some((mut c, store)) = collector(LiteralPolicy::Masked, CollectParams::default()) else {
        return;
    };
    let _ = c.detect_tick().await;

    // 리터럴이 눈에 띄는 값이어야 검증이 의미가 있다.
    //
    // 리터럴을 **조건절이 아니라 select 목록에** 둔다. 조건절에 두면 옵티마이저가
    // 그걸 먼저 평가해 행을 걸러내고 `SLEEP` 을 호출하지 않을 수 있다 —
    // 그러면 쿼리가 느려지지 않아 탐지되지 않는다(첫 실행에서 실제로 그랬다).
    let secret = "TOPSECRET-900101-1234567";
    let sql =
        format!("SELECT COUNT(*), '{secret}' AS tag FROM orders WHERE id = 1 AND SLEEP(10) = 0");
    let Some(running) = start_long_query(MYSQL84, LOADGEN, sql).await else {
        return;
    };

    let s = tick_until(&mut c, 10, |s| s.prefetch_saved > 0).await;
    assert!(s.prefetch_saved > 0, "선행 저장이 되지 않았다: {s:?}");

    for rec in store.all() {
        let dump = format!("{rec:?}");
        assert!(
            !dump.contains(secret),
            "in_flight 레코드에 리터럴이 남았다 (F2 위반).\n  record: {dump}"
        );
        assert_eq!(rec.literal_policy, LiteralPolicy::Masked);
    }
    // 마스킹 후조건이 실패해 강등된 것이 아니어야 한다 — 정상 SQL 이다.
    assert_eq!(s.masking_degraded, 0, "정상 SQL 이 강등됐다");

    kill_and_wait(running).await;
}

/// DML 은 **조건절을 SELECT 로 변환한 근사 플랜**을 얻는다 (M1-4b).
#[tokio::test]
async fn dml_gets_approximate_plan_via_select_conversion() {
    if !containers_up().await {
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let Some((mut c, store)) = collector(LiteralPolicy::Full, CollectParams::default()) else {
        return;
    };
    let _ = c.detect_tick().await;

    let Some(running) = start_long_statements(
        MYSQL84,
        LOADGEN,
        vec![
            "START TRANSACTION".into(),
            "UPDATE orders SET memo = 'dml-plan-test' WHERE status = 'PAID' AND SLEEP(6) = 0"
                .into(),
        ],
    )
    .await
    else {
        return;
    };

    let s = tick_until(&mut c, 6, |s| s.deep_probed > 0).await;
    assert!(s.deep_probed > 0, "UPDATE 를 탐지하지 못했다: {s:?}");

    let saved = store.all();
    let rec = saved
        .iter()
        .find(|q| {
            q.sql_text
                .as_deref()
                .is_some_and(|t| t.contains("dml-plan-test"))
        })
        .unwrap_or_else(|| panic!("UPDATE 가 저장되지 않았다: {saved:#?}"));

    assert_eq!(rec.statement_type.as_str(), "UPDATE");
    assert_eq!(
        rec.plan.source,
        PlanSource::RerunAsSelect,
        "DML 은 SELECT 변환 경로여야 한다. EXPLAIN UPDATE 는 1142 로 거부된다"
    );
    assert!(
        !rec.plan.source.is_exact(),
        "근사 플랜이므로 UI 에 배지가 필요하다"
    );
    assert!(rec.plan.normalized_json.is_some());
    assert_eq!(s.plans_rerun_as_select, 1, "{s:?}");

    kill_and_wait(running).await;
}

/// 우리 계정(`dbmon`)의 쿼리는 후보가 되지 않아야 한다 ([05 §10](../../../../docs/05-collector.md)).
///
/// 자기 제외가 실패하면 1초 주기 `detect` 쿼리가 상위 N 후보·`_other`·커버리지·
/// 계정 롤업을 전부 오염시킨다.
#[tokio::test]
async fn self_queries_are_excluded() {
    if !containers_up().await {
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let Some((mut c, _store)) = collector(LiteralPolicy::Full, CollectParams::default()) else {
        return;
    };

    // 모니터링 계정으로 느린 쿼리를 실행한다.
    let Some(running) = start_long_query(MYSQL84, MONITOR, "SELECT SLEEP(10)").await else {
        return;
    };
    let own_thread = running.connection_id;

    // **개수가 아니라 우리 스레드가 추적되는지**를 본다. 다른 테스트의 잔여 쿼리가
    // 후보로 잡힐 수 있으므로 총 개수로 판정하면 흔들린다.
    for _ in 0..6 {
        let _ = c.detect_tick().await.expect("tick");
        assert!(
            !c.is_tracking(own_thread),
            "모니터링 계정({})의 쿼리가 추적됐다 — 자기 제외가 동작하지 않는다. \
             실패하면 1초 주기 detect 쿼리가 상위 N 후보·_other·커버리지를 오염시킨다",
            MONITOR.0
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    kill_and_wait(running).await;
}

/// 시계 오프셋이 추정되고, `probe` 가 0행일 때도 갱신 경로가 있다 (F14).
#[tokio::test]
async fn clock_offset_is_estimated() {
    if !containers_up().await {
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let Some((mut c, _)) = collector(LiteralPolicy::Full, CollectParams::default()) else {
        return;
    };

    // 후보가 있어야 `probe` 가 DB 시각을 준다.
    let Some(running) = start_long_query(MYSQL84, LOADGEN, "SELECT SLEEP(5)").await else {
        return;
    };
    tick_until(&mut c, 5, |s| s.candidates > 0).await;
    kill_and_wait(running).await;

    let offset = c.clock_offset();
    assert!(offset.has_estimate(), "오프셋을 추정하지 못했다");
    // 로컬 컨테이너는 호스트와 시계를 공유하므로 오차가 작아야 한다.
    assert!(
        offset.as_ms().abs() < 5_000,
        "로컬 컨테이너와 5초 이상 어긋났다: {}ms",
        offset.as_ms()
    );
    assert_eq!(offset.severity().as_str(), "ok");
}

/// 정상 상태에서는 tick 당 쿼리 1건이어야 한다 — 후보가 없으면 조기 종료한다.
#[tokio::test]
async fn idle_tick_does_no_deep_probe() {
    if !containers_up().await {
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let Some((mut c, store)) = collector(LiteralPolicy::Full, CollectParams::default()) else {
        return;
    };
    let _ = c.detect_tick().await;

    let s = c.detect_tick().await.expect("tick");
    assert_eq!(s.candidates, 0, "격리 후에도 후보가 남아 있다: {s:?}");
    assert_eq!(
        s.deep_probed, 0,
        "유휴 상태에서 심층 조회가 발생했다: {s:?}"
    );
    assert_eq!(s.prefetch_saved, 0);
    assert!(store.is_empty(), "유휴 상태에서 레코드가 저장됐다");
}

/// 심층 조회 상한이 폭주를 막는다.
#[tokio::test]
async fn deep_probe_limit_caps_work() {
    if !containers_up().await {
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let params = CollectParams {
        deep_probe_limit: 1,
        ..Default::default()
    };
    let Some((mut c, _)) = collector(LiteralPolicy::Full, params) else {
        return;
    };
    let _ = c.detect_tick().await;

    let mut running = Vec::new();
    for i in 0..3 {
        if let Some(r) = start_long_query(
            MYSQL84,
            LOADGEN,
            format!(
                "SELECT COUNT(*) FROM orders WHERE id = {} AND SLEEP(12) = 0",
                i + 1
            ),
        )
        .await
        {
            running.push(r);
        }
    }

    let s = tick_until(&mut c, 10, |s| s.candidates >= 3).await;
    assert!(s.candidates >= 3, "3건을 동시에 탐지하지 못했다: {s:?}");
    assert_eq!(s.deep_probed, 1, "상한을 넘겼다: {s:?}");

    // **`deep_probe_skipped == candidates - 1` 은 불변식이 아니다.**
    //
    // `deep_probe_skipped` 는 `needs_deep_probe` 초과분이고, 엔트리는
    // `plan_attempts >= max_plan_attempts` 가 되면 후보로 남은 채 `needs_deep_probe` 에서
    // 빠진다(`inflight.rs`). tick 이 여러 번 도는 동안 하나가 시도를 소진하면
    // `skipped` 가 `candidates - 1` 보다 작아진다 — 릴리스 빌드가 더 빨라 tick 이 많이
    // 돌므로 **릴리스에서만** 약 14% 실패했다.
    //
    // 실제로 확인할 것은 "상한이 지켜졌고 초과분이 보고된다" 다.
    assert!(
        s.deep_probe_skipped >= 1,
        "상한을 넘은 후보가 있으면 건너뛴 수를 보고해야 한다: {s:?}"
    );
    assert!(
        s.deep_probed + s.deep_probe_skipped <= s.candidates,
        "심층 조회 + 건너뜀이 후보 수를 넘을 수 없다: {s:?}"
    );

    for r in running {
        kill_and_wait(r).await;
    }
}

/// 정확 지표가 `events_statements_current` 에서 온다.
#[tokio::test]
async fn exact_metrics_come_from_statements_current() {
    if !containers_up().await {
        return;
    }
    let _guard = exclusive_target(MYSQL84).await;
    let Some((mut c, store)) = collector(LiteralPolicy::Full, CollectParams::default()) else {
        return;
    };
    let _ = c.detect_tick().await;

    let sql = "SELECT COUNT(*) FROM orders WHERE status = 'PAID' AND SLEEP(0.0002) = 0";
    let Some(running) = start_long_query(MYSQL84, LOADGEN, sql).await else {
        return;
    };

    tick_until(&mut c, 10, |s| s.deep_probed > 0).await;
    let saved = store.all();
    let rec = saved
        .iter()
        .find(|q| {
            q.sql_text
                .as_deref()
                .is_some_and(|t| t.contains("SLEEP(0.0002)"))
        })
        .unwrap_or_else(|| panic!("대상 쿼리가 저장되지 않았다: {saved:#?}"));

    // **`ROWS_EXAMINED` 는 실행 중 스냅샷이다.** 캡처 시점에 아직 0일 수 있으므로
    // 값의 크기를 단정하지 않는다. 중요한 것은 필드가 채워지는 경로가 살아 있는가다.
    assert!(
        rec.stats.rows_examined.is_some(),
        "events_statements_current 지표를 받지 못했다 — consumer 설정을 확인한다"
    );
    assert_eq!(
        rec.duration_source,
        DurationSource::Timer,
        "TIMER_WAIT 가 있으면 초 단위 근사보다 정확하다"
    );

    kill_and_wait(running).await;
}
