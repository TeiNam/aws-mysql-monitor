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
    ///
    /// **원문을 담지 않는다.** 로그 내용은 L1(헤더 위조)로 공격자 통제 하에 있을 수
    /// 있고, 이 타입이 `Debug` 로 어딘가 찍히는 순간 유출된다. 타입 자체가 들 수
    /// 없게 두는 것이 호출부의 실수와 무관해지는 방법이다.
    BadTimestamp,
    /// 관리 명령(`# administrator command: Quit`). 쿼리가 아니다.
    AdminCommand,
    /// SQL 본문이 없다.
    EmptySql,
    /// **우리 임계값 미만이다.** 초 단위 `record_id` 충돌을 막기 위해 버린다.
    BelowThreshold { duration_ms: i64 },
    /// 소요 시간이 있을 수 없는 값이다(음수, 24시간 초과, 오버플로).
    ImplausibleDuration { duration_ms: i64 },
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
                SkipReason::BadTimestamp => "bad_timestamp",
                SkipReason::AdminCommand => "admin_command",
                SkipReason::EmptySql => "empty_sql",
                SkipReason::BelowThreshold { .. } => "below_threshold",
                SkipReason::ImplausibleDuration { .. } => "implausible_duration",
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
    let mut current: Option<Builder> = None;

    for line in lines_of(text) {
        // **`# Time:` 이 새 엔트리를 시작하는지는 상태에 달렸다.** 아래 참고.
        if line.starts_with("# Time:") && current.as_ref().is_none_or(Builder::accepts_boundary) {
            if let Some(b) = current.take() {
                push(&mut outcome, b, min_duration_ms);
            }
            let mut b = Builder::default();
            b.feed(line);
            current = Some(b);
            continue;
        }
        if let Some(b) = current.as_mut() {
            b.feed(line);
        }
        // `# Time:` 앞의 텍스트(서버 기동 배너)는 버린다.
    }
    if let Some(b) = current.take() {
        push(&mut outcome, b, min_duration_ms);
    }
    outcome
}

/// 줄 단위로 나눈다. CRLF 도 처리한다.
fn lines_of(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
        .map(|raw| raw.trim_end_matches(['\n', '\r']))
}

fn push(outcome: &mut ParseOutcome, b: Builder, min_duration_ms: i64) {
    match b.finish(min_duration_ms) {
        Ok(entry) => outcome.entries.push(entry),
        Err(reason) => outcome.skipped.push(reason),
    }
}

/// 엔트리 하나를 조립한다. **헤더 구간과 본문 구간을 상태로 구분한다.**
///
/// # 왜 상태가 필요한가 (보안)
///
/// 처음 구현은 블록의 **모든 줄**에 헤더 접두를 다시 판정하고 매칭마다 덮어썼다.
/// SQL 은 여러 줄일 수 있고 MySQL 은 문장을 개행 그대로 기록하므로, **대상 DB 에
/// 쿼리를 던질 수 있는 아무 계정이 헤더를 위조**할 수 있었다:
///
/// ```text
/// SELECT SLEEP(3) /*
/// # User@Host: 4111-1111-1111-1111[x] @ evil []  Id: 999
/// # Query_time: 120.0  Rows_examined: 999999999
/// */
/// ```
///
/// 실제로 통했다 — `db_user` 에 카드번호 모양 문자열이, `thread_id` 에 999 가,
/// `duration` 에 120초가 들어갔다. `db_user`·`db_host`·`schema_name` 은 리터럴
/// 정책의 마스킹 대상이 아니므로 **정책이 `masked` 여도 임의 문자열이 저장**되고,
/// 위조된 `thread_id` 는 다른 실행의 `record_id` 를 겨냥할 수 있었다.
///
/// 그래서 **본문이 시작되면 그 뒤로는 어떤 줄도 헤더가 아니다.** 헤더도 첫 등장만
/// 채택한다(심층 방어).
#[derive(Default)]
struct Builder {
    ended_at_ms: Option<EpochMs>,
    bad_timestamp: bool,
    duration_ms: Option<i64>,
    lock_time_ms: Option<i64>,
    thread_id: Option<u64>,
    db_user: Option<String>,
    db_host: Option<String>,
    schema_name: Option<String>,
    rows_sent: Option<u64>,
    rows_examined: Option<u64>,
    rows_affected: Option<u64>,
    admin_command: bool,
    sql: String,
    /// 본문이 시작됐다. 이후 모든 줄은 SQL 이다.
    in_body: bool,
}

