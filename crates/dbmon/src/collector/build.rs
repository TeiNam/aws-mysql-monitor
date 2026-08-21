//! 관측 → [`SlowQuery`] 레코드 조립.
//!
//! # 정규화·마스킹이 저장보다 **앞**이다 (F2)
//!
//! 초기 설계의 파이프라인은 `선행 저장 → (확정 시) 정규화·마스킹` 이었다.
//! 그러면 `masked` 정책 인스턴스에서도 **선행 저장 시점에 원문이 DynamoDB 에 들어간다.**
//! 확정되지 않은 고아 레코드는 영구히 마스킹되지 않은 상태로 남고, 그 사이 증분
//! 내보내기가 아카이브로 옮긴다. [08 §6.1](../../../../docs/08-security-auth.md)의
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
    CaptureSource, DurationSource, ExecStats, LiteralPolicy, PlanBundle, PlanSource, SlowQuery,
    SlowQueryState, UNKNOWN_DIGEST_PREFIX,
};
use dbmon_core::time::EpochMs;
use dbmon_normalize::{DIGEST_ALGO_VERSION, StatementType, normalize};

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
    let (duration_ms, duration_source) = match stmt.and_then(|s| s.timer_wait_ps) {
        // `TIMER_WAIT` 는 피코초이고 `TIME` 의 초 단위 오차를 없앤다.
        Some(ps) => ((ps / 1_000_000_000) as i64, DurationSource::Timer),
        None => (tracked.duration_ms(), DurationSource::Polled),
    };

    // 종료를 관측하지 못했으면 `ended_at_ms` 를 남기지 않는다 — 추측한 시각을
    // 사실처럼 저장하면 리포트가 틀린다.
    let ended_at_ms = finalize_reason
        .filter(|r| r.observed_end())
        .map(|_| offset.to_db_time(now_ms));

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

fn exec_stats(s: &StmtCurrentRow) -> ExecStats {
    ExecStats {
        rows_examined: s.rows_examined,
        rows_sent: s.rows_sent,
        rows_affected: s.rows_affected,
        // `LOCK_TIME` 도 피코초다.
        lock_time_ms: s.lock_time_ps.map(|ps| (ps / 1_000_000_000) as i64),
        tmp_tables: s.created_tmp_tables,
        tmp_disk_tables: s.created_tmp_disk_tables,
        sort_merge_passes: s.sort_merge_passes,
        no_index_used: s.no_index_used,
        no_good_index_used: s.no_good_index_used,
        full_join: s.select_full_join.map(|v| v > 0),
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
            last_seen_at_ms: 1_004_000,
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
        assert_eq!(out.query.duration_ms, 4_000);
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
