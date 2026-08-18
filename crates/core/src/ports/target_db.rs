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
    /// 대상 DB 의 `NOW(6)`. 추가 왕복 없이 탐지 쿼리에 컬럼으로 붙인다.
    pub db_now_ms: EpochMs,
    /// `LIMIT` 에 걸려 잘렸다 → `detect_overflow` 메트릭.
    pub truncated: bool,
}

/// `information_schema.PROCESSLIST` 타깃 조회 — 전문 SQL.
///
/// `INFO` 는 `LONGTEXT` 이며 절단되지 않는다(이 전제가 ADR-005 의 근거이며
/// [OPEN-Q-07](../../../docs/OPEN-QUESTIONS.md) 에서 실측 검증한다).
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
    /// 1044 / 1045 / 1227 — `PROCESS` 미부여. 자가진단 실패로 승격.
    Denied,
    /// 클라이언트 타임아웃. 연결을 폐기한다.
    Timeout,
    /// 분류되지 않은 코드. 기록해 두고 나중에 표에 추가한다.
    Other(u16),
}

impl PlanFailure {
    pub fn from_mysql_error_code(code: u16) -> Self {
        match code {
            1094 => Self::ThreadGone,
            3012 => Self::NotExplainable,
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
            Self::Timeout => "timeout".into(),
            Self::Other(c) => format!("other:{c}"),
        }
    }

    /// 사후 재실행 폴백을 시도할 가치가 있는가.
    ///
    /// `NotExplainable` 은 재실행해도 같은 결과다. `Denied` 는 권한 문제이므로 무의미하다.
    pub fn allows_rerun_fallback(&self) -> bool {
        matches!(self, Self::ThreadGone | Self::Timeout | Self::Other(_))
    }

    /// 자가진단 실패로 승격해야 하는가.
    pub fn escalates_to_diagnostic(&self) -> bool {
        *self == Self::Denied
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
    async fn explain_for_connection(&self, connection_id: u64) -> Result<ExplainOutcome>;

    /// 다이제스트 스냅샷 (지표 컬럼만). `last_seen_gte_ms` 로 활성 다이제스트만 받는다.
    async fn digest_snapshot(&self, last_seen_gte_ms: Option<EpochMs>) -> Result<DigestSnapshot>;

    /// 캐시에 없는 다이제스트의 텍스트만 2차 조회.
    async fn digest_texts(&self, digests: &[String]) -> Result<Vec<DigestTextRow>>;

    /// `performance_schema.global_status` — 실시간 지표.
    async fn global_status(&self) -> Result<BTreeMap<String, String>>;

    /// `SELECT STATEMENT_DIGEST(?)` — 우리 문장의 다이제스트를 계산해 자기 제외에 쓴다.
    async fn statement_digest(&self, sql: &str) -> Result<Option<String>>;

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
            !PlanFailure::Denied.allows_rerun_fallback(),
            "권한 문제는 재실행이 무의미하다"
        );
        assert!(PlanFailure::Denied.escalates_to_diagnostic());
        assert!(PlanFailure::Timeout.requires_connection_drop());
        assert!(!PlanFailure::ThreadGone.requires_connection_drop());
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
