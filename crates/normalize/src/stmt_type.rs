//! `statement_type` 판정 ([05 §3.5](../../../docs/05-collector.md)).

use crate::lexer::Tok;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum StatementType {
    Select,
    Insert,
    Update,
    Delete,
    Replace,
    /// `CREATE` / `ALTER` / `DROP` / `TRUNCATE` / `RENAME`
    Ddl,
    Other,
}

impl StatementType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Replace => "REPLACE",
            Self::Ddl => "DDL",
            Self::Other => "OTHER",
        }
    }

    /// 사후 `EXPLAIN` 재실행 폴백을 허용할 문장인가
    /// ([05 §2.4](../../../docs/05-collector.md): SELECT 만 재실행한다).
    pub fn is_replay_safe(self) -> bool {
        self == Self::Select
    }
}

/// 첫 유의미 토큰으로 판정한다. `WITH`(CTE)면 본체의 첫 DML 키워드를 찾는다.
pub fn classify(tokens: &[Tok]) -> StatementType {
    let mut it = tokens.iter().peekable();
    // 선행 힌트와 여는 괄호(괄호로 감싼 SELECT, `(SELECT ...) UNION (SELECT ...)`)를 건너뛴다.
    while let Some(t) = it.peek() {
        match t {
            Tok::Hint(_) | Tok::Punct('(') => {
                it.next();
            }
            _ => break,
        }
    }
    let Some(first) = it.next() else {
        return StatementType::Other;
    };

    if let Some(ty) = word_to_type(first) {
        return ty;
    }
    // `WITH ... UPDATE`: CTE 정의를 지나 **괄호 깊이 0의** 첫 DML 키워드를 찾는다.
    //
    // 깊이를 보지 않으면 `WITH c AS (SELECT id FROM s) UPDATE t ...` 가 CTE 정의 안의
    // `SELECT` 를 집어 `Select` 로 오판한다. 그러면 `is_replay_safe()` 가 true 가 되어
    // **UPDATE 문을 사후 EXPLAIN 으로 재실행**하게 된다 — 관측 도구가 데이터를 바꾼다.
    if matches!(first, Tok::Kw(k) if k == "WITH") {
        let mut depth = 0i32;
        for t in it {
            match t {
                Tok::Punct('(') => depth += 1,
                Tok::Punct(')') => depth -= 1,
                _ if depth == 0 => {
                    if let Some(ty) = word_to_type(t) {
                        return ty;
                    }
                }
                _ => {}
            }
        }
    }
    StatementType::Other
}

fn word_to_type(t: &Tok) -> Option<StatementType> {
    let w = match t {
        Tok::Kw(k) => k.as_str(),
        // `TRUNCATE` 는 MySQL 비예약어라 `Ident` 로 온다 (소문자로 접혀 있다).
        Tok::Ident(i) => {
            return match i.as_str() {
                "truncate" => Some(StatementType::Ddl),
                _ => None,
            };
        }
        _ => return None,
    };
    Some(match w {
        "SELECT" => StatementType::Select,
        "INSERT" => StatementType::Insert,
        "UPDATE" => StatementType::Update,
        "DELETE" => StatementType::Delete,
        "REPLACE" => StatementType::Replace,
        "CREATE" | "ALTER" | "DROP" | "RENAME" => StatementType::Ddl,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn ty(s: &str) -> StatementType {
        classify(&Lexer::new(s).tokenize().0)
    }

    #[test]
    fn basic_statements() {
        assert_eq!(ty("SELECT 1"), StatementType::Select);
        assert_eq!(ty("  update t set a=1"), StatementType::Update);
        assert_eq!(ty("DELETE FROM t"), StatementType::Delete);
        assert_eq!(ty("insert into t values (1)"), StatementType::Insert);
        assert_eq!(ty("REPLACE INTO t VALUES (1)"), StatementType::Replace);
    }

    #[test]
    fn ddl_and_other() {
        assert_eq!(ty("ALTER TABLE t ADD COLUMN c INT"), StatementType::Ddl);
        assert_eq!(ty("TRUNCATE TABLE t"), StatementType::Ddl);
        assert_eq!(ty("CALL sp_x(1)"), StatementType::Other);
        assert_eq!(ty("SET autocommit=1"), StatementType::Other);
        assert_eq!(ty(""), StatementType::Other);
    }

    #[test]
    fn cte_resolves_to_body() {
        assert_eq!(
            ty("WITH c AS (SELECT 1) SELECT * FROM c"),
            StatementType::Select
        );
        assert_eq!(
            ty("WITH c AS (SELECT id FROM s) UPDATE t JOIN c USING(id) SET t.x=1"),
            StatementType::Update
        );
    }

    #[test]
    fn leading_hint_and_paren() {
        assert_eq!(ty("/*+ NO_ICP(t) */ SELECT 1"), StatementType::Select);
        assert_eq!(ty("(SELECT 1) UNION (SELECT 2)"), StatementType::Select);
    }

    #[test]
    fn only_select_is_replay_safe() {
        assert!(StatementType::Select.is_replay_safe());
        for t in [
            StatementType::Update,
            StatementType::Delete,
            StatementType::Insert,
            StatementType::Replace,
            StatementType::Ddl,
            StatementType::Other,
        ] {
            assert!(!t.is_replay_safe(), "{t:?} 는 재실행하면 안 된다");
        }
    }
}
