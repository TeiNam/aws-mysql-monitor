//! 대상 접속 옵션 조립 (M2-6, [07 §3.3](../../../../.claude/docs/07-credentials-bootstrap.md)).
//!
//! # RDS CA 번들을 바이너리에 심는다
//!
//! `include_bytes!` 로 임베드하고 **내장 루트를 끈다.** RDS 인증서는 Amazon RDS CA 가
//! 서명하므로 공개 CA 루트를 신뢰할 이유가 없다 — 켜 두면 신뢰 범위만 넓어진다.
//!
//! # 인증서 검증을 끄는 설정 항목을 만들지 않는다 (NFR-S-03)
//!
//! "만들면 언젠가 쓰인다." 대신 **위조할 수 없는 속성**에서 로컬 예외를 유도한다:
//!
//! ```text
//! 평문 접속 허용 = (deployment_env == dev)  AND  (호스트가 루프백)
//! ```
//!
//! 둘 다 필요하다. RDS 엔드포인트는 루프백일 수 없고, prd 배포는 첫 조건에서 걸린다.
//! 설정 한 줄로 프로덕션 TLS 를 끌 수 있는 경로가 생기지 않는다.
//! DynamoDB 엔드포인트 재지정에 쓴 것과 같은 방식이다(`config.rs`).
//!
//! 로컬 컨테이너는 자체 서명 인증서를 자동 생성하므로 이 예외가 없으면 **수집 루프를
//! 로컬에서 한 번도 돌려 볼 수 없다** — 이 프로젝트의 로컬 우선 원칙이 무너진다.

use dbmon_core::env::Env;
use dbmon_core::error::{DomainError, Result};
use dbmon_core::instance::Instance;
use mysql_async::{Opts, OptsBuilder, SslOpts};

/// RDS 글로벌 CA 번들. `truststore.pki.rds.amazonaws.com/global/global-bundle.pem`.
///
/// 갱신하면 [`CA_BUNDLE_EARLIEST_EXPIRY_DAY`] 도 함께 고쳐야 한다 —
/// `scripts/check-ca-bundle.sh` 가 둘의 일치를 확인한다.
pub const RDS_CA_BUNDLE: &[u8] = include_bytes!("../../assets/rds-global-bundle.pem");

/// 번들에서 **가장 이른** 인증서 만료일 (`YYYY-MM-DD`).
///
/// # 왜 기동 시 파싱하지 않는가
///
/// X.509 파서를 바이너리에 넣지 않기로 했다. 만료일은 번들이 바뀔 때만 바뀌므로
/// **오프라인에서 계산해 상수로 박고**, `scripts/check-ca-bundle.sh` 가 CI 에서
/// 번들과 이 상수의 일치를 확인한다. 기동 시에는 이 상수와 현재 시각만 비교한다.
///
/// 문서(07 §3.3)는 "기동 시 계산" 이라고 적었지만, 파서를 들이는 대가보다
/// CI 검사가 싸다. 관측 결과는 같다 — 90일 미만이면 경고가 나온다.
pub const CA_BUNDLE_EARLIEST_EXPIRY_DAY: &str = "2061-05-18";

/// 번들의 SHA-256. **번들을 갱신하면 이 값도 함께 고친다.**
///
/// # 왜 만료일만으로는 부족한가
///
/// `with_disable_built_in_roots(true)` 로 신뢰를 좁힌 대가로 **이 파일 하나가 유일한
/// 트러스트 앵커 집합**이 됐다. 공격은 삭제가 아니라 **추가**다 — 자기 CA 를 번들에
/// 한 장 덧붙이면 모든 대상 접속을 MITM 할 수 있고, `mysql_clear_password` 로
/// IAM 토큰이 그쪽으로 평문 전달된다.
///
/// 그런데 만료일 검사(`CA_BUNDLE_EARLIEST_EXPIRY_DAY`)는 **추가를 통과시킨다** —
/// 만료가 더 늦은 CA 를 붙이면 최솟값이 바뀌지 않는다. 인증서 개수 하한
/// (`count > 50`)도 마찬가지다.
///
/// 다이제스트를 박으면 번들 변경이 **반드시 한 줄 상수 변경을 수반**한다. 리뷰어가
/// 볼 대상이 165KB diff 가 아니라 한 줄이 된다.
pub const RDS_CA_BUNDLE_SHA256: &str =
    "e5bb2084ccf45087bda1c9bffdea0eb15ee67f0b91646106e466714f9de3c7e3";

/// 번들에 든 인증서 개수. 개수만으로는 부족하지만(위 참조) 다이제스트와 함께 두면
/// "무엇이 바뀌었나" 를 즉시 말해 준다.
pub const RDS_CA_BUNDLE_CERT_COUNT: usize = 108;

