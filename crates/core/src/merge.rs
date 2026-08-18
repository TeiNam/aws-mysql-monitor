//! 3소스 병합 ([05 §8.2](../../../docs/05-collector.md), F5, R43).
//!
//! # 왜 대칭이어야 하는가
//!
//! 초기 설계는 병합 로직을 **슬로우로그 수집 경로에만** 뒀고, 실시간 캡처 확정 경로는
//! 그냥 `PutItem` 이었다. 백필로 슬로우로그가 **먼저** 적재된 뒤 같은 실행의 캡처가
//! 확정되면 `PutItem` 이 슬로우로그의 정확 지표를 **덮어쓴다**
//! (`rows_examined`, `duration_source=slowlog`).
//!
//! → 양쪽 경로가 **같은 함수**를 쓴다. 경로마다 따로 구현하면 다시 비대칭이 된다.
//!
//! # 불변식
//!
//! | 항목 | 규칙 |
//! |---|---|
//! | `record_id` | **먼저 저장된 쪽을 유지** — 키가 바뀌면 멱등성이 깨진다 |
//! | `literal_policy` | 먼저 기록된 쪽 유지 (F2). 소급 마스킹은 불가하므로 정책을 고정한다 |
//! | `duration_ms` 계열 | 정확도 높은 소스(slowlog > timer > polled) |
//! | `sql_text` | 더 긴 쪽. **단 고정된 정책이 허용하는 범위 안에서만** |
//! | `plan_*` | `for_connection > rerun > none` |
//! | `started_at_ms` | 더 이른 쪽 |

use crate::slow_query::{
    CaptureSource, ExecStats, LiteralPolicy, PlanBundle, SlowQuery, SlowQueryState, duration_rank,
};

/// 두 레코드를 병합한다. `existing` 이 이미 저장된 쪽이다.
///
/// **새 값을 반환하며 입력을 변경하지 않는다.**
pub fn merge(existing: &SlowQuery, incoming: &SlowQuery) -> SlowQuery {
    let inc_wins_duration =
        duration_rank(incoming.duration_source) > duration_rank(existing.duration_source);
    let (better, worse) = if inc_wins_duration {
        (incoming, existing)
    } else {
        (existing, incoming)
    };

    // 정책은 **먼저 기록된 쪽**을 고정한다. 두 레코드의 `literal_policy_at_ms` 중 이른 쪽.
    let (policy, policy_at) = if existing.literal_policy_at_ms <= incoming.literal_policy_at_ms {
        (existing.literal_policy, existing.literal_policy_at_ms)
    } else {
        (incoming.literal_policy, incoming.literal_policy_at_ms)
    };

    SlowQuery {
        // 키와 신원 — 먼저 저장된 쪽을 유지한다.
        record_id: existing.record_id.clone(),
        instance_id: existing.instance_id.clone(),
        cluster_id: existing
            .cluster_id
            .clone()
            .or_else(|| incoming.cluster_id.clone()),
        env: existing.env,
        engine: existing.engine,
        engine_version: pick_str(&existing.engine_version, &incoming.engine_version),
        state: merge_state(existing.state, incoming.state),

        thread_id: existing.thread_id,
        schema_name: existing
            .schema_name
            .clone()
            .or_else(|| incoming.schema_name.clone()),
        db_user: existing
            .db_user
            .clone()
            .or_else(|| incoming.db_user.clone()),
        db_host: existing
            .db_host
            .clone()
            .or_else(|| incoming.db_host.clone()),

        // 시각 — 더 이른 시작, 더 정확한 종료.
        started_at_ms: existing.started_at_ms.min(incoming.started_at_ms),
        started_at_ms_precise: existing
            .started_at_ms_precise
            .or(incoming.started_at_ms_precise),
        ended_at_ms: better.ended_at_ms.or(worse.ended_at_ms),
        captured_at_ms: existing.captured_at_ms.min(incoming.captured_at_ms),
        duration_ms: if inc_wins_duration {
            incoming.duration_ms
        } else if duration_rank(existing.duration_source) == duration_rank(incoming.duration_source)
        {
            // 같은 정확도면 관측된 최대값을 쓴다 (보수적).
            existing.duration_ms.max(incoming.duration_ms)
        } else {
            existing.duration_ms
        },
        duration_source: better.duration_source,

        sql_text: merge_sql_text(existing, incoming, policy),
        // 둘 중 하나라도 온전한 전문을 가졌으면 절단이 아니다.
        sql_text_truncated: existing.sql_text_truncated && incoming.sql_text_truncated,
        literal_policy: policy,
        literal_policy_at_ms: policy_at,

        app_digest: pick_str(&existing.app_digest, &incoming.app_digest),
        digest_algo_version: existing
            .digest_algo_version
            .max(incoming.digest_algo_version),
        mysql_digest: existing
            .mysql_digest
            .clone()
            .or_else(|| incoming.mysql_digest.clone()),
        statement_type: existing.statement_type,
        is_nested: existing.is_nested || incoming.is_nested,

        stats: merge_stats(&existing.stats, &incoming.stats, inc_wins_duration),
        plan: merge_plan(&existing.plan, &incoming.plan),
        capture_source: merge_capture_source(existing.capture_source, incoming.capture_source),

        owner_worker: existing
            .owner_worker
            .clone()
            .or_else(|| incoming.owner_worker.clone()),
        owner_epoch: existing.owner_epoch.max(incoming.owner_epoch),
        last_seen_at_ms: existing.last_seen_at_ms.max(incoming.last_seen_at_ms),
        abandoned_reason: existing
            .abandoned_reason
            .clone()
            .or_else(|| incoming.abandoned_reason.clone()),
        long_running: existing.long_running || incoming.long_running,
    }
}

