//! 슬로우 로그 엔트리 → 도메인 레코드, 그리고 백필 실행 (M2-7).
//!
//! # 왜 `upsert_merged` 로 넣는가
//!
//! 실시간 캡처가 이미 저장한 레코드와 **같은 실행**을 가리킨다. `PutItem` 이면
//! 실시간 쪽이 채운 것(플랜, 다이제스트 스냅샷)을 덮어쓴다. 병합은 교환·결합법칙을
//! 만족하므로(R43, 4.25M 쌍 전수 검증) 어느 쪽이 먼저 도착해도 결과가 같다.
//!
//! # 무엇이 슬로우로그의 권위값인가
//!
//! | 필드 | 권위 | 근거 |
//! |---|---|---|
//! | `duration_ms` | **슬로우로그** | `Query_time` 은 서버가 측정한 값이다 |
//! | `rows_examined` 등 | **슬로우로그** | 실행 중에는 0 이다([19 §G2]) |
//! | `sql_text` | 상황에 따라 | 슬로우로그는 절단되지 않지만 리터럴이 들어 있다 |
//! | `plan` | 실시간 | 슬로우로그에는 없다 |
//!
//! `merge` 가 그 규칙을 이미 구현한다 — 여기서 다시 판정하지 않는다.

use std::sync::Arc;

use dbmon_core::error::Result;
use dbmon_core::instance::Instance;
use dbmon_core::ports::SlowQueryStore;
use dbmon_core::slow_query::{
    CaptureSource, DurationSource, ExecStats, LiteralPolicy, SlowQuery, SlowQueryState,
};
use dbmon_core::time::EpochMs;

use super::SlowLogEntry;

/// 저장할 SQL 텍스트의 바이트 상한.
///
/// DynamoDB 항목 한도(400KB)보다 훨씬 작게 둔다 — 한 항목에 플랜·지표도 들어간다.
const MAX_STORED_SQL_BYTES: usize = 64 * 1024;

/// 백필 결과. 조용히 넘기지 않기 위해 센다.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillStats {
    pub merged: usize,
    /// 도메인 레코드를 만들 수 없어 버린 수 (식별자 위반 등).
    pub unnormalizable: usize,
    /// **마스킹을 포기하고 텍스트 없이 저장한 수.**
    ///
    /// `unnormalizable` 과 섞으면 마스킹 회귀가 파서 문제로 오인된다.
    pub masking_degraded: usize,
    pub errors: usize,
    /// **로그를 읽지 못한 인스턴스 수.**
    ///
    /// 이게 없으면 "로그를 못 읽었다" 와 "읽을 게 없었다" 가 구분되지 않는다 —
    /// 전자는 장애고 후자는 정상이다.
    pub fetch_errors: usize,
    /// **한 라운드에 다 읽지 못한 인스턴스 수** (`has_more`).
    ///
    /// `FilterLogEvents` 는 페이지당 10,000건 상한이 있다. 세지 않으면 바쁜
    /// 인스턴스가 계속 뒤처지는 것을 알 수 없다.
    pub incomplete: usize,
}