impl Builder {
    /// **`# Time:` 을 새 엔트리 경계로 받아들일 수 있는가.**
    ///
    /// 본문이 아직 시작되지 않았거나(연속 헤더 — 비정상이지만 무해)
    /// **문장이 `;` 로 종료됐으면** 경계다.
    ///
    /// # 왜 `;` 인가 — 측정 근거
    ///
    /// 실제 로그 1,983 엔트리 **전부**가 `;` 로 끝났다(100%). MySQL 은 기록한 문장을
    /// 항상 종료 문자와 함께 쓴다. 반면 위조된 `# Time:` 은 공격자 문장의 종료 `;`
    /// **앞**에 있다 — 그래서 이 규칙이 유령 엔트리 주입을 막는다.
    ///
    /// 안전 밸브: 본문이 [`MAX_BODY_BYTES`] 를 넘으면 종료되지 않았어도 경계로 본다.
    /// 규칙이 틀린 경우(종료 문자 없는 엔트리)에 뒤 엔트리들을 무한히 삼키지 않는다.
    fn accepts_boundary(&self) -> bool {
        if !self.in_body {
            return true;
        }
        if self.sql.len() > MAX_BODY_BYTES {
            return true;
        }
        self.sql
            .trim_end()
            .rsplit('\n')
            .next()
            .is_some_and(|l| l.trim_end().ends_with(';'))
    }

    fn feed(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() && !self.in_body {
            return;
        }

        if !self.in_body {
            if let Some(rest) = trimmed.strip_prefix("# Time:") {
                if self.ended_at_ms.is_none() && !self.bad_timestamp {
                    match parse_timestamp(rest.trim()) {
                        Ok(t) => self.ended_at_ms = Some(t),
                        Err(()) => self.bad_timestamp = true,
                    }
                }
                return;
            }
            if let Some(rest) = trimmed.strip_prefix("# User@Host:") {
                if self.thread_id.is_none() {
                    let (u, h, id) = parse_user_host(rest);
                    self.db_user = u;
                    self.db_host = h;
                    self.thread_id = id;
                }
                return;
            }
            if trimmed.starts_with("# Query_time:") {
                if self.duration_ms.is_none() {
                    self.duration_ms = secs_field(trimmed, "Query_time:");
                    self.lock_time_ms = secs_field(trimmed, "Lock_time:");
                    self.rows_sent = int_field(trimmed, "Rows_sent:");
                    self.rows_examined = int_field(trimmed, "Rows_examined:");
                    // **같은 줄에 있을 수도 있다.** Percona·Aurora 는 `Rows_affected` 를
                    // `Query_time` 줄에 붙인다. 별도 줄만 보면 조용히 누락된다.
                    self.rows_affected = int_field(trimmed, "Rows_affected:");
                }
                return;
            }
            if trimmed.starts_with('#') {
                // RDS·Percona 가 덧붙이는 줄들. **모르는 줄을 실패로 만들지 않는다** —
                // 벤더가 필드를 추가할 때마다 백필 전체가 멈추면 안 된다.
                if trimmed.contains("administrator command:") {
                    self.admin_command = true;
                }
                if self.rows_affected.is_none() {
                    self.rows_affected = int_field(trimmed, "Rows_affected:");
                }
                return;
            }
            if let Some(db) = trimmed
                .strip_prefix("use ")
                .or_else(|| trimmed.strip_prefix("USE "))
            {
                if self.schema_name.is_none() {
                    self.schema_name = Some(db.trim_end_matches(';').trim().to_string());
                }
                return;
            }
            if trimmed.starts_with("SET timestamp=") {
                // 시작 시각을 여기서 읽지 않는다 — 초 단위라 `Time − Query_time` 보다
                // 정밀도가 낮다. 측정으로 확인했다([19 §G3]).
                //
                // **하지만 헤더 구간의 끝 표지로는 쓴다.** `#` 는 MySQL 의 주석
                // 문자이므로 사용자 쿼리에 `# 설명` 줄이 있을 수 있다. 그걸 헤더로
                // 오해하면 SQL 에서 사라지고, **훼손된 텍스트로 계산한 다이제스트**가
                // 실시간 경로와 영구히 달라진다(유령 다이제스트).
                //
                // 실측: 실제 로그 1,983 엔트리 **전부**가 `SET timestamp=` 를 갖고,
                // 전부 그 직후가 SQL 이었다(100%). 없는 벤더 형식은 아래 폴백을 탄다.
                self.in_body = true;
                return;
            }
            // 헤더가 아니다 → 여기서부터 본문이다 (`SET timestamp=` 가 없는 형식).
            self.in_body = true;
        }

        // 본문. **여기서는 어떤 줄도 헤더가 아니다.**
        if !self.sql.is_empty() {
            self.sql.push('\n');
        }
        self.sql.push_str(line.trim_end());
    }