/// 종료를 실제로 관측한 쪽이 이긴다. `Abandoned` 는 "관측이 끊겼다"이므로
/// 나중에 `Finalized` 근거가 도착하면 정정한다.
fn merge_state(a: SlowQueryState, b: SlowQueryState) -> SlowQueryState {
    use SlowQueryState::*;
    match (a, b) {
        (Finalized, _) | (_, Finalized) => Finalized,
        (Abandoned, _) | (_, Abandoned) => Abandoned,
        _ => InFlight,
    }
}

/// **고정된 정책이 허용하는 범위 안에서** 더 긴 텍스트를 고른다.
///
/// 정책이 전환되는 중이라면 두 레코드의 정책이 다를 수 있다. 고정 정책이 `masked` 인데
/// 상대가 `full` 로 만든 원문을 "더 길다"는 이유로 채택하면 **리터럴이 유출된다**.
/// 이 함수가 없으면 F2 의 방어가 병합 경로에서 무력화된다.
fn merge_sql_text(a: &SlowQuery, b: &SlowQuery, policy: LiteralPolicy) -> Option<String> {
    if policy.stores_nothing() {
        return None;
    }
    /// 고정 정책이 이 레코드의 텍스트를 받아들일 수 있는가.
    fn usable(q: &SlowQuery, policy: LiteralPolicy) -> Option<&String> {
        let t = q.sql_text.as_ref()?;
        if policy.stores_literals() || !q.literal_policy.stores_literals() {
            Some(t)
        } else {
            // 고정 정책은 마스킹인데 이 텍스트는 원문이다 → 쓸 수 없다.
            None
        }
    }
    match (usable(a, policy), usable(b, policy)) {
        (Some(x), Some(y)) => Some(if y.len() > x.len() {
            y.clone()
        } else {
            x.clone()
        }),
        (Some(x), None) => Some(x.clone()),
        (None, Some(y)) => Some(y.clone()),
        (None, None) => None,
    }
}

fn merge_stats(a: &ExecStats, b: &ExecStats, prefer_b: bool) -> ExecStats {
    let (p, s) = if prefer_b { (b, a) } else { (a, b) };
    ExecStats {
        rows_examined: p.rows_examined.or(s.rows_examined),
        rows_sent: p.rows_sent.or(s.rows_sent),
        rows_affected: p.rows_affected.or(s.rows_affected),
        lock_time_ms: p.lock_time_ms.or(s.lock_time_ms),
        tmp_tables: p.tmp_tables.or(s.tmp_tables),
        tmp_disk_tables: p.tmp_disk_tables.or(s.tmp_disk_tables),
        sort_merge_passes: p.sort_merge_passes.or(s.sort_merge_passes),
        no_index_used: p.no_index_used.or(s.no_index_used),
        no_good_index_used: p.no_good_index_used.or(s.no_good_index_used),
        full_join: p.full_join.or(s.full_join),
    }
}

