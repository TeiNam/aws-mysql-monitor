//! 관측 → [`SlowQuery`] 레코드 조립.
//!
//! # 정규화·마스킹이 저장보다 **앞**이다 (F2)
//!
//! 초기 설계의 파이프라인은 `선행 저장 → (확정 시) 정규화·마스킹` 이었다.
//! 그러면 `masked` 정책 인스턴스에서도 **선행 저장 시점에 원문이 DynamoDB 에 들어간다.**
//! 확정되지 않은 고아 레코드는 영구히 마스킹되지 않은 상태로 남고, 그 사이 증분
//! 내보내기가 아카이브로 옮긴다. [08 §6.1](../../../../.claude/docs/08-security-auth.md)의
//! "소급 마스킹은 불가"와 결합되면 되돌릴 수 없다.
//!
//! → 이 모듈이 **모든 쓰기 경로**의 유일한 입구다. 선행 저장이든 확정이든 여기를 지난다.
//!
//! # 정책은 레코드에 고정된다 (A3-2)
//!
//! 진행 중 레코드가 있는 상태에서 관리자가 `full` → `masked` 로 바꾸면?
//! **선행 저장 시점의 정책을 `literal_policy_at_ms` 와 함께 고정**하고 확정 시에도 그걸 쓴다.
//! 확정 시점 정책을 쓰면 이미 저장된 원문을 덮어써야 하는데, 그건 소급 마스킹이고
//! 부분적으로만 동작한다(플랜은 이미 아카이브에 갔을 수 있다).

use dbmon_core::clock_offset::ClockOffset;
use dbmon_core::ids::RecordId;
use dbmon_core::inflight::{FinalizeReason, Tracked};
use dbmon_core::instance::Instance;
use dbmon_core::ports::target_db::StmtCurrentRow;
use dbmon_core::slow_query::{
    CaptureSource, ExecStats, LiteralPolicy, PlanBundle, PlanSource, SlowQuery, SlowQueryState,
    UNKNOWN_DIGEST_PREFIX,
};
use dbmon_core::time::EpochMs;
use dbmon_normalize::{DIGEST_ALGO_VERSION, StatementType, normalize};

/// 저장할 실행계획 JSON 의 상한.
///
/// DynamoDB 항목 상한이 400KB 다. 한 레코드에는 계획 말고도 SQL·다이제스트·통계가
/// 들어가므로 계획 하나에 그 절반 이상을 주지 않는다. 넘으면 **계획만 버리고 레코드는
/// 저장한다** — 계획 때문에 슬로우 쿼리 자체를 잃는 것이 훨씬 나쁘다.
const MAX_PLAN_BYTES: usize = 150 * 1024;

use crate::mysql::IS_PROCESSLIST_INFO_MAX_BYTES;

/// 조립 입력.
pub struct CaptureInput<'a> {
    pub instance: &'a Instance,
    pub tracked: &'a Tracked,
    /// `information_schema.PROCESSLIST.INFO` — 리터럴이 살아 있는 원문.
    pub full_sql: Option<&'a str>,
    pub stmt: Option<&'a StmtCurrentRow>,
    /// `EXPLAIN FORMAT=JSON` 원문. **마스킹은 이 모듈이 한다.**
    pub plan_json: Option<&'a str>,
    pub plan_source: PlanSource,
    pub plan_error: Option<String>,
    pub plan_tree: Option<String>,
    pub policy: LiteralPolicy,
    /// 정책을 고정한 시각. 선행 저장 시점이다.
    pub policy_at_ms: EpochMs,
    pub state: SlowQueryState,
    /// 확정 사유. `None` 이면 선행 저장이다.
    pub finalize_reason: Option<FinalizeReason>,
    pub now_ms: EpochMs,
    pub offset: &'a ClockOffset,
    pub owner_worker: &'a str,
    pub owner_epoch: Option<u64>,
}

/// 조립 결과. **관측 가능한 부작용을 함께 반환한다** — 조용히 강등하면 안 된다.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildOutcome {
    pub query: SlowQuery,
    /// 마스킹 후조건이 실패해 `off` 로 강등됐다 (T-36).
    /// 호출자는 `masking_postcondition_failed` 카운터를 올려야 한다.
    pub masking_degraded: bool,
    /// 플랜에서 `<redacted>` 로 대체된 필드 수.
    pub plan_redactions: usize,
    /// 플랜 파싱이 실패했다.
    pub plan_parse_failed: bool,
}

