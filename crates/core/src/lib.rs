//! `dbmon-core` — 도메인 타입과 포트.
//!
//! # 규칙
//!
//! **이 크레이트는 I/O 를 하지 않는다.** AWS SDK · MySQL 드라이버 · HTTP 서버에 대한
//! 의존성이 없고, 앞으로도 없어야 한다. `dbmon::aws` / `dbmon::mysql` 이 여기의 trait 를
//! 구현하고, 조립은 `main.rs` 에서 1회 한다
//! ([02 §3](../../../.claude/docs/02-architecture.md)).
//!
//! → 테스트에서 AWS·MySQL 없이 도메인 로직을 전부 검증할 수 있다.
//! 그게 SSO 만료와 무관하게 개발을 이어갈 수 있는 근거이기도 하다.
//!
//! # 여기에 있는 것
//!
//! | 모듈 | 내용 |
//! |---|---|
//! | [`ids`] | `InstanceId` · `RecordId` · `DurBucket`. **M0-2a 키 포맷 결정** |
//! | [`time`] | `HourBucket` · `DatePart` · `TimeRange` · `Clock` |
//! | [`env`] | 환경 분류와 오버라이드 |
//! | [`instance`] | 인스턴스 레지스트리, **버전 게이팅** |
//! | [`slow_query`] | 슬로우 쿼리 레코드, 리터럴 정책 |
//! | [`merge`] | 3소스 대칭 병합 (F5) |
//! | [`digest`] | 다이제스트 델타, 리셋 감지 |
//! | [`inflight`] | in-flight 상태 머신, 스레드 재사용 방어 (R5) |
//! | [`clock_offset`] | 시계 오프셋 추정 (F14) |
//! | [`rollup`] | 상위 N + `_other` 총량 보존 (F6, R9) |
//! | [`pause`] | 수집 정지 스코프 (전체 / 환경 / 인스턴스) |
//! | [`cw_metrics`] | CloudWatch 메트릭 카탈로그·period 규칙 (엔진별 분리) |
//! | [`rbac`] | 토큰 ∩ 서버 레코드 인가 (T-20) |
//! | [`secret`] | 로깅할 수 없는 값 |
//! | [`error`] | 에러 체계, AWS 오류 분류 (F25) |
//! | [`ports`] | trait 정의 |
//! | `fakes` | 테스트용 인메모리 구현 = **두 번째 구현** |

pub mod bootstrap;
pub mod clock_offset;
pub mod cw_metrics;
pub mod digest;
pub mod env;
pub mod error;
// **프로덕션 빌드에서는 컴파일되지 않는다.** 이유는 Cargo.toml 의 `testing` 피처 설명에 있다.
#[cfg(any(test, feature = "testing"))]
pub mod fakes;
pub mod ident;
pub mod ids;
pub mod inflight;
pub mod instance;
pub mod merge;
pub mod pause;
pub mod ports;
pub mod rbac;
pub mod rollup;
pub mod secret;
pub mod settings;
pub mod slow_query;
pub mod time;
pub mod tuning;

pub use clock_offset::{ClockOffset, OffsetSeverity};
pub use error::{DomainError, Result};
pub use ids::{ClusterId, DurBucket, InstanceId, RecordId};
pub use inflight::{InFlightTracker, Observation, Tracked};
pub use secret::{Secret, SecretString};
pub use time::{Clock, DatePart, EpochMs, HourBucket, SystemClock, TimeRange};

/// DynamoDB 항목 스키마 버전. 읽기 시 하위호환 처리의 기준
/// ([04 §0](../../../.claude/docs/04-data-model.md)).
pub const SCHEMA_VERSION: u32 = 1;

/// 핫 티어 보관 경계. 이보다 오래된 조회는 아카이브로 라우팅된다.
pub const HOT_TIER_DAYS: i64 = 31;

/// DynamoDB TTL. 핫 경계보다 4일 길다 — 아카이브 잡 실패에 여유를 준다.
pub const HOT_TTL_DAYS: i64 = 35;

/// 조회를 어디로 보낼지 ([04 §5](../../../.claude/docs/04-data-model.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRoute {
    /// 전 구간이 핫 티어 안.
    HotOnly,
    /// 전 구간이 핫 경계보다 과거.
    ColdOnly,
    /// 경계를 걸친다 → 둘 다 조회하고 `record_id` 로 dedup 병합한다.
    Both,
}

/// 조회 라우팅 결정. `hot_boundary_ms` 는 `now - 31일`(환경별 설정 가능, FR-STO-11).
pub fn route_query(range: TimeRange, hot_boundary_ms: EpochMs) -> QueryRoute {
    if range.from_ms() >= hot_boundary_ms {
        QueryRoute::HotOnly
    } else if range.to_ms() < hot_boundary_ms {
        QueryRoute::ColdOnly
    } else {
        QueryRoute::Both
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    #[test]
    fn routing_boundaries() {
        let now = 1_787_203_391_500; // 2026-08-20T05:23:11.5Z
        let boundary = now - HOT_TIER_DAYS * DAY;

        let hot = TimeRange::new(now - DAY, now).unwrap();
        assert_eq!(route_query(hot, boundary), QueryRoute::HotOnly);

        let cold = TimeRange::new(now - 90 * DAY, now - 40 * DAY).unwrap();
        assert_eq!(route_query(cold, boundary), QueryRoute::ColdOnly);

        let crossing = TimeRange::new(now - 60 * DAY, now).unwrap();
        assert_eq!(route_query(crossing, boundary), QueryRoute::Both);

        // 경계에 정확히 붙는 경우 — 핫에서 처리한다(더 최신 상태를 가진 쪽).
        let exact = TimeRange::new(boundary, now).unwrap();
        assert_eq!(route_query(exact, boundary), QueryRoute::HotOnly);
    }

    /// 아카이브 잡이 며칠 실패해도 데이터가 사라지기 전에 복구할 여유가 있어야 한다.
    ///
    /// 컴파일 타임 단정으로 둔다 — 상수 비교라 런타임 테스트는 의미가 없다.
    const _: () = assert!(HOT_TTL_DAYS > HOT_TIER_DAYS);
}
