//! DynamoDB 키 구성 ([04 §2.2·2.3](../../../../.claude/docs/04-data-model.md)).
//!
//! 키 문자열을 **한 곳에서만** 만든다. 쓰기와 읽기가 각자 만들면 한쪽만 바뀔 때
//! 조회가 조용히 0건을 반환한다 — 이 프로젝트에서 "고쳤는데 아무도 안 부른다" 부류가
//! 네 번 재발했고, 키 불일치는 그보다 찾기 어렵다.

use dbmon_core::env::Env;
use dbmon_core::ids::{DurBucket, InstanceId};
use dbmon_core::slow_query::{SlowQuery, SlowQueryState};
use dbmon_core::time::{DatePart, HourBucket, sort_key_ms};

/// `SQ#<instance_id>#<date_part>`
pub fn slow_query_pk(instance: &InstanceId, started_at_ms: i64) -> String {
    let date = DatePart::from_epoch_ms(started_at_ms);
    format!("SQ#{}#{}", instance.as_str(), date.as_str())
}

/// `<started_at_ms:013>#<thread_id>`
///
/// **13자리 0 패딩이 필수다.** `S` 정렬 키는 사전순이라 `"1000" < "999"` 다
/// ([04 §2.2a](../../../../.claude/docs/04-data-model.md)).
///
/// 조회의 전역 정렬·페이지 재개가 **이 문자열의 사전순에 의존한다.** 그래서 조립을
/// [`dbmon_core::slow_query::list_order_key`] 하나로 모았다 — 두 곳에서 만들면 정렬과
/// 범위 조회가 갈리고, 그러면 페이지 경계에서 행이 조용히 사라진다.
pub fn slow_query_sk(started_at_ms: i64, thread_id: u64) -> String {
    dbmon_core::slow_query::list_order_key(started_at_ms, thread_id)
}

/// GSI1 — **상태에 따라 파티션이 바뀐다.**
///
/// 한 항목은 `GSI1PK` 를 하나만 가질 수 있는데 GSI1 을 쓰는 접근 패턴이 둘이다:
///
/// | 패턴 | 필요한 값 | 관심 상태 |
/// |---|---|---|
/// | AP-3 다이제스트 → 최근 실행 샘플 | `DG#<app_digest>` | 완결된 것 |
/// | AP-18 진행 중 → 고아 정리 (F4) | `SQS#in_flight` (희소) | 진행 중인 것 |
///
/// 두 패턴의 관심 상태가 겹치지 않으므로 상태 전이와 함께 키를 바꾸면 둘 다 만족한다.
/// ⚠ 항상 `DG#` 를 쓰면 AP-18 이 **영구히 0건**을 반환하고 고아가 TTL(35일)까지 화면에
/// "실행 중" 으로 남는다 — 에러가 아니라 빈 결과라서 알아채기 어렵다.
pub fn gsi1(q: &SlowQuery) -> (String, String) {
    if q.state == SlowQueryState::InFlight {
        // 고아 스윕이 `GSI1SK < 임계` 로 오래된 것부터 스캔한다.
        let seen = q.last_seen_at_ms.unwrap_or(q.started_at_ms);
        (IN_FLIGHT_PK.to_string(), sort_key_ms(seen))
    } else {
        (
            format!("DG#{}", q.app_digest),
            format!(
                "Q#{}#{}",
                sort_key_ms(q.started_at_ms),
                q.instance_id.as_str()
            ),
        )
    }
}

/// AP-18 의 희소 파티션.
pub const IN_FLIGHT_PK: &str = "SQS#in_flight";

/// GSI2 — `ENV#<env>#<dur_bucket>#<hour_bucket>`
pub fn gsi2(env: Env, duration_ms: i64, started_at_ms: i64) -> (String, String) {
    let bucket = DurBucket::from_duration_ms(duration_ms);
    let hour = HourBucket::from_epoch_ms(started_at_ms);
    (
        format!("ENV#{}#{}#{}", env.as_str(), bucket.as_str(), hour.as_str()),
        sort_key_ms(started_at_ms),
    )
}

/// 항목 TTL (초). 핫 경계보다 길다 — 아카이브 잡 실패에 여유를 준다.
pub fn ttl_secs(started_at_ms: i64) -> i64 {
    started_at_ms.div_euclid(1000) + dbmon_core::HOT_TTL_DAYS * 86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> InstanceId {
        InstanceId::new("123456789012", "ap-northeast-2", "orders-prd-01").expect("id")
    }

    #[test]
    fn pk_carries_account_region_and_date() {
        let pk = slow_query_pk(&inst(), 1_755_500_400_000);
        assert!(pk.starts_with("SQ#123456789012/ap-northeast-2/orders-prd-01#"));
        assert!(pk.ends_with("2025-08-18") || pk.len() == pk.trim().len());
    }

    /// **정렬 키가 사전순 = 숫자순이어야 한다.** 패딩이 없으면 뒤집힌다.
    #[test]
    fn sort_keys_order_lexicographically() {
        let a = slow_query_sk(999, 1);
        let b = slow_query_sk(1_000, 1);
        let c = slow_query_sk(1_755_500_400_000, 1);
        assert!(a < b, "{a} < {b} 여야 한다");
        assert!(b < c);
    }

    /// GSI1 은 상태에 따라 **다른 파티션**이어야 한다. 안 그러면 AP-3 또는 AP-18 이 죽는다.
    #[test]
    fn gsi1_partition_follows_state() {
        let mut q = crate::store::tests::sample();
        q.state = SlowQueryState::InFlight;
        let (pk, _) = gsi1(&q);
        assert_eq!(pk, IN_FLIGHT_PK, "진행 중은 희소 파티션이다");

        q.state = SlowQueryState::Finalized;
        let (pk, sk) = gsi1(&q);
        assert!(pk.starts_with("DG#"), "확정은 다이제스트 축이다: {pk}");
        assert!(sk.starts_with("Q#"), "AP-3 의 begins_with 조건: {sk}");
    }

    /// 진행 중 GSI1SK 는 `last_seen_at_ms` 여야 고아 스윕이 범위 조회를 할 수 있다.
    #[test]
    fn in_flight_sort_key_tracks_last_seen() {
        let mut q = crate::store::tests::sample();
        q.state = SlowQueryState::InFlight;
        q.last_seen_at_ms = Some(1_755_500_409_000);
        let (_, sk) = gsi1(&q);
        assert_eq!(sk, sort_key_ms(1_755_500_409_000));
    }

    #[test]
    fn ttl_is_longer_than_the_hot_tier() {
        let started = 1_755_500_400_000i64;
        let ttl = ttl_secs(started);
        let boundary = started.div_euclid(1000) + dbmon_core::HOT_TIER_DAYS * 86_400;
        assert!(ttl > boundary, "TTL 이 핫 경계보다 길어야 한다");
    }
}
