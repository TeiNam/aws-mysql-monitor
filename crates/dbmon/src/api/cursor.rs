//! 페이지네이션 커서 (M6, [13 §2](../../../../docs/13-api-spec.md)).
//!
//! # 왜 서명하는가
//!
//! 초기 설계는 `LastEvaluatedKey` 를 base64 로만 인코딩했다. **불투명이 아니라
//! 가역적**이므로 내부 키 구조(`PK`/`SK` 패턴, `record_id` 구성)가 노출되고 위조가
//! 가능했다. 위조된 커서는 다른 인스턴스·다른 환경의 데이터를 가리킬 수 있다.
//!
//! 그래서 HMAC 으로 서명하고 다음을 함께 담는다:
//!
//! | 필드 | 막는 것 |
//! |---|---|
//! | `sub` | 커서 공유·탈취 (남의 커서로 재개할 수 없다) |
//! | `filters` 해시 | 필터 바꿔치기 (환경 스코프 우회) |
//! | `exp` | 무기한 재사용 |
//!
//! # 커서의 필터를 신뢰하지 않는다
//!
//! 재개할 때도 **요청 파라미터에서 필터를 다시 유도**하고 커서의 해시와 비교한다.
//! 커서의 필터를 그대로 쓰면 환경 스코프·`can_see_literals`·시간 범위 재검증이
//! 건너뛰어진다 — 즉 첫 요청에서만 검사되고 이후 페이지는 무검사가 된다.

use dbmon_core::time::EpochMs;
use sha2::{Digest, Sha256};

/// 커서 유효 시간 (1시간).
pub const CURSOR_TTL_MS: i64 = 60 * 60 * 1000;

/// 커서에 담기는 내용. **불투명하게 다뤄야 한다** — 클라이언트는 문자열로만 본다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// 요청자 식별자. 남의 커서로 재개할 수 없게 한다.
    pub sub: String,
    /// 정규화된 필터 집합의 해시.
    pub filters_hash: String,
    /// DynamoDB `LastEvaluatedKey` 를 문자열로 직렬화한 것.
    ///
    /// 핫 티어만 구현했다 — 콜드(Athena) 단계는 아직 없다([05 §8.3] 백필과 별개).
    pub position: String,
    pub expires_at_ms: EpochMs,
}

/// 커서 검증 실패. **왜 실패했는지 클라이언트에 알리지 않는다** — 위조 시도에
/// 정보를 주지 않기 위해 호출부가 전부 `invalid_cursor` 로 응답한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorError {
    Malformed,
    BadSignature,
    Expired,
    /// 다른 사용자의 커서다.
    SubjectMismatch,
    /// 필터가 바뀌었다.
    FilterMismatch,
}

