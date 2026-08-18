//! 식별자 타입 ([04 §1](../../../docs/04-data-model.md)).
//!
//! # M0-2a 결정: `instance_id` 에 **계정 ID 를 포함한다** (2026-08-19)
//!
//! ```text
//! instance_id = <account_id>/<region>/<db_instance_identifier>
//!               123456789012/ap-northeast-2/orders-prd-01
//! ```
//!
//! [OPEN-Q-20](../../../docs/OPEN-QUESTIONS.md) 의 권고를 채택했다. 근거:
//!
//! - 이 값은 **모든 파티션 키에 들어간다**(`SQ#`, `DR#`, `EV#`, `MR#`, `UR#`, `HG#`, `SNAP#`).
//!   나중에 바꾸려면 1차 키 포맷 변경 + 전 데이터 마이그레이션이다.
//! - 지금 넣는 비용은 "키가 13자 길어진다"뿐이다(항목당 13B × 96M ≈ 1.2GB, 전체의 2%).
//! - 서로 다른 계정에 같은 리전·같은 식별자가 존재하는 것은 흔하다(`prod-db-01`).
//!   포함하지 않으면 두 계정 데이터가 한 파티션에 섞인다.
//! - **개발계 계정에 프로덕션 워크로드가 함께 있다**(T-37). 계정이 키에 있으면
//!   잘못 들어온 데이터를 키로 식별·격리할 수 있다.
//!
//! 단일 계정 운영에서는 계정 ID 가 고정값이므로 실질 변화가 없다.
//!
//! # 구분자 규약
//!
//! | 구분자 | 쓰이는 곳 | 값에 들어갈 수 없음이 보장되는 이유 |
//! |---|---|---|
//! | `/` | `instance_id` 내부 | RDS 식별자는 영숫자·하이픈만 허용. 리전도 동일 |
//! | `:` | `record_id` 내부 | 위와 같음 |
//! | `#` | DynamoDB PK/SK 접두 | 위와 같음 |
//!
//! 보장에 의존하지 않고 **경계에서 검증**한다. AWS API 응답도 외부 데이터다.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    Format {
        what: &'static str,
        got: String,
    },
    Account(String),
    Region(String),
    Identifier(String),
    /// 키 구분자가 값에 들어 있다. 들어오면 파티션이 오염되므로 거부한다.
    ForbiddenChar {
        what: &'static str,
        ch: char,
    },
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format { what, got } => write!(f, "{what} 형식이 아니다: {got:?}"),
            Self::Account(v) => write!(f, "AWS 계정 ID 는 12자리 숫자여야 한다: {v:?}"),
            Self::Region(v) => write!(f, "리전 형식이 아니다: {v:?}"),
            Self::Identifier(v) => write!(f, "RDS 식별자 형식이 아니다: {v:?}"),
            Self::ForbiddenChar { what, ch } => write!(f, "{what} 에 금지 문자 {ch:?} 가 있다"),
        }
    }
}
impl std::error::Error for IdError {}

/// 키에 들어갈 수 없는 문자. DynamoDB 키 구성과 충돌한다.
const FORBIDDEN: [char; 4] = ['/', ':', '#', '\u{0}'];

fn reject_forbidden(what: &'static str, s: &str) -> Result<(), IdError> {
    if let Some(ch) = s.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(IdError::ForbiddenChar { what, ch });
    }
    Ok(())
}

/// `<account>/<region>/<identifier>`
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstanceId {
    raw: String,
    /// `raw` 안에서 리전이 시작하는 바이트 위치.
    region_at: usize,
    /// `raw` 안에서 식별자가 시작하는 바이트 위치.
    ident_at: usize,
}

impl InstanceId {
    pub fn new(account: &str, region: &str, identifier: &str) -> Result<Self, IdError> {
        validate_account(account)?;
        validate_region(region)?;
        validate_identifier(identifier)?;
        let region_at = account.len() + 1;
        let ident_at = region_at + region.len() + 1;
        Ok(Self {
            raw: format!("{account}/{region}/{identifier}"),
            region_at,
            ident_at,
        })
    }

