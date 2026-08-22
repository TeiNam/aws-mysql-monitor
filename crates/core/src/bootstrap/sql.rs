//! 부트스트랩 SQL 조립 ([08 §7.1](../../../../docs/08-security-auth.md) T-18, T-19).
//!
//! # 여기가 문자열로 SQL 을 만드는 유일한 곳이고, 마스터 권한으로 실행된다
//!
//! 그래서 3중 방어를 전부 이 모듈 안에서 적용한다:
//!
//! 1. 화이트리스트 — 호출부([`super::Desired::validate`])가 이름을 먼저 검사한다
//! 2. 인용 유틸 — [`crate::ident`] 의 `quote_ident`/`quote_literal` 만 쓴다
//! 3. 문장 구분자 검사 — 조립 결과를 [`crate::ident::has_statement_break`] 로 확인하고
//!    걸리면 **문장을 만들지 않는다**
//!
//! # T-19 — 비밀번호 폴백 문장 **자체가 비밀이다**
//!
//! `CREATE USER … IDENTIFIED BY '<pw>'` 의 전문은 살아 있는 DB 비밀번호다. 그런데 이
//! 문장은 설계상 (a) `plan` 응답 (b) 감사 레코드 (c) WS 진행 메시지에 들어간다 →
//! HTTP 응답 → 브라우저 메모리 → DynamoDB → Iceberg 3년 보관까지 남는다.
//!
//! 그래서 [`Statement`] 는 **원문을 밖으로 주는 방법을 하나만** 갖는다
//! ([`Statement::to_execute`], 이름으로 의도가 드러난다). `Display`·`Debug`·`Serialize`
//! 는 전부 마스킹된 형태를 낸다 — 실수로 로그·응답에 흘릴 경로를 타입으로 막는다.

use crate::ident::{has_statement_break, quote_literal};

use super::grants::GrantScope;
use super::{AuthMethod, Desired, InvalidDesired};

/// 실행할 SQL 한 문장.
///
/// 원문(`raw`)과 표시용(`redacted`)을 함께 들고 있다. 비밀이 없는 문장은 두 값이 같다.
#[derive(Clone, PartialEq, Eq)]
pub struct Statement {
    raw: String,
    redacted: String,
}

impl Statement {
    /// 비밀이 없는 문장. 두 형태가 같다.
    fn plain(sql: String) -> Result<Self, InvalidDesired> {
        Self::checked(sql.clone(), sql)
    }

    /// 비밀이 든 문장. `redacted` 가 표시·저장·지문에 쓰인다.
    fn secret(raw: String, redacted: String) -> Result<Self, InvalidDesired> {
        Self::checked(raw, redacted)
    }

    /// T-18 3번 — 문장 구분자가 있으면 **만들지 않는다.**
    ///
    /// 인용 유틸을 통과한 값으로 조립했으면 여기 걸릴 것이 없다. 그래도 두는 이유는
    /// 조립 코드가 새로 생길 때마다 마지막 그물이 필요해서다.
    fn checked(raw: String, redacted: String) -> Result<Self, InvalidDesired> {
        if has_statement_break(&raw) || has_statement_break(&redacted) {
            // 이 값을 에러 메시지에 넣지 않는다 — 비밀이 들어 있을 수 있다.
            return Err(InvalidDesired::User("<statement break detected>".into()));
        }
        Ok(Self { raw, redacted })
    }

    /// **대상 DB 로 보낼 원문.** 이 함수를 부르는 곳은 실행 경로 하나여야 한다.
    ///
    /// 이름이 길고 구체적인 것이 의도다 — 코드 리뷰에서 눈에 걸려야 한다.
    pub fn to_execute(&self) -> &str {
        &self.raw
    }

    /// 화면·감사 로그·지문에 쓰는 형태. 비밀이 없다.
    pub fn redacted(&self) -> &str {
        &self.redacted
    }

    /// 이 문장에 비밀이 들어 있는가 (화면이 "복사할 수 없음" 을 표시할 근거).
    pub fn contains_secret(&self) -> bool {
        self.raw != self.redacted
    }
}

/// **마스킹된 형태만 낸다.**
impl std::fmt::Display for Statement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.redacted)
    }
}

/// `Debug` 도 마스킹한다 — 파생 `Debug` 가 비밀번호를 찍었던 실수를 반복하지 않는다.
impl std::fmt::Debug for Statement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Statement")
            .field("sql", &self.redacted)
            .field("contains_secret", &self.contains_secret())
            .finish()
    }
}

/// `Serialize` 도 마스킹한다 — API 응답·감사 레코드가 이 경로로 나간다.
impl serde::Serialize for Statement {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.redacted)
    }
}

