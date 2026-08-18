//! SQL 토크나이저.
//!
//! 이 렉서는 **원문 SQL과 MySQL `DIGEST_TEXT` 양쪽**을 같은 규칙으로 처리한다.
//! `app_digest` 의 목표는 `DIGEST_TEXT` 를 글자 단위로 재현하는 것이 아니라
//! `normalize(원문) == normalize(DIGEST_TEXT)` 가 성립하는 것이다([05 §3.1](../../../docs/05-collector.md)).
//! 그래서 정규 형식은 우리가 정할 수 있고, "토큰을 공백 1칸으로 이어붙인다"는
//! 단순한 규칙으로 양쪽이 수렴한다.

use crate::keywords::is_reserved;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// 예약어. 대문자로 정규화한다.
    Kw(String),
    /// 식별자. 백틱을 벗기고 원본 대소문자를 유지한다.
    Ident(String),
    /// 리터럴에서 유래한 `?`. 마스킹 후조건 검증이 이 종류를 센다.
    Placeholder,
    /// 입력에 이미 있던 `?` (프리페어드 파라미터 또는 `DIGEST_TEXT`).
    Param,
    /// `DIGEST_TEXT` 의 축약 표기 `...`
    Ellipsis,
    /// 옵티마이저 힌트 `/*+ ... */` — 실행계획에 영향을 주므로 보존한다.
    Hint(String),
    /// 연산자 (`=`, `<=`, `<>`, `:=` 등)
    Op(String),
    /// 구두점 (`(`, `)`, `,`, `.`, `;`)
    Punct(char),
}

