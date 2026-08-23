//! AWS 어댑터.
//!
//! **위험한 판정은 순수 함수로 분리한다.** AWS 호출은 로컬에서 검증할 수 없지만
//! (SSO 만료 시 아예 못 돈다) 판정은 전수 검증할 수 있다 — 그리고 prd 인스턴스를
//! 하나라도 통과시키면 그걸로 끝이므로 위험은 호출이 아니라 판정에 있다.

/// 크로스 계정 `sts:AssumeRole` 에 보내는 `ExternalId`.
///
/// # 왜 상수인가
///
/// `ExternalId` 는 비밀이 아니다 — **혼동된 대리인**(confused deputy)을 막는 값이다.
/// 대상 계정의 신뢰 정책이 `sts:ExternalId` 를 요구하면, 우리 역할 ARN 을 알아낸
/// 제3자가 자기 계정에서 그 역할을 맡아 보려 해도 이 값을 함께 보내지 못한다.
///
/// 설정 항목으로 만들지 않는 이유: 값이 무엇인지는 중요하지 않고 **양쪽이 같기만**
/// 하면 된다. 설정으로 두면 화면과 신뢰 정책이 어긋나는 경로가 하나 늘고, 그 어긋남은
/// `AccessDenied` 로만 나타난다.
///
/// 신뢰 정책에 조건이 **없어도** 안전하다 — 보낸 `ExternalId` 는 그냥 무시된다.
/// 그래서 항상 보낸다.
///
/// ⚠ 이 값은 문서(`README` §3.6, [23 §3.2](../../../../.claude/docs/23-settings.md))의 신뢰
/// 정책과 **같아야 한다.** 한쪽만 바꾸면 크로스 계정 탐색·메트릭이 전부 실패한다.
/// 교차 리뷰가 잡은 것이 정확히 그 상태였다 — 문서는 요구하는데 코드가 보내지 않았다.
pub const ASSUME_ROLE_EXTERNAL_ID: &str = "dbmon";

pub mod auth_token;
pub mod bedrock;
pub mod cloudwatch;
pub mod cw_fetchers;
pub mod discovery;
pub mod filter;
pub mod fleet;
pub mod rds;
