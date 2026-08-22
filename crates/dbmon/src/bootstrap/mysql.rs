//! 마스터 자격증명으로 대상 DB 를 읽고 부트스트랩 문장을 실행한다
//! (M3-4·M3-5·M3-7, [07 §2](../../../../docs/07-credentials-bootstrap.md)).
//!
//! # 왜 풀을 쓰지 않는가
//!
//! 마스터 연결은 **부트스트랩 1회당 하나**다. 풀은 연결을 살려 두는 장치이고, 살려 둔
//! 마스터 연결은 그 자체가 위험이다. `Conn` 을 만들고 끝나면 닫는다.
//!
//! # 상태 판정을 `SHOW CREATE USER` 파싱으로 하지 않는다
//!
//! 실측 출력이 이렇다:
//!
//! ```text
//! CREATE USER `dbmon`@`10.1.%` IDENTIFIED WITH 'AWSAuthenticationPlugin' AS 'RDS'
//!   REQUIRE SSL PASSWORD EXPIRE DEFAULT ACCOUNT UNLOCK PASSWORD HISTORY DEFAULT …
//! ```
//!
//! `REQUIRE SSL` 유무를 이 문자열에서 읽으려면 `REQUIRE` 뒤에 올 수 있는 모든 조합
//! (`SSL`/`X509`/`ISSUER …`/`NONE`)을 파싱해야 하고, 버전마다 뒤에 붙는 절이 다르다.
//! 대신 `mysql.user` 의 **구조화된 컬럼**을 읽는다:
//!
//! | 판정 | 컬럼 | 값 |
//! |---|---|---|
//! | 인증 플러그인 | `plugin` | `AWSAuthenticationPlugin` |
//! | `REQUIRE SSL` | `ssl_type` | `ANY` (없으면 빈 문자열) |
//! | 계정 잠금 | `account_locked` | `Y`/`N` |
//!
//! RDS 마스터가 `mysql.user` 를 읽을 수 있음을 실측으로 확인했다. 읽지 못하는 환경
//! (일부 관리형 포크)에서는 [`Introspection::user_state`] 가 오류를 내고, 계획은
//! **판정 불가로 차단**된다 — 못 읽은 상태를 정상으로 가정하지 않는다.

use dbmon_core::bootstrap::grants::{SchemaNameMode, parse_grants};
use dbmon_core::bootstrap::{CurrentState, Desired, sql as bsql};
use dbmon_core::error::{DomainError, Result};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, Row, Value};

/// `SHOW GRANTS` 가 없는 계정에 내는 오류 코드.
const ER_NONEXISTING_GRANT: u16 = 1141;

/// `mysql.user` 에서 계정 상태를 읽는다. 계정이 없으면 행이 없다.
///
/// `authentication_string` 을 **SHA2 로 해시해서** 읽는다. 원문(비밀번호 해시)은
/// 오프라인 대입 공격의 재료이므로 프로세스 메모리에도 들이지 않는다 — 우리가
/// 필요한 것은 "계획 이후에 바뀌었는가" 뿐이다.
const USER_STATE: &str = "SELECT plugin, ssl_type, account_locked, \
                          SHA2(COALESCE(authentication_string, ''), 256) \
                          FROM mysql.user WHERE user = ? AND host = ?";

/// 대상 DB 에 붙은 마스터 연결.
pub struct MasterConn {
    conn: Conn,
    /// 진단용 라벨. 엔드포인트를 그대로 쓰지 않는다(호스트명이 사업 정보일 수 있다).
    label: String,
}

impl MasterConn {
    /// 접속한다. `opts` 는 [`crate::mysql::connect::target_opts`] 가 만든다 —
    /// TLS·CA 검증이 그 함수에 들어 있고, 마스터 연결이 그것을 우회하면 안 된다.
    pub async fn connect(opts: Opts, label: impl Into<String>) -> Result<Self> {
        let label = label.into();
        let conn = Conn::new(opts)
            .await
            .map_err(|e| DomainError::Unavailable {
                dependency: "target-mysql",
                reason: format!("{label}: 마스터 연결 실패 — {e}"),
            })?;
        Ok(Self { conn, label })
    }