fn merge_plan(a: &PlanBundle, b: &PlanBundle) -> PlanBundle {
    let (p, s) = if b.source > a.source || (b.source == a.source && !a.has_plan() && b.has_plan()) {
        (b, a)
    } else {
        (a, b)
    };
    let mut referenced_tables = p.referenced_tables.clone();
    for t in &s.referenced_tables {
        if !referenced_tables.contains(t) {
            referenced_tables.push(t.clone());
        }
    }
    referenced_tables.sort();
    PlanBundle {
        normalized_json: p
            .normalized_json
            .clone()
            .or_else(|| s.normalized_json.clone()),
        s3_key: p.s3_key.clone().or_else(|| s.s3_key.clone()),
        format_version: p
            .format_version
            .clone()
            .or_else(|| s.format_version.clone()),
        tree_text: p.tree_text.clone().or_else(|| s.tree_text.clone()),
        source: p.source,
        // 플랜을 얻었으면 실패 사유를 남기지 않는다.
        error: if p.has_plan() {
            None
        } else {
            p.error.clone().or_else(|| s.error.clone())
        },
        fingerprint: p.fingerprint.clone().or_else(|| s.fingerprint.clone()),
        referenced_tables,
    }
}

fn merge_capture_source(a: CaptureSource, b: CaptureSource) -> CaptureSource {
    if a == b { a } else { CaptureSource::Merged }
}

