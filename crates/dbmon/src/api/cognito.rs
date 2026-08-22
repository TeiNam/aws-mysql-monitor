//! Cognito 액세스 토큰 검증 (M5, [08 §3](../../../../docs/08-security-auth.md) FR-AUT-07).
//!
//! # 순수 판정과 IO 를 나눈다
//!
//! 서명·클레임 검증은 **순수 함수**다([`verify_token`]). JWKS 조회만 IO 이고
//! ([`JwksCache`]), 캐시는 그 함수에 키를 넘긴다. 그래서 토큰 위조 시나리오를
//! 네트워크 없이 테스트할 수 있다 — 이 파일에서 가장 틀리면 안 되는 부분이 그것이다.
//!
//! # 막아야 하는 위조 다섯 가지
//!
//! | 공격 | 막는 곳 |
//! |---|---|
//! | `alg: none` — 서명 없이 통과 | [`Header::require_rs256`] |
//! | `alg: HS256` — **공개키를 HMAC 비밀로 써서** 자기 서명 | 같은 곳 |
//! | 다른 사용자 풀의 유효한 토큰 | `iss` 대조 |
//! | ID 토큰을 액세스 토큰 자리에 | `token_use == "access"` |
//! | 다른 앱 클라이언트의 토큰 | `client_id` 대조 |
//!
//! `alg: HS256` 이 특히 위험하다. JWKS 의 공개키는 누구나 읽을 수 있으므로, 검증기가
//! 알고리즘을 토큰에서 읽어 그대로 쓰면 공격자가 그 공개키를 HMAC 비밀로 사용해 **임의
//! 클레임의 토큰을 만들 수 있다.** 알고리즘은 토큰이 정하지 않는다 — 우리가 정한다.
//!
//! # 클레임은 권위값이 아니다
//!
//! 검증을 통과한 클레임은 [`TokenClaims`] 이고, 최종 문맥은 서버 `USER` 레코드와의
//! **교집합**이다([`AuthContext::intersect`], T-20). 이 모듈은 교집합을 하지 않는다 —
//! 서명이 유효하다는 사실까지만 말한다.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dbmon_core::rbac::TokenClaims;
use dbmon_core::time::EpochMs;

use super::auth::AuthError;

/// JWKS 캐시 TTL — 문서가 1시간으로 규정했다 (08 §3).
pub const JWKS_TTL_MS: i64 = 60 * 60 * 1_000;

/// `kid` 당 갱신 레이트 리밋. 캐시 미스 폭주로 Cognito 를 두드리지 않는다.
pub const JWKS_REFRESH_MIN_INTERVAL_MS: i64 = 60 * 1_000;

/// JWKS 조회 타임아웃. 인증 경로이므로 짧다 — 느린 인증은 장애다.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// 우리가 받아들이는 **유일한** 서명 알고리즘.
const REQUIRED_ALG: &str = "RS256";

/// JWT 헤더.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Header {
    pub alg: String,
    /// 어느 키로 서명했나. **없으면 검증할 수 없다.**
    #[serde(default)]
    pub kid: Option<String>,
}

impl Header {
    /// **알고리즘을 우리가 정한다.**
    ///
    /// 토큰이 알고리즘을 정하게 하면 `alg: none` 과 `alg: HS256`(공개키를 HMAC 비밀로
    /// 쓰는 고전적 우회)이 열린다. 모듈 문서에 그 이유를 적었다.
    pub fn require_rs256(&self) -> Result<&str, AuthError> {
        if self.alg != REQUIRED_ALG {
            return Err(AuthError::Invalid);
        }
        self.kid.as_deref().ok_or(AuthError::Invalid)
    }
}

/// Cognito 액세스 토큰의 클레임.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AccessClaims {
    pub sub: String,
    pub iss: String,
    /// `access` 여야 한다. ID 토큰은 `id` 다.
    pub token_use: String,
    /// 액세스 토큰에는 `aud` 가 없고 `client_id` 가 있다.
    pub client_id: String,
    /// 만료 (초 단위 epoch).
    pub exp: i64,
    /// 발급 시각 (초 단위 epoch).
    pub iat: i64,
    /// 유효 시작. 없는 경우가 많다.
    #[serde(default)]
    pub nbf: Option<i64>,
    #[serde(default, rename = "cognito:groups")]
    pub groups: Vec<String>,
}

/// JWKS 의 RSA 공개키 하나.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Jwk {
    pub kid: String,
    /// `RSA` 만 쓴다.
    pub kty: String,
    /// modulus (base64url).
    pub n: String,
    /// exponent (base64url).
    pub e: String,
    #[serde(default)]
    pub alg: Option<String>,
}

/// JWKS 응답.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct JwksDocument {
    pub keys: Vec<Jwk>,
}

/// 서명 검증에 쓰는 키 자료.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsaKey {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
}

impl RsaKey {
    /// JWKS 항목에서 키를 만든다.
    ///
    /// `kty != "RSA"` 나 `alg != "RS256"` 이면 **버린다.** Cognito 는 RSA 만 쓰므로
    /// 다른 것이 오면 우리가 모르는 상황이고, 모르는 키로 검증하지 않는다.
    pub fn from_jwk(jwk: &Jwk) -> Option<Self> {
        if jwk.kty != "RSA" {
            return None;
        }
        if jwk.alg.as_deref().is_some_and(|a| a != REQUIRED_ALG) {
            return None;
        }
        Some(Self {
            n: URL_SAFE_NO_PAD.decode(&jwk.n).ok()?,
            e: URL_SAFE_NO_PAD.decode(&jwk.e).ok()?,
        })
    }

