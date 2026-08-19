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
/// - **첫 인용부호부터 마지막 인용부호까지 통째로** → `'?'`
/// - 3자리 이상 숫자 → `?` (에러 코드·짧은 카운트는 남긴다)
///
/// # 짝맞추기로는 안 된다
///
/// 이전 구현은 여는 부호를 만나면 같은 부호까지 소비하는 방식이었다. MySQL 에러는
/// `... near '<조각>' at line N` 형태이고 **그 조각 안에 또 인용부호가 있으므로**
/// 짝이 한 칸씩 밀린다 — 닫는 부호와 다음 여는 부호 사이의 내용이 평문으로 새어 나온다:
///
/// ```text
/// IN : near 'WHERE email = 'kim@example.com' AND ssn = '900101-1234567'' at line 1
/// OUT: near '?'kim@example.com'?'?-?'?' at line 1
///                ^^^^^^^^^^^^^^^ 유출
/// ```
///
/// 어느 부호가 여는 것인지는 문자열만 보고 판정할 수 없다. 그래서 판정을 포기하고
/// **범위 전체를 버린다.** `Unknown column '?'` 처럼 뒷부분 진단이 조금 손실되지만,
/// 이건 보안 통제다 — 진단은 `sql_fingerprint` 와 구조화 필드가 담당한다.
///
/// 백틱까지 지우는 이유: MySQL 에러의 `` `db`.`table` `` 은 스키마 정보이고,
/// 스키마 이름이 사업 정보를 담는 경우가 있다.
pub fn scrub(input: &str) -> String {
    // 인용부호가 하나라도 있으면 첫 것부터 마지막 것까지 전부 버린다.
    // 단 **영문 축약형의 어포스트로피는 인용부호가 아니다** (아래 참조).
    let find_q = |from: usize| next_quote(input, from, false);
    let rfind_q = || next_quote(input, input.len(), true);
    let (head, tail) = match (find_q(0), rfind_q()) {
        // 여는 부호와 닫는 부호가 따로 있다 → 그 사이를 버리고 양쪽을 남긴다.
        (Some(first), Some(last)) if last > first => (&input[..first], &input[last + 1..]),
        // 인용부호가 **하나뿐**이다 → 닫히지 않았으므로 뒤를 전부 버린다.
        // (`value 'oops` 에서 `oops` 가 남으면 안 된다.)
        (Some(first), Some(_)) => (&input[..first], ""),
        // 인용부호가 없으면 숫자만 처리한다.
        _ => (input, ""),
    };

    let mut out = mask_digit_runs(head);
    if head.len() + tail.len() != input.len() {
        out.push_str("'?'");
    }
    out.push_str(&mask_digit_runs(tail));
    out
}

/// 인용부호를 찾는다. **영문 축약형(`Can't`, `doesn't`, `isn't`)의 어포스트로피는 건너뛴다.**
///
/// MySQL 에러의 절반 이상이 축약형으로 시작한다:
///
/// ```text
/// Can't connect to MySQL server on 'db.internal' (111 "Connection refused")
/// Table 'shop.t' doesn't exist
/// ```
///
/// 축약형의 `'` 를 여는 인용부호로 보면 그 뒤 마지막 인용부호까지 전부 버려져
/// **메시지가 `Can'?'` 하나로 붕괴한다.** 운영자가 "연결 거부"와 "접근 거부"를
/// 구분할 수 없게 되는데, 그건 이 도구의 자가진단 전체를 무력화한다.
///
/// 판정 규칙: `'` 의 **양옆이 모두 ASCII 알파벳**이면 축약형이다. 인용부호는
/// 값을 감싸므로 최소 한쪽이 공백·괄호·문장 끝이다. `'s`·`'t`·`'re`·`'ll` 를
/// 열거하지 않는 이유는 새 형태가 나올 때 다시 벌어지기 때문이다.
fn next_quote(input: &str, from: usize, backward: bool) -> Option<usize> {
    const QUOTES: [char; 3] = ['\'', '"', '`'];
    let bytes = input.as_bytes();
    let is_alpha = |i: usize| bytes.get(i).is_some_and(|b| b.is_ascii_alphabetic());
    let is_contraction =
        |i: usize| bytes[i] == b'\'' && i > 0 && is_alpha(i - 1) && is_alpha(i + 1);

    let indices: Box<dyn Iterator<Item = usize>> = if backward {
        Box::new((0..from).rev())
    } else {
        Box::new(from..input.len())
    };
    for i in indices {
        if !input.is_char_boundary(i) {
            continue;
        }
        let c = input[i..].chars().next()?;
        if QUOTES.contains(&c) && !is_contraction(i) {
            return Some(i);
        }
    }
    None
}