    fn finish(self, min_duration_ms: i64) -> Result<SlowLogEntry, SkipReason> {
        if self.admin_command {
            return Err(SkipReason::AdminCommand);
        }
        if self.bad_timestamp {
            return Err(SkipReason::BadTimestamp);
        }
        let ended_at_ms = self
            .ended_at_ms
            .ok_or(SkipReason::MissingHeader { what: "Time" })?;
        let duration_ms = self
            .duration_ms
            .ok_or(SkipReason::MissingHeader { what: "Query_time" })?;
        let thread_id = self
            .thread_id
            .ok_or(SkipReason::MissingHeader { what: "Id" })?;

        // **소요 시간에 상한을 둔다.**
        //
        // `secs_field` 는 `f64 → i64` 포화 변환이므로 `Query_time: 1e308` 이
        // `i64::MAX` 가 된다. 그러면 (a) `ended − duration` 이 오버플로하고
        // (b) 슬로우로그 duration 이 권위값이라 그 쓰레기가 영구히 저장된다.
        // L1 의 헤더 위조와 결합하면 공격자가 직접 넣을 수 있었다.
        if !(0..=MAX_DURATION_MS).contains(&duration_ms) {
            return Err(SkipReason::ImplausibleDuration { duration_ms });
        }
        let lock_time_ms = self.lock_time_ms.unwrap_or(0).clamp(0, MAX_DURATION_MS);

        // 오버플로 없이 시작 시각을 구한다.
        let started_at_ms = ended_at_ms
            .checked_sub(duration_ms)
            .ok_or(SkipReason::ImplausibleDuration { duration_ms })?;
        if started_at_ms < 0 {
            return Err(SkipReason::ImplausibleDuration { duration_ms });
        }

        let sql = self.sql.trim().trim_end_matches(';').trim().to_string();
        if sql.is_empty() {
            return Err(SkipReason::EmptySql);
        }
        if duration_ms < min_duration_ms {
            return Err(SkipReason::BelowThreshold { duration_ms });
        }

        Ok(SlowLogEntry {
            ended_at_ms,
            // **`Time − Query_time`.** `SET timestamp` 보다 정밀하다.
            started_at_ms,
            duration_ms,
            lock_time_ms,
            thread_id,
            // **메타 필드도 검증한다.** 리터럴 정책은 `sql_text` 에만 걸리므로,
            // 여기에 임의 문자열이 들어오면 정책을 우회해 저장된다.
            db_user: self.db_user.filter(|v| is_safe_identifier(v, MAX_USER_LEN)),
            db_host: self.db_host.filter(|v| is_safe_host(v)),
            schema_name: self
                .schema_name
                .filter(|v| is_safe_identifier(v, MAX_SCHEMA_LEN)),
            rows_sent: self.rows_sent,
            rows_examined: self.rows_examined,
            rows_affected: self.rows_affected,
            sql_text: sql,
        })
    }
}

/// 본문 안전 밸브. 이 크기를 넘으면 종료 문자가 없어도 다음 `# Time:` 을 경계로 본다.
const MAX_BODY_BYTES: usize = 1_048_576;

/// 소요 시간 상한 (24시간). 넘으면 파싱 오류로 본다.
const MAX_DURATION_MS: i64 = 24 * 60 * 60 * 1000;

const MAX_USER_LEN: usize = 96;
const MAX_SCHEMA_LEN: usize = 96;
const MAX_HOST_LEN: usize = 255;

