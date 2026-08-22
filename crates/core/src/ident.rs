//! MySQL 식별자 인용 ([08 §7.1](../../../docs/08-security-auth.md) T-18).
//!
//! # 왜 core 에 있는가
//!
//! 원래 `dbmon::mysql::sql` 에만 있었다. 부트스트랩([`crate::bootstrap`])이 **마스터
//! 권한으로 실행되는 SQL** 을 조립하면서 같은 규칙이 필요해졌고, 인용 규칙이 두 벌이
//! 되는 순간 한쪽만 고쳐지는 결함이 생긴다 — 그게 T-18 이 경고한 자리다.
//!
//! 어댑터 쪽은 `pub use` 로 이 함수를 재수출한다. 구현은 하나다.

/// MySQL 식별자 인용. **백틱을 이중화하고, 제어문자·개행이 있으면 거부한다.**
///
/// 거부가 중요하다: 백틱만 이중화하면 개행이 든 이름으로 주석(`/* … */`)을 닫고 다른
/// 문장을 붙일 수 있다. RDS 식별자 규칙에 그런 이름은 없으므로 잃는 것이 없다.
pub fn quote_ident(raw: &str) -> Option<String> {
    // **문자 수로 센다.** MySQL 식별자 상한(64)은 문자 기준이라 바이트로 재면
    // 한글·이모지 이름을 실재하는데도 거부한다(`raw.len()` 은 UTF-8 바이트다).
    let chars = raw.chars().count();
    if chars == 0 || chars > 64 {
        return None;
    }
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(format!("`{}`", raw.replace('`', "``")))
}

/// 작은따옴표 문자열 리터럴 인용 — 사용자·호스트 이름 자리에 쓴다.
///
/// # 왜 백틱이 아닌가
///
/// `CREATE USER 'u'@'h'` 의 두 자리는 **식별자가 아니라 문자열 리터럴**이다. MySQL 은
/// 백틱도 받아 주지만, 리터럴 자리에 식별자 인용을 쓰면 `NO_BACKSLASH_ESCAPES`
/// 같은 SQL 모드 차이에서 규칙이 갈린다. 자리에 맞는 인용을 쓴다.
///
/// 백슬래시와 작은따옴표를 모두 이스케이프하고, 제어문자는 거부한다.
/// `NO_BACKSLASH_ESCAPES` 가 켜져 있으면 백슬래시 이스케이프가 무효가 되므로
/// **백슬래시가 들어 있으면 거부한다** — 인용 규칙이 서버 설정에 따라 갈리는 값을
/// 마스터 권한으로 실행하지 않는다.
pub fn quote_literal(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.chars().count() > 255 {
        return None;
    }
    if raw.chars().any(|c| c.is_control()) || raw.contains('\\') {
        return None;
    }
    Some(format!("'{}'", raw.replace('\'', "''")))
}

/// `GRANT ... ON <여기>` 자리의 **스키마 이름**을 인용한다.
///
/// # 왜 `quote_ident` 로는 부족한가 (실측으로 확인한 결함)
///
/// `GRANT` 의 데이터베이스 이름은 식별자가 아니라 **패턴**이다. `partial_revokes` 가
/// 꺼져 있으면(RDS MySQL 8.4.11 기본값) `_` 와 `%` 가 와일드카드로 해석된다 —
/// 백틱으로 감싸도 그렇다:
///
/// ```text
/// GRANT SELECT ON `dbmon_wild_a`.* TO 'u'@'10.1.%';
/// → 그 계정이 `dbmon_wildXa` 도 읽는다 (실측)
/// ```
///
/// 즉 `order_items` 같은 흔한 이름 하나가 **의도하지 않은 스키마까지 열어 준다.**
/// 모드 B(화이트리스트)의 요점이 정확히 그것을 막는 것이므로 치명적이다.
///
/// 백슬래시로 이스케이프하면 리터럴이 된다(실측: 이스케이프 후 의도한 스키마만 보였다):
///
/// ```text
/// GRANT SELECT ON `dbmon_wild\_a`.* → `dbmon_wild_a` 만
/// ```
///
/// `SHOW GRANTS` 는 이스케이프된 형태를 그대로 되돌려 주므로, 파서도 그것을 벗겨야
/// 한다([`unescape_grant_pattern`]).
pub fn quote_grant_schema(raw: &str) -> Option<String> {
    // 먼저 식별자 규칙을 통과해야 한다 (제어문자·길이).
    quote_ident(raw)?;
    // 백틱 이중화 + 패턴 문자 이스케이프.
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('_', "\\_")
        .replace('%', "\\%")
        .replace('`', "``");
    Some(format!("`{escaped}`"))
}