pub fn build(input: CaptureInput<'_>) -> BuildOutcome {
    let CaptureInput {
        instance,
        tracked,
        full_sql,
        stmt,
        plan_json,
        plan_source,
        plan_error,
        plan_tree,
        policy,
        policy_at_ms,
        state,
        finalize_reason,
        now_ms,
        offset,
        owner_worker,
        owner_epoch,
    } = input;

    // ── SQL 텍스트와 다이제스트 ────────────────────────────────────────────
    // 전문이 있으면 그걸 쓴다. 없으면 `DIGEST_TEXT`(이미 `?` 로 치환된 것)를 쓴다.
    // 둘 다 없으면 다이제스트를 계산할 수 없다.
    let digest_text = stmt.and_then(|s| s.digest_text.as_deref());
    let ps_sql = stmt.and_then(|s| s.sql_text.as_deref());
    let picked = pick_sql_text(full_sql, ps_sql);
    let full_sql = picked.map(|p| p.text);
    let source_text = full_sql.or(digest_text);
    let normalized = source_text.map(normalize);

    // **플래그는 채택한 소스의 속성이다.** 두 소스의 상한이 다르므로(IS 65,535 /
    // PS 기본 1,024) 한쪽 기준으로만 판정하면 잘린 SQL 이 "잘리지 않았다" 로 저장된다.
    let sql_text_truncated = picked.is_some_and(|p| p.maybe_truncated);
    let sql_text_lossy = picked.is_some_and(|p| p.lossy);

    // 마스킹 후조건은 **`masked` 정책에서만** 강제한다. `full` 은 원문을 저장하는 것이
    // 의도이므로 리터럴이 남아 있는 것이 정상이다.
    let mut masking_degraded = false;
    let effective_policy = if policy.requires_postcondition() {
        match normalized.as_ref().map(|n| n.check_no_literals()) {
            Some(Ok(())) => policy,
            // 마스킹을 신뢰할 수 없으면 **저장하지 않는다.** 원문을 남기는 것보다
            // 정보를 버리는 쪽이 맞다 — 이건 보안 통제다.
            Some(Err(_)) | None => {
                masking_degraded = true;
                policy.degrade()
            }
        }
    } else {
        policy
    };

    let sql_text = match effective_policy {
        LiteralPolicy::Off => None,
        LiteralPolicy::Masked => normalized.as_ref().map(|n| n.canonical.clone()),
        LiteralPolicy::Full | LiteralPolicy::FullRestricted => {
            // 전문이 없으면 `DIGEST_TEXT` 라도 저장한다 — 없는 것보다 낫다.
            source_text.map(str::to_string)
        }
    };

    // ── 실행계획 ───────────────────────────────────────────────────────────
    let mut plan_redactions = 0;
    let mut plan_parse_failed = false;
    let plan = match plan_json {
        Some(raw) => match dbmon_planparse::parse(raw, tracked.schema_name.as_deref()) {
            Ok(parsed) if parsed.normalized_json.len() > MAX_PLAN_BYTES => {
                // **계획만 버린다, 레코드는 남긴다.**
                //
                // DynamoDB 항목 상한은 400KB 다. 큰 조인의 `EXPLAIN FORMAT=JSON` 은
                // 그걸 넘길 수 있고, 그러면 `upsert_merged` 가 실패해 **슬로우 쿼리
                // 자체가 기록되지 않는다** — 가장 중요한 정보를 부수적인 것 때문에
                // 잃는다. 계획을 빼면 나머지(SQL·소요시간·검사 행수)는 남는다.
                //
                // 버렸다는 사실과 크기를 남긴다. 화면이 "계획 없음" 과 "계획이 너무
                // 커서 버렸다" 를 구분할 수 있어야 한다.
                //
                // S3 오프로드가 원래 계획이었으나(`storage.plan_bucket`) 읽는 코드가
                // 없어 설정만 존재했다 — 그 설정은 지웠다([17](../../../.claude/docs/17-roadmap-tasks.md)).
                plan_parse_failed = false;
                PlanBundle {
                    source: PlanSource::None,
                    error: Some(format!(
                        "too_large:{}KB>{}KB",
                        parsed.normalized_json.len() / 1024,
                        MAX_PLAN_BYTES / 1024
                    )),
                    tree_text: None,
                    // 참조 테이블은 남긴다 — 작고, 다이제스트 사전이 쓴다.
                    referenced_tables: parsed.referenced_tables,
                    ..Default::default()
                }
            }
            Ok(parsed) => {
                plan_redactions = parsed.redactions;
                PlanBundle {
                    // **마스킹된 JSON 만 저장한다.** 원문은 버린다 (T-16).
                    normalized_json: Some(parsed.normalized_json),
                    s3_key: None,
                    format_version: Some(parsed.format.as_str().to_string()),
                    tree_text: plan_tree,
                    source: plan_source,
                    error: None,
                    fingerprint: Some(parsed.fingerprint),
                    referenced_tables: parsed.referenced_tables,
                }
            }
            Err(e) => {
                plan_parse_failed = true;
                PlanBundle {
                    source: PlanSource::None,
                    // **원문을 에러 메시지에 넣지 않는다.** 파싱 실패 상황에서
                    // 마스킹을 신뢰할 수 없다 (T-36).
                    error: Some(format!("parse_failed:{}", short_reason(&e))),
                    tree_text: None,
                    ..Default::default()
                }
            }
        },
        None => PlanBundle {
            source: PlanSource::None,
            error: plan_error,
            tree_text: plan_tree,
            ..Default::default()
        },
    };

    // ── 시각과 지속시간 ────────────────────────────────────────────────────
    let started_at_ms = tracked.started_at_ms;
    // **추적기가 모아 둔 최선의 관측을 쓴다.**
    //
    // 전에는 이 tick 의 `TIMER_WAIT` 만 봤다. 그러면 (1) 확정 경로(`stmt: None`)가 정밀값을
    // 잃고, (2) 하트비트가 쓰는 값과 여기서 쓰는 값이 갈려 **저장된 소요가 줄어들 수
    // 있었다**(`TIME` 은 상태 전이에서 리셋된다 — 교차 리뷰 27라운드).
    //
    // `record_deep_probe` 가 이 tick 의 `TIMER_WAIT` 를 이미 반영했으므로(호출 순서가
    // 보장된다) 여기서는 모아진 값을 읽으면 된다.
    let (duration_ms, duration_source) = (tracked.duration_ms(), tracked.duration_source());

    // 종료를 관측하지 못했으면 `ended_at_ms` 를 남기지 않는다 — 추측한 시각을
    // 사실처럼 저장하면 리포트가 틀린다.
    //
    // # 종료 시각은 **마지막으로 본 시각**이다 (사라진 것을 알아챈 시각이 아니다)
    //
    // 우리가 아는 것은 "그때는 있었고 지금은 없다" 뿐이고, 실제 종료는 그 사이에 있다.
    // 알아챈 시각을 쓰면 **tick 간격만큼 부풀린다** — `detect_interval_ms` 를 60초로 두면
    // 2.1초 쿼리가 62초로 저장되고, `merge_duration` 이 구간(`Span`)을 가장 정확한 것으로
    // 보므로 타이머 증거(2.1초)를 덮는다(교차 리뷰 28라운드).
    //
    // 마지막 관측 시각은 **하한**이다 — 최대 tick 한 번만큼 짧게 잡는다. 없는 시간을
    // 만들어 내는 것보다 관측한 만큼만 적는 편이 이 프로젝트의 규칙에 맞다.
    let ended_at_ms = finalize_reason
        .filter(|r| r.observed_end())
        .map(|_| offset.to_db_time(tracked.last_seen_at_ms));

    let stats = stmt.map(exec_stats).unwrap_or_default();

    let app_digest = normalized
        .as_ref()
        .map(|n| n.app_digest.clone())
        // 텍스트가 전혀 없으면 다이제스트를 만들 수 없다. 빈 문자열로 두면 집계가
        // 오염되므로 스레드 기반의 명시적 자리표를 쓴다.
        .unwrap_or_else(|| format!("{UNKNOWN_DIGEST_PREFIX}{}", tracked.thread_id));

    let statement_type = normalized
        .as_ref()
        .map(|n| n.statement_type)
        .unwrap_or(StatementType::Other);

    BuildOutcome {
        query: SlowQuery {
            record_id: RecordId::new(&instance.id, tracked.thread_id, started_at_ms),
            instance_id: instance.id.clone(),
            cluster_id: instance.cluster_id.clone(),
            env: instance.env.effective,
            engine: instance.engine,
            engine_version: instance.engine_version.raw.clone(),
            state,
            thread_id: tracked.thread_id,
            schema_name: tracked
                .schema_name
                .clone()
                .or_else(|| stmt.and_then(|s| s.current_schema.clone())),
            db_user: tracked.identity.db_user.clone(),
            db_host: tracked.identity.db_host.clone(),
            started_at_ms,
            started_at_ms_precise: tracked.started_at_ms_precise,
            ended_at_ms,
            captured_at_ms: now_ms,
            duration_ms,
            duration_source,
            sql_text,
            sql_text_truncated,
            sql_text_lossy,
            literal_policy: effective_policy,
            literal_policy_at_ms: policy_at_ms,
            app_digest,
            digest_algo_version: DIGEST_ALGO_VERSION,
            mysql_digest: tracked.identity.digest.clone(),
            statement_type,
            is_nested: stmt.is_some_and(|s| s.is_nested_statement()),
            stats,
            plan,
            capture_source: CaptureSource::Processlist,
            owner_worker: Some(owner_worker.to_string()),
            owner_epoch,
            last_seen_at_ms: Some(tracked.last_seen_at_ms),
            abandoned_reason: finalize_reason
                .filter(|r| !r.observed_end())
                .map(|r| r.as_str().to_string()),
            long_running: finalize_reason == Some(FinalizeReason::TooLong),
        },
        masking_degraded,
        plan_redactions,
        plan_parse_failed,
    }
}

