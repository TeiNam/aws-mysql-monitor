//! `EXPLAIN FORMAT=JSON` 파싱 · 리터럴 마스킹 · 플랜 지문 · 참조 테이블 추출.
//!
//! 순수 함수만 있다. `EXPLAIN` 을 실행하지 않는다([02 §5](../../../.claude/docs/02-architecture.md)).

pub mod mask;
pub mod walk;

pub use mask::REDACTED;
pub use walk::PlanNode;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFormat {
    /// 기본 형식 (`query_block` 루트).
    JsonV1,
    /// `explain_json_format_version=2` (MySQL 8.3+, `query_plan` 루트).
    JsonV2,
    /// 알 수 없는 형태. 노드 추출은 시도하되 형식을 단정하지 않는다.
    Unknown,
}

impl PlanFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JsonV1 => "json_v1",
            Self::JsonV2 => "json_v2",
            Self::Unknown => "json_unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanWarning {
    /// `access_type=ALL` — 풀 테이블 스캔.
    FullTableScan {
        table: String,
        rows: Option<u64>,
    },
    /// 인덱스를 아예 쓰지 않았다.
    NoIndexUsed {
        table: String,
    },
    /// `access_type=index` — 인덱스 전체 스캔(커버링이라도 비싸다).
    FullIndexScan {
        table: String,
        key: String,
    },
    /// 후보 인덱스 자체가 없다 → 인덱스 추가 대상.
    NoPossibleKeys {
        table: String,
    },
    /// 조건 선택도가 낮다 — 읽고 버리는 행이 많다.
    LowFiltered {
        table: String,
        filtered_pct: f64,
        rows: Option<u64>,
    },
    Filesort,
    TemporaryTable,
}

/// 이 크레이트의 의존성을 늘리지 않기 위해 `thiserror` 없이 손으로 구현한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    InvalidJson(String),
    /// JSON 이지만 플랜으로 보이지 않는다 (객체가 아니거나 노드가 하나도 없다).
    NotAPlan,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(m) => write!(f, "플랜 JSON 파싱 실패: {m}"),
            Self::NotAPlan => write!(f, "플랜 구조가 아니다"),
        }
    }
}
impl std::error::Error for ParseError {}

#[derive(Debug, Clone)]
pub struct ParsedPlan {
    pub format: PlanFormat,
    /// **리터럴이 마스킹된** 플랜 JSON. 저장·표시·AI 프롬프트에 쓰는 것은 이쪽이다.
    pub normalized_json: String,
    /// 접근 방식 구조의 해시 (앞 16 hex). 행수·비용 변동에는 반응하지 않는다.
    pub fingerprint: String,
    pub nodes: Vec<PlanNode>,
    /// `schema.table` 형태. 스키마를 알 수 없으면 테이블 이름만 담는다.
    pub referenced_tables: Vec<String>,
    pub warnings: Vec<PlanWarning>,
    pub query_cost: Option<f64>,
    pub max_rows_examined: Option<u64>,
    /// 마스킹에 실패해 `<redacted>` 로 대체된 필드 수. 0이 아니면 카운터를 올려야 한다.
    pub redactions: usize,
}

