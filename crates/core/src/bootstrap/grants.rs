//! `SHOW GRANTS` 파싱과 권한 집합 대수 ([07 §2.6](../../../../.claude/docs/07-credentials-bootstrap.md)).
//!
//! # 왜 `SHOW GRANTS` 를 1차 소스로 쓰는가
//!
//! `information_schema.*_PRIVILEGES` 는 (a) 인증 플러그인·`REQUIRE SSL` 을 보여주지
//! 않고 (b) 동적 권한·롤이 나오지 않고 (c) MySQL 8.0 에서 deprecated 다. 문서가 그
//! 판단을 이미 적었다.
//!
//! # 파싱이 인용을 제대로 읽어야 하는 이유
//!
//! 이 파싱 결과가 **마스터 권한으로 실행할 문장을 결정한다.** 스키마 이름에 ` ON ` 이나
//! ` TO ` 가 들어갈 수 있다 — `` CREATE DATABASE `a ON b` `` 는 합법이다. 단순 문자열
//! 분할로 읽으면 그런 이름에서 권한 범위를 잘못 판정하고, 잘못 판정한 차집합이 곧
//! 잘못된 `GRANT` 다.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// 서버가 `GRANT` 의 **데이터베이스 이름**을 어떻게 해석하는가.
///
/// # 왜 이 구분이 필요한가
///
/// `partial_revokes` 시스템 변수가 이름 해석을 바꾼다:
///
/// | 값 | 이름 해석 | 우리가 해야 하는 것 |
/// |---|---|---|
/// | `OFF` (RDS 기본값) | `_`·`%` 가 **와일드카드** | 백슬래시로 이스케이프한다 |
/// | `ON` | 이름이 **리터럴** | 이스케이프하지 **않는다** |
///
/// **틀리면 조용히 망가진다.** `ON` 인 서버에 `` `a\_b` `` 를 부여하면 이름이 literally
/// `a\_b` 인 데이터베이스에 권한이 생긴다 — 그런 DB 는 없으므로 `GRANT` 는 성공하는데
/// 아무 효과가 없고, 다음 계획이 같은 `GRANT` 를 다시 요구한다(무한 반복).
///
/// 그래서 서버 값을 읽어 정한다. RDS·Aurora 실측값은 둘 다 `OFF` 다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemaNameMode {
    /// `partial_revokes = OFF` — 이스케이프한다.
    #[default]
    Pattern,
    /// `partial_revokes = ON` — 이름이 리터럴이다.
    Literal,
}

impl SchemaNameMode {
    /// `@@partial_revokes` 값에서 정한다. `1`/`ON` 이면 리터럴이다.
    pub fn from_partial_revokes(raw: &str) -> Self {
        let on = raw == "1" || raw.eq_ignore_ascii_case("on") || raw.eq_ignore_ascii_case("true");
        if on { Self::Literal } else { Self::Pattern }
    }

    fn escapes(self) -> bool {
        self == Self::Pattern
    }
}

/// 권한을 부여하는 대상 범위.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GrantScope {
    /// `*.*` — 전역.
    Global,
    /// `` `db`.* `` — 스키마 전체.
    Schema(String),
    /// `` `db`.`tbl` `` — 테이블 하나.
    Table(String, String),
}

impl GrantScope {
    /// SQL 의 `ON <여기>` 자리에 넣을 문자열.
    ///
    /// 식별자가 인용될 수 없으면(제어문자 등) `None` — **거부가 기본값이다.**
    /// SQL 의 `ON <여기>` 자리에 넣을 문자열.
    ///
    /// `mode` 가 [`SchemaNameMode::Pattern`] 이면 `_`·`%` 를 이스케이프한다 — 안 하면
    /// `order_items` 가 `orderXitems` 까지 부여한다(실측). `Literal` 이면 하지 않는다 —
    /// 하면 존재하지 않는 이름에 부여된다.
    pub fn render(&self, mode: SchemaNameMode) -> Option<String> {
        use crate::ident::{quote_grant_schema, quote_ident};
        let db_quote = |db: &str| {
            if mode.escapes() {
                quote_grant_schema(db)
            } else {
                quote_ident(db)
            }
        };
        match self {
            Self::Global => Some("*.*".to_string()),
            Self::Schema(db) => Some(format!("{}.*", db_quote(db)?)),
            Self::Table(db, tbl) => Some(format!(
                "{}.{}",
                db_quote(db)?,
                // 테이블 이름은 패턴이 아니다 — `GRANT` 문법에서 와일드카드는
                // 데이터베이스 자리에만 적용된다.
                quote_ident(tbl)?
            )),
        }
    }

    /// 이 범위가 데이터 스키마를 읽는가 (감사 화면에서 구분한다).
    pub fn touches_user_data(&self) -> bool {
        match self {
            Self::Global => true,
            Self::Schema(db) => !super::schemas::is_system_schema(db),
            Self::Table(db, _) => !super::schemas::is_system_schema(db),
        }
    }
}

