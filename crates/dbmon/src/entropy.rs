//! OS 엔트로피 — **구현을 한 벌로 둔다.**
//!
//! `main.rs` 가 커서 서명 키와 dev 토큰에 쓰고, [`crate::bootstrap`] 이 `plan_id` 에
//! 쓴다. 각자 `/dev/urandom` 을 열면 실패 처리가 갈릴 수 있고, 갈리면 한쪽이 조용히
//! 약한 값을 만든다.
//!
//! # 크레이트를 들이지 않는 이유
//!
//! `getrandom` 은 이 한 가지를 위해 의존성 하나를 늘린다. `/dev/urandom` 읽기는 이
//! 프로젝트가 도는 두 플랫폼(Linux 컨테이너, macOS 개발기)에서 모두 정상 동작하고,
//! 실패는 곧 시스템 고장이므로 폴백이 필요하지 않다.

use std::io::Read;

/// 버퍼를 무작위 바이트로 채운다.
pub fn fill(buf: &mut [u8]) -> std::io::Result<()> {
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf))
}

/// 128비트 무작위 16진수 문자열 (32자).
///
/// **실패를 삼키지 않는다.** 엔트로피를 못 읽으면 `None` 이고, 호출부가 약한 값을
/// 만드는 대신 요청을 거부한다 — 예측 가능한 토큰은 없는 것보다 나쁘다는 판단이
/// 이 프로젝트에 이미 있다([`crate::api::auth::mint_dev_token`]).
pub fn hex128() -> Option<String> {
    let mut buf = [0u8; 16];
    fill(&mut buf).ok()?;
    Some(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex128_is_32_hex_chars_and_differs_each_call() {
        let a = hex128().expect("엔트로피");
        let b = hex128().expect("엔트로피");
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "같은 값이 두 번 나왔다");
    }

    #[test]
    fn fill_writes_every_byte() {
        // 0 으로 시작한 버퍼가 그대로 남아 있으면 읽지 않은 것이다. 32바이트가 전부
        // 0 일 확률은 무시할 수 있다.
        let mut buf = [0u8; 32];
        fill(&mut buf).expect("엔트로피");
        assert!(buf.iter().any(|&b| b != 0));
    }
}