/// 3자리 이상 연속 숫자를 `?` 로. 에러 코드(`1064`)는 4자리라 지워지지만, 그건
/// 구조화 필드로 따로 넣는다 — 숫자 길이로 값을 역추정할 여지를 남기지 않는다.
fn mask_digit_runs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if !c.is_ascii_digit() {
            out.push(c);
            continue;
        }
        let mut run = String::from(c);
        while chars.peek().is_some_and(char::is_ascii_digit) {
            run.push(chars.next().expect("peek 가 확인했다"));
        }
        if run.len() >= 3 {
            out.push('?');
        } else {
            out.push_str(&run);
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

    /// **영문 축약형의 어포스트로피를 인용부호로 보면 메시지가 붕괴한다.**
    ///
    /// MySQL 에러의 절반 이상이 `Can't`·`doesn't` 로 시작한다. 축약형을 여는 인용부호로
    /// 보면 그 뒤 전부가 버려져 `Can'?'` 하나만 남고, 운영자가 "연결 거부" 와
    /// "접근 거부" 를 구분할 수 없게 된다 — 자가진단이 무력화된다.
    #[test]
    fn contractions_do_not_swallow_the_diagnostic() {
        let cases = [
            (
                "Can't connect to MySQL server on 'db.internal' (111 \"refused\")",
                "Can't connect to MySQL server on",
                "db.internal",
            ),
            (
                "Table 'shop.nosuchtable' doesn't exist",
                "doesn't exist",
                "nosuchtable",
            ),
            (
                "Can't find FULLTEXT index matching the column list",
                "FULLTEXT index matching",
                "\u{0}", // 유출 후보 없음 — 원문이 그대로 남아야 한다
            ),
        ];
        for (input, must_keep, must_drop) in cases {
            let out = scrub(input);
            assert!(
                out.contains(must_keep),
                "진단이 사라졌다\n  입력: {input}\n  출력: {out}"
            );
            if must_drop != "\u{0}" {
                assert!(
                    !out.contains(must_drop),
                    "값이 유출됐다: {must_drop}\n  출력: {out}"
                );
            }
        }
    }

    /// 축약형 예외가 **실제 인용 구간을 열어주지는 않아야** 한다.
    #[test]
    fn contraction_exception_does_not_open_a_leak() {
        // `'kim` 은 앞이 공백이므로 축약형이 아니다 → 인용부호로 본다.
        let out = scrub("user isn't allowed: 'kim@example.com' rejected");
        assert!(!out.contains("kim@example.com"), "유출: {out}");
        assert!(out.contains("isn't allowed"), "진단 손실: {out}");
    }

    /// **인용부호 짝맞추기로는 안 된다.** MySQL 에러는 `near '<조각>'` 형태이고
    /// 그 조각 안에 또 인용부호가 있으므로 짝이 한 칸씩 밀려 내용이 새어 나왔다.
    #[test]
    fn nested_quotes_do_not_leak_interior_content() {
        let msg = "You have an error in your SQL syntax near \
                   'WHERE email = 'kim@example.com' AND ssn = '900101-1234567'' at line 1";
        let out = scrub(msg);
        assert!(!out.contains("kim@example.com"), "이메일이 유출됐다: {out}");
        assert!(!out.contains("900101"), "주민번호가 유출됐다: {out}");
        assert!(!out.contains('@'), "인용 구간의 흔적이 남았다: {out}");
        // 앞뒤의 구조는 남아야 진단이 가능하다.
        assert!(out.contains("SQL syntax near"), "문맥이 사라졌다: {out}");
        assert!(out.contains("at line"), "꼬리가 사라졌다: {out}");
    }

    /// 짝이 없는 어포스트로피 하나로 메시지 전체가 사라지지 않아야 한다.
    #[test]
    fn unpaired_apostrophe_keeps_surrounding_text() {
        let out = scrub("Table `shop`.`orders` doesn't exist");
        assert!(!out.contains("shop"), "스키마 이름이 남았다: {out}");
        assert!(out.starts_with("Table "), "머리가 사라졌다: {out}");
        assert!(out.ends_with(" exist"), "꼬리가 사라졌다: {out}");
    }

    /// 인용부호가 없으면 숫자만 처리한다.
    #[test]
    fn digit_runs_masked_without_quotes() {
        assert_eq!(
            scrub("Lock wait timeout exceeded 50 tries 1205"),
            "Lock wait timeout exceeded 50 tries ?"
        );
    }

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
