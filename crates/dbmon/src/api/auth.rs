//! API 인증 (M5, [08 §1](../../../../docs/08-security-auth.md) T-01).
//!
//! # T-01 — `/healthz` 외 전 엔드포인트는 인증이 필요하다
//!
//! 이 API 는 운영 SQL 을 반환한다. 리터럴 정책이 `full`/`full_restricted` 면
//! **개인정보가 실린 문장 원문**이 나온다. 인증 없이 열면 그게 곧 유출이다.
//!
//! ## 인증 없이 답하는 엔드포인트 — 전부 여기 적는다
//!
//! T-01 은 `/healthz` 하나만 예외로 적었지만 실제로는 셋이다. 예외를 코드 여러
//! 곳에 흩어 두면 목록을 잃어버리고, 잃어버린 예외가 곧 구멍이다.
//!
//! | 경로 | 인증 없이 답하는 이유 |
//! |---|---|
//! | `/healthz` | 컨테이너 라이브니스. 데이터를 담지 않는다 |
//! | `/readyz` | ALB 타깃 헬스체크. 준비 사유 문자열만 담는다 |
//! | `/api/auth/config` | **로그인하기 전에** 로그인 방법을 알아야 한다. 담는 값은 전부 공개값(issuer·client_id·도메인)이다 |
//!
//! 임베드 화면(`/`)은 예외가 아니다 — `allows_local_bypass()` 가 참일 때만
//! 라우트가 붙는다(`main.rs`). prd 에서는 404 다.
//!
//! # 로컬 개발 예외 — 설정 항목을 만들지 않는다
//!
//! Cognito 를 로컬에 세울 수 없다. 그렇다고 "인증 끄기" 설정을 만들면
//! **언젠가 프로덕션에서 켜진다** — 이 프로젝트가 TLS·DynamoDB 엔드포인트에서
//! 반복해 온 판단이다. 대신 **위조할 수 없는 두 속성의 곱**에서 유도한다:
//!
//! ```text
//! 인증 우회 = (deployment_env == dev)  AND  (bind 주소가 루프백)
//! ```
//!
//! 둘 다 필요하다. prd 배포는 첫 조건에서 걸리고, `0.0.0.0` 으로 열면 두 번째에서
//! 걸린다 — 즉 **다른 호스트에서 접근할 수 있는 순간 우회가 꺼진다.**
//!
//! # 컨테이너는 루프백에 바인드할 수 없다 — 그래서 토큰을 발급한다
//!
//! 위 규칙은 컨테이너에서 **화면을 못 쓰게 만든다.** `docker run -p 8080:8080` 으로
//! 호스트에서 닿으려면 컨테이너 안에서 `0.0.0.0` 에 바인드해야 하고, 그러면 두
//! 번째 조건이 깨진다. 컨테이너 안의 프로세스는 호스트가 포트를 루프백에만
//! 공개했는지 알 수 없다 — 그건 네임스페이스 밖의 사실이다.
//!
//! 그래서 그 경우에는 우회를 넓히지 않고 **실제 자격증명을 하나 만든다.**
//! 기동 시 무작위 토큰을 발급해 로그에 URL 로 찍는다(`docker logs` 로 본다).
//! 우회가 아니라 인증이므로 바인드 주소가 무엇이든 구멍이 없다 — 토큰을 가진
//! 쪽만 들어온다.
//!
//! ```text
//! 토큰 발급 = (deployment_env == dev) AND (bind 이 루프백이 아님) AND (ECS 가 아님)
//! ```
//!
//! 세 번째 조건이 있는 이유: `deployment_env=dev` 인 ECS 배포가 공개 ALB 뒤에
//! 있으면 토큰이 CloudWatch Logs 에 남는다. ECS 는 태스크마다
//! `ECS_CONTAINER_METADATA_URI_V4` 를 주입하므로 **그 부재**로 판정한다 —
//! 위험한 방향(프로덕션에서 켜짐)을 막으려면 부재를 조건으로 두는 것이 맞다.
//!
//! # Cognito 검증은 아직 없다 — 그리고 조용히 통과시키지 않는다
//!
//! JWT 검증(JWKS 조회·kid 캐시·클레임 교집합)은 M5 의 남은 작업이다. 그 전까지
//! 비-dev 배포는 API 요청을 **거부한다.** 스텁으로 통과시키면 인증이 있는 것처럼
//! 보이면서 없는 상태가 되고, 그게 가장 나쁘다.

