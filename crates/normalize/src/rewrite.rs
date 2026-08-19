//! DML → 플랜 조회용 `SELECT` 변환.
//!
//! # 왜 필요한가 (M1-4b 실측)
//!
//! 읽기 전용 모니터링 계정으로는 실행계획을 얻을 방법이 둘뿐이다.
//!
//! | 경로 | 최소 권한 계정 |
//! |---|---|
//! | `EXPLAIN ... FOR CONNECTION <타인>` | ✗ 정적 전역 권한 전체 필요 → RDS 불가 |
//! | `EXPLAIN FORMAT=JSON <SELECT>` | ✓ `SELECT` 권한만 |
//! | `EXPLAIN FORMAT=JSON <UPDATE/DELETE/INSERT>` | ✗ `ERROR 1142` — 해당 DML 권한 필요 |
//!
//! `EXPLAIN UPDATE` 는 데이터를 변경하지 않지만 `UPDATE` 권한을 요구한다. 읽기 전용
//! 원칙을 지키려면 **조건절을 `SELECT` 로 바꿔** 근사 플랜을 얻어야 한다.
//! → [19 §B](../../../docs/19-m1-findings.md)
//!
//! # 무엇이 보존되고 무엇이 빠지는가
//!
//! | 보존 | 빠짐 |
//! |---|---|
//! | 행을 찾아가는 접근 경로 (`access_type`, `key`, `rows`) | 보조 인덱스 갱신 비용 |
//! | 조인 순서·조인 방식 | 트리거·외래키 검사 |
//! | `WHERE` 조건의 선택도 | 잠금 범위 |
//!
//! 튜닝 대상은 거의 항상 "행을 어떻게 찾는가"이므로 실용적이다. 다만 **근사**이므로
//! `plan_source=rerun_as_select` 로 표시하고 UI 에 배지를 붙인다.
//!
//! # 원문 리터럴을 보존한다
//!
//! 정규화된 텍스트(`?`)로 `EXPLAIN` 하면 옵티마이저가 범위를 추정할 수 없어 플랜이
//! 달라진다. 그래서 [`crate::lexer::Lexer::tokenize_with_spans`] 로 **원문을 잘라 붙인다.**

use crate::lexer::{Lexer, Tok};
use crate::stmt_type::StatementType;

/// 변환 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanQuery {
    /// `EXPLAIN FORMAT=JSON` 에 붙여 실행할 문장.
    pub sql: String,
    /// 원문 그대로인가. `false` 면 근사 플랜이다.
    pub is_exact: bool,
}