fn pick_str(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else {
        a.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::ids::{InstanceId, RecordId};
    use crate::instance::Engine;
    use crate::slow_query::{DurationSource, PlanSource};
    use dbmon_normalize::StatementType;

    fn inst() -> InstanceId {
        InstanceId::new("123456789012", "ap-northeast-2", "orders-prd-01").unwrap()
    }

    fn base() -> SlowQuery {
        let i = inst();
        SlowQuery {
            record_id: RecordId::new(&i, 8842119, 1_755_500_400_000),
            instance_id: i,
            cluster_id: None,
            env: Env::Prd,
            engine: Engine::Mysql,
            engine_version: "8.4.5".into(),
            state: SlowQueryState::Finalized,
            thread_id: 8842119,
            schema_name: Some("shop".into()),
            db_user: Some("app".into()),
            db_host: Some("10.0.3.44".into()),
            started_at_ms: 1_755_500_400_000,
            started_at_ms_precise: None,
            ended_at_ms: Some(1_755_500_404_000),
            captured_at_ms: 1_755_500_404_100,
            duration_ms: 4000,
            duration_source: DurationSource::Polled,
            sql_text: Some("SELECT a FROM t WHERE id = ?".into()),
            sql_text_truncated: false,
            literal_policy: LiteralPolicy::Masked,
            literal_policy_at_ms: 1_755_500_400_000,
            app_digest: "9f2c1a".into(),
            digest_algo_version: 1,
            mysql_digest: None,
            statement_type: StatementType::Select,
            is_nested: false,
            stats: ExecStats::default(),
            plan: PlanBundle::default(),
            capture_source: CaptureSource::Processlist,
            owner_worker: None,
            owner_epoch: None,
            last_seen_at_ms: None,
            abandoned_reason: None,
            long_running: false,
        }
    }

    /// R43 — 슬로우로그 선착 후 캡처 도착. 레코드 1건 + 정확 지표 보존.
    #[test]
    fn r43_slowlog_first_then_capture_preserves_exact_metrics() {
        let mut slowlog = base();
        slowlog.duration_ms = 4213;
        slowlog.duration_source = DurationSource::Slowlog;
        slowlog.stats.rows_examined = Some(8_213_445);
        slowlog.stats.lock_time_ms = Some(0);
        slowlog.capture_source = CaptureSource::Slowlog;

        let mut capture = base();
        capture.duration_ms = 4000; // 초 단위 근사
        capture.duration_source = DurationSource::Polled;
        capture.plan.source = PlanSource::ForConnection;
        capture.plan.normalized_json = Some("{\"query_block\":{}}".into());
        capture.capture_source = CaptureSource::Processlist;

        // 어느 순서로 병합해도 결과가 같아야 한다 (대칭성).
        for (a, b) in [(&slowlog, &capture), (&capture, &slowlog)] {
            let m = merge(a, b);
            assert_eq!(m.duration_ms, 4213, "슬로우로그의 정확값이 살아야 한다");
            assert_eq!(m.duration_source, DurationSource::Slowlog);
            assert_eq!(m.stats.rows_examined, Some(8_213_445));
            assert_eq!(
                m.plan.source,
                PlanSource::ForConnection,
                "플랜은 캡처만 갖고 있다"
            );
            assert!(m.plan.has_plan());
            assert_eq!(m.capture_source, CaptureSource::Merged);
        }
    }

    #[test]
    fn record_id_and_policy_pin_to_first_stored() {
        let first = base();
        let mut second = base();
        second.record_id = RecordId::new(&inst(), 8842119, 1_755_500_401_000); // 1초 차이
        second.literal_policy = LiteralPolicy::Full;
        second.literal_policy_at_ms = 1_755_500_500_000; // 나중에 정책이 완화됨

        let m = merge(&first, &second);
        assert_eq!(m.record_id, first.record_id, "먼저 저장된 키를 유지한다");
        assert_eq!(
            m.literal_policy,
            LiteralPolicy::Masked,
            "정책은 소급 완화되지 않는다"
        );
        assert_eq!(m.literal_policy_at_ms, first.literal_policy_at_ms);
    }

    /// F2 방어가 병합 경로에서 무력화되지 않는지.
    #[test]
    fn masked_policy_rejects_longer_raw_text() {
        let masked = base(); // policy = Masked, 짧은 마스킹 텍스트
        let mut raw = base();
        raw.literal_policy = LiteralPolicy::Full;
        raw.literal_policy_at_ms = 1_755_500_500_000;
        raw.sql_text = Some("SELECT a FROM t WHERE id = 12345 AND ssn = '900101-1234567'".into());

        let m = merge(&masked, &raw);
        let text = m.sql_text.unwrap();
        assert!(
            !text.contains("900101"),
            "리터럴이 병합으로 유출됐다: {text}"
        );
        assert_eq!(text, "SELECT a FROM t WHERE id = ?");
    }

    #[test]
    fn longer_text_wins_within_same_policy() {
        let mut short = base();
        short.sql_text = Some("SELECT a FROM t WHERE ...".into());
        short.sql_text_truncated = true;
        let mut long = base();
        long.sql_text = Some("SELECT a FROM t WHERE id = ? AND x = ? AND y = ?".into());

        let m = merge(&short, &long);
        assert_eq!(
            m.sql_text.as_deref(),
            Some("SELECT a FROM t WHERE id = ? AND x = ? AND y = ?")
        );
        assert!(!m.sql_text_truncated, "한쪽이 온전하면 절단이 아니다");
    }

    #[test]
    fn off_policy_never_gains_text() {
        let mut off = base();
        off.literal_policy = LiteralPolicy::Off;
        off.sql_text = None;
        let with_text = base();
        assert_eq!(merge(&off, &with_text).sql_text, None);
    }

    #[test]
    fn plan_priority_and_error_clearing() {
        let mut failed = base();
        failed.plan.source = PlanSource::None;
        failed.plan.error = Some("thread_gone".into());
        failed.plan.referenced_tables = vec!["shop.a".into()];

        let mut ok = base();
        ok.plan.source = PlanSource::ForConnection;
        ok.plan.normalized_json = Some("{}".into());
        ok.plan.referenced_tables = vec!["shop.b".into()];

        let m = merge(&failed, &ok);
        assert_eq!(m.plan.source, PlanSource::ForConnection);
        assert_eq!(
            m.plan.error, None,
            "플랜을 얻었으면 실패 사유를 남기지 않는다"
        );
        assert_eq!(
            m.plan.referenced_tables,
            vec!["shop.a", "shop.b"],
            "참조 테이블은 합집합"
        );
    }

    #[test]
    fn in_flight_plus_finalized_is_finalized() {
        let mut flying = base();
        flying.state = SlowQueryState::InFlight;
        let done = base();
        assert_eq!(merge(&flying, &done).state, SlowQueryState::Finalized);

        let mut lost = base();
        lost.state = SlowQueryState::Abandoned;
        assert_eq!(
            merge(&lost, &done).state,
            SlowQueryState::Finalized,
            "종료를 실제로 관측한 근거가 도착하면 정정한다"
        );

        let mut flying2 = base();
        flying2.state = SlowQueryState::InFlight;
        assert_eq!(merge(&flying, &flying2).state, SlowQueryState::InFlight);
    }

    #[test]
    fn earliest_start_and_capture_time_win() {
        let mut early = base();
        early.started_at_ms = 1_755_500_399_000;
        early.captured_at_ms = 1_755_500_403_000;
        let late = base();
        let m = merge(&late, &early);
        assert_eq!(m.started_at_ms, 1_755_500_399_000);
        assert_eq!(m.captured_at_ms, 1_755_500_403_000);
    }

    #[test]
    fn same_source_takes_max_duration() {
        let mut a = base();
        a.duration_ms = 4000;
        let mut b = base();
        b.duration_ms = 6000;
        assert_eq!(merge(&a, &b).duration_ms, 6000);
        assert_eq!(merge(&b, &a).duration_ms, 6000);
    }

    #[test]
    fn merge_is_idempotent() {
        let a = base();
        let once = merge(&a, &a);
        let twice = merge(&once, &a);
        assert_eq!(once, twice);
        assert_eq!(
            once.capture_source,
            CaptureSource::Processlist,
            "같은 소스면 merged 로 바꾸지 않는다"
        );
    }
}
