//! `FROM`·`JOIN` 절의 테이블과 별칭을 뽑는다.
//!
//! # 왜 필요한가 — 플랜에는 별칭만 있다
//!
//! `EXPLAIN FORMAT=JSON` 의 `table_name` 은 **별칭**이다. 실측(Aurora MySQL 8.0):
//!
//! ```text
//! SELECT COUNT(*) FROM order_items a STRAIGHT_JOIN customers c ON c.id % 13 = a.id % 13
//!
//! {"table_name": "a", "access_type": "index", "key": "idx_items_order"}
//! {"table_name": "c", "attached_condition": "((`shop`.`c`.`id` % 13) = (`shop`.`a`.`id` % 13))"}
//! ```
//!
//! `attached_condition` 의 3단 참조조차 `스키마 . 별칭 . 컬럼` 이므로
//! [`crate::walk::schema_hints`](../planparse/walk.rs) 도 별칭을 돌려준다. 즉 **플랜만으로는
//! 원래 테이블 이름을 알 수 없다.**
//!
//! 그래서 튜닝 컨텍스트가 `information_schema` 에서 `shop.a` 를 찾다가 못 찾고, 별칭을 쓴
//! 쿼리는 전부 스키마·인덱스·카디널리티 없이 분석됐다. 실제 SQL 은 대부분 별칭을 쓴다.
//!
//! # 정규화된 SQL 로도 된다
//!
//! `masked` 정책이 저장하는 canonical 형태는 리터럴만 `?` 로 바뀌고 **테이블 이름과 별칭은
//! 그대로**다. 그래서 원문이 없어도(정책이 `off` 가 아니면) 해석할 수 있다.

use crate::lexer::{Lexer, Tok};

/// SQL 이 참조한 테이블 하나.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// `FROM shop.orders` 처럼 명시됐을 때만 있다.
    pub schema: Option<String>,
    pub name: String,
    /// `FROM orders o` 의 `o`. 없으면 테이블 이름 자체가 참조 이름이다.
    pub alias: Option<String>,
}

/// 테이블 소스를 기대하게 만드는 키워드.
///
/// `INNER`·`LEFT`·`CROSS` 는 뒤에 `JOIN` 이 오므로 따로 볼 필요가 없다.
/// `INSERT INTO` 는 넣지 않는다 — 대상 테이블은 플랜의 참조 목록에 뜨지 않는다.
fn arms_table_source(kw: &str) -> bool {
    matches!(kw, "FROM" | "JOIN" | "STRAIGHT_JOIN" | "UPDATE")
}

/// 테이블 이름 뒤에 와도 **별칭이 아닌** 것. 키워드는 전부 별칭이 아니지만,
/// `AS` 는 "다음이 별칭" 이라는 뜻이라 호출부가 따로 처리한다.
fn is_alias_stop(tok: &Tok) -> bool {
    !matches!(tok, Tok::Ident(_))
}