/// 90일 미만이면 경고 (07 §3.3).
pub const CA_WARN_DAYS: i64 = 90;
/// 30일 미만이면 critical.
pub const CA_CRITICAL_DAYS: i64 = 30;

/// CA 번들 만료 임박 여부. 기동 시 1회 판정한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaBundleHealth {
    Ok { days_left: i64 },
    Warn { days_left: i64 },
    Critical { days_left: i64 },
    Expired,
}

/// 번들 상태를 판정한다. **순수 함수** — `now_ms` 를 받는다.
pub fn ca_bundle_health(now_ms: i64) -> CaBundleHealth {
    ca_bundle_health_for(now_ms, CA_BUNDLE_EARLIEST_EXPIRY_DAY)
}

/// 만료일을 **인자로** 받는 형태.
///
/// 상수를 읽는 형태만 두면 "상수가 깨지면 fail-closed" 를 테스트할 수 없다 —
/// 실제로 그 테스트가 `ca_bundle_health` 를 부르지도 않는 공허한 것이었다
/// (2차 리뷰가 지적).
pub fn ca_bundle_health_for(now_ms: i64, expiry_day: &str) -> CaBundleHealth {
    let Some(expiry_ms) = day_to_epoch_ms(expiry_day) else {
        // 상수가 깨졌다. 만료로 취급한다 — fail-closed.
        return CaBundleHealth::Expired;
    };
    let days_left = (expiry_ms - now_ms).div_euclid(86_400_000);
    match days_left {
        d if d <= 0 => CaBundleHealth::Expired,
        d if d < CA_CRITICAL_DAYS => CaBundleHealth::Critical { days_left: d },
        d if d < CA_WARN_DAYS => CaBundleHealth::Warn { days_left: d },
        d => CaBundleHealth::Ok { days_left: d },
    }
}

/// `YYYY-MM-DD` → epoch ms (UTC 자정). 날짜 라이브러리를 쓰지 않는다.
fn day_to_epoch_ms(day: &str) -> Option<i64> {
    let mut parts = day.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // 그레고리력 → 율리우스 일수 (Howard Hinnant 의 days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400_000)
}

