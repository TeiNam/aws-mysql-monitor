//! 플랜 JSON 을 평탄한 접근 노드 목록으로 바꾼다.
//!
//! # 스키마를 고정하지 않고 재귀 순회하는 이유
//!
//! `EXPLAIN FORMAT=JSON` 의 구조는 문장 종류·버전·최적화 경로에 따라 키가 달라진다
//! (`nested_loop`, `ordering_operation`, `grouping_operation`, `duplicates_removal`,
//! `union_result`, `materialized_from_subquery`, `attached_subqueries`,
//! `optimized_away_subqueries`, `insert_from`, `buffer_result` …).
//! 구조체로 고정하면 새 키가 나올 때마다 파싱이 실패하거나 조용히 노드를 놓친다.
//!
//! **"`table_name` 을 가진 객체는 테이블 접근이다"** 라는 하나의 불변식만 쓴다.

use serde_json::Value;

/// MySQL 이 만드는 가상 테이블 이름 접두. 실제 테이블이 아니므로 참조 목록에서 제외한다.
const PSEUDO_TABLE_PREFIXES: &[&str] = &["<derived", "<union", "<subquery", "<temporary"];

#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    pub depth: u32,
    /// 이 노드를 담고 있던 부모 키 (`table`, `nested_loop`, `union_result` …).
    pub role: String,
    pub table_name: Option<String>,
    pub access_type: Option<String>,
    pub key: Option<String>,
    pub possible_keys: Vec<String>,
    pub used_key_parts: Vec<String>,
    pub rows_examined_per_scan: Option<u64>,
    pub rows_produced_per_join: Option<u64>,
    pub filtered_pct: Option<f64>,
    pub using_filesort: bool,
    pub using_temporary_table: bool,
    pub using_index: bool,
    /// **이미 마스킹된** 조건식.
    pub attached_condition: Option<String>,
    pub message: Option<String>,
}

impl PlanNode {
    /// 실제 테이블 접근인가 (가상 테이블·연산 노드가 아닌가).
    pub fn is_real_table(&self) -> bool {
        self.table_name
            .as_deref()
            .is_some_and(|t| !is_pseudo_table(t))
    }
}