/// **진단·지문용 표시.** 실행할 SQL 은 [`GrantScope::render`] 를 쓴다 — 그쪽은
/// 서버의 `partial_revokes` 를 알아야 한다.
impl fmt::Display for GrantScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.render(SchemaNameMode::default()) {
            Some(s) => f.write_str(&s),
            // 인용할 수 없는 이름은 화면에도 원문으로 내보내지 않는다.
            None => f.write_str("<unrenderable>"),
        }
    }
}

/// 권한 집합 — 범위별 권한 이름 모음.
///
/// 권한 이름을 열거형이 아니라 정규화된 문자열로 둔다. `SHOW GRANTS` 는 우리가 모르는
/// 동적 권한(`SYSTEM_VARIABLES_ADMIN` 등)을 돌려주고, 열거형으로 두면 어차피
/// `Other(String)` 이 필요하다 — 그 변형이 생기는 순간 열거형의 이점이 사라진다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantSet {
    entries: BTreeMap<GrantScope, BTreeSet<String>>,
    /// `GRANT <role> TO <user>` 로 받은 롤. 권한을 간접적으로 늘리므로 **초과 권한
    /// 판정에 포함한다** — 롤 안을 들여다보지 않고 "우리가 요구하지 않은 것" 으로 센다.
    roles: BTreeSet<String>,
    /// `WITH GRANT OPTION` 이 붙은 범위가 있는가. 모니터링 계정에는 있어서는 안 된다.
    pub has_grant_option: bool,
}

/// 의미 없는 권한 — `SHOW GRANTS` 는 권한이 없는 계정에도 이 줄을 낸다.
const USAGE: &str = "USAGE";

impl GrantSet {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.roles.is_empty()
    }

    pub fn roles(&self) -> &BTreeSet<String> {
        &self.roles
    }

    pub fn scopes(&self) -> impl Iterator<Item = (&GrantScope, &BTreeSet<String>)> {
        self.entries.iter()
    }

    /// 범위에 권한을 더한다. `USAGE` 는 버린다(권한이 아니다).
    pub fn add(&mut self, scope: GrantScope, privileges: impl IntoIterator<Item = String>) {
        let set: BTreeSet<String> = privileges
            .into_iter()
            .map(|p| normalize_privilege(&p))
            .filter(|p| p != USAGE)
            .collect();
        if set.is_empty() {
            return;
        }
        self.entries.entry(scope).or_default().extend(set);
    }

    pub fn privileges_on(&self, scope: &GrantScope) -> Option<&BTreeSet<String>> {
        self.entries.get(scope)
    }

    /// 이 범위에서 해당 권한이 **실효적으로** 있는가.
    ///
    /// 상위 범위가 덮으면 참이다: 전역 `SELECT` 는 모든 스키마의 `SELECT` 를 포함하고,
    /// 스키마 `SELECT` 는 그 안 모든 테이블을 포함한다. 이 포함 관계를 보지 않으면
    /// **이미 있는 권한을 다시 `GRANT`** 하게 되고, 멱등성 테스트(M3-8)가 깨진다.
    pub fn covers(&self, scope: &GrantScope, privilege: &str) -> bool {
        let want = normalize_privilege(privilege);
        let has = |s: &GrantScope| {
            self.entries
                .get(s)
                .is_some_and(|set| set.contains(want.as_str()))
        };
        if has(&GrantScope::Global) {
            return true;
        }
        match scope {
            GrantScope::Global => false,
            GrantScope::Schema(_) => has(scope),
            GrantScope::Table(db, _) => has(&GrantScope::Schema(db.clone())) || has(scope),
        }
    }

    /// **우리가 요구하지 않은 권한** — 초과 권한 경고의 근거 (FR-CRD-11).
    ///
    /// `desired` 가 덮지 못하는 (범위, 권한) 쌍을 돌려준다. 롤은 별도로 세어
    /// [`Self::roles`] 로 노출한다 — 롤 안을 펼치지 않으므로 전부 초과로 본다.
    pub fn excess_over(&self, desired: &GrantSet) -> Vec<(GrantScope, String)> {
        let mut out = Vec::new();
        for (scope, privs) in &self.entries {
            for p in privs {
                if !desired.covers(scope, p) {
                    out.push((scope.clone(), p.clone()));
                }
            }
        }
        out
    }

    /// **상위 범위가 덮는 항목을 제거한 집합.**
    ///
    /// 모드 A 는 전역 `SELECT` 를 주는데, 관측 스키마(`performance_schema`·`sys`)의
    /// `SELECT` 도 함께 요구한다. 최소화하지 않으면 [`Self::missing_from`] 이 그
    /// 둘을 **따로 요구**하고, 사람이 승인해야 할 문장이 셋으로 늘어난다.
    ///
    /// 첫 실행 뒤에는 전역 `SELECT` 가 나머지를 덮으므로 결과가 수렴하기는 한다 —
    /// 즉 정확성 결함이 아니라 **승인 화면에 군더더기를 보여주는** 결함이다. 사람이
    /// 읽고 승인하는 값이라 군더더기가 곧 결함이다.
    pub fn minimized(&self) -> GrantSet {
        let mut out = GrantSet {
            entries: BTreeMap::new(),
            roles: self.roles.clone(),
            has_grant_option: self.has_grant_option,
        };
        for (scope, privs) in &self.entries {
            let kept: BTreeSet<String> = privs
                .iter()
                .filter(|p| !self.covered_by_broader(scope, p))
                .cloned()
                .collect();
            if !kept.is_empty() {
                out.entries.insert(scope.clone(), kept);
            }
        }
        out
    }

    /// 이 권한이 **더 넓은 범위**에서 이미 부여됐는가 (자기 범위는 보지 않는다).
    fn covered_by_broader(&self, scope: &GrantScope, privilege: &str) -> bool {
        let want = normalize_privilege(privilege);
        let has = |s: &GrantScope| {
            self.entries
                .get(s)
                .is_some_and(|set| set.contains(want.as_str()))
        };
        match scope {
            GrantScope::Global => false,
            GrantScope::Schema(_) => has(&GrantScope::Global),
            GrantScope::Table(db, _) => {
                has(&GrantScope::Global) || has(&GrantScope::Schema(db.clone()))
            }
        }
    }

    /// **부족한 권한** — 실행할 `GRANT` 의 근거.
    ///
    /// `self`(현재 상태)가 덮지 못하는 `desired` 항목을 범위별로 묶어 돌려준다.
    /// 묶는 이유: `GRANT a, b ON x` 한 문장이 두 문장보다 낫다(원자적이고 읽기 쉽다).
    pub fn missing_from(&self, desired: &GrantSet) -> Vec<(GrantScope, BTreeSet<String>)> {
        let mut out = Vec::new();
        for (scope, privs) in &desired.entries {
            let missing: BTreeSet<String> = privs
                .iter()
                .filter(|p| !self.covers(scope, p))
                .cloned()
                .collect();
            if !missing.is_empty() {
                out.push((scope.clone(), missing));
            }
        }
        out
    }
}