/// 플랜을 얻기 위해 실행할 문장을 만든다.
///
/// - `SELECT` → 원문 그대로 (`is_exact = true`)
/// - `UPDATE` / `DELETE` → 조건절을 `SELECT` 로 (`is_exact = false`)
/// - `INSERT ... SELECT` / `REPLACE ... SELECT` → 내부 `SELECT` (`is_exact = false`)
/// - `INSERT ... VALUES` / DDL / 기타 → `None` (플랜이 의미 없다)
pub fn plan_query(sql: &str) -> Option<PlanQuery> {
    // **버전 조건 주석을 거부한다.** `/*!NNNNN ... */` 의 내용은 렉서가 버리지만
    // 서버 버전이 NNNNN 이상이면 MySQL 이 **실행한다**. SELECT 경로는 검증한 토큰이 아니라
    // 원문 부분문자열을 그대로 서버에 보내므로, 렉서가 못 본 코드가 실행된다
    // (검증기-실행기 불일치). 실측:
    //
    // ```text
    // SELECT COUNT(*) FROM orders WHERE id=1              → 1행
    // SELECT COUNT(*) FROM orders WHERE id=1 /*!11111 OR 1=1 */ → 60,000행
    // ```
    //
    // 우리 정규화 결과는 두 입력 모두 `SELECT count ( * ) FROM orders WHERE id = ?` 다.
    // SQL 은 `information_schema.PROCESSLIST.INFO` 에서 온다 — 우리가 쓴 문자열이 아니다.
    //
    // `/*+ ... */`(옵티마이저 힌트)는 무해하므로 막지 않는다. `/*!` 만 거부한다.
    if sql.contains("/*!") {
        return None;
    }
    let (toks, spans, unterminated) = Lexer::new(sql).tokenize_with_spans();
    // **바인딩 자리표가 남은 텍스트를 거부한다.**
    //
    // 전문 SQL 이 없으면 호출자가 `DIGEST_TEXT` 를 넘긴다. 거기에는 리터럴이 `?` 로,
    // 축약된 그룹이 `(...)` 로 치환돼 있다. 그걸 `EXPLAIN` 뒤에 붙이면 prepared statement
    // 밖이라 **항상 `ERROR 1064`** 다 — 즉 실패가 보장된 쿼리를 대상 DB 로 보낸다.
    // `max_plan_attempts` 만큼 반복되므로 "대상 DB 에 부하를 주지 않는다" 는 전제와 어긋난다.
    //
    // 리터럴 **안**의 `?` 는 무해하다(`WHERE memo = 'why?'`). 그래서 문자열이 아니라
    // **토큰**을 본다 — 그게 자리표와 실제 물음표를 가르는 기준이다.
    // 렉서는 이미 둘을 구분한다: `Tok::Param` 은 **입력에 있던** `?`,
    // `Tok::Placeholder` 는 우리가 리터럴을 마스킹한 결과다. 전자만 거부한다.
    if toks.iter().any(|t| matches!(t, Tok::Param | Tok::Ellipsis)) {
        return None;
    }
    if unterminated {
        // 인용부호가 닫히지 않았다 → 잘린 SQL 이다. 실행하면 구문 오류이거나,
        // 더 나쁘게는 **의도와 다른 문장**이 된다. 시도하지 않는다.
        return None;
    }
    // **멀티문장을 거부한다.** 이 문장은 `EXPLAIN FORMAT=JSON ` 뒤에 붙어 대상 DB 에서
    // 실행된다. 중간에 `;` 가 있으면 두 번째 문장이 EXPLAIN 없이 실행될 수 있다
    // (드라이버가 멀티문장을 막지만, 방어를 드라이버 설정에만 의존하지 않는다).
    if toks
        .iter()
        .enumerate()
        .any(|(i, t)| matches!(t, Tok::Punct(';')) && i + 1 != toks.len())
    {
        return None;
    }
    let first = first_meaningful(&toks)?;
    let kw = match &toks[first] {
        Tok::Kw(k) => k.as_str(),
        _ => return None,
    };
    let end = trimmed_end(sql, &toks, &spans);

    match kw {
        "SELECT" => Some(strip_resource_hints(sql, &toks, &spans, first, end)),
        // `WITH` 는 첫 키워드만으로 판정할 수 없다. `WITH c AS (...) UPDATE t ...` 는
        // MySQL 8 의 유효 문법이고, 그걸 원문 그대로 통과시키면 **관측 도구가 EXPLAIN 을
        // 붙여 DML 을 서버로 보낸다.** `classify` 는 이미 괄호 깊이를 보고 `Update` 로
        // 정확히 판정하는데 여기서 그 결과를 쓰지 않았다 — 방어가 배선되지 않았다.
        "WITH" => match crate::stmt_type::classify(&toks) {
            StatementType::Select => Some(strip_resource_hints(sql, &toks, &spans, first, end)),
            // CTE 뒤의 DML 은 재작성 대상이 아니다(어느 절이 조건절인지 이 코드는 모른다).
            // 플랜을 포기한다 — 잘못된 문장을 보내는 것보다 낫다.
            _ => None,
        },
        "UPDATE" => rewrite_update(sql, &toks, &spans, first, end),
        "DELETE" => rewrite_delete(sql, &toks, &spans, first, end),
        "INSERT" | "REPLACE" => rewrite_insert(sql, &toks, &spans, first, end),
        _ => None,
    }
}

