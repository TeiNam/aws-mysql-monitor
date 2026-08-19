//! 대상 MySQL 읽기 포트.
//!
//! **이 포트는 AWS 를 모른다.** IAM 토큰 발급은 [`super::AuthTokenProvider`] 로 분리했다
//! ([02 §5](../../../docs/02-architecture.md) F31 정정).
//! 어댑터(`dbmon::mysql`)는 "비밀번호 문자열을 주는 무언가"만 알고, 그게 IAM 토큰인지
//! Secrets Manager 값인지 모른다.
//!
//! **모든 문장은 읽기 전용이다.** DDL·DML 을 실행하는 메서드가 하나도 없다
//! (FR-CAP-09, FR-AI-10 — `ANALYZE TABLE`·`FLUSH`·`TRUNCATE` 금지).

use crate::digest::DigestSnapshotRow;
use crate::error::Result;
use crate::time::EpochMs;
use async_trait::async_trait;
use std::collections::BTreeMap;

/// `performance_schema.processlist` 한 행 (경량 탐지 쿼리).
///
/// **`INFO` 를 읽지 않는다.** 여기의 `INFO` 는 1024바이트 절단본이므로 무의미하다
/// ([ADR-005](../../../docs/03-decisions.md)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub id: u64,
    pub user: Option<String>,
    pub host: Option<String>,
    pub db: Option<String>,
    pub command: Option<String>,
    /// **초 단위 정수다.** 임계값 2초면 실제 탐지 시점은 2.0~3.0초 사이.
    pub time_secs: i64,
    pub state: Option<String>,
}

/// 탐지 쿼리 결과. `db_now_ms` 를 **같은 왕복으로** 받아 시계 오프셋을 추정한다 (F14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub rows: Vec<ProcessRow>,
    /// 대상 DB 의 `NOW(6)`. 탐지 쿼리에 컬럼으로 붙이므로 추가 왕복이 없다.
    ///
    /// **행이 0개면 `None` 이다** — 정상 상태의 99% 가 그렇다. 컬럼으로 붙인 값은
    /// 행이 있어야 돌아온다. 초기 설계는 "매 tick 에 함께 읽으므로 추가 왕복이 없다"고
    /// 적었는데, 0행일 때 시각을 못 받는다는 점을 놓쳤다.
    /// → 오프셋 갱신은 [`TargetDb::db_now_ms`] 를 낮은 빈도로 호출해 보완한다.
    pub db_now_ms: Option<EpochMs>,
    /// `LIMIT` 에 걸려 잘렸다 → `detect_overflow` 메트릭.
    pub truncated: bool,
}

/// `information_schema.PROCESSLIST` 타깃 조회 — 전문 SQL.
///
/// **`INFO` 는 `varchar(21845)` 이고 65,535바이트에서 절단된다** (M1-1 실측,
/// [19 §A](../../../docs/19-m1-findings.md)). `LONGTEXT` 가 아니다.
/// `performance_schema` 의 1,024바이트보다 64배 넉넉하지만 무제한은 아니므로,
/// 정확히 65,535바이트를 받으면 `sql_text_truncated = true` 로 표시한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullSqlRow {
    pub id: u64,
    pub db: Option<String>,
    pub user: Option<String>,
    pub host: Option<String>,
    pub time_secs: i64,
    pub info: Option<String>,
}

/// `events_statements_current` 정확 지표. 타이머는 **피코초**다.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StmtCurrentRow {
    pub processlist_id: u64,
    pub thread_id: u64,
    pub event_name: Option<String>,
    pub current_schema: Option<String>,
    pub digest: Option<String>,
    pub digest_text: Option<String>,
    /// `events_statements_current.SQL_TEXT` — **무손실 `utf8mb4`** 원문.
    /// `performance_schema_max_sql_text_length`(기본 1024)로 잘린다.
    /// `information_schema.PROCESSLIST.INFO` 와 상보적이다: 이쪽은 짧지만 정확하고,
    /// 저쪽은 길지만 4바이트 문자를 잃는다 (19 §A-2).
    pub sql_text: Option<String>,
    pub timer_wait_ps: Option<u64>,
    pub lock_time_ps: Option<u64>,
    pub rows_examined: Option<u64>,
    pub rows_sent: Option<u64>,
    pub rows_affected: Option<u64>,
    pub created_tmp_tables: Option<u64>,
    pub created_tmp_disk_tables: Option<u64>,
    pub select_full_join: Option<u64>,
    pub sort_merge_passes: Option<u64>,
    pub no_index_used: Option<bool>,
    pub no_good_index_used: Option<bool>,
    /// `STATEMENT` 면 프로시저 내부 문장이다 — 양쪽 SQL 을 모두 저장하고 `is_nested=true`.
    pub nesting_event_type: Option<String>,
}