/// 비밀번호 자리에 넣는 마스크.
const REDACTED: &str = "<redacted>";

/// `'user'@'host'` — 인용된 계정 지정자.
fn account(desired: &Desired) -> Result<String, InvalidDesired> {
    let user =
        quote_literal(&desired.user).ok_or_else(|| InvalidDesired::User(desired.user.clone()))?;
    let host =
        quote_literal(&desired.host).ok_or_else(|| InvalidDesired::Host(desired.host.clone()))?;
    Ok(format!("{user}@{host}"))
}

/// `CREATE USER` — IAM 방식은 비밀이 없고, 비밀번호 방식은 문장 자체가 비밀이다.
///
/// `IF NOT EXISTS` 를 **쓰지 않는다** (T-28). 존재 검사는 호출부가 이미 했고,
/// `IF NOT EXISTS` 는 잘못된 기존 상태를 조용히 통과시킨다 — 그게 선점 공격 통로다.
/// 계정이 그 사이에 생겼으면 이 문장이 에러를 내는 것이 맞다.
pub fn create_user(desired: &Desired) -> Result<Statement, InvalidDesired> {
    let acct = account(desired)?;
    match desired.auth {
        AuthMethod::IamDbAuth => Statement::plain(format!(
            "CREATE USER {acct} IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL"
        )),
        AuthMethod::Password => Err(InvalidDesired::User(
            "비밀번호 방식은 create_user_with_password 를 쓴다".into(),
        )),
    }
}

/// 비밀번호 폴백용 `CREATE USER` (FR-CRD-05).
///
/// `password` 는 CSPRNG 로 만든 값이고, **이 문장의 원문이 곧 그 비밀번호다.**
/// 호출부는 [`Statement::to_execute`] 를 실행 직전에만 부른다.
pub fn create_user_with_password(
    desired: &Desired,
    password: &str,
) -> Result<Statement, InvalidDesired> {
    let acct = account(desired)?;
    let quoted = safe_password_literal(password)?;
    Statement::secret(
        format!("CREATE USER {acct} IDENTIFIED BY {quoted} REQUIRE SSL"),
        format!("CREATE USER {acct} IDENTIFIED BY '{REDACTED}' REQUIRE SSL"),
    )
}

/// 비밀번호를 **알파벳 검사 + 인용** 두 단계로 통과시킨다.
///
/// 에러 메시지에 값을 넣지 않는다 — 이 값이 곧 비밀이다.
fn safe_password_literal(password: &str) -> Result<String, InvalidDesired> {
    if !is_safe_generated_password(password) {
        return Err(InvalidDesired::User(
            "생성된 비밀번호가 허용 문자 집합을 벗어났다".into(),
        ));
    }
    quote_literal(password).ok_or(InvalidDesired::User(
        "생성된 비밀번호가 인용 규칙을 통과하지 못했다".into(),
    ))
}

/// 자기 비밀번호 변경 — 로테이션 Lambda 가 쓴다 (마스터가 필요 없다).
pub fn alter_own_password(password: &str) -> Result<Statement, InvalidDesired> {
    let quoted = safe_password_literal(password)?;
    Statement::secret(
        format!("ALTER USER USER() IDENTIFIED BY {quoted}"),
        format!("ALTER USER USER() IDENTIFIED BY '{REDACTED}'"),
    )
}

/// `GRANT <권한들> ON <범위> TO 'user'@'host'`.
///
/// 권한 이름은 [`super::grants`] 가 정규화한 값이고 우리가 만든 상수에서만 온다 —
/// 그래도 대상 DB 에서 읽어 온 이름이 섞일 수 있으므로 **문자 집합을 검사한다.**
pub fn grant<'a>(
    desired: &Desired,
    scope: &GrantScope,
    privileges: impl IntoIterator<Item = &'a String>,
) -> Result<Statement, InvalidDesired> {
    let acct = account(desired)?;
    let target = scope
        .render()
        .ok_or_else(|| InvalidDesired::Schema(format!("{scope:?}")))?;

    let mut names: Vec<&str> = Vec::new();
    for p in privileges {
        if !is_valid_privilege_name(p) {
            return Err(InvalidDesired::Schema(format!("권한 이름 {p:?}")));
        }
        names.push(p.as_str());
    }
    if names.is_empty() {
        return Err(InvalidDesired::Schema("빈 권한 목록".into()));
    }
    // 정렬해 문장을 결정적으로 만든다 — 지문이 순서에 흔들리면 재검증이 오탐한다.
    names.sort_unstable();
    Statement::plain(format!("GRANT {} ON {target} TO {acct}", names.join(", ")))
}