/// 플랜 JSON 을 파싱한다.
///
/// `default_schema` 는 세션의 `CURRENT_SCHEMA` 다. 플랜에서 스키마를 알 수 없는 테이블을
/// 한정하는 데 쓴다.
///
/// # 별칭 주의
///
/// `EXPLAIN FORMAT=JSON` 의 `table_name` 은 **별칭일 수 있다**(`FROM orders o` → `"o"`).
/// 그래서 `referenced_tables` 는 확정 목록이 아니라 후보다. 어드바이저는 이 목록을
/// `information_schema.TABLES` 로 검증하고, 검증에 실패한 이름은 SQL 파서 폴백으로 채운다
/// ([17](../../../.claude/docs/17-roadmap-tasks.md) M10-2).
pub fn parse(plan_json: &str, default_schema: Option<&str>) -> Result<ParsedPlan, ParseError> {
    let raw: serde_json::Value =
        serde_json::from_str(plan_json).map_err(|e| ParseError::InvalidJson(e.to_string()))?;
    if !raw.is_object() {
        return Err(ParseError::NotAPlan);
    }
    let format = detect_format(&raw);
    let (masked, redactions) = mask::mask_plan(&raw);
    let nodes = walk::collect_nodes(&masked);
    if nodes.is_empty() && format == PlanFormat::Unknown {
        return Err(ParseError::NotAPlan);
    }

    let referenced_tables = referenced_tables(&nodes, default_schema);
    let mut warnings = warnings(&nodes);
    // `using_filesort` / `using_temporary_table` 는 **테이블 노드가 아니라**
    // `ordering_operation` · `grouping_operation` · `duplicates_removal` 에 붙는다.
    // 노드만 훑으면 가장 흔한 튜닝 신호 두 개를 통째로 놓친다.
    let (filesort, temp_table) = scan_operation_flags(&masked);
    if filesort && !warnings.contains(&PlanWarning::Filesort) {
        warnings.push(PlanWarning::Filesort);
    }
    if temp_table && !warnings.contains(&PlanWarning::TemporaryTable) {
        warnings.push(PlanWarning::TemporaryTable);
    }
    let query_cost = find_query_cost(&masked);
    let max_rows_examined = nodes.iter().filter_map(|n| n.rows_examined_per_scan).max();
    let fingerprint = fingerprint(&nodes);

    Ok(ParsedPlan {
        format,
        normalized_json: masked.to_string(),
        fingerprint,
        nodes,
        referenced_tables,
        warnings,
        query_cost,
        max_rows_examined,
        redactions,
    })
}

/// 형식 판정.
///
/// # v2 루트는 **계획 노드 자신**이다 (8.4.11 실측)
///
/// 처음 판은 `query_plan` 래퍼를 가정했다. 실제 출력에는 그 래퍼가 없다:
///
/// ```json
/// { "query": "select …", "inputs": [ { "operation": "Nested loop inner join", … } ] }
/// ```
///
/// 그래서 실제 v2 계획이 `Unknown` 으로 분류됐다 — 저장 레코드의 `format_version` 이
/// "unknown" 이 되고, 테이블이 없는 v2 계획(`SELECT 1`)은 `NotAPlan` 으로 **버려진다.**
/// 래퍼 형태도 계속 받아들인다(있어서 손해 볼 것이 없다).
fn detect_format(v: &serde_json::Value) -> PlanFormat {
    if v.get("query_block").is_some() {
        PlanFormat::JsonV1
    } else if v.get("query_plan").is_some()
        || v.get("inputs").is_some()
        || v.get("operation").is_some()
    {
        PlanFormat::JsonV2
    } else {
        PlanFormat::Unknown
    }
}