impl StmtCurrentRow {
    pub fn is_nested_statement(&self) -> bool {
        self.nesting_event_type.as_deref() == Some("STATEMENT")
    }
}

/// 다이제스트 텍스트 2차 조회 결과 (캐시에 없는 다이제스트만).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestTextRow {
    pub digest: String,
    pub digest_text: Option<String>,
    /// `QUERY_SAMPLE_TEXT` — 실시간 캡처에 안 걸린 다이제스트의 유일한 실행 가능 샘플.
    /// **리터럴을 포함하므로 정책 적용 대상이다.**
    pub query_sample_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestSnapshot {
    pub rows: Vec<DigestSnapshotRow>,
    /// 다음 스냅샷의 `LAST_SEEN` 필터 기준이 된다. **대상 DB 시각이다.**
    pub db_now_ms: EpochMs,
    /// `DIGEST IS NULL` 오버플로 집계 행이 존재한다
    /// → `performance_schema_digests_size` 상향 권고.
    pub overflow_detected: bool,
}

/// `EXPLAIN ... FOR CONNECTION` 실패 사유.
///
/// **에러 메시지 문자열이 아니라 에러 코드로 분류한다.** `lc_messages` 설정에 따라
/// 메시지가 번역되므로 문자열 매칭은 깨진다 ([05 §2.4](../../../docs/05-collector.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFailure {
    /// 1094 `ER_NO_SUCH_THREAD` — 스레드가 이미 종료됨. 폴백 시도 가능.
    ThreadGone,
    /// 3012 `ER_EXPLAIN_NOT_SUPPORTED` — CALL·SET·DDL 등. 폴백 없음.
    NotExplainable,
    /// 1044 / 1045 / 1227 — 타인 커넥션을 explain 할 권한이 없다.
    ///
    /// **RDS 에서는 항상 이 결과다** (M1-4 실측). `PROCESS` 도 `SUPER` 도 부족하고
    /// 정적 전역 권한 전체가 필요하다 → `Rerun` 폴백으로 전환한다.
    /// 자가진단 실패로 승격하지 **않는다** — 고칠 수 있는 설정이 아니다.
    Denied,
    /// 1142 `ER_TABLEACCESS_DENIED_ERROR` — 읽기 전용 계정으로 DML 을 `EXPLAIN` 했다.
    /// → `RerunAsSelect`(DML 을 SELECT 로 변환) 경로로 전환한다.
    DmlPrivilegeMissing,
    /// `EXPLAIN` 은 성공했지만 결과가 비어 있다 (유휴 커넥션이었다).
    ///
    /// 실측: 유휴 커넥션에 `FOR CONNECTION` 을 걸면 **에러가 아니라 빈 결과**다.
    /// 에러로 취급하면 정상 상태를 실패로 센다.
    NoStatement,
    /// 클라이언트 타임아웃. 연결을 폐기한다.
    Timeout,
    /// 분류되지 않은 코드. 기록해 두고 나중에 표에 추가한다.
    Other(u16),
}

impl PlanFailure {
    /// 실측으로 확정한 매핑 (MySQL 8.4.11 / 8.0.46,
    /// [19 §B](../../../docs/19-m1-findings.md)).
    pub fn from_mysql_error_code(code: u16) -> Self {
        match code {
            1094 => Self::ThreadGone,
            3012 => Self::NotExplainable,
            1142 => Self::DmlPrivilegeMissing,
            1044 | 1045 | 1227 => Self::Denied,
            other => Self::Other(other),
        }
    }

    /// `plan_error` 에 저장하는 문자열.
    pub fn as_str(&self) -> String {
        match self {
            Self::ThreadGone => "thread_gone".into(),
            Self::NotExplainable => "not_explainable".into(),
            Self::Denied => "denied".into(),
            Self::DmlPrivilegeMissing => "dml_privilege_missing".into(),
            Self::NoStatement => "no_statement".into(),
            Self::Timeout => "timeout".into(),
            Self::Other(c) => format!("other:{c}"),
        }
    }

