//! 마스터 자격증명 소스 (M3-1·M3-2, [07 §1](../../../../docs/07-credentials-bootstrap.md)).
//!
//! # 두 경로만 만든다
//!
//! 문서는 세 경로를 적었다: (a) RDS 관리형 마스터 시크릿, (b) 지정 ARN,
//! (c) UI 수동 입력. **(c) 를 만들지 않는다** — 사용자가 Secrets Manager 경로만
//! 쓰기로 결정했고, 수동 입력은 값이 HTTP 본문 → 서버 메모리 → WS 진행 메시지를
//! 지나는 경로를 새로 만든다. 없는 경로는 새지 않는다.
//!
//! 실제 시드가 두 경로를 모두 쓴다:
//!
//! | 인스턴스 | 경로 | 근거 |
//! |---|---|---|
//! | Aurora 클러스터 | (a) 관리형 | `manage_master_user_password = true` |
//! | RDS MySQL | (b) 지정 ARN | Terraform 이 만든 시크릿 |
//!
//! # 값을 어디에도 쓰지 않는다
//!
//! [`MasterCredentials`] 는 [`SecretString`] 을 담아 `Drop` 에서 zeroize 한다.
//! `Debug` 는 사용자명만 찍고 비밀번호는 가린다. 파일·로그·응답에 넣는 경로가 없다.

use std::time::Duration;

use aws_sdk_secretsmanager::Client as SecretsClient;
use dbmon_core::secret::{Secret, SecretString};

/// 마스터 계정 자격증명. **부트스트랩 동안만 존재한다.**
pub struct MasterCredentials {
    pub username: String,
    pub password: SecretString,
    /// 어느 경로로 얻었는가 — 감사 로그에 남긴다(값은 남기지 않는다).
    pub source: CredentialSource,
}

/// `Debug` 는 비밀번호를 찍지 않는다.
impl std::fmt::Debug for MasterCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// 자격증명을 얻은 경로 (감사 로그의 `credential_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    /// RDS 가 관리하고 자동 로테이션하는 마스터 시크릿 (`rds!db-…`).
    RdsManagedSecret,
    /// 운영자가 등록한 Secrets Manager ARN.
    TaggedSecret,
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("시크릿을 읽을 수 없다 ({name}): {source}")]
    Fetch {
        name: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("시크릿이 로테이션 중이다 (SecretStatus={status}) — 잠시 후 다시 시도한다")]
    Rotating { status: String },
    #[error("시크릿에 자격증명이 없다 ({name}): {reason}")]
    Malformed { name: String, reason: String },
    #[error("이 인스턴스에는 등록된 마스터 시크릿이 없다 ({instance_id})")]
    NoSource { instance_id: String },
    #[error("시크릿이 dbmon 용으로 태그되지 않았다 ({name}) — `dbmon=true` 태그가 필요하다")]
    NotTagged { name: String },
}

/// 시크릿 JSON 에서 사용자명·비밀번호를 뽑는다.
///
/// # 왜 키 이름을 여러 개 시도하는가
///
/// RDS 관리형 시크릿은 `{"username","password"}` 를 쓴다. 사람이 만든 시크릿은
/// `masterUsername`/`masterPassword` 나 `user`/`pass` 를 쓰기도 한다. 문서(07 §1.2)가
/// 그 순서를 규정했다.
///
/// **모두 없으면 명확한 에러를 낸다.** 여기서 조용히 빈 문자열을 쓰면 부트스트랩이
/// `Access denied` 로 실패하고, 원인이 시크릿 형식이라는 사실이 드러나지 않는다.
pub fn parse_credentials(name: &str, json: &str) -> Result<(String, SecretString), SecretError> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| SecretError::Malformed {
            name: name.to_string(),
            reason: format!("JSON 이 아니다: {e}"),
        })?;
    let obj = value.as_object().ok_or_else(|| SecretError::Malformed {
        name: name.to_string(),
        reason: "최상위가 객체가 아니다".into(),
    })?;

    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .filter_map(|k| obj.get(*k))
            .filter_map(|v| v.as_str())
            .find(|s| !s.is_empty())
            .map(str::to_string)
    };

    let username =
        pick(&["username", "masterUsername", "user"]).ok_or_else(|| SecretError::Malformed {
            name: name.to_string(),
            reason: "username / masterUsername / user 키가 모두 없다".into(),
        })?;
    let password =
        pick(&["password", "masterPassword", "pass"]).ok_or_else(|| SecretError::Malformed {
            name: name.to_string(),
            reason: "password / masterPassword / pass 키가 모두 없다".into(),
        })?;
    Ok((username, Secret::new(password)))
}

