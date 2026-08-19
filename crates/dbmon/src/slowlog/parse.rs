//! 슬로우 로그 파서 (M2-7) — **실행별 정확 지표의 유일한 출처.**
//!
//! # 왜 이것이 선택이 아닌가
//!
//! 실측 결과 `events_statements_current.ROWS_EXAMINED` 는 **실행 중 문장에 대해 0**
//! 이다([19 §G2](../../../../docs/19-m1-findings.md)). 완료 후에도
//! `events_statements_history` 에는 남지 않는다(스레드 버퍼라 커넥션이 끊기면 사라진다).
//! 다이제스트 누산기에는 있지만 그건 합계다.
//!
//! 즉 "이 실행이 60만 행을 읽었다" 는 말은 **슬로우 로그로만** 할 수 있다.
//! F5 병합(실시간 확정 + 슬로우로그 백필)이 필수인 이유가 이것이다.
//!
//! # 측정으로 확인한 형식 (MySQL 8.4.11, `log_timestamps=UTC`)
//!
//! ```text
//! # Time: 2026-08-19T13:48:54.659276Z      ← **완료 시각** (C-26 확인)
//! # User@Host: loadgen[loadgen] @ localhost []  Id: 25192
//! # Query_time: 8.003907  Lock_time: 0.000000 Rows_sent: 1  Rows_examined: 1
//! SET timestamp=1787147326;                ← 시작 시각, **초 단위**
//! SELECT /* c26-probe */ SLEEP(8);
//! ```
//!
//! | 항목 | 측정 결과 |
//! |---|---|
//! | `# Time:` | 완료 시각 (시작 13:48:46 + 8초 = 13:48:54) |
//! | 시작 시각 유도 | `Time − Query_time` = 13:48:46.655 (**소수점까지**) |
//! | `SET timestamp` | 1787147326 = 13:48:46 (초 단위 — 정밀도가 낮다) |
//! | 같은 초 연속 3건 | `# Time:` 이 **생략되지 않는다** — 각 엔트리가 자기 헤더를 갖는다 |
//!
//! 시작 시각은 `Time − Query_time` 을 쓴다. `SET timestamp` 보다 정밀하고,
//! ±2초 병합 창에서 그 차이가 실제로 중요하다.
//!
//! # 왜 임계값 아래 엔트리를 버리는가
//!
//! `record_id` 는 `(instance, thread_id, started_at_sec)` 다 — **초 단위**다.
//! 서버의 `long_query_time` 이 1초 미만이면 같은 커넥션의 여러 실행이 같은 초에
//! 들어오고, 그 전부가 **하나의 레코드로 병합된다**(조용한 데이터 손실).
//! 실측에서 `long_query_time=0.1` 로 3건이 같은 초·같은 `Id` 로 기록되는 것을 봤다.
//!
//! `long_query_time` 은 우리가 통제하지 않는다. 그래서 **우리 임계값 미만을 버린다** —
//! 임계값이 1초 이상임은 설정 검증이 보장하므로(`1..=3600`) 충돌이 불가능해진다.

use dbmon_core::time::EpochMs;

/// 파싱된 슬로우 로그 엔트리. **도메인 타입이 아니다** — 로그가 말한 것만 담는다.
///
/// `SlowQuery` 로 옮기는 일은 호출부가 한다(인스턴스·환경·정책을 알아야 한다).
#[derive(Debug, Clone, PartialEq)]
pub struct SlowLogEntry {
    /// `# Time:` — **완료 시각** (C-26, 측정으로 확인).
    pub ended_at_ms: EpochMs,
    /// `Time − Query_time`. `SET timestamp` 보다 정밀하다.
    pub started_at_ms: EpochMs,
    pub duration_ms: i64,
    pub lock_time_ms: i64,
    /// `Id:` — 커넥션 식별자. `record_id` 의 `thread_id` 다.
    pub thread_id: u64,
    pub db_user: Option<String>,
    pub db_host: Option<String>,
    /// `use <db>;` 가 있었으면 그 값.
    pub schema_name: Option<String>,
    pub rows_sent: Option<u64>,
    pub rows_examined: Option<u64>,
    pub rows_affected: Option<u64>,
    pub sql_text: String,
}

