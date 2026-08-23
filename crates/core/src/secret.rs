//! `Secret<T>` — 로깅할 수 없는 값 (M0-6, NFR-S-05).
//!
//! # 왜 타입으로 강제하는가
//!
//! "비밀을 로그에 남기지 않는다"를 규율로만 지키면 반드시 새어나간다.
//! 구조체 하나에 `#[derive(Debug)]` 를 붙이는 순간 그 안의 비밀번호가 로그에 찍힌다.
//!
//! `Secret<T>` 는 `Debug` 를 **가려진 형태로만** 구현하고 `Display` · `Serialize` 를
//! 구현하지 않는다. 그래서 다음이 **컴파일되지 않는다**:
//!
//! ```compile_fail
//! # use dbmon_core::secret::Secret;
//! let pw = Secret::new("hunter2".to_string());
//! println!("{pw}");                      // Display 없음
//! serde_json::to_string(&pw).unwrap();   // Serialize 없음
//! ```
//!
//! 값을 꺼내려면 `expose()` 를 명시적으로 호출해야 한다 — 코드 리뷰에서 눈에 띈다.

use std::fmt;
use zeroize::Zeroize;

/// 노출을 명시적으로만 허용하는 래퍼. `Drop` 시 메모리를 0으로 덮는다.
pub struct Secret<T: Zeroize> {
    inner: T,
}

impl<T: Zeroize> Secret<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// 값을 노출한다. **호출 지점이 감사 대상이다.**
    pub fn expose(&self) -> &T {
        &self.inner
    }

    /// 소유권을 넘겨 값을 꺼낸다. 꺼낸 값은 더 이상 보호되지 않는다.
    pub fn into_exposed(mut self) -> T
    where
        T: Default,
    {
        std::mem::take(&mut self.inner)
    }
}

/// `Debug` 는 **내용을 절대 출력하지 않는다.** 길이도 노출하지 않는다
/// (짧은 비밀번호라는 사실 자체가 정보다).
impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

impl<T: Zeroize + Clone> Clone for Secret<T> {
    fn clone(&self) -> Self {
        Self::new(self.inner.clone())
    }
}

impl<T: Zeroize> From<T> for Secret<T> {
    fn from(v: T) -> Self {
        Self::new(v)
    }
}

/// 비밀번호·토큰에 쓰는 별칭.
pub type SecretString = Secret<String>;

/// 만료 시각이 있는 비밀 (IAM DB Auth 토큰은 15분).
#[derive(Debug)]
pub struct ExpiringSecret {
    secret: SecretString,
    pub expires_at_ms: crate::time::EpochMs,
}

impl ExpiringSecret {
    pub fn new(secret: SecretString, expires_at_ms: crate::time::EpochMs) -> Self {
        Self {
            secret,
            expires_at_ms,
        }
    }

    pub fn expose(&self) -> &str {
        self.secret.expose()
    }

    /// 만료 `margin_ms` 전이면 갱신해야 한다.
    ///
    /// IAM 토큰은 **연결 수립 시점**에만 쓰이므로 기존 연결은 만료돼도 유지된다.
    /// 갱신이 필요한 건 새 연결을 만들 때뿐이다 ([05 §5](../../../.claude/docs/05-collector.md)).
    pub fn needs_refresh(&self, now_ms: crate::time::EpochMs, margin_ms: i64) -> bool {
        now_ms + margin_ms >= self.expires_at_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_content_or_length() {
        let s = Secret::new("hunter2-very-long-password".to_string());
        let rendered = format!("{s:?}");
        assert_eq!(rendered, "Secret(<redacted>)");
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("26"), "길이도 정보다");
    }

    #[test]
    fn debug_of_containing_struct_is_also_safe() {
        // 실제 유출 경로는 이쪽이다: 비밀을 품은 구조체에 derive(Debug).
        #[derive(Debug)]
        #[allow(dead_code)] // Debug 출력만 검증한다
        struct Conn {
            host: String,
            password: SecretString,
        }
        let c = Conn {
            host: "db.example".into(),
            password: Secret::new("s3cr3t".into()),
        };
        let out = format!("{c:?}");
        assert!(out.contains("db.example"));
        assert!(!out.contains("s3cr3t"), "구조체 Debug 로 유출됐다: {out}");
    }

    #[test]
    fn expose_returns_value() {
        let s = Secret::new("abc".to_string());
        assert_eq!(s.expose(), "abc");
        assert_eq!(s.into_exposed(), "abc");
    }

    #[test]
    fn expiring_secret_refresh_window() {
        let t = ExpiringSecret::new(Secret::new("tok".into()), 15 * 60_000);
        assert!(!t.needs_refresh(0, 5 * 60_000));
        assert!(
            t.needs_refresh(10 * 60_000, 5 * 60_000),
            "만료 5분 전이면 갱신"
        );
        assert!(t.needs_refresh(20 * 60_000, 0), "이미 만료");
        assert!(!format!("{t:?}").contains("tok"));
    }
}
