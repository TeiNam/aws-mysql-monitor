//! 토큰 스트림 → 정규 텍스트.
//!
//! # 축약 규칙이 안전한 이유
//!
//! `normalize()` 는 **원문 SQL과 `DIGEST_TEXT` 양쪽에 같은 함수로** 적용된다.
//! 따라서 여기서 정의하는 축약은 토큰 스트림의 함수이기만 하면 수렴성을 깨지 않는다.
//!
//! ```text
//! MySQL 이 IN (1,2,3) 을 IN (...) 으로 줄여도    → 우리 축약: IN ( ... )
//! MySQL 이 IN (?,?,?) 로 남겨도                  → 우리 축약: IN ( ... )
//! ```
//!
//! 어느 쪽이든 양쪽이 같은 결과에 도달한다. MySQL 의 실제 동작을 맞출 필요가 없다.

use crate::lexer::Tok;

/// 정규 텍스트 최대 길이. **인스턴스 설정과 무관한 결정론적 절단**이다
/// (`performance_schema_max_digest_length` 가 인스턴스마다 달라도 우리 해시는 같아야 한다).
pub const MAX_CANONICAL_CHARS: usize = 8192;

/// 축약 후 토큰을 공백 1칸으로 이어붙인다.
pub fn render(tokens: &[Tok]) -> (String, bool) {
    let collapsed = collapse(tokens);
    let mut out = String::new();
    for t in &collapsed {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t.render());
    }
    truncate_chars(out, MAX_CANONICAL_CHARS)
}

/// 문자 경계를 지켜 절단한다. 반환값의 두 번째는 "절단했는가".
fn truncate_chars(s: String, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s, false);
    }
    let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    (s[..cut].to_string(), true)
}

/// `IN (...)` / `VALUES (...)` 축약과 후행 세미콜론 제거.
fn collapse(tokens: &[Tok]) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Tok::Kw(k) if k == "IN" => {
                out.push(tokens[i].clone());
                i += 1;
                i += collapse_one_group(tokens, i, &mut out);
            }
            Tok::Kw(k) if k == "VALUES" => {
                out.push(tokens[i].clone());
                i += 1;
                i += collapse_value_rows(tokens, i, &mut out);
            }
            Tok::Punct(';') => i += 1, // 세미콜론은 문장 구분자이므로 버린다
            t => {
                out.push(t.clone());
                i += 1;
            }
        }
    }
    out
}

/// `( ? , ? , ? )` 한 덩어리를 `( ... )` 로 줄인다. 소비한 토큰 수를 반환한다.
/// 리터럴만으로 이뤄지지 않았으면(서브쿼리 등) 아무것도 하지 않고 0을 반환한다.
fn collapse_one_group(tokens: &[Tok], start: usize, out: &mut Vec<Tok>) -> usize {
    let Some(end) = literal_only_group(tokens, start) else {
        return 0;
    };
    out.push(Tok::Punct('('));
    out.push(Tok::Ellipsis);
    out.push(Tok::Punct(')'));
    end - start + 1
}

/// `VALUES ( ... ) , ( ... ) , ( ... )` → `VALUES ( ... )`
fn collapse_value_rows(tokens: &[Tok], start: usize, out: &mut Vec<Tok>) -> usize {
    let mut i = start;
    let mut rows = 0usize;
    loop {
        let Some(end) = literal_only_group(tokens, i) else {
            break;
        };
        rows += 1;
        i = end + 1;
        if matches!(tokens.get(i), Some(Tok::Punct(','))) {
            i += 1;
        } else {
            break;
        }
    }
    if rows == 0 {
        return 0;
    }
    out.push(Tok::Punct('('));
    out.push(Tok::Ellipsis);
    out.push(Tok::Punct(')'));
    i - start
}

/// `start` 위치가 `(` 이고 닫는 `)` 까지의 내용이 리터럴·쉼표·축약뿐이면
/// 닫는 괄호의 인덱스를 반환한다.
fn literal_only_group(tokens: &[Tok], start: usize) -> Option<usize> {
    if !matches!(tokens.get(start), Some(Tok::Punct('('))) {
        return None;
    }
    let mut i = start + 1;
    let mut saw_content = false;
    while let Some(t) = tokens.get(i) {
        match t {
            Tok::Punct(')') => return if saw_content { Some(i) } else { None },
            Tok::Placeholder | Tok::Param | Tok::Ellipsis => {
                saw_content = true;
                i += 1;
            }
            Tok::Punct(',') => i += 1,
            _ => return None, // 서브쿼리·식·컬럼 참조 → 축약하지 않는다
        }
    }
    None // 닫히지 않은 괄호
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn canon(s: &str) -> String {
        render(&Lexer::new(s).tokenize().0).0
    }

    #[test]
    fn in_list_collapses_to_ellipsis() {
        assert_eq!(
            canon("SELECT a FROM t WHERE id IN (1,2,3)"),
            canon("SELECT a FROM t WHERE id IN (...)")
        );
        assert_eq!(
            canon("SELECT a FROM t WHERE id IN (1)"),
            canon("SELECT a FROM t WHERE id IN (9,9,9,9)")
        );
    }

    #[test]
    fn in_subquery_is_not_collapsed() {
        let c = canon("SELECT a FROM t WHERE id IN (SELECT id FROM u)");
        assert!(
            c.contains("SELECT id FROM u"),
            "서브쿼리를 축약하면 안 된다: {c}"
        );
    }

    #[test]
    fn multi_row_values_collapse_to_one_group() {
        assert_eq!(
            canon("INSERT INTO t VALUES (1,2),(3,4),(5,6)"),
            canon("INSERT INTO t VALUES (7,8)")
        );
    }

    #[test]
    fn values_with_expression_is_not_collapsed() {
        let c = canon("INSERT INTO t VALUES (NOW(), 1)");
        assert!(c.contains("now"), "식이 있으면 축약하지 않는다: {c}");
    }

    #[test]
    fn trailing_semicolon_removed() {
        assert_eq!(canon("SELECT 1;"), canon("SELECT 1"));
        assert_eq!(canon("SELECT 1 ;  "), canon("SELECT 1"));
    }

    #[test]
    fn whitespace_and_comments_normalized() {
        assert_eq!(
            canon("select\n  a,\tb\nfrom   t  /* c */ where x=1"),
            canon("SELECT a, b FROM t WHERE x = 2")
        );
    }

    #[test]
    fn truncation_is_deterministic_and_char_safe() {
        // 한글(3바이트)로 경계를 만들어 UTF-8 분할 패닉이 없는지 본다.
        let long = format!("SELECT '{}' FROM t", "가".repeat(20_000));
        let (s, truncated) = render(&Lexer::new(&long).tokenize().0);
        assert!(!truncated, "리터럴은 ? 로 치환되므로 짧아진다");
        assert!(s.is_char_boundary(s.len()));

        let wide = format!(
            "SELECT {} FROM t",
            (0..4000)
                .map(|i| format!("컬럼{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let (s2, truncated2) = render(&Lexer::new(&wide).tokenize().0);
        assert!(truncated2);
        assert_eq!(s2.chars().count(), MAX_CANONICAL_CHARS);
    }

    #[test]
    fn unclosed_paren_does_not_hang() {
        // literal_only_group 이 None 을 반환해 그대로 흘려보내야 한다.
        let c = canon("SELECT a FROM t WHERE id IN (1,2");
        assert!(c.starts_with("SELECT a FROM t WHERE"), "{c}");
    }
}