/// `events_statements_current` 한 행을 실행 통계로.
///
/// **실행 중인 문장의 이벤트 카운터는 버린다** ([`StmtCurrentRow::counters_are_final`]).
/// MySQL 은 그때 0 을 주는데, 그걸 저장하면 "모른다" 가 "0행을 훑었다" 는 사실이 된다 —
/// 화면은 `0` 을 보여주고 튜닝 모델은 실행계획과의 모순을 근거로 신뢰도를 내린다.
///
/// `None` 은 병합과도 맞는다: [`dbmon_core::merge`] 의 `max_opt`·`any` 가 나중에 도착한
/// 실제 값을 그대로 채택한다(슬로우로그 병합, 또는 확정 시점의 히스토리 재조회).
fn exec_stats(s: &StmtCurrentRow) -> ExecStats {
    // 끝났으면 그대로, 아직이면 모른다.
    let counter = |v: Option<u64>| if s.counters_are_final() { v } else { None };
    ExecStats {
        rows_examined: counter(s.rows_examined),
        rows_sent: counter(s.rows_sent),
        rows_affected: counter(s.rows_affected),
        // **`LOCK_TIME` 은 실행 중에도 유효하다**(실측: 0.004ms). 피코초다.
        lock_time_ms: s.lock_time_ps.map(|ps| (ps / 1_000_000_000) as i64),
        tmp_tables: counter(s.created_tmp_tables),
        tmp_disk_tables: counter(s.created_tmp_disk_tables),
        sort_merge_passes: counter(s.sort_merge_passes),
        // **옵티마이즈 시점에 정해진다** — 실행 중에도 유효하다(실측: 실행 중 `1`).
        no_index_used: s.no_index_used,
        no_good_index_used: s.no_good_index_used,
        full_join: counter(s.select_full_join).map(|v| v > 0),
    }
}