/// 권한 이름 정규화 — 대문자 + 공백 축약.
///
/// `SHOW GRANTS` 는 `REPLICATION CLIENT` 처럼 공백이 든 이름을 낸다. 서버·버전에 따라
/// 공백 수가 다를 수 있어 축약한다.
fn normalize_privilege(raw: &str) -> String {
    raw.split_whitespace()
        .map(|w| w.to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `SHOW GRANTS` 한 줄을 파싱한다.
///
/// 인식하는 형태:
/// ```text
/// GRANT USAGE ON *.* TO `dbmon`@`10.1.%`
/// GRANT PROCESS, REPLICATION CLIENT ON *.* TO 'dbmon'@'10.1.%'
/// GRANT SELECT ON `shop`.* TO `dbmon`@`10.1.%`
/// GRANT SELECT ON `mysql`.`innodb_table_stats` TO `dbmon`@`10.1.%`
/// GRANT SELECT (col1, col2) ON `shop`.`t` TO `dbmon`@`10.1.%`
/// GRANT `some_role`@`%` TO `dbmon`@`10.1.%`
/// GRANT SELECT ON *.* TO `dbmon`@`10.1.%` WITH GRANT OPTION
/// ```
///
/// 읽을 수 없는 줄은 `None` 이고, 호출부는 그것을 **경고로 남기고 진행을 멈춘다** —
/// 못 읽은 줄에 초과 권한이 숨어 있을 수 있으므로 조용히 무시하지 않는다.
pub fn parse_grant_line(line: &str, mode: SchemaNameMode) -> Option<ParsedGrant> {
    let rest = line.trim();
    let rest = strip_keyword(rest, "GRANT")?;

    // 롤 부여 형태: `GRANT <role> TO <user>` — ` ON ` 이 없다.
    let Some((priv_part, after_on)) = split_at_top_level_keyword(rest, "ON") else {
        let (roles, _) = split_at_top_level_keyword(rest, "TO")?;
        let names = parse_name_list(roles)?;
        return Some(ParsedGrant::Roles(names));
    };

    let privileges = parse_privilege_list(priv_part)?;
    let (scope_part, tail) = split_at_top_level_keyword(after_on, "TO")?;
    let scope = parse_scope(scope_part.trim(), mode)?;
    let with_grant_option = contains_top_level_keywords(tail, &["WITH", "GRANT", "OPTION"]);

    Some(ParsedGrant::Privileges {
        scope,
        privileges,
        with_grant_option,
    })
}

/// 파싱된 `SHOW GRANTS` 한 줄.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedGrant {
    Privileges {
        scope: GrantScope,
        privileges: Vec<String>,
        with_grant_option: bool,
    },
    /// `GRANT <role> TO <user>` — 롤 이름들.
    Roles(Vec<String>),
}

/// 여러 줄을 집합으로 접는다. **읽을 수 없는 줄을 함께 돌려준다.**
pub fn parse_grants<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    mode: SchemaNameMode,
) -> (GrantSet, Vec<String>) {
    let mut set = GrantSet::default();
    let mut unparsed = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        match parse_grant_line(line, mode) {
            Some(ParsedGrant::Privileges {
                scope,
                privileges,
                with_grant_option,
            }) => {
                if with_grant_option {
                    set.has_grant_option = true;
                }
                set.add(scope, privileges);
            }
            Some(ParsedGrant::Roles(roles)) => set.roles.extend(roles),
            None => unparsed.push(line.to_string()),
        }
    }
    (set, unparsed)
}