/// 접근 방식 구조만으로 지문을 만든다.
///
/// **행수·비용·`filtered` 를 넣지 않는다.** 넣으면 데이터가 조금 늘어날 때마다 지문이
/// 바뀌어 `plan_change` 알림(M9-21)이 매시간 발화한다. 지문이 바뀌었다는 것은
/// "옵티마이저가 다른 길을 골랐다"를 뜻해야 한다.
pub fn fingerprint(nodes: &[PlanNode]) -> String {
    let mut sig = String::new();
    for n in nodes {
        sig.push_str(&format!(
            "{}|{}|{}|{}|{}|{}{}{}\n",
            n.depth,
            n.role,
            n.table_name.as_deref().unwrap_or("-"),
            n.access_type.as_deref().unwrap_or("-"),
            n.key.as_deref().unwrap_or("-"),
            n.used_key_parts.join(","),
            if n.using_filesort { "|fs" } else { "" },
            if n.using_temporary_table { "|tmp" } else { "" },
        ));
    }
    let h = Sha256::digest(sig.as_bytes());
    h.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn referenced_tables(nodes: &[PlanNode], default_schema: Option<&str>) -> Vec<String> {
    let hints = walk::schema_hints(nodes);
    let mut out: Vec<String> = Vec::new();
    for n in nodes.iter().filter(|n| n.is_real_table()) {
        let table = n.table_name.as_deref().unwrap_or_default();
        let qualified = hints
            .iter()
            .find(|(_, t)| t == table)
            .map(|(s, t)| format!("{s}.{t}"))
            .or_else(|| default_schema.map(|s| format!("{s}.{table}")))
            .unwrap_or_else(|| table.to_string());
        if !out.contains(&qualified) {
            out.push(qualified);
        }
    }
    out.sort();
    out
}

/// `filtered` 가 이 값 미만이면 선택도가 낮다고 본다.
const LOW_FILTERED_PCT: f64 = 10.0;
/// 선택도 경고를 낼 최소 스캔 행수. 작은 테이블에서는 낮은 선택도가 문제가 아니다.
const LOW_FILTERED_MIN_ROWS: u64 = 1_000;

fn warnings(nodes: &[PlanNode]) -> Vec<PlanWarning> {
    let mut out = Vec::new();
    for n in nodes {
        if !n.is_real_table() {
            // 가상 테이블에도 filesort/temp 는 붙을 수 있으므로 아래 플래그는 본다.
            if n.using_filesort && !out.contains(&PlanWarning::Filesort) {
                out.push(PlanWarning::Filesort);
            }
            if n.using_temporary_table && !out.contains(&PlanWarning::TemporaryTable) {
                out.push(PlanWarning::TemporaryTable);
            }
            continue;
        }
        let table = n.table_name.clone().unwrap_or_default();
        match n.access_type.as_deref() {
            Some("ALL") => out.push(PlanWarning::FullTableScan {
                table: table.clone(),
                rows: n.rows_examined_per_scan,
            }),
            Some("index") => out.push(PlanWarning::FullIndexScan {
                table: table.clone(),
                key: n.key.clone().unwrap_or_default(),
            }),
            _ => {}
        }
        if n.key.is_none() && n.access_type.as_deref() != Some("system") {
            out.push(PlanWarning::NoIndexUsed {
                table: table.clone(),
            });
        }
        if n.possible_keys.is_empty() && n.access_type.as_deref() == Some("ALL") {
            out.push(PlanWarning::NoPossibleKeys {
                table: table.clone(),
            });
        }
        if let Some(pct) = n.filtered_pct {
            let rows = n.rows_examined_per_scan.unwrap_or(0);
            if pct < LOW_FILTERED_PCT && rows >= LOW_FILTERED_MIN_ROWS {
                out.push(PlanWarning::LowFiltered {
                    table: table.clone(),
                    filtered_pct: pct,
                    rows: n.rows_examined_per_scan,
                });
            }
        }
        if n.using_filesort && !out.contains(&PlanWarning::Filesort) {
            out.push(PlanWarning::Filesort);
        }
        if n.using_temporary_table && !out.contains(&PlanWarning::TemporaryTable) {
            out.push(PlanWarning::TemporaryTable);
        }
    }
    out
}

/// 플랜 전체에서 `using_filesort` / `using_temporary_table` 를 찾는다.
///
/// 이 플래그는 위치가 고정되어 있지 않다. MySQL 은 정렬·그룹핑·중복제거 연산 객체에
/// 붙이는데, 어떤 객체가 생기는지는 최적화 경로에 따라 다르다. 위치를 가정하지 않고
/// 트리 전체에서 찾는다.
fn scan_operation_flags(v: &serde_json::Value) -> (bool, bool) {
    fn truthy(v: Option<&serde_json::Value>) -> bool {
        match v {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
            _ => false,
        }
    }
    fn scan(v: &serde_json::Value, fs: &mut bool, tmp: &mut bool) {
        match v {
            serde_json::Value::Object(m) => {
                *fs |= truthy(m.get("using_filesort"));
                *tmp |= truthy(m.get("using_temporary_table"));
                for child in m.values() {
                    scan(child, fs, tmp);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|c| scan(c, fs, tmp)),
            _ => {}
        }
    }
    let (mut fs, mut tmp) = (false, false);
    scan(v, &mut fs, &mut tmp);
    (fs, tmp)
}

/// **최상위** `cost_info.query_cost` 를 쓴다. 없으면 트리에서 가장 큰 값으로 대체한다.
///
/// 이전 구현은 doc 과 달리 항상 트리 전체의 최대값을 썼다. 서브쿼리 비용이 상위보다
/// 크면 그 값이 `query_cost` 로 보고되어, 리포트의 "가장 비싼 쿼리" 순위가 뒤틀린다.
/// v1 은 `query_block.cost_info.query_cost` 가 최상위이고, v2 는 그 키가 없어
/// 트리 최대값이 유일한 근거다 — 그래서 폴백을 남긴다.
fn find_query_cost(v: &serde_json::Value) -> Option<f64> {
    fn as_f64(v: &serde_json::Value) -> Option<f64> {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }
    fn scan(v: &serde_json::Value, best: &mut Option<f64>) {
        match v {
            serde_json::Value::Object(m) => {
                let candidate = m
                    .get("cost_info")
                    .and_then(|c| c.get("query_cost"))
                    .and_then(as_f64)
                    // v2 노드는 `estimated_total_cost` 를 쓴다.
                    .or_else(|| m.get("estimated_total_cost").and_then(as_f64));
                if let Some(c) = candidate {
                    *best = Some(best.map_or(c, |b: f64| b.max(c)));
                }
                for child in m.values() {
                    scan(child, best);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|c| scan(c, best)),
            _ => {}
        }
    }
    // ① v1 최상위: `query_block.cost_info.query_cost`.
    if let Some(top) = v
        .get("query_block")
        .or(Some(v))
        .and_then(|b| b.get("cost_info"))
        .and_then(|c| c.get("query_cost"))
        .and_then(as_f64)
    {
        return Some(top);
    }
    // ② **v2 최상위: `query_plan.estimated_total_cost`.**
    //
    // v2 는 `cost_info.query_cost` 를 쓰지 않는다 — 키 이름이 완전히 다르다.
    // 이걸 읽지 않으면 MySQL 9.x(v2 가 기본값)에서 `query_cost` 가 **항상 `None`** 이고
    // "가장 비싼 쿼리" 리포트가 빈다. 8.4.11 실측으로 확인했다.
    if let Some(top) = v
        .get("query_plan")
        .or(Some(v))
        .and_then(|b| b.get("estimated_total_cost"))
        .and_then(as_f64)
    {
        return Some(top);
    }
    // ② 없으면 트리 최대값 (v2 경로).
    let mut best = None;
    scan(v, &mut best);
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOIN_PLAN: &str = r#"{
      "query_block": {
        "select_id": 1,
        "cost_info": {"query_cost": "84213.55"},
        "ordering_operation": {
          "using_filesort": true,
          "nested_loop": [
            {"table": {"table_name": "o", "access_type": "ALL", "possible_keys": null,
                       "rows_examined_per_scan": 812345, "filtered": "3.33",
                       "attached_condition": "(`shop`.`o`.`status` = 'PENDING')"}},
            {"table": {"table_name": "i", "access_type": "ref", "key": "idx_order",
                       "used_key_parts": ["order_id"], "rows_examined_per_scan": 3,
                       "filtered": "100.00"}}
          ]
        }
      }
    }"#;

    #[test]
    fn parses_join_plan_end_to_end() {
        let p = parse(JOIN_PLAN, Some("shop")).unwrap();
        assert_eq!(p.format, PlanFormat::JsonV1);
        assert_eq!(p.nodes.len(), 2);
        assert_eq!(p.referenced_tables, vec!["shop.i", "shop.o"]);
        assert_eq!(p.query_cost, Some(84213.55));
        assert_eq!(p.max_rows_examined, Some(812345));
        assert_eq!(p.fingerprint.len(), 16);
        assert_eq!(p.redactions, 0);
    }

    #[test]
    fn plan_literals_never_reach_normalized_json() {
        let p = parse(JOIN_PLAN, Some("shop")).unwrap();
        assert!(
            !p.normalized_json.contains("PENDING"),
            "{}",
            p.normalized_json
        );
    }

    #[test]
    fn warnings_detected() {
        let w = parse(JOIN_PLAN, Some("shop")).unwrap().warnings;
        assert!(w.iter().any(|x| matches!(
            x,
            PlanWarning::FullTableScan {
                rows: Some(812345),
                ..
            }
        )));
        assert!(
            w.iter()
                .any(|x| matches!(x, PlanWarning::NoPossibleKeys { .. }))
        );
        assert!(
            w.iter()
                .any(|x| matches!(x, PlanWarning::LowFiltered { .. }))
        );
        assert!(w.contains(&PlanWarning::Filesort));
    }

    #[test]
    fn fingerprint_ignores_row_counts_but_tracks_access_change() {
        let cheaper = JOIN_PLAN
            .replace("812345", "42")
            .replace("\"3.33\"", "\"99.00\"");
        assert_eq!(
            parse(JOIN_PLAN, None).unwrap().fingerprint,
            parse(&cheaper, None).unwrap().fingerprint,
            "행수·선택도 변화로 지문이 바뀌면 plan_change 알림이 매시간 발화한다"
        );

        let worse = JOIN_PLAN.replace(
            r#""access_type": "ref", "key": "idx_order""#,
            r#""access_type": "ALL""#,
        );
        assert_ne!(
            parse(JOIN_PLAN, None).unwrap().fingerprint,
            parse(&worse, None).unwrap().fingerprint,
            "접근 방식이 바뀌면 지문이 달라져야 한다"
        );
    }

    #[test]
    fn json_v2_root_detected() {
        let wrapped = r#"{"query_plan": {"operation": "Table scan on o", "table_name": "o", "access_type": "ALL"}}"#;
        assert_eq!(parse(wrapped, None).unwrap().format, PlanFormat::JsonV2);
    }

    /// **실제 v2 출력에는 `query_plan` 래퍼가 없다** (8.4.11 실측). 루트가 계획 노드다.
    ///
    /// 래퍼를 가정한 판정은 실제 계획을 `Unknown` 으로 분류했다 — 저장 레코드의
    /// `format_version` 이 "unknown" 이 되고, **테이블 없는 v2 계획은 `NotAPlan` 으로
    /// 버려졌다.** 가정으로 쓴 위 테스트는 통과하면서 실제만 틀린 상태였다.
    #[test]
    fn real_v2_output_without_a_wrapper_is_detected() {
        let real = r#"{
          "query": "/* select#1 */ select count(0) from `shop`.`orders`",
          "inputs": [{
            "operation": "Aggregate: count(0)",
            "access_type": "aggregate",
            "estimated_rows": 1.0,
            "estimated_total_cost": 6074.55,
            "inputs": [{
              "alias": "orders", "table_name": "orders", "schema_name": "shop",
              "operation": "Table scan on orders", "access_type": "table",
              "estimated_rows": 60023.0, "estimated_total_cost": 6074.55
            }]
          }]
        }"#;
        let p = parse(real, Some("shop")).unwrap();
        assert_eq!(p.format, PlanFormat::JsonV2);
        // 노드 수집은 `table_name` 을 찾아 트리를 훑으므로 루트 모양과 무관하다.
        assert_eq!(p.referenced_tables, vec!["shop.orders"]);
        // 비용은 루트에서 읽는다 — v2 는 `cost_info` 를 쓰지 않는다.
        assert_eq!(p.query_cost, Some(6074.55));
    }

    /// 테이블이 없는 v2 계획도 **계획이다.** 예전 판정에서는 노드가 0개 + `Unknown` 이라
    /// `NotAPlan` 으로 버려졌다 — `SELECT 1` 같은 쿼리의 계획이 통째로 사라진다.
    #[test]
    fn a_v2_plan_without_tables_is_still_a_plan() {
        let no_tables = r#"{"query": "select 1", "operation": "Rows fetched before execution",
                            "access_type": "rows_fetched_before_execution"}"#;
        let p = parse(no_tables, None).expect("계획으로 받아야 한다");
        assert_eq!(p.format, PlanFormat::JsonV2);
        assert!(p.nodes.is_empty());
    }

    #[test]
    fn update_plan_is_parsed() {
        // DML 플랜도 노드를 얻어야 한다 (ADR-006 의 요점: UPDATE 도 커버한다).
        let upd = r#"{"query_block": {"select_id": 1,
            "table": {"update": true, "table_name": "orders", "access_type": "range",
                      "key": "PRIMARY", "rows_examined_per_scan": 10}}}"#;
        let p = parse(upd, Some("shop")).unwrap();
        assert_eq!(p.referenced_tables, vec!["shop.orders"]);
    }

    #[test]
    fn derived_table_excluded_from_references() {
        let plan = r#"{"query_block": {"table": {"table_name": "<derived2>", "access_type": "ALL",
            "materialized_from_subquery": {"query_block": {"table": {"table_name": "src", "access_type": "ALL"}}}}}}"#;
        let p = parse(plan, Some("shop")).unwrap();
        assert_eq!(p.referenced_tables, vec!["shop.src"]);
    }

    #[test]
    fn hostile_input_returns_error_not_panic() {
        for bad in [
            "",
            "not json",
            "[]",
            "null",
            "123",
            r#""str""#,
            "{}",
            r#"{"a":1}"#,
        ] {
            let _ = parse(bad, None); // 패닉하지 않는 것이 요구사항
        }
        assert!(parse("{}", None).is_err());
        assert!(parse("nope", None).is_err());
    }

    #[test]
    fn deeply_nested_json_does_not_panic() {
        let deep = format!("{}{}{}", "{\"a\":".repeat(200), "1", "}".repeat(200));
        let _ = parse(&deep, None);
    }
}