/// 시크릿이 dbmon 용으로 태그됐는가 (경로 b 의 전제).
///
/// # 왜 태그를 요구하는가
///
/// 임의 ARN 을 받으면 **이 앱이 계정의 어떤 시크릿이든 읽는 도구**가 된다. IAM 정책의
/// 리소스를 태그 조건(`secretsmanager:ResourceTag/dbmon=true`)으로 제한하고, 앱도
/// 같은 조건을 확인한다 — 정책이 느슨해져도 코드가 한 겹 막는다.
pub fn is_tagged_for_dbmon(tags: &[(String, String)]) -> bool {
    tags.iter()
        .any(|(k, v)| k == "dbmon" && v.eq_ignore_ascii_case("true"))
}

/// `SecretStatus` 가 사용 가능한 상태인가.
///
/// RDS 관리형 시크릿은 로테이션 중에 `rotating` 이 된다. 그 동안 읽은 값은 이전
/// 비밀번호일 수 있으므로 **부트스트랩을 미룬다** (07 §1.1).
pub fn is_secret_usable(status: &str) -> bool {
    status.eq_ignore_ascii_case("active")
}

/// Secrets Manager 에서 마스터 자격증명을 가져온다.
pub struct MasterSecretFetcher {
    client: SecretsClient,
    timeout: Duration,
}

/// 시크릿 조회 타임아웃. 부트스트랩은 사람이 버튼을 누르고 기다리는 동작이므로
/// 짧게 둔다 — 무한정 기다리는 화면보다 실패가 낫다.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

impl MasterSecretFetcher {
    pub fn new(client: SecretsClient) -> Self {
        Self {
            client,
            timeout: FETCH_TIMEOUT,
        }
    }

    /// 경로 (a) — RDS 관리형 마스터 시크릿.
    ///
    /// `DescribeDB*` 응답의 `MasterUserSecret` 에서 온 ARN 과 상태를 받는다. 상태를
    /// 호출부가 넘기는 이유: RDS 응답이 권위값이고, 우리가 다시 조회하면 그 사이
    /// 바뀔 수 있다.
    pub async fn from_rds_managed(
        &self,
        secret_arn: &str,
        secret_status: &str,
    ) -> Result<MasterCredentials, SecretError> {
        if !is_secret_usable(secret_status) {
            return Err(SecretError::Rotating {
                status: secret_status.to_string(),
            });
        }
        let json = self.get_value(secret_arn).await?;
        let (username, password) = parse_credentials(secret_arn, json.expose())?;
        Ok(MasterCredentials {
            username,
            password,
            source: CredentialSource::RdsManagedSecret,
        })
    }

    /// 경로 (b) — 운영자가 등록한 ARN. **태그를 확인한 뒤에만 읽는다.**
    pub async fn from_tagged_arn(&self, secret_id: &str) -> Result<MasterCredentials, SecretError> {
        let described = tokio::time::timeout(
            self.timeout,
            self.client.describe_secret().secret_id(secret_id).send(),
        )
        .await
        .map_err(|_| SecretError::Fetch {
            name: secret_id.to_string(),
            source: "DescribeSecret 타임아웃".into(),
        })?
        .map_err(|e| SecretError::Fetch {
            name: secret_id.to_string(),
            source: Box::new(e),
        })?;

        let tags: Vec<(String, String)> = described
            .tags()
            .iter()
            .filter_map(|t| Some((t.key()?.to_string(), t.value()?.to_string())))
            .collect();
        if !is_tagged_for_dbmon(&tags) {
            return Err(SecretError::NotTagged {
                name: secret_id.to_string(),
            });
        }

        let json = self.get_value(secret_id).await?;
        let (username, password) = parse_credentials(secret_id, json.expose())?;
        Ok(MasterCredentials {
            username,
            password,
            source: CredentialSource::TaggedSecret,
        })
    }