/// 권한 이름 — 대문자와 공백만. `SELECT`, `REPLICATION CLIENT` 같은 형태.
///
/// **인용할 수 없는 자리다.** MySQL 권한 이름은 키워드이므로 백틱으로 감쌀 수 없다.
/// 그래서 문자 집합을 좁게 막는 것이 유일한 방어다.
pub fn is_valid_privilege_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c == ' ')
        && !s.starts_with(' ')
        && !s.ends_with(' ')
        && !s.contains("  ")
}

/// 생성 비밀번호 길이 (07 §2.5 — CSPRNG 32자).
pub const GENERATED_PASSWORD_LEN: usize = 32;

/// 생성 비밀번호에 쓸 문자 집합.
///
/// # 무엇을 뺐고 왜인가
///
/// | 뺀 문자 | 이유 |
/// |---|---|
/// | `'` | 문자열 리터럴 인용부호. `quote_literal` 이 이중화로 처리하지만, **인용을 두 번 거치는 경로**(예: Secrets Manager JSON → 셸 → SQL)에서 규칙이 갈린다 |
/// | `\` | `NO_BACKSLASH_ESCAPES` 에 따라 의미가 바뀐다 — [`quote_literal`] 이 아예 거부한다 |
/// | 백틱 | 식별자 인용부호. 사람이 복사해 붙이는 경로에서 혼동을 만든다 |
/// | `"` | `ANSI_QUOTES` SQL 모드에서 식별자 인용부호가 된다 |
///
/// 문서(07 §2.5)가 지목한 세 문자에 `"` 를 더했다. 남은 특수문자만으로도 32자면
/// 엔트로피가 충분하다(약 190비트).
pub const PASSWORD_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&()*+,-./:;<=>?@[]^_{|}~";

/// 생성된 비밀번호가 알파벳을 벗어나지 않았는가.
///
/// 생성기(어댑터)와 이 검사를 나눠 두면, 알파벳을 넓히는 실수가 **문장을 만들기 전에**
/// 걸린다. [`create_user_with_password`] 가 이 검사를 통과한 값만 받는다.
pub fn is_safe_generated_password(pw: &str) -> bool {
    pw.len() >= 16 && pw.bytes().all(|b| PASSWORD_ALPHABET.contains(&b))
}