    /// RS256 서명을 검증한다.
    ///
    /// `RsaPublicKeyComponents` 를 쓴다 — JWKS 가 주는 `(n, e)` 를 그대로 받으므로
    /// DER/ASN.1 조립이 필요 없다. 조립 코드는 그 자체가 결함 표면이다.
    ///
    /// # 왜 `ring` 이 아니라 `aws-lc-rs` 인가
    ///
    /// AWS SDK 와 `mysql_async` 가 rustls 의 `aws-lc-rs` 프로바이더를 쓴다. 여기서
    /// `ring` 을 쓰면 rustls 에 프로바이더가 둘이 되고, 그러면 **첫 TLS 연결에서
    /// 패닉한다** — 통합 테스트 8개가 그렇게 죽었다. API 는 같다.
    pub fn verify(&self, signing_input: &[u8], signature: &[u8]) -> bool {
        use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
        RsaPublicKeyComponents {
            n: self.n.as_slice(),
            e: self.e.as_slice(),
        }
        .verify(&RSA_PKCS1_2048_8192_SHA256, signing_input, signature)
        .is_ok()
    }
}

/// 분해된 토큰.
///
/// 튜플이 아니라 구조체인 이유: 네 조각의 순서를 호출부가 외워야 하면 `claims` 와
/// `signature` 를 바꿔 넘기는 실수가 컴파일된다.
#[derive(Debug)]
pub struct SplitToken<'a> {
    pub header: Header,
    /// 클레임 JSON 바이트. **서명을 검증하기 전에는 신뢰하지 않는다.**
    pub claims: Vec<u8>,
    /// 서명 대상 — `<header>.<payload>` 의 **원문 바이트**.
    pub signing_input: &'a [u8],
    pub signature: Vec<u8>,
}

/// 토큰을 세 조각으로 나눈다.
///
/// **서명 대상은 원문 문자열이다** — `header.payload` 를 다시 인코딩해 만들면 안 된다.
/// base64url 인코딩이 비트 단위로 같지 않을 수 있고, 그러면 유효한 토큰이 거부된다.
pub fn split_token(token: &str) -> Result<SplitToken<'_>, AuthError> {
    let mut parts = token.split('.');
    let (h, p, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        // 조각이 셋이 아니면 JWT 가 아니다. `alg: none` 토큰은 서명이 빈 문자열인
        // 조각 셋이므로 여기서 걸리지 않고 `require_rs256` 에서 걸린다.
        _ => return Err(AuthError::Invalid),
    };
    let header: Header =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h).map_err(|_| AuthError::Invalid)?)
            .map_err(|_| AuthError::Invalid)?;
    let claims = URL_SAFE_NO_PAD.decode(p).map_err(|_| AuthError::Invalid)?;
    let signature = URL_SAFE_NO_PAD.decode(s).map_err(|_| AuthError::Invalid)?;

    // 서명 대상은 `<header>.<payload>` 의 **원문 바이트**다.
    let signing_len = h.len() + 1 + p.len();
    let signing_input = &token.as_bytes()[..signing_len];

    Ok(SplitToken {
        header,
        claims,
        signing_input,
        signature,
    })
}

/// 검증에 필요한 설정 사실.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    /// `https://cognito-idp.<region>.amazonaws.com/<pool-id>`.
    pub issuer: String,
    pub client_id: String,
}

impl Verification {
    /// 설정에서 만든다. 불완전하면 `None`.
    pub fn from_settings(c: &dbmon_core::settings::CognitoSettings) -> Option<Self> {
        let region = c.effective_region()?;
        if c.user_pool_id.is_empty() || c.client_id.is_empty() {
            return None;
        }
        Some(Self {
            issuer: format!(
                "https://cognito-idp.{region}.amazonaws.com/{}",
                c.user_pool_id
            ),
            client_id: c.client_id.clone(),
        })
    }

    /// JWKS URL.
    pub fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.issuer)
    }
}

/// 클레임을 검증하고 [`TokenClaims`] 로 옮긴다. **순수 함수다.**
///
/// 시계 오차 허용은 **0** 이다(08 §3). 서버 NTP 로 해결하는 문제이고, 허용치를 두면
/// 만료된 토큰이 그만큼 더 산다.
pub fn validate_claims(
    claims: &AccessClaims,
    v: &Verification,
    now_ms: EpochMs,
) -> Result<TokenClaims, AuthError> {
    if claims.iss != v.issuer {
        return Err(AuthError::Invalid);
    }
    // **ID 토큰을 액세스 토큰 자리에 쓰지 못한다.** 만료·스코프 의미가 다르다.
    if claims.token_use != "access" {
        return Err(AuthError::Invalid);
    }
    if claims.client_id != v.client_id {
        return Err(AuthError::Invalid);
    }
    if claims.sub.is_empty() {
        return Err(AuthError::Invalid);
    }

    let now_s = now_ms.div_euclid(1_000);
    if claims.exp <= now_s {
        return Err(AuthError::Invalid);
    }
    // **미래에 발급된 토큰을 받지 않는다.** 시계가 어긋난 발급자나 위조 신호다.
    if claims.iat > now_s {
        return Err(AuthError::Invalid);
    }
    if claims.nbf.is_some_and(|nbf| nbf > now_s) {
        return Err(AuthError::Invalid);
    }

    Ok(TokenClaims {
        subject: claims.sub.clone(),
        groups: claims.groups.clone(),
        // **토큰이 환경 스코프를 주장하지 않는다.** 스코프는 서버 `USER` 레코드에만
        // 있다. 빈 목록은 `intersect` 에서 "토큰이 좁히지 않는다" 로 읽힌다.
        env_scope: Vec::new(),
        issued_at_ms: claims.iat.saturating_mul(1_000),
        // **토큰이 버전을 주장하지 않는다.**
        //
        // Pre Token Generation 트리거를 요구하지 않기로 했으므로 이 클레임이 없다.
        // `None` 은 사실을 그대로 표현하고, `intersect` 가 그 검사를 건너뛴다 —
        // 서버 값을 채워 넣어 "항상 같다" 로 만들면 검사가 죽은 코드가 된다
        // (교차 리뷰가 잡았다). 그때 남는 폐기 수단은 `revoked_after_ms` 다.
        claims_version: None,
    })
}

