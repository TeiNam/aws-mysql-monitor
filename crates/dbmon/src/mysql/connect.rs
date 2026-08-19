//! 대상 접속 옵션 조립 (M2-6, [07 §3.3](../../../../docs/07-credentials-bootstrap.md)).
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
    let Some(expiry_ms) = day_to_epoch_ms(CA_BUNDLE_EARLIEST_EXPIRY_DAY) else {
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
pub fn target_opts(
    instance: &Instance,
    db_user: &str,
    secret: &str,
    deployment_env: Env,
) -> Result<Opts> {
    let host = instance
        .endpoint
        .as_deref()
        .ok_or_else(|| DomainError::InvalidInput {
            field: "endpoint".into(),
            reason: format!(
                "{} 에 엔드포인트가 없다 — 생성 중이거나 정지 상태다",
                instance.id.as_str()
            ),
        })?;

    let builder = OptsBuilder::default()
        .ip_or_hostname(host.to_string())
        .tcp_port(instance.port)
        .user(Some(db_user.to_string()))
        .pass(Some(secret.to_string()))
        // **DB 를 고정하지 않는다.** 대상마다 스키마가 다르고, 우리 쿼리는
        // `information_schema`·`performance_schema` 만 본다.
        .db_name(None::<String>)
        // IAM 토큰은 서버가 `mysql_clear_password` 플러그인을 요구한다.
        // TLS 위에서만 쓰이므로 평문 전송이 노출되지 않는다.
        .secure_auth(false);

    let builder = match tls_mode(host, deployment_env) {
        TlsMode::RdsCa => builder.ssl_opts(Some(
            SslOpts::default()
                // **RDS CA 만 신뢰한다.** 내장 루트를 끄면 신뢰 범위가 좁아진다.
                //
                // `PathOrBuf` 는 `mysql_async` 가 재수출하지 않으므로 타입을 적을 수
                // 없다. `From<&'static [u8]>` 이 있어 `.into()` 로 넘긴다 —
                // 파일로 떨어뜨리지 않으므로 디스크에 CA 가 남지 않는다.
                .with_root_certs(vec![RDS_CA_BUNDLE.into()])
                .with_disable_built_in_roots(true),
        )),
        // 로컬 컨테이너. TLS 를 쓰지 않는다 — 자체 서명 인증서를 신뢰하는 것보다
        // 아예 쓰지 않는 것이 정직하고, 루프백 밖으로 나가지 않는다.
        TlsMode::PlaintextLoopback => builder.ssl_opts(None),
    };

    Ok(Opts::from(builder))
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
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
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

    /// 엔드포인트가 없으면 옵션을 만들 수 없다 — 조용히 빈 호스트로 붙으면 안 된다.
    #[test]
    fn missing_endpoint_is_an_error_not_an_empty_host() {
        let e = target_opts(&instance(None), "dbmon", "secret", Env::Prd)
            .expect_err("엔드포인트 없이 옵션을 만들었다");
        assert!(matches!(e, DomainError::InvalidInput { .. }), "{e:?}");
    }

    /// 옵션에 사용자·포트·호스트가 반영돼야 한다.
    #[test]
    fn opts_carry_host_port_and_user() {
        let i = instance(Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com"));
        let opts = target_opts(&i, "dbmon", "tok", Env::Prd).expect("옵션");
        assert_eq!(
            opts.ip_or_hostname(),
            "orders-01.abc.ap-northeast-2.rds.amazonaws.com"
        );
        assert_eq!(opts.tcp_port(), 3306);
        assert_eq!(opts.user(), Some("dbmon"));
        assert!(opts.ssl_opts().is_some(), "프로덕션인데 TLS 가 없다");
    }

    /// 로컬 대상은 TLS 없이 만들어져야 한다.
    #[test]
    fn loopback_dev_target_has_no_tls() {
        let mut i = instance(Some("127.0.0.1"));
        i.state = InstanceState::Collecting;
        let opts = target_opts(&i, "dbmon", "pw", Env::Dev).expect("옵션");
        assert!(opts.ssl_opts().is_none());
    }

    /// **CA 번들이 실제로 임베드돼야 한다.** 빈 파일이면 모든 TLS 접속이 실패한다.
    #[test]
    fn ca_bundle_is_embedded_and_looks_like_a_bundle() {
        let text = std::str::from_utf8(RDS_CA_BUNDLE).expect("PEM 은 UTF-8 이다");
        let count = text.matches("-----BEGIN CERTIFICATE-----").count();
        assert!(count > 50, "인증서가 {count}개뿐이다 — 번들이 잘렸다");
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
    #[test]
    fn a_broken_expiry_constant_fails_closed() {
        // `day_to_epoch_ms` 가 `None` 인 경우를 직접 확인한다.
        assert_eq!(day_to_epoch_ms("깨진값"), None);
        // 상수 자체는 유효해야 한다.
        assert!(day_to_epoch_ms(CA_BUNDLE_EARLIEST_EXPIRY_DAY).is_some());
    }
}
