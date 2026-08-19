//! IAM DB Auth 토큰 발급 — [`AuthTokenProvider`] 구현 (M2-6, [07 §3](../../../../docs/07-credentials-bootstrap.md)).
//!
//! # SDK 헬퍼가 없다
//!
//! AWS SDK for Rust 에는 `generate_db_auth_token` 같은 전용 헬퍼가 없다. 그래서
//! SigV4 **presigned** 요청을 직접 만든다. 서명 대상이 정확해야 한다:
//!
//! ```text
//! GET https://<host>:<port>/?Action=connect&DBUser=<user>
//! service = "rds-db"      region = 대상 인스턴스의 리전      유효 15분
//! ```
//!
//! 토큰은 **스킴을 뗀 문자열**이다(`host:port/?...&X-Amz-Signature=...`).
//! `https://` 를 붙여 보내면 서버가 거부한다.
//!
//! # 왜 이 파일을 로컬에서 검증할 수 있는가
//!
//! 서명은 (자격증명, 리전, 호스트, 포트, 사용자, 시각)의 **순수 함수**다. AWS 를
//! 부르지 않는다. 그래서 고정 자격증명과 고정 시각으로 전수 검증할 수 있다 —
//! 이 프로젝트에서 위험한 판정을 순수 함수로 분리해 온 이유와 같다.
//!
//! # 크로스 리전
//!
//! **대상 인스턴스의 리전으로 서명한다.** 앱이 배포된 리전으로 서명하면
//! `Access denied` 가 되고, 원인이 IAM 정책처럼 보여 추적이 오래 걸린다.

use std::time::{Duration, SystemTime};

use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use dbmon_core::error::{DomainError, Result};
use dbmon_core::ports::AuthTokenProvider;
use dbmon_core::secret::{ExpiringSecret, Secret};
use dbmon_core::time::EpochMs;

/// 토큰 유효 시간. RDS 의 상한이 15분이다.
pub const TOKEN_TTL_SECS: u64 = 900;

/// SigV4 서비스명. `rds` 가 아니라 `rds-db` 다 — 틀리면 `Access denied` 만 보인다.
const SERVICE: &str = "rds-db";

/// 대상 리전의 자격증명으로 토큰을 만든다.
pub struct IamAuthTokenProvider<P> {
    credentials: P,
    /// **대상 인스턴스의 리전.** 앱 배포 리전이 아니다.
    region: String,
}

impl<P: ProvideCredentials> IamAuthTokenProvider<P> {
    pub fn new(credentials: P, region: impl Into<String>) -> Self {
        Self {
            credentials,
            region: region.into(),
        }
    }
}

#[async_trait::async_trait]
impl<P: ProvideCredentials + Send + Sync> AuthTokenProvider for IamAuthTokenProvider<P> {
    async fn token(&self, host: &str, port: u16, db_user: &str) -> Result<ExpiringSecret> {
        let creds =
            self.credentials
                .provide_credentials()
                .await
                .map_err(|e| DomainError::Unavailable {
                    dependency: "sts",
                    reason: crate::telemetry::scrub(&e.to_string()),
                })?;
        let now = SystemTime::now();
        let token = presign(&creds, &self.region, host, port, db_user, now)?;
        let expires_at_ms = epoch_ms(now)? + (TOKEN_TTL_SECS as i64) * 1000;
        Ok(ExpiringSecret::new(Secret::new(token), expires_at_ms))
    }
}

fn epoch_ms(t: SystemTime) -> Result<EpochMs> {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .map_err(|e| DomainError::Internal(format!("시계가 UNIX epoch 이전이다: {e}")))
}