    /// 원문 그대로 `EXPLAIN` 재실행을 시도할 가치가 있는가.
    ///
    /// `Denied` 도 포함한다 — 타인 커넥션 explain 권한이 없어도 **우리 커넥션에서
    /// 재실행**하는 것은 `SELECT` 권한만으로 된다(M1-4b).
    /// `NotExplainable` 은 재실행해도 같은 결과다.
    pub fn allows_rerun_fallback(&self) -> bool {
        !matches!(self, Self::NotExplainable | Self::NoStatement)
    }

    /// DML 을 `SELECT` 로 변환해 근사 플랜을 얻어야 하는가.
    pub fn requires_select_conversion(&self) -> bool {
        *self == Self::DmlPrivilegeMissing
    }

    /// 자가진단 실패로 승격해야 하는가.
    ///
    /// **`Denied` 는 승격하지 않는다.** RDS 에서는 정상이며 사용자가 고칠 수 없다.
    /// 초기 설계는 이걸 "`PROCESS` 미부여"로 해석해 승격시켰는데, 그러면 모든 RDS
    /// 인스턴스가 상시 자가진단 실패로 표시된다.
    pub fn escalates_to_diagnostic(&self) -> bool {
        false
    }

    /// 연결을 폐기해야 하는가 (취소해도 서버 측 작업이 남을 수 있다).
    pub fn requires_connection_drop(&self) -> bool {
        *self == Self::Timeout
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainOutcome {
    Plan(String),
    Failed(PlanFailure),
}

/// 수집 제외 규칙 중 **SQL 로 밀어넣을 수 있는 것** (M4-13).
///
/// 정규식 제외는 SQL 로 표현하지 않고 수집기에서 적용한다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Excludes {
    pub schemas: Vec<String>,
    pub users: Vec<String>,
}

impl Excludes {
    /// 시스템 스키마 + 우리 계정을 기본으로 제외한다.
    ///
    /// **자기 제외가 실패하면** 1초 주기 `detect` 쿼리가 상위 N 후보·`_other`·
    /// 커버리지·계정 롤업을 전부 오염시킨다 ([05 §10](../../../docs/05-collector.md)).
    pub fn with_defaults(monitor_user: &str) -> Self {
        Self {
            schemas: ["mysql", "sys", "performance_schema", "information_schema"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            users: vec![monitor_user.to_string()],
        }
    }
}

#[async_trait]
pub trait TargetDb: Send + Sync {
    /// 1초 주기 경량 탐지. **정상 상태에서 tick 당 이 쿼리 1건만 나간다.**
    async fn probe(&self, threshold_secs: u32, excludes: &Excludes) -> Result<ProbeResult>;

    /// 임계값 초과 스레드의 전문 SQL.
    async fn full_sql(&self, ids: &[u64]) -> Result<Vec<FullSqlRow>>;

    /// 임계값 초과 스레드의 정확 지표 + 다이제스트.
    async fn stmt_current(&self, ids: &[u64]) -> Result<Vec<StmtCurrentRow>>;

    /// 실행 중 실행계획. **별도 연결에서** 실행해야 폴링이 밀리지 않는다.
    ///
    /// ⚠ RDS 에서는 항상 [`PlanFailure::Denied`] 다 — 타인 커넥션 explain 은 정적 전역
    /// 권한 전체를 요구한다([19 §B](../../../docs/19-m1-findings.md)).
    /// 실질 기본 경로는 [`TargetDb::explain_rerun`] 이다.
    async fn explain_for_connection(&self, connection_id: u64) -> Result<ExplainOutcome>;

    /// 문장을 `EXPLAIN FORMAT=JSON` 으로 재실행한다. **`SELECT` 권한만으로 동작한다.**
    ///
    /// 호출자는 `dbmon_normalize::plan_query()` 로 만든 문장을 넘긴다 — DML 이면
    /// 조건절이 `SELECT` 로 변환돼 있다. `EXPLAIN` 은 문장을 실행하지 않는다.
    async fn explain_rerun(&self, sql: &str) -> Result<ExplainOutcome>;

    /// `FORMAT=TREE` 텍스트. 사람이 읽기 쉬워 UI 가치가 크다. 미지원이면 `None`.
    async fn explain_tree(&self, sql: &str) -> Result<Option<String>>;