/// `FROM`·`JOIN`·`UPDATE` 절의 테이블과 별칭을 순서대로 돌려준다.
///
/// # 다루지 않는 것 (알고 남긴 천장)
///
/// - **파생 테이블·서브쿼리** (`FROM (SELECT …) x`) — 실제 테이블이 아니므로 건너뛴다.
///   플랜은 그걸 `<derived2>` 로 부르고, 그 이름은 `information_schema` 에 없으니 지금과
///   같이 스키마 없이 분석된다.
/// - **인덱스 힌트** (`FROM t USE INDEX (i) x`) — 힌트를 건너뛰고 별칭을 찾는다.
/// - `PARTITION (p1)` 절 — 힌트와 같은 방식으로 건너뛴다.
///
/// ponytail: 토큰 스캔이다. SQL 파서를 들이지 않는다 — 필요한 것은 `별칭 → 테이블` 한 장이고,
/// 그건 `FROM`/`JOIN` 뒤의 두세 토큰에 있다. 파서가 필요해지는 신호는 파생 테이블의
/// 컬럼까지 따라가야 할 때다.
pub fn table_refs(sql: &str) -> Vec<TableRef> {
    let (toks, _truncated) = Lexer::new(sql).tokenize();
    let mut out: Vec<TableRef> = Vec::new();
    let mut i = 0usize;
    // 방금 테이블 소스를 읽었는가. `FROM a x, b y` 의 쉼표를 여기서만 인정한다 —
    // 아무 쉼표나 인정하면 `GROUP BY x, y` 의 컬럼이 테이블이 된다.
    let mut after_source = false;

    while i < toks.len() {
        let armed = match &toks[i] {
            Tok::Kw(k) if arms_table_source(k) => true,
            Tok::Punct(',') if after_source => true,
            _ => {
                after_source = false;
                i += 1;
                continue;
            }
        };
        i += 1;
        if !armed {
            continue;
        }
        after_source = false;

        // 파생 테이블·괄호 조인은 건너뛴다.
        if matches!(toks.get(i), Some(Tok::Punct('('))) {
            i = skip_parens(&toks, i);
            // 파생 테이블에도 별칭이 붙지만(`) x`) 실제 테이블이 아니므로 담지 않는다.
            // 그 별칭을 건너뛰어야 다음 쉼표를 테이블 목록으로 오해하지 않는다.
            if matches!(toks.get(i), Some(Tok::Kw(k)) if k == "AS") {
                i += 1;
            }
            if matches!(toks.get(i), Some(Tok::Ident(_))) {
                i += 1;
            }
            after_source = true;
            continue;
        }

        let Some(Tok::Ident(first)) = toks.get(i) else {
            continue;
        };
        i += 1;
        // `schema . table`
        let (schema, name) = if matches!(toks.get(i), Some(Tok::Punct('.'))) {
            match toks.get(i + 1) {
                Some(Tok::Ident(second)) => {
                    let pair = (Some(first.clone()), second.clone());
                    i += 2;
                    pair
                }
                // `t.*` 같은 것 — 테이블 소스가 아니다.
                _ => continue,
            }
        } else {
            (None, first.clone())
        };

        // 인덱스 힌트·파티션 절을 건너뛴다: `USE|FORCE|IGNORE INDEX (…)`, `PARTITION (…)`.
        loop {
            let skip_kw = matches!(
                toks.get(i),
                Some(Tok::Kw(k)) if k == "USE" || k == "FORCE" || k == "IGNORE" || k == "PARTITION"
            );
            if !skip_kw {
                break;
            }
            i += 1;
            // `INDEX`/`KEY` 와 선택적 `FOR JOIN` 등은 괄호 앞까지 그냥 넘긴다.
            while matches!(toks.get(i), Some(Tok::Kw(_))) {
                i += 1;
            }
            if matches!(toks.get(i), Some(Tok::Punct('('))) {
                i = skip_parens(&toks, i);
            }
        }

        // 별칭. `AS` 는 있어도 없어도 된다.
        let mut alias = None;
        if matches!(toks.get(i), Some(Tok::Kw(k)) if k == "AS") {
            i += 1;
        }
        if let Some(tok) = toks.get(i) {
            if !is_alias_stop(tok) {
                if let Tok::Ident(a) = tok {
                    alias = Some(a.clone());
                    i += 1;
                }
            }
        }

        out.push(TableRef {
            schema,
            name,
            alias,
        });
        after_source = true;
    }
    out
}