use std::sync::Arc;

use axum::http::StatusCode;
use dbmon_core::env::Env;
use dbmon_core::rbac::{AuthContext, Role};

/// 인증 판정에 필요한 배포 사실.
///
/// `Copy` 가 아니다 — 토큰을 담으므로 참조로 넘긴다. 값 복사가 안 되는 편이
/// 낫다: 비밀을 실수로 로그에 흘릴 경로가 줄어든다(`Debug` 도 아래에서 가린다).
#[derive(Clone, PartialEq, Eq)]
pub struct AuthPolicy {
    pub deployment_env: Env,
    /// HTTP 서버가 **루프백에만** 바인드돼 있는가.
    pub bind_is_loopback: bool,
    /// 로컬 개발용으로 발급된 토큰. [`mint_dev_token`] 만 채운다.
    pub dev_token: Option<Arc<str>>,
}

/// **토큰을 절대 찍지 않는다.** `Opts` 가 파생 `Debug` 로 비밀번호를 평문
/// 출력했던 적이 있다 — 같은 실수를 반복하지 않는다.
impl std::fmt::Debug for AuthPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPolicy")
            .field("deployment_env", &self.deployment_env)
            .field("bind_is_loopback", &self.bind_is_loopback)
            .field("dev_token", &self.dev_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl AuthPolicy {
    /// 로컬 개발 우회가 허용되는가. **두 조건이 모두 참일 때만.**
    pub fn allows_local_bypass(&self) -> bool {
        self.deployment_env == Env::Dev && self.bind_is_loopback
    }

    /// 임베드 화면을 서빙해도 되는가.
    ///
    /// 우회가 되거나(루프백 `cargo run`) 로컬 토큰이 있을 때(dev 컨테이너).
    /// prd 에서는 둘 다 거짓이므로 `/` 가 404 다.
    pub fn serves_local_ui(&self) -> bool {
        self.allows_local_bypass() || self.dev_token.is_some()
    }
}

/// 로컬 개발 토큰을 발급한다. **세 조건이 모두 맞을 때만 `Some`.**
///
/// `on_ecs` 는 호출부가 `ECS_CONTAINER_METADATA_URI_V4` 존재로 판정해 넘긴다 —
/// 환경변수 읽기를 함수 안에 두면 테스트가 프로세스 환경을 건드려야 한다.
pub fn mint_dev_token(
    deployment_env: Env,
    bind_is_loopback: bool,
    on_ecs: bool,
    entropy: impl FnOnce() -> Option<[u8; 16]>,
) -> Option<Arc<str>> {
    if deployment_env != Env::Dev || bind_is_loopback || on_ecs {
        return None;
    }
    // 엔트로피를 못 읽으면 **약한 토큰을 만들지 않는다.** 화면을 포기한다.
    let bytes = entropy()?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(Arc::from(hex.as_str()))
}

/// 상수 시간 비교. 비밀을 비교하므로 조기 반환하지 않는다.
fn secrets_match(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 바인드 주소가 루프백인지 판정한다.
///
/// **접두 비교로 판정하지 않는다** — `config.rs` 에서 그 구멍을 만들었다가
/// 테스트가 잡았다(`127.0.0.1.attacker.example`). 주소를 파싱한다.
pub fn is_loopback_bind(bind: &str) -> bool {
    if bind.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let trimmed = bind
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(bind);
    trimmed
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// 인증 실패. 본문은 사유를 최소로만 담는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 토큰이 없다.
    Missing,
    /// 토큰이 왔지만 맞지 않는다.
    Invalid,
    /// 이 배포에서는 인증 수단이 구성되지 않았다 (Cognito 미배선).
    NotConfigured,
}

impl AuthError {
    pub fn status(&self) -> StatusCode {
        match self {
            // **`501` 이 아니라 `401` 을 준다.** 구성 상태를 외부에 알리지 않는다.
            Self::Missing | Self::Invalid | Self::NotConfigured => StatusCode::UNAUTHORIZED,
        }
    }
}

/// 로컬 개발용 문맥. **`dev` + 루프백에서만 만들어진다.**
///
/// 역할은 `admin`, 환경 스코프는 전체, 리터럴 열람 허용이다 — 로컬 DynamoDB 에는
/// 개인정보가 없고, 제한을 걸면 화면을 개발할 수 없다.
fn local_dev_context() -> AuthContext {
    AuthContext {
        subject: "local-dev".into(),
        role: Role::Admin,
        env_scope: Env::ALL.to_vec(),
        can_see_literals: true,
        claims_version: 0,
    }
}

/// Cognito 토큰 **검증기가 배선돼 있는가.**
///
/// `false` 인 동안 설정에서 `cognito` 를 골라도 적용되지 않는다
/// ([`AppSettings`](dbmon_core::settings::AppSettings) 는 저장하되
/// `effective_mode` 가 토큰 방식으로 떨어뜨린다). 검증 없이 통과시키는 것보다 낫고,
/// 화면이 이 값을 받아 "아직 준비되지 않았다" 를 말한다.
///
/// M5 에서 JWKS 조회 → 서명 검증 → `AuthContext::intersect` 가 들어오면 `true` 다.
pub const COGNITO_READY: bool = false;

/// **인증을 끈 배포**의 문맥 (`settings.auth.mode = off`).
///
/// # 왜 admin 인가
///
/// 인증이 없으면 주체를 알 수 없고, 주체를 모르면 역할을 나눌 근거가 없다.
/// viewer 로 떨어뜨리면 "권한이 낮아서 안 된다" 는 화면을 보여주면서 정작 아무나
/// 들어와 있는 상태 — 통제하는 것처럼 보이지만 통제하지 않는다. 그래서 역할 놀이를
/// 하지 않고, 대신 **화면과 기동 로그가 인증 없음을 크게 알린다.**
///
/// 이 문맥이 만들어지는 조건은 두 곳의 명시적 허용이다: 파일 설정
/// (`api.allow_auth_disable`) 과 운영 설정(`auth.mode = off`). 기본값은 둘 다 아니다.
pub fn no_auth_context() -> AuthContext {
    AuthContext {
        // `local-dev` 와 구분한다 — 감사 로그에서 "루프백 개발" 과 "인증을 끈 배포" 는
        // 전혀 다른 사실이다.
        subject: "anonymous".into(),
        role: Role::Admin,
        env_scope: Env::ALL.to_vec(),
        can_see_literals: true,
        claims_version: 0,
    }
}

/// 요청의 인증 문맥을 만든다.
///
/// `bearer` 는 `Authorization: Bearer …` 의 토큰 부분이다.
pub fn authenticate(policy: &AuthPolicy, bearer: Option<&str>) -> Result<AuthContext, AuthError> {
    // **우회는 토큰이 없을 때만 적용한다.** 토큰이 왔으면 검증해야 한다 —
    // 우회가 토큰 검증을 덮으면 로컬에서 검증 경로를 한 번도 돌리지 못한다.
    if bearer.is_none() && policy.allows_local_bypass() {
        return Ok(local_dev_context());
    }
    let Some(token) = bearer else {
        return Err(AuthError::Missing);
    };

    // 로컬 개발 토큰. `mint_dev_token` 이 dev 에서만 채우므로 prd 에서는
    // `dev_token` 이 `None` 이고 이 분기가 죽어 있다.
    if let Some(expected) = policy.dev_token.as_deref() {
        return if secrets_match(token.as_bytes(), expected.as_bytes()) {
            Ok(local_dev_context())
        } else {
            Err(AuthError::Invalid)
        };
    }

    // M5 의 남은 작업: JWKS 조회 → 서명 검증 → `TokenClaims` →
    // `AuthContext::intersect(claims, user_record)`.
    // 그 전까지 **통과시키지 않는다.**
    Err(AuthError::NotConfigured)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTROPY: fn() -> Option<[u8; 16]> = || Some([0xab; 16]);

    fn dev_loopback() -> AuthPolicy {
        AuthPolicy {
            deployment_env: Env::Dev,
            bind_is_loopback: true,
            dev_token: None,
        }
    }

    /// **로컬 개발은 토큰 없이 된다** — 아니면 화면을 만들 수 없다.
    #[test]
    fn dev_on_loopback_gets_a_context_without_a_token() {
        let ctx = authenticate(&dev_loopback(), None).expect("우회");
        assert_eq!(ctx.role, Role::Admin);
        assert!(ctx.can_see_literals);
    }

    /// **두 조건이 모두 필요하다.**
    ///
    /// `deployment_env` 만 보면 dev 배포가 `0.0.0.0` 으로 열렸을 때 인증 없이
    /// 외부에 노출된다. 바인드만 보면 prd 가 루프백 프록시로 우회된다.
    #[test]
    fn the_bypass_needs_both_dev_and_loopback() {
        // dev 인데 외부 바인드 → 우회 없음
        let exposed = AuthPolicy {
            deployment_env: Env::Dev,
            bind_is_loopback: false,
            dev_token: None,
        };
        assert_eq!(authenticate(&exposed, None), Err(AuthError::Missing));

        // 루프백인데 dev 아님 → 우회 없음
        for env in [Env::Prd, Env::Stg, Env::Unknown] {
            let p = AuthPolicy {
                deployment_env: env,
                bind_is_loopback: true,
                dev_token: None,
            };
            assert_eq!(
                authenticate(&p, None),
                Err(AuthError::Missing),
                "{env} 에서 인증이 우회됐다"
            );
        }
    }

    /// **토큰이 오면 우회하지 않는다.**
    ///
    /// 우회가 검증을 덮으면 로컬에서 토큰 경로를 한 번도 실행하지 못하고,
    /// Cognito 를 배선한 뒤에도 그 사실을 모른다.
    #[test]
    fn a_present_token_is_never_bypassed() {
        assert_eq!(
            authenticate(&dev_loopback(), Some("some.jwt.token")),
            Err(AuthError::NotConfigured)
        );
    }

    /// **Cognito 미배선 상태에서 비-dev 는 거부한다.**
    ///
    /// 스텁으로 통과시키면 인증이 있는 것처럼 보이면서 없는 상태가 된다.
    #[test]
    fn production_refuses_until_cognito_is_wired() {
        let p = AuthPolicy {
            deployment_env: Env::Prd,
            bind_is_loopback: false,
            dev_token: None,
        };
        assert!(authenticate(&p, None).is_err());
        assert!(authenticate(&p, Some("jwt")).is_err());
        // 상태 코드는 구성 상태를 노출하지 않는다.
        assert_eq!(AuthError::NotConfigured.status(), StatusCode::UNAUTHORIZED);
    }

    // ── 컨테이너용 로컬 토큰 ──────────────────────────────────────────────────

    /// **prd 에는 토큰이 발급되지 않는다.** 이게 깨지면 프로덕션에 로컬
    /// 자격증명이 생긴다 — 가장 막아야 하는 실패다.
    #[test]
    fn no_token_is_minted_outside_dev() {
        for env in [Env::Prd, Env::Stg, Env::Unknown] {
            assert!(
                mint_dev_token(env, false, false, ENTROPY).is_none(),
                "{env} 에 로컬 토큰이 발급됐다"
            );
        }
    }

    /// **ECS 에서는 발급하지 않는다.** dev 배포라도 공개 ALB 뒤에 있을 수 있고,
    /// 그러면 토큰이 CloudWatch Logs 에 남는다.
    #[test]
    fn no_token_is_minted_on_ecs() {
        assert!(mint_dev_token(Env::Dev, false, true, ENTROPY).is_none());
    }

    /// 루프백이면 우회로 충분하므로 발급하지 않는다 — 비밀을 늘리지 않는다.
    #[test]
    fn no_token_is_minted_when_the_bypass_already_applies() {
        assert!(mint_dev_token(Env::Dev, true, false, ENTROPY).is_none());
    }

    /// dev 컨테이너(비루프백·비ECS)에서만 발급된다.
    #[test]
    fn a_token_is_minted_for_a_dev_container() {
        let t = mint_dev_token(Env::Dev, false, false, ENTROPY).expect("발급");
        assert_eq!(t.len(), 32, "128비트를 16진수로");
    }

    /// **엔트로피를 못 읽으면 약한 토큰을 만들지 않는다.**
    ///
    /// 예측 가능한 토큰은 인증이 없는 것보다 나쁘다 — 있다고 착각하게 만든다.
    #[test]
    fn a_token_is_not_minted_without_entropy() {
        assert!(mint_dev_token(Env::Dev, false, false, || None).is_none());
    }

    /// 발급된 토큰으로 들어오고, **틀린 토큰은 막힌다.**
    #[test]
    fn the_dev_token_authenticates_and_a_wrong_one_does_not() {
        let token = mint_dev_token(Env::Dev, false, false, ENTROPY).expect("발급");
        let p = AuthPolicy {
            deployment_env: Env::Dev,
            bind_is_loopback: false,
            dev_token: Some(Arc::clone(&token)),
        };

        let ctx = authenticate(&p, Some(&token)).expect("토큰 인증");
        assert_eq!(ctx.role, Role::Admin);

        // 토큰이 없으면 여전히 막힌다 — 발급이 우회를 켜지 않는다.
        assert_eq!(authenticate(&p, None), Err(AuthError::Missing));
        for wrong in ["", "0", &token[..31], &format!("{token}x"), &"a".repeat(32)] {
            assert_eq!(
                authenticate(&p, Some(wrong)),
                Err(AuthError::Invalid),
                "{wrong:?} 가 통과했다"
            );
        }
    }

    /// 화면은 우회가 되거나 토큰이 있을 때만 서빙된다.
    #[test]
    fn the_ui_is_served_only_locally() {
        assert!(dev_loopback().serves_local_ui());
        assert!(
            AuthPolicy {
                deployment_env: Env::Dev,
                bind_is_loopback: false,
                dev_token: Some(Arc::from("t")),
            }
            .serves_local_ui()
        );
        assert!(
            !AuthPolicy {
                deployment_env: Env::Prd,
                bind_is_loopback: false,
                dev_token: None,
            }
            .serves_local_ui()
        );
    }

    /// **`Debug` 가 토큰을 찍지 않는다.** `Opts` 가 비밀번호를 평문 출력했던
    /// 실수를 반복하지 않는다 — 정책은 기동 로그에 실릴 수 있다.
    #[test]
    fn debug_does_not_print_the_token() {
        let p = AuthPolicy {
            deployment_env: Env::Dev,
            bind_is_loopback: false,
            dev_token: Some(Arc::from("s3cr3t-token-value")),
        };
        let s = format!("{p:?}");
        assert!(!s.contains("s3cr3t"), "토큰이 Debug 에 찍혔다: {s}");
        assert!(s.contains("redacted"));
    }

    /// 상수 시간 비교가 **정확하기도** 해야 한다.
    #[test]
    fn secret_comparison_is_correct() {
        assert!(secrets_match(b"abc", b"abc"));
        assert!(secrets_match(b"", b""));
        assert!(!secrets_match(b"abc", b"abd"));
        assert!(!secrets_match(b"abc", b"ab"));
        assert!(!secrets_match(b"", b"a"));
    }

    /// 루프백 판정에 접두 비교 구멍이 없어야 한다.
    #[test]
    fn loopback_lookalikes_are_not_loopback() {
        for bind in [
            "127.0.0.1.attacker.example",
            "localhost.attacker.example",
            "0.0.0.0",
            "10.0.0.5",
            "",
        ] {
            assert!(!is_loopback_bind(bind), "{bind} 를 루프백으로 봤다");
        }
        for bind in ["127.0.0.1", "localhost", "LOCALHOST", "::1", "[::1]"] {
            assert!(is_loopback_bind(bind), "{bind} 를 루프백으로 못 봤다");
        }
    }
}
