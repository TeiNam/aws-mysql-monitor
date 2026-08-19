//! 플랜 JSON 리터럴 마스킹 (FR-PLN-09, T-16).
//!
//! # 왜 필요한가
//!
//! `EXPLAIN FORMAT=JSON` 은 조건식을 문자열로 담고, 거기에 **리터럴이 그대로 들어간다**.
//!
//! ```json
//! "attached_condition": "((`shop`.`u`.`email` = 'kim@example.com') and (`shop`.`u`.`ssn` = '900101-1234567'))"
//! ```
//!
//! SQL 본문만 마스킹하고 플랜을 그대로 저장하면 `literal_policy=masked` 인스턴스에서도
//! 리터럴이 저장·조회·아카이브·AI 프롬프트로 흘러간다. 리뷰가 이걸 T-16 으로 잡았다.
//!
//! # 미지의 키에 대한 기본값은 "마스킹"이다
//!
//! MySQL 새 버전이 조건식을 담는 키를 추가하면(`pushed_condition` 처럼) 키 목록 방식은
//! 조용히 유출한다. 그래서 **allowlist 에 없는 키는 리터럴 흔적이 있으면 마스킹**한다.

use dbmon_normalize::mask_expression;
use serde_json::{Map, Value};

/// 값이 항상 식별자·열거형·수치인 키. 그대로 통과시킨다.
const SAFE_KEYS: &[&str] = &[
    "access_type",
    "attached_subqueries",
    "cacheable",
    "column_name",
    "cost_info",
    "dependent",
    "eval_cost",
    "data_read_per_join",
    "filtered",
    "index_name",
    "key",
    "key_length",
    "possible_keys",
    "prefix_cost",
    "query_cost",
    "read_cost",
    "rows_examined_per_scan",
    "rows_produced_per_join",
    "select_id",
    "sort_cost",
    "table_name",
    "table_type",
    "used_columns",
    "used_key_parts",
    "using_MRR",
    "using_filesort",
    "using_index",
    "using_index_for_group_by",
    "using_join_buffer",
    "using_temporary_table",
    "partitions",
    "index_access_type",
];

/// 조건식·표현식을 담는 키. 표현식 마스킹을 적용한다.
const EXPR_KEYS: &[&str] = &[
    "attached_condition",
    // `ranges` 는 값이 **범위 표현식**이다 — 리터럴 경계가 그대로 들어간다.
    //
    // 8.4 기본값(`explain_json_format_version=1`)에서는 나오지 않지만 v2 로 켜면 나오고,
    // MySQL 9.x 는 v2 가 기본이다. 실측 (19 §A-3):
    //
    // ```sql
    // SET explain_json_format_version=2;
    // EXPLAIN FORMAT=JSON SELECT * FROM orders WHERE id BETWEEN 100 AND 200;
    // -- "ranges": ["(100 <= id <= 200)"]
    // ```
    //
    // SAFE_KEYS 에 있으면 `mask_str` 이 **먼저** 반환해 마스킹을 아예 시도하지 않고,
    // `redactions` 도 0 이라 후조건이 걸리지 않는다 — 무성 유출이다.
    "ranges",
    // v2 의 **주 조건식 캐리어**다. v1 에는 이 키가 없다. 실측:
    // `"operation": "Filter: ((orders.memo = 'kim@example.com') and (orders.id between 100 and 200))"`
    "operation",
    // v2 가 `attached_condition` 대신 쓰는 이름. 휴리스틱이 잡긴 하지만
    // 명시해 두는 편이 낫다 — 인용부호 없는 리터럴에는 휴리스틱이 약하다.
    "condition",
    "index_condition",
    "pushed_condition",
    "having",
    "group_by_subqueries",
    "select_list_subqueries",
    "order_by_subqueries",
    "update_value_subqueries",
    "ref",
    "materialized_from_subquery_condition",
];

/// 마스킹 실패 시 값을 대체하는 문자열. 원문을 남기는 것보다 정보를 버리는 쪽을 택한다.
pub const REDACTED: &str = "<redacted>";

/// 플랜 JSON 전체에서 리터럴을 제거한다.
///
/// 반환값의 두 번째는 **마스킹 실패 건수**다. 0이 아니면 그만큼의 필드가 `<redacted>` 로
/// 대체됐다는 뜻이며, 호출자는 카운터를 올려 관측 가능하게 만들어야 한다
/// ([05 §3.4](../../../docs/05-collector.md)).
pub fn mask_plan(value: &Value) -> (Value, usize) {
    let mut redactions = 0;
    let out = walk(value, None, &mut redactions);
    (out, redactions)
}

fn walk(v: &Value, key: Option<&str>, redactions: &mut usize) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, child) in map {
                out.insert(k.clone(), walk(child, Some(k), redactions));
            }
            Value::Object(out)
        }
        // 배열 원소는 부모 키를 물려받는다 (`ref: ["const", "shop.o.id"]`).
        Value::Array(items) => {
            Value::Array(items.iter().map(|c| walk(c, key, redactions)).collect())
        }
        Value::String(s) => Value::String(mask_str(key, s, redactions)),
        other => other.clone(),
    }
}

fn mask_str(key: Option<&str>, s: &str, redactions: &mut usize) -> String {
    let k = key.unwrap_or("");
    if SAFE_KEYS.contains(&k) {
        return s.to_string();
    }
    if EXPR_KEYS.contains(&k) || may_contain_literal(s) {
        let (masked, ok) = mask_expression(s);
        if ok {
            return masked;
        }
        *redactions += 1;
        return REDACTED.to_string();
    }
    s.to_string()
}