/// 두 전문 SQL 소스 중 **온전한 쪽**을 고른다.
///
/// 두 소스는 상보적이고, 어느 쪽도 단독으로는 충분하지 않다 (19 §A-2):
///
/// | 소스 | 상한 | 4바이트 문자 |
/// |---|---|---|
/// | `information_schema.PROCESSLIST.INFO` | 65,535바이트 | **`?` 로 손실** (`utf8mb3`) |
/// | `events_statements_current.SQL_TEXT` | 1,024바이트 (기본) | 보존 (`utf8mb4`) |
///
/// 손실은 서버가 IS 테이블을 채울 때 일어나므로 `CONVERT` 로 되돌릴 수 없다. 이모지가
/// 섞인 SQL 을 그대로 저장하면 운영자가 복사해 재현할 때 **다른 쿼리**가 된다.
///
/// # 판정에 서버 변수를 쓰지 않는다
///
/// `performance_schema_max_sql_text_length` 를 읽어 비교하면 인스턴스마다 값을 캐시해야
/// 하고 설정 변경을 놓친다. 대신 **문자 수**를 비교한다 — `?` 치환은 1문자를 1문자로
/// 바꾸므로 두 소스의 문자 수는 절단이 없을 때만 같다.
fn pick_sql_text<'a>(is_text: Option<&'a str>, ps_text: Option<&'a str>) -> Option<PickedSql<'a>> {
    // PS 상한은 설정(`performance_schema_max_sql_text_length`)으로 바뀌므로 값을
    // 하드코딩하지 않는다. 대신 **IS 와 비교**해 절단 여부를 추론한다.
    match (is_text, ps_text) {
        (Some(is), Some(ps)) => {
            let (is_chars, ps_chars) = (is.chars().count(), ps.chars().count());
            if ps_chars >= is_chars {
                // PS 가 짧지 않다 → 절단되지 않았다 → 무손실 쪽을 쓴다.
                Some(PickedSql {
                    text: ps,
                    maybe_truncated: is.len() >= IS_PROCESSLIST_INFO_MAX_BYTES,
                    lossy: false,
                })
            } else {
                // PS 가 잘렸다 → IS 를 쓴다. IS 는 `utf8mb3` 라 4바이트 문자를 잃었을 수 있다.
                Some(PickedSql {
                    text: is,
                    maybe_truncated: is.len() >= IS_PROCESSLIST_INFO_MAX_BYTES,
                    // 문자 수가 같으면 손실이 없다(`?` 치환은 1문자를 1문자로 바꾼다).
                    // 다르면 PS 가 잘려서 비교 자체가 불가능하므로 "알 수 없음" 이다.
                    lossy: true,
                })
            }
        }
        (Some(is), None) => Some(PickedSql {
            text: is,
            maybe_truncated: is.len() >= IS_PROCESSLIST_INFO_MAX_BYTES,
            // 비교 대상이 없으니 손실 여부를 알 수 없다. 보수적으로 표시한다.
            lossy: true,
        }),
        (None, Some(ps)) => Some(PickedSql {
            text: ps,
            // **IS 가 없으면 PS 의 절단을 판정할 근거가 없다.** IS 행이 사라지는 것은
            // 정상 경로다(두 조회 사이에 스레드가 끝난다). 근거 없이 `false` 를 박으면
            // 잘린 SQL 을 전문으로 오독하게 되므로 보수적으로 표시한다.
            maybe_truncated: true,
            lossy: false,
        }),
        (None, None) => None,
    }
}