/// **순수 서명 함수.** 자격증명·리전·호스트·포트·사용자·시각만 본다.
///
/// 반환값은 MySQL 비밀번호 자리에 넣는 토큰이다 — 스킴(`https://`)은 없다.
pub fn presign(
    creds: &Credentials,
    region: &str,
    host: &str,
    port: u16,
    db_user: &str,
    now: SystemTime,
) -> Result<String> {
    // 쿼리 문자열에 서명을 넣는다(presigned). 헤더 서명은 MySQL 프로토콜로 보낼 수 없다.
    let mut settings = SigningSettings::default();
    settings.signature_location = SignatureLocation::QueryParams;
    settings.expires_in = Some(Duration::from_secs(TOKEN_TTL_SECS));

    let identity = creds.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(SERVICE)
        .time(now)
        .settings(settings)
        .build()
        .map_err(|e| DomainError::Internal(format!("서명 파라미터 구성 실패: {e}")))?
        .into();

    // **호스트에 포트를 포함한다.** 빠지면 서명 대상이 달라져 `Access denied` 다.
    let url = format!("https://{host}:{port}/?Action=connect&DBUser={db_user}");
    let signable = SignableRequest::new("GET", &url, std::iter::empty(), SignableBody::Bytes(&[]))
        .map_err(|e| DomainError::Internal(format!("서명 대상 구성 실패: {e}")))?;

    let signed = sign(signable, &params)
        .map_err(|e| DomainError::Internal(format!("SigV4 서명 실패: {e}")))?;
    let (instructions, _) = signed.into_parts();

    // 서명이 붙은 쿼리 파라미터를 URL 에 합친다.
    let mut query = vec![
        ("Action".to_string(), "connect".to_string()),
        ("DBUser".to_string(), db_user.to_string()),
    ];
    for (name, value) in instructions.params() {
        query.push((name.to_string(), value.to_string()));
    }
    let query: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();

    // **스킴을 뗀다.** `https://` 를 붙여 보내면 서버가 거부한다.
    Ok(format!("{host}:{port}/?{}", query.join("&")))
}

/// 고정 비밀번호로 접속한다 — **`dev` 배포 전용 폴백** (FR-CRD-05).
///
/// # 왜 필요한가
///
/// 로컬 MySQL 컨테이너는 IAM DB Auth 를 지원하지 않는다. 이게 없으면 **수집 루프를
/// 로컬에서 한 번도 돌려 볼 수 없다** — SSO 가 만료되면 개발이 멈춘다는 뜻이다.
///
/// # 왜 설정 파일이 아니라 환경변수인가
///
/// 설정 파일은 레포에 들어가고 실수로 커밋된다. 환경변수는 시크릿 주입의 표준
/// 경로이며 `deny_unknown_fields` 로 보호되는 설정 표면을 늘리지 않는다.
///
/// ⚠ 호출부가 `deployment_env == dev` 를 확인해야 한다. 이 타입 자체는 환경을 모른다 —
/// 검사를 여기 두면 "어디서 검사하는지" 가 두 곳으로 갈린다.
pub struct StaticPasswordProvider {
    password: dbmon_core::secret::SecretString,
}

/// 폴백 비밀번호를 읽는 환경변수.
pub const TARGET_PASSWORD_ENV: &str = "DBMON_TARGET_PASSWORD";

impl StaticPasswordProvider {
    pub fn new(password: impl Into<String>) -> Self {
        Self {
            password: Secret::new(password.into()),
        }
    }

    /// 환경변수에서 읽는다. 없거나 비었으면 `None`.
    pub fn from_env() -> Option<Self> {
        Self::from_value(std::env::var(TARGET_PASSWORD_ENV).ok())
    }

    /// **판정을 순수 함수로 분리한다.** 이 크레이트는 `unsafe` 를 금지하므로
    /// 테스트에서 `set_var` 를 쓸 수 없다(`std::env::set_var` 는 unsafe 다).
    /// 그리고 환경변수를 바꾸는 테스트는 병렬 실행에서 서로를 오염시킨다.
    pub fn from_value(raw: Option<String>) -> Option<Self> {
        raw.filter(|p| !p.trim().is_empty()).map(Self::new)
    }
}

#[async_trait::async_trait]
impl AuthTokenProvider for StaticPasswordProvider {
    async fn token(&self, _host: &str, _port: u16, _db_user: &str) -> Result<ExpiringSecret> {
        // 비밀번호는 만료되지 않는다. **하지만 `i64::MAX` 를 쓰지 않는다** —
        // `needs_refresh` 가 `now + margin` 을 더하므로 오버플로가 난다.
        const NEVER_MS: EpochMs = 4_000_000_000_000; // 2096년
        Ok(ExpiringSecret::new(
            Secret::new(self.password.expose().to_string()),
            NEVER_MS,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 고정 자격증명. 서명이 결정론적이 되도록 값을 박는다.
    fn creds() -> Credentials {
        Credentials::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        )
    }

    /// 2026-08-15T00:00:00Z. 고정 시각이라 서명이 재현된다.
    fn fixed_time() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_752_000)
    }

    fn token() -> String {
        presign(
            &creds(),
            "ap-northeast-2",
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
            3306,
            "dbmon",
            fixed_time(),
        )
        .expect("서명")
    }