/// 대상 인스턴스에 붙을 `Opts` 를 만든다.
///
/// `secret` 은 IAM 토큰이거나 폴백 비밀번호다. 어느 쪽인지 이 함수는 모른다 —
/// MySQL 프로토콜에서 둘 다 비밀번호 자리에 들어간다.
///
/// `via` 는 **접속 주소만** 바꾼다 (SSM 터널, dev 전용 —
/// `collector.target_endpoint_overrides`). 등록부의 엔드포인트는 그대로 남고
/// IAM 토큰도 그 주소로 서명된 채로 들어온다 — RDS 가 검증하는 대상이 그것이다.
pub fn target_opts(
    instance: &Instance,
    db_user: &str,
    secret: &str,
    deployment_env: Env,
    via: Option<(&str, u16)>,
) -> Result<Opts> {
    let registry_host = instance
        .endpoint
        .as_deref()
        .ok_or_else(|| DomainError::InvalidInput {
            field: "endpoint".into(),
            reason: format!(
                "{} 에 엔드포인트가 없다 — 생성 중이거나 정지 상태다",
                instance.id.as_str()
            ),
        })?;
    // 터널을 쓰면 그 주소로 붙는다. **엔드포인트가 없는 인스턴스는 여전히 오류다** —
    // 터널만 있고 대상이 정지 상태인 것을 성공으로 볼 이유가 없다.
    let (host, port) = match via {
        Some((h, p)) => (h, p),
        None => (registry_host, instance.port),
    };

    // **엔드포인트 형태를 확인한다 (등록부를 신뢰하지 않는 두 번째 방어선).**
    //
    // `with_disable_built_in_roots(true)` 는 신뢰를 좁히지만 "Amazon RDS CA 가 서명한
    // **아무** 호스트" 까지만 좁힌다 — 다른 AWS 고객의 RDS 인스턴스도 그 CA 로
    // 서명돼 있으므로 TLS 검증을 정상 통과한다.
    //
    // `endpoint` 는 DynamoDB 등록부를 왕복한다. 등록부에 쓸 수 있는 공격자가 이 값을
    // 자기 계정의 RDS 로 바꾸면, IAM 경로는 토큰이 그 호스트로 서명되므로 자기
    // 보호가 되지만 **고정 비밀번호 경로는 평문으로 전달된다.**
    //
    // 그래서 탐색이 넣을 때(`aws::discovery`)와 쓸 때(여기) 두 번 본다.
    if !is_valid_target_host(host, deployment_env) {
        return Err(DomainError::InvalidInput {
            field: "endpoint".into(),
            reason: format!("{}: 대상 호스트 형태가 아니다", instance.id.as_str()),
        });
    }

    let builder = OptsBuilder::default()
        .ip_or_hostname(host.to_string())
        .tcp_port(port)
        .user(Some(db_user.to_string()))
        .pass(Some(secret.to_string()))
        // **DB 를 고정하지 않는다.** 대상마다 스키마가 다르고, 우리 쿼리는
        // `information_schema`·`performance_schema` 만 본다.
        .db_name(None::<String>);
    // ⚠ `secure_auth(false)` 를 쓰지 않는다. **이름이 오해를 부르는 손잡이다.**
    //
    // `mysql_async` 에서 `secure_auth` 는 "`mysql_old_password` 플러그인을 막는가"
    // 이고 기본값이 `true` 다. `false` 로 두면 얻는 것 없이 pre-4.1 의 깨진
    // 8바이트 스크램블을 **다시 허용하는 다운그레이드**만 열린다.
    //
    // `mysql_clear_password`(IAM 토큰이 요구하는 것)를 켜는 것은
    // `enable_cleartext_plugin` 이고 기본값이 `false` 다 — 꺼진 상태에서 서버가
    // auth switch 를 보내면 `DriverError::CleartextPluginDisabled` 로 즉시 끊긴다.
    //
    // 처음에 이 둘을 혼동해서 **IAM DB Auth 가 한 커넥션도 성립할 수 없는** 상태로
    // 배선했다. 로컬은 `caching_sha2_password` 라 이 분기를 타지 않아 가려졌고,
    // 통합 테스트도 고정 비밀번호를 쓴다. 실제 RDS 에 붙어야 드러난다.

    let builder = match tls_mode(host, deployment_env) {
        TlsMode::RdsCa => builder
            .ssl_opts(Some(
                SslOpts::default()
                    // **RDS CA 만 신뢰한다.** 내장 루트를 끄면 신뢰 범위가 좁아진다.
                    //
                    // `PathOrBuf` 는 `mysql_async` 가 재수출하지 않으므로 타입을 적을
                    // 수 없다. `From<&'static [u8]>` 이 있어 `.into()` 로 넘긴다 —
                    // 파일로 떨어뜨리지 않으므로 디스크에 CA 가 남지 않는다.
                    .with_root_certs(vec![RDS_CA_BUNDLE.into()])
                    .with_disable_built_in_roots(true),
            ))
            // **이 팔 안에 두는 것이 강제 지점이다.**
            //
            // RDS 의 `AWSAuthenticationPlugin` 은 `mysql_clear_password` 로 스위치한다.
            // 즉 토큰이 평문으로 전송되므로 **검증된 TLS 위에서만** 켜야 한다.
            // 주석으로 "TLS 위에서만 쓰인다" 고 적는 대신 위치로 강제한다 —
            // 주석은 지켜지지 않고 위치는 지켜진다.
            .enable_cleartext_plugin(true),
        // 로컬 컨테이너. TLS 를 쓰지 않는다 — 자체 서명 인증서를 신뢰하는 것보다
        // 아예 쓰지 않는 것이 정직하고, 루프백 밖으로 나가지 않는다.
        //
        // **평문 구간에서는 cleartext 플러그인을 켜지 않는다.** 켜면 dev 비밀번호가
        // 암호화 없이 전송된다. 로컬 MySQL 은 `caching_sha2_password` 를 쓰므로
        // 필요도 없다.
        TlsMode::PlaintextLoopback => {
            // ⚠ **여기서 길이를 막지 않으면 드라이버가 패닉한다.**
            //
            // `caching_sha2_password` 전체 인증은 비밀을 서버 공개키로 RSA
            // 암호화한다. IAM DB Auth 토큰은 약 1000바이트라 RSA 블록에 들어가지
            // 않고, `mysql_common` 이 `assert!` 로 **패닉**한다
            // (`crypto/rsa.rs`: "message too long"). tokio 워커 스레드가 죽는다.
            //
            // 실제로 이걸 만들었다: `DBMON_TARGET_PASSWORD` 를 안 주면 dev 도
            // IAM 폴백을 타고, 로컬 MySQL 에 붙는 순간 프로세스가 패닉했다.
            // 오류로 바꿔야 원인을 읽을 수 있다.
            if !plaintext_secret_is_sendable(secret.len()) {
                return Err(DomainError::InvalidInput {
                    field: "target_secret".into(),
                    reason: format!(
                        "평문 접속에 {}바이트 비밀을 보낼 수 없다 (RSA 한도 {MAX_PLAINTEXT_SECRET_LEN}). \
                         IAM 토큰을 로컬 MySQL 에 쓰려 한 것으로 보인다 — \
                         로컬 개발은 {} 환경변수로 비밀번호를 준다",
                        secret.len(),
                        crate::aws::auth_token::TARGET_PASSWORD_ENV
                    ),
                });
            }
            builder.ssl_opts(None)
        }
    };

    Ok(Opts::from(builder))
}