/// [`pick_sql_text`] 의 결과. 텍스트와 **그 텍스트의 품질**을 함께 나른다.
#[derive(Debug, Clone, Copy)]
struct PickedSql<'a> {
    text: &'a str,
    /// 절단됐을 수 있다. 판정 근거가 없을 때도 `true` 다 (보수적).
    maybe_truncated: bool,
    /// 4바이트 문자가 `?` 로 손실됐을 수 있다 (`utf8mb3` 소스).
    lossy: bool,
}

/// 파싱 실패 사유를 짧게. **원문을 포함하지 않는다.**
fn short_reason(e: &dbmon_planparse::ParseError) -> &'static str {
    match e {
        dbmon_planparse::ParseError::InvalidJson(_) => "invalid_json",
        dbmon_planparse::ParseError::NotAPlan => "not_a_plan",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::{Env, EnvResolution};
    use dbmon_core::ids::InstanceId;
    use dbmon_core::inflight::Identity;
    use dbmon_core::instance::{Engine, EngineVersion, InstanceState};
    use dbmon_core::slow_query::DurationSource;

    fn instance() -> Instance {
        Instance {
            id: InstanceId::new("123456789012", "ap-northeast-2", "orders-prd-01").unwrap(),
            cluster_id: None,
            engine: Engine::Mysql,
            engine_version: EngineVersion::parse("8.4.5").unwrap(),
            env: EnvResolution::resolve(Env::Prd, None),
            state: InstanceState::Collecting,
            endpoint: Some("db.example".into()),
            port: 3306,
            dbi_resource_id: "db-ABC".into(),
            vpc_id: None,
            availability_zone: None,
            instance_class: None,
            allocated_storage_gb: None,
            is_cluster_writer: true,
            iam_auth_enabled: true,
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

    fn tracked() -> Tracked {
        Tracked {
            thread_id: 8842119,
            identity: Identity {
                digest: Some("mysqldigest".into()),
                db_user: Some("app".into()),
                db_host: Some("10.0.3.44".into()),
            },
            schema_name: Some("shop".into()),
            state: dbmon_core::inflight::TrackedState::Tracking,
            first_observed_at_ms: 1_000_000,
            started_at_ms: 1_000_000,
            started_at_ms_precise: None,
            max_time_secs: 4,
            best_duration_ms: 4_213,
            best_duration_source: DurationSource::Timer,
            last_seen_at_ms: 1_004_000,
            storage_key: Some(dbmon_core::ports::StoredKey::new("row-8842119")),
            touch_attempt_ms: Some(1_004_000),
            plan_attempts: 1,
            has_plan: true,
        }
    }

    fn input<'a>(
        inst: &'a Instance,
        tr: &'a Tracked,
        offset: &'a ClockOffset,
        policy: LiteralPolicy,
        full_sql: Option<&'a str>,
    ) -> CaptureInput<'a> {
        CaptureInput {
            instance: inst,
            tracked: tr,
            full_sql,
            stmt: None,
            plan_json: None,
            plan_source: PlanSource::None,
            plan_error: None,
            plan_tree: None,
            policy,
            policy_at_ms: 1_000_000,
            state: SlowQueryState::Finalized,
            finalize_reason: Some(FinalizeReason::Disappeared),
            now_ms: 1_004_500,
            offset,
            owner_worker: "task-abc",
            owner_epoch: Some(3),
        }
    }

    const RAW: &str = "SELECT * FROM users WHERE ssn = '900101-1234567' AND id = 42";

    #[test]
    fn full_policy_stores_raw_text() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let out = build(input(&i, &t, &o, LiteralPolicy::Full, Some(RAW)));
        assert_eq!(out.query.sql_text.as_deref(), Some(RAW));
        assert!(!out.masking_degraded);
        assert_eq!(out.query.literal_policy, LiteralPolicy::Full);
    }

    #[test]
    fn masked_policy_stores_canonical_without_literals() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let out = build(input(&i, &t, &o, LiteralPolicy::Masked, Some(RAW)));
        let text = out.query.sql_text.expect("마스킹본이 있어야 한다");
        assert!(!text.contains("900101"), "{text}");
        assert!(!text.contains("42"), "{text}");
        assert!(text.contains("ssn"), "식별자는 남아야 한다: {text}");
        assert!(!out.masking_degraded);
    }

    /// T-36 — 마스킹을 신뢰할 수 없으면 **저장하지 않는다.**
    #[test]
    fn t36_unmaskable_sql_degrades_to_off() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        // 닫히지 않은 인용부호 → 후조건 실패
        let broken = "SELECT * FROM users WHERE n = 'unterminated";
        let out = build(input(&i, &t, &o, LiteralPolicy::Masked, Some(broken)));
        assert!(out.masking_degraded, "강등을 보고해야 한다");
        assert_eq!(out.query.literal_policy, LiteralPolicy::Off);
        assert_eq!(out.query.sql_text, None, "원문을 저장하면 안 된다");
    }

    #[test]
    fn full_policy_does_not_degrade_on_unterminated_quote() {
        // `full` 은 원문 저장이 의도다. 후조건을 강제하지 않는다.
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let broken = "SELECT * FROM users WHERE n = 'unterminated";
        let out = build(input(&i, &t, &o, LiteralPolicy::Full, Some(broken)));
        assert!(!out.masking_degraded);
        assert_eq!(out.query.sql_text.as_deref(), Some(broken));
    }

    #[test]
    fn off_policy_stores_no_text_but_keeps_digest() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let out = build(input(&i, &t, &o, LiteralPolicy::Off, Some(RAW)));
        assert_eq!(out.query.sql_text, None);
        // 다이제스트는 남아야 한다 — 집계는 계속 되어야 한다.
        assert_eq!(out.query.app_digest, normalize(RAW).app_digest);
        assert_eq!(out.query.statement_type, StatementType::Select);
    }

    #[test]
    fn digest_text_is_used_when_full_sql_is_missing() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let stmt = StmtCurrentRow {
            digest_text: Some("SELECT * FROM `users` WHERE `id` = ?".into()),
            ..Default::default()
        };
        let mut inp = input(&i, &t, &o, LiteralPolicy::Masked, None);
        inp.stmt = Some(&stmt);
        let out = build(inp);
        assert!(out.query.sql_text.is_some());
        assert!(!out.masking_degraded, "DIGEST_TEXT 는 이미 마스킹돼 있다");
    }

    /// **실행 중인 문장의 카운터를 사실로 저장하지 않는다.**
    ///
    /// MySQL 은 실행 중에 `ROWS_EXAMINED = 0` 을 준다(실측: 12만 행을 훑는 조인이
    /// 완료 후 120,005). 그 0 을 저장하면 화면이 `0` 을 보여주고 튜닝 모델은 실행계획과의
    /// 모순을 근거로 신뢰도를 내린다 — 실제로 전체 4,814건 중 2,016건이 그 상태였다.
    ///
    /// 이 테스트가 없어서 결함이 통과했다. 기존 픽스처는 전부 `end_event_id: None`
    /// (실행 중)인데 카운터 흐름을 아무도 검사하지 않았다.
    #[test]
    fn counters_from_a_running_statement_are_unknown_not_zero() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));

        // ① 실행 중 — MySQL 이 주는 0 을 버린다.
        let running = StmtCurrentRow {
            rows_examined: Some(0),
            rows_sent: Some(0),
            rows_affected: Some(0),
            created_tmp_tables: Some(0),
            created_tmp_disk_tables: Some(0),
            select_full_join: Some(0),
            sort_merge_passes: Some(0),
            // 이 둘은 옵티마이즈 시점에 정해진다 — 실행 중에도 유효하다.
            no_index_used: Some(true),
            no_good_index_used: Some(true),
            lock_time_ps: Some(4_000_000),
            end_event_id: None,
            ..Default::default()
        };
        let mut inp = input(&i, &t, &o, LiteralPolicy::Masked, Some(RAW));
        inp.stmt = Some(&running);
        let s = build(inp).query.stats;
        assert_eq!(s.rows_examined, None, "0 을 '0행을 훑었다' 로 저장했다");
        assert_eq!(s.rows_sent, None);
        assert_eq!(s.rows_affected, None);
        assert_eq!(s.tmp_tables, None);
        assert_eq!(s.tmp_disk_tables, None);
        assert_eq!(s.sort_merge_passes, None);
        assert_eq!(s.full_join, None);
        // 실행 중에도 유효한 것은 남아야 한다 — 다 버리면 기능이 죽는다.
        assert_eq!(s.no_index_used, Some(true));
        assert_eq!(s.no_good_index_used, Some(true));
        assert_eq!(s.lock_time_ms, Some(0), "LOCK_TIME 은 실행 중에도 유효하다");

        // ② 완료 — 그대로 통과해야 한다.
        let done = StmtCurrentRow {
            rows_examined: Some(120_005),
            rows_sent: Some(5),
            created_tmp_disk_tables: Some(2),
            select_full_join: Some(1),
            end_event_id: Some(4),
            ..Default::default()
        };
        let mut inp = input(&i, &t, &o, LiteralPolicy::Masked, Some(RAW));
        inp.stmt = Some(&done);
        let s = build(inp).query.stats;
        assert_eq!(s.rows_examined, Some(120_005));
        assert_eq!(s.rows_sent, Some(5));
        assert_eq!(s.tmp_disk_tables, Some(2));
        assert_eq!(s.full_join, Some(true));
    }

    #[test]
    fn truncation_is_flagged_at_the_ceiling() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let long = format!("SELECT '{}'", "x".repeat(IS_PROCESSLIST_INFO_MAX_BYTES));
        let out = build(input(&i, &t, &o, LiteralPolicy::Full, Some(&long)));
        assert!(out.query.sql_text_truncated, "65,535바이트면 절단이다");

        let short = build(input(&i, &t, &o, LiteralPolicy::Full, Some(RAW)));
        assert!(!short.query.sql_text_truncated);
    }

    /// T-16 — 플랜 조건식의 리터럴이 저장되면 안 된다.
    #[test]
    fn t16_plan_literals_are_masked() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let plan = r#"{"query_block":{"table":{"table_name":"users",
            "access_type":"ALL","rows_examined_per_scan":812345,
            "attached_condition":"(`shop`.`users`.`ssn` = '900101-1234567')"}}}"#;
        let mut inp = input(&i, &t, &o, LiteralPolicy::Masked, Some(RAW));
        inp.plan_json = Some(plan);
        inp.plan_source = PlanSource::Rerun;
        let out = build(inp);
        let json = out.query.plan.normalized_json.expect("마스킹된 플랜");
        assert!(!json.contains("900101"), "{json}");
        assert_eq!(out.query.plan.source, PlanSource::Rerun);
        assert_eq!(out.query.plan.referenced_tables, vec!["shop.users"]);
        assert!(out.query.plan.fingerprint.is_some());
        assert!(out.query.plan.error.is_none());
    }

    /// **거대한 계획 때문에 슬로우 쿼리를 잃지 않는다.**
    ///
    /// DynamoDB 항목 상한(400KB)을 넘기면 `upsert_merged` 가 실패하고, 그러면 계획이
    /// 아니라 **레코드 전체**가 사라진다. 계획만 버리고 나머지를 남긴다.
    #[test]
    fn an_oversized_plan_is_dropped_but_the_record_survives() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        // 테이블을 많이 만들어 정규화된 JSON 이 상한을 넘게 한다.
        let tables: Vec<String> = (0..4_000)
            .map(|n| {
                format!(
                    r#"{{"table":{{"table_name":"t{n}","access_type":"ALL","rows_examined_per_scan":10}}}}"#
                )
            })
            .collect();
        let plan = format!(
            r#"{{"query_block":{{"nested_loop":[{}]}}}}"#,
            tables.join(",")
        );
        assert!(
            plan.len() > MAX_PLAN_BYTES,
            "테스트 입력이 상한을 넘어야 한다"
        );

        let mut inp = input(&i, &t, &o, LiteralPolicy::Masked, Some(RAW));
        inp.plan_json = Some(&plan);
        inp.plan_source = PlanSource::Rerun;
        let out = build(inp);

        assert!(
            out.query.plan.normalized_json.is_none(),
            "계획을 버려야 한다"
        );
        // **왜 없는지 말한다.** "계획 없음" 과 "너무 커서 버렸다" 는 다른 사실이다.
        let err = out.query.plan.error.expect("사유");
        assert!(err.starts_with("too_large:"), "{err}");
        // 레코드는 살아 있다.
        assert!(out.query.sql_text.is_some());
        assert!(
            !out.query.plan.referenced_tables.is_empty(),
            "참조 테이블은 남긴다"
        );
    }

    #[test]
    fn plan_parse_failure_does_not_leak_the_plan() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let mut inp = input(&i, &t, &o, LiteralPolicy::Masked, Some(RAW));
        inp.plan_json = Some("{ not json 'secret-value'");
        inp.plan_source = PlanSource::Rerun;
        let out = build(inp);
        assert!(out.plan_parse_failed);
        let err = out.query.plan.error.expect("사유가 있어야 한다");
        assert!(!err.contains("secret-value"), "{err}");
        assert_eq!(out.query.plan.source, PlanSource::None);
        assert_eq!(out.query.plan.normalized_json, None);
    }

    #[test]
    fn timer_wait_overrides_polled_duration() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let stmt = StmtCurrentRow {
            timer_wait_ps: Some(4_213_000_000_000),
            lock_time_ps: Some(112_000_000),
            rows_examined: Some(8_213_445),
            // **완료된 문장이다.** 실행 중이면 MySQL 이 `ROWS_EXAMINED = 0` 을 주므로
            // 위 값과 함께 오는 조합이 존재하지 않는다(실측). `TIMER_WAIT` 는 양쪽에서
            // 유효하니 이 테스트의 주장은 그대로 성립한다.
            end_event_id: Some(4),
            ..Default::default()
        };
        let mut inp = input(&i, &t, &o, LiteralPolicy::Full, Some(RAW));
        inp.stmt = Some(&stmt);
        let out = build(inp);
        assert_eq!(
            out.query.duration_ms, 4_213,
            "초 단위 근사(4000)보다 정확하다"
        );
        assert_eq!(out.query.duration_source, DurationSource::Timer);
        assert_eq!(out.query.stats.rows_examined, Some(8_213_445));
        assert_eq!(out.query.stats.lock_time_ms, Some(0), "112µs 는 0ms 다");
    }

    #[test]
    fn ended_at_is_only_set_when_end_was_observed() {
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));

        let observed = build(input(&i, &t, &o, LiteralPolicy::Full, Some(RAW)));
        assert!(observed.query.ended_at_ms.is_some());
        assert_eq!(observed.query.abandoned_reason, None);

        // 강제 확정은 종료를 관측하지 못했다 → 추측한 시각을 저장하지 않는다.
        let mut inp = input(&i, &t, &o, LiteralPolicy::Full, Some(RAW));
        inp.finalize_reason = Some(FinalizeReason::TooLong);
        let forced = build(inp);
        assert_eq!(forced.query.ended_at_ms, None);
        assert_eq!(forced.query.abandoned_reason.as_deref(), Some("too_long"));
        assert!(forced.query.long_running);
    }

    #[test]
    fn record_id_is_stable_across_prefetch_and_finalize() {
        // 선행 저장과 확정이 **같은 키**여야 병합이 성립한다.
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let mut pre = input(&i, &t, &o, LiteralPolicy::Full, Some(RAW));
        pre.state = SlowQueryState::InFlight;
        pre.finalize_reason = None;
        let a = build(pre);
        let b = build(input(&i, &t, &o, LiteralPolicy::Full, Some(RAW)));
        assert_eq!(a.query.record_id, b.query.record_id);
        assert_eq!(a.query.state, SlowQueryState::InFlight);
        assert_eq!(b.query.state, SlowQueryState::Finalized);
    }

    #[test]
    fn no_text_at_all_still_produces_a_record() {
        // 심층 조회가 전부 실패해도 "느린 쿼리가 있었다"는 사실은 남아야 한다.
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let out = build(input(&i, &t, &o, LiteralPolicy::Full, None));
        assert_eq!(out.query.sql_text, None);
        assert!(
            out.query.app_digest.starts_with(UNKNOWN_DIGEST_PREFIX),
            "{}",
            out.query.app_digest
        );
        // **추적기가 모아 둔 최선의 관측을 쓴다.** 이 tick 에 `stmt` 가 없어도 앞선
        // 심층 조회에서 얻은 `TIMER_WAIT` 값이 남아 있다 — 전에는 그걸 잃고 초 단위
        // 값으로 떨어졌다(확정 경로가 그 상태였다, 교차 리뷰 27라운드).
        assert_eq!(out.query.duration_ms, 4_213);
        assert_eq!(out.query.duration_source, DurationSource::Timer);
    }

    /// **종료 시각은 마지막으로 본 시각이다.**
    ///
    /// 사라진 것을 알아챈 시각을 쓰면 tick 간격만큼 부풀린다. `detect_interval_ms` 를
    /// 60초로 두면 2.1초 쿼리가 62초로 저장되고, `merge_duration` 이 구간을 가장 정확한
    /// 것으로 보므로 타이머 증거를 덮는다(교차 리뷰 28라운드).
    #[test]
    fn the_end_time_is_the_last_observation_not_the_discovery() {
        let (i, mut t, o) = (instance(), tracked(), ClockOffset::restored(0));
        t.last_seen_at_ms = 1_004_000;
        // 사라진 것을 60초 뒤에 알아챘다.
        let discovered_ms = t.last_seen_at_ms + 60_000;
        let out = build(CaptureInput {
            instance: &i,
            tracked: &t,
            full_sql: Some(RAW),
            stmt: None,
            plan_json: None,
            plan_source: PlanSource::None,
            plan_error: None,
            plan_tree: None,
            policy: LiteralPolicy::Full,
            policy_at_ms: 1_000_000,
            state: SlowQueryState::Finalized,
            finalize_reason: Some(FinalizeReason::Disappeared),
            now_ms: discovered_ms,
            offset: &o,
            owner_worker: "w1",
            owner_epoch: Some(1),
        });
        assert_eq!(
            out.query.ended_at_ms,
            Some(1_004_000),
            "알아챈 시각을 종료로 적었다 — tick 간격만큼 부풀린다"
        );
    }

    #[test]
    fn owner_fields_are_recorded_for_orphan_cleanup() {
        // F4 — 스케줄러 리더가 이 필드로 고아를 판정한다.
        let (i, t, o) = (instance(), tracked(), ClockOffset::restored(0));
        let out = build(input(&i, &t, &o, LiteralPolicy::Full, Some(RAW)));
        assert_eq!(out.query.owner_worker.as_deref(), Some("task-abc"));
        assert_eq!(out.query.owner_epoch, Some(3));
        assert_eq!(out.query.last_seen_at_ms, Some(1_004_000));
    }
}
