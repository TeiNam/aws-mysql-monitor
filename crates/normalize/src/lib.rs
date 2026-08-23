//! SQL 정규화 · `app_digest` · 리터럴 마스킹.
//!
//! # 왜 이 크레이트가 존재하는가
//!
//! 슬로우 쿼리는 세 소스에서 오는데 형태가 다르다([05 §3.1](../../../.claude/docs/05-collector.md)).
//!
//! ```text
//! A  information_schema.PROCESSLIST.INFO    리터럴이 살아있는 원문
//! B  events_statements_*.DIGEST_TEXT        MySQL 이 이미 ? 로 치환한 텍스트
//! C  CloudWatch 슬로우로그                   리터럴이 살아있는 원문
//! ```
//!
//! 목표는 `canonical(A) == canonical(B) == canonical(C)` 이고,
//! `app_digest = sha256(canonical)[..32]` 가 모든 그룹핑의 축이 된다
//! ([ADR-011](../../../.claude/docs/03-decisions.md)).
//!
//! **`DIGEST_TEXT` 를 글자 단위로 재현하는 것이 목표가 아니다.** 같은 함수를 양쪽에
//! 적용하므로 정규 형식은 우리가 정하면 되고, 그래서 "토큰을 공백 1칸으로 잇는다"는
//! 단순한 규칙이 성립한다. 자세한 근거는 [`lexer::fold_ident`] 문서 참조.
//!
//! # 의존성
//!
//! `sha2` 하나뿐이다. I/O·AWS·DB 의존성은 없다(설계 문서의 "의존성 0"은 이 뜻이다).

pub mod canonical;
pub mod keywords;
pub mod lexer;
pub mod postcondition;
pub mod rewrite;
pub mod stmt_type;
pub mod tables;

pub use canonical::MAX_CANONICAL_CHARS;
pub use postcondition::{LiteralResidue, check_no_literals};
pub use rewrite::{PlanQuery, plan_query};
pub use stmt_type::StatementType;

use sha2::{Digest, Sha256};

/// 정규화 알고리즘 버전. 규칙이 바뀌면 올린다.
///
/// 모든 레코드에 함께 저장한다(F29). 버전이 다른 행을 같은 집계에 섞으면
/// 다이제스트 그룹이 조용히 갈라지므로, 리포트가 경계를 표시할 수 있어야 한다.
pub const DIGEST_ALGO_VERSION: u32 = 1;

/// `app_digest` 의 16진수 길이.
pub const APP_DIGEST_HEX_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// 정규 텍스트. 리터럴은 모두 `?` 다.
    pub canonical: String,
    /// `sha256(canonical)` 앞 32 hex.
    pub app_digest: String,
    pub statement_type: StatementType,
    /// 8192자 상한에서 잘렸다.
    pub truncated: bool,
    /// 입력에 닫히지 않은 인용부호가 있었다 → 마스킹을 신뢰할 수 없다.
    pub unterminated_quote: bool,
    /// 옵티마이저 힌트를 보존했다.
    pub has_hint: bool,
}

impl Normalized {
    /// 리터럴이 정말 제거됐는지 확인한다. `masked` / `off` 정책의 저장 전 게이트.
    pub fn check_no_literals(&self) -> Result<(), LiteralResidue> {
        if self.unterminated_quote {
            return Err(LiteralResidue::UnterminatedQuote);
        }
        postcondition::check_no_literals(&self.canonical)
    }
}

/// SQL 을 정규화한다. 원문·`DIGEST_TEXT` 어느 쪽이든 같은 함수를 쓴다.
pub fn normalize(sql: &str) -> Normalized {
    let (tokens, unterminated_quote) = lexer::Lexer::new(sql).tokenize();
    let statement_type = stmt_type::classify(&tokens);
    let has_hint = tokens.iter().any(|t| matches!(t, lexer::Tok::Hint(_)));
    let (canonical, truncated) = canonical::render(&tokens);
    let app_digest = app_digest_of(&canonical);
    Normalized {
        canonical,
        app_digest,
        statement_type,
        truncated,
        unterminated_quote,
        has_hint,
    }
}