/// `caching_sha2_password` 전체 인증이 RSA 로 암호화할 수 있는 최대 비밀 길이.
///
/// RSA-2048 블록 256바이트 − PKCS#1 v1.5 패딩 11바이트 = 245.
/// MySQL 은 비밀 뒤에 NUL 을 붙여 암호화하므로 실제로는 1바이트 더 줄지만,
/// 판정 목적(IAM 토큰 ~1000바이트 vs 비밀번호 수십 바이트)에는 차이가 없다.
const MAX_PLAINTEXT_SECRET_LEN: usize = 245;

/// 평문(TLS 없음) 접속에 이 길이의 비밀을 보낼 수 있는가.
///
/// **순수 함수로 둔다** — 드라이버 패닉을 재현하지 않고 판정을 검증할 수 있어야 한다.
pub fn plaintext_secret_is_sendable(len: usize) -> bool {
    len <= MAX_PLAINTEXT_SECRET_LEN
}

/// RDS 엔드포인트 도메인 접미.
const RDS_HOST_SUFFIX: &str = ".rds.amazonaws.com";

/// 접속해도 되는 호스트 형태인가.
///
/// 프로덕션 경로는 RDS 엔드포인트만 허용한다. `dev` + 루프백은 로컬 컨테이너용으로
/// 허용한다 — 그쪽은 [`tls_mode`] 가 이미 판정하는 조건과 같다.
pub fn is_valid_target_host(host: &str, deployment_env: Env) -> bool {
    if deployment_env == Env::Dev && is_loopback_host(host) {
        return true;
    }
    // 대소문자를 무시한다 — DNS 이름은 대소문자를 구분하지 않는다.
    let lower = host.to_ascii_lowercase();
    // 접미만 보면 `evil-rds.amazonaws.com` 이 통과할 수 있으므로 점을 포함해 본다.
    lower.ends_with(RDS_HOST_SUFFIX) && lower.len() > RDS_HOST_SUFFIX.len()
}

/// 이 접속에 어떤 TLS 를 쓸 것인가.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// RDS CA 번들로 검증한다. **기본이자 프로덕션 경로.**
    RdsCa,
    /// 평문. `dev` 배포 + 루프백 호스트일 때만.
    PlaintextLoopback,
}

/// TLS 판정. **두 조건이 모두 참일 때만** 평문이다.
///
/// `deployment_env` 하나만 보면 dev 배포가 실제 RDS 에 평문으로 붙을 수 있다.
/// 호스트만 보면 prd 배포에 루프백 프록시를 세우는 경로가 생긴다.
pub fn tls_mode(host: &str, deployment_env: Env) -> TlsMode {
    if deployment_env == Env::Dev && is_loopback_host(host) {
        return TlsMode::PlaintextLoopback;
    }
    TlsMode::RdsCa
}

