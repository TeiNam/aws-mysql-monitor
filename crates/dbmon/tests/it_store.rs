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
#[tokio::test]
async fn two_consecutive_misses_stamp_deleted_but_keep_the_item() {
    let Some(r) = registry("reg-twomiss").await else {
        return;
    };
    let i = discovered("orders-01");
    r.upsert(&i).await.expect("등록");

    r.mark_missing(&i.id, T0 + 1_000).await.expect("1회");
    let after = r.mark_missing(&i.id, T0 + 2_000).await.expect("2회");

    assert_eq!(after.missing_count, 2);
    assert_eq!(after.deleted_at_ms, Some(T0 + 2_000));
    assert_eq!(after.state, InstanceState::Deleted);

    // **항목이 지워지지 않았다** — 과거 슬로우 쿼리의 메타 참조가 살아 있어야 한다.
    assert!(
        r.get(&i.id).await.expect("조회").is_some(),
        "삭제 판정이 항목을 지웠다 — 과거 데이터의 인스턴스 메타를 영구히 잃는다"
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
    let first = r.mark_missing(&i.id, T0 + 2_000).await.expect("2회");
    let later = r.mark_missing(&i.id, T0 + 999_000).await.expect("3회");

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
    r.mark_missing(&i.id, T0 + 2_000)
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