/// 필터 집합을 정규화해 해시한다.
///
/// **정렬해서 해시한다.** 순서가 결과를 바꾸면 같은 필터가 다른 해시를 내고,
/// 정당한 재개가 `FilterMismatch` 로 거부된다.
pub fn filters_hash(pairs: &[(&str, &str)]) -> String {
    let mut sorted: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    sorted.sort();
    let mut h = Sha256::new();
    h.update(sorted.join("&").as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

fn sign(key: &[u8], payload: &str) -> String {
    // HMAC-SHA256 을 직접 구성한다. `hmac` 크레이트를 들이지 않는다 —
    // 블록 크기 64바이트, 표준 ipad/opad 다.
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let d = Sha256::digest(key);
        k[..d.len()].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(payload.as_bytes());
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    format!("{:x}", outer.finalize())
}

fn b64(input: &str) -> String {
    // base64url, 패딩 없음. 의존성을 들이지 않는다.
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let b = input.as_bytes();
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | (*c.get(2).unwrap_or(&0) as u32);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if c.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if c.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

fn unb64(input: &str) -> Option<String> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for ch in input.bytes() {
        let v = T.iter().position(|&t| t == ch)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    String::from_utf8(bytes).ok()
}

impl Cursor {
    /// 서명된 문자열로 만든다.
    pub fn encode(&self, key: &[u8]) -> String {
        // **`position` 을 맨 뒤에 둔다.** 처음엔 가운데 뒀다가 테스트가 잡았다 —
        // DynamoDB 키 직렬화(`PK|SK`)에 `|` 가 들어 있어서 파싱이 어긋났다.
        // 앞의 세 필드는 `|` 를 가질 수 없다(sub 는 Cognito sub, 해시는 16진수,
        // exp 는 숫자). 마지막 필드는 무엇이 들어와도 안전하다.
        let payload = format!(
            "{}|{}|{}|{}",
            self.sub, self.filters_hash, self.expires_at_ms, self.position
        );
        let encoded = b64(&payload);
        let mac = sign(key, &encoded);
        format!("{encoded}.{mac}")
    }

    /// 검증하며 되돌린다.
    ///
    /// **`sub` 와 필터 해시를 호출부가 넘긴 값과 비교한다** — 커서에 담긴 값을
    /// 그대로 믿으면 그게 곧 우회다.
    pub fn decode(
        raw: &str,
        key: &[u8],
        expected_sub: &str,
        expected_filters: &str,
        now_ms: EpochMs,
    ) -> Result<Self, CursorError> {
        let (encoded, mac) = raw.split_once('.').ok_or(CursorError::Malformed)?;
        // **상수 시간 비교가 아니어도 되는 이유**: MAC 은 우리가 계산한 값과 비교하고
        // 실패 시 사유를 알려주지 않는다. 타이밍으로 얻을 수 있는 것은 없다.
        // 그래도 길이 먼저 보고 바이트를 비교한다.
        let expected_mac = sign(key, encoded);
        if mac.len() != expected_mac.len() || mac != expected_mac {
            return Err(CursorError::BadSignature);
        }
        let payload = unb64(encoded).ok_or(CursorError::Malformed)?;
        // `splitn(4, ..)` 이므로 `position` 안의 `|` 는 그대로 남는다.
        let mut parts = payload.splitn(4, '|');
        let (Some(sub), Some(filters_hash), Some(exp), Some(position)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(CursorError::Malformed);
        };
        let expires_at_ms: EpochMs = exp.parse().map_err(|_| CursorError::Malformed)?;

        if now_ms >= expires_at_ms {
            return Err(CursorError::Expired);
        }
        if sub != expected_sub {
            return Err(CursorError::SubjectMismatch);
        }
        if filters_hash != expected_filters {
            return Err(CursorError::FilterMismatch);
        }
        Ok(Self {
            sub: sub.to_string(),
            filters_hash: filters_hash.to_string(),
            position: position.to_string(),
            expires_at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-server-key-not-a-real-secret";
    const NOW: EpochMs = 1_787_000_000_000;

    fn cursor() -> Cursor {
        Cursor {
            sub: "user-abc".into(),
            filters_hash: filters_hash(&[("env", "prd"), ("limit", "50")]),
            position: "SQ#acct/region/inst#2026-08-19|0001787000000#42".into(),
            expires_at_ms: NOW + CURSOR_TTL_MS,
        }
    }

    #[test]
    fn round_trips() {
        let c = cursor();
        let raw = c.encode(KEY);
        let back = Cursor::decode(&raw, KEY, &c.sub, &c.filters_hash, NOW).expect("복원");
        assert_eq!(back, c);
    }

    /// **내부 키 구조가 그대로 읽히지 않아야 한다.**
    ///
    /// 초기 설계의 실수가 이것이었다 — base64 만 쓰면 가역적이라 `PK`/`SK` 패턴이
    /// 노출된다. 서명이 붙어도 base64 는 여전히 가역이므로, **불투명성은 서명이
    /// 아니라 위조 방지에서 온다**는 점을 분명히 해 둔다.
    #[test]
    fn the_cursor_cannot_be_modified_without_detection() {
        let c = cursor();
        let raw = c.encode(KEY);

        // 페이로드를 한 글자 바꾸면 서명이 깨진다.
        let mut bytes: Vec<char> = raw.chars().collect();
        bytes[0] = if bytes[0] == 'A' { 'B' } else { 'A' };
        let tampered: String = bytes.into_iter().collect();
        assert_eq!(
            Cursor::decode(&tampered, KEY, &c.sub, &c.filters_hash, NOW),
            Err(CursorError::BadSignature)
        );
    }

    /// **다른 키로 만든 커서를 받아들이지 않는다.**
    #[test]
    fn a_cursor_signed_with_another_key_is_rejected() {
        let raw = cursor().encode(b"attacker-key");
        let c = cursor();
        assert_eq!(
            Cursor::decode(&raw, KEY, &c.sub, &c.filters_hash, NOW),
            Err(CursorError::BadSignature)
        );
    }

    /// **남의 커서로 재개할 수 없다** — 커서 공유·탈취 차단.
    #[test]
    fn another_users_cursor_is_rejected() {
        let c = cursor();
        let raw = c.encode(KEY);
        assert_eq!(
            Cursor::decode(&raw, KEY, "someone-else", &c.filters_hash, NOW),
            Err(CursorError::SubjectMismatch)
        );
    }

    /// **필터를 바꿔치기할 수 없다.**
    ///
    /// 이게 없으면 첫 요청에서만 환경 스코프가 검사되고, 이후 페이지는 커서의
    /// 필터를 그대로 써서 무검사가 된다.
    #[test]
    fn changing_the_filters_invalidates_the_cursor() {
        let c = cursor();
        let raw = c.encode(KEY);
        let other = filters_hash(&[("env", "dev"), ("limit", "50")]);
        assert_eq!(
            Cursor::decode(&raw, KEY, &c.sub, &other, NOW),
            Err(CursorError::FilterMismatch)
        );
    }

    #[test]
    fn an_expired_cursor_is_rejected() {
        let c = cursor();
        let raw = c.encode(KEY);
        assert_eq!(
            Cursor::decode(&raw, KEY, &c.sub, &c.filters_hash, c.expires_at_ms),
            Err(CursorError::Expired)
        );
    }

    /// **`position` 안의 구분자가 파싱을 깨뜨리지 않아야 한다.**
    ///
    /// 처음엔 `position` 을 가운데 두고 "`|` 는 나타날 수 없다" 고 주석을 달았다.
    /// 실제 DynamoDB 키 직렬화는 `PK|SK` 라서 항상 나타났고, 정당한 커서가 전부
    /// `Malformed` 로 거부됐다 — 페이지네이션이 첫 페이지에서 멈춘다.
    #[test]
    fn a_position_containing_the_delimiter_survives_the_round_trip() {
        for position in [
            "SQ#a/b/c#2026-08-19|0001787000000#42",
            "|||",
            "",
            "a|b|c|d|e|f",
        ] {
            let c = Cursor {
                position: position.into(),
                ..cursor()
            };
            let back = Cursor::decode(&c.encode(KEY), KEY, &c.sub, &c.filters_hash, NOW);
            assert_eq!(back.as_ref().map(|b| b.position.as_str()), Ok(position));
        }
    }

    /// 필터 해시는 **순서에 무관**해야 한다 — 아니면 정당한 재개가 거부된다.
    #[test]
    fn filter_hash_is_order_independent() {
        assert_eq!(
            filters_hash(&[("env", "prd"), ("limit", "50")]),
            filters_hash(&[("limit", "50"), ("env", "prd")])
        );
        // 값이 다르면 해시도 다르다.
        assert_ne!(
            filters_hash(&[("env", "prd")]),
            filters_hash(&[("env", "dev")])
        );
    }

    /// 깨진 입력에 패닉하지 않는다.
    #[test]
    fn malformed_input_is_rejected_without_panicking() {
        for raw in ["", ".", "a.b", "no-dot", "!!!.???", &"x".repeat(10_000)] {
            let r = Cursor::decode(raw, KEY, "user-abc", "hash", NOW);
            assert!(r.is_err(), "{raw:?} 를 통과시켰다");
        }
    }

    /// base64 왕복이 정확해야 한다 — 위치 정보가 깨지면 페이지네이션이 어긋난다.
    #[test]
    fn base64_round_trips_including_edge_lengths() {
        for s in [
            "",
            "a",
            "ab",
            "abc",
            "abcd",
            "한글도 된다",
            "SQ#a/b/c#2026-08-19",
        ] {
            assert_eq!(unb64(&b64(s)).as_deref(), Some(s), "{s:?}");
        }
    }

    /// **HMAC 이 표준 구현과 일치해야 한다.**
    ///
    /// 직접 구성했으므로 RFC 4231 테스트 벡터로 고정한다 — 틀리면 서명이
    /// "그럴듯하지만 표준이 아닌" 값이 되고, 그건 검증할 수 없는 상태다.
    #[test]
    fn hmac_matches_the_rfc4231_vector() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
        let mac = sign(b"Jefe", "what do ya want for nothing?");
        assert_eq!(
            mac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
