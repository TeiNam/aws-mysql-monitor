//! 로깅·트레이싱 + **마스킹 레이어** (M0-5, T-36, NFR-S-05).
//!
//! # 로그가 리터럴 유출 경로다
//!
//! 이 시스템은 운영 SQL 을 다룬다. 로그에 SQL 을 넣으면 CloudWatch Logs 에
//! **리터럴 정책과 무관하게** 리터럴이 쌓인다. `literal_policy=masked` 인스턴스에서도
//! 그렇게 되므로 [08 §6.1](../../../docs/08-security-auth.md)의 통제가 무력해진다.
//!
//! 세 가지로 막는다.
//!
//! | 수단 | 무엇을 막는가 |
//! |---|---|
//! | [`sql_fingerprint`] | 파싱 실패·디버그 시 원본 대신 길이+해시만 남긴다 ([05 §6](../../../docs/05-collector.md)) |
//! | [`scrub`] | **에러 메시지에 섞여 들어온** SQL 을 지운다. MySQL 서버 에러는 쿼리 조각을 담는다 |
//! | 정적 검사 | `tracing` 매크로에 SQL 필드를 직접 넘기는 코드를 CI 가 거부한다 ([15 §9.1](../../../docs/15-testing.md)) |
//!
//! `scrub` 이 필요한 이유는 우리가 만들지 않은 문자열이 로그에 들어오기 때문이다.
//! `mysql_async::Error::Server` 의 메시지에는 실패한 문장의 조각이 들어 있다.

use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

/// 로그·에러에 남기는 SQL 지문. **원문을 남기지 않는다.**
///
/// 형식: `sql[len=1234 sha=9f2c1a3b]`
pub fn sql_fingerprint(sql: &str) -> String {
    let h = Sha256::digest(sql.as_bytes());
    let hex: String = h.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("sql[len={} sha={}]", sql.len(), hex)
}

/// 파싱 실패 지점을 함께 남긴다 (원문 없이).
pub fn parse_failure_fingerprint(input: &str, byte_offset: usize) -> String {
    format!("{} at byte {}", sql_fingerprint(input), byte_offset)
}

/// 임의 문자열에서 리터럴처럼 보이는 것을 제거한다.
///
/// **외부에서 온 문자열에만** 쓴다(SDK 에러 메시지 등). 우리가 만든 메시지는
/// 애초에 리터럴을 담지 않아야 한다.
///
/// 지우는 것:
/// - 인용부호로 감싼 구간 (`'...'`, `"..."`, `` `...` ``) → `'?'`
/// - 3자리 이상 숫자 → `?` (에러 코드·짧은 카운트는 남긴다)
///
/// 백틱 구간까지 지우는 이유: MySQL 에러의 `` `db`.`table` `` 은 스키마 정보이고,
/// 스키마 이름이 사업 정보를 담는 경우가 있다. 식별자가 필요하면 구조화 필드로 따로 넣는다.
pub fn scrub(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                // 닫는 같은 부호까지 소비한다. 없으면 끝까지.
                out.push_str("'?'");
                let quote = c;
                let mut escaped = false;
                for (_, n) in chars.by_ref() {
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if n == '\\' {
                        escaped = true;
                        continue;
                    }
                    if n == quote {
                        break;
                    }
                }
            }
            d if d.is_ascii_digit() => {
                let mut run = String::from(d);
                while let Some((_, n)) = chars.peek() {
                    if n.is_ascii_digit() {
                        run.push(*n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if run.len() >= 3 {
                    out.push('?');
                } else {
                    out.push_str(&run);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// 외부 에러를 로그에 넣기 전에 감싼다. `Display` 가 자동으로 마스킹한다.
///
/// ```
/// # use dbmon::telemetry::Scrubbed;
/// let e = "Duplicate entry 'kim@example.com' for key 'uk_email'";
/// assert!(!Scrubbed(e).to_string().contains("kim@example.com"));
/// ```
pub struct Scrubbed<T: std::fmt::Display>(pub T);

impl<T: std::fmt::Display> std::fmt::Display for Scrubbed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&scrub(&self.0.to_string()))
    }
}

impl<T: std::fmt::Display> std::fmt::Debug for Scrubbed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// 로깅을 초기화한다.
///
/// `json=true` 면 CloudWatch Logs Insights 가 파싱할 수 있는 JSON 한 줄씩 출력한다.
/// 로컬 개발에서는 사람이 읽는 형식이 낫다.
pub fn init(json: bool) {
    let filter = EnvFilter::try_from_env("DBMON_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` 을 쓰는 이유: 테스트에서 여러 번 호출돼도 패닉하지 않는다.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);
    if json {
        let _ = builder.json().flatten_event(true).try_init();
    } else {
        let _ = builder.try_init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_hides_content_but_is_stable() {
        let sql = "SELECT * FROM users WHERE ssn = '900101-1234567'";
        let f = sql_fingerprint(sql);
        assert!(!f.contains("900101"));
        assert!(!f.contains("SELECT"));
        assert_eq!(f, sql_fingerprint(sql), "결정론적이어야 재현이 된다");
        assert!(f.contains(&format!("len={}", sql.len())), "{f}");
    }

    #[test]
    fn different_sql_gives_different_fingerprint() {
        assert_ne!(sql_fingerprint("SELECT 1"), sql_fingerprint("SELECT 2"));
    }

    #[test]
    fn scrub_removes_quoted_literals() {
        let cases = [
            (
                "Duplicate entry 'kim@example.com' for key 'uk_email'",
                "kim@example.com",
            ),
            (
                "Unknown column \"secret_col\" in 'field list'",
                "secret_col",
            ),
            ("Table `payments`.`cards` doesn't exist", "cards"),
            (r"value 'it\'s here' rejected", "here"),
        ];
        for (input, secret) in cases {
            let s = scrub(input);
            assert!(
                !s.contains(secret),
                "입력={input:?} 결과={s:?} 에 {secret:?} 가 남았다"
            );
        }
    }

    #[test]
    fn scrub_keeps_short_numbers_but_hides_long_ones() {
        // 에러 코드(3자리 미만)는 진단에 필요하다.
        let s = scrub("error 45 at row 1234567 for id 987654321");
        assert!(s.contains("45"), "{s}");
        assert!(!s.contains("1234567"), "{s}");
        assert!(!s.contains("987654321"), "{s}");
    }

    #[test]
    fn scrub_handles_unterminated_quote_without_hanging() {
        // 닫히지 않은 인용부호에서 무한 루프·패닉이 없어야 한다.
        for bad in ["value 'oops", "`unterminated", "\"", "'", "''", "a'b'c'"] {
            let _ = scrub(bad);
        }
        assert!(!scrub("value 'oops").contains("oops"));
    }

    #[test]
    fn scrub_is_utf8_safe() {
        let s = scrub("고객 '홍길동' 오류 12345");
        assert!(s.contains("고객"));
        assert!(!s.contains("홍길동"), "{s}");
        assert!(!s.contains("12345"), "{s}");
    }

    #[test]
    fn scrubbed_wrapper_masks_on_display_and_debug() {
        let e = "Duplicate entry 'secret-value' for key 'k'";
        assert!(!format!("{}", Scrubbed(e)).contains("secret-value"));
        assert!(!format!("{:?}", Scrubbed(e)).contains("secret-value"));
    }

    #[test]
    fn parse_failure_records_offset_without_content() {
        let f = parse_failure_fingerprint("SELECT 'leak' FROM t", 8);
        assert!(f.contains("at byte 8"));
        assert!(!f.contains("leak"));
    }
}
