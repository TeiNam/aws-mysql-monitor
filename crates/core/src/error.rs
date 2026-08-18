//! 에러 체계 (M0-7). 안정적인 `code` 문자열과 HTTP 매핑을 함께 가진다.
//!
//! `code` 는 **API 계약의 일부**다. 프론트엔드가 분기하므로 이름을 바꾸면 파괴적 변경이다
//! ([13 §1.2](../../../docs/13-api-spec.md)).

use std::fmt;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DomainError {
    #[error("{kind}을(를) 찾을 수 없다: {id}")]
    NotFound { kind: &'static str, id: String },

    #[error("입력이 올바르지 않다 ({field}): {reason}")]
    InvalidInput { field: String, reason: String },

    #[error("상태가 충돌한다: {0}")]
    Conflict(String),

    #[error("인증이 필요하다")]
    Unauthenticated,

    #[error("권한이 없다: {action}")]
    Forbidden { action: String },

    #[error("요청이 너무 많다. {retry_after_ms}ms 후 재시도")]
    RateLimited { retry_after_ms: u64 },

    #[error("의존 서비스를 쓸 수 없다 ({dependency}): {reason}")]
    Unavailable {
        dependency: &'static str,
        reason: String,
    },

    #[error("예산을 초과했다: {what}")]
    BudgetExceeded { what: String },

    #[error("지원하지 않는다 ({what}): {reason}")]
    Unsupported { what: String, reason: String },

    /// **재시도가 무의미한 권한·암호화 거부** (F25).
    ///
    /// 스로틀로 오분류하면 지수 백오프 → 버퍼 초과 → **드롭 경로**를 탄다.
    /// 이 변형은 버퍼를 유지하고 사람이 고칠 때까지 기다리게 하는 신호다.
    #[error("접근이 거부됐다 ({service}): {reason} — 재시도해도 해결되지 않는다")]
    AccessDenied {
        service: &'static str,
        reason: String,
    },

    #[error("내부 오류: {0}")]
    Internal(String),
}

impl DomainError {
    /// 프론트엔드가 분기하는 안정 코드.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::InvalidInput { .. } => "invalid_input",
            Self::Conflict(_) => "conflict",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden { .. } => "forbidden",
            Self::RateLimited { .. } => "rate_limited",
            Self::Unavailable { .. } => "unavailable",
            Self::BudgetExceeded { .. } => "budget_exceeded",
            Self::Unsupported { .. } => "unsupported",
            Self::AccessDenied { .. } => "access_denied",
            Self::Internal(_) => "internal",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidInput { .. } => 400,
            Self::Unauthenticated => 401,
            // `access_denied` 는 **우리 IAM 문제**이므로 클라이언트에게 403 이 아니라 503 이다.
            // 사용자가 고칠 수 있는 것이 없다.
            Self::Forbidden { .. } => 403,
            Self::NotFound { .. } => 404,
            Self::Conflict(_) => 409,
            Self::BudgetExceeded { .. } => 402,
            Self::Unsupported { .. } => 422,
            Self::RateLimited { .. } => 429,
            Self::Unavailable { .. } | Self::AccessDenied { .. } => 503,
            Self::Internal(_) => 500,
        }
    }

    /// 재시도가 의미 있는가. 수집 루프의 백오프 판단에 쓴다.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Unavailable { .. } | Self::RateLimited { .. } | Self::Internal(_) => true,
            // 권한·암호화 거부는 재시도해도 같은 결과다. 버퍼를 유지하고 알림을 띄운다.
            Self::AccessDenied { .. }
            | Self::Forbidden { .. }
            | Self::Unauthenticated
            | Self::NotFound { .. }
            | Self::InvalidInput { .. }
            | Self::Conflict(_)
            | Self::BudgetExceeded { .. }
            | Self::Unsupported { .. } => false,
        }
    }

    /// 버퍼에 담긴 데이터를 **버려도 되는가**.
    ///
    /// `false` 면 사람이 고칠 때까지 버퍼를 유지한다 (F25).
    pub fn may_drop_buffer(&self) -> bool {
        !matches!(self, Self::AccessDenied { .. })
    }

    /// 사용자에게 그대로 보여도 되는 메시지인가.
    ///
    /// `Internal` 은 내부 문맥을 담을 수 있어 그대로 노출하면 정보 유출이다
    /// (NFR-S: "에러 메시지가 민감 정보를 흘리지 않는다").
    pub fn is_safe_to_expose(&self) -> bool {
        !matches!(self, Self::Internal(_))
    }

    /// API 응답 본문에 담을 메시지.
    pub fn public_message(&self) -> String {
        if self.is_safe_to_expose() {
            self.to_string()
        } else {
            "내부 오류가 발생했다. 잠시 후 다시 시도해 주세요.".to_string()
        }
    }
}

