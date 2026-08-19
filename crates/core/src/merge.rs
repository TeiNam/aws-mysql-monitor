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
//! | `duration_ms` | `slowlog` 가 있으면 그것, 없으면 **관측된 최대값** |
//! | `stats` 카운터 | 필드별 **최대값** (같은 실행의 하한이므로 큰 쪽이 참에 가깝다) |
//! | `sql_text` | 더 긴 쪽. **단 고정된 정책이 허용하는 범위 안에서만** |
//! | `plan_*` | `for_connection > rerun > none` |
//! | `started_at_ms` | 더 이른 쪽 |

use crate::slow_query::UNKNOWN_DIGEST_PREFIX;
use crate::slow_query::duration_rank;
use crate::slow_query::{
    CaptureSource, DurationSource, ExecStats, LiteralPolicy, PlanBundle, SlowQuery, SlowQueryState,
};
use crate::time::EpochMs;

/// 두 레코드를 병합한다. `existing` 이 이미 저장된 쪽이다.
///
/// **새 값을 반환하며 입력을 변경하지 않는다.**
pub fn merge(existing: &SlowQuery, incoming: &SlowQuery) -> SlowQuery {
    // **"더 나은 레코드 하나를 골라 그 필드를 쓴다"는 방식을 버렸다.**
    // 그 방식은 (a) 미완결 `Timer` 가 완결 `Polled` 를 이기게 만들고(62초 → 2.1초),
    // (b) 동률일 때 `existing` 이 이겨 `merge(a,b) != merge(b,a)` 가 됐다.
    // 이제 속성마다 그 속성에 맞는 규칙으로 병합한다.
    let duration = merge_duration(existing, incoming);
    let digest = merge_digest(existing, incoming);

    // 정책은 **먼저 기록된 쪽**을 고정한다. 두 레코드의 `literal_policy_at_ms` 중 이른 쪽.
    let (policy, policy_at) = match existing
        .literal_policy_at_ms
        .cmp(&incoming.literal_policy_at_ms)
    {
        std::cmp::Ordering::Less => (existing.literal_policy, existing.literal_policy_at_ms),
        std::cmp::Ordering::Greater => (incoming.literal_policy, incoming.literal_policy_at_ms),
        // **동률에서는 더 제한적인 쪽이 이긴다.** `existing` 우선으로 두면 보안 통제(F2)의
        // 결과가 쓰기 순서에 달린다 — `merge(masked, full)` 은 마스킹본을,
        // `merge(full, masked)` 는 원문을 저장했다. 방향이 fail-open 이었다.
        std::cmp::Ordering::Equal => (
            existing
                .literal_policy
                .more_restrictive(incoming.literal_policy),
            existing.literal_policy_at_ms,
        ),
    };
    let chosen_text = merge_sql_text(existing, incoming, policy);

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
        // `started_at_ms` 와 같은 규칙(더 이른 쪽)이어야 한다. `or` 로 두면 두 필드가
        // 어긋나 `coarse < precise` 같은 모순 조합이 나온다.
        started_at_ms_precise: min_opt(
            existing.started_at_ms_precise,
            incoming.started_at_ms_precise,
        ),
        // **`duration_ms` 와 짝을 맞춘다.** 따로 고르면 레코드가 자기모순이 된다:
        // duration=62,000ms 인데 `ended_at - started_at = 4,000ms` 인 레코드가 나온다.
        // `dur_bucket`(GSI2PK)은 `duration_ms` 로 계산되고 UI 구간은 두 시각의 차로
        // 계산되므로 둘이 어긋나면 화면과 인덱스가 다른 이야기를 한다.
        ended_at_ms: duration.2,
        captured_at_ms: existing.captured_at_ms.min(incoming.captured_at_ms),
        duration_ms: duration.0,
        duration_source: duration.1,

        sql_text: chosen_text.map(|c| c.text.clone()),
        // **채택한 텍스트**의 속성이다. 버린 쪽이 온전했어도 의미가 없다.
        sql_text_truncated: chosen_text.is_some_and(|c| c.truncated),
        sql_text_lossy: chosen_text.is_some_and(|c| c.lossy),
        literal_policy: policy,
        literal_policy_at_ms: policy_at,

        // **다이제스트와 알고리즘 버전은 짝이다.** 따로 고르면 버전 필드가 그 다이제스트를
        // 설명하지 않게 되어 "리포트가 알고리즘 경계를 표시할 수 있어야 한다" 는 목적이
        // 깨진다 (예: app_digest=aaaa 인데 algo_version=2).
        app_digest: digest.0.clone(),
        digest_algo_version: digest.1,
        mysql_digest: existing
            .mysql_digest
            .clone()
            .or_else(|| incoming.mysql_digest.clone()),
        statement_type: existing.statement_type,
        is_nested: existing.is_nested || incoming.is_nested,

        stats: merge_stats(&existing.stats, &incoming.stats),
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
/// 저장할 SQL 텍스트와 **그 텍스트의 절단 여부**를 함께 고른다.
///
/// 플래그를 따로 계산하면(`a.truncated && b.truncated`) 절단된 텍스트를 채택하면서
/// 플래그만 `false` 가 될 수 있다 — UI 가 절단 배지를 못 붙이고 사용자는 잘린 SQL 을
/// 전문으로 오독한다. 그래서 선택과 플래그를 한 곳에서 결정한다.
fn merge_sql_text<'a>(
    a: &'a SlowQuery,
    b: &'a SlowQuery,
    policy: LiteralPolicy,
) -> Option<ChosenText<'a>> {
    if policy.stores_nothing() {
        return None;
    }
    /// 고정 정책이 이 레코드의 텍스트를 받아들일 수 있는가.
    fn usable(q: &SlowQuery, policy: LiteralPolicy) -> Option<ChosenText<'_>> {
        let t = q.sql_text.as_ref()?;
        if policy.stores_literals() || !q.literal_policy.stores_literals() {
            Some(ChosenText {
                text: t,
                truncated: q.sql_text_truncated,
                lossy: q.sql_text_lossy,
            })
        } else {
            // 고정 정책은 마스킹인데 이 텍스트는 원문이다 → 쓸 수 없다.
            None
        }
    }
    match (usable(a, policy), usable(b, policy)) {
        (Some(x), Some(y)) => Some(if y.text.len() > x.text.len() { y } else { x }),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// 병합이 채택한 텍스트와 **그 텍스트의 품질**.
#[derive(Debug, Clone, Copy)]
struct ChosenText<'a> {
    text: &'a String,
    truncated: bool,
    lossy: bool,
}