/// 엔트리를 못 만든 이유. **버리기 전에 사유를 남긴다.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// 필수 헤더가 없다. 형식이 바뀌었거나 잘린 조각이다.
    MissingHeader { what: &'static str },
    /// 시각을 해석할 수 없다.
    BadTimestamp { got: String },
    /// 관리 명령(`# administrator command: Quit`). 쿼리가 아니다.
    AdminCommand,
    /// SQL 본문이 없다.
    EmptySql,
    /// **우리 임계값 미만이다.** 초 단위 `record_id` 충돌을 막기 위해 버린다.
    BelowThreshold { duration_ms: i64 },
}

/// 파싱 결과. 버린 것을 **세어서 보고한다** — 조용히 버리면 "왜 백필이 안 되나" 를
/// 추적할 수 없다.
#[derive(Debug, Default)]
pub struct ParseOutcome {
    pub entries: Vec<SlowLogEntry>,
    pub skipped: Vec<SkipReason>,
}

impl ParseOutcome {
    /// 사유별 개수. 로그에 그대로 쓴다.
    pub fn skip_counts(&self) -> std::collections::BTreeMap<&'static str, usize> {
        let mut m = std::collections::BTreeMap::new();
        for r in &self.skipped {
            let key = match r {
                SkipReason::MissingHeader { .. } => "missing_header",
                SkipReason::BadTimestamp { .. } => "bad_timestamp",
                SkipReason::AdminCommand => "admin_command",
                SkipReason::EmptySql => "empty_sql",
                SkipReason::BelowThreshold { .. } => "below_threshold",
            };
            *m.entry(key).or_insert(0) += 1;
        }
        m
    }
}

/// 슬로우 로그 텍스트를 파싱한다. **순수 함수** — 파일도 AWS 도 모른다.
///
/// `min_duration_ms` 미만 엔트리는 버린다(위 모듈 문서 참고).
pub fn parse(text: &str, min_duration_ms: i64) -> ParseOutcome {
    let mut outcome = ParseOutcome::default();

    // `# Time:` 을 경계로 자른다. 첫 조각은 서버 기동 배너이므로 버린다.
    for block in split_entries(text) {
        match parse_entry(block, min_duration_ms) {
            Ok(entry) => outcome.entries.push(entry),
            Err(reason) => outcome.skipped.push(reason),
        }
    }
    outcome
}

/// `# Time:` 헤더를 기준으로 엔트리를 나눈다.
///
/// **`# Time:` 이 없는 선행 텍스트는 버린다.** 슬로우 로그 파일은 서버 기동 시
/// 배너(`/usr/sbin/mysqld, Version: ...`)로 시작한다. 그걸 엔트리로 오해하면
/// 첫 쿼리마다 파싱 실패가 하나씩 생긴다.
fn split_entries(text: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    for (idx, line) in line_offsets(text) {
        if line.starts_with("# Time:") {
            starts.push(idx);
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(text.len());
        out.push(&text[start..end]);
    }
    out
}

/// 각 줄의 시작 바이트 오프셋과 내용.
fn line_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0usize;
    text.split_inclusive('\n').map(move |raw| {
        let at = offset;
        offset += raw.len();
        (at, raw.trim_end_matches(['\n', '\r']))
    })
}

