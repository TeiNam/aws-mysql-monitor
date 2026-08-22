//! 부트스트랩 감사 레코드 (M3-15, FR-CRD-10,
//! [07 §4](../../../../docs/07-credentials-bootstrap.md)).
//!
//! # 무엇을 남기고 무엇을 남기지 않는가
//!
//! | 남긴다 | 남기지 않는다 |
//! |---|---|
//! | 누가(`actor`)·언제·어느 인스턴스 | 마스터 자격증명 값 |
//! | 자격증명 **소스** (`rds_managed_secret` / `tagged_secret`) | 비밀번호가 든 문장 원문 |
//! | 실행한 문장 — [`Statement`](dbmon_core::bootstrap::sql::Statement) 의 마스킹된 형태 | |
//! | before/after 상태, 차단 사유, 초과 권한 | |
//!
//! 문장이 마스킹된 형태로만 들어가는 것은 **타입이 보장한다** — `Statement` 의
//! `Serialize` 가 그렇게 구현돼 있어서 이 파일이 실수할 방법이 없다(T-19).
//!
//! # 저장 위치
//!
//! `dbmon-config` 테이블의 `PK = AUDIT#<yyyy-mm>` 파티션이다. 월 단위로 나누는 이유는
//! 아카이브 단위가 월이기 때문이다(Iceberg `audit_log`, 3년 보관).

use dbmon_core::bootstrap::{Blocker, Plan, sql::Statement};
use dbmon_core::env::Env;
use dbmon_core::time::EpochMs;

use super::secret::CredentialSource;

/// 감사 이벤트 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEvent {
    /// 계획만 만들었다 (대상 DB 를 읽었지만 바꾸지 않았다).
    BootstrapPlan,
    /// 실행했다.
    BootstrapApply,
}

/// 실행 결과.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    Success,
    /// 일부만 실행됐다. **어디까지 갔는지가 중요하다** — `GRANT` 도중 실패하면
    /// 계정은 있고 권한은 반쪽이다.
    Partial {
        completed: usize,
        total: usize,
    },
    Failed {
        reason: String,
    },
    /// 차단돼서 실행하지 않았다.
    Blocked,
}

/// 실행한 액션 하나의 기록.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditAction {
    pub kind: &'static str,
    /// **`Statement` 를 그대로 담는다.** `Serialize` 가 마스킹된 형태를 낸다.
    pub statement: Option<Statement>,
    /// SQL 이 아닌 액션의 설명 (예: IAM 인증 활성화 안내).
    pub detail: Option<String>,
    pub result: &'static str,
}

/// 감사 레코드.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditRecord {
    pub event: AuditEvent,
    /// 주체. 지금은 `shared-token` 같은 값이고, Cognito 가 붙으면 `sub` 다.
    pub actor: String,
    pub at_ms: EpochMs,
    pub instance_id: String,
    pub env: Env,
    pub credential_source: Option<CredentialSource>,
    pub privilege_mode: &'static str,
    pub monitor_user: String,
    pub monitor_host: String,
    pub actions: Vec<AuditAction>,
    pub before: StateSnapshot,
    pub after: Option<StateSnapshot>,
    pub result: AuditResult,
    /// prd 에서 식별자를 타이핑했는가.
    pub confirmation_typed: bool,
    /// 진행을 막은 사유 (사람이 읽는 문장).
    pub blockers: Vec<String>,
    /// 요구하지 않은 권한. 회수하지 않았다는 사실이 함께 남는다.
    pub excess_privileges: Vec<String>,
}

/// 계정 상태 요약 — before/after 비교용.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StateSnapshot {
    pub user_exists: bool,
    pub auth_plugin: Option<String>,
    pub requires_ssl: bool,
    pub locked: bool,
    pub iam_auth_enabled: bool,
    /// 권한 줄 수. 전문을 넣지 않는 이유는 크기다 — 필요하면 재조회한다.
    pub grant_scopes: usize,
}

impl StateSnapshot {
    pub fn of(state: &dbmon_core::bootstrap::CurrentState) -> Self {
        Self {
            user_exists: state.user_exists,
            auth_plugin: state.auth_plugin.clone(),
            requires_ssl: state.requires_ssl,
            locked: state.locked,
            iam_auth_enabled: state.iam_auth_enabled,
            grant_scopes: state.grants.scopes().count(),
        }
    }
}