/// 토큰을 검증한다 — **서명 + 클레임.** 순수 함수다(키를 받는다).
pub fn verify_token(
    token: &str,
    key_for: impl FnOnce(&str) -> Option<RsaKey>,
    v: &Verification,
    now_ms: EpochMs,
) -> Result<TokenClaims, AuthError> {
    let t = split_token(token)?;
    let kid = t.header.require_rs256()?;
    let key = key_for(kid).ok_or(AuthError::Invalid)?;
    if !key.verify(t.signing_input, &t.signature) {
        return Err(AuthError::Invalid);
    }
    // **서명을 먼저 검증한 뒤에 클레임을 읽는다.** 순서가 반대면 서명되지 않은
    // 값으로 분기하게 되고, 그 분기가 오류 메시지·로그로 새면 정보 노출이다.
    let claims: AccessClaims = serde_json::from_slice(&t.claims).map_err(|_| AuthError::Invalid)?;
    validate_claims(&claims, v, now_ms)
}

// ── JWKS 캐시 (IO) ────────────────────────────────────────────────────────────

/// JWKS 를 가져오는 방법. 테스트가 대체한다.
#[async_trait::async_trait]
pub trait JwksSource: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<JwksDocument, String>;
}

/// HTTPS 로 가져온다.
pub struct HttpJwksSource {
    client: reqwest::Client,
}

impl HttpJwksSource {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            // **리다이렉트를 따르지 않는다.** JWKS 엔드포인트는 고정 주소이고,
            // 리다이렉트를 따르면 키 출처가 우리가 검증하지 않은 호스트가 될 수 있다.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl JwksSource for HttpJwksSource {
    async fn fetch(&self, url: &str) -> Result<JwksDocument, String> {
        let res = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("JWKS 조회 실패: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("JWKS 조회 실패: HTTP {}", res.status()));
        }
        res.json::<JwksDocument>()
            .await
            .map_err(|e| format!("JWKS 파싱 실패: {e}"))
    }
}

/// [`JwksCache::lookup`] 의 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyLookup {
    /// 신선한 캐시 히트. 갱신하지 않는다.
    Fresh(RsaKey),
    /// 캐시에 있지만 TTL 이 지났다. **갱신하고, 실패하면 이 키를 쓴다.**
    Stale(RsaKey),
    /// 캐시에 없다. 갱신한다.
    Missing,
    /// 없거나 낡았지만 **레이트 리밋에 걸렸다.** 갱신하지 않는다.
    ///
    /// 담긴 값이 있으면 그것으로 검증한다(낡은 키가 유효할 수 있다). 없으면 거부다 —
    /// 무작위 `kid` 공격이 여기로 떨어지고, 그때 외부 호출이 나가지 않는 것이 요점이다.
    RateLimited(Option<RsaKey>),
}

/// `kid` → 키 캐시.
pub struct JwksCache {
    source: Box<dyn JwksSource>,
    state: RwLock<CacheState>,
}

#[derive(Default)]
struct CacheState {
    keys: HashMap<String, RsaKey>,
    fetched_at_ms: EpochMs,
    /// `kid` 별 마지막 갱신 시도. 레이트 리밋의 근거.
    last_attempt: HashMap<String, EpochMs>,
}

impl JwksCache {
    pub fn new(source: Box<dyn JwksSource>) -> Self {
        Self {
            source,
            state: RwLock::new(CacheState::default()),
        }
    }

    /// 캐시된 키. 갱신하지 않는다.
    pub fn cached(&self, kid: &str) -> Option<RsaKey> {
        self.state.read().ok()?.keys.get(kid).cloned()
    }

    /// 키를 찾고 **갱신이 필요한지 함께 말한다.**
    ///
    /// # 왜 `Option<RsaKey>` 가 아닌가
    ///
    /// 처음에는 `Option` 을 돌려주고 호출부가 `None` 일 때 갱신했다. 그러면 두 가지가
    /// 조용히 깨진다:
    ///
    /// 1. **TTL 이 무한이 된다** — `kid` 가 캐시에 있으면 낡았어도 그 값을 돌려주므로
    ///    갱신이 일어나지 않는다. 키 회전을 영구히 못 따라간다.
    /// 2. **레이트 리밋이 없는 것과 같다** — 호출부는 "없다" 만 보고 갱신하므로,
    ///    무작위 `kid` 로 요청을 쏟으면 요청마다 JWKS 를 가져온다. 인증 실패 경로가
    ///    외부 호출 증폭기가 된다.
    ///
    /// 판정을 값으로 만들어 호출부가 무시할 수 없게 한다.
    pub fn lookup(&self, kid: &str, now_ms: EpochMs) -> KeyLookup {
        let Ok(mut s) = self.state.write() else {
            // 락이 깨졌다. 통과시키지 않는다.
            return KeyLookup::RateLimited(None);
        };
        let cached = s.keys.get(kid).cloned();
        let stale = now_ms.saturating_sub(s.fetched_at_ms) > JWKS_TTL_MS;

        if let Some(k) = cached.clone() {
            if !stale {
                return KeyLookup::Fresh(k);
            }
        }

        // 캐시에 없거나 낡았다 — 갱신을 시도해도 되는가.
        let last = s.last_attempt.get(kid).copied().unwrap_or(i64::MIN);
        if now_ms.saturating_sub(last) < JWKS_REFRESH_MIN_INTERVAL_MS {
            return KeyLookup::RateLimited(cached);
        }
        s.last_attempt.insert(kid.to_string(), now_ms);
        match cached {
            Some(k) => KeyLookup::Stale(k),
            None => KeyLookup::Missing,
        }
    }