// ── 인용을 아는 스캐너 ────────────────────────────────────────────────────────

/// 문장 앞의 키워드를 벗긴다 (대소문자 무시).
///
/// **`s.get(..)` 으로 자른다.** `s[..kw.len()]` 은 그 위치가 문자 경계가 아니면
/// 패닉한다 — `SHOW GRANTS` 출력이 아닌 문자열(한글 로그 한 줄)이 들어와 실제로
/// 터졌다. 파서는 어떤 입력에도 패닉하지 않아야 한다.
fn strip_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if !s
        .get(..kw.len())
        .is_some_and(|h| h.eq_ignore_ascii_case(kw))
    {
        return None;
    }
    let rest = &s[kw.len()..];
    // 키워드 뒤에 공백이 와야 한다 — `GRANTS` 를 `GRANT` 로 읽지 않는다.
    if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        return None;
    }
    Some(rest)
}

/// **인용·괄호 밖에 있는** 키워드에서 자른다. 첫 등장에서 자른다.
///
/// 이 함수가 이 모듈의 핵심이다. `` `a ON b` `` 라는 스키마 이름이 있으면 단순
/// `split(" ON ")` 은 범위를 잘못 자른다.
fn split_at_top_level_keyword<'a>(s: &'a str, kw: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    let mut depth = 0i32;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == b'\\' && q != b'`' {
                    i += 2;
                    continue;
                }
                if c == q {
                    if bytes.get(i + 1) == Some(&q) {
                        i += 2;
                        continue;
                    }
                    quote = None;
                }
            }
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0 && is_word_at(s, i, kw) => {
                    return Some((&s[..i], &s[i + kw.len()..]));
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// `i` 위치에서 키워드가 **단어 경계로** 시작하는가.
///
/// 슬라이싱에 `get` 을 쓴다 — `i` 는 바이트 루프에서 오므로 다중바이트 문자의
/// 중간일 수 있고, 그러면 `s[i..]` 가 패닉한다.
fn is_word_at(s: &str, i: usize, kw: &str) -> bool {
    let bytes = s.as_bytes();
    if !s
        .get(i..i + kw.len())
        .is_some_and(|w| w.eq_ignore_ascii_case(kw))
    {
        return false;
    }
    let before_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
    let after = bytes.get(i + kw.len());
    let after_ok = after.is_none_or(|c| c.is_ascii_whitespace());
    before_ok && after_ok
}

/// 인용 밖에서 키워드들이 순서대로 나타나는가 (`WITH GRANT OPTION` 판정용).
fn contains_top_level_keywords(s: &str, kws: &[&str]) -> bool {
    let mut rest = s;
    for kw in kws {
        match split_at_top_level_keyword(rest, kw) {
            Some((_, after)) => rest = after,
            None => return false,
        }
    }
    true
}

/// 컬럼 단위 `GRANT` 를 표시하는 접미어.
///
/// # 왜 이름을 바꿔서 기록하는가 (교차 리뷰가 잡은 결함)
///
/// 처음에는 컬럼 목록을 버리고 `SELECT` 로 기록했다. 그러면
/// `GRANT SELECT (last_update) ON mysql.innodb_table_stats` 가 **테이블 전체
/// `SELECT` 로 읽힌다** — [`GrantSet::covers`] 가 참을 주고, 우리가 필요한 전체
/// `SELECT` 를 부여하지 않는다. 그러면 통계 신선도 조회가 조용히 실패한다.
///
/// 이름을 바꿔 기록하면 두 가지가 동시에 맞는다:
/// - `covers("SELECT")` 가 거짓이므로 필요한 GRANT 를 건너뛰지 않는다
/// - 우리가 요구하지 않은 권한이므로 **초과 권한으로 보고된다** (정확하다)
pub const COLUMN_SUFFIX: &str = "(COLUMNS)";

/// 권한 목록 파싱 — `PROCESS, REPLICATION CLIENT, SELECT (a, b)`.
///
/// 컬럼 목록이 붙은 권한은 [`COLUMN_SUFFIX`] 를 달아 **다른 권한으로** 기록한다.
fn parse_privilege_list(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    // 현재 이름에 컬럼 목록이 붙었는가.
    let mut has_columns = false;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                if depth == 1 {
                    has_columns = true;
                }
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            ',' if depth == 0 => {
                push_privilege(&mut out, &mut cur, &mut has_columns);
            }
            // 인용부호는 권한 이름에 올 수 없다 — 오면 우리가 모르는 형태다.
            '\'' | '"' | '`' => return None,
            _ if depth == 0 => cur.push(c),
            _ => {}
        }
    }
    push_privilege(&mut out, &mut cur, &mut has_columns);
    if depth != 0 || out.is_empty() {
        return None;
    }
    Some(out)
}

