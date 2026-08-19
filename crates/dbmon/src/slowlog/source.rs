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

/// 백필 결과. 조용히 넘기지 않기 위해 센다.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillStats {
    pub merged: usize,
    /// 정규화·마스킹에서 버려진 수.
    pub unnormalizable: usize,
    pub errors: usize,
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

    // **인용부호가 닫히지 않았으면 파싱을 신뢰할 수 없다.** 그 상태로 마스킹하면
    // 리터럴이 남을 수 있다 — 이 프로젝트에서 마스킹 누출이 다섯 번 재발했다.
    // 다이제스트도 의미가 없으므로 엔트리를 버린다(fail-closed).
    if normalized.unterminated_quote {
        return None;
    }

    let sql_text = match literal_policy {
        // 저장하지 않는다. 다이제스트만 남는다.
        LiteralPolicy::Off => None,
        // **정규 텍스트만 저장한다.** 슬로우 로그에는 리터럴이 그대로 있으므로
        // 여기서 원문을 쓰면 정책이 `masked` 인데 리터럴이 저장된다.
        LiteralPolicy::Masked => {
            // 정규화가 리터럴을 남겼다면 저장하지 않는다 — 정책 위반보다 결측이 낫다.
            if normalized.check_no_literals().is_err() {
                return None;
            }
            Some(normalized.canonical.clone())
        }
        // 정책이 원문 저장을 허용한다. `full_restricted` 의 열람 제한은 조회 계층의 일이다.
        LiteralPolicy::Full | LiteralPolicy::FullRestricted => Some(entry.sql_text.clone()),
    };

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
        // 슬로우 로그는 절단하지 않는다. 그게 이 경로의 장점이다.
        sql_text_truncated: false,
        sql_text_lossy: false,
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
            // 정규화가 거부했다(파라미터 자리표, 힌트 등). 사유를 세어 둔다.
            stats.unnormalizable += 1;
            continue;
        };
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
            "정규화할 수 없는 슬로우로그 엔트리를 건너뛰었다"
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

    /// **`record_id` 가 실시간 경로와 같아야 병합된다.**
    ///
    /// 다르면 같은 실행이 두 레코드로 저장되고, 화면에 중복으로 보인다.
    #[test]
    fn record_id_matches_the_realtime_path_for_the_same_execution() {
        use dbmon_core::ids::RecordId;

        let e = entry("SELECT 1");
        let q = to_slow_query(&e, &instance(), LiteralPolicy::Masked, 0).expect("변환");

        // 실시간 경로는 같은 (인스턴스, 스레드, 시작 초) 로 키를 만든다.
        let realtime = RecordId::new(&instance().id, e.thread_id, e.started_at_ms);
        assert_eq!(q.record_id, realtime);

        // ±1초 흔들림도 같은 키로 접힌다 — `record_id` 가 초 단위인 이유다.
        let jittered = RecordId::new(&instance().id, e.thread_id, e.started_at_ms + 300);
        assert_eq!(q.record_id, jittered);
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