    /// 현재 상태를 읽는다 (M3-5).
    ///
    /// `iam_auth_enabled` 는 DB 가 아니라 `DescribeDB*` 에서 온다 — 호출부가 넘긴다.
    pub async fn introspect(
        &mut self,
        desired: &Desired,
        iam_auth_enabled: bool,
    ) -> Result<CurrentState> {
        let user_state = self.user_state(&desired.user, &desired.host).await?;
        let (user_exists, auth_plugin, requires_ssl, locked, auth_string_digest) =
            match user_state.as_ref() {
                Some(s) => (
                    true,
                    Some(s.plugin.clone()),
                    s.requires_ssl,
                    s.locked,
                    s.auth_string_digest.clone(),
                ),
                None => (false, None, false, false, None),
            };

        // **서버가 GRANT 의 DB 이름을 어떻게 해석하는지 읽는다.**
        //
        // 이걸 가정하면 조용히 망가진다: `partial_revokes = ON` 인 서버에
        // `` `a\_b` `` 를 부여하면 존재하지 않는 이름에 권한이 생기고, `GRANT` 는
        // 성공하는데 효과가 없어 다음 계획이 같은 문장을 무한히 반복한다.
        let schema_name_mode = self.schema_name_mode().await?;

        let (grants, unparsed_grants) = if user_exists {
            let lines = self.show_grants(&desired.user, &desired.host).await?;
            parse_grants(lines.iter().map(String::as_str), schema_name_mode)
        } else {
            Default::default()
        };

        // **스냅샷이 찢어지지 않았는지 확인한다** (교차 리뷰 3차가 잡은 결함).
        //
        // `mysql.user` 와 `SHOW GRANTS` 는 두 번의 왕복이다. 그 사이에 `ALTER USER`
        // 권한이 있는 내부자가 인증 수단을 바꾸면, **앞은 안전한 플러그인이고 뒤는
        // 기존 권한인 혼합 스냅샷**이 만들어진다. 그 지문은 계획과 같으므로 재검증을
        // 통과하고, 이어지는 `GRANT` 가 탈취된 계정에 적용된다.
        //
        // MySQL 에 계정 상태의 스냅샷 격리가 없으므로 **다시 읽어 같은지 본다.**
        // 다르면 상태가 움직이는 중이고, 그때는 진행하지 않는 것이 맞다.
        if user_exists {
            let again = self.user_state(&desired.user, &desired.host).await?;
            if again.as_ref() != user_state.as_ref() {
                return Err(DomainError::Conflict(format!(
                    "{}: 상태를 읽는 동안 계정이 바뀌었다 — 다시 시도한다",
                    self.label
                )));
            }
        }

        Ok(CurrentState {
            user_exists,
            auth_plugin,
            requires_ssl,
            locked,
            grants,
            unparsed_grants,
            iam_auth_enabled,
            auth_string_digest,
            schema_name_mode,
        })
    }

    /// `@@partial_revokes` 를 읽어 이름 해석 모드를 정한다.
    ///
    /// 읽을 수 없으면 **오류다.** 기본값(`Pattern`)으로 접으면 `ON` 인 서버에서
    /// 이스케이프된 이름을 부여하게 되고, 그건 조용한 무한 반복이다.
    async fn schema_name_mode(&mut self) -> Result<SchemaNameMode> {
        let rows: Vec<String> = self
            .conn
            .query("SELECT @@global.partial_revokes")
            .await
            .map_err(|e| DomainError::Unavailable {
                dependency: "target-mysql",
                reason: format!("{}: partial_revokes 를 읽을 수 없다 — {e}", self.label),
            })?;
        let raw = rows.first().ok_or_else(|| DomainError::Unavailable {
            dependency: "target-mysql",
            reason: format!("{}: partial_revokes 값이 비었다", self.label),
        })?;
        Ok(SchemaNameMode::from_partial_revokes(raw))
    }