fn push_privilege(out: &mut Vec<String>, cur: &mut String, has_columns: &mut bool) {
    let name = normalize_privilege(cur);
    cur.clear();
    let columns = std::mem::take(has_columns);
    if name.is_empty() {
        return;
    }
    // **컬럼 GRANT 는 다른 권한이다.** 이유는 `COLUMN_SUFFIX` 문서에 있다.
    out.push(if columns {
        format!("{name} {COLUMN_SUFFIX}")
    } else {
        name
    });
}

/// 범위 파싱 — `*.*` / `` `db`.* `` / `db.*` / `` `db`.`tbl` ``.
fn parse_scope(s: &str, mode: SchemaNameMode) -> Option<GrantScope> {
    let (db_raw, tbl_raw) = split_scope_parts(s)?;
    let db_quoted = unquote_name(&db_raw)?;
    // **패턴 모드에서만 이스케이프를 벗긴다.** 벗기지 않으면 우리가 부여한
    // `` `a\_b` `` 를 이름이 `a\_b` 인 스키마로 읽고, 차집합이 매번 같은 `GRANT` 를
    // 다시 요구한다(멱등성이 깨진다). 리터럴 모드에서 벗기면 반대로 이름이 망가진다.
    let db = if mode.escapes() {
        crate::ident::unescape_grant_pattern(&db_quoted)
    } else {
        db_quoted
    };
    let tbl = unquote_name(&tbl_raw)?;
    match (db.as_str(), tbl.as_str()) {
        ("*", "*") => Some(GrantScope::Global),
        // `*.tbl` 은 "현재 스키마의 tbl" 이라 의미가 세션에 달려 있다. 우리는 만들지
        // 않고, 오면 읽을 수 없는 줄로 취급한다.
        ("*", _) => None,
        (d, "*") => Some(GrantScope::Schema(d.to_string())),
        (d, t) => Some(GrantScope::Table(d.to_string(), t.to_string())),
    }
}

/// `db.tbl` 을 인용을 존중해 두 조각으로 나눈다.
fn split_scope_parts(s: &str) -> Option<(String, String)> {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == q {
                    if bytes.get(i + 1) == Some(&q) {
                        i += 2;
                        continue;
                    }
                    quote = None;
                }
            }
            None => match c {
                b'`' | b'\'' | b'"' => quote = Some(c),
                b'.' => return Some((s[..i].to_string(), s[i + 1..].to_string())),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// 인용을 벗긴다. 인용이 없으면 그대로. 짝이 맞지 않으면 `None`.
fn unquote_name(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let first = t.chars().next()?;
    if !matches!(first, '`' | '\'' | '"') {
        // 인용 없는 이름에는 공백·인용부호가 올 수 없다.
        if t.chars()
            .any(|c| c.is_whitespace() || matches!(c, '`' | '\'' | '"'))
        {
            return None;
        }
        return Some(t.to_string());
    }
    let inner = t.strip_prefix(first)?.strip_suffix(first)?;
    // **이중화를 되돌리기 전에 짝을 검사한다.**
    //
    // 먼저 `replace` 한 뒤 "그 문자가 남아 있으면 거부" 하면 안 된다 — 정상적으로
    // 이스케이프된 이름(`` `we``ird` `` → `` we`ird ``)이 전부 거부된다. 실제로 그
    // 버그를 만들었고 테스트가 잡았다. 짝이 맞는지는 순회로 판정해야 한다.
    let mut restored = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != first {
            restored.push(c);
            continue;
        }
        // 인용부호는 반드시 짝으로 온다. 혼자 오면 인용이 깨진 것이다.
        if chars.next() != Some(first) {
            return None;
        }
        restored.push(first);
    }
    Some(restored)
}