/// `(` 에서 시작해 짝이 맞는 `)` **다음** 인덱스를 돌려준다.
fn skip_parens(toks: &[Tok], open_at: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open_at;
    while i < toks.len() {
        match &toks[i] {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    // 짝이 없다(절단된 SQL). 끝으로 보낸다 — 남은 토큰을 테이블로 오해하지 않는다.
    toks.len()
}

/// 플랜이 부르는 이름(`별칭` 이거나 실제 이름)을 실제 테이블로 옮긴다.
///
/// 별칭이 실제 테이블 이름과 같은 이름을 가리키는 경우(`FROM orders orders`)도 그냥 맞는다.
/// 못 찾으면 `None` — 호출부가 원래 이름을 그대로 쓴다(파생 테이블 등).
pub fn resolve<'a>(refs: &'a [TableRef], plan_name: &str) -> Option<&'a TableRef> {
    // ① 별칭이 정확히 맞는 것.
    if let Some(hit) = refs.iter().find(|r| r.alias.as_deref() == Some(plan_name)) {
        return Some(hit);
    }
    // ② 별칭이 없는 테이블의 이름이 맞는 것.
    refs.iter()
        .find(|r| r.alias.is_none() && r.name == plan_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(sql: &str) -> Vec<(Option<String>, String, Option<String>)> {
        table_refs(sql)
            .into_iter()
            .map(|r| (r.schema, r.name, r.alias))
            .collect()
    }

    /// **이 결함의 원본 쿼리다.** 실배포에서 `referenced_tables` 가
    /// `("shop.a", "shop.c")` 로 저장돼 있었다.
    #[test]
    fn the_query_that_exposed_this_resolves() {
        let sql = "SELECT COUNT(*) FROM order_items a \
                   STRAIGHT_JOIN customers c ON c.id % 13 = a.id % 13";
        assert_eq!(
            refs(sql),
            vec![
                (None, "order_items".into(), Some("a".into())),
                (None, "customers".into(), Some("c".into())),
            ]
        );
        let r = table_refs(sql);
        assert_eq!(
            resolve(&r, "a").map(|t| t.name.as_str()),
            Some("order_items")
        );
        assert_eq!(resolve(&r, "c").map(|t| t.name.as_str()), Some("customers"));
        // 플랜이 실제 이름을 줬을 때도(별칭 없는 쿼리) 맞아야 한다 — 여기서는 별칭이
        // 있으므로 `order_items` 라는 플랜 이름은 못 찾는 것이 맞다.
        assert!(resolve(&r, "order_items").is_none());
    }

    /// **정규화된 SQL 로도 된다.** `masked` 정책은 canonical 만 저장한다.
    #[test]
    fn the_normalized_form_still_carries_names_and_aliases() {
        let canonical = crate::normalize(
            "SELECT COUNT(*) FROM order_items a JOIN order_items b ON b.order_id = a.order_id + 80",
        )
        .canonical;
        assert!(
            canonical.contains('?'),
            "리터럴이 남아 있으면 전제가 틀렸다"
        );
        let r = table_refs(&canonical);
        assert_eq!(
            resolve(&r, "a").map(|t| t.name.as_str()),
            Some("order_items")
        );
        assert_eq!(
            resolve(&r, "b").map(|t| t.name.as_str()),
            Some("order_items")
        );
    }

    #[test]
    fn schema_qualified_and_as_keyword() {
        assert_eq!(
            refs("SELECT * FROM shop.orders AS o JOIN shop.items i ON i.oid = o.id"),
            vec![
                (Some("shop".into()), "orders".into(), Some("o".into())),
                (Some("shop".into()), "items".into(), Some("i".into())),
            ]
        );
    }

    #[test]
    fn a_table_without_an_alias_keeps_its_name() {
        assert_eq!(
            refs("SELECT * FROM orders WHERE id = 1"),
            vec![(None, "orders".into(), None)]
        );
        let r = table_refs("SELECT * FROM orders WHERE id = 1");
        assert_eq!(
            resolve(&r, "orders").map(|t| t.name.as_str()),
            Some("orders")
        );
    }

    /// **쉼표 목록은 테이블 자리에서만 인정한다.**
    ///
    /// 아무 쉼표나 인정하면 `GROUP BY a, b` 의 컬럼이 테이블이 된다 — 그러면 존재하지
    /// 않는 테이블을 `information_schema` 에 묻고, 상한(10개)을 그것들이 먹는다.
    #[test]
    fn commas_outside_the_table_list_are_not_tables() {
        assert_eq!(
            refs("SELECT a, b FROM t1 x, t2 y GROUP BY a, b ORDER BY c, d"),
            vec![
                (None, "t1".into(), Some("x".into())),
                (None, "t2".into(), Some("y".into())),
            ]
        );
    }

    /// 절 키워드는 별칭이 아니다 — `FROM orders WHERE` 의 `WHERE` 를 별칭으로 읽으면
    /// 그 뒤 전부가 어긋난다.
    #[test]
    fn clause_keywords_are_not_aliases() {
        for sql in [
            "SELECT * FROM orders WHERE id = 1",
            "SELECT * FROM orders ORDER BY id",
            "SELECT * FROM orders GROUP BY id",
            "SELECT * FROM orders LIMIT 10",
            "SELECT * FROM a JOIN b ON a.id = b.id",
            "SELECT * FROM a JOIN b USING (id)",
        ] {
            let r = table_refs(sql);
            assert!(
                r.iter().all(|t| t.alias.as_deref() != Some("where")),
                "{sql}"
            );
            assert_eq!(
                r[0].name,
                if sql.contains("orders") {
                    "orders"
                } else {
                    "a"
                },
                "{sql}"
            );
            assert_eq!(r[0].alias, None, "{sql}");
        }
    }

    /// 파생 테이블은 담지 않는다. **그 별칭도 건너뛴다** — 안 넘기면 뒤따르는 쉼표를
    /// 테이블 목록으로 오해한다.
    #[test]
    fn derived_tables_are_skipped_along_with_their_alias() {
        assert_eq!(
            refs("SELECT * FROM (SELECT id FROM orders) x, customers c WHERE x.id = c.id"),
            vec![(None, "customers".into(), Some("c".into()))]
        );
    }

    #[test]
    fn index_hints_and_partitions_do_not_become_aliases() {
        assert_eq!(
            refs("SELECT * FROM orders USE INDEX (idx_a) o WHERE o.id = 1"),
            vec![(None, "orders".into(), Some("o".into()))]
        );
        assert_eq!(
            refs("SELECT * FROM orders PARTITION (p1) o"),
            vec![(None, "orders".into(), Some("o".into()))]
        );
        assert_eq!(
            refs("SELECT * FROM orders FORCE INDEX FOR JOIN (idx_a)"),
            vec![(None, "orders".into(), None)]
        );
    }

    #[test]
    fn update_and_delete_are_covered() {
        assert_eq!(
            refs("UPDATE orders o SET o.state = 1 WHERE o.id = 2"),
            vec![(None, "orders".into(), Some("o".into()))]
        );
        assert_eq!(
            refs("DELETE FROM orders WHERE id = 1"),
            vec![(None, "orders".into(), None)]
        );
    }

    /// 절단·깨진 SQL 에 패닉하지 않는다.
    #[test]
    fn malformed_input_does_not_panic() {
        for sql in [
            "",
            "FROM",
            "SELECT * FROM",
            "SELECT * FROM (",
            "SELECT * FROM (SELECT",
            "SELECT * FROM a JOIN",
            "FROM , , ,",
            "SELECT * FROM shop.",
        ] {
            let _ = table_refs(sql);
        }
    }
}