    /// `mysql.user` 한 행. 계정이 없으면 `None`.
    ///
    /// **읽을 수 없으면 오류다.** 못 읽은 상태를 "계정 없음" 으로 보면 T-28 방어가
    /// 무력해진다 — 선점된 계정을 새 계정으로 착각하고 `CREATE USER` 로 넘어간다.
    async fn user_state(&mut self, user: &str, host: &str) -> Result<Option<UserState>> {
        let rows: Vec<Row> = self
            .conn
            .exec(USER_STATE, (Value::from(user), Value::from(host)))
            .await
            .map_err(|e| DomainError::Unavailable {
                dependency: "target-mysql",
                reason: format!("{}: mysql.user 를 읽을 수 없다 — {e}", self.label),
            })?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(UserState::from_row(&row)))
    }

    /// `SHOW GRANTS FOR 'u'@'h'` — 줄 목록.
    ///
    /// 계정이 없으면 오류 1141 이다. 여기서는 **오류가 아니라 빈 목록**으로 본다 —
    /// 존재 판정은 `mysql.user` 가 이미 했고, 그 사이에 지워졌으면 권한이 없는 것이
    /// 맞다.
    async fn show_grants(&mut self, user: &str, host: &str) -> Result<Vec<String>> {
        // 식별자가 아니라 문자열 리터럴 자리다 — `quote_literal` 을 쓴다.
        // 자리표시자(`?`)를 쓸 수 없는 구문이다.
        let u =
            dbmon_core::ident::quote_literal(user).ok_or_else(|| DomainError::InvalidInput {
                field: "monitor_user".into(),
                reason: "인용할 수 없는 계정 이름".into(),
            })?;
        let h =
            dbmon_core::ident::quote_literal(host).ok_or_else(|| DomainError::InvalidInput {
                field: "monitor_host".into(),
                reason: "인용할 수 없는 호스트 패턴".into(),
            })?;
        let stmt = format!("SHOW GRANTS FOR {u}@{h}");

        match self.conn.query::<String, _>(stmt).await {
            Ok(lines) => Ok(lines),
            Err(e) if mysql_error_code(&e) == Some(ER_NONEXISTING_GRANT) => Ok(Vec::new()),
            Err(e) => Err(DomainError::Unavailable {
                dependency: "target-mysql",
                reason: format!("{}: SHOW GRANTS 실패 — {e}", self.label),
            }),
        }
    }

    /// 문장 하나를 실행한다 (M3-7).
    ///
    /// [`bsql::Statement::to_execute`] 를 부르는 **유일한 자리**다. 다른 곳에서 원문을
    /// 꺼내면 비밀이 로그·응답으로 새는 경로가 생긴다.
    pub async fn execute(&mut self, statement: &bsql::Statement) -> Result<()> {
        self.conn
            .query_drop(statement.to_execute())
            .await
            .map_err(|e| DomainError::Unavailable {
                dependency: "target-mysql",
                // **에러 메시지에 문장을 넣을 때 마스킹된 형태를 쓴다.**
                //
                // MySQL 은 오류에 문장을 포함하지 않지만 우리가 붙이면 그때부터
                // 로그에 남는다. 비밀번호 폴백 경로에서 그게 곧 유출이다.
                reason: format!("{}: {} 실패 — {e}", self.label, statement.redacted()),
            })
    }

    /// 사용자 스키마 목록 — 화면·모드 B 화이트리스트 후보에 쓴다.
    ///
    /// 시스템 스키마는 제외한다([`dbmon_core::bootstrap::schemas`]).
    pub async fn user_schemas(&mut self) -> Result<Vec<String>> {
        let all: Vec<String> = self
            .conn
            .query("SELECT schema_name FROM information_schema.schemata")
            .await
            .map_err(|e| DomainError::Unavailable {
                dependency: "target-mysql",
                reason: format!("{}: 스키마 목록을 읽을 수 없다 — {e}", self.label),
            })?;
        Ok(all
            .into_iter()
            .filter(|s| !dbmon_core::bootstrap::schemas::is_system_schema(s))
            .collect())
    }

    /// 연결을 닫는다. 실패는 삼킨다 — 이미 할 일은 끝났다.
    pub async fn close(self) {
        let _ = self.conn.disconnect().await;
    }
}

