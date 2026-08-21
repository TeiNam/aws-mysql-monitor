//! 포트(trait) 정의. **`core` 는 AWS·MySQL·HTTP 를 모른다.**
//!
//! 여기 없는 포트(`Notifier`, `LlmAdvisor`, `MetricSource`)는 해당 마일스톤에서 추가한다.
//! 구현이 없는 trait 를 미리 선언하면 첫 구현 때 시그니처가 바뀌어 순수한 손실이다
//! (YAGNI). 대신 [02 §10](../../../docs/02-architecture.md) 의 확장 지점 표가 자리를 지킨다.

pub mod stores;
pub mod target_db;

pub use stores::{
    COLLECT_LEADER_KEY, CRON_LEADER_KEY, DigestStore, DigestTextEntry, DigestTextSource,
    InstanceRegistry, LEASE_RENEW_INTERVAL_MS, LEASE_TTL_MS, Lease, LeaseStore, PauseStore,
    SHARD_COUNT, SettingsStore, SlowQueryStore, shard_key, shard_of,
};
pub use target_db::{
    DigestSnapshot, DigestTextRow, Excludes, ExplainOutcome, FullSqlRow, PlanFailure, ProbeResult,
    ProcessRow, StmtCurrentRow, TargetDb,
};

use crate::error::Result;
use crate::secret::ExpiringSecret;
use crate::time::EpochMs;
use async_trait::async_trait;

/// 대상 DB 접속용 비밀을 공급한다.
///
/// **이 포트가 존재하는 이유** — 초기 설계는 `dbmon::mysql` 이 "IAM 토큰 발급 연동"을
/// 한다고 적어, MySQL 어댑터가 AWS SDK 를 알아야 한다고 스스로 인정하고 있었다(F19/F31).
/// 이 포트로 분리하면 어댑터는 "비밀번호 문자열을 주는 무언가"만 알고,
/// 그게 IAM 토큰인지 Secrets Manager 값인지 모른다.
#[async_trait]
pub trait AuthTokenProvider: Send + Sync {
    /// 접속 시점에 쓸 비밀을 발급한다. IAM DB Auth 면 15분 유효 토큰이다.
    async fn token(&self, host: &str, port: u16, db_user: &str) -> Result<ExpiringSecret>;
}

/// 아카이브 조회 — **도메인 언어로만** 정의한다.
///
/// 초기 설계는 Athena 실행 모델(`execution_id`, `next_token`, `FOR VERSION AS OF`)을
/// 그대로 노출해 "core 는 아무것도 모른다"가 성립하지 않았다(F31).
/// `QueryHandle` · `Cursor` · `AsOf` 는 **불투명 타입**이고, Athena 의 개념은
/// `dbmon::aws` 안에 갇힌다.
#[async_trait]
pub trait ArchiveQuery: Send + Sync {
    async fn start(&self, spec: ArchiveQuerySpec, at: Option<AsOf>) -> Result<QueryHandle>;
    async fn status(&self, h: &QueryHandle) -> Result<QueryStatus>;
    async fn page(&self, h: &QueryHandle, cursor: Option<Cursor>)
    -> Result<(Rows, Option<Cursor>)>;
    async fn cancel(&self, h: &QueryHandle) -> Result<()>;
}

/// 아카이브 조회 핸들. 내부 표현은 어댑터의 것이다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryHandle(String);

impl QueryHandle {
    /// 어댑터만 생성한다.
    pub fn from_opaque(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_opaque(&self) -> &str {
        &self.0
    }
}

/// 페이지 커서. **HMAC 서명은 API 계층의 책임**이고 여기서는 불투명 값이다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(String);

impl Cursor {
    pub fn from_opaque(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_opaque(&self) -> &str {
        &self.0
    }
}

/// 특정 시점 고정 조회. 리포트 재현성에 쓴다(FR-RPT-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsOf(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryStatus {
    Running {
        elapsed_ms: i64,
        scanned_bytes: Option<u64>,
    },
    Succeeded {
        scanned_bytes: Option<u64>,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}

impl QueryStatus {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running { .. })
    }
}