    /// 다이제스트 스냅샷 (지표 컬럼만). `last_seen_gte_ms` 로 활성 다이제스트만 받는다.
    async fn digest_snapshot(&self, last_seen_gte_ms: Option<EpochMs>) -> Result<DigestSnapshot>;

    /// 캐시에 없는 다이제스트의 텍스트만 2차 조회.
    async fn digest_texts(&self, digests: &[String]) -> Result<Vec<DigestTextRow>>;

    /// `performance_schema.global_status` — 실시간 지표.
    async fn global_status(&self) -> Result<BTreeMap<String, String>>;

    /// `SELECT STATEMENT_DIGEST(?)` — 우리 문장의 다이제스트를 계산해 자기 제외에 쓴다.
    async fn statement_digest(&self, sql: &str) -> Result<Option<String>>;

    /// 대상 DB 의 현재 시각. 시계 오프셋 추정용 (F14).
    ///
    /// `probe` 가 0행을 반환하면 시각을 못 얻으므로, 낮은 빈도(기본 30초)로 이걸 호출한다.
    async fn db_now_ms(&self) -> Result<EpochMs>;

    async fn ping(&self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_failure_classified_by_code_not_message() {
        assert_eq!(
            PlanFailure::from_mysql_error_code(1094),
            PlanFailure::ThreadGone
        );
        assert_eq!(
            PlanFailure::from_mysql_error_code(3012),
            PlanFailure::NotExplainable
        );
        for c in [1044, 1045, 1227] {
            assert_eq!(PlanFailure::from_mysql_error_code(c), PlanFailure::Denied);
        }
        assert_eq!(
            PlanFailure::from_mysql_error_code(9999),
            PlanFailure::Other(9999)
        );
    }

    #[test]
    fn fallback_policy_per_failure() {
        assert!(PlanFailure::ThreadGone.allows_rerun_fallback());
        assert!(PlanFailure::Timeout.allows_rerun_fallback());
        assert!(
            !PlanFailure::NotExplainable.allows_rerun_fallback(),
            "재실행해도 같은 결과다"
        );
        assert!(
            PlanFailure::Denied.allows_rerun_fallback(),
            "타인 커넥션 explain 권한이 없어도 우리 커넥션에서 재실행하는 것은 된다 (M1-4b)"
        );
        assert!(
            !PlanFailure::NoStatement.allows_rerun_fallback(),
            "실행 중 문장이 없었다"
        );

        assert!(PlanFailure::DmlPrivilegeMissing.requires_select_conversion());
        assert!(!PlanFailure::Denied.requires_select_conversion());

        assert!(PlanFailure::Timeout.requires_connection_drop());
        assert!(!PlanFailure::ThreadGone.requires_connection_drop());
    }

    /// RDS 에서는 `Denied` 가 상시 발생한다. 승격하면 모든 인스턴스가 자가진단 실패가 된다.
    #[test]
    fn denied_does_not_escalate_to_diagnostic_failure() {
        for f in [
            PlanFailure::Denied,
            PlanFailure::DmlPrivilegeMissing,
            PlanFailure::ThreadGone,
            PlanFailure::NotExplainable,
            PlanFailure::NoStatement,
            PlanFailure::Timeout,
        ] {
            assert!(!f.escalates_to_diagnostic(), "{f:?}");
        }
    }

    #[test]
    fn dml_privilege_error_is_classified() {
        assert_eq!(
            PlanFailure::from_mysql_error_code(1142),
            PlanFailure::DmlPrivilegeMissing
        );
    }

    #[test]
    fn plan_error_strings_are_stable() {
        assert_eq!(PlanFailure::ThreadGone.as_str(), "thread_gone");
        assert_eq!(PlanFailure::Other(1317).as_str(), "other:1317");
    }

    #[test]
    fn default_excludes_cover_system_schemas_and_self() {
        let e = Excludes::with_defaults("dbmon");
        assert!(e.schemas.contains(&"performance_schema".to_string()));
        assert!(
            e.users.contains(&"dbmon".to_string()),
            "자기 제외가 빠지면 통계가 오염된다"
        );
    }

    #[test]
    fn nested_statement_detected() {
        let mut r = StmtCurrentRow::default();
        assert!(!r.is_nested_statement());
        r.nesting_event_type = Some("STATEMENT".into());
        assert!(r.is_nested_statement());
        r.nesting_event_type = Some("TRANSACTION".into());
        assert!(!r.is_nested_statement());
    }
}