fn parse_entry(block: &str, min_duration_ms: i64) -> Result<SlowLogEntry, SkipReason> {
    let mut ended_at_ms: Option<EpochMs> = None;
    let mut duration_ms: Option<i64> = None;
    let mut lock_time_ms: i64 = 0;
    let mut thread_id: Option<u64> = None;
    let mut db_user = None;
    let mut db_host = None;
    let mut schema_name = None;
    let mut rows_sent = None;
    let mut rows_examined = None;
    let mut rows_affected = None;
    let mut sql = String::new();

    for (_, line) in line_offsets(block) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("# Time:") {
            ended_at_ms = Some(parse_timestamp(rest.trim())?);
        } else if let Some(rest) = trimmed.strip_prefix("# User@Host:") {
            let (u, h, id) = parse_user_host(rest);
            db_user = u;
            db_host = h;
            thread_id = id;
        } else if trimmed.starts_with("# Query_time:") {
            duration_ms = Some(secs_field(trimmed, "Query_time:").unwrap_or(0));
            lock_time_ms = secs_field(trimmed, "Lock_time:").unwrap_or(0);
            rows_sent = int_field(trimmed, "Rows_sent:");
            rows_examined = int_field(trimmed, "Rows_examined:");
        } else if trimmed.starts_with('#') {
            // RDS·Percona 가 덧붙이는 줄들(`Thread_id`, `Rows_affected`, `Bytes_sent` …).
            // **모르는 줄을 실패로 만들지 않는다** — 벤더가 필드를 추가할 때마다
            // 백필 전체가 멈추면 안 된다.
            if let Some(v) = int_field(trimmed, "Rows_affected:") {
                rows_affected = Some(v);
            }
            if trimmed.contains("administrator command:") {
                return Err(SkipReason::AdminCommand);
            }
        } else if let Some(db) = trimmed
            .strip_prefix("use ")
            .or_else(|| trimmed.strip_prefix("USE "))
        {
            schema_name = Some(db.trim_end_matches(';').trim().to_string());
        } else if trimmed.starts_with("SET timestamp=") {
            // 시작 시각을 여기서 읽지 않는다 — 초 단위라 `Time − Query_time` 보다
            // 정밀도가 낮다. 측정으로 확인했다([19 §G2a]).
        } else {
            // SQL 본문. **여러 줄일 수 있다.**
            if !sql.is_empty() {
                sql.push('\n');
            }
            sql.push_str(line.trim_end());
        }
    }

    let ended_at_ms = ended_at_ms.ok_or(SkipReason::MissingHeader { what: "Time" })?;
    let duration_ms = duration_ms.ok_or(SkipReason::MissingHeader { what: "Query_time" })?;
    let thread_id = thread_id.ok_or(SkipReason::MissingHeader { what: "Id" })?;

    let sql = sql.trim().trim_end_matches(';').trim().to_string();
    if sql.is_empty() {
        return Err(SkipReason::EmptySql);
    }
    // **임계값 미만은 버린다.** 초 단위 `record_id` 충돌을 막는다(모듈 문서 참고).
    if duration_ms < min_duration_ms {
        return Err(SkipReason::BelowThreshold { duration_ms });
    }

    Ok(SlowLogEntry {
        ended_at_ms,
        // **`Time − Query_time`.** `SET timestamp` 보다 정밀하다.
        started_at_ms: ended_at_ms - duration_ms,
        duration_ms,
        lock_time_ms,
        thread_id,
        db_user,
        db_host,
        schema_name,
        rows_sent,
        rows_examined,
        rows_affected,
        sql_text: sql,
    })
}

/// `2026-08-19T13:48:54.659276Z` → epoch ms.
///
/// **`Z` 를 요구한다.** `log_timestamps=SYSTEM` 이면 오프셋 없는 서버 로컬 시각이
/// 찍히는데, 그걸 UTC 로 읽으면 시간대만큼 어긋난 레코드가 저장된다 — ±2초 병합 창이
/// 통째로 빗나가고, 원인이 "병합이 안 된다" 로만 보인다. 그래서 **fail-closed** 다.
fn parse_timestamp(s: &str) -> Result<EpochMs, SkipReason> {
    use chrono::{DateTime, Utc};

    if !s.ends_with('Z') && !s.contains('+') {
        return Err(SkipReason::BadTimestamp { got: s.to_string() });
    }
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc).timestamp_millis())
        .map_err(|_| SkipReason::BadTimestamp { got: s.to_string() })
}

/// `loadgen[loadgen] @ localhost []  Id: 25192` → (user, host, id).
fn parse_user_host(rest: &str) -> (Option<String>, Option<String>, Option<u64>) {
    let id = rest
        .split("Id:")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok());

    let before_id = rest.split("Id:").next().unwrap_or("");
    let user = before_id
        .split('[')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // `@ host [ip]` — 호스트명이 비면 대괄호 안의 IP 를 쓴다.
    let after_at = before_id.split('@').nth(1).unwrap_or("").trim();
    let host_name = after_at
        .split('[')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let host_ip = after_at
        .split_once('[')
        .and_then(|(_, r)| r.split_once(']'))
        .map(|(ip, _)| ip.trim())
        .filter(|s| !s.is_empty());
    let host = host_name.or(host_ip).map(str::to_string);

    (user, host, id)
}

