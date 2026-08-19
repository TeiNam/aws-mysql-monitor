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
//!
//! # M1-6 실측이 바꾼 것
//!
//! 초기 설계는 `IN` 과 `VALUES` 뒤의 괄호만 축약했다. 그런데 MySQL 은 **함수 인자에도**
//! 같은 축약을 한다: `JSON_EXTRACT('{"a":1}', '$.a')` → `json_extract (...)`.
//! 인자 2개 이상일 때만 줄이는 것으로 보이지만, 그 임계값을 맞추려 하면 버전에 의존한다.
//! → **모든 "리터럴만으로 이뤄진 괄호 그룹"을 축약한다.** MySQL 이 어느 임계값을 쓰든
//! 양쪽이 같은 결과에 도달하므로 버전 의존성이 사라진다.
//!
//! 대가: `f(?)` 와 `f(?,?)` 가 같은 다이제스트로 묶인다. 인자 개수만 다른 같은 함수 호출은
//! 튜닝 관점에서 같은 대상이므로 수용한다.

use crate::lexer::{Lexer, Tok};

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
        match t {
            // MySQL 은 힌트 내부도 토큰화한다: `MAX_EXECUTION_TIME(1000)` → `MAX_EXECUTION_TIME (?)`.
            // 원문을 그대로 보존하면 수렴하지 않는다 (M1-6 실측).
            // 힌트 안에 힌트가 들어갈 수 없으므로 재귀 깊이는 1 이다.
            Tok::Hint(inner) => {
                let (inner_canon, _) = render(&Lexer::new(inner).tokenize().0);
                out.push_str("/*+ ");
                out.push_str(&inner_canon);
                out.push_str(" */");
            }
            other => out.push_str(other.render()),
        }
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

/// 두 단계로 축약한다. MySQL 8.4.11 실측으로 확정한 규칙이다.
///
/// | MySQL 입력 | `DIGEST_TEXT` | 규칙 |
/// |---|---|---|
/// | `f(1,2)` · `IN (1)` · `VALUES (1,2)` | `f (...)` | 괄호 안이 **전부** 리터럴 → `(...)` |
/// | `f(memo,1,2)` | `f (\`memo\` , ?, ... )` | 리터럴 **2개 이상 연속** → `?, ...` |
/// | `LIMIT 5, 10` | `LIMIT ?, ...` | 괄호 밖에도 적용된다 |
/// | `VALUES (1,2),(3,4)` | `VALUES (...) /* , ... */` | 주석으로 표시 → 우리는 주석을 제거 |
fn collapse(tokens: &[Tok]) -> Vec<Tok> {
    reduce_literal_runs(&collapse_literal_groups(tokens))
}

/// 리터럴 **2개 이상**이 쉼표로 이어지면 `? , ...` 로 줄인다.
///
/// 1개는 줄이지 않는다 — MySQL 도 그렇다(`SELECT a, 1, b` 는 그대로).
/// `Ellipsis` 도 리터럴 자리이므로 이 함수는 멱등이다(`? , ...` → `? , ...`).
fn reduce_literal_runs(tokens: &[Tok]) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        // `L , L (, L)*` 형태의 최대 런을 찾는다.
        let mut end = i;
        let mut count = 0usize;
        while tokens.get(end).is_some_and(|t| t.is_literal_slot()) {
            count += 1;
            end += 1;
            if matches!(tokens.get(end), Some(Tok::Punct(',')))
                && tokens.get(end + 1).is_some_and(|t| t.is_literal_slot())
            {
                end += 1;
            } else {
                break;
            }
        }
        if count >= 2 {
            out.push(Tok::Placeholder);
            out.push(Tok::Punct(','));
            out.push(Tok::Ellipsis);
            i = end;
        } else {
            out.push(tokens[i].clone());
            i += 1;
        }
    }
    out
}

/// 리터럴만으로 이뤄진 괄호 그룹을 `( ... )` 로 줄이고, 후행 세미콜론을 제거한다.
fn collapse_literal_groups(tokens: &[Tok]) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            // 세미콜론은 문장 구분자이므로 버린다.
            Tok::Punct(';') => i += 1,
            Tok::Punct('(') => match literal_only_group(tokens, i) {
                Some(end) => {
                    out.push(Tok::Punct('('));
                    out.push(Tok::Ellipsis);
                    out.push(Tok::Punct(')'));
                    i = end + 1;
                    // `VALUES (?,?),(?,?),(?,?)` 처럼 이어지는 동종 그룹을 하나로 흡수한다.
                    // 다중 행 INSERT 가 행 수에 따라 다른 다이제스트로 갈라지면 안 된다.
                    while matches!(tokens.get(i), Some(Tok::Punct(','))) {
                        match literal_only_group(tokens, i + 1) {
                            Some(e2) => i = e2 + 1,
                            None => break,
                        }
                    }
                }
                None => {
                    out.push(tokens[i].clone());
                    i += 1;
                }
            },
            t => {
                out.push(t.clone());
                i += 1;
            }
        }
    }
    out
}