    pub fn parse(s: &str) -> Result<Self, IdError> {
        let mut parts = s.split('/');
        let (Some(a), Some(r), Some(i), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(IdError::Format {
                what: "instance_id",
                got: s.to_string(),
            });
        };
        Self::new(a, r, i)
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
    pub fn account(&self) -> &str {
        &self.raw[..self.region_at - 1]
    }
    pub fn region(&self) -> &str {
        &self.raw[self.region_at..self.ident_at - 1]
    }
    pub fn identifier(&self) -> &str {
        &self.raw[self.ident_at..]
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}
impl TryFrom<String> for InstanceId {
    type Error = IdError;
    fn try_from(v: String) -> Result<Self, Self::Error> {
        Self::parse(&v)
    }
}
impl From<InstanceId> for String {
    fn from(v: InstanceId) -> Self {
        v.raw
    }
}

/// `<account>/<region>/<db_cluster_identifier>` — Aurora 클러스터.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClusterId(String);

impl ClusterId {
    pub fn new(account: &str, region: &str, identifier: &str) -> Result<Self, IdError> {
        validate_account(account)?;
        validate_region(region)?;
        validate_identifier(identifier)?;
        Ok(Self(format!("{account}/{region}/{identifier}")))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ClusterId {
    type Error = IdError;
    fn try_from(v: String) -> Result<Self, Self::Error> {
        let mut p = v.split('/');
        match (p.next(), p.next(), p.next(), p.next()) {
            (Some(a), Some(r), Some(i), None) => Self::new(a, r, i),
            _ => Err(IdError::Format {
                what: "cluster_id",
                got: v,
            }),
        }
    }
}
impl From<ClusterId> for String {
    fn from(v: ClusterId) -> Self {
        v.0
    }
}
impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `<instance_id>:<thread_id>:<started_at_sec>` ([04 §1.2](../../../docs/04-data-model.md)).
///
/// **시각 성분은 초 단위로 floor 한다.** 밀리초를 넣으면 같은 실행이 소스마다 다른
/// `record_id` 를 갖게 되어 (a) 3소스 병합이 키로 성립하지 않고, (b) 아카이브
/// `MERGE INTO` 가 중복 행을 만든다.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RecordId(String);

impl RecordId {
    pub fn new(instance: &InstanceId, thread_id: u64, started_at_ms: i64) -> Self {
        let sec = started_at_ms.div_euclid(1000);
        Self(format!("{}:{thread_id}:{sec}", instance.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `(instance_id, thread_id, started_at_sec)` 로 되돌린다.
    /// [04 §7](../../../docs/04-data-model.md) 의 "키 인코딩 왕복" 검증 항목.
    pub fn parts(&self) -> Result<(InstanceId, u64, i64), IdError> {
        let mut it = self.0.rsplitn(3, ':');
        let (Some(sec), Some(thread), Some(inst)) = (it.next(), it.next(), it.next()) else {
            return Err(IdError::Format {
                what: "record_id",
                got: self.0.clone(),
            });
        };
        let thread_id = thread.parse().map_err(|_| IdError::Format {
            what: "record_id.thread_id",
            got: thread.to_string(),
        })?;
        let started_at_sec = sec.parse().map_err(|_| IdError::Format {
            what: "record_id.started_at_sec",
            got: sec.to_string(),
        })?;
        Ok((InstanceId::parse(inst)?, thread_id, started_at_sec))
    }
}

impl TryFrom<String> for RecordId {
    type Error = IdError;
    fn try_from(v: String) -> Result<Self, Self::Error> {
        let id = Self(v);
        id.parts()?; // 형식 검증
        Ok(id)
    }
}
impl From<RecordId> for String {
    fn from(v: RecordId) -> Self {
        v.0
    }
}
impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 실행시간 구간. `GSI2PK` 에 들어가 "느린 것만" 조회의 스캔량을 줄인다
/// ([04 §2.3](../../../docs/04-data-model.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurBucket {
    /// 2~5초 (2초 미만도 여기에 넣는다 — 임계값 설정이 낮은 인스턴스 대비)
    B0,
    /// 5~15초
    B1,
    /// 15~60초
    B2,
    /// 60초 이상
    B3,
}

impl DurBucket {
    /// 경계는 **하한 포함**이다. 정확히 5000ms 는 `B1`.
    pub fn from_duration_ms(ms: i64) -> Self {
        match ms {
            i64::MIN..5_000 => Self::B0,
            5_000..15_000 => Self::B1,
            15_000..60_000 => Self::B2,
            _ => Self::B3,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::B0 => "b0",
            Self::B1 => "b1",
            Self::B2 => "b2",
            Self::B3 => "b3",
        }
    }
    pub const ALL: [DurBucket; 4] = [Self::B0, Self::B1, Self::B2, Self::B3];
}

impl fmt::Display for DurBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── 검증 ─────────────────────────────────────────────────────────────────────

fn validate_account(v: &str) -> Result<(), IdError> {
    if v.len() == 12 && v.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(IdError::Account(v.to_string()))
    }
}

fn validate_region(v: &str) -> Result<(), IdError> {
    reject_forbidden("region", v)?;
    let ok = (5..=32).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && v.starts_with(|c: char| c.is_ascii_lowercase());
    if ok {
        Ok(())
    } else {
        Err(IdError::Region(v.to_string()))
    }
}

/// RDS DB 인스턴스 식별자 규칙: 1~63자, 영문자로 시작, 영숫자와 하이픈, 하이픈 연속 금지,
/// 하이픈으로 끝나지 않음. 대소문자를 구분하지 않으므로 소문자로 정규화한다.
fn validate_identifier(v: &str) -> Result<(), IdError> {
    reject_forbidden("identifier", v)?;
    let bad = v.is_empty()
        || v.len() > 63
        || !v.starts_with(|c: char| c.is_ascii_alphabetic())
        || v.ends_with('-')
        || v.contains("--")
        || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if bad {
        Err(IdError::Identifier(v.to_string()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCT: &str = "123456789012";

    #[test]
    fn instance_id_roundtrips_with_account() {
        let id = InstanceId::new(ACCT, "ap-northeast-2", "orders-prd-01").unwrap();
        assert_eq!(id.as_str(), "123456789012/ap-northeast-2/orders-prd-01");
        assert_eq!(id.account(), ACCT);
        assert_eq!(id.region(), "ap-northeast-2");
        assert_eq!(id.identifier(), "orders-prd-01");
        assert_eq!(InstanceId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn same_identifier_in_two_accounts_does_not_collide() {
        // 계정을 키에서 뺐을 때 실제로 일어나는 사고 (OPEN-Q-20).
        let a = InstanceId::new("111111111111", "ap-northeast-2", "prod-db-01").unwrap();
        let b = InstanceId::new("222222222222", "ap-northeast-2", "prod-db-01").unwrap();
        assert_ne!(a, b);
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn rejects_bad_components() {
        assert!(matches!(
            InstanceId::new("123", "ap-northeast-2", "a"),
            Err(IdError::Account(_))
        ));
        assert!(matches!(
            InstanceId::new(ACCT, "AP-NE-2", "a"),
            Err(IdError::Region(_))
        ));
        for bad in ["", "1abc", "a--b", "a-", "a_b", &"a".repeat(64)] {
            assert!(
                InstanceId::new(ACCT, "ap-northeast-2", bad).is_err(),
                "{bad:?} 는 거부돼야 한다"
            );
        }
    }

    #[test]
    fn rejects_key_separators_in_values() {
        // 이게 통과하면 DynamoDB 파티션이 오염된다.
        for bad in ["a#b", "a/b", "a:b"] {
            assert!(
                InstanceId::new(ACCT, "ap-northeast-2", bad).is_err(),
                "{bad:?}"
            );
        }
        assert!(InstanceId::parse("123456789012/ap-northeast-2/a/b").is_err());
        assert!(InstanceId::parse("123456789012/ap-northeast-2").is_err());
    }

    #[test]
    fn record_id_floors_to_second_and_roundtrips() {
        let inst = InstanceId::new(ACCT, "ap-northeast-2", "orders-prd-01").unwrap();
        // 같은 실행을 세 소스가 서로 다른 밀리초로 추정해도 record_id 는 같아야 한다.
        let a = RecordId::new(&inst, 8842119, 1_755_500_400_000);
        let b = RecordId::new(&inst, 8842119, 1_755_500_400_999);
        let c = RecordId::new(&inst, 8842119, 1_755_500_400_001);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(
            a.as_str(),
            "123456789012/ap-northeast-2/orders-prd-01:8842119:1755500400"
        );

        let (i2, t2, s2) = a.parts().unwrap();
        assert_eq!(i2, inst);
        assert_eq!(t2, 8842119);
        assert_eq!(s2, 1_755_500_400);
    }

    #[test]
    fn record_id_handles_pre_epoch_without_panic() {
        // div_euclid 를 써야 음수 시각에서 0 쪽으로 반올림되지 않는다.
        let inst = InstanceId::new(ACCT, "ap-northeast-2", "t").unwrap();
        let r = RecordId::new(&inst, 1, -1500);
        assert_eq!(r.parts().unwrap().2, -2);
    }

    #[test]
    fn dur_bucket_boundaries() {
        use DurBucket::*;
        let cases = [
            (0, B0),
            (1_999, B0),
            (2_000, B0),
            (4_999, B0),
            (5_000, B1),
            (14_999, B1),
            (15_000, B2),
            (59_999, B2),
            (60_000, B3),
            (3_600_000, B3),
        ];
        for (ms, want) in cases {
            assert_eq!(DurBucket::from_duration_ms(ms), want, "{ms}ms");
        }
    }

    #[test]
    fn serde_uses_string_form() {
        let id = InstanceId::new(ACCT, "ap-northeast-2", "orders-prd-01").unwrap();
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(j, "\"123456789012/ap-northeast-2/orders-prd-01\"");
        assert_eq!(serde_json::from_str::<InstanceId>(&j).unwrap(), id);
        // 형식이 틀린 값은 역직렬화 단계에서 막혀야 한다 (경계 검증).
        assert!(serde_json::from_str::<InstanceId>("\"bogus\"").is_err());
    }
}