/// 지속시간과 그 출처를 함께 고른다.
///
/// # `Timer > Polled` 순위만으로는 62초 쿼리가 2.1초로 저장된다
///
/// 선행 저장은 첫 tick 의 `TIMER_WAIT` 를 쓴다 — 그건 **실행 도중** 값이므로 총
/// 실행시간의 하한이다. 확정 경로는 스레드가 이미 사라져 `stmt` 를 다시 조회할 수
/// 없어 `Polled`(관측된 최대 `TIME`)를 붙인다. 순위만 보면 미완결 `Timer` 가 이긴다:
///
/// ```text
/// merge(선행 = Timer 2,100ms, 확정 = Polled 62,000ms)
///   duration_ms = 2,100          ← 62초 쿼리가 2.1초로 기록된다
///   ended_at - started_at = 62,000ms   ← 레코드 내부 자기모순
///   dur_bucket = b0 (b3 이어야 한다)   ← GSI2PK 라 "느린 것만" 조회에서 사라진다
/// ```
///
/// 느린 쿼리 모니터가 **가장 느린 쿼리를 가장 크게 축소 보고**한다.
///
/// # 규칙
///
/// | 조합 | 채택 | 이유 |
/// |---|---|---|
/// | 한쪽이 `slowlog` | 그쪽 | 완결된 실행의 권위 있는 측정값이다 |
/// | 그 외 | **최대값** | 둘 다 같은 실행의 하한이다 — 큰 쪽이 참에 가깝다 |
///
/// 정확도 순위(`duration_rank`)는 **완결성**을 담지 못한다. 그래서 `slowlog` 여부만
/// 순위로 쓰고, in-flight 관측끼리는 크기로 비교한다.
fn merge_duration(a: &SlowQuery, b: &SlowQuery) -> (i64, DurationSource, Option<EpochMs>) {
    let pick = |q: &SlowQuery, other: &SlowQuery| {
        // 채택한 쪽에 종료 시각이 없으면 다른 쪽 것을 쓴다(없는 것보다 낫다).
        (
            q.duration_ms,
            q.duration_source,
            q.ended_at_ms.or(other.ended_at_ms),
        )
    };
    let authoritative = |q: &SlowQuery| q.duration_source == DurationSource::Slowlog;
    match (authoritative(a), authoritative(b)) {
        (true, false) => return pick(a, b),
        (false, true) => return pick(b, a),
        _ => {}
    }
    // 둘 다 슬로우로그이거나 둘 다 in-flight → 큰 쪽.
    match a.duration_ms.cmp(&b.duration_ms) {
        std::cmp::Ordering::Less => pick(b, a),
        std::cmp::Ordering::Greater => pick(a, b),
        // **동률에서도 결정론적이어야 한다.** `a` 우선으로 두면 `merge(a,b) != merge(b,a)` 이고,
        // 그건 이 함수가 고치려던 결함과 같은 부류다. 정확도 순위로 가르고, 그것도 같으면
        // 이른 종료 시각을 쓴다(탐지는 지연되므로 이른 쪽이 참에 가깝다).
        std::cmp::Ordering::Equal => {
            match duration_rank(a.duration_source).cmp(&duration_rank(b.duration_source)) {
                std::cmp::Ordering::Less => pick(b, a),
                std::cmp::Ordering::Greater => pick(a, b),
                std::cmp::Ordering::Equal => (
                    a.duration_ms,
                    a.duration_source,
                    min_opt(a.ended_at_ms, b.ended_at_ms),
                ),
            }
        }
    }
}

