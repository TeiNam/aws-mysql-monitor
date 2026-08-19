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
    // v2 가 `attached_condition` 대신 쓰는 이름. 휴리스틱이 잡긴 하지만
    // 명시해 두는 편이 낫다 — 인용부호 없는 리터럴에는 휴리스틱이 약하다.
    "condition",
    // **함수형 인덱스가 걸리면 컬럼명이 아니라 표현식이 들어온다.** 8.4.11 실측:
    //
    // ```sql
    // CREATE TABLE fx (email VARCHAR(200),
    //   INDEX idx ((CONCAT(email,'@internal-payroll.example.com'))));
    // -- "used_columns": [..., "concat(`email`,_utf8mb4'@internal-payroll.example.com')"]
    // -- "used_key_parts": ["concat(`email`,_utf8mb4'@internal-payroll.example.com')"]
    // ```
    //
    // `SAFE_KEYS` 에 있으면 마스킹을 시도조차 하지 않고 `redactions` 도 0 이다 —
    // `ranges`·`operation` 과 **글자 그대로 같은 무성 유출**이었다.
    "used_columns",
    "used_key_parts",
    // v2 는 재작성된 문장 전문을 담고, const-table 최적화 시 **실제 행 데이터**까지 넣는다.
    // 지금은 휴리스틱이 잡지만 가장 리터럴을 많이 담는 키이므로 명시한다.
    "query",
    "heading",
    "lookup_condition",
    "sort_fields",
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

/// **산문형 라벨** 키. 리터럴만 지우고 나머지는 원문 그대로 둔다.
///
/// v2 의 `operation` 은 사람이 읽는 트리 노드 라벨이다. SQL 정규화기를 적용하면
/// 리터럴은 사라지지만 라벨도 함께 망가진다 (실측):
///
/// ```text
/// IN : Limit: 10 row(s)
/// OUT: LIMIT : ? ROW ( s )          ← 정규화기를 쓰면
/// OUT: Limit: ? row(s)              ← 라벨 규칙을 쓰면
///
/// IN : Single-row index lookup on o using PRIMARY (id=5)
/// OUT: single - ROW INDEX lookup ON o USING PRIMARY ( id = ? )
/// OUT: Single-row index lookup on o using PRIMARY (id=?)
/// ```
///
/// 라벨의 가치는 가독성이다. 대소문자·공백을 뒤섞으면 화면에서 쓸 수 없다.
const LABEL_KEYS: &[&str] = &["operation", "heading"];

/// 마스킹 실패 시 값을 대체하는 문자열. 원문을 남기는 것보다 정보를 버리는 쪽을 택한다.
pub const REDACTED: &str = "<redacted>";

/// 산문형 라벨에서 **리터럴만** 지운다. 나머지는 바이트 단위로 보존한다.
///
/// 지우는 것:
/// - 인용부호로 감싼 구간 → `?`
/// - **독립** 숫자 → `?` (식별자에 붙은 숫자는 남긴다: `t1`, `idx_2`)
///
/// 판정: 숫자 앞이 식별자 문자(`[A-Za-z_]` 또는 숫자)면 식별자의 일부다.
/// `cost=1.25` 는 앞이 `=` 이므로 리터럴, `t1` 은 앞이 `t` 이므로 식별자다.
fn mask_label(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' {
            // 닫는 같은 부호까지(없으면 끝까지) 버린다.
            out.push('?');
            i += 1;
            while i < b.len() && b[i] != c {
                i += 1;
            }
            i += 1; // 닫는 부호
            continue;
        }
        if c.is_ascii_digit() {
            let prev_is_ident = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            if prev_is_ident {
                out.push_str(&s[start..i]);
            } else {
                out.push('?');
            }
            continue;
        }
        // 멀티바이트 문자를 쪼개지 않는다.
        let ch = s[i..].chars().next().expect("경계 확인됨");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

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
        // **숫자도 리터럴일 수 있다.** `explain_json_format_version=2` 는 `LIMIT`/`OFFSET`
        // 을 JSON 숫자로 내보낸다 (8.4.11 실측: `"limit_offset": 4242`).
        // 문자열만 마스킹하면 후조건이 걸리지 않고 `redactions` 도 0 이다 — 지금 새는 값이
        // PII 가 아니라 해도, MySQL 이 앞으로 어떤 조건 상수를 숫자로 내보내도
        // **아무도 알아채지 못하는** 구조가 된다.
        Value::Number(_) if !key.is_some_and(is_estimate_key) => {
            *redactions += 1;
            Value::String("?".to_string())
        }
        other => other.clone(),
    }
}

/// 값이 **추정치·구조 정보**여서 보존해야 하는 숫자 키인가.
///
/// 플랜의 유용성은 이 숫자들에서 온다(비용·행수·순번). 반대로 `limit`·`offset` 처럼
/// 쿼리에서 온 상수는 리터럴이다. allowlist 로 두는 이유: 새 MySQL 버전이 추가하는
/// 숫자 키는 **기본적으로 마스킹**돼야 한다 (fail-closed).
fn is_estimate_key(key: &str) -> bool {
    const ESTIMATE_KEYS: &[&str] = &[
        "select_id",
        "rows_examined_per_scan",
        "rows_produced_per_join",
        "rows_for_plan",
        "estimated_rows",
        "estimated_total_cost",
        "estimated_first_row_cost",
        "filtered",
        "key_length",
        "used_key_parts_count",
        "depth",
        "index_dives_for_eq_ranges",
        "chosen",
    ];
    ESTIMATE_KEYS.contains(&key)
        // `*_cost`·`*_per_join` 같은 접미로 끝나는 비용 계열은 전부 추정치다.
        || key.ends_with("_cost")
        || key.ends_with("_per_join")
        || key.ends_with("_per_scan")
}