/// **자원 제어 힌트를 제거한다.** 그건 우리가 대상 DB 에 걸어 둔 유일한 서버측 상한을
/// 공격자가 무력화하는 수단이다.
///
/// `plan_query` 는 옵티마이저 힌트(`/*+ ... */`)를 의도적으로 통과시킨다 — 힌트가 플랜을
/// 바꾸므로 보존해야 정확한 플랜이 나온다. 그런데 힌트 중 두 종류는 플랜이 아니라
/// **실행 자원**을 바꾼다. 8.4.11 실측:
///
/// ```text
/// SET SESSION max_execution_time = 1000;
/// SELECT SLEEP(2)                                        → 1 (죽었다)
/// SELECT /*+ MAX_EXECUTION_TIME(600000) */ SLEEP(2)      → 0 (완료됐다)
/// SELECT /*+ SET_VAR(max_execution_time=0) */ @@SESSION.max_execution_time → 0
/// ```
///
/// SQL 은 `PROCESSLIST.INFO` 에서 온다 — 대상 인스턴스에 쿼리를 날릴 수 있는 누구나
/// 힌트를 통제한다. `Timeouts::query` 는 클라이언트를 취소할 뿐 서버 작업은 계속되므로,
/// `max_execution_time` 이 유일한 서버측 바운드다.
///
/// | 힌트 | 처리 | 이유 |
/// |---|---|---|
/// | `MAX_EXECUTION_TIME(n)` | 제거, `is_exact` 유지 | 런타임 상한이라 플랜에 영향 없다 |
/// | `SET_VAR(...)` | 제거, **`is_exact = false`** | `optimizer_switch` 등으로 플랜을 바꿀 수 있다 |
/// | 그 외 (`INDEX`, `JOIN_ORDER`, `NO_ICP` …) | 보존 | 플랜을 결정하므로 있어야 정확하다 |
fn strip_resource_hints(
    sql: &str,
    toks: &[Tok],
    spans: &[std::ops::Range<usize>],
    first: usize,
    end: usize,
) -> PlanQuery {
    let start = spans[first].start;
    // (스팬, 대체 문자열) — 대체가 `None` 이면 힌트 주석을 통째로 지운다.
    let mut edits: Vec<(std::ops::Range<usize>, Option<String>)> = Vec::new();
    let mut plan_may_differ = false;

    for (i, t) in toks.iter().enumerate() {
        let Tok::Hint(body) = t else { continue };
        let (kept, removed_any, removed_set_var) = filter_hint_body(body);
        if !removed_any {
            continue;
        }
        plan_may_differ |= removed_set_var;
        let span = &spans[i];
        if span.start < start || span.end > end {
            continue;
        }
        // **남은 힌트를 보존한다.** MySQL 은 한 주석에 힌트 여러 개를 허용하므로
        // 주석을 통째로 지우면 플랜 힌트까지 사라진다. 8.4.11 실측:
        //
        // ```text
        // /*+ NO_RANGE_OPTIMIZATION(orders PRIMARY) MAX_EXECUTION_TIME(600000) */
        //   있음: access_type=index  key=idx_orders_customer  rows=60023
        //   통째로 지움: access_type=range  key=PRIMARY  rows=100
        // ```
        //
        // 그걸 `is_exact = true` 로 저장하면 60,023행 스캔 쿼리를 100행 플랜으로 보여준다.
        let replacement = kept
            .as_ref()
            .filter(|k| !k.trim().is_empty())
            .map(|k| format!("/*+{k}*/"));
        edits.push((span.clone(), replacement));
    }

    if edits.is_empty() {
        return PlanQuery {
            sql: sql[start..end].to_string(),
            is_exact: true,
        };
    }

    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(end - start);
    let mut cursor = start;
    for (cut, replacement) in edits {
        if cut.start > cursor {
            out.push_str(&sql[cursor..cut.start]);
        }
        match replacement {
            Some(text) => out.push_str(&text),
            // **공백 한 칸을 넣는다.** 아무것도 넣지 않으면 양옆 문자가 붙어
            // 다른 토큰이 된다. 8.4.11 실측 — 둘 다 유효한 문장이다:
            //
            // ```text
            // 원문:   SELECT a FROM t WHERE x = 5 -/*+ MAX_EXECUTION_TIME(9) */- 3   (x = 8)
            // 접합후: SELECT a FROM t WHERE x = 5 -- 3                              (x = 5)
            // ```
            //
            // `-` + `-` 가 라인 주석이 되어 뒤가 조용히 사라진다.
            // `SELECT/*+h*/a` → `SELECTa` 도 같은 원인이다.
            None => out.push(' '),
        }
        cursor = cut.end.max(cursor);
    }
    out.push_str(&sql[cursor..end]);
    PlanQuery {
        sql: out,
        is_exact: !plan_may_differ,
    }
}