pub fn is_pseudo_table(name: &str) -> bool {
    PSEUDO_TABLE_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// 마스킹된 플랜 JSON 을 순회해 노드를 모은다.
pub fn collect_nodes(masked_plan: &Value) -> Vec<PlanNode> {
    let mut out = Vec::new();
    visit(masked_plan, "root", 0, &mut out);
    out
}

fn visit(v: &Value, role: &str, depth: u32, out: &mut Vec<PlanNode>) {
    // 깊이 상한 — 악의적/기형 플랜에서 스택을 지키는 방어선.
    if depth > 64 {
        return;
    }
    match v {
        Value::Object(map) => {
            if map.contains_key("table_name") {
                out.push(node_from(map, role, depth));
            }
            for (k, child) in map {
                if matches!(child, Value::Object(_) | Value::Array(_)) {
                    visit(child, k, depth + 1, out);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                visit(item, role, depth, out);
            }
        }
        _ => {}
    }
}

fn node_from(map: &serde_json::Map<String, Value>, role: &str, depth: u32) -> PlanNode {
    PlanNode {
        depth,
        role: role.to_string(),
        table_name: str_of(map, "table_name"),
        access_type: str_of(map, "access_type"),
        key: str_of(map, "key"),
        possible_keys: str_vec(map, "possible_keys"),
        used_key_parts: str_vec(map, "used_key_parts"),
        rows_examined_per_scan: u64_of(map, "rows_examined_per_scan"),
        rows_produced_per_join: u64_of(map, "rows_produced_per_join"),
        filtered_pct: f64_of(map, "filtered"),
        using_filesort: bool_of(map, "using_filesort"),
        using_temporary_table: bool_of(map, "using_temporary_table"),
        using_index: bool_of(map, "using_index"),
        attached_condition: str_of(map, "attached_condition"),
        message: str_of(map, "message"),
    }
}

// ── 관대한 타입 변환 ─────────────────────────────────────────────────────────
// MySQL 은 같은 의미의 값을 버전·필드에 따라 문자열("100.00")로도 수치(100)로도 준다.
// 엄격하게 파싱하면 한 필드 때문에 플랜 전체를 잃는다.

fn str_of(m: &serde_json::Map<String, Value>, k: &str) -> Option<String> {
    m.get(k)?.as_str().map(str::to_string)
}

fn str_vec(m: &serde_json::Map<String, Value>, k: &str) -> Vec<String> {
    match m.get(k) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn u64_of(m: &serde_json::Map<String, Value>, k: &str) -> Option<u64> {
    match m.get(k)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        Value::String(s) => s
            .parse::<u64>()
            .ok()
            .or_else(|| s.parse::<f64>().ok().map(|f| f.max(0.0) as u64)),
        _ => None,
    }
}

fn f64_of(m: &serde_json::Map<String, Value>, k: &str) -> Option<f64> {
    match m.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn bool_of(m: &serde_json::Map<String, Value>, k: &str) -> bool {
    match m.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// 마스킹된 조건식에서 `스키마 . 테이블 . 컬럼` 3단 참조를 찾아 `(스키마, 테이블)` 을 모은다.
///
/// `EXPLAIN FORMAT=JSON` 의 `table_name` 은 **별칭**일 수 있고 스키마를 포함하지 않는다.
/// 조건식에는 정규화된 3단 참조가 남으므로 여기서 스키마를 보강한다.
pub fn schema_hints(nodes: &[PlanNode]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for n in nodes {
        let Some(cond) = n.attached_condition.as_deref() else {
            continue;
        };
        let toks: Vec<&str> = cond.split_whitespace().collect();
        // `a . b . c` 패턴: i, i+1=".", i+2, i+3=".", i+4
        for i in 0..toks.len().saturating_sub(4) {
            if toks[i + 1] == "." && toks[i + 3] == "." {
                let (schema, table) = (strip_punct(toks[i]), strip_punct(toks[i + 2]));
                if is_plain_ident(schema) && is_plain_ident(table) {
                    let pair = (schema.to_string(), table.to_string());
                    if !out.contains(&pair) {
                        out.push(pair);
                    }
                }
            }
        }
    }
    out
}

fn strip_punct(s: &str) -> &str {
    s.trim_matches(|c: char| c == '(' || c == ')' || c == ',')
}

fn is_plain_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_nested_table_nodes() {
        let plan = json!({"query_block": {"select_id": 1, "nested_loop": [
            {"table": {"table_name": "o", "access_type": "ALL", "rows_examined_per_scan": 812345}},
            {"table": {"table_name": "i", "access_type": "ref", "key": "idx_oid",
                       "used_key_parts": ["order_id"], "filtered": "100.00"}}
        ]}});
        let nodes = collect_nodes(&plan);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].table_name.as_deref(), Some("o"));
        assert_eq!(nodes[0].rows_examined_per_scan, Some(812345));
        assert_eq!(nodes[1].key.as_deref(), Some("idx_oid"));
        assert_eq!(nodes[1].filtered_pct, Some(100.0));
    }

    #[test]
    fn tolerates_string_or_number_types() {
        let a = json!({"table": {"table_name": "t", "rows_examined_per_scan": "1000", "filtered": 12.5}});
        let n = &collect_nodes(&a)[0];
        assert_eq!(n.rows_examined_per_scan, Some(1000));
        assert_eq!(n.filtered_pct, Some(12.5));
    }

    #[test]
    fn pseudo_tables_flagged() {
        let plan = json!({"table": {"table_name": "<derived2>"}});
        assert!(!collect_nodes(&plan)[0].is_real_table());
        let plan2 = json!({"table": {"table_name": "orders"}});
        assert!(collect_nodes(&plan2)[0].is_real_table());
    }

    #[test]
    fn schema_extracted_from_masked_condition() {
        let nodes = vec![PlanNode {
            depth: 1,
            role: "table".into(),
            table_name: Some("u".into()),
            attached_condition: Some(
                "( ( shop . u . email = ? ) and ( shop . u . age > ? ) )".into(),
            ),
            access_type: None,
            key: None,
            possible_keys: vec![],
            used_key_parts: vec![],
            rows_examined_per_scan: None,
            rows_produced_per_join: None,
            filtered_pct: None,
            using_filesort: false,
            using_temporary_table: false,
            using_index: false,
            message: None,
        }];
        assert_eq!(
            schema_hints(&nodes),
            vec![("shop".to_string(), "u".to_string())]
        );
    }

    #[test]
    fn deep_nesting_does_not_overflow_stack() {
        // 64단 상한이 동작하는지. 패닉·스택오버플로가 없어야 한다.
        let mut v = json!({"table": {"table_name": "leaf"}});
        for _ in 0..500 {
            v = json!({"nested_loop": [v]});
        }
        let _ = collect_nodes(&v);
    }
}