/// `app_digest` 와 그것을 만든 알고리즘 버전을 **함께** 고른다.
///
/// 더 새 버전의 다이제스트를 쓴다. 같은 버전이면 사전순으로 결정론적으로 고른다
/// (`pick_str` 과 같은 규칙 — 교환법칙을 만족해야 한다).
fn merge_digest(a: &SlowQuery, b: &SlowQuery) -> (String, u32) {
    // **자리표는 값이 아니다.** 선행 저장이 `deep_probe_limit` 으로 스킵되면
    // `app_digest = "unknown-<thread_id>"` 로 저장된다. 그 뒤 도착한 슬로우로그의
    // 진짜 다이제스트가 버려지면 그 실행은 영구히 어느 그룹에도 속하지 않는다.
    let real = |q: &SlowQuery| !q.app_digest.starts_with(UNKNOWN_DIGEST_PREFIX);
    match (real(a), real(b)) {
        (true, false) => return (a.app_digest.clone(), a.digest_algo_version),
        (false, true) => return (b.app_digest.clone(), b.digest_algo_version),
        _ => {}
    }
    match a.digest_algo_version.cmp(&b.digest_algo_version) {
        std::cmp::Ordering::Greater => (a.app_digest.clone(), a.digest_algo_version),
        std::cmp::Ordering::Less => (b.app_digest.clone(), b.digest_algo_version),
        // **사전순으로 고른다.** `pick_str` 은 사전순이 아니라 "`a` 우선" 이므로
        // 교환법칙을 만족하지 않는다. `app_digest` 는 GSI1PK 이므로 도착 순서가
        // 집계 파티션을 바꾼다 — 대칭성을 요구하는 이유가 바로 그것이다 (R43).
        std::cmp::Ordering::Equal => (
            a.app_digest.clone().min(b.app_digest.clone()),
            a.digest_algo_version,
        ),
    }
}

/// 두 `Option` 중 작은 값. 한쪽만 있으면 그것.
fn min_opt<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// 두 `Option` 중 큰 값. 한쪽만 있으면 그것.
fn max_opt<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