/// 힌트 본문에서 **자원 제어 힌트만** 걸러낸다.
///
/// 반환: `(남은 본문, 제거했는가, SET_VAR 를 제거했는가)`.
/// 남은 본문이 `None` 이면 전부 자원 힌트였다는 뜻이다.
fn filter_hint_body(body: &str) -> (Option<String>, bool, bool) {
    /// 플랜이 아니라 **실행 자원**을 바꾸는 힌트.
    const RESOURCE_HINTS: [&str; 3] = ["SET_VAR", "MAX_EXECUTION_TIME", "RESOURCE_GROUP"];

    let b = body.as_bytes();
    let mut kept = String::with_capacity(body.len());
    let mut removed_any = false;
    let mut removed_set_var = false;
    let mut i = 0usize;

    while i < b.len() {
        if !(b[i].is_ascii_alphabetic() || b[i] == b'_') {
            kept.push(body[i..].chars().next().expect("경계"));
            i += body[i..].chars().next().expect("경계").len_utf8();
            continue;
        }
        // 식별자를 읽는다.
        let name_start = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        let name = &body[name_start..i];
        // 이름 뒤의 공백을 건너뛰고 `(` 인지 본다 (`SET_VAR ( ... )` 도 유효하다).
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let is_resource = RESOURCE_HINTS.iter().any(|h| name.eq_ignore_ascii_case(h));
        if !(is_resource && j < b.len() && b[j] == b'(') {
            kept.push_str(name);
            continue;
        }
        // 괄호를 균형 맞춰 건너뛴다.
        let mut depth = 0usize;
        let mut k = j;
        while k < b.len() {
            match b[k] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        k += 1;
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        removed_any = true;
        removed_set_var |= name.eq_ignore_ascii_case("SET_VAR");
        i = k;
    }

    if removed_any {
        (Some(kept), true, removed_set_var)
    } else {
        (None, false, false)
    }
}

/// `UPDATE <mods> <refs> SET ... [WHERE c] [ORDER BY o] [LIMIT n]`
/// → `SELECT * FROM <refs> [WHERE c] [ORDER BY o] [LIMIT n]`
fn rewrite_update(
    sql: &str,
    toks: &[Tok],
    spans: &[std::ops::Range<usize>],
    first: usize,
    end: usize,
) -> Option<PlanQuery> {
    let set_idx = find_depth0(toks, first + 1, "SET")?;
    // `UPDATE` 다음의 수정자를 건너뛴다. 테이블 참조가 어디서 시작하는지 알아야 한다.
    let mut refs_start = first + 1;
    while matches!(toks.get(refs_start), Some(Tok::Kw(k)) if matches!(k.as_str(), "LOW_PRIORITY" | "IGNORE"))
    {
        refs_start += 1;
    }
    if refs_start >= set_idx {
        return None; // 테이블 참조가 없다
    }
    let refs = &sql[spans[refs_start].start..spans[set_idx - 1].end];

    // `SET` 절 뒤의 꼬리(WHERE/ORDER BY/LIMIT)는 그대로 옮긴다.
    let tail = ["WHERE", "ORDER", "LIMIT"]
        .iter()
        .filter_map(|k| find_depth0(toks, set_idx + 1, k))
        .min()
        .map(|i| &sql[spans[i].start..end])
        .unwrap_or("");

    Some(PlanQuery {
        sql: join_sql(&["SELECT * FROM", refs, tail]),
        is_exact: false,
    })
}

/// `DELETE <mods> [t1, t2] FROM <refs> [WHERE c] …` → `SELECT * FROM <refs> [WHERE c] …`
///
/// 다중 테이블 삭제(`DELETE t1 FROM t1 JOIN t2 …`)도 `FROM` 이후를 그대로 쓰면 된다.
fn rewrite_delete(
    sql: &str,
    toks: &[Tok],
    spans: &[std::ops::Range<usize>],
    first: usize,
    end: usize,
) -> Option<PlanQuery> {
    let from_idx = find_depth0(toks, first + 1, "FROM")?;
    Some(PlanQuery {
        sql: join_sql(&["SELECT *", &sql[spans[from_idx].start..end]]),
        is_exact: false,
    })
}

/// `INSERT INTO t (…) SELECT …` → 내부 `SELECT`.
/// `INSERT … VALUES (…)` 는 읽을 행이 없으므로 `None`.
fn rewrite_insert(
    sql: &str,
    toks: &[Tok],
    spans: &[std::ops::Range<usize>],
    first: usize,
    end: usize,
) -> Option<PlanQuery> {
    // 컬럼 목록은 괄호 안(깊이 1)이므로 깊이 0 의 SELECT 가 본체다.
    let sel =
        find_depth0(toks, first + 1, "SELECT").or_else(|| find_depth0(toks, first + 1, "WITH"))?;
    Some(PlanQuery {
        sql: sql[spans[sel].start..end].to_string(),
        is_exact: false,
    })
}

/// 선행 힌트를 건너뛴 첫 유의미 토큰의 인덱스.
fn first_meaningful(toks: &[Tok]) -> Option<usize> {
    toks.iter().position(|t| !matches!(t, Tok::Hint(_)))
}

/// `from` 이후에서 **괄호 깊이 0** 인 키워드의 인덱스를 찾는다.
///
/// 깊이를 보지 않으면 서브쿼리 안의 `WHERE`·`SELECT` 를 집는다.
fn find_depth0(toks: &[Tok], from: usize, kw: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(from) {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth -= 1,
            Tok::Kw(k) if depth == 0 && k == kw => return Some(i),
            _ => {}
        }
    }
    None
}

/// 후행 세미콜론을 제외한 끝 위치.
fn trimmed_end(sql: &str, toks: &[Tok], spans: &[std::ops::Range<usize>]) -> usize {
    match toks.last() {
        Some(Tok::Punct(';')) => spans[toks.len() - 1].start,
        _ => sql.len(),
    }
}