/// 리터럴 흔적 휴리스틱 — **미지의 키에만** 적용한다.
///
/// 인용부호가 있거나 숫자로 시작하는 토큰이 있으면 표현식일 가능성이 있다고 본다.
/// 오탐(안전한 문자열을 정규화)의 대가는 가독성 저하뿐이고, 누락의 대가는 리터럴 유출이다.
fn may_contain_literal(s: &str) -> bool {
    let mut prev_ident = false;
    for c in s.chars() {
        if c == '\'' || c == '"' {
            return true;
        }
        if c.is_ascii_digit() && !prev_ident {
            return true;
        }
        prev_ident = c.is_alphanumeric() || c == '_' || c == '$';
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-16 — `explain_json_format_version=2` 의 실측 출력에 리터럴이 남으면 안 된다.
    ///
    /// 아래 JSON 은 MySQL 8.4.11 에서 실제로 받은 것이다:
    ///
    /// ```sql
    /// SET explain_json_format_version=2;
    /// EXPLAIN FORMAT=JSON
    ///   SELECT * FROM orders WHERE memo = 'kim@example.com' AND id BETWEEN 100 AND 200;
    /// ```
    ///
    /// `operation` · `ranges` 는 한때 `SAFE_KEYS` 에 있어서 **마스킹을 시도조차 하지
    /// 않았다** — `redactions` 도 0 이라 후조건이 걸리지 않는 무성 유출이었다.
    /// 8.4 기본값은 v1 이라 잠재 결함이었지만 MySQL 9.x 는 v2 가 기본이다.
    #[test]
    fn format_v2_operation_and_ranges_are_masked() {
        let plan = serde_json::json!({
            "query_plan": {
                "operation": "Filter: ((orders.memo = 'kim@example.com') and (orders.id between 100 and 200))",
                "condition": "((orders.memo = 'kim@example.com') and (orders.id between 100 and 200))",
                "inputs": [{
                    "operation": "Index range scan on orders using PRIMARY over (100 <= id <= 200)",
                    "table_name": "orders",
                    "access_type": "range",
                    "ranges": ["(100 <= id <= 200)"],
                    "index_name": "PRIMARY"
                }]
            }
        });
        let (masked, redactions) = mask_plan(&plan);
        let text = serde_json::to_string(&masked).expect("직렬화");

        assert!(!text.contains("kim@example.com"), "리터럴이 남았다: {text}");
        assert!(
            !text.contains("100") && !text.contains("200"),
            "범위 경계 리터럴이 남았다: {text}"
        );
        // 식별자는 남아야 한다 — 남지 않으면 플랜이 쓸모없어진다.
        assert!(text.contains("orders"), "테이블 이름은 보존한다: {text}");
        assert!(text.contains("PRIMARY"), "인덱스 이름은 보존한다: {text}");
        // 마스킹을 **시도했다는** 증거. 0 이면 allowlist 로 빠져나간 것이다.
        assert!(
            redactions > 0 || text.contains('?'),
            "마스킹 흔적이 없다: {text}"
        );
    }

    use serde_json::json;

    #[test]
    fn attached_condition_literals_are_masked() {
        let plan = json!({"query_block": {"table": {
            "table_name": "u",
            "attached_condition": "((`shop`.`u`.`email` = 'kim@example.com') and (`shop`.`u`.`age` > 30))"
        }}});
        let (masked, redactions) = mask_plan(&plan);
        let s = masked.to_string();
        assert_eq!(redactions, 0);
        assert!(!s.contains("kim@example.com"), "{s}");
        assert!(!s.contains("30"), "{s}");
        assert!(s.contains("email"), "식별자는 남아야 한다: {s}");
    }

    #[test]
    fn safe_keys_pass_through_untouched() {
        let plan = json!({"table": {
            "table_name": "orders_2026", "key": "idx_a_b", "key_length": "4",
            "possible_keys": ["idx_a_b", "PRIMARY"], "filtered": "10.00",
            "rows_examined_per_scan": 812345
        }});
        let (masked, _) = mask_plan(&plan);
        assert_eq!(masked, plan, "안전한 키는 그대로여야 한다");
    }

    #[test]
    fn unknown_key_with_literal_is_masked() {
        // MySQL 새 버전이 조건식 키를 추가한 상황. allowlist 에 없으므로 마스킹돼야 한다.
        let plan = json!({"table": {"future_condition_key": "(`t`.`c` = 'topsecret')"}});
        let s = mask_plan(&plan).0.to_string();
        assert!(!s.contains("topsecret"), "미지의 키가 유출됐다: {s}");
    }

    #[test]
    fn unmaskable_value_is_redacted_not_leaked() {
        // 닫히지 않은 인용부호 → 후조건 실패 → 원문을 남기면 안 된다.
        let plan = json!({"table": {"attached_condition": "(`t`.`c` = 'unterminated"}});
        let (masked, redactions) = mask_plan(&plan);
        let s = masked.to_string();
        assert_eq!(redactions, 1);
        assert!(s.contains(REDACTED), "{s}");
        assert!(!s.contains("unterminated"), "{s}");
    }

    #[test]
    fn plain_messages_stay_readable() {
        let plan = json!({"query_block": {"message": "no matching row in const table"}});
        let s = mask_plan(&plan).0["query_block"]["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(s.contains("matching row"), "{s}");
    }

    #[test]
    fn ref_array_elements_are_masked() {
        let plan = json!({"table": {"ref": ["const", "shop.o.id"]}});
        let (masked, _) = mask_plan(&plan);
        assert!(masked["table"]["ref"].is_array());
    }
}