/// 호스트가 루프백인가. **접두 비교로 판정하지 않는다.**
///
/// `127.0.0.1.attacker.example` 같은 이름이 통과하면 안 된다 — 설정 검증에서 실제로
/// 그 구멍을 만들었다가 테스트가 잡았다. 주소를 파싱해 `is_loopback()` 에 맡긴다.
pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // `trim_*_matches` 는 대괄호를 **반복** 제거해 `[[::1]]` 도 통과시킨다.
    // `strip_*` 는 한 겹만 벗기고 짝이 맞아야 한다 — 의도를 코드가 말한다.
    let trimmed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    trimmed
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::instance::InstanceState;

    fn instance(endpoint: Option<&str>) -> Instance {
        let raw = crate::aws::discovery::RawDbInstance {
            identifier: "orders-01".into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: "ap-northeast-2".into(),
            endpoint_address: endpoint.map(str::to_string),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        crate::aws::discovery::to_instance(
            &raw,
            "123456789012",
            &dbmon_core::env::EnvMapping::default(),
            1_000,
        )
        .expect("매핑")
    }

    /// **프로덕션 경로는 RDS CA 검증이다.**
    #[test]
    fn production_targets_use_the_rds_ca() {
        for env in [Env::Prd, Env::Stg, Env::Unknown, Env::Dev] {
            assert_eq!(
                tls_mode("orders-01.abc.ap-northeast-2.rds.amazonaws.com", env),
                TlsMode::RdsCa,
                "{env} 에서 RDS 호스트에 TLS 를 쓰지 않았다"
            );
        }
    }

    /// **두 조건이 모두 참일 때만 평문이다.**
    #[test]
    fn plaintext_needs_both_dev_and_loopback() {
        assert_eq!(
            tls_mode("127.0.0.1", Env::Dev),
            TlsMode::PlaintextLoopback,
            "로컬 개발 경로가 막혔다 — 수집 루프를 로컬에서 돌릴 수 없다"
        );
        // dev 인데 원격이면 TLS.
        assert_eq!(
            tls_mode("orders-01.rds.amazonaws.com", Env::Dev),
            TlsMode::RdsCa,
            "dev 배포가 실제 RDS 에 평문으로 붙는다"
        );
        // 루프백인데 prd 면 TLS.
        for env in [Env::Prd, Env::Stg, Env::Unknown] {
            assert_eq!(
                tls_mode("127.0.0.1", env),
                TlsMode::RdsCa,
                "{env} 에서 루프백이면 평문을 허용했다"
            );
        }
    }

    /// **접두 비교 구멍이 없어야 한다.** 설정 검증에서 실제로 이 구멍을 만들었다.
    #[test]
    fn loopback_lookalike_hostnames_are_not_loopback() {
        for host in [
            "127.0.0.1.attacker.example",
            "localhost.attacker.example",
            "127.0.0.1.nip.io",
            "notlocalhost",
            "0.0.0.0",
            "10.0.0.1",
        ] {
            assert_eq!(
                tls_mode(host, Env::Dev),
                TlsMode::RdsCa,
                "{host} 를 루프백으로 봤다 — 평문 접속이 열린다"
            );
        }
    }

    /// 루프백 표기 변형은 모두 인식해야 한다 — 로컬 개발이 막히면 안 된다.
    #[test]
    fn all_loopback_spellings_are_recognised() {
        for host in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST",
            "::1",
            "[::1]",
            "127.0.0.2",
        ] {
            assert_eq!(
                tls_mode(host, Env::Dev),
                TlsMode::PlaintextLoopback,
                "{host} 를 루프백으로 인식하지 못했다"
            );
        }
    }

    /// **RDS 엔드포인트가 아닌 호스트는 거부한다.**
    ///
    /// RDS CA 전용 신뢰는 "Amazon RDS CA 가 서명한 아무 호스트" 까지만 좁힌다 —
    /// 다른 AWS 고객의 인스턴스도 그 CA 로 서명돼 있다. 등록부가 오염되면
    /// 고정 비밀번호가 그쪽으로 평문 전달된다.
    #[test]
    fn only_rds_endpoints_are_accepted_in_production() {
        for bad in [
            "attacker.example",
            "evil-rds.amazonaws.com",
            "rds.amazonaws.com.attacker.example",
            ".rds.amazonaws.com",
            "127.0.0.1",
            "10.0.0.5",
        ] {
            let i = instance(Some(bad));
            assert!(
                target_opts(&i, "dbmon", "s", Env::Prd, None).is_err(),
                "{bad} 를 대상으로 받았다"
            );
        }
        // 정상 RDS 엔드포인트는 통과한다 (대소문자 무시).
        for ok in [
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
            "ORDERS-01.ABC.AP-NORTHEAST-2.RDS.AMAZONAWS.COM",
        ] {
            let i = instance(Some(ok));
            target_opts(&i, "dbmon", "s", Env::Prd, None).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
    }

    /// dev + 루프백은 허용한다 — 로컬 컨테이너가 대상이다.
    #[test]
    fn dev_loopback_hosts_are_accepted() {
        for ok in ["127.0.0.1", "localhost", "::1"] {
            let i = instance(Some(ok));
            target_opts(&i, "dbmon", "pw", Env::Dev, None).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        // prd 에서는 같은 호스트가 거부된다.
        assert!(target_opts(&instance(Some("127.0.0.1")), "dbmon", "pw", Env::Prd, None).is_err());
    }

    /// **`Opts` 의 `Debug` 는 비밀번호를 평문으로 담는다.**
    ///
    /// 직관과 반대라서 근거를 테스트로 남긴다. `mysql_async::Opts` 는 `derive(Debug)`
    /// 이고 `pass: Option<String>` 이 가려지지 않는다(같은 크레이트의
    /// `ChangeUserOpts` 는 수동 `Debug` 로 가린다 — 즉 `Opts` 는 의도적으로 안 가린다).
    ///
    /// 그래서 **`Opts`/`Pool` 을 품은 타입에 `derive(Debug)` 를 붙이면 안 된다.**
    /// `TargetMysql` 이 `Debug` 를 파생하지 않는 이유가 이것이고, 그 사실을 아래
    /// 정적 검사가 지킨다.
    #[test]
    fn opts_debug_leaks_the_password_so_never_print_it() {
        let i = instance(Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com"));
        let opts = target_opts(&i, "dbmon", "super-secret-token", Env::Prd, None).expect("옵션");
        assert!(
            format!("{opts:?}").contains("super-secret-token"),
            "mysql_async 가 `Opts` 의 Debug 를 가리도록 바뀌었다 — 이 테스트와 \
             `TargetMysql` 의 Debug 금지 주석을 갱신한다"
        );
    }

    /// **`Opts`/`Pool` 을 품은 타입은 `Debug` 를 파생하지 않는다.**
    ///
    /// 위 테스트가 보여 준 대로 그 순간 IAM 토큰이 로그로 나간다. 소스를 읽어
    /// 확인한다 — 이 크레이트에서 이미 쓰는 정적 검사 방식이다(`adapter_never_scans`).
    #[test]
    fn no_type_holding_a_pool_derives_debug() {
        let src = include_str!("mod.rs");
        let code = src.split("#[cfg(test)]").next().expect("본문");
        // `TargetMysql` 선언 앞의 attribute 를 확인한다.
        let decl = code
            .find("pub struct TargetMysql")
            .expect("TargetMysql 선언");
        let before = &code[decl.saturating_sub(200)..decl];
        assert!(
            !before.contains("derive") || !before.contains("Debug"),
            "TargetMysql 이 Debug 를 파생한다 — Opts 안의 비밀번호가 로그로 나간다"
        );
        // `{opts:?}` / `?opts` 로 찍는 곳이 없어야 한다.
        for pattern in ["{opts:?}", "?opts", "{opts:#?}"] {
            assert!(
                !code.contains(pattern),
                "`{pattern}` 이 있다 — Opts 의 Debug 는 비밀번호를 담는다"
            );
        }
    }

    /// 엔드포인트가 없으면 옵션을 만들 수 없다 — 조용히 빈 호스트로 붙으면 안 된다.
    /// **SSM 터널: 접속 주소만 바뀌고 등록부 엔드포인트는 남는다.**
    ///
    /// 토큰은 호출부에서 실제 엔드포인트로 서명된다(`main.rs`). 여기서 호스트까지
    /// 루프백으로 서명하면 RDS 가 거부하고, 원인이 IAM 정책처럼 보인다.
    #[test]
    fn a_tunnel_changes_the_connect_address_only() {
        let i = instance(Some("orders.abc.ap-northeast-2.rds.amazonaws.com"));
        let opts = target_opts(&i, "dbmon", "pw", Env::Dev, Some(("127.0.0.1", 14321)))
            .expect("터널 옵션");
        assert_eq!(opts.ip_or_hostname(), "127.0.0.1");
        assert_eq!(opts.tcp_port(), 14321);
        // 등록부 값은 그대로다 — 화면과 토큰이 같은 주소를 봐야 한다.
        assert_eq!(
            i.endpoint.as_deref(),
            Some("orders.abc.ap-northeast-2.rds.amazonaws.com")
        );

        // 터널이 없으면 등록부 주소·포트를 쓴다.
        let direct = target_opts(&i, "dbmon", "pw", Env::Dev, None).expect("직접 옵션");
        assert_eq!(
            direct.ip_or_hostname(),
            "orders.abc.ap-northeast-2.rds.amazonaws.com"
        );
        assert_eq!(direct.tcp_port(), 3306);
    }

    /// 터널이 있어도 **엔드포인트가 없는 인스턴스는 오류다.** 대상이 정지 상태인 것을
    /// 터널이 있다는 이유로 성공으로 볼 수 없다.
    #[test]
    fn a_tunnel_does_not_rescue_an_instance_without_an_endpoint() {
        let e = target_opts(
            &instance(None),
            "dbmon",
            "pw",
            Env::Dev,
            Some(("127.0.0.1", 14321)),
        )
        .expect_err("엔드포인트가 없다");
        assert!(format!("{e}").contains("엔드포인트"), "{e}");
    }

    #[test]
    fn missing_endpoint_is_an_error_not_an_empty_host() {
        let e = target_opts(&instance(None), "dbmon", "secret", Env::Prd, None)
            .expect_err("엔드포인트 없이 옵션을 만들었다");
        assert!(matches!(e, DomainError::InvalidInput { .. }), "{e:?}");
    }

    /// 옵션에 사용자·포트·호스트가 반영돼야 한다.
    #[test]
    fn opts_carry_host_port_and_user() {
        let i = instance(Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com"));
        let opts = target_opts(&i, "dbmon", "tok", Env::Prd, None).expect("옵션");
        assert_eq!(
            opts.ip_or_hostname(),
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com"
        );
        assert_eq!(opts.tcp_port(), 3306);
        assert_eq!(opts.user(), Some("dbmon"));
        assert!(opts.ssl_opts().is_some(), "프로덕션인데 TLS 가 없다");
    }

    /// **IAM 토큰은 `mysql_clear_password` 로 전송된다 — 켜져 있어야 한다.**
    ///
    /// 꺼져 있으면 서버의 auth switch 에서 `CleartextPluginDisabled` 로 끊겨
    /// **RDS IAM DB Auth 가 한 커넥션도 성립하지 못한다.** 처음에
    /// `secure_auth(false)`(전혀 다른 손잡이)를 써서 그 상태였다.
    #[test]
    fn rds_targets_enable_the_cleartext_plugin_over_tls() {
        let i = instance(Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com"));
        let opts = target_opts(&i, "dbmon", "tok", Env::Prd, None).expect("옵션");
        assert!(
            opts.enable_cleartext_plugin(),
            "cleartext 플러그인이 꺼져 있다 — IAM 토큰으로 접속할 수 없다"
        );
        assert!(opts.ssl_opts().is_some(), "평문인데 cleartext 를 켰다");
    }

    /// **평문 구간에서는 cleartext 플러그인을 켜지 않는다.**
    ///
    /// 켜면 dev 비밀번호가 암호화 없이 전송된다. TLS 여부와 cleartext 여부가
    /// 같은 `match` 팔에서 결정되는 것이 그 강제 장치다.
    #[test]
    fn plaintext_targets_never_enable_the_cleartext_plugin() {
        let i = instance(Some("127.0.0.1"));
        let opts = target_opts(&i, "dbmon", "pw", Env::Dev, None).expect("옵션");
        assert!(opts.ssl_opts().is_none());
        assert!(
            !opts.enable_cleartext_plugin(),
            "평문 구간에서 cleartext 를 켰다 — 비밀번호가 암호화 없이 전송된다"
        );
    }

    /// **IAM 토큰을 평문 접속에 쓰면 오류다 — 패닉이 아니라.**
    ///
    /// `caching_sha2_password` 전체 인증은 비밀을 RSA 로 암호화하는데 IAM 토큰
    /// (~1000바이트)은 블록에 안 들어가고 `mysql_common` 이 `assert!` 로 패닉한다.
    /// 실제로 이 상태를 만들어 tokio 워커가 죽는 것을 봤다 — 그때 로그에는
    /// "message too long" 만 남아서 원인을 읽을 수 없었다.
    #[test]
    fn an_iam_token_on_a_plaintext_target_is_an_error_not_a_panic() {
        let i = instance(Some("127.0.0.1"));
        // 실제 IAM DB Auth 토큰 길이대 (서명 쿼리 문자열이 붙어 매우 길다).
        let token = "x".repeat(1_000);
        let err = target_opts(&i, "dbmon", &token, Env::Dev, None).expect_err("통과했다");
        let msg = format!("{err}");
        assert!(
            msg.contains(crate::aws::auth_token::TARGET_PASSWORD_ENV),
            "오류가 해결 방법을 알려 주지 않는다: {msg}"
        );
    }

    /// 정상 길이 비밀번호는 계속 통과해야 한다 — 가드가 로컬 개발을 막으면 안 된다.
    #[test]
    fn ordinary_passwords_still_pass_on_plaintext_targets() {
        let i = instance(Some("127.0.0.1"));
        for pw in ["p", "dbmon-local-monitor", &"a".repeat(245)] {
            assert!(
                target_opts(&i, "dbmon", pw, Env::Dev, None).is_ok(),
                "{}바이트 비밀번호가 막혔다",
                pw.len()
            );
        }
        assert!(!plaintext_secret_is_sendable(246));
        assert!(plaintext_secret_is_sendable(245));
    }

    /// **TLS 경로에는 길이 제한이 없다** — 토큰이 암호화된 채널로 평문 전송된다.
    ///
    /// 여기까지 제한하면 프로덕션 IAM 인증이 전부 막힌다.
    #[test]
    fn tls_targets_accept_long_iam_tokens() {
        let i = instance(Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com"));
        let token = "x".repeat(1_000);
        assert!(target_opts(&i, "dbmon", &token, Env::Prd, None).is_ok());
    }

    /// **`mysql_old_password` 다운그레이드를 열지 않는다.**
    ///
    /// `secure_auth` 기본값 `true` 를 유지해야 한다. `false` 면 손상된 서버·MITM 이
    /// pre-4.1 스크램블로 스위치를 요구할 때 클라이언트가 응한다.
    #[test]
    fn the_old_password_downgrade_stays_closed() {
        for (host, env) in [
            ("orders-01.abc.ap-northeast-2.rds.amazonaws.com", Env::Prd),
            ("127.0.0.1", Env::Dev),
        ] {
            let i = instance(Some(host));
            let opts = target_opts(&i, "dbmon", "s", env, None).expect("옵션");
            assert!(
                opts.secure_auth(),
                "{host}: mysql_old_password 다운그레이드가 열려 있다"
            );
        }
    }

    /// 로컬 대상은 TLS 없이 만들어져야 한다.
    #[test]
    fn loopback_dev_target_has_no_tls() {
        let mut i = instance(Some("127.0.0.1"));
        i.state = InstanceState::Collecting;
        let opts = target_opts(&i, "dbmon", "pw", Env::Dev, None).expect("옵션");
        assert!(opts.ssl_opts().is_none());
    }

    /// **CA 번들의 다이제스트를 핀한다.**
    ///
    /// 개수 하한(`count > 50`)만 두면 **CA 를 한 장 덧붙이는 공격을 통과시킨다** —
    /// 그게 실제 공격 방향이다(삭제가 아니라 추가). 만료일 검사도 만료가 더 늦은
    /// CA 에는 반응하지 않는다. 다이제스트가 유일하게 추가를 잡는다.
    #[test]
    fn ca_bundle_matches_its_pinned_digest() {
        use sha2::{Digest, Sha256};

        let actual = Sha256::digest(RDS_CA_BUNDLE)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(
            actual, RDS_CA_BUNDLE_SHA256,
            "CA 번들이 바뀌었다 — 의도한 갱신이면 RDS_CA_BUNDLE_SHA256 과 \
             RDS_CA_BUNDLE_CERT_COUNT 를 함께 고친다. 의도하지 않았다면 \
             누가 트러스트 앵커를 추가했는지 확인한다"
        );

        let text = std::str::from_utf8(RDS_CA_BUNDLE).expect("PEM 은 UTF-8 이다");
        let count = text.matches("-----BEGIN CERTIFICATE-----").count();
        assert_eq!(
            count, RDS_CA_BUNDLE_CERT_COUNT,
            "인증서 개수가 바뀌었다 (기대 {RDS_CA_BUNDLE_CERT_COUNT}, 실제 {count})"
        );
        assert_eq!(
            count,
            text.matches("-----END CERTIFICATE-----").count(),
            "BEGIN/END 개수가 다르다 — 번들이 잘렸다"
        );
    }

    /// 번들 만료 감시가 동작해야 한다 (07 §3.3).
    #[test]
    fn ca_bundle_expiry_is_monitored() {
        let expiry = day_to_epoch_ms(CA_BUNDLE_EARLIEST_EXPIRY_DAY).expect("상수 형식");

        // 지금은 여유가 충분하다.
        assert!(matches!(
            ca_bundle_health(expiry - 200 * 86_400_000),
            CaBundleHealth::Ok { .. }
        ));
        // 90일 미만 → 경고.
        assert!(matches!(
            ca_bundle_health(expiry - 60 * 86_400_000),
            CaBundleHealth::Warn { .. }
        ));
        // 30일 미만 → critical.
        assert!(matches!(
            ca_bundle_health(expiry - 10 * 86_400_000),
            CaBundleHealth::Critical { .. }
        ));
        // 지나면 만료.
        assert_eq!(ca_bundle_health(expiry + 1), CaBundleHealth::Expired);
    }

    /// 날짜 변환이 맞아야 한다 — 틀리면 만료 감시가 조용히 무의미해진다.
    #[test]
    fn day_conversion_matches_known_epochs() {
        assert_eq!(day_to_epoch_ms("1970-01-01"), Some(0));
        assert_eq!(day_to_epoch_ms("2000-03-01"), Some(951_868_800_000));
        assert_eq!(day_to_epoch_ms("2026-08-19"), Some(1_787_097_600_000));
        // 윤년 경계.
        assert_eq!(day_to_epoch_ms("2024-02-29"), Some(1_709_164_800_000));
        // 형식이 깨지면 `None` — `ca_bundle_health` 가 fail-closed 로 만료 처리한다.
        for bad in [
            "",
            "2026-13-01",
            "2026-08",
            "2026-08-19-01",
            "abc",
            "2026-00-10",
        ] {
            assert_eq!(day_to_epoch_ms(bad), None, "{bad} 를 통과시켰다");
        }
    }

    /// 상수가 깨지면 **만료로 취급**해야 한다 (fail-closed).
    ///
    /// 처음 쓴 테스트는 `ca_bundle_health` 를 부르지 않아서 `else` 팔을
    /// `Ok { days_left: i64::MAX }` 로 바꿔도 통과했다(2차 리뷰가 지적).
    #[test]
    fn a_broken_expiry_constant_fails_closed() {
        for broken in ["깨진값", "", "2026-13-01", "not-a-date"] {
            assert_eq!(
                ca_bundle_health_for(0, broken),
                CaBundleHealth::Expired,
                "{broken:?} 에서 fail-closed 하지 않았다"
            );
        }
        // 상수 자체는 유효해야 한다.
        assert!(day_to_epoch_ms(CA_BUNDLE_EARLIEST_EXPIRY_DAY).is_some());
    }
}