/// 계획의 액션을 감사 기록으로 옮긴다. `executed` 개까지 성공한 것으로 표시한다.
///
/// `executed` 를 받는 이유: 중간에 실패하면 **어느 문장까지 갔는지**가 복구에
/// 필요하다. 전부 성공했다고 적으면 반쪽 상태를 사람이 모른다.
pub fn actions_of(plan: &Plan, executed: usize) -> Vec<AuditAction> {
    plan.actions
        .iter()
        .enumerate()
        .map(|(i, a)| match a {
            dbmon_core::bootstrap::Action::Sql(s) => AuditAction {
                kind: "sql",
                statement: Some(s.clone()),
                detail: None,
                result: if i < executed { "ok" } else { "skipped" },
            },
            dbmon_core::bootstrap::Action::EnableIamAuth { instance_id } => AuditAction {
                kind: "modify_rds",
                statement: None,
                detail: Some(format!(
                    "EnableIAMDatabaseAuthentication=true ({instance_id}) — 앱이 실행하지 않는다"
                )),
                // 우리가 실행하지 않는 액션이다. `ok` 라고 적으면 거짓이 된다.
                result: "manual",
            },
        })
        .collect()
}

/// 차단 사유를 사람이 읽는 문장으로.
pub fn blocker_messages(blockers: &[Blocker]) -> Vec<String> {
    blockers.iter().map(|b| b.to_string()).collect()
}

/// `AUDIT#<yyyy-mm>` 파티션 키. 아카이브 단위가 월이라 월로 나눈다.
pub fn audit_pk(at_ms: EpochMs) -> String {
    format!("AUDIT#{}", year_month(at_ms))
}

/// 정렬 키 — 시각 + 인스턴스. **같은 밀리초에 두 건이 와도 덮어쓰지 않는다.**
///
/// 인스턴스를 뒤에 붙이는 이유: 일괄 부트스트랩(FR-CRD-08)은 한 요청에서 여러
/// 인스턴스를 순차 처리하고, 빠른 인스턴스 둘이 같은 밀리초에 끝날 수 있다.
pub fn audit_sk(at_ms: EpochMs, instance_id: &str) -> String {
    format!("{at_ms:013}#{instance_id}")
}

/// `yyyy-mm` — UTC 기준.
///
/// **로컬 시간대를 쓰지 않는다.** 아카이브 잡이 UTC 로 도는데 파티션이 로컬이면
/// 월 경계에서 레코드가 두 파티션에 흩어진다.
fn year_month(at_ms: EpochMs) -> String {
    let days = at_ms.div_euclid(86_400_000);
    let (y, m, _) = civil_from_days(days);
    format!("{y:04}-{m:02}")
}