/// [`quote_grant_schema`] 의 역함수 — `SHOW GRANTS` 가 돌려준 이름에서 이스케이프를 벗긴다.
///
/// 벗기지 않으면 우리가 부여한 `` `a\_b` `` 를 이름이 `a\_b` 인 스키마로 읽고,
/// 그러면 차집합이 **매번 같은 GRANT 를 다시 요구한다**(멱등성이 깨진다).
pub fn unescape_grant_pattern(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 생성된 문장에 **문자열·식별자 리터럴 밖의 문장 구분자**가 있는가 (T-18 3중 방어의 3번).
///
/// 참이면 실행하지 않는다. 인용 유틸을 통과했으면 여기 걸릴 값이 없어야 하지만,
/// 조립 코드가 새로 생길 때마다 마지막 그물이 필요하다.
///
/// 주석 시작(`--`, `#`, `/*`)도 같이 본다. 우리 문장에는 `/* dbmon:… */` 태그가 있으므로
/// 그건 예외로 두지 않고 **애초에 붙이지 않는다** — 부트스트랩 문장은 사람이 읽고
/// 승인하는 값이라 태그가 필요 없다.
pub fn has_statement_break(sql: &str) -> bool {
    let mut chars = sql.chars().peekable();
    // 어떤 인용 안에 있는가. `None` 이면 밖이다.
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q != '`' {
                    // 이스케이프된 다음 문자를 건너뛴다 (백틱 안에서는 백슬래시가
                    // 특별하지 않다).
                    chars.next();
                } else if c == q {
                    // 같은 인용부호가 이중화되면 리터럴 안의 인용부호다.
                    if chars.peek() == Some(&q) {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                ';' | '#' => return true,
                '-' if chars.peek() == Some(&'-') => return true,
                '/' if chars.peek() == Some(&'*') => return true,
                _ => {}
            },
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_doubles_backticks() {
        assert_eq!(quote_ident("orders").as_deref(), Some("`orders`"));
        assert_eq!(quote_ident("we`ird").as_deref(), Some("`we``ird`"));
        assert_eq!(quote_ident("한글").as_deref(), Some("`한글`"));
    }

    #[test]
    fn rejects_control_characters_and_empty() {
        for bad in ["", "a\nb", "a\tb", "a\0b", "a\rb"] {
            assert_eq!(quote_ident(bad), None, "{bad:?} 를 통과시켰다");
        }
        // 64자 상한은 문자 기준이다.
        assert!(quote_ident(&"가".repeat(64)).is_some());
        assert!(quote_ident(&"a".repeat(65)).is_none());
    }

    #[test]
    fn literal_quoting_doubles_single_quotes() {
        assert_eq!(quote_literal("dbmon").as_deref(), Some("'dbmon'"));
        assert_eq!(quote_literal("10.1.%").as_deref(), Some("'10.1.%'"));
        assert_eq!(quote_literal("o'brien").as_deref(), Some("'o''brien'"));
    }

    /// **백슬래시가 들어 있으면 거부한다.**
    ///
    /// `NO_BACKSLASH_ESCAPES` 가 켜진 서버에서는 `\'` 가 이스케이프가 아니라
    /// 백슬래시 + 인용부호 종료다 — 즉 같은 문자열이 서버 설정에 따라 다르게
    /// 파싱된다. 마스터 권한으로 실행할 문장에 그런 값을 넣지 않는다.
    #[test]
    fn literal_quoting_refuses_backslashes() {
        for bad in ["a\\b", "\\", "a\\'; DROP", ""] {
            assert_eq!(quote_literal(bad), None, "{bad:?} 를 통과시켰다");
        }
    }

    /// T-18 3번 — 인젝션 페이로드 표.
    #[test]
    fn statement_breaks_are_detected_outside_quotes() {
        for sql in [
            "CREATE USER 'x'@'%'; DROP DATABASE shop",
            "GRANT SELECT ON `a`.* TO 'u'@'h' -- comment",
            "GRANT SELECT ON `a`.* TO 'u'@'h' # comment",
            "GRANT SELECT ON `a`.* TO 'u'@'h' /* comment */",
        ] {
            assert!(has_statement_break(sql), "놓쳤다: {sql}");
        }
    }

    /// **인용 안의 구분자는 문장을 끊지 않는다.** 여기서 거짓 양성이 나면
    /// 정상적인 이름(예: 호스트 패턴)이 실행을 막는다.
    #[test]
    fn quoted_separators_are_not_statement_breaks() {
        for sql in [
            "CREATE USER 'a;b'@'%'",
            "GRANT SELECT ON `a;b`.* TO 'u'@'h'",
            "GRANT SELECT ON `a--b`.* TO 'u'@'h'",
            "GRANT SELECT ON `a/*b`.* TO 'u'@'h'",
            "CREATE USER 'o''brien'@'%'",
            "GRANT SELECT ON `a``b`.* TO 'u'@'h'",
            "GRANT PROCESS ON *.* TO 'dbmon'@'10.1.%'",
        ] {
            assert!(!has_statement_break(sql), "거짓 양성: {sql}");
        }
    }

    /// **`GRANT` 의 스키마 이름에서 `_`·`%` 를 이스케이프한다.**
    ///
    /// 실측: `GRANT SELECT ON `dbmon_wild_a`.*` 가 `dbmon_wildXa` 까지 부여했고,
    /// 이스케이프 후에는 의도한 스키마만 부여했다.
    #[test]
    fn grant_schema_names_escape_pattern_wildcards() {
        assert_eq!(
            quote_grant_schema("order_items").as_deref(),
            Some("`order\\_items`"),
            "밑줄이 와일드카드로 남았다"
        );
        assert_eq!(quote_grant_schema("a%b").as_deref(), Some("`a\\%b`"));
        // 백슬래시 자체도 이스케이프한다 — 안 하면 사용자 이름의 `\` 가 다음 문자를
        // 삼킨다.
        assert_eq!(quote_grant_schema("a\\b").as_deref(), Some("`a\\\\b`"));
        // 백틱은 여전히 이중화한다.
        assert_eq!(quote_grant_schema("a`b").as_deref(), Some("`a``b`"));
        // 패턴 문자가 없으면 `quote_ident` 와 같다.
        assert_eq!(
            quote_grant_schema("shop").as_deref(),
            quote_ident("shop").as_deref()
        );
        // 식별자 규칙을 통과하지 못하면 거부한다.
        assert_eq!(quote_grant_schema("a\nb"), None);
        assert_eq!(quote_grant_schema(""), None);
    }

    /// **왕복이 원래 이름을 준다.** 안 그러면 차집합이 매번 같은 GRANT 를 요구한다.
    #[test]
    fn escaping_round_trips_through_show_grants() {
        for name in ["shop", "order_items", "a%b", "a_b_c", "we`ird", "한글_이름"] {
            let quoted = quote_grant_schema(name).expect("인용");
            // `SHOW GRANTS` 가 돌려주는 형태: 백틱을 벗긴 안쪽.
            let inner = quoted
                .strip_prefix('`')
                .and_then(|s| s.strip_suffix('`'))
                .expect("백틱")
                .replace("``", "`");
            assert_eq!(
                unescape_grant_pattern(&inner),
                name,
                "{name}: 왕복이 깨졌다 (인용: {quoted})"
            );
        }
    }

    /// 인용을 통과한 값으로 조립한 문장은 구분자 검사도 통과한다 —
    /// **두 방어선이 서로를 부정하지 않는다**(그러면 정상 입력이 전부 막힌다).
    #[test]
    fn quoted_values_always_survive_the_break_check() {
        for name in ["shop", "we`ird", "a;b", "a--b", "한글"] {
            let ident = quote_ident(name).expect("인용");
            let sql = format!("GRANT SELECT ON {ident}.* TO 'dbmon'@'10.1.%'");
            assert!(!has_statement_break(&sql), "{name}: {sql}");
        }
    }
}