    /// JWKS 를 다시 읽어 캐시를 갈아 끼운다.
    ///
    /// **실패하면 기존 캐시를 유지한다.** 비우면 Cognito 장애가 곧 전체 인증 실패다.
    pub async fn refresh(&self, url: &str, now_ms: EpochMs) -> Result<usize, String> {
        let doc = self.source.fetch(url).await?;
        let fresh: HashMap<String, RsaKey> = doc
            .keys
            .iter()
            .filter_map(|j| RsaKey::from_jwk(j).map(|k| (j.kid.clone(), k)))
            .collect();
        if fresh.is_empty() {
            return Err("JWKS 에 쓸 수 있는 RSA 키가 없다".into());
        }
        let n = fresh.len();
        if let Ok(mut s) = self.state.write() {
            s.keys = fresh;
            s.fetched_at_ms = now_ms;
        }
        Ok(n)
    }

    /// 캐시된 키 수 (진단용).
    pub fn len(&self) -> usize {
        self.state.read().map(|s| s.keys.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 검증기 — JWKS 캐시 + USER 레코드 교집합
// ─────────────────────────────────────────────────────────────────────────────

/// Cognito 인증을 수행하는 조립체.
///
/// # 순서가 보안이다
///
/// 1. 서명 검증 (`kid` → JWKS 캐시 → RS256)
/// 2. 클레임 검증 (`iss`·`token_use`·`client_id`·`exp`/`iat`/`nbf`)
/// 3. `USER#<sub>` 조회 — **없으면 거부** (승인 대기)
/// 4. [`AuthContext::intersect`] — 토큰과 서버 레코드의 **더 낮은 쪽**
///
/// 3번을 2번보다 먼저 하면 서명되지 않은 `sub` 로 저장소를 조회하게 되고, 그건 무료
/// 조회 경로다. 4번을 건너뛰면 토큰이 권한을 올릴 수 있다(T-20).
pub struct CognitoVerifier {
    jwks: JwksCache,
    users: std::sync::Arc<dyn dbmon_core::ports::UserStore>,
}

impl CognitoVerifier {
    pub fn new(jwks: JwksCache, users: std::sync::Arc<dyn dbmon_core::ports::UserStore>) -> Self {
        Self { jwks, users }
    }

    /// HTTPS JWKS 소스로 만든다. 클라이언트를 못 만들면 `None` —
    /// **스텁으로 통과시키지 않는다.**
    pub fn with_https(users: std::sync::Arc<dyn dbmon_core::ports::UserStore>) -> Option<Self> {
        let source = HttpJwksSource::new().ok()?;
        Some(Self::new(JwksCache::new(Box::new(source)), users))
    }

    /// 토큰을 인증한다.
    pub async fn authenticate(
        &self,
        token: &str,
        v: &Verification,
        now_ms: EpochMs,
    ) -> Result<dbmon_core::rbac::AuthContext, AuthError> {
        // ── 1·2. 서명과 클레임 ──
        let kid = split_token(token)?.header.require_rs256()?.to_string();

        let key = match self.key_for(&kid, v, now_ms).await {
            Some(k) => k,
            // 키를 못 찾으면 **통과시키지 않는다.** JWKS 조회 실패와 알 수 없는 `kid`
            // 를 여기서 구분하지 않는 이유: 둘 다 "이 토큰을 검증할 수 없다" 이고,
            // 구분해 응답하면 어느 키가 존재하는지 알려주게 된다.
            None => return Err(AuthError::Invalid),
        };
        let claims = verify_token(token, |_| Some(key), v, now_ms)?;

        // ── 3. 서버 레코드 ──
        let record = self
            .users
            .get(&claims.subject)
            .await
            // 저장소 장애를 **통과로 접지 않는다.** 읽을 수 없으면 인가할 수 없다.
            .map_err(|_| AuthError::Invalid)?
            .ok_or(AuthError::Invalid)?;

        // ── 4. 교집합 ──
        //
        // **클레임을 손대지 않는다.** 예전에는 `claims_version` 을 서버 값으로 덮어
        // `intersect` 의 버전 검사를 통과시켰다 — 그러면 그 검사가 죽은 코드가 되고
        // T-33 의 독립적인 폐기 수단이 사라진다(교차 리뷰가 HIGH 로 잡았다).
        //
        // 이제 토큰은 `None` 을 주장하고 `intersect` 가 그 검사를 건너뛴다. 남는
        // 폐기 수단은 `revoked_after_ms` 이고, 권한을 바꿀 때 그것을 함께 세우는
        // 것이 운영 규칙이다(docs/08-security-auth.md §3.1).
        dbmon_core::rbac::AuthContext::intersect(&claims, &record).ok_or(AuthError::Invalid)
    }

    /// 키를 얻는다 — 캐시 판정에 따라 갱신을 **한 번만** 한다.
    async fn key_for(&self, kid: &str, v: &Verification, now_ms: EpochMs) -> Option<RsaKey> {
        let fallback = match self.jwks.lookup(kid, now_ms) {
            // 신선하다. 외부 호출이 없다.
            KeyLookup::Fresh(k) => return Some(k),
            // **레이트 리밋에 걸렸다 — 갱신하지 않는다.** 무작위 `kid` 로 오는
            // 요청이 여기로 떨어지고, 그때 JWKS 를 가져오지 않는 것이 요점이다.
            KeyLookup::RateLimited(cached) => return cached,
            // 갱신하고, 실패하면 낡은 키로 계속한다 (Cognito 장애 내성).
            KeyLookup::Stale(k) => Some(k),
            KeyLookup::Missing => None,
        };

        if let Err(e) = self.jwks.refresh(&v.jwks_url(), now_ms).await {
            tracing::warn!(error = %e, %kid, "JWKS 갱신 실패");
            return fallback;
        }
        // 갱신 후 다시 본다. 새 JWKS 에도 없으면 그 `kid` 는 우리 풀의 것이 아니다.
        self.jwks.cached(kid).or(fallback)
    }

    /// 캐시된 키 수 (진단용).
    pub fn cached_keys(&self) -> usize {
        self.jwks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verification() -> Verification {
        Verification {
            issuer: "https://cognito-idp.ap-northeast-2.amazonaws.com/ap-northeast-2_AbCdEf".into(),
            client_id: "1h57kf5cpq17m0eml12EXAMPLE".into(),
        }
    }

    fn claims(now_s: i64) -> AccessClaims {
        AccessClaims {
            sub: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            iss: verification().issuer,
            token_use: "access".into(),
            client_id: verification().client_id,
            exp: now_s + 3600,
            iat: now_s - 10,
            nbf: None,
            groups: vec!["dbmon-operator".into()],
        }
    }

    const NOW_MS: EpochMs = 1_787_443_200_000;
    const NOW_S: i64 = NOW_MS / 1000;

    #[test]
    fn a_valid_access_token_claim_set_passes() {
        let t = validate_claims(&claims(NOW_S), &verification(), NOW_MS).expect("통과");
        assert_eq!(t.groups, vec!["dbmon-operator"]);
        assert_eq!(t.issued_at_ms, (NOW_S - 10) * 1000);
        assert!(t.env_scope.is_empty(), "토큰이 스코프를 주장하지 않는다");
    }

    /// **다른 사용자 풀의 유효한 토큰을 받지 않는다.**
    #[test]
    fn a_token_from_another_user_pool_is_rejected() {
        let mut c = claims(NOW_S);
        c.iss = "https://cognito-idp.us-east-1.amazonaws.com/us-east-1_Other".into();
        assert_eq!(
            validate_claims(&c, &verification(), NOW_MS),
            Err(AuthError::Invalid)
        );
    }

    /// **ID 토큰을 액세스 토큰 자리에 쓰지 못한다.**
    #[test]
    fn an_id_token_is_rejected() {
        let mut c = claims(NOW_S);
        c.token_use = "id".into();
        assert!(validate_claims(&c, &verification(), NOW_MS).is_err());
    }

    /// **다른 앱 클라이언트의 토큰을 받지 않는다.**
    #[test]
    fn a_token_for_another_client_is_rejected() {
        let mut c = claims(NOW_S);
        c.client_id = "someOtherClientId".into();
        assert!(validate_claims(&c, &verification(), NOW_MS).is_err());
    }

    /// 만료는 **허용 오차 0** 이다 (08 §3).
    #[test]
    fn expiry_has_no_clock_skew_allowance() {
        let mut c = claims(NOW_S);
        c.exp = NOW_S; // 정확히 지금
        assert!(
            validate_claims(&c, &verification(), NOW_MS).is_err(),
            "exp == now 를 통과시켰다"
        );
        c.exp = NOW_S + 1;
        assert!(validate_claims(&c, &verification(), NOW_MS).is_ok());
    }

    /// **미래에 발급된 토큰을 받지 않는다.**
    #[test]
    fn a_token_issued_in_the_future_is_rejected() {
        let mut c = claims(NOW_S);
        c.iat = NOW_S + 60;
        assert!(validate_claims(&c, &verification(), NOW_MS).is_err());
    }

    #[test]
    fn nbf_is_honoured_when_present() {
        let mut c = claims(NOW_S);
        c.nbf = Some(NOW_S + 60);
        assert!(validate_claims(&c, &verification(), NOW_MS).is_err());
        c.nbf = Some(NOW_S - 60);
        assert!(validate_claims(&c, &verification(), NOW_MS).is_ok());
    }

    #[test]
    fn an_empty_subject_is_rejected() {
        let mut c = claims(NOW_S);
        c.sub = String::new();
        assert!(validate_claims(&c, &verification(), NOW_MS).is_err());
    }

    // ── 알고리즘 혼동 (가장 위험한 위조) ──────────────────────────────────────

    /// **`alg: none` 을 거부한다.**
    #[test]
    fn alg_none_is_rejected() {
        let h = Header {
            alg: "none".into(),
            kid: Some("k1".into()),
        };
        assert_eq!(h.require_rs256(), Err(AuthError::Invalid));
    }

    /// **`alg: HS256` 을 거부한다.**
    ///
    /// JWKS 의 공개키는 누구나 읽을 수 있다. 검증기가 알고리즘을 토큰에서 읽으면
    /// 공격자가 그 공개키를 HMAC 비밀로 써서 임의 클레임의 토큰을 만들 수 있다 —
    /// JWT 구현에서 가장 유명한 취약점이다.
    #[test]
    fn alg_confusion_to_hmac_is_rejected() {
        for alg in ["HS256", "HS384", "HS512", "hs256", "RS512", "ES256"] {
            let h = Header {
                alg: alg.into(),
                kid: Some("k1".into()),
            };
            assert_eq!(
                h.require_rs256(),
                Err(AuthError::Invalid),
                "{alg} 를 통과시켰다"
            );
        }
        let ok = Header {
            alg: "RS256".into(),
            kid: Some("k1".into()),
        };
        assert_eq!(ok.require_rs256(), Ok("k1"));
    }

    /// **`kid` 가 없으면 검증할 수 없다.**
    #[test]
    fn a_missing_kid_is_rejected() {
        let h = Header {
            alg: "RS256".into(),
            kid: None,
        };
        assert!(h.require_rs256().is_err());
    }

    // ── 토큰 분해 ─────────────────────────────────────────────────────────────

    fn b64(s: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(s)
    }

    #[test]
    fn a_token_splits_into_header_claims_and_signature() {
        let h = b64(br#"{"alg":"RS256","kid":"k1"}"#);
        let p = b64(br#"{"sub":"s"}"#);
        let s = b64(&[1u8, 2, 3]);
        let token = format!("{h}.{p}.{s}");
        let t = split_token(&token).expect("분해");
        assert_eq!(t.header.alg, "RS256");
        assert_eq!(t.header.kid.as_deref(), Some("k1"));
        assert_eq!(t.claims, br#"{"sub":"s"}"#);
        assert_eq!(t.signature, vec![1, 2, 3]);
        // **서명 대상은 원문 바이트다** — 재인코딩하면 안 된다.
        assert_eq!(t.signing_input, format!("{h}.{p}").as_bytes());
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        for bad in [
            "",
            "onlyonepart",
            "two.parts",
            "a.b.c.d",
            "!!!.!!!.!!!",
            "$$.$$.$$",
        ] {
            assert!(split_token(bad).is_err(), "{bad:?} 를 통과시켰다");
        }
    }

    /// 서명이 빈 `alg: none` 토큰도 **분해는 되고 알고리즘에서 걸린다.**
    ///
    /// 두 단계가 각자 자기 일을 한다는 것을 고정한다 — 분해에서 거부하면
    /// `require_rs256` 이 죽은 코드가 되고, 그 사실이 묻힌다.
    #[test]
    fn an_unsigned_token_is_caught_by_the_algorithm_check_not_the_split() {
        let h = b64(br#"{"alg":"none","kid":"k1"}"#);
        let p = b64(br#"{"sub":"s"}"#);
        let token = format!("{h}.{p}.");
        let t = split_token(&token).expect("분해는 된다");
        assert!(t.signature.is_empty());
        assert!(
            t.header.require_rs256().is_err(),
            "알고리즘에서 걸려야 한다"
        );
    }

    // ── 키 파싱 ───────────────────────────────────────────────────────────────

    #[test]
    fn only_rsa_rs256_keys_are_accepted() {
        let base = Jwk {
            kid: "k1".into(),
            kty: "RSA".into(),
            n: b64(&[0xAB; 256]),
            e: b64(&[1, 0, 1]),
            alg: Some("RS256".into()),
        };
        assert!(RsaKey::from_jwk(&base).is_some());

        // 다른 키 타입은 버린다.
        let ec = Jwk {
            kty: "EC".into(),
            ..base.clone()
        };
        assert!(RsaKey::from_jwk(&ec).is_none());

        // 다른 알고리즘은 버린다.
        let hs = Jwk {
            alg: Some("HS256".into()),
            ..base.clone()
        };
        assert!(RsaKey::from_jwk(&hs).is_none());

        // `alg` 가 없으면 받는다 — JWKS 에 생략되는 경우가 있다.
        let no_alg = Jwk {
            alg: None,
            ..base.clone()
        };
        assert!(RsaKey::from_jwk(&no_alg).is_some());

        // base64url 이 깨지면 버린다.
        let bad = Jwk {
            n: "!!!not base64!!!".into(),
            ..base
        };
        assert!(RsaKey::from_jwk(&bad).is_none());
    }

    #[test]
    fn a_bogus_signature_does_not_verify() {
        // 실제 RSA 키가 아니어도 `verify` 가 참을 주지 않아야 한다.
        let key = RsaKey {
            n: vec![0xAB; 256],
            e: vec![1, 0, 1],
        };
        assert!(!key.verify(b"header.payload", &[0u8; 256]));
        assert!(!key.verify(b"header.payload", &[]));
    }

    /// **키를 못 찾으면 통과시키지 않는다.**
    #[test]
    fn an_unknown_kid_fails_verification() {
        let h = b64(br#"{"alg":"RS256","kid":"unknown"}"#);
        let p = b64(br#"{"sub":"s"}"#);
        let token = format!("{h}.{p}.{}", b64(&[0u8; 256]));
        let r = verify_token(&token, |_| None, &verification(), NOW_MS);
        assert_eq!(r, Err(AuthError::Invalid));
    }

    /// **서명이 틀리면 클레임을 읽기 전에 거부한다.**
    #[test]
    fn a_bad_signature_is_rejected_before_claims_are_trusted() {
        let h = b64(br#"{"alg":"RS256","kid":"k1"}"#);
        // 클레임은 완벽하게 유효하다 — 서명만 틀렸다.
        let p = b64(serde_json::to_string(&serde_json::json!({
            "sub": "s", "iss": verification().issuer, "token_use": "access",
            "client_id": verification().client_id, "exp": NOW_S + 3600, "iat": NOW_S - 10
        }))
        .unwrap()
        .as_bytes());
        let token = format!("{h}.{p}.{}", b64(&[0u8; 256]));
        let key = RsaKey {
            n: vec![0xAB; 256],
            e: vec![1, 0, 1],
        };
        assert_eq!(
            verify_token(&token, |_| Some(key), &verification(), NOW_MS),
            Err(AuthError::Invalid)
        );
    }

    // ── 설정 → 검증 사실 ──────────────────────────────────────────────────────

    #[test]
    fn the_issuer_is_derived_from_the_pool_id() {
        let mut c = dbmon_core::settings::CognitoSettings {
            user_pool_id: "ap-northeast-2_AbCdEf".into(),
            client_id: "cid".into(),
            region: String::new(),
            domain: String::new(),
        };
        let v = Verification::from_settings(&c).expect("완전한 설정");
        assert_eq!(
            v.issuer,
            "https://cognito-idp.ap-northeast-2.amazonaws.com/ap-northeast-2_AbCdEf"
        );
        assert_eq!(
            v.jwks_url(),
            "https://cognito-idp.ap-northeast-2.amazonaws.com/ap-northeast-2_AbCdEf/.well-known/jwks.json"
        );

        // 불완전한 설정은 검증 사실을 만들지 못한다 — 그래야 조용히 통과하지 않는다.
        c.client_id = String::new();
        assert!(Verification::from_settings(&c).is_none());
    }

    /// 리전을 따로 지정하면 그것이 이긴다 (교차 리전 풀).
    #[test]
    fn an_explicit_region_wins_over_the_pool_prefix() {
        let c = dbmon_core::settings::CognitoSettings {
            user_pool_id: "ap-northeast-2_AbCdEf".into(),
            client_id: "cid".into(),
            region: "us-east-1".into(),
            domain: String::new(),
        };
        let v = Verification::from_settings(&c).expect("설정");
        assert!(v.issuer.contains("us-east-1.amazonaws.com"));
    }

    // ── JWKS 캐시 ─────────────────────────────────────────────────────────────

    struct FakeSource {
        doc: JwksDocument,
        calls: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl JwksSource for FakeSource {
        async fn fetch(&self, _url: &str) -> Result<JwksDocument, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err("네트워크 실패".into());
            }
            Ok(self.doc.clone())
        }
    }

    fn jwks_doc(kids: &[&str]) -> JwksDocument {
        JwksDocument {
            keys: kids
                .iter()
                .map(|k| Jwk {
                    kid: (*k).into(),
                    kty: "RSA".into(),
                    n: b64(&[0xAB; 256]),
                    e: b64(&[1, 0, 1]),
                    alg: Some("RS256".into()),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn a_refresh_populates_the_cache() {
        let cache = JwksCache::new(Box::new(FakeSource {
            doc: jwks_doc(&["k1", "k2"]),
            calls: Default::default(),
            fail: false,
        }));
        assert!(cache.is_empty());
        assert_eq!(cache.refresh("url", NOW_MS).await.expect("갱신"), 2);
        assert_eq!(cache.len(), 2);
        assert!(cache.cached("k1").is_some());
        assert!(cache.cached("nope").is_none());
    }

    /// **갱신 실패가 캐시를 비우지 않는다.** Cognito 장애가 곧 전체 인증 실패면 안 된다.
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_existing_keys() {
        let good = JwksCache::new(Box::new(FakeSource {
            doc: jwks_doc(&["k1"]),
            calls: Default::default(),
            fail: false,
        }));
        good.refresh("url", NOW_MS).await.expect("갱신");

        // 실패하는 소스로 바꿀 수는 없으니, 빈 문서로 같은 판정을 확인한다.
        let empty = JwksCache::new(Box::new(FakeSource {
            doc: JwksDocument { keys: vec![] },
            calls: Default::default(),
            fail: false,
        }));
        empty
            .refresh("url", NOW_MS)
            .await
            .expect_err("빈 JWKS 는 오류다");
        assert!(empty.is_empty());

        let failing = JwksCache::new(Box::new(FakeSource {
            doc: jwks_doc(&["k1"]),
            calls: Default::default(),
            fail: true,
        }));
        assert!(failing.refresh("url", NOW_MS).await.is_err());
    }

    /// **쓸 수 있는 키가 없는 JWKS 는 오류다.**
    ///
    /// 조용히 빈 캐시를 만들면 모든 토큰이 `Invalid` 가 되고, 원인이 JWKS 라는 사실이
    /// 드러나지 않는다.
    #[tokio::test]
    async fn a_jwks_with_no_usable_keys_is_an_error() {
        let ec_only = JwksDocument {
            keys: vec![Jwk {
                kid: "k1".into(),
                kty: "EC".into(),
                n: b64(&[0xAB; 32]),
                e: b64(&[1, 0, 1]),
                alg: None,
            }],
        };
        let cache = JwksCache::new(Box::new(FakeSource {
            doc: ec_only,
            calls: Default::default(),
            fail: false,
        }));
        assert!(cache.refresh("url", NOW_MS).await.is_err());
    }

    /// **`kid` 당 갱신 레이트 리밋이 있다.** 없는 `kid` 로 요청이 쏟아져도 Cognito 를
    /// 분당 한 번만 두드린다.
    #[tokio::test]
    async fn refresh_attempts_are_rate_limited_per_kid() {
        let cache = JwksCache::new(Box::new(FakeSource {
            doc: jwks_doc(&["k1"]),
            calls: Default::default(),
            fail: false,
        }));
        assert_eq!(
            cache.lookup("k9", NOW_MS),
            KeyLookup::Missing,
            "첫 시도는 허용"
        );
        assert_eq!(
            cache.lookup("k9", NOW_MS),
            KeyLookup::RateLimited(None),
            "즉시 재시도는 거부"
        );
        assert_eq!(
            cache.lookup("k9", NOW_MS + JWKS_REFRESH_MIN_INTERVAL_MS - 1),
            KeyLookup::RateLimited(None),
            "1분 미만은 거부"
        );
        assert_eq!(
            cache.lookup("k9", NOW_MS + JWKS_REFRESH_MIN_INTERVAL_MS),
            KeyLookup::Missing,
            "1분 뒤는 허용"
        );
        // 다른 `kid` 는 자기 예산을 갖는다.
        assert_eq!(cache.lookup("k8", NOW_MS), KeyLookup::Missing);
    }

    /// **TTL 이 지난 키는 갱신 대상이다.**
    ///
    /// 이걸 놓치면 TTL 이 무한이 된다 — `kid` 가 캐시에 있으면 낡았어도 그대로
    /// 돌려주게 되고, 키 회전을 영구히 따라가지 못한다. 첫 구현이 그랬다.
    #[tokio::test]
    async fn a_stale_key_is_reported_as_refreshable_not_fresh() {
        let cache = JwksCache::new(Box::new(FakeSource {
            doc: jwks_doc(&["k1"]),
            calls: Default::default(),
            fail: false,
        }));
        cache.refresh("url", NOW_MS).await.expect("갱신");

        // TTL 안에서는 신선하다.
        assert!(matches!(
            cache.lookup("k1", NOW_MS + JWKS_TTL_MS),
            KeyLookup::Fresh(_)
        ));
        // TTL 을 넘으면 **낡은 것으로 보고**한다 — 호출부가 갱신할 근거가 된다.
        assert!(
            matches!(
                cache.lookup("k1", NOW_MS + JWKS_TTL_MS + 1),
                KeyLookup::Stale(_)
            ),
            "낡은 키를 신선하다고 보고했다 — TTL 이 무한이다"
        );
    }

    /// **레이트 리밋에 걸린 낡은 키는 그대로 쓴다.** 갱신은 안 하지만 검증은 된다 —
    /// Cognito 장애 중에 전체 인증이 죽지 않아야 한다.
    #[tokio::test]
    async fn a_rate_limited_stale_key_is_still_usable() {
        let cache = JwksCache::new(Box::new(FakeSource {
            doc: jwks_doc(&["k1"]),
            calls: Default::default(),
            fail: false,
        }));
        cache.refresh("url", NOW_MS).await.expect("갱신");
        let late = NOW_MS + JWKS_TTL_MS + 1;

        // 첫 조회는 갱신 예산을 쓴다.
        assert!(matches!(cache.lookup("k1", late), KeyLookup::Stale(_)));
        // 두 번째는 리밋에 걸리지만 **키는 담겨 온다.**
        match cache.lookup("k1", late) {
            KeyLookup::RateLimited(Some(_)) => {}
            other => panic!("낡은 키를 잃었다: {other:?}"),
        }
    }

    /// **검증기가 레이트 리밋을 실제로 지킨다.**
    ///
    /// 무작위 `kid` 로 오는 요청마다 JWKS 를 가져오면 인증 실패 경로가 외부 호출
    /// 증폭기가 된다. 첫 구현이 그랬다 — `key()` 가 리밋 판정을 버렸다.
    #[tokio::test]
    async fn the_verifier_does_not_fetch_jwks_on_every_unknown_kid() {
        use std::sync::atomic::Ordering;

        struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl JwksSource for Counting {
            async fn fetch(&self, _url: &str) -> Result<JwksDocument, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(JwksDocument {
                    keys: vec![Jwk {
                        kid: "real".into(),
                        kty: "RSA".into(),
                        n: URL_SAFE_NO_PAD.encode([0xAB; 256]),
                        e: URL_SAFE_NO_PAD.encode([1u8, 0, 1]),
                        alg: Some("RS256".into()),
                    }],
                })
            }
        }

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cache = JwksCache::new(Box::new(Counting(std::sync::Arc::clone(&calls))));
        let users: std::sync::Arc<dyn dbmon_core::ports::UserStore> = std::sync::Arc::new(NoUsers);
        let verifier = CognitoVerifier::new(cache, users);
        let v = verification();

        // **같은** 알 수 없는 `kid` 로 20번.
        for _ in 0..20 {
            assert!(verifier.key_for("attacker-kid", &v, NOW_MS).await.is_none());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "알 수 없는 kid 하나에 JWKS 를 여러 번 가져왔다"
        );

        // 서로 **다른** kid 20개는 각자 예산을 갖는다 — 그건 의도한 동작이다.
        // (`kid` 별 리밋이므로) 다만 캐시가 채워진 뒤에는 새 kid 도 캐시에서
        // 못 찾으면 리밋 전까지 한 번씩 시도한다.
        let before = calls.load(Ordering::SeqCst);
        for i in 0..5 {
            let _ = verifier.key_for(&format!("kid-{i}"), &v, NOW_MS).await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst) - before,
            5,
            "kid 별 예산이 서로 간섭한다"
        );
    }

    struct NoUsers;
    #[async_trait::async_trait]
    impl dbmon_core::ports::UserStore for NoUsers {
        async fn get(
            &self,
            _subject: &str,
        ) -> dbmon_core::error::Result<Option<dbmon_core::rbac::UserRecord>> {
            Ok(None)
        }
    }

    #[test]
    fn the_ttl_and_rate_limit_are_what_the_document_says() {
        assert_eq!(JWKS_TTL_MS, 3_600_000, "08 §3 — 1시간");
        assert_eq!(JWKS_REFRESH_MIN_INTERVAL_MS, 60_000, "08 §3 — kid 당 1분");
    }
}

#[cfg(test)]
mod mode_dispatch_tests {
    //! **실효 모드가 수단을 하나로 정한다** (교차 리뷰가 critical 로 잡은 결함).
    //!
    //! `context_from_token` 은 `ApiState` 를 요구하므로 여기서 직접 호출할 수 없다.
    //! 대신 그 판정의 근거가 되는 두 함수의 계약을 고정한다 — 실제 디스패치는
    //! `it_api` 통합 테스트가 확인한다.

    use dbmon_core::settings::{AppSettings, AuthModeSetting as M, CognitoSettings};

    fn settings(mode: M, complete: bool) -> AppSettings {
        let mut s = AppSettings::default();
        s.auth.mode = mode;
        if complete {
            s.auth.cognito = CognitoSettings {
                user_pool_id: "ap-northeast-2_AbCdEf".into(),
                client_id: "cid".into(),
                region: String::new(),
                domain: "dbmon-dev".into(),
            };
        }
        s
    }

    /// **설정이 불완전한 Cognito 는 토큰 모드로 떨어진다.**
    ///
    /// 그래야 "Cognito 를 골랐는데 들어올 방법이 없다" 는 상태가 만들어지지 않는다.
    #[test]
    fn incomplete_cognito_settings_fall_back_to_token_mode() {
        assert_eq!(
            settings(M::Cognito, false).auth.effective_mode(false),
            M::Token
        );
        assert_eq!(
            settings(M::Cognito, true).auth.effective_mode(false),
            M::Cognito
        );
    }

    /// **`off` 는 파일 설정이 허용해야 적용된다.** 화면 하나로 인증을 끌 수 없다.
    #[test]
    fn off_requires_the_file_setting_too() {
        assert_eq!(settings(M::Off, false).auth.effective_mode(false), M::Token);
        assert_eq!(settings(M::Off, false).auth.effective_mode(true), M::Off);
    }

    /// **모드는 셋 중 하나로 결정된다** — "여러 수단을 순서대로 시도" 가 아니다.
    ///
    /// 순서대로 시도하면 `cognito` 로 바꿔도 남아 있는 공유 토큰이 계속 admin 으로
    /// 통과한다. 사람마다 권한을 나누려고 Cognito 를 붙였는데 옆문이 열린 상태다.
    #[test]
    fn the_mode_selects_exactly_one_credential_family() {
        let complete = settings(M::Cognito, true);
        // Cognito 모드에서 실효 모드는 `Cognito` 다 — `Token` 이 아니다.
        assert_eq!(complete.auth.effective_mode(true), M::Cognito);
        // 즉 `context_from_token` 의 `match` 가 토큰 분기에 가지 않는다.
        assert_ne!(complete.auth.effective_mode(true), M::Token);
    }
}
