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
/// # 손으로 쓴 스캐너를 버리고 렉서를 쓴다
///
/// 2차에서 인용부호를 직접 훑는 스캐너를 썼는데 이스케이프를 몰라서 **리터럴이 그대로
/// 새어 나갔다.** `redactions` 도 올리지 않아 아무도 알아채지 못하는 형태였다(1차 L1 과 동형):
///
/// ```text
/// IN : Filter: (t.memo = 'It\'s a secret')
/// OUT: Filter: (t.memo = ?s a secret?          ← "secret" 유출, redactions=0
/// IN : Filter: (t.blob = 0x536563726574)
/// OUT: Filter: (t.blob = ?x536563726574)       ← 16진수가 "Secret" 이다
/// ```
///
/// 랜덤 퍼징에서 30.1% 가 유출됐다. 렉서는 `\'`·`''`·`0x`·`b'`·지수 표기를 **이미**
/// 정확히 처리하므로, 직접 훑는 대신 **토큰 스팬**을 받아 리터럴 구간만 치환한다.
/// 라벨의 가독성(대소문자·공백)은 리터럴 밖 원문을 그대로 복사해 유지한다.
///
/// ```text
/// IN : Limit: 10 row(s)                        OUT: Limit: ? row(s)
/// IN : Table scan on t1  (cost=1.25 rows=5)    OUT: Table scan on t1  (cost=? rows=?)
/// ```
///
/// # 애매하면 포기한다 (fail-closed)
///
/// 인용부호가 닫히지 않았으면 리터럴 경계를 알 수 없다. 그때는 [`REDACTED`] 를 반환하고
/// `redactions` 를 올린다 — 이전 구현에는 이 경로가 아예 없었다.
fn mask_label(s: &str, redactions: &mut usize) -> String {
    use dbmon_normalize::lexer::{Lexer, Tok};

    let (toks, spans, unterminated) = Lexer::new(s).tokenize_with_spans();
    if unterminated {
        // 닫히지 않은 인용부호 → 어디까지가 리터럴인지 알 수 없다.
        *redactions += 1;
        return REDACTED.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    for (tok, span) in toks.iter().zip(spans.iter()) {
        if span.start < cursor || span.end > s.len() {
            *redactions += 1;
            return REDACTED.to_string();
        }
        // **토큰 사이의 간격은 공백뿐이어야 한다.**
        //
        // 렉서는 주석을 토큰으로 만들지 않고 **버린다.** 버려진 바이트는 스팬 사이의
        // 간격에 남고, 그 간격을 원문 복사하면 주석 안의 리터럴이 그대로 나간다.
        // MySQL 8.4.11 이 이걸 실제로 만든다 — v2 의 `operation` 은 테이블 별칭을
        // **백틱 없이** 출력하므로 공격자가 별칭을 `c/*` 로 지으면 라벨의 나머지가
        // 주석이 된다:
        //
        // ```text
        // SELECT name FROM customers AS `c/*` WHERE email >= 'victim-ssn-...@example.com'
        //   → "operation": "Index range scan on c/* using uk_customers_email
        //                   over ('victim-ssn-...@example.com' <= email), ..."
        // ```
        //
        // 닫히지 않은 `/*` 는 `unterminated` 를 세우지 않으므로 위 가드도 발동하지 않았다.
        // 퍼징에서 19.2% 가 이 경로로 유출됐다.
        //
        // 대상 DB 에 쿼리를 날릴 수 있는 누구나 별칭을 고른다 — 의도적 exfiltration 수단이다.
        let gap = &s[cursor..span.start];
        if !gap.chars().all(char::is_whitespace) {
            *redactions += 1;
            return REDACTED.to_string();
        }
        out.push_str(gap);

        let is_literal = matches!(
            tok,
            Tok::Placeholder
                | Tok::IntroducedLiteral
                | Tok::Param
                | Tok::Ellipsis
                // 힌트 **본문**에 리터럴이 들어간다 (`SET_VAR(sql_mode='...')`).
                // 라벨에 힌트가 나올 일은 없지만, 별칭으로 `/*+` 를 만들 수 있다.
                | Tok::Hint(_)
        );
        if is_literal {
            out.push('?');
        } else {
            // **리터럴이 아닌 토큰의 원문에도 인용부호가 들어갈 수 있다.**
            //
            // 백틱 식별자는 임의 내용을 담는다. 라벨에 짝 없는 백틱이 섞이면 렉서가
            // 그 뒤 전체를 **하나의 식별자**로 읽고, 원문 복사가 그 안의 리터럴을 그대로
            // 내보낸다. 내 속성 테스트가 이걸 찾았다:
            //
            // ```text
            // Filter: ` (t.c = 'ssn900101')  idx `
            //   → 전체가 백틱 식별자 하나 → 원문 복사 → 'ssn900101' 유출
            // ```
            //
            // 간격 검사(공백만 허용)로는 못 잡는다 — 내용이 **토큰 안**에 있다.
            //
            // 백틱 자체는 허용한다. MySQL 이 라벨에서 식별자를 감쌀 때 쓰기 때문이다
            // (`` (`c/*`.email >= ?) ``). 위험한 것은 식별자 안의 `'`·`"` 다.
            let raw = &s[span.start..span.end];
            if !raw_matches_token_kind(raw) {
                *redactions += 1;
                return REDACTED.to_string();
            }
            out.push_str(raw);
        }
        cursor = span.end;
    }

    // 마지막 토큰 뒤의 꼬리도 공백뿐이어야 한다.
    let tail = &s[cursor..];
    if !tail.chars().all(char::is_whitespace) {
        *redactions += 1;
        return REDACTED.to_string();
    }
    out.push_str(tail);
    out
}

/// 토큰의 **원문 바이트가 그 토큰 종류와 일치하는가.**
///
/// # 다섯 번째 유출이 여기서 나왔다
///
/// `mask_label` 은 리터럴이 아닌 토큰의 원문을 그대로 복사해 가독성을 유지한다. 그게
/// 안전한 전제는 **원문이 렉서가 분류한 종류와 같은 모양**일 때뿐이다. 짝 없는 백틱이
/// 있으면 렉서는 그 뒤 전체를 **식별자 하나**로 삼키고, 원문 복사가 그 안의 리터럴을
/// 내보낸다. 4차의 `'`·`"` 검사는 **인용부호 없는** 리터럴을 통과시켰다:
///
/// ```text
/// IN : Filter: ` (t.c = 0x536563726574)  idx `
/// OUT: 입력과 동일, redactions=0        ← 0x536563726574 = "Secret"
/// ```
///
/// 숫자·16진수·실수 각각 2,197 조합 중 21건(1.0%)이 유출됐다. 인용된 값은 막혔으므로
/// 4차 테스트는 자기가 방어하는 모양만 측정하고 있었다.
///
/// # 판정
///
/// 백틱 식별자만 임의 내용을 담을 수 있다 — 다른 종류는 렉서의 토큰 경계가 이미
/// 모양을 제한한다. 그래서 백틱 안의 내용이 **식별자인지** 본다. 실제 MySQL 라벨은
/// `` `c` ``·`` `email` ``·`` `한글컬럼` `` 처럼 단순한 이름을 쓰므로 가용성은 유지된다.
///
/// 공백이 든 컬럼명(`` `my col` ``)은 이 규칙에서 `REDACTED` 가 된다. 드물고, 안전한
/// 방향이며, 라벨 하나를 잃는 것이 리터럴을 내보내는 것보다 낫다.
fn raw_matches_token_kind(raw: &str) -> bool {
    // 인용부호는 어느 종류에도 나올 수 없다.
    if raw.contains('\'') || raw.contains('"') {
        return false;
    }
    if !raw.starts_with('`') {
        return true;
    }
    // 백틱 식별자: 내용이 식별자 문자로만 이뤄져야 한다.
    let inner = raw.trim_matches('`');
    inner
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
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
        return mask_label(s, redactions);
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

    /// **이스케이프된 인용부호와 MySQL 수치 리터럴 형태 전부에서 유출이 없어야 한다.**
    ///
    /// 2차의 손으로 쓴 스캐너는 이 다섯 중 다섯을 모두 유출했고 `redactions` 도 0 이었다.
    /// 실제 8.4.11 플랜 출력에서 나오는 형태다.
    #[test]
    fn label_masking_handles_every_mysql_literal_form() {
        let cases: &[(&str, &str)] = &[
            (r"Filter: (t.memo = 'It\'s a secret')", "secret"),
            (r#"Filter: (t.memo = "say \"hunter2\" now")"#, "hunter2"),
            // 16진수는 디코드하면 "Secret" 이다.
            ("Filter: (t.blob = 0x536563726574)", "536563726574"),
            // 이스케이프가 짝을 뒤집으면 **다음** 리터럴이 통째로 드러났다.
            (r"Filter: (t.a = 'p\'q' and t.b = 'topsecret')", "topsecret"),
            ("Filter: (t.b = 0b0101)", "0101"),
            ("Filter: (t.c = 1.5e10)", "5e10"),
            ("Filter: (t.d = 0X4A)", "4A"),
            (r"Filter: (t.e = b'0110')", "0110"),
            // 연속 인용부호 이스케이프.
            ("Filter: (t.f = 'a''b secret2')", "secret2"),
        ];
        for (input, must_not_survive) in cases {
            let plan = serde_json::json!({ "query_plan": { "operation": input } });
            let (masked, _) = mask_plan(&plan);
            let got = serde_json::to_string(&masked).expect("직렬화");
            assert!(
                !got.contains(must_not_survive),
                "리터럴 유출: {must_not_survive}\n  입력: {input}\n  출력: {got}"
            );
        }
    }

    /// 닫히지 않은 인용부호는 **경계를 알 수 없으므로 전체를 버린다** (fail-closed).
    /// 이전 구현에는 이 경로가 아예 없었다.
    #[test]
    fn unterminated_quote_in_label_fails_closed() {
        let plan = serde_json::json!({
            "query_plan": { "operation": "Filter: (t.a = 'unclosed secret3" }
        });
        let (masked, redactions) = mask_plan(&plan);
        let got = serde_json::to_string(&masked).expect("직렬화");
        assert!(!got.contains("secret3"), "유출: {got}");
        assert!(got.contains(REDACTED), "REDACTED 로 대체되지 않았다: {got}");
        assert_eq!(redactions, 1, "관측 가능해야 한다 — 조용히 버리면 안 된다");
    }

    /// `heading` 은 `LABEL_KEYS` 에만 있어야 한다. 양쪽에 두면 `EXPR_KEYS` 쪽이
    /// 죽은 코드가 되고, 주석이 실제 동작을 설명하지 않게 된다.
    #[test]
    fn label_keys_and_expr_keys_do_not_overlap() {
        let dup: Vec<&&str> = LABEL_KEYS
            .iter()
            .filter(|k| EXPR_KEYS.contains(k))
            .collect();
        assert!(dup.is_empty(), "두 목록에 겹치는 키가 있다: {dup:?}");
    }

    /// **인용된 리터럴 안의 값은 어떤 문맥에서도 살아남지 못한다.**
    ///
    /// 리뷰어가 이 성질로 19.2% 유출을 찾았다(주석 경로). 성질을 저장소 테스트로
    /// 남겨야 다음 회귀가 잡힌다 — 외부 퍼징은 한 번 돌고 사라진다.
    ///
    /// 마커를 **항상 인용부호 안에** 넣고, 그 바깥을 온갖 조합으로 흔든다.
    #[test]
    fn quoted_values_never_survive_in_any_context() {
        const MARKER: &str = "ssn900101";
        // 라벨 바깥을 흔드는 조각들. 주석 시작·백틱·이스케이프·도입자를 포함한다.
        let noise = [
            "", " ", "/*", "*/", "--", "#", "`", "\\", "x", "_binary", "(", ")", "/*+",
        ];

        let mut checked = 0usize;
        for a in noise {
            for b in noise {
                for c in noise {
                    // 마커는 항상 `'...'` 안에 있다.
                    let label = format!("Filter: {a} (t.c = '{MARKER}') {b} idx {c}");
                    let plan = serde_json::json!({ "query_plan": { "operation": label } });
                    let (masked, _) = mask_plan(&plan);
                    let out = serde_json::to_string(&masked).expect("직렬화");
                    assert!(
                        !out.contains(MARKER),
                        "유출\n  라벨: {label}\n  출력: {out}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, noise.len().pow(3));
    }

    /// **인용부호 없는 리터럴도 살아남지 못한다.**
    ///
    /// 4차의 검사는 리터럴이 아닌 토큰의 원문에서 `\'`·`"` 만 걸렀다. 짝 없는 백틱이
    /// 렉서로 하여금 뒤 전체를 **식별자 하나**로 삼키게 만들면, 그 안의 숫자·16진수·실수는
    /// 인용부호가 없으므로 통과했다:
    ///
    /// ```text
    /// IN : Filter: ` (t.c = 0x536563726574)  idx `
    /// OUT: 입력과 동일, redactions=0        ← 0x536563726574 = "Secret"
    /// ```
    ///
    /// 5차 실측으로 세 형태 각각 21/2,197 (1.0%) 유출.
    #[test]
    fn unquoted_literals_never_survive_either() {
        // 16진수는 디코드하면 "Secret", 숫자는 주민번호 형태, 실수도 값이다.
        const MARKERS: [&str; 3] = ["0x536563726574", "9001011234567", "1.5e300"];
        let noise = [
            "", " ", "/*", "*/", "--", "#", "`", "\\", "x", "_binary", "(", ")", "/*+",
        ];

        let mut checked = 0usize;
        for marker in MARKERS {
            for a in noise {
                for b in noise {
                    for c in noise {
                        let label = format!("Filter: {a} (t.c = {marker}) {b} idx {c}");
                        let plan = serde_json::json!({ "query_plan": { "operation": label } });
                        let (masked, _) = mask_plan(&plan);
                        let out = serde_json::to_string(&masked).expect("직렬화");
                        assert!(
                            !out.contains(marker),
                            "인용 없는 리터럴 유출\n  라벨: {label}\n  출력: {out}"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, MARKERS.len() * noise.len().pow(3));
    }

    /// **실서버 라벨은 여전히 온전히 읽혀야 한다.** 백틱 식별자를 통째로 막으면
    /// 대부분의 v2 라벨이 `REDACTED` 가 되어 화면이 쓸모없어진다.
    #[test]
    fn real_mysql_labels_stay_fully_readable() {
        let cases: &[(&str, &str)] = &[
            (
                "Filter: ((`c`.email >= 'x@example.com') and (`c`.id < 500))",
                "Filter: ((`c`.email >= ?) and (`c`.id < ?))",
            ),
            ("Filter: (`한글컬럼` = 'v')", "Filter: (`한글컬럼` = ?)"),
            ("Limit: 10 row(s)", "Limit: ? row(s)"),
            (
                "Single-row index lookup on o using PRIMARY (id=5)",
                "Single-row index lookup on o using PRIMARY (id=?)",
            ),
        ];
        for (input, expected) in cases {
            let plan = serde_json::json!({ "query_plan": { "operation": input } });
            let (masked, redactions) = mask_plan(&plan);
            let got = masked
                .get("query_plan")
                .and_then(|q| q.get("operation"))
                .and_then(|o| o.as_str())
                .expect("operation");
            assert_eq!(got, *expected, "\n  입력: {input}");
            assert_eq!(redactions, 0, "실서버 라벨이 REDACTED 됐다: {input}");
        }
    }

    /// 같은 성질을 **`heading`** 과 알 수 없는 키에서도 확인한다.
    /// `LABEL_KEYS` 가 다른 목록보다 먼저 검사되므로 경로가 다르다.
    #[test]
    fn quoted_values_never_survive_for_any_key() {
        const MARKER: &str = "ssn900101";
        let label = format!("Filter: /* (t.c = '{MARKER}')");
        for key in [
            "operation",
            "heading",
            "condition",
            "attached_condition",
            "future_key",
        ] {
            let plan = serde_json::json!({ "query_plan": { key: label.clone() } });
            let (masked, _) = mask_plan(&plan);
            let out = serde_json::to_string(&masked).expect("직렬화");
            assert!(!out.contains(MARKER), "키 {key} 에서 유출: {out}");
        }
    }

    /// 패닉하지 않아야 한다 — 릴리스 빌드는 `panic = "abort"` 라 한 번이면 컨테이너가 죽는다.
    #[test]
    fn label_masking_never_panics() {
        let alphabet = [
            '\'', '"', '`', '\\', '0', 'x', 'b', 'e', '.', ' ', '(', ')', '한', '📊',
        ];
        let mut count = 0usize;
        for a in alphabet {
            for b in alphabet {
                for c in alphabet {
                    for d in alphabet {
                        let input: String = [a, b, c, d].iter().collect();
                        let plan = serde_json::json!({ "query_plan": { "operation": input } });
                        let (masked, _) = mask_plan(&plan);
                        // 출력이 유효한 UTF-8 문자열이어야 한다.
                        let _ = serde_json::to_string(&masked).expect("직렬화");
                        count += 1;
                    }
                }
            }
        }
        assert_eq!(count, alphabet.len().pow(4));
    }

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