/// 실행 통계를 **필드별 최대값**으로 병합한다.
///
/// 이전 구현은 `prefer_b` 하나로 레코드를 통째로 골랐다. 동률이면 `existing` 이 이겨서
/// `merge(a,b) != merge(b,a)` 였다 — 같은 두 레코드가 도착 순서에 따라 다른 결과를 냈다.
///
/// 카운터는 같은 실행에 대해 **단조 증가**한다(실행 도중 관측은 하한이다). 그래서
/// 필드별 최대값이 정답이고, 동시에 교환법칙을 만족한다.
fn merge_stats(a: &ExecStats, b: &ExecStats) -> ExecStats {
    // 불리언은 **한 번이라도 참이면 참**이다 (관측 시점에 따라 달라진다).
    fn any(x: Option<bool>, y: Option<bool>) -> Option<bool> {
        match (x, y) {
            (Some(p), Some(q)) => Some(p || q),
            (p, q) => p.or(q),
        }
    }
    ExecStats {
        rows_examined: max_opt(a.rows_examined, b.rows_examined),
        rows_sent: max_opt(a.rows_sent, b.rows_sent),
        rows_affected: max_opt(a.rows_affected, b.rows_affected),
        lock_time_ms: max_opt(a.lock_time_ms, b.lock_time_ms),
        tmp_tables: max_opt(a.tmp_tables, b.tmp_tables),
        tmp_disk_tables: max_opt(a.tmp_disk_tables, b.tmp_disk_tables),
        sort_merge_passes: max_opt(a.sort_merge_passes, b.sort_merge_passes),
        no_index_used: any(a.no_index_used, b.no_index_used),
        no_good_index_used: any(a.no_good_index_used, b.no_good_index_used),
        full_join: any(a.full_join, b.full_join),
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
    /// 선행 저장의 **실행 도중** `TIMER_WAIT` 가 확정된 총 실행시간을 이기면 안 된다.
    ///
    /// 이 테스트가 없으면 62초 쿼리가 2.1초로 저장되고, `dur_bucket` 이 GSI2PK 라
    /// "느린 것만" 조회에서 아예 사라진다 — 느린 쿼리 모니터의 핵심 실패다.
    #[test]
    fn midflight_timer_does_not_beat_completed_polled_duration() {
        // 선행 저장: 첫 tick 의 TIMER_WAIT = 2.1초 (실행 도중이라 하한이다).
        let pre = SlowQuery {
            state: SlowQueryState::InFlight,
            duration_ms: 2_100,
            duration_source: DurationSource::Timer,
            ended_at_ms: None,
            ..base()
        };
        // 확정: 스레드가 사라져 stmt 재조회 불가 → Polled, 관측된 최대 TIME = 62초.
        let fin = SlowQuery {
            state: SlowQueryState::Finalized,
            duration_ms: 62_000,
            duration_source: DurationSource::Polled,
            ended_at_ms: Some(base().started_at_ms + 62_000),
            ..base()
        };

        for (a, b, label) in [(&pre, &fin, "선행→확정"), (&fin, &pre, "확정→선행")] {
            let m = merge(a, b);
            assert_eq!(m.duration_ms, 62_000, "{label}: 관측된 최대값이어야 한다");
            assert_eq!(m.duration_source, DurationSource::Polled, "{label}");
            // 레코드 내부 자기모순이 없어야 한다.
            let span = m.ended_at_ms.expect("종료 관측됨") - m.started_at_ms;
            assert_eq!(
                m.duration_ms, span,
                "{label}: duration 과 구간이 일치해야 한다"
            );
        }
    }

    /// `slowlog` 는 완결된 실행의 권위 있는 측정값이므로 크기와 무관하게 이긴다.
    /// 폴링 `TIME` 은 초 단위라 실제보다 최대 1초 크게 나올 수 있다.
    #[test]
    fn slowlog_wins_even_when_smaller_than_polled() {
        let polled = SlowQuery {
            duration_ms: 6_000,
            duration_source: DurationSource::Polled,
            ..base()
        };
        let slowlog = SlowQuery {
            duration_ms: 5_200,
            duration_source: DurationSource::Slowlog,
            ..base()
        };
        for (a, b) in [(&polled, &slowlog), (&slowlog, &polled)] {
            let m = merge(a, b);
            assert_eq!(m.duration_ms, 5_200);
            assert_eq!(m.duration_source, DurationSource::Slowlog);
        }
    }

    /// **R43 전수 검사.** 손으로 고른 한 쌍으로는 대칭성을 확인할 수 없다 —
    /// 1차·2차 리뷰가 모두 "고쳤다" 고 한 뒤에도 비대칭이 남아 있었다.
    ///
    /// 병합 결과에 영향을 주는 축을 조합해 **모든 쌍**에 대해 `merge(a,b) == merge(b,a)` 를
    /// 확인한다. `record_id`·`instance_id`·`literal_policy` 는 "먼저 저장된 쪽 유지" 가
    /// 문서화된 의도이므로 비교에서 제외한다.
    #[test]
    fn merge_is_commutative_across_all_axes() {
        use DurationSource::*;
        use SlowQueryState::*;

        let durations = [1_000i64, 4_000, 62_000];
        let sources = [Polled, Timer, Slowlog];
        let states = [InFlight, Finalized, Abandoned];
        let ends = [None, Some(1_755_500_404_000i64), Some(1_755_500_462_000)];
        let digests = ["9f2c1a", "aaaa1111", "unknown-8842119"];

        let mut variants = Vec::new();
        for d in durations {
            for src in sources {
                for st in states {
                    for e in ends {
                        for dg in digests {
                            variants.push(SlowQuery {
                                duration_ms: d,
                                duration_source: src,
                                state: st,
                                ended_at_ms: e,
                                app_digest: dg.into(),
                                ..base()
                            });
                        }
                    }
                }
            }
        }

        let mut checked = 0usize;
        for (i, a) in variants.iter().enumerate() {
            for b in &variants[i..] {
                let ab = merge(a, b);
                let ba = merge(b, a);
                checked += 1;

                // 문서화된 순서 의존 필드는 제외하고 비교한다.
                let strip = |q: &SlowQuery| {
                    (
                        q.duration_ms,
                        q.duration_source,
                        q.ended_at_ms,
                        q.state,
                        q.app_digest.clone(),
                        q.digest_algo_version,
                        q.stats,
                        q.sql_text_truncated,
                        q.sql_text_lossy,
                        q.started_at_ms,
                        q.started_at_ms_precise,
                    )
                };
                assert_eq!(
                    strip(&ab),
                    strip(&ba),
                    "비대칭:\n  a = {:?}ms/{:?}/{:?}/{:?}\n  b = {:?}ms/{:?}/{:?}/{:?}",
                    a.duration_ms,
                    a.duration_source,
                    a.ended_at_ms,
                    a.app_digest,
                    b.duration_ms,
                    b.duration_source,
                    b.ended_at_ms,
                    b.app_digest,
                );

                // `duration_ms` 와 관측된 구간이 모순되지 않아야 한다.
                if let Some(end) = ab.ended_at_ms {
                    let span = end - ab.started_at_ms;
                    assert!(span >= 0, "종료가 시작보다 이르다: span={span}ms");
                }
            }
        }
        assert_eq!(
            checked, 29_646,
            "조합 수가 바뀌었다 — 축을 늘렸으면 이 값도 갱신한다"
        );
    }

    /// R43 — 병합은 교환법칙을 만족해야 한다. 도착 순서가 결과를 바꾸면 두 워커가
    /// 같은 두 레코드를 보고 다른 값을 저장한다.
    #[test]
    fn merge_is_commutative_for_stats_and_times() {
        let a = SlowQuery {
            stats: ExecStats {
                rows_examined: Some(100),
                lock_time_ms: Some(5),
                no_index_used: Some(false),
                ..Default::default()
            },
            started_at_ms_precise: Some(base().started_at_ms + 900),
            ended_at_ms: Some(base().started_at_ms + 4_100),
            ..base()
        };
        let b = SlowQuery {
            stats: ExecStats {
                rows_examined: Some(999_999),
                lock_time_ms: Some(12),
                no_index_used: Some(true),
                ..Default::default()
            },
            started_at_ms_precise: Some(base().started_at_ms + 500),
            ended_at_ms: Some(base().started_at_ms + 4_000),
            ..base()
        };
        let ab = merge(&a, &b);
        let ba = merge(&b, &a);
        assert_eq!(ab.stats, ba.stats, "통계가 순서에 따라 달라진다");
        assert_eq!(ab.ended_at_ms, ba.ended_at_ms);
        assert_eq!(ab.started_at_ms_precise, ba.started_at_ms_precise);
        // 카운터는 같은 실행의 하한이므로 큰 쪽이 참에 가깝다.
        assert_eq!(ab.stats.rows_examined, Some(999_999));
        assert_eq!(ab.stats.lock_time_ms, Some(12));
        // 한 번이라도 인덱스를 못 썼다고 관측되면 참이다.
        assert_eq!(ab.stats.no_index_used, Some(true));
        // 시각은 `started_at_ms` 와 같은 규칙(더 이른 쪽)이어야 한다.
        assert_eq!(ab.started_at_ms_precise, Some(base().started_at_ms + 500));
    }

    /// 절단 플래그는 **채택된 텍스트**의 속성이어야 한다. 어긋나면 UI 가 잘린 SQL 에
    /// 배지를 못 붙이고 사용자가 전문으로 오독한다.
    #[test]
    fn truncation_flag_describes_the_chosen_text() {
        let short_complete = SlowQuery {
            sql_text: Some("SELECT a FROM t".into()),
            sql_text_truncated: false,
            sql_text_lossy: false,
            ..base()
        };
        let long_truncated = SlowQuery {
            sql_text: Some("SELECT a FROM t WHERE id IN (1,2,3) AND x =".into()),
            sql_text_truncated: true,
            ..base()
        };
        let m = merge(&short_complete, &long_truncated);
        assert!(
            m.sql_text.as_deref().unwrap().len() > 15,
            "더 긴 쪽을 채택한다"
        );
        assert!(
            m.sql_text_truncated,
            "채택한 텍스트가 절단본이면 true 여야 한다"
        );

        // 반대로 온전한 쪽을 채택했으면 false 다.
        let long_complete = SlowQuery {
            sql_text: Some("SELECT a FROM t WHERE id IN (1,2,3) AND x = 1".into()),
            sql_text_truncated: false,
            ..base()
        };
        let m = merge(&long_complete, &long_truncated);
        assert!(!m.sql_text_truncated);
    }

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
            sql_text_lossy: false,
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