/// 수동 실행용 스크립트 (M3-14, [07 §5](../../../../docs/07-credentials-bootstrap.md)).
///
/// 앱에 마스터 권한을 주지 않는 조직을 위한 경로다. 문장은 [`Statement::redacted`] 를
/// 쓴다 — 비밀번호가 든 문장은 사람이 값을 채워야 하고, 우리가 화면에 찍지 않는다.
pub fn manual_script(plan: &super::Plan) -> String {
    let mut out = String::from(
        "-- dbmon 모니터링 계정 부트스트랩 (마스터 계정으로 접속해 실행한다)\n\
         -- FLUSH PRIVILEGES 는 필요하지 않다 — GRANT 가 권한 캐시를 즉시 갱신한다.\n",
    );
    for action in &plan.actions {
        match action {
            super::Action::Sql(s) => {
                if s.contains_secret() {
                    out.push_str(
                        "-- ⚠ 다음 문장에는 비밀번호가 들어간다. \
                         <redacted> 를 직접 만든 값으로 바꿔 실행한다.\n",
                    );
                }
                out.push_str(s.redacted());
                out.push_str(";\n");
            }
            super::Action::EnableIamAuth { instance_id } => {
                out.push_str(&format!(
                    "\n-- IAM DB 인증 활성화 (SQL 이 아니다. 셸에서 실행한다)\n\
                     -- aws rds modify-db-instance --db-instance-identifier {instance_id} \\\n\
                     --   --enable-iam-database-authentication --apply-immediately\n\n"
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::PrivilegeMode;

    fn desired() -> Desired {
        Desired {
            user: "dbmon".into(),
            host: "10.1.%".into(),
            auth: AuthMethod::IamDbAuth,
            mode: PrivilegeMode::Broad,
            schemas: vec![],
        }
    }

    #[test]
    fn create_user_uses_the_iam_plugin_and_requires_ssl() {
        let s = create_user(&desired()).expect("문장");
        assert_eq!(
            s.to_execute(),
            "CREATE USER 'dbmon'@'10.1.%' IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL"
        );
        assert!(!s.contains_secret());
    }

    /// **`IF NOT EXISTS` 를 쓰지 않는다** (T-28).
    ///
    /// 그것은 잘못된 기존 상태를 조용히 통과시킨다. 계정이 계획 이후에 생겼으면 이
    /// 문장이 에러를 내는 것이 우리가 원하는 동작이다.
    #[test]
    fn create_user_does_not_use_if_not_exists() {
        let s = create_user(&desired()).expect("문장");
        assert!(
            !s.to_execute().contains("IF NOT EXISTS"),
            "선점된 계정을 조용히 통과시킨다: {}",
            s.to_execute()
        );
    }

    #[test]
    fn grant_renders_sorted_privileges() {
        let privs: Vec<String> = ["SHOW VIEW", "PROCESS", "SELECT"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let s = grant(&desired(), &GrantScope::Global, &privs).expect("문장");
        assert_eq!(
            s.to_execute(),
            "GRANT PROCESS, SELECT, SHOW VIEW ON *.* TO 'dbmon'@'10.1.%'"
        );
    }

    /// 순서가 달라도 같은 문장이 나온다 — 지문이 흔들리지 않는다.
    #[test]
    fn grant_is_order_independent() {
        let a: Vec<String> = ["SELECT", "PROCESS"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: Vec<String> = ["PROCESS", "SELECT"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            grant(&desired(), &GrantScope::Global, &a)
                .unwrap()
                .to_execute(),
            grant(&desired(), &GrantScope::Global, &b)
                .unwrap()
                .to_execute()
        );
    }

    #[test]
    fn grant_quotes_schema_and_table_identifiers() {
        let privs = vec!["SELECT".to_string()];
        let s = grant(&desired(), &GrantScope::Schema("we`ird".into()), &privs).expect("문장");
        assert_eq!(
            s.to_execute(),
            "GRANT SELECT ON `we``ird`.* TO 'dbmon'@'10.1.%'"
        );

        let t = grant(
            &desired(),
            &GrantScope::Table("mysql".into(), "innodb_table_stats".into()),
            &privs,
        )
        .expect("문장");
        assert!(t.to_execute().contains("`mysql`.`innodb_table_stats`"));
    }

    /// 권한 이름 자리는 인용할 수 없으므로 **문자 집합으로 막는다.**
    #[test]
    fn privilege_names_are_restricted_to_uppercase_words() {
        for bad in [
            "SELECT; DROP DATABASE shop",
            "SELECT ON *.* TO 'evil'@'%'",
            "select",
            "SELECT`",
            "SELECT'",
            "",
            " SELECT",
            "SELECT ",
            "SELECT  VIEW",
            "SELECT\n",
        ] {
            assert!(!is_valid_privilege_name(bad), "{bad:?} 가 통과했다");
            let privs = vec![bad.to_string()];
            assert!(
                grant(&desired(), &GrantScope::Global, &privs).is_err(),
                "{bad:?} 로 GRANT 가 만들어졌다"
            );
        }
        for ok in [
            "SELECT",
            "REPLICATION CLIENT",
            "SHOW DATABASES",
            "SET_USER_ID",
        ] {
            assert!(is_valid_privilege_name(ok), "{ok} 가 막혔다");
        }
    }

    #[test]
    fn an_empty_privilege_list_is_rejected() {
        let empty: Vec<String> = vec![];
        assert!(grant(&desired(), &GrantScope::Global, &empty).is_err());
    }

    // ── T-19: 비밀번호가 든 문장 ────────────────────────────────────────────

    /// **원문에만 비밀번호가 있고, 나머지 모든 출력 경로는 마스킹된다.**
    #[test]
    fn t19_the_password_only_appears_in_the_execution_string() {
        let pw = "Xk7-p4ssw0rd-v4lue-32ch-aaaaaaaa";
        let s = create_user_with_password(&desired(), pw).expect("문장");

        assert!(s.to_execute().contains(pw), "실행 문장에는 있어야 한다");
        assert!(s.contains_secret());

        // 나머지 경로 전부.
        assert!(!s.redacted().contains(pw), "redacted: {}", s.redacted());
        assert!(!format!("{s}").contains(pw), "Display 가 비밀번호를 찍었다");
        assert!(!format!("{s:?}").contains(pw), "Debug 가 비밀번호를 찍었다");
        let json = serde_json::to_string(&s).expect("직렬화");
        assert!(!json.contains(pw), "Serialize 가 비밀번호를 찍었다: {json}");
        assert!(json.contains(REDACTED));
    }

    #[test]
    fn rotation_uses_alter_user_on_self() {
        let s = alter_own_password("N3w-p4ssw0rd-v4lue-32chars-aaaa").expect("문장");
        assert!(
            s.to_execute()
                .starts_with("ALTER USER USER() IDENTIFIED BY '")
        );
        assert!(!s.redacted().contains("N3w"));
    }

    /// **알파벳을 벗어난 비밀번호로는 문장을 만들지 않는다.**
    ///
    /// 생성기가 제외 문자를 빠뜨리는 회귀를 여기서 잡는다. `'` 는 `quote_literal` 이
    /// 이중화로 안전하게 처리하지만 — 그래도 거부한다: 인용을 두 번 거치는 경로에서
    /// 규칙이 갈리고, 생성 값에 굳이 넣을 이유가 없다.
    #[test]
    fn a_password_outside_the_alphabet_is_refused() {
        for pw in [
            "has'quoteAAAAAAAAAAAAAAAAAAAAAAA",
            "has\\backslashAAAAAAAAAAAAAAAAAAA",
            "has\nnewlineAAAAAAAAAAAAAAAAAAAAA",
            "has`backtickAAAAAAAAAAAAAAAAAAAAA",
            "has\"dquoteAAAAAAAAAAAAAAAAAAAAAA",
            "",
            "tooshort",
        ] {
            assert!(!is_safe_generated_password(pw), "{pw:?} 를 안전하다고 봤다");
            assert!(
                create_user_with_password(&desired(), pw).is_err(),
                "{pw:?} 가 통과했다"
            );
            assert!(alter_own_password(pw).is_err(), "{pw:?} 가 통과했다");
        }
    }

    /// 알파벳 안의 값은 통과하고, **알파벳 자체가 인용을 깨지 않는다.**
    ///
    /// 알파벳에 `'` 나 `\\` 가 실수로 들어가면 이 테스트가 잡는다 — 그러면
    /// `quote_literal` 이 거부해 폴백 경로 전체가 죽는다.
    #[test]
    fn every_character_in_the_alphabet_survives_quoting() {
        let all = String::from_utf8(PASSWORD_ALPHABET.to_vec()).expect("ASCII");
        assert!(
            quote_literal(&all).is_some(),
            "알파벳에 인용을 깨는 문자가 있다"
        );
        assert!(!all.contains('\''));
        assert!(!all.contains('\\'));
        assert!(!all.contains('`'));
        assert!(!all.contains('"'));

        let pw = "Ab3!#$%&()*+,-./:;<=>?@[]^_{|}~";
        assert!(is_safe_generated_password(pw));
        assert!(create_user_with_password(&desired(), pw).is_ok());
    }

    /// T-18 3번 — 조립 결과에 문장 구분자가 있으면 만들지 않는다.
    ///
    /// 정상 경로에서는 1·2번 방어에 먼저 걸리므로, 이 검사를 직접 확인한다.
    #[test]
    fn t18_the_statement_break_check_refuses_and_does_not_leak_the_value() {
        let err =
            Statement::plain("SELECT 1; DROP DATABASE shop".into()).expect_err("거부해야 한다");
        let msg = format!("{err}");
        assert!(!msg.contains("DROP"), "에러 메시지가 값을 흘렸다: {msg}");
    }

    // ── 수동 스크립트 (M3-14) ───────────────────────────────────────────────

    #[test]
    fn the_manual_script_contains_every_statement_and_no_secrets() {
        let plan = crate::bootstrap::plan(
            &desired(),
            &crate::bootstrap::CurrentState {
                iam_auth_enabled: true,
                ..Default::default()
            },
            "inst",
            false,
        )
        .expect("계획");
        let script = manual_script(&plan);

        assert!(script.contains("CREATE USER 'dbmon'@'10.1.%'"));
        assert!(script.contains("GRANT"));
        assert!(
            !script.contains("FLUSH PRIVILEGES;"),
            "FLUSH PRIVILEGES 는 필요하지 않다"
        );
        // 문장마다 세미콜론으로 끝난다 (복사해서 실행할 수 있어야 한다).
        for line in script
            .lines()
            .filter(|l| l.starts_with("CREATE") || l.starts_with("GRANT"))
        {
            assert!(line.ends_with(';'), "세미콜론이 없다: {line}");
        }
    }

    /// IAM 인증이 꺼져 있으면 SQL 이 아니라 **CLI 명령**을 보여준다.
    #[test]
    fn the_manual_script_shows_the_cli_command_for_rds_changes() {
        let plan = crate::bootstrap::plan(
            &desired(),
            &crate::bootstrap::CurrentState::default(),
            "orders-prd-01",
            false,
        )
        .expect("계획");
        let script = manual_script(&plan);
        assert!(script.contains("aws rds modify-db-instance"));
        assert!(script.contains("orders-prd-01"));
        assert!(script.contains("--enable-iam-database-authentication"));
    }
}