/// 조회 결과 행. 컬럼 이름 → 값(문자열). 타입 변환은 호출자가 한다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rows {
    pub columns: Vec<String>,
    pub values: Vec<Vec<Option<String>>>,
}

/// 아카이브 조회 명세. **`TimeRange` 가 필수다** — 파티션 프루닝 없이 전체를 스캔하는
/// 경로를 타입으로 막는다 (M7-10, R17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveQuerySpec {
    pub template: ArchiveTemplate,
    pub range: crate::time::TimeRange,
    /// 파라미터. **문자열 결합을 하지 않는다** — 어댑터가 파라미터화한다.
    pub params: Vec<(String, String)>,
    pub limit: usize,
}

/// 허용된 조회만 열거한다. 임의 SQL 을 받는 경로가 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveTemplate {
    SlowQueriesByInstance,
    SlowQueriesByDigest,
    DigestRollupsByInstance,
    DigestRollupsByEnv,
    SlowQueryTextSearch,
    MonthlyAggregate,
    AuditSearch,
}

impl ArchiveTemplate {
    /// 이 템플릿을 실행할 최소 역할. `audit_search` 를 viewer 가 못 쓰게 한다(T-17).
    pub fn min_role(self) -> crate::rbac::Role {
        use crate::rbac::Role;
        match self {
            Self::AuditSearch => Role::Admin,
            Self::MonthlyAggregate => Role::Operator,
            _ => Role::Viewer,
        }
    }
}

/// 마스터 자격증명 소스 ([07 §2](../../../docs/07-credentials-bootstrap.md)).
///
/// **초기 모니터링 계정 생성에만** 쓴다. 상시 보관하지 않는다.
#[async_trait]
pub trait SecretSource: Send + Sync {
    async fn fetch(&self, reference: &SecretRef) -> Result<MasterCredential>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// RDS 관리형 마스터 시크릿 (`MasterUserSecret`).
    RdsManaged { instance: crate::ids::InstanceId },
    /// 사용자 지정 ARN. **태그 조건을 검증해야 한다** — 임의 ARN 을 읽으면 안 된다.
    UserProvided { arn: String },
    /// 수동 입력. 메모리에만 있고 10분 후 만료된다.
    ManualEntry { session_id: String },
}

pub struct MasterCredential {
    pub username: String,
    pub password: crate::secret::SecretString,
    /// 이 자격증명이 무효해지는 시각. 수동 입력은 10분이다.
    pub expires_at_ms: Option<EpochMs>,
}

impl std::fmt::Debug for MasterCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 사용자명도 로그에 남기지 않는다 — 마스터 계정명은 공격 표면이다.
        f.write_str("MasterCredential(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbac::Role;

    #[test]
    fn audit_search_requires_admin() {
        // T-17 — Athena 경로로 RBAC 를 우회하지 못하게 한다.
        assert_eq!(ArchiveTemplate::AuditSearch.min_role(), Role::Admin);
        assert_eq!(ArchiveTemplate::MonthlyAggregate.min_role(), Role::Operator);
        assert_eq!(
            ArchiveTemplate::SlowQueriesByInstance.min_role(),
            Role::Viewer
        );
    }

    #[test]
    fn query_status_terminality() {
        assert!(
            !QueryStatus::Running {
                elapsed_ms: 1,
                scanned_bytes: None
            }
            .is_terminal()
        );
        assert!(
            QueryStatus::Succeeded {
                scanned_bytes: Some(1)
            }
            .is_terminal()
        );
        assert!(QueryStatus::Failed { reason: "x".into() }.is_terminal());
        assert!(QueryStatus::Cancelled.is_terminal());
    }

    #[test]
    fn master_credential_debug_is_redacted() {
        let c = MasterCredential {
            username: "admin".into(),
            password: crate::secret::Secret::new("pw".into()),
            expires_at_ms: None,
        };
        let s = format!("{c:?}");
        assert!(!s.contains("admin"));
        assert!(!s.contains("pw"));
    }
}