/// MySQL 계정·스키마 이름으로 그럴듯한가.
///
/// **관용적으로 받지 않는다.** 위조된 값은 리터럴 정책을 우회해 저장되므로,
/// 의심스러우면 버리는 쪽이 맞다(필드가 비는 것이 개인정보가 저장되는 것보다 낫다).
fn is_safe_identifier(v: &str, max_len: usize) -> bool {
    !v.is_empty()
        && v.len() <= max_len
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'$'))
}

/// 호스트명 또는 IP.
fn is_safe_host(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= MAX_HOST_LEN
        && v.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'[' | b']')
        })
}

/// `2026-08-19T13:48:54.659276Z` → epoch ms.
///
/// **`Z` 를 요구한다.** `log_timestamps=SYSTEM` 이면 오프셋 없는 서버 로컬 시각이
/// 찍히는데, 그걸 UTC 로 읽으면 시간대만큼 어긋난 레코드가 저장된다 — ±2초 병합 창이
/// 통째로 빗나가고, 원인이 "병합이 안 된다" 로만 보인다. 그래서 **fail-closed** 다.
fn parse_timestamp(s: &str) -> Result<EpochMs, ()> {
    use chrono::{DateTime, Utc};

    if !s.ends_with('Z') && !s.contains('+') {
        return Err(());
    }
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc).timestamp_millis())
        .map_err(|_| ())
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

    /// **시작 시각은 `Time − Query_time` 이어야 한다.**
    ///
    /// 처음 쓴 테스트는 `started > set_timestamp*1000` 만 봤다 — `ended_at_ms` 를
    /// 그대로 반환해도 통과했다(2차 리뷰가 지적). 정확한 값을 단정한다.
    #[test]
    fn start_time_is_exactly_end_minus_duration() {
        let e = &parse(MEASURED, 1_000).entries[0];
        assert_eq!(
            e.started_at_ms,
            e.ended_at_ms - e.duration_ms,
            "시작 시각이 Time − Query_time 이 아니다"
        );
        // 그리고 `SET timestamp`(초 단위 내림)보다 정밀하다.
        let set_timestamp_ms = 1_787_147_326_000i64;
        assert_ne!(e.started_at_ms, set_timestamp_ms);
        assert_ne!(e.started_at_ms, e.ended_at_ms, "완료 시각을 그대로 썼다");
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

    /// 빈 입력·잘린 조각에 패닉하지 않는다. **그리고 사유를 남긴다.**
    ///
    /// 처음 쓴 테스트는 "엔트리 없음" 만 봐서 파서가 아무 일도 안 해도 통과했다
    /// (2차 리뷰가 지적). 잘린 조각은 **스킵 사유가 기록돼야** 한다.
    #[test]
    fn degenerate_input_is_skipped_with_a_reason() {
        // 엔트리 경계가 없는 입력 — 조각조차 아니다.
        for text in ["", "\n", "쓰레기", "SELECT 1;"] {
            let out = parse(text, 1_000);
            assert!(out.entries.is_empty(), "{text:?} 에서 엔트리가 나왔다");
            assert!(out.skipped.is_empty(), "{text:?} 를 엔트리로 셌다");
        }
        // `# Time:` 이 있으면 엔트리 시도이므로 **사유가 남아야** 한다.
        for text in [
            "# Time:",
            "# Time: \n# Query_time:",
            "# Time: 깨짐\nSELECT 1;",
        ] {
            let out = parse(text, 1_000);
            assert!(out.entries.is_empty(), "{text:?} 에서 엔트리가 나왔다");
            assert!(
                !out.skipped.is_empty(),
                "{text:?} 를 조용히 버렸다 — 사유가 없으면 추적할 수 없다"
            );
        }
    }

    /// **SQL 안의 `#` 주석이 살아 있어야 한다.**
    ///
    /// `#` 는 MySQL 의 주석 문자다. 헤더로 오해하면 SQL 에서 사라지고,
    /// **훼손된 텍스트로 계산한 다이제스트**가 실시간 경로와 영구히 달라진다.
    #[test]
    fn hash_comments_inside_sql_are_preserved() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: u[u] @ h []  Id: 9
# Query_time: 3.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SET timestamp=1787147326;
SELECT a
# 이건 사용자 주석이다
  FROM t;
";
        let out = parse(text, 1_000);
        assert_eq!(out.entries.len(), 1, "{:?}", out.skipped);
        let sql = &out.entries[0].sql_text;
        assert!(
            sql.contains("# 이건 사용자 주석이다"),
            "SQL 안의 # 주석이 사라졌다 — 다이제스트가 실시간 경로와 달라진다: {sql}"
        );
    }

    /// **본문이 헤더를 위조할 수 없다** (2차 리뷰 CRITICAL).
    ///
    /// 대상 DB 에 쿼리를 던질 수 있는 아무 계정이 헤더를 주입할 수 있었다.
    /// 실제로 통했다 — `db_user` 에 카드번호 모양 문자열, `thread_id` 에 999,
    /// `duration` 에 120초, `rows_examined` 에 999999999 가 들어갔다.
    ///
    /// `db_user`·`db_host`·`schema_name` 은 리터럴 정책의 마스킹 대상이 **아니므로**
    /// 정책이 `masked` 여도 임의 문자열이 저장됐고, 위조된 `thread_id` 는 다른
    /// 실행의 `record_id` 를 겨냥할 수 있었다.
    #[test]
    fn sql_body_cannot_forge_headers() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: app[app] @ realhost []  Id: 100