/// 엔트리를 도메인 레코드로 옮긴다.
///
/// `sql_text` 는 **정책에 따라 마스킹**한다 — 슬로우 로그에는 리터럴이 그대로 들어
/// 있으므로, 이 단계를 건너뛰면 `masked` 정책인데 리터럴이 저장된다.
/// 그건 되돌릴 수 없는 종류의 실수다.
pub fn to_slow_query(
    entry: &SlowLogEntry,
    instance: &Instance,
    literal_policy: LiteralPolicy,
    captured_at_ms: EpochMs,
) -> Option<SlowQuery> {
    use dbmon_core::ids::RecordId;

    let normalized = dbmon_normalize::normalize(&entry.sql_text);

    // **텍스트를 포기하되 레코드는 남긴다.**
    //
    // 처음에는 인용부호 미종료·리터럴 잔류에서 엔트리 전체를 버렸다. 그러면
    // **렉서가 못 다루는 문장 — 즉 가장 조사할 필요가 큰 문장 — 의 `rows_examined` 와
    // 정확한 소요 시간이 영구히 없다.** 실시간 경로는 같은 상황에서 텍스트만
    // 포기하고 레코드를 남긴다(`masking_degraded`). 여기서도 그렇게 한다.
    let mut degraded = false;
    let mut sql_text = match literal_policy {
        // 저장하지 않는다. 다이제스트만 남는다.
        LiteralPolicy::Off => None,
        // **정규 텍스트만 저장한다.** 슬로우 로그에는 리터럴이 그대로 있으므로
        // 여기서 원문을 쓰면 정책이 `masked` 인데 리터럴이 저장된다.
        LiteralPolicy::Masked => {
            // 인용부호가 닫히지 않았으면 파싱을 신뢰할 수 없다 — 마스킹이 리터럴을
            // 남길 수 있다. 리터럴 잔류도 같다. **텍스트만 버린다.**
            if normalized.unterminated_quote || normalized.check_no_literals().is_err() {
                degraded = true;
                None
            } else {
                Some(normalized.canonical.clone())
            }
        }
        // 정책이 원문 저장을 허용한다. `full_restricted` 의 열람 제한은 조회 계층의 일이다.
        LiteralPolicy::Full | LiteralPolicy::FullRestricted => Some(entry.sql_text.clone()),
    };

    // **길이 상한.** 없으면 400KB 를 넘는 문장 하나가 `upsert_merged` 를 결정적으로
    // 실패시키고, 그 실패가 `errors > 0` 이 되어 **체크포인트가 전진하지 못한다** —
    // 다음 라운드도 같은 엔트리로 실패해 그 인스턴스의 백필이 영구히 멈춘다.
    // DB 계정 하나로 유발할 수 있었다.
    let mut truncated = normalized.truncated;
    if let Some(t) = sql_text.as_mut()
        && t.len() > MAX_STORED_SQL_BYTES
    {
        // UTF-8 경계에서 자른다.
        let mut cut = MAX_STORED_SQL_BYTES;
        while cut > 0 && !t.is_char_boundary(cut) {
            cut -= 1;
        }
        t.truncate(cut);
        truncated = true;
    }

    let record_id = RecordId::new(&instance.id, entry.thread_id, entry.started_at_ms);
    Some(SlowQuery {
        record_id,
        instance_id: instance.id.clone(),
        cluster_id: instance.cluster_id.clone(),
        env: instance.env.effective,
        engine: instance.engine,
        engine_version: instance.engine_version.raw.clone(),
        // 슬로우 로그에 실린 것은 **이미 끝난** 실행이다.
        state: SlowQueryState::Finalized,
        thread_id: entry.thread_id,
        schema_name: entry.schema_name.clone(),
        db_user: entry.db_user.clone(),
        db_host: entry.db_host.clone(),
        started_at_ms: entry.started_at_ms,
        // `Time − Query_time` 이 이미 소수점을 담으므로 별도 정밀값이 필요 없다.
        started_at_ms_precise: None,
        ended_at_ms: Some(entry.ended_at_ms),
        captured_at_ms,
        duration_ms: entry.duration_ms,
        // **슬로우로그가 권위값이다** — `merge` 가 이 값을 이긴 것으로 취급한다.
        duration_source: DurationSource::Slowlog,
        sql_text,
        // **원문은 절단되지 않지만 저장하는 텍스트는 절단될 수 있다.**
        // `canonical` 은 8,192자에서 잘리고, 위 길이 상한도 자른다. 플래그를
        // `false` 로 박아 두면 UI 가 잘린 SQL 에 절단 배지를 못 붙인다.
        sql_text_truncated: truncated,
        // 마스킹을 포기했으면 손실이 있었다는 사실을 남긴다.
        sql_text_lossy: degraded,
        literal_policy,
        literal_policy_at_ms: captured_at_ms,
        app_digest: normalized.app_digest.clone(),
        digest_algo_version: dbmon_normalize::DIGEST_ALGO_VERSION,
        mysql_digest: None,
        statement_type: normalized.statement_type,
        is_nested: false,
        stats: ExecStats {
            rows_examined: entry.rows_examined,
            rows_sent: entry.rows_sent,
            rows_affected: entry.rows_affected,
            lock_time_ms: Some(entry.lock_time_ms),
            ..Default::default()
        },
        plan: Default::default(),
        capture_source: CaptureSource::Slowlog,
        owner_worker: None,
        owner_epoch: None,
        last_seen_at_ms: None,
        abandoned_reason: None,
        long_running: false,
    })
}