/// 일수 → (년, 월, 일). Howard Hinnant 의 `civil_from_days`.
///
/// `chrono` 를 들이지 않는다 — 이 프로젝트가 날짜 계산에 쓰는 방식이 이미
/// [`crate::mysql::connect`] 에 있고, 같은 알고리즘이다.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::bootstrap::{AuthMethod, CurrentState, Desired, PrivilegeMode};

    fn desired() -> Desired {
        Desired {
            user: "dbmon".into(),
            host: "10.1.%".into(),
            auth: AuthMethod::IamDbAuth,
            mode: PrivilegeMode::Broad,
            schemas: vec![],
        }
    }

    fn plan_for_fresh() -> Plan {
        dbmon_core::bootstrap::plan(
            &desired(),
            &CurrentState {
                iam_auth_enabled: true,
                ..Default::default()
            },
            "inst",
            false,
        )
        .expect("계획")
    }

    /// **감사 레코드에 비밀번호가 들어갈 수 없다** (T-19).
    ///
    /// `Statement` 의 `Serialize` 가 마스킹하므로 이 파일이 실수할 방법이 없다 —
    /// 그 사실을 테스트로 고정한다.
    #[test]
    fn t19_a_password_statement_serializes_redacted_in_the_audit_record() {
        let pw = "Xk7p4ssw0rdv4lue32chAAAAAAAAAAAA";
        let stmt =
            dbmon_core::bootstrap::sql::create_user_with_password(&desired(), pw).expect("문장");
        let record = AuditRecord {
            event: AuditEvent::BootstrapApply,
            actor: "shared-token".into(),
            at_ms: 1_755_527_391_000,
            instance_id: "acct/ap-northeast-2/orders-prd-01".into(),
            env: Env::Prd,
            credential_source: Some(CredentialSource::RdsManagedSecret),
            privilege_mode: "broad",
            monitor_user: "dbmon".into(),
            monitor_host: "10.1.%".into(),
            actions: vec![AuditAction {
                kind: "sql",
                statement: Some(stmt),
                detail: None,
                result: "ok",
            }],
            before: StateSnapshot::of(&CurrentState::default()),
            after: None,
            result: AuditResult::Success,
            confirmation_typed: true,
            blockers: vec![],
            excess_privileges: vec![],
        };

        let json = serde_json::to_string(&record).expect("직렬화");
        assert!(!json.contains(pw), "감사 레코드에 비밀번호가 들어갔다");
        assert!(json.contains("<redacted>"));
        // 자격증명 소스는 남는다.
        assert!(json.contains("rds_managed_secret"));
    }

    /// **중간 실패는 `skipped` 로 남는다.** 전부 `ok` 라고 적으면 반쪽 상태를 모른다.
    #[test]
    fn a_partial_run_records_which_statements_did_not_execute() {
        let plan = plan_for_fresh();
        assert!(plan.actions.len() >= 2, "문장이 둘 이상이어야 의미가 있다");

        let actions = actions_of(&plan, 1);
        assert_eq!(actions[0].result, "ok");
        assert_eq!(actions[1].result, "skipped");

        let all = actions_of(&plan, plan.actions.len());
        assert!(all.iter().all(|a| a.result == "ok"));
    }

    /// 우리가 실행하지 않는 액션은 `ok` 가 아니다.
    #[test]
    fn rds_modify_actions_are_recorded_as_manual() {
        let plan = dbmon_core::bootstrap::plan(
            &desired(),
            &CurrentState::default(), // iam_auth_enabled = false
            "orders-prd-01",
            false,
        )
        .expect("계획");
        let actions = actions_of(&plan, plan.actions.len());
        let modify = actions
            .iter()
            .find(|a| a.kind == "modify_rds")
            .expect("액션");
        assert_eq!(modify.result, "manual", "우리가 실행하지 않았다");
        assert!(modify.statement.is_none());
    }

    /// 파티션 키는 **UTC 월**이다.
    #[test]
    fn the_partition_key_is_the_utc_year_month() {
        // 2026-08-23T00:00:00Z
        assert_eq!(audit_pk(1_787_443_200_000), "AUDIT#2026-08");
        // 1970-01-01
        assert_eq!(audit_pk(0), "AUDIT#1970-01");
        // 월 경계 직전/직후 (2026-09-01T00:00:00Z = 1788220800000)
        assert_eq!(audit_pk(1_788_220_800_000 - 1), "AUDIT#2026-08");
        assert_eq!(audit_pk(1_788_220_800_000), "AUDIT#2026-09");
    }

    /// **같은 밀리초의 두 인스턴스가 서로를 덮지 않는다.**
    #[test]
    fn the_sort_key_separates_instances_within_the_same_millisecond() {
        let a = audit_sk(1_787_443_200_000, "acct/ap-northeast-2/a");
        let b = audit_sk(1_787_443_200_000, "acct/ap-northeast-2/b");
        assert_ne!(a, b);
        // 시각 순서가 문자열 순서와 같아야 한다 (0 패딩).
        assert!(audit_sk(999, "x") < audit_sk(1_000, "x"));
        assert!(audit_sk(1_787_443_200_000, "x") > audit_sk(1_787_443_100_000, "z"));
    }

    #[test]
    fn blockers_become_readable_messages() {
        let msgs = blocker_messages(&[
            Blocker::SslNotRequired,
            Blocker::AuthPluginMismatch {
                found: "mysql_native_password".into(),
                expected: "AWSAuthenticationPlugin".into(),
            },
        ]);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].contains("mysql_native_password"));
        assert!(msgs.iter().all(|m| !m.is_empty()));
    }
}