/// `mysql.user` 한 행에서 뽑은 판정값.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserState {
    pub plugin: String,
    pub requires_ssl: bool,
    pub locked: bool,
    /// `SHA2(authentication_string, 256)`. **원문을 담지 않는다.**
    pub auth_string_digest: Option<String>,
}

impl UserState {
    fn from_row(row: &Row) -> Self {
        Self {
            plugin: column_string(row, 0).unwrap_or_default(),
            requires_ssl: is_ssl_required(column_string(row, 1).as_deref().unwrap_or("")),
            locked: is_yes(column_string(row, 2).as_deref().unwrap_or("")),
            auth_string_digest: column_string(row, 3),
        }
    }
}

/// `ssl_type` 이 `REQUIRE SSL` 이상을 뜻하는가.
///
/// # 왜 `ANY` 만이 아닌가
///
/// `REQUIRE SSL` → `ANY`, `REQUIRE X509` → `X509`, `REQUIRE ISSUER/SUBJECT/CIPHER` →
/// `SPECIFIED`. 뒤의 둘은 SSL **보다 강한** 요구다. `ANY` 만 참으로 보면
/// X509 인증서를 요구하는 계정이 "SSL 이 없다" 로 판정돼 차단된다 — 더 안전한 설정을
/// 벌주는 셈이다.
fn is_ssl_required(ssl_type: &str) -> bool {
    matches!(
        ssl_type.to_ascii_uppercase().as_str(),
        "ANY" | "X509" | "SPECIFIED"
    )
}

fn is_yes(v: &str) -> bool {
    v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes") || v == "1"
}

/// 컬럼을 문자열로 읽는다. `mysql.user` 의 컬럼은 바이너리 문자열로 올 수 있다.
fn column_string(row: &Row, idx: usize) -> Option<String> {
    match row.as_ref(idx)? {
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::NULL => None,
        other => Some(format!("{other:?}")),
    }
}