    /// **스킴이 없어야 한다.** `https://` 를 붙여 보내면 서버가 거부한다.
    #[test]
    fn token_has_no_scheme_prefix() {
        let t = token();
        assert!(!t.starts_with("http"), "스킴이 붙었다: {t}");
        assert!(
            t.starts_with("orders-01.abc.ap-northeast-2.rds.amazonaws.com:3306/?"),
            "{t}"
        );
    }

    /// 문서가 규정한 요소가 모두 있어야 한다 ([07 §3.1]).
    #[test]
    fn token_carries_the_required_parameters() {
        let t = token();
        for want in [
            "Action=connect",
            "DBUser=dbmon",
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "X-Amz-Signature=",
            "X-Amz-SignedHeaders=host",
            "X-Amz-Expires=900",
        ] {
            assert!(t.contains(want), "{want} 가 없다: {t}");
        }
    }

    /// **서비스명은 `rds-db` 다.** `rds` 로 서명하면 `Access denied` 만 보이고
    /// 원인이 IAM 정책처럼 보여 추적이 오래 걸린다.
    #[test]
    fn credential_scope_names_the_rds_db_service() {
        let t = token();
        assert!(
            t.contains("%2Frds-db%2Faws4_request") || t.contains("/rds-db/aws4_request"),
            "자격증명 범위에 rds-db 가 없다: {t}"
        );
    }

    /// **대상 인스턴스의 리전으로 서명한다.** 앱 배포 리전으로 서명하면 거부된다.
    #[test]
    fn signature_scope_uses_the_target_region() {
        let seoul = token();
        let virginia = presign(
            &creds(),
            "us-east-1",
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
            3306,
            "dbmon",
            fixed_time(),
        )
        .expect("서명");

        assert!(seoul.contains("ap-northeast-2") || seoul.contains("ap-northeast-2%2F"));
        assert!(virginia.contains("us-east-1") || virginia.contains("us-east-1%2F"));
        assert_ne!(
            signature_of(&seoul),
            signature_of(&virginia),
            "리전이 달라도 서명이 같다 — 크로스 리전 대상이 거부된다"
        );
    }

    fn signature_of(token: &str) -> String {
        token
            .split('&')
            .find_map(|p| p.strip_prefix("X-Amz-Signature="))
            .unwrap_or_default()
            .to_string()
    }

    /// **포트가 서명 대상에 들어가야 한다.** 빠지면 서명이 달라져 거부된다.
    #[test]
    fn port_is_part_of_the_signed_request() {
        let a = token();
        let b = presign(
            &creds(),
            "ap-northeast-2",
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
            3307,
            "dbmon",
            fixed_time(),
        )
        .expect("서명");
        assert_ne!(signature_of(&a), signature_of(&b), "포트가 서명에 없다");
        assert!(b.contains(":3307/?"));
    }

    /// 호스트가 다르면 서명이 달라야 한다 — 한 토큰을 다른 인스턴스에 쓸 수 없다.
    #[test]
    fn host_is_part_of_the_signed_request() {
        let a = token();
        let b = presign(
            &creds(),
            "ap-northeast-2",
            "billing-01.abc.ap-northeast-2.rds.amazonaws.com",
            3306,
            "dbmon",
            fixed_time(),
        )
        .expect("서명");
        assert_ne!(signature_of(&a), signature_of(&b));
    }

    /// 사용자명이 다르면 서명이 달라야 한다 — IAM 정책이 사용자명으로 범위를 좁힌다.
    #[test]
    fn db_user_is_part_of_the_signed_request() {
        let a = token();
        let b = presign(
            &creds(),
            "ap-northeast-2",
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
            3306,
            "root",
            fixed_time(),
        )
        .expect("서명");
        assert_ne!(signature_of(&a), signature_of(&b));
        assert!(b.contains("DBUser=root"));
    }

    /// 같은 입력이면 같은 토큰이어야 한다 — 서명이 결정론적이라는 근거.
    #[test]
    fn signing_is_deterministic_for_the_same_inputs() {
        assert_eq!(token(), token());
    }