/// `Query_time: 8.003907` → 8004 (ms).
fn secs_field(line: &str, key: &str) -> Option<i64> {
    let raw = line.split(key).nth(1)?.split_whitespace().next()?;
    let secs: f64 = raw.parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some((secs * 1000.0).round() as i64)
}

fn int_field(line: &str, key: &str) -> Option<u64> {
    line.split(key)
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실측한 실제 로그 조각. **손으로 만든 예시가 아니다.**
    const MEASURED: &str = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: loadgen[loadgen] @ localhost []  Id: 25192
# Query_time: 8.003907  Lock_time: 0.000000 Rows_sent: 1  Rows_examined: 1
SET timestamp=1787147326;
SELECT /* c26-probe */ SLEEP(8);
";

    #[test]
    fn parses_the_measured_entry() {
        let out = parse(MEASURED, 1_000);
        assert_eq!(out.skipped.len(), 0, "{:?}", out.skipped);
        let e = &out.entries[0];

        // **C-26: `# Time:` 은 완료 시각이다.** 측정으로 확인했다.
        assert_eq!(e.ended_at_ms, 1_787_147_334_659);
        assert_eq!(e.duration_ms, 8_004);
        // 시작 = 완료 − 소요. `SET timestamp`(1787147326000)보다 정밀하다.
        assert_eq!(e.started_at_ms, 1_787_147_326_655);
        assert_eq!(e.thread_id, 25_192);
        assert_eq!(e.db_user.as_deref(), Some("loadgen"));
        assert_eq!(e.db_host.as_deref(), Some("localhost"));
        assert_eq!(e.rows_sent, Some(1));
        assert_eq!(e.rows_examined, Some(1));
        assert_eq!(e.sql_text, "SELECT /* c26-probe */ SLEEP(8)");
    }

    /// **시작 시각이 `SET timestamp` 보다 정밀해야 한다.**
    ///
    /// `SET timestamp` 는 초 단위다. ±2초 병합 창에서 그 차이가 실제로 중요하다.
    #[test]
    fn start_time_is_more_precise_than_set_timestamp() {
        let e = &parse(MEASURED, 1_000).entries[0];
        let set_timestamp_ms = 1_787_147_326_000i64;
        assert_ne!(e.started_at_ms, set_timestamp_ms);
        assert!(
            e.started_at_ms > set_timestamp_ms,
            "SET timestamp 는 내림이므로 유도값이 더 크거나 같다"
        );
        assert!((e.started_at_ms - set_timestamp_ms).abs() < 1_000);
    }

    /// 실측한 버스트 3건 — 같은 초·같은 `Id` 다.
    const BURST: &str = "\
# Time: 2026-08-19T13:49:34.540794Z
# User@Host: loadgen[loadgen] @ localhost []  Id: 25203
# Query_time: 0.201446  Lock_time: 0.000000 Rows_sent: 1  Rows_examined: 1
SET timestamp=1787147374;
SELECT /* burst-a */ SLEEP(0.2);
# Time: 2026-08-19T13:49:34.741614Z
# User@Host: loadgen[loadgen] @ localhost []  Id: 25203
# Query_time: 0.200501  Lock_time: 0.000000 Rows_sent: 1  Rows_examined: 1
SET timestamp=1787147374;
SELECT /* burst-b */ SLEEP(0.2);
# Time: 2026-08-19T13:49:34.945792Z
# User@Host: loadgen[loadgen] @ localhost []  Id: 25203
# Query_time: 0.203306  Lock_time: 0.000000 Rows_sent: 1  Rows_examined: 1
SET timestamp=1787147374;
SELECT /* burst-c */ SLEEP(0.2);
";

    /// **엔트리마다 자기 `# Time:` 을 갖는다** — 같은 초여도 생략되지 않는다.
    ///
    /// "같은 초면 헤더가 생략된다" 는 통념이 있는데 MySQL 8.4 에서는 아니다.
    /// 측정으로 확인했다.
    #[test]
    fn each_entry_carries_its_own_time_header_even_within_one_second() {
        let out = parse(BURST, 0);
        assert_eq!(out.entries.len(), 3, "{:?}", out.skip_counts());
        // 완료 시각이 서로 다르다 — 마이크로초까지 기록된다.
        let ends: Vec<i64> = out.entries.iter().map(|e| e.ended_at_ms).collect();
        assert_eq!(ends.len(), 3);
        assert!(ends[0] < ends[1] && ends[1] < ends[2], "{ends:?}");
    }

    /// **임계값 미만은 버린다 — 그게 조용한 병합을 막는 유일한 장치다.**
    ///
    /// 세 엔트리는 같은 `thread_id`·같은 시작 **초**다. `record_id` 가 초 단위이므로
    /// 그대로 저장하면 셋이 하나로 병합된다(두 건은 사라진다).
    /// 서버의 `long_query_time` 은 우리가 통제하지 않으므로 여기서 막는다.
    #[test]
    fn sub_threshold_entries_are_dropped_to_prevent_silent_merging() {
        let out = parse(BURST, 1_000);
        assert_eq!(out.entries.len(), 0, "임계값 미만이 통과했다");
        assert_eq!(out.skipped.len(), 3);
        assert_eq!(out.skip_counts().get("below_threshold"), Some(&3));

        // 그리고 그 위험이 실재함을 고정한다: 같은 초 + 같은 스레드다.
        let all = parse(BURST, 0);
        let secs: Vec<i64> = all
            .entries
            .iter()
            .map(|e| e.started_at_ms.div_euclid(1000))
            .collect();
        assert_eq!(secs[0], secs[1], "같은 초가 아니면 이 테스트가 무의미하다");
        assert_eq!(secs[1], secs[2]);
        assert!(all.entries.iter().all(|e| e.thread_id == 25_203));
    }

    /// **서버 기동 배너를 엔트리로 오해하지 않는다.**
    #[test]
    fn the_startup_banner_is_not_an_entry() {
        let text = format!(
            "/usr/sbin/mysqld, Version: 8.4.11 (MySQL). started with:\n\
             Tcp port: 3306  Unix socket: /var/run/mysqld/mysqld.sock\n\
             Time                 Id Command    Argument\n{MEASURED}"
        );
        let out = parse(&text, 1_000);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(
            out.skipped.len(),
            0,
            "배너를 엔트리로 셌다: {:?}",
            out.skipped
        );
    }

    /// **`log_timestamps=SYSTEM` 은 거부한다 (fail-closed).**
    ///
    /// 오프셋 없는 로컬 시각을 UTC 로 읽으면 시간대만큼 어긋난 레코드가 저장되고,
    /// ±2초 병합 창이 통째로 빗나간다. 원인은 "병합이 안 된다" 로만 보인다.
    #[test]
    fn timestamps_without_a_zone_are_rejected() {
        let text = "\
# Time: 2026-08-19 22:48:54
# User@Host: loadgen[loadgen] @ localhost []  Id: 1
# Query_time: 8.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SELECT 1;
";
        let out = parse(text, 1_000);
        assert_eq!(out.entries.len(), 0, "시간대 없는 시각을 UTC 로 읽었다");
        assert_eq!(out.skip_counts().get("bad_timestamp"), Some(&1));
    }

    /// 오프셋이 붙은 형태는 받아들인다.
    #[test]
    fn timestamps_with_an_offset_are_accepted() {
        let text = "\
# Time: 2026-08-19T22:48:54.659276+09:00
# User@Host: u[u] @ h []  Id: 7
# Query_time: 8.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SELECT 1;
";
        let out = parse(text, 1_000);
        assert_eq!(out.entries.len(), 1, "{:?}", out.skipped);
        // +09:00 22:48:54 == UTC 13:48:54
        assert_eq!(out.entries[0].ended_at_ms, 1_787_147_334_659);
    }

    /// 관리 명령은 쿼리가 아니다.
    #[test]
    fn administrator_commands_are_skipped() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: root[root] @ localhost []  Id: 3
# Query_time: 2.0  Lock_time: 0.0 Rows_sent: 0  Rows_examined: 0
# administrator command: Quit;
";
        let out = parse(text, 1_000);
        assert_eq!(out.entries.len(), 0);
        assert_eq!(out.skip_counts().get("admin_command"), Some(&1));
    }

    /// **모르는 `#` 줄이 백필을 멈추면 안 된다.**
    ///
    /// RDS·Percona 는 필드를 덧붙인다. 벤더가 하나 추가할 때마다 파싱이 전부
    /// 실패하면 백필이 조용히 죽는다.
    #[test]
    fn unknown_header_lines_do_not_fail_the_entry() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: u[u] @ h []  Id: 9