    /// `GetSecretValue` — 값을 곧바로 [`SecretString`] 에 담는다.
    ///
    /// 중간 변수에 `String` 으로 두지 않는다. 담기는 순간부터 `Drop` 이 zeroize 한다.
    async fn get_value(&self, secret_id: &str) -> Result<SecretString, SecretError> {
        let out = tokio::time::timeout(
            self.timeout,
            self.client.get_secret_value().secret_id(secret_id).send(),
        )
        .await
        .map_err(|_| SecretError::Fetch {
            name: secret_id.to_string(),
            source: "GetSecretValue 타임아웃".into(),
        })?
        .map_err(|e| SecretError::Fetch {
            name: secret_id.to_string(),
            source: Box::new(e),
        })?;

        let raw = out.secret_string().ok_or_else(|| SecretError::Malformed {
            name: secret_id.to_string(),
            reason: "SecretString 이 비어 있다 (바이너리 시크릿은 지원하지 않는다)".into(),
        })?;
        Ok(Secret::new(raw.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rds_managed_key_names_are_read() {
        let (u, p) =
            parse_credentials("s", r#"{"username":"admin","password":"hunter2"}"#).expect("파싱");
        assert_eq!(u, "admin");
        assert_eq!(p.expose(), "hunter2");
    }

    /// **실제 시드가 이 형식이다.** 마스터 사용자명이 `admin` 이 아니라
    /// `dbmonadmin` 이었고, 이름을 가정했다가 `1045` 로 실패했다.
    #[test]
    fn the_username_is_read_not_assumed() {
        let (u, _) =
            parse_credentials("s", r#"{"username":"dbmonadmin","password":"x"}"#).expect("파싱");
        assert_eq!(u, "dbmonadmin", "시크릿의 username 을 써야 한다");
    }

    #[test]
    fn alternate_key_names_are_tried_in_order() {
        let (u, p) = parse_credentials("s", r#"{"masterUsername":"m","masterPassword":"mp"}"#)
            .expect("파싱");
        assert_eq!((u.as_str(), p.expose().as_str()), ("m", "mp"));

        let (u, p) = parse_credentials("s", r#"{"user":"u","pass":"up"}"#).expect("파싱");
        assert_eq!((u.as_str(), p.expose().as_str()), ("u", "up"));

        // 표준 키가 있으면 그것이 우선이다.
        let (u, _) = parse_credentials("s", r#"{"user":"low","username":"high","password":"p"}"#)
            .expect("파싱");
        assert_eq!(u, "high");
    }

    /// **키가 없으면 조용히 빈 값을 쓰지 않는다.**
    ///
    /// 빈 사용자명으로 접속하면 `Access denied` 가 나고, 원인이 시크릿 형식이라는
    /// 사실이 드러나지 않는다.
    #[test]
    fn missing_or_empty_keys_produce_a_clear_error() {
        for json in [
            r#"{"password":"p"}"#,
            r#"{"username":"u"}"#,
            r#"{"username":"","password":"p"}"#,
            r#"{"username":"u","password":""}"#,
            r#"{}"#,
            r#"[]"#,
            "not json",
        ] {
            let err = parse_credentials("mysecret", json).expect_err("거부해야 한다");
            assert!(
                matches!(err, SecretError::Malformed { .. }),
                "{json}: {err:?}"
            );
            // 에러 메시지에 시크릿 이름은 있어도 값은 없어야 한다.
            assert!(!format!("{err}").contains("hunter"));
        }
    }

    /// 숫자·불리언 값은 문자열이 아니므로 무시한다 — 잘못된 타입으로 접속하지 않는다.
    #[test]
    fn non_string_values_are_ignored() {
        assert!(parse_credentials("s", r#"{"username":123,"password":"p"}"#).is_err());
    }

    #[test]
    fn rotating_secrets_are_not_used() {
        assert!(is_secret_usable("active"));
        assert!(is_secret_usable("ACTIVE"));
        for bad in ["rotating", "Rotating", "impaired", ""] {
            assert!(!is_secret_usable(bad), "{bad} 를 사용 가능으로 봤다");
        }
    }

    /// 태그가 없으면 읽지 않는다 — 임의 ARN 으로 다른 시크릿을 읽는 것을 막는다.
    #[test]
    fn only_dbmon_tagged_secrets_are_accepted() {
        let tagged = vec![("dbmon".to_string(), "true".to_string())];
        assert!(is_tagged_for_dbmon(&tagged));
        assert!(is_tagged_for_dbmon(&[("dbmon".into(), "TRUE".into())]));

        for tags in [
            vec![],
            vec![("dbmon".to_string(), "false".to_string())],
            vec![("Dbmon".to_string(), "true".to_string())],
            vec![("app".to_string(), "dbmon".to_string())],
        ] {
            assert!(!is_tagged_for_dbmon(&tags), "{tags:?} 가 통과했다");
        }
    }

    /// **`Debug` 가 비밀번호를 찍지 않는다.**
    #[test]
    fn debug_does_not_print_the_password() {
        let c = MasterCredentials {
            username: "dbmonadmin".into(),
            password: Secret::new("s3cr3t-master-pw".to_string()),
            source: CredentialSource::TaggedSecret,
        };
        let s = format!("{c:?}");
        assert!(!s.contains("s3cr3t"), "비밀번호가 Debug 에 찍혔다: {s}");
        // 사용자명은 비밀이 아니고 진단에 필요하다.
        assert!(s.contains("dbmonadmin"));
    }
}