/// 조각을 공백 하나로 잇고 앞뒤를 다듬는다.
fn join_sql(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **한 주석에 자원 힌트와 플랜 힌트가 섞여 있으면 플랜 힌트를 보존해야 한다.**
    ///
    /// MySQL 은 한 주석에 힌트 여러 개를 허용한다. 통째로 지우면 플랜이 달라진다 —
    /// 8.4.11 실측: `NO_RANGE_OPTIMIZATION` 이 있으면 `rows=60023`, 없으면 `rows=100`.
    /// 그걸 `is_exact = true` 로 저장하면 운영자가 완전히 다른 플랜을 본다.
    #[test]
    fn combined_hints_keep_the_plan_shaping_ones() {
        let q = plan_query(
            "SELECT /*+ NO_RANGE_OPTIMIZATION(orders PRIMARY) MAX_EXECUTION_TIME(600000) */ \
             COUNT(*) FROM orders WHERE id BETWEEN 1 AND 100",
        )
        .expect("통과");
        assert!(
            q.sql.contains("NO_RANGE_OPTIMIZATION"),
            "플랜 힌트가 사라졌다: {}",
            q.sql
        );
        assert!(
            !q.sql.to_ascii_uppercase().contains("MAX_EXECUTION_TIME"),
            "자원 힌트가 남았다: {}",
            q.sql
        );
        assert!(q.is_exact, "플랜 힌트가 남았으므로 정확하다");
        // 힌트 주석 형태가 유지돼야 서버가 인식한다.
        assert!(q.sql.contains("/*+"), "힌트 주석이 깨졌다: {}", q.sql);
    }

    /// **절단 자리에 공백을 넣어야 한다.** 아무것도 넣지 않으면 양옆이 붙어
    /// 다른 토큰이 된다 — 8.4.11 실측으로 둘 다 유효한 문장이다:
    ///
    /// ```text
    /// 5 -/*+ MAX_EXECUTION_TIME(9) */- 3  → 2
    /// 5 -- 3                              → 5   (뒤가 라인 주석으로 사라진다)
    /// ```
    #[test]
    fn cutting_a_hint_does_not_join_adjacent_tokens() {
        let q = plan_query("SELECT a FROM t WHERE x = 5 -/*+ MAX_EXECUTION_TIME(9) */- 3")
            .expect("통과");
        assert!(
            !q.sql.contains("--"),
            "라인 주석이 만들어져 뒤가 사라진다: {}",
            q.sql
        );
        assert!(
            q.sql.contains("- 3") || q.sql.contains("-  - 3"),
            "{}",
            q.sql
        );

        // 공백 없는 형태도 토큰이 붙으면 안 된다.
        let q = plan_query("SELECT/*+ MAX_EXECUTION_TIME(9) */a FROM t").expect("통과");
        assert!(!q.sql.contains("SELECTa"), "토큰이 접합됐다: {}", q.sql);
    }

    /// `RESOURCE_GROUP` 도 자원 힌트다. 모니터링 롤에 `RESOURCE_GROUP_USER` 가 붙으면
    /// 공격자가 우리 EXPLAIN 스레드를 스로틀 그룹에 묶을 수 있다.
    #[test]
    fn resource_group_hint_is_stripped() {
        let q = plan_query("SELECT /*+ RESOURCE_GROUP(throttled) */ a FROM t").expect("통과");
        assert!(
            !q.sql.to_ascii_uppercase().contains("RESOURCE_GROUP"),
            "RESOURCE_GROUP 이 남았다: {}",
            q.sql
        );
    }

    /// 자원 힌트만 있던 주석은 통째로 사라지고, 문장은 여전히 유효해야 한다.
    #[test]
    fn hint_comment_with_only_resource_hints_is_removed_entirely() {
        let q = plan_query("SELECT /*+ MAX_EXECUTION_TIME(9) */ a FROM t").expect("통과");
        assert!(!q.sql.contains("/*+"), "빈 힌트 주석이 남았다: {}", q.sql);
        assert!(q.sql.contains("SELECT") && q.sql.contains("FROM t"));
        assert!(q.is_exact);
    }

    /// **자원 제어 힌트를 서버로 보내면 안 된다.**
    ///
    /// 8.4.11 실측: `max_execution_time=1000` 세션에서 `SELECT SLEEP(2)` 는 죽지만
    /// `SELECT /*+ MAX_EXECUTION_TIME(600000) */ SLEEP(2)` 는 완료된다.
    /// SQL 은 `PROCESSLIST.INFO` 에서 오므로 대상 DB 에 쿼리를 날릴 수 있는 누구나
    /// 우리 서버측 상한을 통제한다.
    #[test]
    fn resource_control_hints_are_stripped() {
        let cases: &[(&str, bool)] = &[
            ("SELECT /*+ MAX_EXECUTION_TIME(600000) */ a FROM t", true),
            (
                "SELECT /*+ SET_VAR(max_execution_time=0) */ a FROM t",
                false,
            ),
            (
                "SELECT /*+ SET_VAR(optimizer_switch='mrr=off') */ a FROM t",
                false,
            ),
            // 소문자·혼합 대소문자도 막아야 한다.
            ("SELECT /*+ max_execution_time(9) */ a FROM t", true),
            (
                "SELECT /*+ Set_Var(sql_mode='ANSI_QUOTES') */ a FROM t",
                false,
            ),
        ];
        for (sql, expect_exact) in cases {
            let q = plan_query(sql).unwrap_or_else(|| panic!("거부되면 안 된다: {sql}"));
            let upper = q.sql.to_ascii_uppercase();
            assert!(
                !upper.contains("MAX_EXECUTION_TIME") && !upper.contains("SET_VAR"),
                "자원 힌트가 남았다: {}",
                q.sql
            );
            assert!(q.sql.contains("FROM t"), "본문이 사라졌다: {}", q.sql);
            assert_eq!(
                q.is_exact, *expect_exact,
                "SET_VAR 는 플랜을 바꿀 수 있으므로 exact 가 아니다: {sql}"
            );
        }
    }

    /// **플랜을 결정하는 힌트는 보존해야 한다.** 지우면 다른 플랜이 나온다.
    #[test]
    fn plan_shaping_hints_are_preserved() {
        for sql in [
            "SELECT /*+ NO_ICP(t) */ a FROM t WHERE id = 1",
            "SELECT /*+ JOIN_ORDER(a, b) */ * FROM a JOIN b USING (id)",
            "SELECT /*+ INDEX(t idx_x) */ a FROM t",
            "SELECT /*+ NO_MERGE(d) */ * FROM (SELECT 1) d",
        ] {
            let q = plan_query(sql).expect("통과해야 한다");
            assert_eq!(q.sql, sql, "힌트가 변경됐다");
            assert!(q.is_exact);
        }
    }

    /// 자원 힌트가 여러 개거나 다른 힌트와 섞여 있어도 정확히 그것만 제거한다.
    #[test]
    fn mixed_hints_keep_only_the_safe_ones() {
        let sql = "SELECT /*+ NO_ICP(t) */ /*+ MAX_EXECUTION_TIME(9) */ a FROM t WHERE id = 1";
        let q = plan_query(sql).expect("통과");
        assert!(q.sql.contains("NO_ICP"), "플랜 힌트가 사라졌다: {}", q.sql);
        assert!(
            !q.sql.to_ascii_uppercase().contains("MAX_EXECUTION_TIME"),
            "자원 힌트가 남았다: {}",
            q.sql
        );
    }

    /// **버전 조건 주석을 서버로 보내면 안 된다.**
    ///
    /// `/*!NNNNN ... */` 의 내용은 렉서가 버리지만 서버 버전이 NNNNN 이상이면 MySQL 이
    /// 실행한다. SELECT 경로는 검증한 토큰이 아니라 **원문 부분문자열**을 그대로
    /// `EXPLAIN FORMAT=JSON ` 뒤에 붙이므로, 렉서가 못 본 코드가 실행된다.
    /// MySQL 8.4.11 실측 (server version 80400 > 11111 이라 내용이 실행된다):
    ///
    /// ```text
    /// SELECT COUNT(*) FROM orders WHERE id=1                    → 1행
    /// SELECT COUNT(*) FROM orders WHERE id=1 /*!11111 OR 1=1 */ → 60,000행
    /// ```
    ///
    /// 두 입력의 정규화 결과는 동일하다 — 즉 우리 눈에는 같은 쿼리로 보인다.
    /// SQL 은 `information_schema.PROCESSLIST.INFO` 에서 온다: 우리가 쓴 문자열이 아니다.
    #[test]
    fn version_execution_comments_are_rejected() {
        for sql in [
            "SELECT COUNT(*) FROM orders WHERE id=1 /*!11111 OR 1=1 */",
            "SELECT 1 /*!11111 ;DROP TABLE x */",
            "SELECT 1 /*!11111 UNION SELECT password FROM users */",
            "SELECT /*!50000 a */ FROM t",
            "UPDATE t SET a=1 WHERE id=1 /*!11111 OR 1=1 */",
        ] {
            assert!(
                plan_query(sql).is_none(),
                "버전 주석이 포함된 SQL 은 거부해야 한다: {sql}"
            );
        }
        // 옵티마이저 힌트는 무해하므로 계속 통과해야 한다.
        let hinted = "SELECT /*+ MAX_EXECUTION_TIME(1000) */ a FROM t WHERE id = 1";
        assert!(
            plan_query(hinted).is_some(),
            "옵티마이저 힌트는 막지 않는다: {hinted}"
        );
    }

    /// `WITH ... UPDATE/DELETE` 는 MySQL 8 의 유효 문법이다. 첫 키워드가 `WITH` 라고
    /// exact SELECT 로 통과시키면 **관측 도구가 EXPLAIN 으로 DML 을 서버에 보낸다.**
    #[test]
    fn cte_followed_by_dml_is_not_treated_as_select() {
        for sql in [
            "WITH c AS (SELECT id FROM s) UPDATE t JOIN c USING(id) SET t.x=1",
            "WITH c AS (SELECT id FROM s) DELETE t FROM t JOIN c USING(id)",
        ] {
            assert!(
                plan_query(sql).is_none(),
                "CTE 뒤의 DML 은 재실행하지 않는다: {sql}"
            );
        }
        // 순수 CTE SELECT 는 계속 통과해야 한다.
        let pure = "WITH c AS (SELECT id FROM s) SELECT * FROM c WHERE id = 3";
        let q = plan_query(pure).expect("CTE SELECT 는 통과한다");
        assert!(q.is_exact);
    }

    use crate::{StatementType, normalize};

    fn q(sql: &str) -> PlanQuery {
        plan_query(sql).unwrap_or_else(|| panic!("변환 실패: {sql}"))
    }

    /// 변환 결과는 **반드시 SELECT 여야** 한다. 아니면 관측 도구가 데이터를 바꾼다.
    fn assert_is_select(p: &PlanQuery, original: &str) {
        assert_eq!(
            normalize(&p.sql).statement_type,
            StatementType::Select,
            "원문={original}\n변환={}",
            p.sql
        );
        for bad in ["UPDATE ", "DELETE ", "INSERT ", "REPLACE "] {
            assert!(
                !p.sql.to_uppercase().starts_with(bad),
                "변환 결과가 DML 이다: {}",
                p.sql
            );
        }
    }

    #[test]
    fn select_passes_through_exactly() {
        let p = q("SELECT a FROM t WHERE id = 1");
        assert_eq!(p.sql, "SELECT a FROM t WHERE id = 1");
        assert!(p.is_exact);
        // 후행 세미콜론은 제거한다 — `EXPLAIN ...;` 은 문제없지만 깔끔하게.
        assert_eq!(q("SELECT 1;").sql, "SELECT 1");
    }

    #[test]
    fn cte_passes_through() {
        let p = q("WITH c AS (SELECT 1) SELECT * FROM c");
        assert!(p.is_exact);
        assert!(p.sql.starts_with("WITH"));
    }

    #[test]
    fn update_becomes_select_of_the_same_rows() {
        let p = q("UPDATE orders SET memo = 'x' WHERE status = 'PAID'");
        assert_eq!(p.sql, "SELECT * FROM orders WHERE status = 'PAID'");
        assert!(!p.is_exact, "근사 플랜이다");
        assert_is_select(&p, "UPDATE …");
    }

    #[test]
    fn update_preserves_literals_for_range_estimation() {
        // 정규화된 `?` 로 EXPLAIN 하면 옵티마이저가 범위를 추정할 수 없다.
        let p = q("UPDATE orders SET a = 1 WHERE id BETWEEN 100 AND 200");
        assert!(p.sql.contains("100"), "{}", p.sql);
        assert!(p.sql.contains("200"), "{}", p.sql);
    }

    #[test]
    fn update_with_modifiers_and_join() {
        let p = q(
            "UPDATE LOW_PRIORITY IGNORE orders o JOIN customers c ON o.customer_id = c.id \
                   SET o.memo = 'x' WHERE c.region_code = 'KR'",
        );
        assert_eq!(
            p.sql,
            "SELECT * FROM orders o JOIN customers c ON o.customer_id = c.id WHERE c.region_code = 'KR'"
        );
        assert_is_select(&p, "UPDATE JOIN");
    }

    #[test]
    fn update_keeps_order_by_and_limit() {
        let p = q("UPDATE orders SET memo = 'x' WHERE status = 'PAID' ORDER BY id LIMIT 10");
        assert!(
            p.sql
                .ends_with("WHERE status = 'PAID' ORDER BY id LIMIT 10"),
            "{}",
            p.sql
        );
    }

    #[test]
    fn update_without_where_still_converts() {
        assert_eq!(
            q("UPDATE orders SET memo = 'x'").sql,
            "SELECT * FROM orders"
        );
    }

    #[test]
    fn update_with_subquery_in_set_is_not_confused() {
        // SET 안의 서브쿼리에 WHERE 가 있다. 깊이 0 의 WHERE 를 집어야 한다.
        let p =
            q("UPDATE orders SET memo = (SELECT name FROM customers WHERE id = 5) WHERE id = 9");
        assert_eq!(p.sql, "SELECT * FROM orders WHERE id = 9");
    }

    #[test]
    fn delete_becomes_select() {
        let p = q("DELETE FROM order_items WHERE order_id = 1");
        assert_eq!(p.sql, "SELECT * FROM order_items WHERE order_id = 1");
        assert_is_select(&p, "DELETE");
    }

    #[test]
    fn multi_table_delete_keeps_from_clause() {
        let p = q(
            "DELETE t1 FROM order_items t1 JOIN orders t2 ON t1.order_id = t2.id \
                   WHERE t2.status = 'CANCELLED'",
        );
        assert_eq!(
            p.sql,
            "SELECT * FROM order_items t1 JOIN orders t2 ON t1.order_id = t2.id WHERE t2.status = 'CANCELLED'"
        );
        assert_is_select(&p, "multi DELETE");
    }

    #[test]
    fn delete_with_subquery_picks_outer_from() {
        let p =
            q("DELETE FROM orders WHERE id IN (SELECT order_id FROM order_items WHERE qty > 3)");
        assert!(p.sql.starts_with("SELECT * FROM orders WHERE"), "{}", p.sql);
    }

    #[test]
    fn insert_select_extracts_inner_select() {
        let p = q("INSERT INTO archive (id, val) SELECT id, val FROM orders WHERE status = 'DONE'");
        assert_eq!(p.sql, "SELECT id, val FROM orders WHERE status = 'DONE'");
        assert!(!p.is_exact);
        assert_is_select(&p, "INSERT SELECT");
    }

    #[test]
    fn insert_values_has_no_plan() {
        assert_eq!(plan_query("INSERT INTO t (a, b) VALUES (1, 2)"), None);
        assert_eq!(plan_query("REPLACE INTO t VALUES (1, 2)"), None);
        assert_eq!(plan_query("INSERT INTO t SET a = 1"), None);
    }

    #[test]
    fn ddl_and_other_have_no_plan() {
        for sql in [
            "ALTER TABLE t ADD COLUMN c INT",
            "TRUNCATE TABLE t",
            "SET autocommit = 1",
            "CALL sp_x(1)",
            "SHOW PROCESSLIST",
            "",
        ] {
            assert_eq!(plan_query(sql), None, "{sql}");
        }
    }

    #[test]
    fn leading_hint_is_preserved_for_select() {
        let p = q("/*+ MAX_EXECUTION_TIME(1000) */ SELECT 1");
        assert!(
            p.sql.starts_with("SELECT"),
            "힌트 뒤부터 잘라낸다: {}",
            p.sql
        );
    }

    /// 잘린 SQL 로 문장을 만들면 **의도와 다른 문장**이 될 수 있다. 시도하지 않는다.
    /// 멀티문장은 두 번째 문장이 `EXPLAIN` 없이 실행될 수 있다.
    #[test]
    fn multi_statement_is_refused() {
        assert_eq!(plan_query("SELECT 1; DROP TABLE orders"), None);
        assert_eq!(
            plan_query("UPDATE t SET a=1 WHERE id=1; DELETE FROM t"),
            None
        );
        // 후행 세미콜론 하나는 허용한다.
        assert!(plan_query("SELECT 1;").is_some());
        // 문자열 안의 세미콜론은 토큰이 아니므로 무해하다.
        assert!(plan_query("SELECT * FROM t WHERE a = 'a;b'").is_some());
    }

    #[test]
    fn truncated_sql_is_refused() {
        assert_eq!(plan_query("UPDATE orders SET memo = 'unterminated"), None);
        assert_eq!(plan_query("DELETE FROM t WHERE n = 'oops"), None);
    }

    #[test]
    fn no_panic_on_malformed_dml() {
        for sql in [
            "UPDATE",
            "UPDATE SET",
            "UPDATE t",
            "DELETE",
            "DELETE FROM",
            "INSERT",
            "INSERT INTO",
            "UPDATE t SET",
            "DELETE t1",
            "UPDATE ( SET a=1",
            "DELETE FROM ((((",
        ] {
            let _ = plan_query(sql);
        }
    }
}