/// `start` 위치가 `(` 이고 닫는 `)` 까지의 내용이 리터럴·쉼표뿐이면 닫는 괄호의 인덱스를 반환한다.
fn literal_only_group(tokens: &[Tok], start: usize) -> Option<usize> {
    if !matches!(tokens.get(start), Some(Tok::Punct('('))) {
        return None;
    }
    let mut i = start + 1;
    let mut saw_content = false;
    while let Some(t) = tokens.get(i) {
        match t {
            Tok::Punct(')') => return if saw_content { Some(i) } else { None },
            _ if t.is_literal_slot() => {
                saw_content = true;
                i += 1;
            }
            Tok::Punct(',') => i += 1,
            // 서브쿼리·식·컬럼 참조·introducer 리터럴 → 축약하지 않는다.
            _ => return None,
        }
    }
    None // 닫히지 않은 괄호
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn function_args_collapse_too() {
        // M1-6 실측: MySQL 이 `json_extract (...)` 로 줄인다.
        assert_eq!(
            canon("SELECT JSON_EXTRACT('{\"a\":1}', '$.a')"),
            canon("SELECT json_extract (...)")
        );
        // 인자 1개도 같은 형태로 수렴한다 (임계값을 맞추지 않는다).
        assert_eq!(canon("SELECT SLEEP(0)"), canon("SELECT sleep (?)"));
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
        // MySQL 이 `VALUES (...) , (...)` 로 주더라도 같은 결과에 도달한다.
        assert_eq!(
            canon("INSERT INTO t VALUES (1,2),(3,4)"),
            canon("INSERT INTO t VALUES (...) , (...)")
        );
    }

    #[test]
    fn values_with_expression_is_not_collapsed() {
        let c = canon("INSERT INTO t VALUES (NOW(), 1)");
        assert!(c.contains("now"), "식이 있으면 축약하지 않는다: {c}");
    }

    #[test]
    fn nested_row_lists_converge() {
        assert_eq!(
            canon("SELECT * FROM t WHERE (a,b) IN ((1,2),(3,4))"),
            canon("SELECT * FROM t WHERE (a,b) IN ((...),(...))")
        );
    }

    #[test]
    fn hint_interior_is_normalized() {
        // MySQL: `MAX_EXECUTION_TIME(1000)` → `MAX_EXECUTION_TIME (?)`
        assert_eq!(
            canon("SELECT /*+ MAX_EXECUTION_TIME(1000) */ 1"),
            canon("SELECT /*+ MAX_EXECUTION_TIME (?) */ 1")
        );
        // 백틱 인용 여부도 흡수한다.
        assert_eq!(
            canon("SELECT /*+ NO_ICP(orders) */ 1"),
            canon("SELECT /*+ NO_ICP ( `orders` ) */ 1")
        );
        // 크기 접미(`16M`)가 리터럴로 취급돼야 한다.
        assert_eq!(
            canon("SELECT /*+ SET_VAR(sort_buffer_size = 16M) */ 1"),
            canon("SELECT /*+ SET_VAR ( `sort_buffer_size` = ? ) */ 1")
        );
        // 힌트가 사라지지는 않아야 한다 — 플랜에 영향을 준다.
        assert!(canon("SELECT /*+ NO_ICP(t) */ 1").contains("no_icp"));
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
    fn keyword_synonyms_unified() {
        // M1-6 실측: MySQL DIGEST_TEXT 가 쓰는 형태와 사람이 쓰는 형태를 통일한다.
        assert_eq!(
            canon("SELECT DISTINCT a FROM t"),
            canon("SELECT DISTINCTROW a FROM t")
        );
        assert_eq!(
            canon("SELECT a FROM t WHERE b <> 1"),
            canon("SELECT a FROM t WHERE b != 2")
        );
        assert_eq!(
            canon("SELECT CAST(a AS CHAR)"),
            canon("SELECT CAST(a AS CHARACTER)")
        );
        assert_eq!(
            canon("SELECT a FROM t WHERE d > NOW() - INTERVAL 7 DAY"),
            canon("SELECT a FROM t WHERE d > NOW() - INTERVAL ? SQL_TSI_DAY")
        );
    }

    #[test]
    fn charset_introducer_matches_mysql_shape() {
        assert_eq!(
            canon("SELECT _utf8mb4'한글'"),
            canon("SELECT ( _charset ) ?")
        );
        // 다른 introducer 는 그냥 `?` 다.
        assert_eq!(canon("SELECT X'41'"), canon("SELECT ?"));
        assert_eq!(canon("SELECT N'a'"), canon("SELECT ?"));
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