impl Tok {
    /// 정규 텍스트로 렌더링한다. 토큰 사이는 canonical 에서 공백 1칸으로 잇는다.
    pub fn render(&self) -> &str {
        match self {
            Tok::Kw(s) | Tok::Ident(s) | Tok::Hint(s) | Tok::Op(s) => s,
            Tok::Placeholder | Tok::Param => "?",
            Tok::Ellipsis => "...",
            Tok::Punct(c) => match c {
                '(' => "(",
                ')' => ")",
                ',' => ",",
                '.' => ".",
                ';' => ";",
                '*' => "*",
                _ => "?", // 도달 불가 — push_punct 가 위 집합만 만든다
            },
        }
    }
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    /// 닫히지 않은 인용부호를 만났다. 후조건 검증이 이걸 보고 실패시킨다.
    pub unterminated_quote: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            unterminated_quote: false,
        }
    }

    pub fn tokenize(mut self) -> (Vec<Tok>, bool) {
        let mut out = Vec::new();
        while let Some(t) = self.next_token() {
            out.push(t);
        }
        (out, self.unterminated_quote)
    }

    fn peek(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn next_token(&mut self) -> Option<Tok> {
        loop {
            self.skip_whitespace();
            let c = self.peek(0)?;
            match c {
                b'/' if self.peek(1) == Some(b'*') => {
                    if let Some(hint) = self.read_block_comment() {
                        return Some(Tok::Hint(hint));
                    }
                    continue; // 일반 주석은 제거
                }
                b'-' if self.peek(1) == Some(b'-') => {
                    // MySQL 은 `--` 뒤에 공백/제어문자를 요구한다. `a--b` 는 뺄셈 두 번.
                    match self.peek(2) {
                        None | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') => {
                            self.skip_line();
                            continue;
                        }
                        _ => return Some(self.read_operator()),
                    }
                }
                b'#' => {
                    self.skip_line();
                    continue;
                }
                b'\'' | b'"' => {
                    self.read_quoted(c);
                    return Some(Tok::Placeholder);
                }
                b'`' => return Some(self.read_backtick_ident()),
                b'?' => {
                    self.pos += 1;
                    return Some(Tok::Param);
                }
                b'.' => {
                    if matches!(self.peek(1), Some(b'0'..=b'9')) {
                        return Some(self.read_number());
                    }
                    if self.peek(1) == Some(b'.') && self.peek(2) == Some(b'.') {
                        self.pos += 3;
                        return Some(Tok::Ellipsis);
                    }
                    self.pos += 1;
                    return Some(Tok::Punct('.'));
                }
                b'0'..=b'9' => return Some(self.read_number()),
                b'(' | b')' | b',' | b';' => {
                    self.pos += 1;
                    return Some(Tok::Punct(c as char));
                }
                _ if is_ident_start(c) => return Some(self.read_word()),
                _ => return Some(self.read_operator()),
            }
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.peek(0),
            Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
        ) {
            self.pos += 1;
        }
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if c == b'\n' {
                break;
            }
        }
    }

    /// 블록 주석을 읽는다. 옵티마이저 힌트(`/*+ ... */`)면 원문을 반환한다.
    ///
    /// ponytail: `/*! ... */` 버전 주석은 제거한다. MySQL 은 그 안을 SQL 로 실행하므로
    /// 엄밀히는 코드지만, 애플리케이션 SQL 에서 거의 쓰이지 않는다. 골든 코퍼스에
    /// 케이스를 넣어 두었고, 실패하면 그때 내용을 토큰화하는 쪽으로 바꾼다.
    fn read_block_comment(&mut self) -> Option<String> {
        let start = self.pos;
        self.pos += 2; // "/*"
        let is_hint = self.peek(0) == Some(b'+');
        while self.pos < self.src.len() {
            if self.peek(0) == Some(b'*') && self.peek(1) == Some(b'/') {
                self.pos += 2;
                if is_hint {
                    // 힌트 내부 공백을 1칸으로 축약해 원문 차이를 흡수한다.
                    let raw = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("/*+ */");
                    return Some(collapse_ws(raw));
                }
                return None;
            }
            self.pos += 1;
        }
        // 닫히지 않은 주석: 나머지를 전부 주석으로 본다 (MySQL 도 그렇게 처리한다).
        if is_hint {
            let raw = std::str::from_utf8(&self.src[start..]).unwrap_or("/*+ */");
            return Some(collapse_ws(raw));
        }
        None
    }

    /// 문자열 리터럴을 소비한다. 백슬래시 이스케이프와 인용부호 중복(`''`) 모두 처리한다.
    fn read_quoted(&mut self, q: u8) {
        self.pos += 1;
        loop {
            match self.peek(0) {
                None => {
                    self.unterminated_quote = true;
                    return;
                }
                Some(b'\\') => self.pos += 2, // NO_BACKSLASH_ESCAPES 미가정
                Some(c) if c == q => {
                    if self.peek(1) == Some(q) {
                        self.pos += 2; // '' → 이스케이프된 인용부호
                    } else {
                        self.pos += 1;
                        return;
                    }
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn read_backtick_ident(&mut self) -> Tok {
        self.pos += 1;
        let mut name = String::new();
        loop {
            match self.peek(0) {
                None => {
                    self.unterminated_quote = true;
                    break;
                }
                Some(b'`') => {
                    if self.peek(1) == Some(b'`') {
                        name.push('`');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        break;
                    }
                }
                Some(c) => {
                    name.push(c as char);
                    self.pos += 1;
                }
            }
        }
        // 백틱으로 인용된 것은 **절대 예약어로 승격하지 않는다**.
        // 원문과 DIGEST_TEXT 의 인용 여부가 같아야 수렴하므로, 인용된 쪽은 항상 식별자다.
        // 소문자로 접는 이유는 §fold_ident 참조.
        Tok::Ident(fold_ident(&from_utf8_lossy_owned(name)))
    }

    /// 수치 리터럴. 정수·실수·과학표기·16진수·비트를 모두 흡수한다.
    fn read_number(&mut self) -> Tok {
        // 0x.. / 0b..
        if self.peek(0) == Some(b'0') {
            match self.peek(1) {
                Some(b'x' | b'X') => {
                    self.pos += 2;
                    while matches!(self.peek(0), Some(c) if c.is_ascii_hexdigit()) {
                        self.pos += 1;
                    }
                    return Tok::Placeholder;
                }
                Some(b'b' | b'B') if matches!(self.peek(2), Some(b'0' | b'1')) => {
                    self.pos += 2;
                    while matches!(self.peek(0), Some(b'0' | b'1')) {
                        self.pos += 1;
                    }
                    return Tok::Placeholder;
                }
                _ => {}
            }
        }
        while matches!(self.peek(0), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek(0) == Some(b'.') && matches!(self.peek(1), Some(b'0'..=b'9') | None) {
            self.pos += 1;
            while matches!(self.peek(0), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // 지수부. `1e10`, `1E-3`. `1efoo` 같은 식별자 경계는 되돌린다.
        if matches!(self.peek(0), Some(b'e' | b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(0), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if matches!(self.peek(0), Some(b'0'..=b'9')) {
                while matches!(self.peek(0), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            } else {
                self.pos = save;
            }
        }
        Tok::Placeholder
    }

    /// 식별자 또는 예약어. introducer 리터럴(`X'41'`, `b'101'`, `_utf8mb4'..'`, `N'..'`)도 여기서 잡는다.
    fn read_word(&mut self) -> Tok {
        let start = self.pos;
        while matches!(self.peek(0), Some(c) if is_ident_part(c)) {
            self.pos += 1;
        }
        let word = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();

        // introducer 리터럴: 단어 바로 뒤에 인용부호가 붙어 있다.
        if self.peek(0) == Some(b'\'') && is_literal_introducer(&word) {
            self.read_quoted(b'\'');
            return Tok::Placeholder;
        }

        if is_reserved(&word) {
            Tok::Kw(word.to_ascii_uppercase())
        } else {
            Tok::Ident(fold_ident(&word))
        }
    }

    fn read_operator(&mut self) -> Tok {
        const THREE: [&[u8]; 1] = [b"<=>"];
        const TWO: [&[u8]; 11] = [
            b"<=", b">=", b"<>", b"!=", b":=", b"||", b"&&", b"<<", b">>", b"->", b"=>",
        ];
        for pat in THREE {
            if self.src[self.pos..].starts_with(pat) {
                self.pos += 3;
                return Tok::Op("<=>".into());
            }
        }
        // `->>` (JSON unquote) 는 2문자 `->` 보다 먼저 본다.
        if self.src[self.pos..].starts_with(b"->>") {
            self.pos += 3;
            return Tok::Op("->>".into());
        }
        for pat in TWO {
            if self.src[self.pos..].starts_with(pat) {
                self.pos += 2;
                return Tok::Op(String::from_utf8_lossy(pat).into_owned());
            }
        }
        let c = self.src[self.pos];
        self.pos += 1;
        if c == b'*' {
            return Tok::Punct('*');
        }
        Tok::Op((c as char).to_string())
    }
}

/// 식별자를 소문자로 접는다.
///
/// **이게 수렴성의 핵심이다.** MySQL 은 `DIGEST_TEXT` 에서 식별자에 백틱을 붙이고
/// 문법 키워드는 대문자로 올린다. 그런데 `DATE`·`COUNT`·`TIMESTAMP` 같은 **비예약어**는
/// 문법 키워드로도 쓰이고 컬럼명으로도 쓰인다. 위치를 봐야 구분되므로 렉서로는 불가능하다.
///
/// | 입력 | 원문 SQL | `DIGEST_TEXT` | 접지 않으면 |
/// |---|---|---|---|
/// | 테이블 `orders` | `orders` | `` `orders` `` | 같음 |
/// | 날짜 리터럴 | `date '..'` | `DATE ?` | `date` vs `DATE` → **갈라진다** |
/// | 함수 | `count(*)` | `COUNT ( * )` | `count` vs `COUNT` → **갈라진다** |
///
/// 소문자로 접으면 세 경우 모두 수렴한다. 예약어만 대문자로 올리는데, 예약어는
/// 인용 없이 식별자가 될 수 없으므로 위치를 몰라도 판정이 확정적이다.
///
/// 대가: 대소문자만 다른 식별자(`T` 와 `t`)가 같은 다이제스트로 묶인다.
/// `lower_case_table_names=0` 인 리눅스 RDS 에서 이론상 가능하지만, 같은 스키마에
/// 대소문자만 다른 테이블을 두는 것은 실무에서 없다. 그룹핑 키의 정확도 손실이므로
/// 데이터 손실이 아니다. 표시용 텍스트는 `digest_text` 를 따로 저장하므로 영향 없다.
///
/// 초기 설계([05 §3.2](../../../docs/05-collector.md) 규칙 8)는 "식별자는 원본 대소문자
/// 유지"였다. 그 규칙으로는 위 표의 2·3행이 수렴하지 않아 M1-6 골든 코퍼스가 실패한다.
fn fold_ident(word: &str) -> String {
    word.to_lowercase()
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_ident_part(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

/// `_latin1'x'`, `N'x'`, `X'41'`, `b'101'` 형태의 introducer.
fn is_literal_introducer(word: &str) -> bool {
    if word.starts_with('_') {
        return true; // 캐릭터셋 introducer
    }
    matches!(word.to_ascii_uppercase().as_str(), "N" | "X" | "B")
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// 백틱 식별자를 바이트 단위로 모았으므로 멀티바이트를 복원한다.
fn from_utf8_lossy_owned(s: String) -> String {
    let bytes: Vec<u8> = s.chars().map(|c| c as u32 as u8).collect();
    match std::str::from_utf8(&bytes) {
        Ok(v) => v.to_string(),
        Err(_) => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        Lexer::new(s).tokenize().0
    }

    #[test]
    fn strings_and_numbers_become_placeholders() {
        let t = toks("SELECT 1, 'a', 0x1F, 1.5e-3, b'101', X'41', _utf8mb4'z'");
        assert_eq!(t.iter().filter(|t| **t == Tok::Placeholder).count(), 7);
    }

    #[test]
    fn escaped_quotes_do_not_leak() {
        // `\'` 와 `''` 를 잘못 처리하면 뒤쪽 SQL 이 문자열로 삼켜진다.
        let t = toks(r"SELECT 'it\'s', 'a''b' FROM t");
        assert_eq!(t.iter().filter(|t| **t == Tok::Placeholder).count(), 2);
        assert!(t.contains(&Tok::Ident("t".into())));
    }

    #[test]
    fn hints_preserved_comments_removed() {
        let t = toks("SELECT /*+ MAX_EXECUTION_TIME(1000) */ a /* junk */ FROM t -- tail\n");
        assert!(matches!(&t[1], Tok::Hint(h) if h.starts_with("/*+")));
        assert!(
            !t.iter()
                .any(|x| matches!(x, Tok::Hint(h) if h.contains("junk")))
        );
    }

    #[test]
    fn minus_minus_without_space_is_operator() {
        let t = toks("SELECT 1--2");
        assert_eq!(t.iter().filter(|t| **t == Tok::Placeholder).count(), 2);
    }

    #[test]
    fn backtick_identifier_never_becomes_keyword() {
        // `order` 는 예약어지만 백틱으로 인용되면 식별자다. 이게 깨지면
        // 원문과 DIGEST_TEXT 가 갈라진다.
        let t = toks("SELECT `order` FROM `select`");
        assert_eq!(t[1], Tok::Ident("order".into()));
        assert_eq!(t[3], Tok::Ident("select".into()));
    }

    #[test]
    fn non_reserved_word_stays_identifier() {
        // `status` 는 비예약어라 인용 없이 컬럼명이 될 수 있다.
        // 예약어 집합에 넣으면 원문(`status`)과 DIGEST_TEXT(`` `status` ``)가 갈라진다.
        assert_eq!(toks("SELECT status FROM t")[1], Tok::Ident("status".into()));
    }

    #[test]
    fn identifiers_fold_to_lowercase() {
        // 수렴성 핵심. 원문의 `Orders`/`ORDERS`/`orders` 와 DIGEST_TEXT 의 `` `Orders` ``
        // 가 같은 토큰이 되어야 한다.
        for src in [
            "SELECT a FROM Orders",
            "SELECT a FROM ORDERS",
            "SELECT a FROM `Orders`",
        ] {
            assert_eq!(toks(src)[3], Tok::Ident("orders".into()), "src={src}");
        }
    }

    #[test]
    fn non_reserved_keyword_converges_across_case() {
        // 원문 `date '..'` vs DIGEST_TEXT `DATE ?` — 소문자 접기가 없으면 갈라진다.
        assert_eq!(
            toks("SELECT date '2026-01-01'")[1],
            toks("SELECT DATE ?")[1]
        );
        assert_eq!(
            toks("SELECT count(*) FROM t")[1],
            toks("SELECT COUNT ( * ) FROM `t`")[1]
        );
    }

    #[test]
    fn ellipsis_is_a_token() {
        assert!(toks("SELECT a FROM t WHERE id IN (...)").contains(&Tok::Ellipsis));
    }

    #[test]
    fn unterminated_quote_is_reported() {
        assert!(Lexer::new("SELECT 'abc").tokenize().1);
        assert!(!Lexer::new("SELECT 'abc'").tokenize().1);
    }
}