/// `` `role`@`%`, `other` `` 형태의 이름 목록.
fn parse_name_list(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        out.push(t.to_string());
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn privs(line: &str) -> (GrantScope, Vec<String>) {
        match parse_grant_line(line, SchemaNameMode::Pattern)
            .unwrap_or_else(|| panic!("파싱 실패: {line}"))
        {
            ParsedGrant::Privileges {
                scope, privileges, ..
            } => (scope, privileges),
            other => panic!("권한 줄이 아니다: {other:?}"),
        }
    }

    /// RDS MySQL 8.x 가 실제로 내는 형태 (백틱 인용).
    #[test]
    fn parses_backtick_quoted_grants() {
        let (scope, p) = privs(
            "GRANT PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO `dbmon`@`10.1.%`",
        );
        assert_eq!(scope, GrantScope::Global);
        assert_eq!(
            p,
            vec![
                "PROCESS",
                "REPLICATION CLIENT",
                "SHOW DATABASES",
                "SHOW VIEW"
            ]
        );
    }

    /// 작은따옴표 형태도 받는다 — 버전·포크에 따라 갈린다.
    #[test]
    fn parses_single_quoted_grants() {
        let (scope, p) = privs("GRANT SELECT ON `shop`.* TO 'dbmon'@'10.1.%'");
        assert_eq!(scope, GrantScope::Schema("shop".into()));
        assert_eq!(p, vec!["SELECT"]);
    }

    #[test]
    fn parses_table_scope() {
        let (scope, _) = privs("GRANT SELECT ON `mysql`.`innodb_table_stats` TO `dbmon`@`10.1.%`");
        assert_eq!(
            scope,
            GrantScope::Table("mysql".into(), "innodb_table_stats".into())
        );
    }

    /// **이 테스트가 이 모듈의 존재 이유다.**
    ///
    /// `` CREATE DATABASE `a ON b` `` 는 합법이다. 단순 `split(" ON ")` 은 이 줄에서
    /// 범위를 `` b`.* `` 로 잘못 자르고, 그 오판이 곧 잘못된 `GRANT` 다.
    #[test]
    fn keywords_inside_quoted_identifiers_do_not_split() {
        let (scope, p) = privs("GRANT SELECT ON `a ON b`.* TO `dbmon`@`10.1.%`");
        assert_eq!(scope, GrantScope::Schema("a ON b".into()));
        assert_eq!(p, vec!["SELECT"]);

        let (scope, _) = privs("GRANT SELECT ON `a TO b`.* TO `dbmon`@`10.1.%`");
        assert_eq!(scope, GrantScope::Schema("a TO b".into()));

        // 사용자 이름 쪽에 있어도 마찬가지다.
        let (scope, _) = privs("GRANT SELECT ON `shop`.* TO `db ON mon`@`10.1.%`");
        assert_eq!(scope, GrantScope::Schema("shop".into()));
    }

    /// 이중화된 백틱을 되돌린다.
    #[test]
    fn doubled_backticks_are_restored() {
        let (scope, _) = privs("GRANT SELECT ON `we``ird`.* TO `dbmon`@`10.1.%`");
        assert_eq!(scope, GrantScope::Schema("we`ird".into()));
    }

    #[test]
    fn usage_is_dropped_because_it_is_not_a_privilege() {
        let (set, unparsed) = parse_grants(
            ["GRANT USAGE ON *.* TO `dbmon`@`10.1.%`"],
            SchemaNameMode::Pattern,
        );
        assert!(unparsed.is_empty());
        assert!(set.is_empty(), "USAGE 만 있으면 권한이 없는 것이다");
    }

    /// **컬럼 GRANT 는 테이블 전체 GRANT 가 아니다.**
    ///
    /// 교차 리뷰가 잡은 결함: 컬럼 목록을 버리고 `SELECT` 로 기록하면
    /// `covers("SELECT")` 가 참이 되어 **필요한 전체 GRANT 를 건너뛴다.**
    /// 그러면 통계 신선도 조회가 조용히 실패한다.
    #[test]
    fn a_column_grant_does_not_satisfy_a_table_grant() {
        let (scope, p) = privs("GRANT SELECT (a, b) ON `shop`.`t` TO `dbmon`@`10.1.%`");
        assert_eq!(scope, GrantScope::Table("shop".into(), "t".into()));
        assert_eq!(p, vec!["SELECT (COLUMNS)"], "컬럼 GRANT 를 구분하지 않았다");

        let (set, unparsed) = parse_grants(
            ["GRANT SELECT (last_update) ON `mysql`.`innodb_table_stats` TO `dbmon`@`10.1.%`"],
            SchemaNameMode::Pattern,
        );
        assert!(unparsed.is_empty(), "파싱은 돼야 한다");
        let tbl = GrantScope::Table("mysql".into(), "innodb_table_stats".into());
        assert!(
            !set.covers(&tbl, "SELECT"),
            "컬럼 GRANT 가 테이블 전체 SELECT 를 만족시켰다"
        );

        // 그리고 **초과 권한으로 보고된다** — 우리가 요구하지 않은 권한이다.
        let mut desired = GrantSet::default();
        desired.add(tbl.clone(), ["SELECT".to_string()]);
        assert!(
            set.excess_over(&desired)
                .iter()
                .any(|(_, p)| p.contains("COLUMNS")),
            "컬럼 GRANT 가 초과 권한에 안 잡혔다"
        );
        // 부족한 권한으로도 잡힌다 (전체 SELECT 가 필요하다).
        assert!(!set.missing_from(&desired).is_empty());
    }

    /// **`partial_revokes = ON` 이면 이스케이프하지 않는다.**
    ///
    /// 하면 존재하지 않는 이름(`a\_b`)에 권한이 생긴다 — `GRANT` 는 성공하는데 효과가
    /// 없고, 다음 계획이 같은 문장을 무한히 반복한다.
    #[test]
    fn literal_mode_does_not_escape_wildcards() {
        let scope = GrantScope::Schema("order_items".into());
        assert_eq!(
            scope.render(SchemaNameMode::Pattern).as_deref(),
            Some("`order\\_items`.*"),
            "패턴 모드는 이스케이프한다"
        );
        assert_eq!(
            scope.render(SchemaNameMode::Literal).as_deref(),
            Some("`order_items`.*"),
            "리터럴 모드에서 이스케이프하면 존재하지 않는 이름에 부여된다"
        );
    }

    /// **리터럴 모드에서는 파싱도 벗기지 않는다.** 벗기면 이름이 망가진다.
    #[test]
    fn literal_mode_round_trips_without_unescaping() {
        for mode in [SchemaNameMode::Pattern, SchemaNameMode::Literal] {
            let scope = GrantScope::Schema("order_items".into());
            let rendered = scope.render(mode).expect("렌더");
            let line = format!("GRANT SELECT ON {rendered} TO `dbmon`@`10.1.%`");
            let (set, unparsed) = parse_grants([line.as_str()], mode);
            assert!(unparsed.is_empty(), "{mode:?}: {unparsed:?}");
            assert!(
                set.covers(&scope, "SELECT"),
                "{mode:?}: 왕복이 깨졌다 — 같은 GRANT 를 무한히 반복한다"
            );
        }

        // **모드를 섞으면 깨진다** — 그래서 서버 값을 읽어야 한다.
        let rendered = GrantScope::Schema("order_items".into())
            .render(SchemaNameMode::Pattern)
            .expect("렌더");
        let line = format!("GRANT SELECT ON {rendered} TO `dbmon`@`10.1.%`");
        let (mismatched, _) = parse_grants([line.as_str()], SchemaNameMode::Literal);
        assert!(
            !mismatched.covers(&GrantScope::Schema("order_items".into()), "SELECT"),
            "모드를 섞었는데 통과했다 — 이 테스트의 전제가 깨졌다"
        );
    }

    /// `@@partial_revokes` 값 해석.
    #[test]
    fn the_mode_comes_from_partial_revokes() {
        for on in ["1", "ON", "on", "true", "TRUE"] {
            assert_eq!(
                SchemaNameMode::from_partial_revokes(on),
                SchemaNameMode::Literal,
                "{on} 을 놓쳤다"
            );
        }
        for off in ["0", "OFF", "off", "false", ""] {
            assert_eq!(
                SchemaNameMode::from_partial_revokes(off),
                SchemaNameMode::Pattern,
                "{off} 를 리터럴로 봤다"
            );
        }
        // 기본값은 RDS 실측값(OFF)과 같다.
        assert_eq!(SchemaNameMode::default(), SchemaNameMode::Pattern);
    }

    /// **스키마 이름의 `_`·`%` 를 이스케이프하고, 파싱이 그것을 벗긴다.**
    ///
    /// 왕복이 깨지면 차집합이 매번 같은 GRANT 를 요구한다(멱등성).
    #[test]
    fn schema_pattern_escaping_round_trips() {
        let scope = GrantScope::Schema("order_items".into());
        let rendered = scope.render(SchemaNameMode::Pattern).expect("렌더");
        assert_eq!(rendered, "`order\\_items`.*", "이스케이프가 없다");

        // MySQL 이 `SHOW GRANTS` 로 되돌려 주는 형태.
        let line = format!("GRANT SELECT ON {rendered} TO `dbmon`@`10.1.%`");
        let (set, unparsed) = parse_grants([line.as_str()], SchemaNameMode::Pattern);
        assert!(unparsed.is_empty(), "{unparsed:?}");
        assert!(
            set.covers(&scope, "SELECT"),
            "왕복이 깨졌다 — 같은 GRANT 를 다시 요구한다"
        );
        // 와일드카드로 해석된 이름은 **다른** 스키마다.
        assert!(!set.covers(&GrantScope::Schema("orderXitems".into()), "SELECT"));
    }

    #[test]
    fn role_grants_are_recognized() {
        let (set, unparsed) = parse_grants(
            ["GRANT `app_role`@`%` TO `dbmon`@`10.1.%`"],
            SchemaNameMode::Pattern,
        );
        assert!(unparsed.is_empty());
        assert_eq!(set.roles().len(), 1);
    }

    /// `WITH GRANT OPTION` 은 모니터링 계정에 있어서는 안 되므로 **판정할 수 있어야**
    /// 한다.
    #[test]
    fn grant_option_is_detected() {
        let (set, _) = parse_grants(
            ["GRANT SELECT ON *.* TO `dbmon`@`10.1.%` WITH GRANT OPTION"],
            SchemaNameMode::Pattern,
        );
        assert!(set.has_grant_option);

        let (plain, _) = parse_grants(
            ["GRANT SELECT ON *.* TO `dbmon`@`10.1.%`"],
            SchemaNameMode::Pattern,
        );
        assert!(!plain.has_grant_option);
    }

    /// **`WITH GRANT OPTION` 이 인용 안에 있으면 오탐하지 않는다.**
    #[test]
    fn grant_option_inside_a_name_is_not_grant_option() {
        let (set, unparsed) = parse_grants(
            ["GRANT SELECT ON `shop`.* TO `WITH GRANT OPTION`@`10.1.%`"],
            SchemaNameMode::Pattern,
        );
        assert!(unparsed.is_empty(), "파싱은 돼야 한다");
        assert!(
            !set.has_grant_option,
            "인용된 사용자 이름을 GRANT OPTION 으로 봤다"
        );
    }

    /// 읽을 수 없는 줄은 **버리지 않고 돌려준다.** 못 읽은 줄에 초과 권한이 숨어 있을
    /// 수 있으므로 호출부가 진행을 멈출 근거가 된다.
    #[test]
    fn unreadable_lines_are_reported_not_swallowed() {
        let (set, unparsed) = parse_grants(
            [
                "GRANT SELECT ON `shop`.* TO `dbmon`@`10.1.%`",
                "이건 GRANT 문이 아니다",
                "GRANT SELECT ON *.tbl TO `dbmon`@`10.1.%`", // 세션 의존 범위
            ],
            SchemaNameMode::Pattern,
        );
        assert_eq!(unparsed.len(), 2, "{unparsed:?}");
        assert!(set.covers(&GrantScope::Schema("shop".into()), "SELECT"));
    }

    // ── 집합 대수 ─────────────────────────────────────────────────────────────

    fn set_of(pairs: &[(GrantScope, &[&str])]) -> GrantSet {
        let mut s = GrantSet::default();
        for (scope, privs) in pairs {
            s.add(
                scope.clone(),
                privs.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            );
        }
        s
    }

    /// **상위 범위가 하위를 덮는다.** 이걸 보지 않으면 이미 있는 권한을 다시
    /// `GRANT` 하고, 멱등성(M3-8)이 깨진다.
    #[test]
    fn global_privileges_cover_schema_and_table_scopes() {
        let g = set_of(&[(GrantScope::Global, &["SELECT"])]);
        assert!(g.covers(&GrantScope::Schema("shop".into()), "SELECT"));
        assert!(g.covers(&GrantScope::Table("shop".into(), "t".into()), "SELECT"));
        assert!(!g.covers(&GrantScope::Schema("shop".into()), "INSERT"));
    }

    #[test]
    fn schema_privileges_cover_tables_in_that_schema() {
        let g = set_of(&[(GrantScope::Schema("shop".into()), &["SELECT"])]);
        assert!(g.covers(&GrantScope::Table("shop".into(), "t".into()), "SELECT"));
        assert!(!g.covers(&GrantScope::Table("other".into(), "t".into()), "SELECT"));
        // 하위가 상위를 덮지는 않는다.
        assert!(!g.covers(&GrantScope::Global, "SELECT"));
    }

    #[test]
    fn missing_is_grouped_per_scope() {
        let current = set_of(&[(GrantScope::Global, &["PROCESS"])]);
        let desired = set_of(&[
            (GrantScope::Global, &["PROCESS", "SHOW VIEW"]),
            (GrantScope::Schema("shop".into()), &["SELECT"]),
        ]);
        let missing = current.missing_from(&desired);
        assert_eq!(missing.len(), 2);
        let global = missing
            .iter()
            .find(|(s, _)| *s == GrantScope::Global)
            .expect("전역");
        assert_eq!(
            global.1.iter().cloned().collect::<Vec<_>>(),
            vec!["SHOW VIEW"],
            "이미 있는 PROCESS 를 다시 요구하면 안 된다"
        );
    }

    #[test]
    fn nothing_is_missing_when_current_already_covers_everything() {
        let current = set_of(&[(GrantScope::Global, &["SELECT", "PROCESS"])]);
        let desired = set_of(&[
            (GrantScope::Schema("shop".into()), &["SELECT"]),
            (GrantScope::Global, &["PROCESS"]),
        ]);
        assert!(current.missing_from(&desired).is_empty());
    }

    /// 초과 권한은 **REVOKE 하지 않고 보고한다** (07 §2.6 4단계).
    #[test]
    fn excess_privileges_are_reported() {
        let current = set_of(&[
            (GrantScope::Global, &["PROCESS", "SUPER"]),
            (GrantScope::Schema("secret".into()), &["INSERT"]),
        ]);
        let desired = set_of(&[(GrantScope::Global, &["PROCESS"])]);
        let excess = current.excess_over(&desired);
        assert!(excess.contains(&(GrantScope::Global, "SUPER".into())));
        assert!(excess.contains(&(GrantScope::Schema("secret".into()), "INSERT".into())));
        assert_eq!(excess.len(), 2);
    }

    #[test]
    fn privilege_names_are_normalized() {
        let (_, p) = privs("GRANT process, replication   client ON *.* TO `d`@`h`");
        assert_eq!(p, vec!["PROCESS", "REPLICATION CLIENT"]);
    }

    /// 범위 렌더링은 **인용을 거친다** (T-18).
    #[test]
    fn scope_rendering_quotes_identifiers() {
        assert_eq!(
            GrantScope::Global
                .render(SchemaNameMode::Pattern)
                .as_deref(),
            Some("*.*")
        );
        assert_eq!(
            GrantScope::Schema("we`ird".into())
                .render(SchemaNameMode::Pattern)
                .as_deref(),
            Some("`we``ird`.*")
        );
        assert_eq!(
            GrantScope::Table("shop".into(), "t".into())
                .render(SchemaNameMode::Pattern)
                .as_deref(),
            Some("`shop`.`t`")
        );
        // 인용할 수 없는 이름은 렌더되지 않는다.
        assert_eq!(
            GrantScope::Schema("a\nb".into()).render(SchemaNameMode::Pattern),
            None
        );
    }
}
