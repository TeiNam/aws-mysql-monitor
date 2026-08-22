//! DynamoDB 어댑터 통합 테스트 — **DynamoDB Local 에 실제로 쓰고 읽는다** (M2-4).
//!
//! AWS 자격증명이 필요 없다. `docker compose up -d dynamodb` 만 있으면 된다 —
//! 로컬 우선 원칙이다.
//!
//! # 이 테스트가 답하는 질문
//!
//! 다섯 라운드 리뷰는 "이 로직이 맞는가" 까지만 답했다. 여기서 처음으로
//! **"키가 실제로 맞는가", "병합이 저장소를 통해도 성립하는가"** 를 확인한다.
//! 키 문자열은 컴파일러가 검사하지 않으므로 이 확인이 유일한 방법이다.

use std::sync::Arc;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
use dbmon::store::DynamoSlowQueryStore;
use dbmon_core::env::Env;
use dbmon_core::ids::{InstanceId, RecordId};
use dbmon_core::instance::Engine;
use dbmon_core::ports::SlowQueryStore;
use dbmon_core::slow_query::*;
use dbmon_core::time::TimeRange;
use dbmon_normalize::StatementType;

const ENDPOINT: &str = "http://127.0.0.1:18000";
const ACCOUNT: &str = "123456789012";

/// DynamoDB Local 클라이언트. 자격증명은 아무 값이어도 된다.
fn client() -> Client {
    let cfg = aws_sdk_dynamodb::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("ap-northeast-2"))
        .credentials_provider(Credentials::new("local", "local", None, None, "test"))
        .endpoint_url(ENDPOINT)
        .build();
    Client::from_conf(cfg)
}

/// 테스트마다 **다른 테이블**을 쓴다. 병렬 실행에서 서로를 보지 않게 한다 —
/// `it_collector`/`m1_capture` 에서 크로스 간섭으로 세 번 데인 뒤의 규칙이다.
async fn store(name: &str) -> Option<Arc<DynamoSlowQueryStore>> {
    let s = Arc::new(DynamoSlowQueryStore::new(
        client(),
        format!("dbmon-test-{name}"),
    ));
    // **매번 지우고 만든다.** DynamoDB Local 은 영속이므로 이전 실행의 레코드가 남는다.
    match s.reset_table_for_local().await {
        Ok(()) => Some(s),
        Err(e) => {
            // **CI 에서는 건너뛰지 않고 실패한다.**
            //
            // 이 파일이 `mark_missing`·`stamp_deleted`·`list()` 페이지네이션을 덮는
            // 유일한 테스트다. 서비스명·포트·이미지가 깨지면 23건이 조용히 무력화되고
            // 스위트는 초록으로 남는다 — `ignored` 도 아니라 `ok` 로 보고된다
            // (2차 리뷰가 지적). CI 는 `DBMON_REQUIRE_DYNAMO=1` 을 준다.
            if std::env::var("DBMON_REQUIRE_DYNAMO").as_deref() == Ok("1") {
                panic!(
                    "DynamoDB Local 에 붙을 수 없다: {e}\n\
                     DBMON_REQUIRE_DYNAMO=1 이므로 건너뛰지 않는다 — \
                     이 테스트가 등록부·리스 어댑터를 덮는 유일한 경로다"
                );
            }
            eprintln!(
                "건너뜀: DynamoDB Local 에 붙을 수 없다 ({e}). docker compose up -d dynamodb"
            );
            None
        }
    }
}

fn instance() -> InstanceId {
    InstanceId::new(ACCOUNT, "ap-northeast-2", "orders-prd-01").expect("id")
}

/// 탐색 라운드 간격. **`MISSING_MIN_GAP_MS`(240초)보다 커야 한다** — 그보다 짧은
/// 간격의 두 번째 미발견은 "같은 라운드" 로 보고 거부된다(한 라운드를 두 번 세는 것을
/// 막는 장치다). 기본 탐색 주기가 300초이므로 그 값을 쓴다.
const ROUND_MS: i64 = 300_000;

const T0: i64 = 1_755_500_400_000;

fn sample(thread_id: u64, started_at_ms: i64) -> SlowQuery {
    let i = instance();
    SlowQuery {
        record_id: RecordId::new(&i, thread_id, started_at_ms),
        instance_id: i,
        cluster_id: None,
        env: Env::Prd,
        engine: Engine::Mysql,
        engine_version: "8.4.6".into(),
        state: SlowQueryState::Finalized,
        thread_id,
        schema_name: Some("shop".into()),
        db_user: Some("app".into()),
        db_host: Some("10.0.3.44".into()),
        started_at_ms,
        started_at_ms_precise: None,
        ended_at_ms: Some(started_at_ms + 4_000),
        captured_at_ms: started_at_ms + 4_100,
        duration_ms: 4_000,
        duration_source: DurationSource::Span,
        sql_text: Some("SELECT a FROM t WHERE id = ?".into()),
        sql_text_truncated: false,
        sql_text_lossy: false,
        literal_policy: LiteralPolicy::Masked,
        literal_policy_at_ms: started_at_ms,
        app_digest: "9f2c1a".into(),
        digest_algo_version: 1,
        mysql_digest: None,
        statement_type: StatementType::Select,
        is_nested: false,
        stats: ExecStats {
            rows_examined: Some(60_023),
            ..Default::default()
        },
        plan: Default::default(),
        capture_source: CaptureSource::Processlist,
        owner_worker: Some("w1".into()),
        owner_epoch: Some(1),
        last_seen_at_ms: Some(started_at_ms + 3_000),
        abandoned_reason: None,
        long_running: false,
    }
}

/// **왕복.** 직렬화·키·역직렬화가 모두 맞아야 한다.
#[tokio::test]
async fn round_trips_every_field() {
    let Some(s) = store("roundtrip").await else {
        return;
    };
    let q = sample(1001, T0);

    let saved = s.upsert_merged(&q).await.expect("저장");
    assert_eq!(saved.record_id, q.record_id);

    let got = s
        .get(&q.record_id)
        .await
        .expect("조회")
        .expect("있어야 한다");
    // 전 필드가 보존돼야 한다 — 한 필드라도 빠지면 조용히 정보를 잃는다.
    assert_eq!(got, q, "왕복에서 필드가 달라졌다");
}

/// **`record_id` 는 초 단위인데 `SK` 는 밀리초다.** 그 간극이 조회를 깨뜨리지 않아야 한다.
#[tokio::test]
async fn record_id_finds_the_item_despite_millisecond_sk() {
    let Some(s) = store("msgap").await else {
        return;
    };
    // 초 안의 임의 밀리초 (`record_id` 는 같아진다)
    let q = sample(1002, T0 + 777);
    s.upsert_merged(&q).await.expect("저장");

    let got = s.get(&q.record_id).await.expect("조회");
    assert!(
        got.is_some(),
        "record_id 가 초 단위라 SK 를 복원할 수 없다 — begins_with 조회가 동작해야 한다"
    );
    assert_eq!(
        got.unwrap().started_at_ms,
        T0 + 777,
        "밀리초가 보존돼야 한다"
    );
}

/// **F5 — 저장소를 통해도 병합이 성립해야 한다.**
///
/// 선행 저장(진행 중, 지표 없음) → 확정(지표 있음). `PutItem` 이면 지표가 덮어써진다.
#[tokio::test]
async fn upsert_merges_instead_of_overwriting() {
    let Some(s) = store("merge").await else {
        return;
    };

    let mut pre = sample(1003, T0);
    pre.state = SlowQueryState::InFlight;
    pre.ended_at_ms = None;
    pre.duration_ms = 2_100;
    pre.duration_source = DurationSource::Timer;
    pre.stats = Default::default(); // 지표 없음
    s.upsert_merged(&pre).await.expect("선행 저장");

    let fin = sample(1003, T0); // 확정: 62초, 지표 있음
    let mut fin = fin;
    fin.duration_ms = 62_000;
    fin.duration_source = DurationSource::Polled;
    fin.ended_at_ms = Some(T0 + 62_000);
    let merged = s.upsert_merged(&fin).await.expect("확정 저장");

    // **62초 쿼리가 2.1초로 저장되면 안 된다** (1차 C1).
    assert_eq!(merged.duration_ms, 62_000, "미완결 Timer 가 이겼다");
    assert_eq!(
        merged.stats.rows_examined,
        Some(60_023),
        "확정의 지표가 보존돼야 한다"
    );
    assert_eq!(merged.state, SlowQueryState::Finalized);

    // 저장된 것도 같아야 한다 — 반환값만 맞고 저장이 틀리면 더 나쁘다.
    let got = s.get(&fin.record_id).await.expect("조회").expect("있음");
    assert_eq!(got.duration_ms, 62_000);
    assert_eq!(got.stats.rows_examined, Some(60_023));
}