/// 엔트리들을 저장소에 병합한다.
///
/// **한 건 실패로 나머지를 버리지 않는다.** 백필은 과거 데이터를 채우는 일이고,
/// 한 엔트리가 이상해서 그 파일 전체를 잃으면 그 구간이 영구히 빈다.
pub async fn backfill<S: SlowQueryStore>(
    store: Arc<S>,
    entries: &[SlowLogEntry],
    instance: &Instance,
    literal_policy: LiteralPolicy,
    now_ms: EpochMs,
) -> Result<BackfillStats> {
    let mut stats = BackfillStats::default();

    for entry in entries {
        let Some(q) = to_slow_query(entry, instance, literal_policy, now_ms) else {
            // 도메인 레코드를 만들 수 없었다(식별자 규칙 위반 등).
            stats.unnormalizable += 1;
            continue;
        };
        if q.sql_text_lossy {
            // 텍스트는 포기했지만 레코드는 저장한다 — 정확 지표가 목적이다.
            stats.masking_degraded += 1;
        }
        match store.upsert_merged(&q).await {
            Ok(_) => stats.merged += 1,
            Err(e) => {
                tracing::warn!(
                    instance = %instance.id.as_str(),
                    thread_id = entry.thread_id,
                    error = %crate::telemetry::Scrubbed(&e),
                    "슬로우로그 백필 병합 실패 — 나머지를 계속한다"
                );
                stats.errors += 1;
            }
        }
    }

    if stats.unnormalizable > 0 {
        tracing::warn!(
            count = stats.unnormalizable,
            "도메인 레코드를 만들 수 없는 엔트리를 건너뛰었다"
        );
    }
    if stats.masking_degraded > 0 {
        // **마스킹 회귀의 신호다.** 파서 문제와 섞이지 않게 따로 센다.
        tracing::warn!(
            count = stats.masking_degraded,
            "마스킹을 신뢰할 수 없어 SQL 텍스트 없이 저장했다 (지표는 남는다)"
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::fakes::FakeSlowQueryStore;

    fn instance() -> Instance {
        let raw = crate::aws::discovery::RawDbInstance {
            identifier: "orders-01".into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: "ap-northeast-2".into(),
            endpoint_address: Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com".into()),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        crate::aws::discovery::to_instance(
            &raw,
            "123456789012",
            &dbmon_core::env::EnvMapping::default(),
            1_000,
        )
        .expect("매핑")
    }

    fn entry(sql: &str) -> SlowLogEntry {
        SlowLogEntry {
            ended_at_ms: 1_787_147_334_659,
            started_at_ms: 1_787_147_326_655,
            duration_ms: 8_004,
            lock_time_ms: 250,
            thread_id: 25_192,
            db_user: Some("app".into()),
            db_host: Some("10.0.3.44".into()),
            schema_name: Some("shop".into()),
            rows_sent: Some(1),
            rows_examined: Some(900_000),
            rows_affected: None,
            sql_text: sql.into(),
        }
    }

    /// **슬로우로그가 정확 지표를 채운다.** 실시간 경로는 이 값을 줄 수 없다([19 §G2]).
    #[test]
    fn carries_the_precise_metrics_realtime_cannot_provide() {
        let q = to_slow_query(
            &entry("SELECT a FROM t WHERE id = 42"),
            &instance(),
            LiteralPolicy::Masked,
            9_999,
        )
        .expect("변환");

        assert_eq!(q.stats.rows_examined, Some(900_000));
        assert_eq!(q.stats.rows_sent, Some(1));
        assert_eq!(q.stats.lock_time_ms, Some(250));
        assert_eq!(q.duration_ms, 8_004);
        assert_eq!(q.duration_source, DurationSource::Slowlog);
        assert_eq!(q.capture_source, CaptureSource::Slowlog);
        assert_eq!(q.state, SlowQueryState::Finalized);
        assert!(!q.sql_text_truncated, "슬로우로그는 절단하지 않는다");
    }

    /// **`masked` 정책이면 리터럴을 저장하지 않는다.**
    ///
    /// 슬로우 로그에는 리터럴이 그대로 있다. 이 단계를 건너뛰면 정책이 `masked` 인데
    /// 원문이 저장되고, 그건 되돌릴 수 없다.
    #[test]
    fn masked_policy_never_stores_literals_from_the_log() {
        let q = to_slow_query(
            &entry("SELECT a FROM t WHERE email = 'someone@example.com' AND id = 42"),
            &instance(),
            LiteralPolicy::Masked,
            0,
        )
        .expect("변환");

        let stored = q.sql_text.as_deref().unwrap_or_default();
        assert!(
            !stored.contains("someone@example.com"),
            "리터럴이 저장됐다: {stored}"
        );
        assert!(!stored.contains("42"), "숫자 리터럴이 저장됐다: {stored}");
    }

    /// `off` 정책이면 SQL 을 아예 저장하지 않는다.
    #[test]
    fn off_policy_stores_no_sql_at_all() {
        let q = to_slow_query(
            &entry("SELECT a FROM t WHERE id = 42"),
            &instance(),
            LiteralPolicy::Off,
            0,
        )
        .expect("변환");
        assert_eq!(q.sql_text, None);
        // 다이제스트는 남는다 — 그게 없으면 집계가 불가능하다.
        assert!(!q.app_digest.is_empty());
    }

    /// **마스킹을 신뢰할 수 없으면 텍스트만 포기하고 레코드는 남긴다.**
    ///
    /// 엔트리를 버리면 렉서가 못 다루는 문장 — 가장 조사할 필요가 큰 문장 — 의
    /// `rows_examined` 와 정확한 소요 시간이 영구히 없다.
    #[test]
    fn unmaskable_sql_keeps_the_record_without_text() {
        // 닫히지 않은 인용부호 — 파싱을 신뢰할 수 없다.
        let mut e = entry("SELECT a FROM t WHERE s = 'unterminated");
        e.rows_examined = Some(900_000);

        let q =
            to_slow_query(&e, &instance(), LiteralPolicy::Masked, 0).expect("레코드는 남아야 한다");
        assert_eq!(q.sql_text, None, "신뢰할 수 없는 마스킹 결과를 저장했다");
        assert!(q.sql_text_lossy, "손실 사실이 기록되지 않았다");
        // **정확 지표는 살아 있다** — 이게 이 경로의 목적이다.
        assert_eq!(q.stats.rows_examined, Some(900_000));
        assert_eq!(q.duration_ms, 8_004);
    }

    /// **저장 텍스트가 길면 자르고 플래그를 세운다.**
    ///
    /// 없으면 400KB 초과 문장 하나가 `upsert_merged` 를 결정적으로 실패시키고,
    /// 그 실패가 체크포인트를 막아 그 인스턴스의 백필이 영구히 멈춘다.
    #[test]
    fn oversized_sql_is_truncated_not_left_to_fail_the_write() {
        let huge = format!("SELECT {}", "a".repeat(200_000));
        let q = to_slow_query(&entry(&huge), &instance(), LiteralPolicy::Full, 0).expect("변환");
        let stored = q.sql_text.as_deref().unwrap_or_default();
        assert!(
            stored.len() <= 64 * 1024,
            "저장 텍스트가 {}바이트다",
            stored.len()
        );
        assert!(
            q.sql_text_truncated,
            "절단 배지가 없다 — 잘린 SQL 을 전문으로 오독한다"
        );
    }

    /// **`record_id` 는 정확한 시작 시각에서 만든다.**
    ///
    /// ⚠ 이전 테스트는 "실시간" 키를 슬로우로그 값으로 만들어 비교해서
    /// `RecordId::new(x) == RecordId::new(x)` 를 확인했다 — 공허했다(2차 리뷰가 지적).
    ///
    /// **두 경로의 키가 초 버킷을 걸쳐 갈릴 수 있다는 것이 실제 문제**이고,
    /// 그건 저장소의 ±2초 보조 조회가 해결한다 —
    /// `it_store::one_execution_never_splits_across_second_buckets` 가 실제 저장소로
    /// 검증한다. 여기서는 키의 구성만 고정한다.
    #[test]
    fn record_id_is_built_from_the_precise_start() {
        use dbmon_core::ids::RecordId;

        let e = entry("SELECT 1");
        let q = to_slow_query(&e, &instance(), LiteralPolicy::Masked, 0).expect("변환");
        assert_eq!(
            q.record_id,
            RecordId::new(&instance().id, e.thread_id, e.started_at_ms)
        );

        // **초 버킷을 걸치면 키가 달라진다** — 이 사실을 명시적으로 고정한다.
        let earlier = RecordId::new(&instance().id, e.thread_id, e.started_at_ms - 800);
        assert_ne!(
            q.record_id, earlier,
            "초 버킷이 갈리는 사실이 사라졌다 — 저장소의 보조 조회가 왜 필요한지의 근거다"
        );
    }

    /// **한 건이 정규화에 실패해도 나머지를 병합한다.**
    #[tokio::test]
    async fn one_unnormalizable_entry_does_not_lose_the_rest() {
        let store = Arc::new(FakeSlowQueryStore::default());
        let entries = vec![
            entry("SELECT a FROM t WHERE id = 1"),
            // 파라미터 자리표는 정규화가 거부한다(실행된 문장이 아니다).
            entry("SELECT a FROM t WHERE id = ?"),
            entry("SELECT b FROM u WHERE id = 2"),
        ];
        let stats = backfill(
            Arc::clone(&store),
            &entries,
            &instance(),
            LiteralPolicy::Masked,
            0,
        )
        .await
        .expect("백필");

        assert_eq!(stats.merged + stats.unnormalizable, 3);
        assert!(stats.merged >= 2, "{stats:?}");
        assert_eq!(stats.errors, 0);
    }

    /// **백필이 실시간 레코드의 플랜을 덮지 않는다** (F5).
    ///
    /// 슬로우 로그에는 플랜이 없다. `PutItem` 이면 실시간 쪽이 채운 플랜이 사라진다.
    #[tokio::test]
    async fn backfill_merges_instead_of_overwriting_the_realtime_record() {
        let store = Arc::new(FakeSlowQueryStore::default());
        let e = entry("SELECT a FROM t WHERE id = 1");

        // 실시간 경로가 먼저 저장했다 — 지표는 없고 소요는 부정확하다.
        let mut realtime = to_slow_query(&e, &instance(), LiteralPolicy::Masked, 0).expect("변환");
        realtime.duration_source = DurationSource::Polled;
        realtime.duration_ms = 7_000;
        realtime.stats = Default::default();
        realtime.plan.normalized_json = Some(r#"{"query_block":{}}"#.into());
        realtime.plan.tree_text = Some("-> Table scan on t".into());
        store.upsert_merged(&realtime).await.expect("실시간 저장");

        // 백필이 도착한다.
        backfill(
            Arc::clone(&store),
            std::slice::from_ref(&e),
            &instance(),
            LiteralPolicy::Masked,
            0,
        )
        .await
        .expect("백필");

        let got = store
            .get(&realtime.record_id)
            .await
            .expect("조회")
            .expect("있음");
        // 슬로우로그의 소요·지표가 이긴다.
        assert_eq!(got.duration_ms, 8_004);
        assert_eq!(got.duration_source, DurationSource::Slowlog);
        assert_eq!(got.stats.rows_examined, Some(900_000));
        // **실시간이 채운 플랜은 살아 있다.**
        assert!(
            got.plan.has_plan(),
            "백필이 플랜을 덮었다 — 슬로우로그에는 플랜이 없으므로 영구히 잃는다"
        );
        assert_eq!(got.plan.tree_text.as_deref(), Some("-> Table scan on t"));
    }
}