# Thread_id: 9  Schema: shop  QC_hit: No
# Query_time: 3.5  Lock_time: 0.25 Rows_sent: 2  Rows_examined: 900000
# Rows_affected: 4
# Bytes_sent: 1234  Tmp_tables: 1
# 새로운_벤더_필드: 무엇이든
use shop;
SELECT a FROM t;
";
        let out = parse(text, 1_000);
        assert_eq!(out.entries.len(), 1, "{:?}", out.skipped);
        let e = &out.entries[0];
        assert_eq!(e.rows_examined, Some(900_000));
        assert_eq!(e.rows_affected, Some(4));
        assert_eq!(e.lock_time_ms, 250);
        assert_eq!(e.schema_name.as_deref(), Some("shop"));
        assert_eq!(e.sql_text, "SELECT a FROM t");
    }

    /// **여러 줄 SQL 을 붙여야 한다.** 첫 줄만 저장하면 문장이 잘린다.
    #[test]
    fn multi_line_sql_is_joined() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: u[u] @ h []  Id: 9
# Query_time: 3.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SELECT a,
       b
  FROM t
 WHERE c = 1;
";
        let out = parse(text, 1_000);
        assert_eq!(out.entries.len(), 1, "{:?}", out.skipped);
        let sql = &out.entries[0].sql_text;
        assert!(sql.contains("SELECT a,"), "{sql}");
        assert!(sql.contains("WHERE c = 1"), "{sql}");
        assert_eq!(sql.lines().count(), 4);
    }

    /// 필수 헤더가 없으면 **사유를 남기고** 버린다.
    #[test]
    fn entries_missing_required_headers_are_reported() {
        // `Id:` 가 없다 — `record_id` 를 만들 수 없다.
        let no_id = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: u[u] @ h []
# Query_time: 3.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SELECT 1;
";
        let out = parse(no_id, 1_000);
        assert_eq!(out.entries.len(), 0);
        assert_eq!(out.skip_counts().get("missing_header"), Some(&1));

        // `Query_time:` 이 없다 — 소요 시간을 만들어 낼 수 없다.
        let no_time = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: u[u] @ h []  Id: 1
SELECT 1;
";
        assert_eq!(parse(no_time, 1_000).entries.len(), 0);
    }

    /// SQL 이 없으면 버린다 — 빈 문장을 저장하면 다이제스트가 오염된다.
    #[test]
    fn entries_without_sql_are_skipped() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: u[u] @ h []  Id: 1