/// 반대 순서(확정 먼저, 백필 나중)도 같은 결과여야 한다 (R43 대칭성).
#[tokio::test]
async fn merge_is_symmetric_through_the_store() {
    let Some(s) = store("symmetric").await else {
        return;
    };

    let mut slowlog = sample(1004, T0);
    slowlog.duration_ms = 5_200;
    slowlog.duration_source = DurationSource::Slowlog;
    slowlog.capture_source = CaptureSource::Slowlog;

    let mut polled = sample(1004, T0);
    polled.duration_ms = 6_000;
    polled.duration_source = DurationSource::Polled;

    // 순서 A
    s.upsert_merged(&polled).await.expect("A1");
    let a = s.upsert_merged(&slowlog).await.expect("A2");

    // 순서 B — 다른 스레드 ID 로 같은 실험
    let mut slowlog_b = slowlog.clone();
    let mut polled_b = polled.clone();
    let i = instance();
    slowlog_b.record_id = RecordId::new(&i, 1005, T0);
    slowlog_b.thread_id = 1005;
    polled_b.record_id = RecordId::new(&i, 1005, T0);
    polled_b.thread_id = 1005;
    s.upsert_merged(&slowlog_b).await.expect("B1");
    let b = s.upsert_merged(&polled_b).await.expect("B2");

    assert_eq!(
        (a.duration_ms, a.duration_source),
        (b.duration_ms, b.duration_source),
        "도착 순서가 결과를 바꿨다"
    );
    assert_eq!(a.duration_ms, 5_200, "슬로우로그가 권위값이다");
}

/// **AP-18 — 진행 중 레코드만 희소 GSI1 에 보여야 한다** (F4 고아 정리).
///
/// `GSI1PK` 를 항상 `DG#` 로 쓰면 이 조회가 **영구히 0건**이고, 고아가 TTL 35일까지
/// 화면에 "실행 중" 으로 남는다. 에러가 아니라 빈 결과라 알아채기 어렵다.
#[tokio::test]
async fn in_flight_index_finds_only_running_records() {
    let Some(s) = store("inflight").await else {
        return;
    };

    let mut running = sample(2001, T0);
    running.state = SlowQueryState::InFlight;
    running.ended_at_ms = None;
    s.upsert_merged(&running).await.expect("진행 중 저장");

    let done = sample(2002, T0 + 1_000); // 확정
    s.upsert_merged(&done).await.expect("확정 저장");

    let listed = s.list_in_flight(50).await.expect("AP-18 조회");
    let ids: Vec<u64> = listed.iter().map(|q| q.thread_id).collect();
    assert!(ids.contains(&2001), "진행 중 레코드가 안 보인다: {ids:?}");
    assert!(!ids.contains(&2002), "확정 레코드가 보인다: {ids:?}");
}

/// 확정되면 GSI1 파티션이 **바뀌어야** 한다 — 진행 중 목록에서 빠진다.
#[tokio::test]
async fn finalizing_moves_the_record_out_of_the_in_flight_index() {
    let Some(s) = store("transition").await else {
        return;
    };

    let mut running = sample(2003, T0);
    running.state = SlowQueryState::InFlight;
    running.ended_at_ms = None;
    s.upsert_merged(&running).await.expect("진행 중");
    assert!(
        s.list_in_flight(50)
            .await
            .expect("조회")
            .iter()
            .any(|q| q.thread_id == 2003),
        "먼저 진행 중으로 보여야 한다"
    );

    let done = sample(2003, T0); // 같은 record_id, 확정 상태
    s.upsert_merged(&done).await.expect("확정");

    assert!(
        !s.list_in_flight(50)
            .await
            .expect("조회")
            .iter()
            .any(|q| q.thread_id == 2003),
        "확정 후에도 진행 중 목록에 남아 있다 — 고아 정리가 계속 이 레코드를 본다"
    );
}

/// **AP-1 — 인스턴스·기간 조회.** 날짜 파티션 순회가 동작해야 한다.
#[tokio::test]
async fn lists_by_instance_within_a_range() {
    let Some(s) = store("bytime").await else {
        return;
    };
    for (i, offset) in [0i64, 60_000, 120_000].iter().enumerate() {
        s.upsert_merged(&sample(3000 + i as u64, T0 + offset))
            .await
            .expect("저장");
    }

    let range = TimeRange::new(T0 - 1_000, T0 + 200_000).expect("구간");
    let got = s
        .list_by_instance(&instance(), range, 10)
        .await
        .expect("조회");
    let ids: Vec<u64> = got.iter().map(|q| q.thread_id).collect();
    for want in [3000u64, 3001, 3002] {
        assert!(ids.contains(&want), "{want} 이 빠졌다: {ids:?}");
    }

    // 좁은 구간은 일부만.
    let narrow = TimeRange::new(T0 - 1_000, T0 + 1_000).expect("구간");
    let got = s
        .list_by_instance(&instance(), narrow, 10)
        .await
        .expect("조회");
    let ids: Vec<u64> = got.iter().map(|q| q.thread_id).collect();
    assert!(ids.contains(&3000), "{ids:?}");
    assert!(!ids.contains(&3002), "구간 밖이 포함됐다: {ids:?}");
}

/// **병합 후보 조회** — `record_id` 가 1초 어긋난 경우 (05 §8.2).
#[tokio::test]
async fn finds_merge_candidate_within_the_window() {
    let Some(s) = store("candidate").await else {
        return;
    };
    let q = sample(4001, T0 + 500);
    s.upsert_merged(&q).await.expect("저장");

    // ±2초 창 안에서 찾는다.
    let found = s
        .find_merge_candidate(&instance(), 4001, "9f2c1a", T0 + 1_400, 2_000)
        .await
        .expect("조회");
    assert!(found.is_some(), "창 안의 후보를 찾지 못했다");
    assert_eq!(found.unwrap().started_at_ms, T0 + 500);

    // 다이제스트가 다르면 후보가 아니다.
    let other = s
        .find_merge_candidate(&instance(), 4001, "다른다이제스트", T0 + 1_400, 2_000)
        .await
        .expect("조회");
    assert!(other.is_none(), "다이제스트가 달라도 후보로 잡혔다");

    // 창 밖이면 못 찾는다.
    let far = s
        .find_merge_candidate(&instance(), 4001, "9f2c1a", T0 + 60_000, 2_000)
        .await
        .expect("조회");
    assert!(far.is_none(), "창 밖의 레코드가 후보로 잡혔다");
}