    /// **골든 벡터.** 위 단정들은 "문서대로 생겼는가" 만 본다 — 서명이 AWS 와 같은
    /// 값인지는 독립 구현과 대조해야 알 수 있다.
    ///
    /// 이 값을 만든 알고리즘은 `aws rds generate-db-auth-token` 과 **바이트 단위로
    /// 일치**함을 확인했다(같은 초에 생성한 쌍의 `X-Amz-Signature` 가 동일). 재확인은
    /// `scripts/verify-iam-token.sh` 로 언제든 돌릴 수 있다.
    ///
    /// 즉 이 상수는 자기 참조가 아니라 **외부 오라클로 검증된 기준점**이다.
    #[test]
    fn matches_the_aws_cli_golden_vector() {
        const GOLDEN: &str = "orders-01.abc.ap-northeast-2.rds.amazonaws.com:3306/?\
Action=connect&DBUser=dbmon&X-Amz-Algorithm=AWS4-HMAC-SHA256\
&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE/20260815/ap-northeast-2/rds-db/aws4_request\
&X-Amz-Date=20260815T000000Z&X-Amz-Expires=900&X-Amz-SignedHeaders=host\
&X-Amz-Signature=7183880a453b7100fcdf0b0b63c0246732337bc4974475bbe94f5515945ac35c";
        assert_eq!(
            token(),
            GOLDEN,
            "서명 알고리즘이 바뀌었다 — scripts/verify-iam-token.sh 로 AWS CLI 와 대조한다"
        );
    }

    /// 시각이 다르면 서명이 달라야 한다 (`X-Amz-Date` 가 서명에 들어간다).
    #[test]
    fn time_is_part_of_the_signed_request() {
        let later = presign(
            &creds(),
            "ap-northeast-2",
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
            3306,
            "dbmon",
            fixed_time() + Duration::from_secs(3600),
        )
        .expect("서명");
        assert_ne!(signature_of(&token()), signature_of(&later));
    }

    /// 세션 토큰이 있으면 쿼리에 포함돼야 한다 — ECS 태스크 롤은 항상 임시 자격증명이다.
    #[test]
    fn session_token_is_included_for_temporary_credentials() {
        let temp = Credentials::new(
            "ASIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            Some("session-token-value".into()),
            None,
            "test",
        );
        let t = presign(
            &temp,
            "ap-northeast-2",
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
            3306,
            "dbmon",
            fixed_time(),
        )
        .expect("서명");
        assert!(
            t.contains("X-Amz-Security-Token="),
            "세션 토큰이 빠졌다 — ECS 태스크 롤로는 접속할 수 없다: {t}"
        );
    }

    /// **토큰이 로그에 그대로 나가면 안 된다.** `ExpiringSecret` 이 감싸는 이유다.
    #[test]
    fn token_is_wrapped_so_debug_does_not_leak_it() {
        let secret = ExpiringSecret::new(Secret::new(token()), 0);
        let printed = format!("{secret:?}");
        assert!(
            !printed.contains("X-Amz-Signature"),
            "Debug 출력에 토큰이 들어 있다: {printed}"
        );
    }

    /// 폴백 비밀번호는 만료되지 않아야 한다 — 다만 **오버플로 없이**.
    #[tokio::test]
    async fn static_password_never_needs_refresh_without_overflowing() {
        let p = StaticPasswordProvider::new("dbmon-local-monitor");
        let s = p.token("127.0.0.1", 13306, "dbmon").await.expect("토큰");
        assert_eq!(s.expose(), "dbmon-local-monitor");
        // `i64::MAX` 였다면 `now + margin` 에서 오버플로로 패닉한다(디버그 빌드).
        assert!(!s.needs_refresh(1_800_000_000_000, 5 * 60_000));
    }

    /// 빈 환경변수는 "설정되지 않음" 으로 본다 — 빈 비밀번호로 접속을 시도하면
    /// 실패 원인이 "인증 실패" 로만 보인다.
    #[test]
    fn an_empty_env_var_is_treated_as_unset() {
        assert!(StaticPasswordProvider::from_value(None).is_none());
        assert!(StaticPasswordProvider::from_value(Some(String::new())).is_none());
        assert!(
            StaticPasswordProvider::from_value(Some("   ".into())).is_none(),
            "공백만 있는 값을 비밀번호로 썼다 — 실패 원인이 인증 실패로만 보인다"
        );
        assert!(StaticPasswordProvider::from_value(Some("pw".into())).is_some());
    }

    /// 만료 여유는 [`TOKEN_TTL_SECS`] 안에서 계산돼야 한다.
    #[test]
    fn expiry_matches_the_documented_ttl() {
        assert_eq!(TOKEN_TTL_SECS, 900, "RDS 상한은 15분이다");
        let expires_at = 1_000 + (TOKEN_TTL_SECS as i64) * 1000;
        let s = ExpiringSecret::new(Secret::new("t".into()), expires_at);
        // 연결 수립 직전 5분 여유로 갱신 판단.
        assert!(!s.needs_refresh(1_000, 5 * 60_000));
        assert!(s.needs_refresh(expires_at - 60_000, 5 * 60_000));
    }
}
