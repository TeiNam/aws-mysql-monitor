//! 마스킹 후조건 검증 (T-36, [05 §3.4](../../../docs/05-collector.md)).
//!
//! 골든 코퍼스는 **등가성**(`normalize(원문) == normalize(DIGEST_TEXT)`)만 검증한다.
//! 등가성이 성립해도 "리터럴이 실제로 제거됐는가"는 아무도 확인하지 않는다.
//! 두 입력에 같은 버그가 있으면 같은 결과가 나오기 때문이다.
//!
//! `masked` / `off` 정책은 **보안 통제**이므로, 저장 직전에 리터럴 잔존 0을 강제한다.
//! 실패 시 `sql_text` 를 저장하지 않고 `off` 로 강등한다.

use crate::lexer::{Lexer, Tok};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralResidue {
    /// `?` 로 치환되지 않은 리터럴이 남아 있다 (렉서가 놓친 형태).
    UnmaskedLiteral { count: usize },
    /// 인용부호가 남아 있다 — 문자열이 닫히지 않았거나 렉서를 빠져나갔다.
    QuoteCharacter { ch: char },
    /// 렉싱 중 닫히지 않은 인용부호를 만났다.
    UnterminatedQuote,
}

impl std::fmt::Display for LiteralResidue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnmaskedLiteral { count } => write!(f, "마스킹되지 않은 리터럴 {count}개"),
            Self::QuoteCharacter { ch } => write!(f, "인용부호 {ch:?} 잔존"),
            Self::UnterminatedQuote => write!(f, "닫히지 않은 인용부호"),
        }
    }
}

/// 정규 텍스트에 리터럴이 남아 있지 않은지 확인한다.
///
/// 검사 방법: **정규 텍스트를 다시 렉싱**한다. 정규화가 성공했다면 모든 리터럴이 이미
/// `?` 이므로 재렉싱 결과에 `Placeholder`(리터럴에서 유래한 `?`)가 하나도 없어야 한다.
/// 하나라도 있으면 렉서가 그 형태를 리터럴로 인식하지 못했다는 뜻이다.
///
/// 옵티마이저 힌트(`/*+ ... */`)는 통째로 하나의 토큰이므로 내부 숫자가 검사에 걸리지
/// 않는다. 힌트는 개발자가 작성한 것이고 사용자 데이터가 아니다.
pub fn check_no_literals(canonical: &str) -> Result<(), LiteralResidue> {
    for ch in ['\'', '"', '`'] {
        if contains_outside_hint(canonical, ch) {
            return Err(LiteralResidue::QuoteCharacter { ch });
        }
    }
    let (tokens, unterminated) = Lexer::new(canonical).tokenize();
    if unterminated {
        return Err(LiteralResidue::UnterminatedQuote);
    }
    let count = tokens.iter().filter(|t| **t == Tok::Placeholder).count();
    if count > 0 {
        return Err(LiteralResidue::UnmaskedLiteral { count });
    }
    Ok(())
}

/// 힌트 블록(`/*+ ... */`) 바깥에서 해당 문자를 찾는다.
fn contains_outside_hint(s: &str, needle: char) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            // 힌트든 일반 주석이든 닫는 `*/` 까지 건너뛴다.
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i += 2;
            continue;
        }
        if b[i] == needle as u8 {
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize;

    #[test]
    fn clean_canonical_passes() {
        for sql in [
            "SELECT a FROM t WHERE id = 1 AND name = 'x'",
            "SELECT a FROM t WHERE id IN (1,2,3)",
            "INSERT INTO t VALUES (1,'a'),(2,'b')",
            "UPDATE t SET x = 0x1F WHERE d = DATE '2026-01-01'",
            "SELECT /*+ MAX_EXECUTION_TIME(1000) */ a FROM t WHERE b = 5",
        ] {
            let n = normalize(sql);
            assert_eq!(
                check_no_literals(&n.canonical),
                Ok(()),
                "sql={sql} canonical={}",
                n.canonical
            );
        }
    }

    #[test]
    fn detects_leaked_number() {
        assert_eq!(
            check_no_literals("SELECT a FROM t WHERE id = 42"),
            Err(LiteralResidue::UnmaskedLiteral { count: 1 })
        );
    }

    #[test]
    fn detects_leaked_string() {
        assert!(matches!(
            check_no_literals("SELECT a FROM t WHERE n = 'secret'"),
            Err(LiteralResidue::QuoteCharacter { ch: '\'' })
        ));
    }

    #[test]
    fn hint_internals_do_not_trigger() {
        // 힌트 안의 숫자·인용부호는 사용자 데이터가 아니다.
        assert_eq!(
            check_no_literals("SELECT /*+ SET_VAR(max_execution_time=1000) */ a FROM t"),
            Ok(())
        );
    }
}
