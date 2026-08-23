//! 부트스트랩 어댑터 (M3, [07](../../../../.claude/docs/07-credentials-bootstrap.md)).
//!
//! 판정은 [`dbmon_core::bootstrap`] 에 있다. 이 모듈은 **IO 만** 한다:
//!
//! | 모듈 | 하는 일 |
//! |---|---|
//! | [`secret`] | Secrets Manager 에서 마스터 자격증명을 얻는다 (경로 a·b) |
//! | [`mysql`] | 마스터로 붙어 현재 상태를 읽고 문장을 실행한다 |
//! | [`audit`] | 무엇을 했는지 남긴다 (값은 남기지 않는다) |
//! | [`run`] | 위 셋을 엮어 계획·실행을 수행한다 |

pub mod audit;
pub mod mysql;
pub mod run;
pub mod secret;
pub mod service;

pub use run::{ApplyOutcome, ApplyReport, BootstrapError, Bootstrapper, PlanOutcome};
pub use secret::{CredentialSource, MasterCredentials};
pub use service::{AuditSink, BootstrapService};