# Query_time: 3.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SET timestamp=1787147326;
SELECT SLEEP(3) /*
# User@Host: 4111-1111-1111-1111[x] @ evil []  Id: 999
# Query_time: 120.0  Lock_time: 0.0 Rows_sent: 0 Rows_examined: 999999999
use 유출된스키마;
*/;
";
        let out = parse(text, 1_000);
        assert_eq!(
            out.entries.len(),
            1,
            "유령 엔트리가 생겼다: {:?}",
            out.skipped
        );
        let e = &out.entries[0];

        assert_eq!(
            e.thread_id, 100,
            "thread_id 가 위조됐다 — 남의 레코드를 겨냥할 수 있다"
        );
        assert_eq!(e.db_user.as_deref(), Some("app"), "db_user 가 위조됐다");
        assert_eq!(
            e.db_host.as_deref(),
            Some("realhost"),
            "db_host 가 위조됐다"
        );
        assert_eq!(e.schema_name, None, "schema_name 이 주입됐다");
        assert_eq!(
            e.duration_ms, 3_000,
            "duration 이 위조됐다 (슬로우로그는 권위값이다)"
        );
        assert_eq!(e.rows_examined, Some(1), "rows_examined 가 위조됐다");
    }

    /// **메타 필드에 임의 문자열이 들어오면 버린다.**
    ///
    /// 리터럴 정책은 `sql_text` 에만 걸린다. 이 필드들이 검증되지 않으면
    /// 정책을 우회해 개인정보가 저장된다.
    #[test]
    fn implausible_metadata_fields_are_dropped() {
        let text = "\
# Time: 2026-08-19T13:48:54.659276Z
# User@Host: 4111-1111-1111-1111 1234[x] @ h []  Id: 5
# Query_time: 3.0  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1
SET timestamp=1787147326;
SELECT 1;
";
        let e = &parse(text, 1_000).entries[0];
        assert_eq!(
            e.db_user, None,
            "공백을 포함한 계정명을 통과시켰다 — 정책을 우회해 저장된다"
        );
        assert_eq!(e.thread_id, 5, "정상 필드는 유지돼야 한다");
    }

    /// **있을 수 없는 소요 시간을 거부한다.**
    ///
    /// `secs_field` 는 `f64 → i64` 포화 변환이라 `Query_time: 1e308` 이 `i64::MAX` 가
    /// 된다. 그러면 `ended − duration` 이 오버플로하고, 슬로우로그 duration 이
    /// 권위값이라 그 쓰레기가 영구히 저장된다.
    #[test]
    fn implausible_durations_are_rejected() {
        for qt in ["1e308", "18446744073709.550781", "-5.0", "999999.0"] {
            let text = format!(
                "# Time: 2026-08-19T13:48:54.659276Z\n\
                 # User@Host: u[u] @ h []  Id: 1\n\
                 # Query_time: {qt}  Lock_time: 0.0 Rows_sent: 1  Rows_examined: 1\n\
                 SET timestamp=1787147326;\n\
                 SELECT 1;\n"
            );
            let out = parse(&text, 1_000);
            assert_eq!(out.entries.len(), 0, "Query_time={qt} 를 통과시켰다");
        }
        // 정상 범위는 통과한다 (24시간 이내).
        let ok = MEASURED;
        assert_eq!(parse(ok, 1_000).entries.len(), 1);
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