/// 정규 텍스트에서 `app_digest` 를 계산한다.
pub fn app_digest_of(canonical: &str) -> String {
    let full = Sha256::digest(canonical.as_bytes());
    let mut out = String::with_capacity(APP_DIGEST_HEX_LEN);
    for b in full.iter().take(APP_DIGEST_HEX_LEN / 2) {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    out
}

/// 표현식 문자열의 리터럴만 마스킹한다.
///
/// 용도는 **실행계획 조건식**이다(`attached_condition` 등, FR-PLN-09 / T-16).
/// 플랜 JSON 은 `` (`shop`.`orders`.`email` = 'a@b.com') `` 처럼 리터럴을 그대로 담으므로
/// SQL 본문만 마스킹하면 리터럴이 플랜 경로로 유출된다.
///
/// 반환값의 두 번째는 **후조건 통과 여부**다. `false` 면 호출자가 그 필드를 저장하지
/// 않아야 한다(마스킹을 신뢰할 수 없는 상태로 저장하는 것보다 버리는 쪽이 맞다).
pub fn mask_expression(expr: &str) -> (String, bool) {
    let (tokens, unterminated) = lexer::Lexer::new(expr).tokenize();
    let (masked, _truncated) = canonical::render(&tokens);
    let ok = !unterminated && postcondition::check_no_literals(&masked).is_ok();
    (masked, ok)
}

/// MySQL 이 `DIGEST_TEXT` 를 절단했는지 판정한다.
///
/// # `...` 접미로는 판정할 수 없다 (M1-13 실측, MySQL 8.4.11)
///
/// 초기 설계는 "잘렸을 때 `...` 로 끝난다"를 전제했다. **실제로는 토큰 중간에서 그냥 끝난다.**
///
/// ```text
/// max_digest_length=1024 → DIGEST_TEXT 992바이트, 끝: "... `id` AS `a_00047` ,"
/// max_digest_length=4096 → DIGEST_TEXT 3917바이트, 끝: "... AS `a_00193` , `id`"
/// ```
///
/// 그래서 세 신호를 함께 본다. **오탐(절단이 아닌데 절단이라 판정)은 무해**하다
/// — `mysql_digest` 매핑 경로를 타는 것뿐이다. 반대로 누락은 조용한 오분류다.
///
/// `max_digest_length` 는 자가진단이 읽어 둔 값이다. **`performance_schema_max_digest_length`
/// 가 아니다** — 두 변수는 다르고, 해시를 바꾸는 것은 전자다.
pub fn looks_truncated_by_server(digest_text: &str, max_digest_length: Option<u32>) -> bool {
    let t = digest_text.trim_end();
    if t.is_empty() {
        return false;
    }
    // ① 일부 버전·경로는 `...` 를 붙인다.
    if t.ends_with("...") {
        return true;
    }
    // ② 백틱 개수가 홀수 → 식별자 중간에서 끊겼다.
    if t.bytes().filter(|b| *b == b'`').count() % 2 == 1 {
        return true;
    }
    // ③ 문장이 끝날 수 없는 토큰으로 끝난다.
    const INCOMPLETE_TAIL: &[char] = &[
        ',', '(', '.', '+', '-', '*', '/', '%', '=', '<', '>', '&', '|', '!', '~', '^',
    ];
    if t.ends_with(INCOMPLETE_TAIL) {
        return true;
    }
    // ④ 길이가 상한에 근접했다. MySQL 은 상한보다 조금 앞에서 자른다(토큰 경계).
    matches!(max_digest_length, Some(lim) if t.len() as u32 + TRUNCATION_MARGIN_BYTES >= lim)
}

/// 절단 판정의 길이 여유. 실측에서 상한 대비 32~179바이트 앞에서 잘렸다.
const TRUNCATION_MARGIN_BYTES: u32 = 256;

#[cfg(test)]
mod tests {
    use super::*;

    // ── P1 결정론성 ────────────────────────────────────────────────────────────
    #[test]
    fn p1_deterministic() {
        let sql = "SELECT a FROM t WHERE id = 1";
        assert_eq!(normalize(sql).app_digest, normalize(sql).app_digest);
        assert_eq!(normalize(sql).app_digest.len(), APP_DIGEST_HEX_LEN);
        assert!(
            normalize(sql)
                .app_digest
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    // ── P3 멱등성: 수렴성이 성립하기 위한 필요조건 ──────────────────────────────
    #[test]
    fn p3_idempotent() {
        for sql in [
            "SELECT a FROM t WHERE id IN (1,2,3)",
            "INSERT INTO `Orders` VALUES (1,'a'),(2,'b')",
            "SELECT /*+ MAX_EXECUTION_TIME(1000) */ COUNT(*) FROM t GROUP BY b HAVING x > 1",
            "WITH c AS (SELECT 1) SELECT * FROM c JOIN d ON c.id = d.id",
            "UPDATE t SET a = 0x1F, b = DATE '2026-01-01' WHERE id = 5",
        ] {
            let once = normalize(sql);
            let twice = normalize(&once.canonical);
            assert_eq!(once.canonical, twice.canonical, "sql={sql}");
            assert_eq!(once.app_digest, twice.app_digest, "sql={sql}");
        }
    }

    // ── P4 리터럴 무관성 ───────────────────────────────────────────────────────
    #[test]
    fn p4_literal_insensitive() {
        let a = normalize("SELECT * FROM t WHERE id = 1 AND n = 'alice'");
        let b = normalize("SELECT * FROM t WHERE id = 99999 AND n = 'bob'");
        assert_eq!(a.app_digest, b.app_digest);
    }

    // ── P5 공백·주석·대소문자 무관성 ────────────────────────────────────────────
    #[test]
    fn p5_formatting_insensitive() {
        let a = normalize("select  *\n from   T\twhere ID=1 -- note\n");
        let b = normalize("/* lead */ SELECT * FROM t WHERE id = 2;");
        assert_eq!(
            a.app_digest, b.app_digest,
            "a={} b={}",
            a.canonical, b.canonical
        );
    }

    // ── P6 임의 입력에서 패닉 없음 ─────────────────────────────────────────────
    #[test]
    fn p6_no_panic_on_hostile_input() {
        for sql in [
            "",
            " ",
            "'",
            "\"",
            "`",
            "/*",
            "/*+",
            "--",
            "#",
            "0x",
            "b'",
            "X'",
            "_utf8'",
            "1e",
            "1e+",
            ".",
            "..",
            "...",
            "(",
            ")",
            ",",
            ";",
            "?",
            "SELECT 'a",
            "IN (",
            "VALUES (",
            "\\",
            "\u{0}\u{1}",
            "SELECT 가나다 FROM 테이블",
            "🙂",
        ] {
            let n = normalize(sql);
            assert_eq!(n.app_digest.len(), APP_DIGEST_HEX_LEN, "sql={sql:?}");
        }
    }

    // ── 수렴성: MySQL 이 만들어 줄 형태를 손으로 흉내낸 것 ───────────────────────
    // 실제 MySQL 대조는 골든 코퍼스(`tests/golden_corpus.rs`, M1-6)가 한다.
    #[test]
    fn converges_with_handwritten_digest_text() {
        let cases: &[(&str, &str)] = &[
            (
                "select id from orders where id = 1",
                "SELECT `id` FROM `orders` WHERE `id` = ?",
            ),
            (
                "SELECT count(*) FROM orders o JOIN items i ON o.id=i.order_id WHERE o.st='P'",
                "SELECT COUNT ( * ) FROM `orders` `o` JOIN `items` `i` ON `o` . `id` = `i` . `order_id` WHERE `o` . `st` = ?",
            ),
            (
                "select * from t where id in (1,2,3,4,5)",
                "SELECT * FROM `t` WHERE `id` IN (...)",
            ),
            (
                "insert into t (a,b) values (1,2),(3,4)",
                "INSERT INTO `t` (`a`,`b`) VALUES (...)",
            ),
        ];
        for (raw, digest_text) in cases {
            let a = normalize(raw);
            let b = normalize(digest_text);
            assert_eq!(
                a.canonical, b.canonical,
                "\nraw     = {}\ndigest  = {}",
                a.canonical, b.canonical
            );
        }
    }

    #[test]
    fn server_truncation_detected() {
        // ① `...` 접미 (구버전·일부 경로)
        assert!(looks_truncated_by_server(
            "SELECT `a` FROM `t` WHERE `id` IN (?, ?, ...",
            None
        ));
        // ② 홀수 백틱 — 식별자 중간에서 끊김 (8.4.11 실측 형태)
        assert!(looks_truncated_by_server(
            "SELECT `id` AS `a_00193` , `id",
            None
        ));
        // ③ 완결될 수 없는 토큰으로 끝남 (8.4.11 실측 형태)
        assert!(looks_truncated_by_server(
            "SELECT `id` AS `a_00047` ,",
            None
        ));
        assert!(looks_truncated_by_server("SELECT `a` + ", None));
        // ④ 길이가 상한에 근접
        let near_limit = format!("SELECT {} FROM `t`", "`c` AS `d` ".repeat(90));
        assert!(near_limit.len() > 900);
        assert!(looks_truncated_by_server(&near_limit, Some(1024)));

        // 정상 종료 형태를 절단으로 오판하면 안 된다.
        for ok in [
            "SELECT `a` FROM `t`",
            "SELECT * FROM `orders` WHERE `id` = ?",
            "INSERT INTO `t` VALUES (...)",
            "",
        ] {
            assert!(
                !looks_truncated_by_server(ok, Some(1024)),
                "{ok:?} 를 절단으로 오판했다"
            );
        }
    }

    #[test]
    fn statement_type_and_hint_flags_exposed() {
        let n = normalize("/*+ NO_ICP(t) */ SELECT 1");
        assert_eq!(n.statement_type, StatementType::Select);
        assert!(n.has_hint);
        assert!(!normalize("SELECT 1").has_hint);
    }

    #[test]
    fn unterminated_quote_fails_postcondition() {
        // 마스킹을 신뢰할 수 없으면 저장하지 않아야 한다.
        let n = normalize("SELECT * FROM t WHERE n = 'oops");
        assert!(n.unterminated_quote);
        assert_eq!(
            n.check_no_literals(),
            Err(LiteralResidue::UnterminatedQuote)
        );
    }
}