/// **`ttl` 이 항목에 있어야 한다.** 없으면 핫 티어가 무한히 자란다.
#[tokio::test]
async fn item_carries_a_ttl_attribute() {
    let Some(s) = store("ttl").await else { return };
    let q = sample(5001, T0);
    s.upsert_merged(&q).await.expect("저장");

    // 원시 항목을 직접 읽어 `ttl` 을 확인한다 — 도메인 타입에는 없는 필드다.
    let out = client()
        .query()
        .table_name("dbmon-test-ttl")
        .key_condition_expression("PK = :pk")
        .expression_attribute_values(
            ":pk",
            aws_sdk_dynamodb::types::AttributeValue::S(dbmon::store::keys::slow_query_pk(
                &instance(),
                T0,
            )),
        )
        .send()
        .await
        .expect("원시 조회");
    let item = out
        .items
        .unwrap_or_default()
        .into_iter()
        .next()
        .expect("항목");
    let ttl = item.get("ttl").expect("ttl 속성이 없다");
    let secs: i64 = ttl.as_n().expect("N 타입").parse().expect("숫자");
    let boundary = T0 / 1000 + dbmon_core::HOT_TIER_DAYS * 86_400;
    assert!(
        secs > boundary,
        "TTL 이 핫 경계보다 짧다: {secs} <= {boundary}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 리스 — F1 (M4-21)
// ═══════════════════════════════════════════════════════════════════════════════

use dbmon::store::lease::{DynamoLeaseStore, target_shard_count};
use dbmon_core::ports::{COLLECT_LEADER_KEY, LEASE_TTL_MS, LeaseStore};

async fn lease_store(name: &str) -> Option<DynamoLeaseStore> {
    // 리스는 슬로우 쿼리와 같은 테이블을 쓴다(단일 테이블 설계). 테이블을 먼저 만든다.
    let _ = store(name).await?;
    Some(DynamoLeaseStore::new(
        client(),
        format!("dbmon-test-{name}"),
    ))
}

/// **F1 의 핵심.** 두 워커가 같은 리스를 동시에 잡으려 하면 하나만 성공해야 한다.
///
/// 실패하면 두 워커가 모든 샤드를 소유하고 같은 인스턴스를 중복 수집한다 —
/// 다이제스트 누산기가 last-writer-wins 로 조용히 손상된다.
#[tokio::test]
async fn only_one_worker_wins_the_collect_leader_lease() {
    let Some(s) = lease_store("lease-mutex").await else {
        return;
    };
    let now = T0;

    let a = s
        .try_acquire(COLLECT_LEADER_KEY, "worker-a", now)
        .await
        .expect("A");
    let b = s
        .try_acquire(COLLECT_LEADER_KEY, "worker-b", now)
        .await
        .expect("B");

    assert!(a.is_some(), "첫 워커는 잡아야 한다");
    assert!(
        b.is_none(),
        "두 번째 워커가 유효한 리스를 빼앗았다 — 중복 수집이 된다"
    );

    // **F1 불변식**: 합계가 SHARD_COUNT 를 넘지 않는다.
    let total = target_shard_count(a.is_some()) + target_shard_count(b.is_some());
    assert_eq!(total, dbmon_core::ports::SHARD_COUNT, "합계 {total}");
}

/// 만료 후에는 다른 워커가 잡을 수 있고, `epoch` 가 올라간다.
#[tokio::test]
async fn expired_lease_is_taken_over_with_a_higher_epoch() {
    let Some(s) = lease_store("lease-takeover").await else {
        return;
    };

    let first = s
        .try_acquire(COLLECT_LEADER_KEY, "worker-a", T0)
        .await
        .expect("A")
        .expect("잡아야 한다");

    // TTL 이 지난 시점.
    let later = T0 + LEASE_TTL_MS + 1;
    let second = s
        .try_acquire(COLLECT_LEADER_KEY, "worker-b", later)
        .await
        .expect("B")
        .expect("만료됐으므로 잡아야 한다");

    assert_eq!(second.owner, "worker-b");
    assert!(
        second.epoch > first.epoch,
        "epoch 가 올라가지 않았다 ({} → {}) — 펜싱 근거가 없다",
        first.epoch,
        second.epoch
    );
}

/// **갱신은 `owner` + `epoch` 가 모두 맞을 때만** 성공해야 한다.
///
/// 우리가 만료를 눈치채지 못한 사이 다른 워커가 잡았다 놓았을 수 있다. `owner` 만
/// 비교하면 우리 것으로 착각하고 계속 쓴다 — 중복 수집이다.
#[tokio::test]
async fn renew_fails_after_another_worker_took_over() {
    let Some(s) = lease_store("lease-renew").await else {
        return;
    };

    let stale = s
        .try_acquire(COLLECT_LEADER_KEY, "worker-a", T0)
        .await
        .expect("A")
        .expect("잡음");

    // 갱신은 아직 된다.
    assert!(
        s.renew(&stale, T0 + 1_000).await.expect("갱신").is_some(),
        "유효한 리스는 갱신돼야 한다"
    );

    // 다른 워커가 만료 후 가져간다.
    let later = T0 + LEASE_TTL_MS * 3;
    s.try_acquire(COLLECT_LEADER_KEY, "worker-b", later)
        .await
        .expect("B")
        .expect("가져감");

    // 예전 리스로 갱신하면 실패해야 한다 — epoch 가 다르다.
    assert!(
        s.renew(&stale, later + 1)
            .await
            .expect("갱신 시도")
            .is_none(),
        "빼앗긴 리스가 갱신됐다 — 두 워커가 리더라고 믿는다"
    );
}

/// 명시적 반납은 즉시 재분배를 허용해야 한다 (그레이스풀 셧다운).
#[tokio::test]
async fn release_allows_immediate_takeover() {
    let Some(s) = lease_store("lease-release").await else {
        return;
    };

    let mine = s
        .try_acquire(COLLECT_LEADER_KEY, "worker-a", T0)
        .await
        .expect("A")
        .expect("잡음");
    s.release(&mine).await.expect("반납");

    // TTL 을 기다리지 않고 잡을 수 있어야 한다.
    let next = s
        .try_acquire(COLLECT_LEADER_KEY, "worker-b", T0 + 1)
        .await
        .expect("B");
    assert!(
        next.is_some(),
        "반납 후에도 TTL 을 기다려야 했다 — 수집 공백이 길어진다"
    );

    // `epoch` 는 보존돼야 한다 — 항목을 지우면 펜싱 근거가 사라진다.
    assert!(
        next.unwrap().epoch > mine.epoch,
        "반납이 epoch 를 초기화했다"
    );
}

/// 샤드 리스를 나열할 수 있어야 한다 — **`Scan` 없이**.
#[tokio::test]
async fn lists_shard_leases_without_scanning() {
    let Some(s) = lease_store("lease-list").await else {
        return;
    };
    for shard in [0u32, 7, 63] {
        s.try_acquire(&dbmon_core::ports::shard_key(shard), "worker-a", T0)
            .await
            .expect("획득");
    }
    let listed = s.list("SHARD").await.expect("나열");
    assert_eq!(listed.len(), 3, "잡은 3개만 보여야 한다: {listed:?}");
    assert!(listed.iter().all(|l| l.owner == "worker-a"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// 인스턴스 등록부 — FR-DSC-07 (M2-5)
// ═══════════════════════════════════════════════════════════════════════════════

use dbmon::aws::discovery::{RawDbInstance, to_instance};
use dbmon::store::registry::{DynamoInstanceRegistry, merge_discovered};
use dbmon_core::instance::InstanceState;
use dbmon_core::ports::InstanceRegistry;

async fn registry(name: &str) -> Option<DynamoInstanceRegistry> {
    let _ = store(name).await?;
    Some(DynamoInstanceRegistry::new(
        client(),
        format!("dbmon-test-{name}"),
    ))
}

fn discovered(identifier: &str) -> dbmon_core::instance::Instance {
    let raw = RawDbInstance {
        identifier: identifier.into(),
        dbi_resource_id: format!("db-{identifier}"),
        engine: "mysql".into(),
        engine_version: "8.4.6".into(),
        status: "available".into(),
        endpoint_address: Some(format!("{identifier}.abc.ap-northeast-2.rds.amazonaws.com")),
        endpoint_port: Some(3306),
        vpc_id: Some("vpc-dev".into()),
        region: "ap-northeast-2".into(),
        tags: [("env".to_string(), "prd".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    to_instance(&raw, ACCOUNT, &dbmon_core::env::EnvMapping::default(), T0).expect("매핑")
}

/// **왕복.** 등록부 항목의 전 필드가 보존돼야 한다.
#[tokio::test]
async fn registry_round_trips_the_instance() {
    let Some(r) = registry("reg-roundtrip").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    let got = r.get(&i.id).await.expect("조회").expect("있어야 한다");
    assert_eq!(got, i, "왕복에서 필드가 달라졌다");
}

/// **전체 목록이 `Scan` 없이 나와야 한다** — IAM 이 `dynamodb:Scan` 을 Deny 한다.
#[tokio::test]
async fn registry_lists_everything_from_one_partition() {
    let Some(r) = registry("reg-list").await else {
        return;
    };
    for id in ["orders-01", "orders-02", "billing-01"] {
        r.upsert(&discovered(id)).await.expect("등록");
    }
    let all = r.list().await.expect("나열");
    assert_eq!(
        all.len(),
        3,
        "{:?}",
        all.iter().map(|i| i.id.as_str()).collect::<Vec<_>>()
    );
}

/// **1회 미발견으로 삭제되지 않는다** (FR-DSC-07).
///
/// `DescribeDBInstances` 가 한 번 조절되면 500대가 전부 사라진 것으로 보인다.
/// 그 한 번으로 `deleted_at` 을 찍으면 UI 가 비고 과거 데이터의 메타 참조가 끊긴다.
#[tokio::test]
async fn one_missed_discovery_does_not_delete_the_instance() {
    let Some(r) = registry("reg-onemiss").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    let after = r.mark_missing(&i.id, T0 + 1_000).await.expect("미발견 1회");
    assert_eq!(after.missing_count, 1);
    assert_eq!(
        after.deleted_at_ms, None,
        "1회 미발견으로 삭제 도장이 찍혔다 — 일시적 API 실패가 등록부를 비운다"
    );
    assert_ne!(after.state, InstanceState::Deleted);
}

/// 2회 연속이면 `deleted_at` 을 찍는다. **항목은 남는다.**
/// **`set_state` 는 "수집하지 않는다" 는 결정을 덮지 않는다.**
///
/// 수집 태스크는 자기가 뜰 때 읽은 사본으로 첫 판정을 쓴다. 그 사이 탐색이
/// `dbmon:enabled=false` 태그를 보고 `Disabled` 로 바꿨다면, 여기서 `Collecting` 을
/// 쓰면 **opt-out 이 조용히 무시된다** — 재조정은 수집 가능한 상태를 보고 계속 수집한다.
#[tokio::test]
async fn set_state_does_not_overwrite_an_opt_out() {
    let Some(r) = registry("optout").await else {
        return;
    };
    let mut inst = discovered("orders-01");

    // 사용자가 끈 상태로 등록부에 있다.
    inst.state = InstanceState::Disabled;
    r.upsert(&inst).await.expect("등록");

    // 수집 태스크의 첫 판정이 그걸 덮으려 한다.
    let err = r
        .set_state(&inst.id, InstanceState::Collecting)
        .await
        .expect_err("덮어써졌다");
    assert!(
        matches!(err, dbmon_core::error::DomainError::Conflict(_)),
        "사유가 구분되지 않는다: {err:?}"
    );

    let after = r.get(&inst.id).await.expect("조회").expect("있다");
    assert_eq!(after.state, InstanceState::Disabled, "상태가 덮어써졌다");

    // 정상 전이는 막지 않는다.
    let mut pending = discovered("orders-02");
    pending.state = InstanceState::Pending;
    r.upsert(&pending).await.expect("등록");
    r.set_state(&pending.id, InstanceState::Collecting)
        .await
        .expect("정상 전이가 막혔다");
    assert_eq!(
        r.get(&pending.id).await.expect("조회").expect("있다").state,
        InstanceState::Collecting
    );
}

#[tokio::test]
async fn two_consecutive_misses_stamp_deleted_but_keep_the_item() {
    let Some(r) = registry("reg-twomiss").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    r.mark_missing(&i.id, T0 + 1_000).await.expect("1회");
    let after = r
        .mark_missing(&i.id, T0 + 1_000 + ROUND_MS)
        .await
        .expect("2회");

    assert_eq!(after.missing_count, 2);
    // 삭제 도장은 **두 번째 미발견 시각**이다 — 라운드 간격만큼 뒤다.
    assert_eq!(after.deleted_at_ms, Some(T0 + 1_000 + ROUND_MS));
    assert_eq!(after.state, InstanceState::Deleted);

    // **항목이 지워지지 않았다** — 과거 슬로우 쿼리의 메타 참조가 살아 있어야 한다.
    assert!(
        r.get(&i.id).await.expect("조회").is_some(),
        "삭제 판정이 항목을 지웠다 — 과거 데이터의 인스턴스 메타를 영구히 잃는다"
    );
}

/// **한 라운드를 두 번 세지 않는다** (FR-DSC-07 의 임계값이 2라서 치명적이다).
///
/// 재조정은 수집 리더만 돌리지만, 멈췄다 되살아난 옛 리더와 새 리더가 같은 미발견을
/// 각각 올릴 수 있다. 임계값이 2이므로 **한 라운드로 삭제 도장이 찍힌다** — 살아 있는
/// 인스턴스가 등록부에서 사라진 것으로 판정된다(교차 리뷰 2회차).
#[tokio::test]
async fn a_second_write_in_the_same_round_does_not_count_twice() {
    let Some(r) = registry("reg-dupmiss").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    // 새 리더가 센다.
    let first = r.mark_missing(&i.id, T0 + 1_000).await.expect("1회");
    assert_eq!(first.missing_count, 1);

    // 멈췄다 되살아난 옛 리더가 **같은 라운드**를 다시 센다 (30초 뒤).
    let again = r.mark_missing(&i.id, T0 + 31_000).await.expect("중복");
    assert_eq!(again.missing_count, 1, "한 라운드가 두 번 세졌다");
    assert_eq!(again.deleted_at_ms, None, "한 라운드로 삭제 도장이 찍혔다");
    assert_ne!(again.state, InstanceState::Deleted);

    // 다음 라운드는 정상적으로 센다 — 가드가 기능을 지우지 않는다.
    let next = r
        .mark_missing(&i.id, T0 + 1_000 + ROUND_MS)
        .await
        .expect("2회");
    assert_eq!(next.missing_count, 2);
    assert_eq!(next.state, InstanceState::Deleted);
}

/// **되살아난 뒤 첫 미발견은 즉시 세야 한다.**
///
/// `mark_seen` 이 `missing_at_ms` 를 지우지 않으면 그 도장이 남아, 다시 사라졌을 때
/// 첫 미발견이 "이미 셌다" 로 거부되고 판정이 한 라운드 늦어진다.
#[tokio::test]
async fn a_reappearance_clears_the_dedupe_stamp() {
    let Some(r) = registry("reg-redupe").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    r.mark_missing(&i.id, T0 + 1_000).await.expect("1회");
    r.mark_seen(&i.id, T0 + 2_000).await.expect("재발견");

    // 라운드 간격보다 **짧은** 간격이어도 세야 한다 — 도장이 지워졌으므로.
    let after = r
        .mark_missing(&i.id, T0 + 3_000)
        .await
        .expect("재발견 후 1회");
    assert_eq!(
        after.missing_count, 1,
        "재발견 뒤 첫 미발견이 중복으로 거부됐다"
    );
}

/// 3회, 4회 미발견에도 **처음 사라진 시각이 유지돼야** 한다.
///
/// 매번 덮으면 30일 보존 기간이 계속 밀려 영구히 보존된다.
#[tokio::test]
async fn deleted_at_keeps_the_first_disappearance_time() {
    let Some(r) = registry("reg-firstgone").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    r.mark_missing(&i.id, T0 + 1_000).await.expect("1회");
    let first = r
        .mark_missing(&i.id, T0 + 1_000 + ROUND_MS)
        .await
        .expect("2회");
    let later = r
        .mark_missing(&i.id, T0 + 1_000 + 3 * ROUND_MS)
        .await
        .expect("3회");

    assert_eq!(
        later.deleted_at_ms, first.deleted_at_ms,
        "삭제 시각이 갱신됐다 — 보존 기간이 계속 밀려 영구 보존이 된다"
    );
    assert_eq!(later.missing_count, 3);
}

/// 중간에 다시 보이면 **카운터가 리셋되고 삭제 도장이 지워진다.**
#[tokio::test]
async fn reappearing_clears_the_counter_and_the_delete_stamp() {
    let Some(r) = registry("reg-revive").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    r.mark_missing(&i.id, T0 + 1_000).await.expect("1회");
    r.mark_missing(&i.id, T0 + 1_000 + ROUND_MS)
        .await
        .expect("2회 — 삭제 판정");
    assert!(
        r.get(&i.id)
            .await
            .expect("조회")
            .unwrap()
            .deleted_at_ms
            .is_some()
    );

    // 정지됐던 인스턴스가 다시 떴다.
    r.mark_seen(&i.id, T0 + 3_000).await.expect("재발견");
    let back = r.get(&i.id).await.expect("조회").expect("있음");
    assert_eq!(back.missing_count, 0);
    assert_eq!(
        back.deleted_at_ms, None,
        "삭제 도장이 남으면 되살아난 인스턴스가 목록에서 계속 안 보인다"
    );
    assert_eq!(back.last_seen_ms, T0 + 3_000);
}

/// **등록되지 않은 인스턴스에 미발견을 찍으면 실패해야 한다.**
///
/// 성공하면 `missing_count` 만 있는 반쪽 항목이 생기고, 그건 역직렬화에서 터진다.
#[tokio::test]
async fn marking_an_unknown_instance_fails_instead_of_creating_a_stub() {
    let Some(r) = registry("reg-stub").await else {
        return;
    };
    let ghost = discovered("never-registered");
    let e = r.mark_missing(&ghost.id, T0).await;
    assert!(e.is_err(), "없는 인스턴스에 카운터만 있는 항목을 만들었다");
    assert!(
        r.get(&ghost.id).await.expect("조회").is_none(),
        "반쪽 항목이 남았다"
    );
}

/// **재탐색이 사용자 오버라이드를 지우지 않아야 한다** (FR-DSC-04) — 저장소를 통해서도.
#[tokio::test]
async fn rediscovery_preserves_the_user_override_through_the_store() {
    let Some(r) = registry("reg-override").await else {
        return;
    };
    // 사용자가 UI 에서 dev 로 지정했다.
    let mut stored = discovered("orders-01");
    stored.env = dbmon_core::env::EnvResolution::resolve(
        dbmon_core::env::Env::Prd,
        Some(dbmon_core::env::Env::Dev),
    );
    stored.state = InstanceState::Collecting;
    r.upsert(&stored).await.expect("등록");

    // 5분 뒤 탐색이 다시 돈다 — 태그는 여전히 prd 다.
    let fresh = discovered("orders-01");
    let existing = r.get(&fresh.id).await.expect("조회");
    let merged = merge_discovered(existing.as_ref(), &fresh);
    r.upsert(&merged).await.expect("갱신");

    let got = r.get(&fresh.id).await.expect("조회").expect("있음");
    assert_eq!(
        got.env.override_value,
        Some(dbmon_core::env::Env::Dev),
        "재탐색이 사용자 오버라이드를 덮었다"
    );
    assert_eq!(
        got.state,
        InstanceState::Collecting,
        "재탐색이 수집 중 인스턴스를 Pending 으로 되돌렸다 — 수집이 5분마다 멈춘다"
    );
}

/// **삭제 도장을 찍을 때 `ttl` 이 함께 들어가야 한다** (FR-DSC-07 의 30일 보존).
///
/// 정리 잡을 따로 두면 그 잡이 죽었을 때 등록부가 조용히 자란다. DynamoDB TTL 이
/// 대신 지운다. 단, **살아 있는 인스턴스에는 TTL 이 없어야** 한다 — 있으면 30일 뒤
/// 정상 인스턴스가 등록부에서 사라진다.
#[tokio::test]
async fn retention_ttl_is_set_only_when_deleted() {
    let Some(r) = registry("reg-ttl").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    let raw = |suffix: &str| {
        let table = format!("dbmon-test-reg-ttl{suffix}");
        async move {
            client()
                .get_item()
                .table_name(table)
                .key(
                    "PK",
                    aws_sdk_dynamodb::types::AttributeValue::S("INST".into()),
                )
                .key(
                    "SK",
                    aws_sdk_dynamodb::types::AttributeValue::S(
                        "ap-northeast-2#123456789012/ap-northeast-2/orders-01".into(),
                    ),
                )
                .send()
                .await
                .expect("원시 조회")
                .item
                .expect("항목")
        }
    };

    // 살아 있는 동안에는 TTL 이 없어야 한다.
    assert!(
        !raw("").await.contains_key("ttl"),
        "살아 있는 인스턴스에 TTL 이 걸렸다 — 30일 뒤 등록부에서 사라진다"
    );

    // 2회 미발견 → 삭제 도장 + TTL.
    r.mark_missing(&i.id, T0 + 1_000).await.expect("1회");
    assert!(
        !raw("").await.contains_key("ttl"),
        "1회 미발견으로 TTL 이 걸렸다"
    );
    r.mark_missing(&i.id, T0 + 1_000 + ROUND_MS)
        .await
        .expect("2회");

    let item = raw("").await;
    let ttl: i64 = item
        .get("ttl")
        .expect("삭제 도장에 TTL 이 없다 — 등록부가 무한히 자란다")
        .as_n()
        .expect("N 타입")
        .parse()
        .expect("숫자");
    // 두 번째 미발견 시각 기준이다 — 라운드 간격만큼 뒤.
    let expected = (T0 + 1_000 + ROUND_MS) / 1000 + 30 * 86_400;
    assert_eq!(ttl, expected, "보존 기간이 30일이 아니다");

    // **되살아나면 TTL 이 지워져야 한다.** 남아 있으면 30일 뒤 조용히 사라진다.
    r.mark_seen(&i.id, T0 + 3_000).await.expect("재발견");
    assert!(
        !raw("").await.contains_key("ttl"),
        "되살아난 인스턴스에 TTL 이 남았다 — 30일 뒤 등록부에서 사라진다"
    );
}

/// **한 실행이 두 레코드로 갈리지 않아야 한다 (F5).**
///
/// # 이 테스트가 존재하는 이유
///
/// 두 경로의 시작 시각 추정 정밀도가 다르다:
///
/// | 경로 | 시작 시각 | 정밀도 |
/// |---|---|---|
/// | 실시간 | `now − PROCESSLIST.TIME × 1000` | **정수 초** |
/// | 슬로우로그 | `# Time − Query_time` | 밀리초 |
///
/// `record_id` 는 시작 시각을 초 버킷으로 접으므로 두 추정이 초 경계를 사이에 두면
/// **버킷이 갈린다** — 밀리초가 균등분포면 약 50%다. 그러면 한 실행이 레코드 2건이
/// 되고 한쪽은 정확 지표만, 다른 쪽은 플랜만 갖는다.
///
/// ⚠ 이전 단위 테스트는 "실시간" 키를 슬로우로그 값으로 만들어 비교해서
/// `RecordId::new(x) == RecordId::new(x)` 를 확인하고 있었다 — 공허했다.
/// 그리고 페이크 저장소에는 ±2초 폴백이 있었지만 **실제 저장소에는 없었다.**
#[tokio::test]
async fn one_execution_never_splits_across_second_buckets() {
    let Some(s) = store("bucket-split").await else {
        return;
    };
    let i = instance();

    // 진짜 시작 시각: 어떤 초의 300ms 지점.
    let true_start = T0 + 300;
    let duration = 8_004;

    // 실시간 경로: `PROCESSLIST.TIME` 이 정수 초라 최대 1초 오차가 난다.
    // 여기서는 800ms 이르게 추정 → **이전 초 버킷**으로 떨어진다.
    let realtime_start = true_start - 800;
    let mut realtime = sample(7001, realtime_start);
    realtime.state = SlowQueryState::InFlight;
    realtime.ended_at_ms = None;
    realtime.stats = Default::default(); // 실시간은 정확 지표를 줄 수 없다
    realtime.plan.normalized_json = Some(r#"{"query_block":{"table":"orders"}}"#.into());
    realtime.plan.tree_text = Some("-> Table scan on orders".into());
    realtime.duration_source = DurationSource::Polled;
    realtime.record_id = RecordId::new(&i, 7001, realtime_start);

    // 슬로우로그 경로: 정확한 시작(655ms).
    let mut slowlog = sample(7001, true_start);
    slowlog.state = SlowQueryState::Finalized;
    slowlog.ended_at_ms = Some(true_start + duration);
    slowlog.duration_ms = duration;
    slowlog.duration_source = DurationSource::Slowlog;
    slowlog.capture_source = CaptureSource::Slowlog;
    slowlog.stats = ExecStats {
        rows_examined: Some(180_000),
        ..Default::default()
    };
    slowlog.plan = Default::default(); // 슬로우로그에는 플랜이 없다
    slowlog.record_id = RecordId::new(&i, 7001, true_start);

    // **전제 확인**: 두 키가 실제로 다르다. 같으면 이 테스트가 무의미하다.
    assert_ne!(
        realtime.record_id, slowlog.record_id,
        "두 키가 같으면 이 테스트는 아무것도 검증하지 않는다"
    );

    s.upsert_merged(&realtime).await.expect("실시간 저장");
    s.upsert_merged(&slowlog).await.expect("백필 저장");

    // **레코드가 하나여야 한다.**
    let range = TimeRange::new(T0 - 10_000, T0 + 60_000).expect("구간");
    let all = s.list_by_instance(&i, range, 50).await.expect("조회");
    let mine: Vec<_> = all.iter().filter(|q| q.thread_id == 7001).collect();
    assert_eq!(
        mine.len(),
        1,
        "한 실행이 {}건으로 갈렸다 — 정확 지표와 플랜이 서로 다른 레코드에 있다",
        mine.len()
    );

    // 그리고 두 경로의 기여가 **모두** 살아 있어야 한다.
    let merged = mine[0];
    assert_eq!(
        merged.stats.rows_examined,
        Some(180_000),
        "슬로우로그의 정확 지표가 없다"
    );
    assert!(merged.plan.has_plan(), "실시간이 채운 플랜이 없다");
    assert_eq!(merged.duration_source, DurationSource::Slowlog);
    assert_eq!(merged.duration_ms, duration);
}

/// 반대 도착 순서도 같은 결과여야 한다 (병합 대칭성).
#[tokio::test]
async fn bucket_split_merge_is_order_independent() {
    let Some(s) = store("bucket-split-rev").await else {
        return;
    };
    let i = instance();
    let true_start = T0 + 655;

    let mut slowlog = sample(7002, true_start);
    slowlog.duration_source = DurationSource::Slowlog;
    slowlog.capture_source = CaptureSource::Slowlog;
    slowlog.stats = ExecStats {
        rows_examined: Some(180_000),
        ..Default::default()
    };
    slowlog.record_id = RecordId::new(&i, 7002, true_start);

    let mut realtime = sample(7002, true_start - 800);
    realtime.stats = Default::default();
    realtime.plan.normalized_json = Some(r#"{"query_block":{}}"#.into());
    realtime.plan.tree_text = Some("-> Table scan".into());
    realtime.record_id = RecordId::new(&i, 7002, true_start - 800);

    // 백필이 **먼저** 도착한다.
    s.upsert_merged(&slowlog).await.expect("백필 먼저");
    s.upsert_merged(&realtime).await.expect("실시간 나중");

    let range = TimeRange::new(T0 - 10_000, T0 + 60_000).expect("구간");
    let mine: Vec<_> = s
        .list_by_instance(&i, range, 50)
        .await
        .expect("조회")
        .into_iter()
        .filter(|q| q.thread_id == 7002)
        .collect();
    assert_eq!(mine.len(), 1, "도착 순서가 결과를 바꿨다");
    assert_eq!(mine[0].stats.rows_examined, Some(180_000));
    assert!(mine[0].plan.has_plan());
}

/// **상한에 걸릴 때 남는 것은 최근이어야 한다.**
///
/// `date_parts()` 는 오래된 순이므로, 파티션을 그 순서로 훑으면 `limit` 이 작을 때
/// **가장 오래된 날의 레코드만** 돌아온다. 목록·플랜·집계는 전부 최근을 보는
/// 화면이므로 그건 조용히 틀린 화면이 된다(9차 리뷰 후속 라운드에서 지적됨).
#[tokio::test]
async fn a_capped_listing_keeps_the_newest_records() {
    let Some(s) = store("newest-first").await else {
        return;
    };
    let i = instance();

    // 이틀에 걸쳐 저장한다 — 날짜 파티션이 둘이어야 이 테스트가 의미를 갖는다.
    const DAY_MS: i64 = 86_400_000;
    let older_ms = T0 - DAY_MS;
    let older = sample(9001, older_ms);
    let newer = sample(9002, T0);

    s.upsert_merged(&older).await.expect("옛 레코드 저장");
    s.upsert_merged(&newer).await.expect("새 레코드 저장");

    let range = TimeRange::new(older_ms - 10_000, T0 + 10_000).expect("구간");

    // 상한이 1이면 **새 것**이 와야 한다.
    let capped = s.list_by_instance(&i, range, 1).await.expect("조회");
    assert_eq!(capped.len(), 1);
    assert_eq!(
        capped[0].thread_id, 9002,
        "상한에 걸렸을 때 오래된 레코드가 남았다 — 화면이 옛 데이터를 최신으로 보여준다"
    );

    // 상한을 풀면 둘 다, 그리고 최신순이어야 한다.
    let all = s.list_by_instance(&i, range, 10).await.expect("조회");
    let mine: Vec<u64> = all
        .iter()
        .filter(|q| q.thread_id == 9001 || q.thread_id == 9002)
        .map(|q| q.thread_id)
        .collect();
    assert_eq!(mine, vec![9002, 9001], "최신순이 아니다");
}

/// **쌍둥이가 있으면 고아 스윕의 확정이 무효화됐다** (실측으로 발견).
///
/// 실시간 캡처와 슬로우로그가 처음 쓰기를 동시에 하면 둘 다 "없다" 를 보고,
/// `SK` 의 밀리초가 몇 ms 달라 `attribute_not_exists(PK)` 조건이 **양쪽 다 통과**한다.
/// 그러면 한 실행에 항목이 둘 생긴다 — 하나는 `finalized`, 하나는 `in_flight`.
///
/// 이 상태에서 고아 스윕이 진행 중 항목을 확정하려 하면, 초 버킷 조회(`begins_with`)가
/// **다른 항목**(작은 SK)을 돌려주고 병합 결과가 그쪽에 써진다. 진행 중 항목은 그대로
/// 남고 스윕은 **성공을 보고한다.** 로컬 스택에서 30초마다 같은 레코드를 영구히 다시
/// 확정하는 것을 보고 찾았다 — 화면에는 끝난 쿼리가 영원히 "진행 중" 이다.
#[tokio::test]
async fn abandoning_an_in_flight_twin_updates_that_very_record() {
    let Some(s) = store("twin-ghost").await else {
        return;
    };

    // 같은 초 버킷·같은 스레드, 밀리초만 3ms 다른 두 항목을 만든다.
    // **저장소를 통해서는 이 상태를 만들 수 없다**(그게 병합의 목적이다) — 동시
    // 첫 쓰기 경합의 결과를 재현하려고 원시 클라이언트로 두 번째 항목을 넣는다.
    let ghost_ms = T0 + 3;
    let mut ghost = sample(4242, ghost_ms);
    ghost.state = SlowQueryState::InFlight;
    ghost.ended_at_ms = None;
    ghost.owner_worker = Some("worker-gone".into());
    ghost.owner_epoch = Some(7);
    s.upsert_merged(&ghost).await.expect("유령 저장");

    // 쌍둥이(먼저 시작한 것으로 기록된 확정 레코드)를 원시로 넣는다. 저장소를 쓰면
    // 유령과 병합돼 한 항목이 된다.
    let twin = sample(4242, T0);
    let raw = client();
    let item: std::collections::HashMap<String, aws_sdk_dynamodb::types::AttributeValue> =
        serde_dynamo::to_item(&twin).expect("직렬화");
    let mut item = item;
    item.insert(
        "PK".into(),
        aws_sdk_dynamodb::types::AttributeValue::S(format!(
            "SQ#{}#2025-08-18",
            instance().as_str()
        )),
    );
    item.insert(
        "SK".into(),
        aws_sdk_dynamodb::types::AttributeValue::S(format!("{T0}#4242")),
    );
    raw.put_item()
        .table_name("dbmon-test-twin-ghost")
        .set_item(Some(item))
        .send()
        .await
        .expect("쌍둥이 원시 저장");

    // 이제 고아 스윕이 하는 일을 한다: **읽은 그 레코드**를 확정한다.
    let in_flight = s.list_in_flight(10).await.expect("진행 중 조회");
    assert_eq!(in_flight.len(), 1, "진행 중은 유령 하나여야 한다");
    let marked = dbmon::orphan::abandon(&in_flight[0], T0 + 90_000);
    s.upsert_merged(&marked).await.expect("확정");

    // **핵심 단정**: 진행 중 인덱스가 비어야 한다. 예전에는 쌍둥이만 갱신되고
    // 유령이 남아 스윕이 매 주기 같은 일을 반복했다.
    let still = s.list_in_flight(10).await.expect("재조회");
    assert!(
        still.is_empty(),
        "확정했다고 보고했는데 유령이 그대로다 — 스윕이 영구히 같은 레코드를 다시 확정한다: {:?}",
        still
            .iter()
            .map(|q| (q.thread_id, q.started_at_ms))
            .collect::<Vec<_>>()
    );
}

/// **병합이 시작 시각을 앞당긴 레코드는 그 뒤로도 계속 갱신돼야 한다.**
///
/// `SK` 는 `started_at_ms` 에서 파생된다. 그래서 병합이 시각을 앞당기면 **항목의
/// 필드와 물리 키가 어긋난다** — 항목은 처음 자리에 머물고 필드만 바뀐다(의도된 설계).
///
/// 예전에는 다음 갱신이 **필드로 키를 다시 만들었다.** 그러면 없는 자리를 겨냥해
/// 조건부 쓰기가 실패하고, 재시도 5회를 넘겨 `낙관적 잠금 재시도 5회 초과` 로 죽는다.
/// 로컬 스택에서 그 오류가 30초마다 반복되는 것을 보고 찾았다 — 그 레코드는 그 시점부터
/// **영구히 갱신 불가**였고, 진행 중이던 것은 영원히 진행 중으로 남았다.
#[tokio::test]
async fn a_record_stays_writable_after_a_merge_moves_its_start_time() {
    let Some(s) = store("key-drift").await else {
        return;
    };

    // ① 실시간 캡처: 시작 시각 추정이 500ms 늦다.
    let mut live = sample(7777, T0 + 500);
    live.state = SlowQueryState::InFlight;
    live.ended_at_ms = None;
    live.capture_source = CaptureSource::Processlist;
    live.stats.rows_examined = None;
    s.upsert_merged(&live).await.expect("실시간 저장");

    // ② 슬로우로그: 같은 실행을 더 이른 시각으로 안다 → 병합이 시각을 앞당긴다.
    let mut log = sample(7777, T0);
    log.capture_source = CaptureSource::Slowlog;
    log.stats.rows_examined = Some(4242);
    let merged = s.upsert_merged(&log).await.expect("슬로우로그 병합");
    assert_eq!(merged.started_at_ms, T0, "병합은 더 이른 시각을 채택한다");

    // ③ **그 다음 갱신이 성공해야 한다.** 여기서 예전 코드가 죽었다.
    let mut again = merged.clone();
    again.stats.rows_sent = Some(7);
    let third = s.upsert_merged(&again).await.expect(
        "병합된 레코드를 다시 갱신할 수 있어야 한다 — 여기서 '낙관적 잠금 재시도 5회 초과' 가 났다",
    );
    assert_eq!(third.stats.rows_sent, Some(7));
    assert_eq!(
        third.stats.rows_examined,
        Some(4242),
        "정확 지표가 남아야 한다"
    );

    // ④ **항목이 하나여야 한다.** 키를 다시 만들면 쌍둥이가 생긴다.
    let range = TimeRange::new(T0 - 10_000, T0 + 10_000).expect("구간");
    let all = s
        .list_by_instance(&instance(), range, 50)
        .await
        .expect("조회");
    let mine: Vec<_> = all.iter().filter(|q| q.thread_id == 7777).collect();
    assert_eq!(
        mine.len(),
        1,
        "한 실행에 항목이 {}개다 — 표에 두 줄, 통계는 두 배가 된다",
        mine.len()
    );
    assert_eq!(mine[0].stats.rows_sent, Some(7));
}

/// **동시 갱신 중 하나는 반드시 조건에 걸려 다시 읽어야 한다.**
///
/// `SK = <읽은 SK>` 조건은 `PutItem` 이 이미 그 키를 겨냥하므로 "그 자리에 항목이
/// 있다" 만 확인한다 — 두 워커가 같은 항목을 읽고 각자 병합해 쓰면 **둘 다 성공하고
/// 나중 것이 앞의 것을 덮는다.** 실행계획이나 정확 지표가 조용히 사라질 수 있다.
///
/// 여기서는 두 쓰기를 `join!` 으로 **겹쳐서** 던진다. 둘의 읽기가 실제로 겹치면 옛
/// 코드에서는 한쪽 기여가 사라지고, 낙관적 잠금이 있으면 진 쪽이 다시 읽어 병합한다.
/// (직렬화되면 이 테스트는 그냥 통과한다 — 약해질 뿐 틀리지는 않는다.)
#[tokio::test]
async fn a_concurrent_update_retries_instead_of_overwriting() {
    let Some(s) = store("rev-lock").await else {
        return;
    };

    let base = sample(3131, T0);
    s.upsert_merged(&base).await.expect("최초 저장");

    // 하나는 플랜을, 하나는 더 큰 정확 지표를 얹는다. 병합은 `max` 이므로 둘 다 남아야 한다.
    let mut with_plan = base.clone();
    with_plan.plan.normalized_json = Some(r#"{"query_block":{"select_id":1}}"#.into());
    with_plan.plan.fingerprint = Some("fp-1".into());

    let mut with_stats = base.clone();
    with_stats.capture_source = CaptureSource::Slowlog;
    with_stats.stats.rows_examined = Some(999_999);

    let (a, b) = tokio::join!(s.upsert_merged(&with_plan), s.upsert_merged(&with_stats));
    a.expect("플랜 쓰기");
    b.expect("지표 쓰기");

    let got = s
        .get(&base.record_id)
        .await
        .expect("조회")
        .expect("레코드가 있어야 한다");
    assert!(
        got.plan.has_plan(),
        "동시 쓰기가 플랜을 덮어썼다 — 낙관적 잠금이 동작하지 않는다"
    );
    assert_eq!(
        got.stats.rows_examined,
        Some(999_999),
        "동시 쓰기가 정확 지표를 덮어썼다"
    );
}

/// **저장소가 연속한 두 실행을 합치지 않는다** (10라운드 지적).
///
/// 종료를 관측하지 못한 레코드의 끝은 "사라진 것을 알아챈 폴링" 까지 늘어난다. 그
/// 늘어난 구간이 다음 실행과 겹쳐 보이고, ±2초 창 안이면 **두 실행이 한 레코드로
/// 합쳐진다 — 되돌릴 수 없다.** 판정은 창이 아니라 추정 오차 폭(1초)이어야 한다.
#[tokio::test]
async fn the_store_does_not_merge_a_rerun_into_the_previous_execution() {
    let Some(s) = store("rerun-merge").await else {
        return;
    };

    // ① 앞 실행: 1.2초에 끝났지만 폴링이 1.9초에 사라진 것을 알아챘다.
    let mut first = sample(5150, T0);
    first.duration_ms = 1_900;
    first.capture_source = CaptureSource::Processlist;
    first.stats.rows_examined = None;
    s.upsert_merged(&first).await.expect("앞 실행 저장");

    // ② 곧바로 다시 돈 같은 쿼리 (1.3초 뒤 시작, 슬로우로그가 정확히 안다).
    //    창(2초) 안이고 구간도 겹쳐 보이지만 **다른 실행이다.**
    let mut rerun = sample(5150, T0 + 1_300);
    rerun.duration_ms = 1_100;
    rerun.capture_source = CaptureSource::Slowlog;
    rerun.stats.rows_examined = Some(77);
    s.upsert_merged(&rerun).await.expect("재실행 저장");

    let range = TimeRange::new(T0 - 10_000, T0 + 20_000).expect("구간");
    let all = s
        .list_by_instance(&instance(), range, 50)
        .await
        .expect("조회");
    let mine: Vec<_> = all.iter().filter(|q| q.thread_id == 5150).collect();
    assert_eq!(
        mine.len(),
        2,
        "연속한 두 실행이 한 레코드로 합쳐졌다 — 실행 하나가 영구히 사라진다"
    );

    // ③ 반면 **같은 실행의 두 관측**(시작 3ms 차이)은 하나로 합쳐져야 한다.
    let mut live = sample(5151, T0 + 3);
    live.state = SlowQueryState::InFlight;
    live.ended_at_ms = None;
    live.capture_source = CaptureSource::Processlist;
    s.upsert_merged(&live).await.expect("실시간 저장");
    let mut log = sample(5151, T0);
    log.capture_source = CaptureSource::Slowlog;
    // 병합은 `max` 를 취하므로 샘플 기본값보다 큰 값을 쓴다.
    log.stats.rows_examined = Some(999_999);
    s.upsert_merged(&log).await.expect("슬로우로그 병합");
    let all = s
        .list_by_instance(&instance(), range, 50)
        .await
        .expect("조회");
    let twins: Vec<_> = all.iter().filter(|q| q.thread_id == 5151).collect();
    assert_eq!(twins.len(), 1, "같은 실행의 두 관측을 합치지 못했다 (F5)");
    assert_eq!(
        twins[0].stats.rows_examined,
        Some(999_999),
        "합쳤지만 슬로우로그의 정확 지표가 반영되지 않았다"
    );
}

/// **같은 초에 시작한 다른 문장(중첩)이 한 레코드로 섞이지 않는다** (11라운드 지적).
///
/// 멱등 키는 `(인스턴스, 스레드, 시작 초)` 다. 스토어드 프로시저의 바깥쪽/안쪽 문장은
/// 같은 스레드·같은 초에 시작하므로 **키가 같다.** 그대로 병합하면 두 문장이 한
/// 레코드가 되고 한쪽 다이제스트는 사라진다 — 그 문장은 어느 다이제스트 그룹에도
/// 속하지 않게 된다.
#[tokio::test]
async fn two_statements_in_the_same_second_bucket_stay_separate() {
    let Some(s) = store("same-second-nested").await else {
        return;
    };

    // 바깥쪽 문장: 같은 초의 100ms 지점에서 시작, 2.1초 걸린다.
    let mut outer = sample(6161, T0 + 100);
    outer.app_digest = "digest-outer".into();
    outer.duration_ms = 2_100;
    s.upsert_merged(&outer).await.expect("바깥쪽 저장");

    // 안쪽 문장: 같은 초의 400ms 지점, 다른 다이제스트. `record_id` 는 **같다.**
    let mut inner = sample(6161, T0 + 400);
    inner.app_digest = "digest-inner".into();
    inner.duration_ms = 900;
    inner.is_nested = true;
    assert_eq!(
        outer.record_id.as_str(),
        inner.record_id.as_str(),
        "이 테스트는 두 문장의 멱등 키가 같을 때를 본다"
    );
    s.upsert_merged(&inner).await.expect("안쪽 저장");

    let range = TimeRange::new(T0 - 10_000, T0 + 20_000).expect("구간");
    let all = s
        .list_by_instance(&instance(), range, 50)
        .await
        .expect("조회");
    let mine: Vec<_> = all.iter().filter(|q| q.thread_id == 6161).collect();
    assert_eq!(
        mine.len(),
        2,
        "같은 초의 두 문장이 한 레코드로 섞였다 — 한쪽 다이제스트가 사라진다"
    );
    let digests: std::collections::BTreeSet<&str> =
        mine.iter().map(|q| q.app_digest.as_str()).collect();
    assert!(digests.contains("digest-outer") && digests.contains("digest-inner"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// 정지 스코프 — 저장소가 진실이다
// ═══════════════════════════════════════════════════════════════════════════════

use dbmon::store::pause::DynamoPauseStore;
use dbmon_core::pause::{PauseScope, PauseSet};
use dbmon_core::ports::PauseStore;

/// 프로덕션은 `config_table` 을 쓴다. 여기서 데이터 테이블에 붙이는 이유는 스키마가
/// PK/SK 두 개로 같고, **이 테스트가 검증하는 것은 키 배치와 조건부 쓰기**이기
/// 때문이다 — 테이블 생성 코드를 하나 더 만들 이유가 없다.
async fn pause_store(name: &str) -> Option<DynamoPauseStore> {
    let _ = store(name).await?;
    Some(DynamoPauseStore::new(
        client(),
        format!("dbmon-test-{name}"),
    ))
}

/// **왕복.** 저장한 스코프를 그대로 읽어야 한다 — 키 문자열은 컴파일러가 검사하지
/// 않으므로 이 확인이 유일한 방법이다.
#[tokio::test]
async fn pause_scopes_round_trip_through_the_store() {
    let Some(s) = pause_store("pause-roundtrip").await else {
        return;
    };
    let one = PauseScope::Instance(instance());
    let env = PauseScope::Env(Env::Prd);

    assert_eq!(s.list().await.expect("조회"), PauseSet::default());

    s.pause(&env, "operator@example.com", T0)
        .await
        .expect("정지");
    s.pause(&one, "operator@example.com", T0 + 5_000)
        .await
        .expect("정지");

    let set = s.list().await.expect("조회");
    assert_eq!(set.len(), 2);
    assert_eq!(set.since_ms(&env), Some(T0));
    assert_eq!(set.since_ms(&one), Some(T0 + 5_000));
    assert!(!set.is_all_paused(), "전체를 멈춘 적이 없다");
    assert!(set.is_paused_id(&instance(), Env::Prd));

    // 재개는 그 스코프만 지운다.
    s.resume(&env).await.expect("재개");
    let set = s.list().await.expect("조회");
    assert_eq!(set.len(), 1);
    assert_eq!(set.since_ms(&one), Some(T0 + 5_000));

    // **없는 것을 지워도 성공이다** — 두 사람이 같이 눌러도 두 번째가 오류로 보이면 안 된다.
    s.resume(&env).await.expect("멱등 재개");
    s.resume(&one).await.expect("재개");
    assert!(s.list().await.expect("조회").is_empty());
}

/// **이미 멈춘 스코프를 다시 눌러도 시작 시각이 유지된다.**
///
/// `PutItem` 으로 덮으면 화면이 "3분 전부터 멈춤" 을 말할 수 없다. `if_not_exists`
/// 조건이 실제로 걸려 있는지는 저장소를 거쳐야만 확인된다.
#[tokio::test]
async fn pausing_an_already_paused_scope_keeps_the_original_time() {
    let Some(s) = pause_store("pause-keeps-time").await else {
        return;
    };
    let all = PauseScope::All;
    s.pause(&all, "first", T0).await.expect("정지");
    s.pause(&all, "second", T0 + 600_000).await.expect("재정지");

    let set = s.list().await.expect("조회");
    assert_eq!(set.since_ms(&all), Some(T0), "시작 시각이 덮였다");
    assert!(set.is_all_paused());

    // 재개 후 다시 멈추면 그때가 시작이다.
    s.resume(&all).await.expect("재개");
    s.pause(&all, "third", T0 + 900_000).await.expect("정지");
    assert_eq!(
        s.list().await.expect("조회").since_ms(&all),
        Some(T0 + 900_000)
    );
}