# Query_time: 3.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
";
        let out = parse(text, 1_000);
        assert_eq!(out.entries.len(), 0);
        assert_eq!(out.skip_counts().get("empty_sql"), Some(&1));
    }

    /// 호스트명이 비면 IP 를 쓴다 — RDS 는 `[10.0.3.44]` 형태를 낸다.
    #[test]
    fn host_falls_back_to_the_ip_when_the_name_is_empty() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: app[app] @  [10.0.3.44]  Id: 42
# Query_time: 3.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SELECT 1;
";
        let e = &parse(text, 1_000).entries[0];
        assert_eq!(e.db_host.as_deref(), Some("10.0.3.44"));
        assert_eq!(e.db_user.as_deref(), Some("app"));
        assert_eq!(e.thread_id, 42);
    }

    /// 빈 입력·잘린 조각에 패닉하지 않는다.
    #[test]
    fn degenerate_input_does_not_panic() {
        for text in ["", "\n", "# Time:", "# Time: \n# Query_time:", "쓰레기"] {
            let out = parse(text, 1_000);
            assert!(out.entries.is_empty(), "{text:?} 에서 엔트리가 나왔다");
        }
    }

    /// 여러 엔트리가 섞여도 각각 독립적으로 처리된다 —
    /// **하나가 실패해도 나머지를 버리지 않는다.**
    #[test]
    fn a_bad_entry_does_not_discard_the_good_ones() {
        let text = format!(
            "{MEASURED}\
# Time: 깨진시각
# User@Host: u[u] @ h []  Id: 2
# Query_time: 5.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SELECT 2;
{MEASURED}"
        );
        let out = parse(&text, 1_000);
        assert_eq!(out.entries.len(), 2, "좋은 엔트리를 버렸다");
        assert_eq!(out.skip_counts().get("bad_timestamp"), Some(&1));
    }
}