fn mask_str(key: Option<&str>, s: &str, redactions: &mut usize) -> String {
    let k = key.unwrap_or("");
    if SAFE_KEYS.contains(&k) {
        return s.to_string();
    }
    if LABEL_KEYS.contains(&k) {
        return mask_label(s);
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

    /// **라벨은 읽을 수 있어야 한다.** SQL 정규화기를 적용하면 리터럴은 사라지지만
    /// 대소문자·공백이 뒤섞여 화면에서 쓸 수 없게 된다.
    #[test]
    fn labels_stay_readable_while_literals_are_removed() {
        let cases = [
            ("Limit: 10 row(s)", "Limit: ? row(s)"),
            (
                "Single-row index lookup on o using PRIMARY (id=5)",
                "Single-row index lookup on o using PRIMARY (id=?)",
            ),
            (
                "Index range scan on orders using PRIMARY over (100 <= id <= 200)",
                "Index range scan on orders using PRIMARY over (? <= id <= ?)",
            ),
            // `t1` 의 `1` 은 식별자의 일부다. `cost=1.25` 는 리터럴이다.
            (
                "Table scan on t1  (cost=1.25 rows=5)",
                "Table scan on t1  (cost=? rows=?)",
            ),
            (
                "Filter: (orders.memo = 'kim@example.com')",
                "Filter: (orders.memo = ?)",
            ),
        ];
        for (input, expected) in cases {
            let plan = serde_json::json!({ "query_plan": { "operation": input } });
            let (masked, _) = mask_plan(&plan);
            let got = masked
                .get("query_plan")
                .and_then(|q| q.get("operation"))
                .and_then(|o| o.as_str())
                .expect("operation");
            assert_eq!(got, expected, "\n  입력: {input}");
        }
    }

    /// 라벨 규칙이 유출 경로가 되지 않아야 한다.
    #[test]
    fn label_rule_does_not_leak_quoted_values() {
        for input in [
            "Filter: (t.ssn = '900101-1234567')",
            r#"Filter: (t.email = "kim@example.com")"#,
            "Filter: (t.memo = '닫히지 않은 인용부호",
        ] {
            let plan = serde_json::json!({ "query_plan": { "operation": input } });
            let (masked, _) = mask_plan(&plan);
            let text = serde_json::to_string(&masked).expect("직렬화");
            assert!(!text.contains("900101"), "{input} → {text}");
            assert!(!text.contains("kim@example"), "{input} → {text}");
            assert!(!text.contains("인용부호"), "{input} → {text}");
        }
    }

    /// **함수형 인덱스는 `used_columns`·`used_key_parts` 에 표현식을 넣는다.**
    /// 8.4.11 실측 출력이다.
    #[test]
    fn functional_index_expressions_are_masked() {
        let plan = serde_json::json!({
            "query_block": {
                "table": {
                    "table_name": "fx2",
                    "key": "idx_lit",
                    "used_columns": [
                        "id", "email",
                        "concat(`email`,_utf8mb4'@internal-payroll.example.com')"
                    ],
                    "used_key_parts": [
                        "concat(`email`,_utf8mb4'@internal-payroll.example.com')"
                    ]
                }
            }
        });
        let (masked, _) = mask_plan(&plan);
        let text = serde_json::to_string(&masked).expect("직렬화");
        assert!(
            !text.contains("internal-payroll"),
            "함수형 인덱스 표현식의 리터럴이 남았다: {text}"
        );
        // 컬럼명·인덱스명은 보존해야 플랜이 쓸모 있다.
        assert!(text.contains("email"), "컬럼명이 사라졌다: {text}");
        assert!(text.contains("idx_lit"), "인덱스명이 사라졌다: {text}");
    }

    /// **v2 는 `LIMIT`/`OFFSET` 을 JSON 숫자로 내보낸다.** 문자열만 마스킹하면
    /// 후조건이 걸리지 않고 `redactions` 도 0 이다.
    #[test]
    fn numeric_query_literals_are_masked_but_estimates_survive() {
        let plan = serde_json::json!({
            "query_plan": {
                "operation": "Limit table",
                "limit": 5241,
                "limit_offset": 4242,
                // 추정치는 보존해야 한다 — 플랜의 유용성이 여기서 온다.
                "estimated_rows": 60023.0,
                "estimated_total_cost": 6053.25,
                "select_id": 1,
                "filtered": 100.0
            }
        });
        let (masked, redactions) = mask_plan(&plan);
        let text = serde_json::to_string(&masked).expect("직렬화");

        assert!(!text.contains("4242"), "OFFSET 리터럴이 남았다: {text}");
        assert!(!text.contains("5241"), "LIMIT 리터럴이 남았다: {text}");
        assert!(
            redactions >= 2,
            "마스킹 흔적이 없다 (redactions={redactions})"
        );

        assert!(text.contains("60023"), "추정 행수가 사라졌다: {text}");
        assert!(text.contains("6053.25"), "추정 비용이 사라졌다: {text}");
        assert!(
            text.contains("\"select_id\":1"),
            "select_id 가 사라졌다: {text}"
        );
    }

    /// 새 숫자 키는 **기본적으로 마스킹**돼야 한다 (fail-closed).
    #[test]
    fn unknown_numeric_keys_are_masked_by_default() {
        let plan = serde_json::json!({ "query_block": { "future_mysql_constant": 900101 } });
        let (masked, redactions) = mask_plan(&plan);
        let text = serde_json::to_string(&masked).expect("직렬화");
        assert!(
            !text.contains("900101"),
            "미지의 숫자 키가 통과했다: {text}"
        );
        assert_eq!(redactions, 1);
    }

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