/// MySQL 서버 오류 코드를 꺼낸다.
fn mysql_error_code(e: &mysql_async::Error) -> Option<u16> {
    match e {
        mysql_async::Error::Server(s) => Some(s.code),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`REQUIRE SSL` 보다 강한 설정을 벌주지 않는다.**
    #[test]
    fn ssl_type_covers_stronger_requirements_too() {
        for t in ["ANY", "any", "X509", "SPECIFIED"] {
            assert!(is_ssl_required(t), "{t} 를 SSL 없음으로 봤다");
        }
        for t in ["", "NONE", "none"] {
            assert!(!is_ssl_required(t), "{t} 를 SSL 있음으로 봤다");
        }
    }

    #[test]
    fn account_locked_flag_is_read() {
        for y in ["Y", "y", "YES", "1"] {
            assert!(is_yes(y));
        }
        for n in ["N", "n", "NO", "0", ""] {
            assert!(!is_yes(n));
        }
    }

    /// **실측 `SHOW GRANTS` 출력을 그대로 고정한다** (골든 테스트).
    ///
    /// 문서(07 §2.6)가 "출력 형식이 안정적이고 골든 테스트로 고정할 수 있다" 고 적은
    /// 근거를 실제 값으로 만든다. 아래 네 줄은 `dbmon-seed-dev-mysql`(MySQL 8.4.11)
    /// 에서 복사한 것이다.
    #[test]
    fn real_rds_show_grants_output_parses_completely() {
        let real = [
            "GRANT PROCESS, SHOW DATABASES, REPLICATION CLIENT, SHOW VIEW ON *.* TO `dbmon`@`10.1.%`",
            "GRANT SELECT ON `shop`.* TO `dbmon`@`10.1.%`",
            "GRANT SELECT ON `sys`.* TO `dbmon`@`10.1.%`",
            "GRANT SELECT ON `performance_schema`.* TO `dbmon`@`10.1.%`",
        ];
        let (set, unparsed) = parse_grants(real, SchemaNameMode::Pattern);
        assert!(unparsed.is_empty(), "읽지 못한 줄: {unparsed:?}");

        use dbmon_core::bootstrap::grants::GrantScope;
        for p in [
            "PROCESS",
            "SHOW DATABASES",
            "REPLICATION CLIENT",
            "SHOW VIEW",
        ] {
            assert!(set.covers(&GrantScope::Global, p), "{p} 를 놓쳤다");
        }
        assert!(set.covers(&GrantScope::Schema("shop".into()), "SELECT"));
        assert!(
            !set.covers(&GrantScope::Global, "SELECT"),
            "전역 SELECT 는 없다"
        );
        assert!(!set.has_grant_option);
    }

    /// **실측 상태로 계획을 만들면 `GRANT SELECT ON *.*` 하나가 나온다.**
    ///
    /// 시드 계정은 모드 B 로 만들어져 있고 설정은 모드 A 다. 모드 A 의 전역 `SELECT`
    /// 가 기존 스키마 권한을 포함하므로 **초과 권한으로 잡히지 않아야** 한다 — 잡히면
    /// dev 에서 경고가, prd 에서 차단이 뜬다.
    #[test]
    fn the_real_seed_state_plans_exactly_one_grant() {
        use dbmon_core::bootstrap::{AuthMethod, PrivilegeMode, plan};

        let (grants, _) = parse_grants(
            [
                "GRANT PROCESS, SHOW DATABASES, REPLICATION CLIENT, SHOW VIEW ON *.* TO `dbmon`@`10.1.%`",
                "GRANT SELECT ON `shop`.* TO `dbmon`@`10.1.%`",
                "GRANT SELECT ON `sys`.* TO `dbmon`@`10.1.%`",
                "GRANT SELECT ON `performance_schema`.* TO `dbmon`@`10.1.%`",
            ],
            SchemaNameMode::Pattern,
        );
        let current = CurrentState {
            user_exists: true,
            auth_plugin: Some("AWSAuthenticationPlugin".into()),
            requires_ssl: true,
            locked: false,
            grants,
            unparsed_grants: vec![],
            iam_auth_enabled: true,
            auth_string_digest: Some("digest".into()),
            schema_name_mode: SchemaNameMode::Pattern,
        };
        let desired = Desired {
            user: "dbmon".into(),
            host: "10.1.%".into(),
            auth: AuthMethod::IamDbAuth,
            mode: PrivilegeMode::Broad,
            schemas: vec![],
        };

        let p = plan(&desired, &current, "dbmon-seed-dev-mysql", false).expect("계획");
        assert!(p.is_executable(), "차단됐다: {:?}", p.blockers);
        assert!(
            p.excess.is_empty(),
            "모드 A 가 기존 스키마 권한을 초과로 봤다: {:?}",
            p.excess
        );
        let sqls: Vec<&str> = p.statements().map(|s| s.redacted()).collect();
        assert_eq!(
            sqls,
            vec!["GRANT SELECT ON *.* TO 'dbmon'@'10.1.%'"],
            "실측 상태에서 필요한 문장은 하나다"
        );
    }

    /// 실측 `SHOW CREATE USER` 출력은 **파싱하지 않는다.** 그 결정을 테스트로 남긴다:
    /// 이 문자열에서 `REQUIRE SSL` 을 문자열 포함으로 읽으면 오판할 수 있다.
    #[test]
    fn show_create_user_is_not_the_source_of_the_ssl_verdict() {
        let real = "CREATE USER `dbmon`@`10.1.%` IDENTIFIED WITH 'AWSAuthenticationPlugin' \
                    AS 'RDS' REQUIRE SSL PASSWORD EXPIRE DEFAULT ACCOUNT UNLOCK \
                    PASSWORD HISTORY DEFAULT PASSWORD REUSE INTERVAL DEFAULT \
                    PASSWORD REQUIRE CURRENT DEFAULT";
        // 순진한 판정이 왜 위험한지: `REQUIRE` 는 두 번 나온다.
        assert_eq!(real.matches("REQUIRE").count(), 2);
        // 우리 판정은 `ssl_type` 컬럼에서 온다.
        assert!(is_ssl_required("ANY"));
    }
}