/// AWS SDK 오류 코드를 도메인 오류로 분류한다 (F25).
///
/// **문자열 접두 매칭이다.** SDK 버전이 코드 이름을 바꿀 수 있으므로,
/// 분류되지 않은 코드는 `Unavailable`(재시도 가능)로 떨어진다. 그건 안전한 기본값이
/// **아니다** — `AccessDenied` 계열을 놓치면 드롭 경로를 탄다. 그래서 목록을 넓게 잡고
/// 미분류 코드는 로그에 남겨 목록에 추가한다.
pub fn classify_aws_error(
    code: &str,
    service: &'static str,
    reason: impl Into<String>,
) -> DomainError {
    const DENY_PREFIXES: &[&str] = &[
        "AccessDenied",
        "KMSAccessDenied",
        "KMSInvalidState",
        "KMSNotFound",
        "KMSDisabled",
        "UnrecognizedClient",
        "InvalidSignature",
        "InvalidClientTokenId",
        "ExpiredToken",
        "AuthFailure",
        "MissingAuthenticationToken",
    ];
    const THROTTLE_PREFIXES: &[&str] = &[
        "Throttling",
        "ThrottledException",
        "TooManyRequests",
        "ProvisionedThroughputExceeded",
        "RequestLimitExceeded",
        "SlowDown",
        "LimitExceeded",
    ];
    let reason = reason.into();
    if DENY_PREFIXES.iter().any(|p| code.starts_with(p)) {
        return DomainError::AccessDenied { service, reason };
    }
    if THROTTLE_PREFIXES.iter().any(|p| code.starts_with(p)) {
        return DomainError::RateLimited {
            retry_after_ms: 1_000,
        };
    }
    DomainError::Unavailable {
        dependency: service,
        reason,
    }
}

pub type Result<T> = std::result::Result<T, DomainError>;

/// 값 변환 에러를 도메인 에러로.
impl From<crate::ids::IdError> for DomainError {
    fn from(e: crate::ids::IdError) -> Self {
        Self::InvalidInput {
            field: "id".into(),
            reason: e.to_string(),
        }
    }
}

/// `code` 를 그대로 표시할 수 있게.
pub struct ErrorCode<'a>(pub &'a DomainError);

impl fmt::Display for ErrorCode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_statuses_are_stable() {
        let cases: &[(DomainError, &str, u16)] = &[
            (
                DomainError::NotFound {
                    kind: "인스턴스",
                    id: "x".into(),
                },
                "not_found",
                404,
            ),
            (DomainError::Unauthenticated, "unauthenticated", 401),
            (
                DomainError::Forbidden {
                    action: "delete".into(),
                },
                "forbidden",
                403,
            ),
            (
                DomainError::RateLimited { retry_after_ms: 5 },
                "rate_limited",
                429,
            ),
            (
                DomainError::AccessDenied {
                    service: "kms",
                    reason: "denied".into(),
                },
                "access_denied",
                503,
            ),
            (DomainError::Internal("boom".into()), "internal", 500),
        ];
        for (e, code, status) in cases {
            assert_eq!(e.code(), *code);
            assert_eq!(e.http_status(), *status);
        }
    }

    /// F25 — KMS 거부를 스로틀로 오분류하면 데이터가 조용히 버려진다.
    #[test]
    fn f25_kms_denial_is_not_retryable_and_keeps_buffer() {
        let e = classify_aws_error("KMSAccessDeniedException", "dynamodb", "key policy");
        assert!(matches!(e, DomainError::AccessDenied { .. }));
        assert!(!e.is_retryable());
        assert!(!e.may_drop_buffer(), "버퍼를 버리면 데이터가 사라진다");
    }

    #[test]
    fn throttling_is_retryable_and_may_drop() {
        let e = classify_aws_error(
            "ProvisionedThroughputExceededException",
            "dynamodb",
            "hot key",
        );
        assert!(matches!(e, DomainError::RateLimited { .. }));
        assert!(e.is_retryable());
        assert!(e.may_drop_buffer());
    }

    #[test]
    fn unknown_aws_code_falls_back_to_unavailable() {
        let e = classify_aws_error("SomeBrandNewError", "athena", "?");
        assert!(matches!(e, DomainError::Unavailable { .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn all_denial_variants_are_classified() {
        for code in [
            "AccessDeniedException",
            "KMSAccessDeniedException",
            "KMSInvalidStateException",
            "KMSNotFoundException",
            "UnrecognizedClientException",
            "InvalidSignatureException",
            "ExpiredTokenException",
        ] {
            let e = classify_aws_error(code, "s3", "x");
            assert!(
                matches!(e, DomainError::AccessDenied { .. }),
                "{code} 가 분류되지 않았다"
            );
        }
    }

    #[test]
    fn internal_errors_are_not_exposed() {
        let e = DomainError::Internal("connection string: user:pw@host".into());
        assert!(!e.is_safe_to_expose());
        let msg = e.public_message();
        assert!(!msg.contains("pw@host"), "내부 문맥이 유출됐다: {msg}");

        let ok = DomainError::NotFound {
            kind: "다이제스트",
            id: "abc".into(),
        };
        assert!(ok.public_message().contains("abc"));
    }

    #[test]
    fn id_error_converts_to_invalid_input() {
        let e: DomainError = crate::ids::IdError::Account("1".into()).into();
        assert_eq!(e.code(), "invalid_input");
        assert_eq!(e.http_status(), 400);
    }
}
