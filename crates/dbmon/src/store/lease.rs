//! DynamoDB 리스 어댑터 (M4-21) — 샤드 소유권과 리더 선출.
//!
//! # F1 — 이 파일이 막는 실패
//!
//! 초기 설계는 워커마다 `SHARD_COUNT` 개를 목표로 잡았다. `desiredCount=2` 로 띄우면
//! **두 워커가 모든 샤드를 소유**하고 같은 인스턴스를 중복 수집한다. 다이제스트 누산기가
//! last-writer-wins 로 손상되고, 그건 조용히 일어난다.
//!
//! 그래서 **수집 리더 리스를 못 잡은 워커의 목표 샤드 수는 0** 이다. 불변식:
//!
//! ```text
//! 모든 워커의 ShardsOwned 합계 = SHARD_COUNT(64) 또는 0
//! ```
//!
//! 0 인 상태(리더 없음)는 리스 TTL 만큼만 지속된다. 그 사이 수집이 멈추는 것이
//! 중복 수집보다 낫다 — 후자는 데이터를 손상시킨다.
//!
//! # 조건부 쓰기가 유일한 상호배제다
//!
//! `try_acquire` 는 "소유자가 없거나 만료됐을 때만" 이라는 조건을 DynamoDB 에 맡긴다.
//! 애플리케이션에서 읽고-판단하고-쓰면 그 사이에 다른 워커가 잡을 수 있다.
//!
//! ⚠ 리스는 **펜싱 토큰이 아니다.** GC 정지나 네트워크 지연으로 만료를 눈치채지 못한
//! 워커가 계속 쓸 수 있다. `epoch` 를 저장해 2단계 펜싱(M12-21)의 기반을 남긴다.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{AttributeValue, ReturnValue};
use dbmon_core::error::Result;
use dbmon_core::ports::{COLLECT_LEADER_KEY, LEASE_TTL_MS, Lease, LeaseStore, SHARD_COUNT};
use dbmon_core::time::EpochMs;

use super::map_sdk_err;

/// 리스 항목의 파티션 키 접두. 슬로우 쿼리와 같은 테이블을 쓴다(단일 테이블 설계).
const PK_PREFIX: &str = "LEASE#";
/// 리스 항목의 정렬 키. 리스는 키당 하나다.
const SK: &str = "L";

pub struct DynamoLeaseStore {
    client: Client,
    table: String,
}

impl DynamoLeaseStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }

    fn pk(key: &str) -> String {
        format!("{PK_PREFIX}{key}")
    }

    fn to_lease(item: &std::collections::HashMap<String, AttributeValue>) -> Option<Lease> {
        let s = |k: &str| item.get(k)?.as_s().ok().cloned();
        let n = |k: &str| -> Option<i64> { item.get(k)?.as_n().ok()?.parse().ok() };
        Some(Lease {
            key: s("lease_key")?,
            owner: s("owner")?,
            expires_at_ms: n("expires_at_ms")?,
            // 음수 epoch 는 있을 수 없다(항상 1부터 증가한다). 방어적으로 0 으로 접는다.
            epoch: u64::try_from(n("epoch")?).unwrap_or(0),
        })
    }
}

#[async_trait::async_trait]
impl LeaseStore for DynamoLeaseStore {
    /// 조건부 획득. **소유자가 없거나 만료됐을 때만** 성공한다.
    ///
    /// 조건을 DynamoDB 에 맡기는 것이 요점이다. 읽고-판단하고-쓰면 그 사이에 다른
    /// 워커가 잡을 수 있고, 그러면 두 워커가 같은 샤드를 소유한다(F1).
    async fn try_acquire(&self, key: &str, owner: &str, now_ms: EpochMs) -> Result<Option<Lease>> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(Self::pk(key)))
            .key("SK", AttributeValue::S(SK.to_string()))
            // `epoch` 를 1 올리고 소유자·만료를 덮어쓴다.
            .update_expression(
                "SET #o = :owner, expires_at_ms = :exp, lease_key = :key, \
                 epoch = if_not_exists(epoch, :zero) + :one",
            )
            // 없거나(신규) 만료됐을 때만.
            .condition_expression("attribute_not_exists(PK) OR expires_at_ms < :now")
            .expression_attribute_names("#o", "owner")
            .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()))
            .expression_attribute_values(
                ":exp",
                AttributeValue::N((now_ms + LEASE_TTL_MS).to_string()),
            )
            .expression_attribute_values(":key", AttributeValue::S(key.to_string()))
            .expression_attribute_values(":now", AttributeValue::N(now_ms.to_string()))
            .expression_attribute_values(":zero", AttributeValue::N("0".into()))
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .return_values(ReturnValue::AllNew)
            .send()
            .await;

        match res {
            Ok(out) => Ok(out.attributes.as_ref().and_then(Self::to_lease)),
            // 다른 워커가 유효한 리스를 갖고 있다. 정상 경로다.
            Err(e) if super::is_conditional_failure(&e) => Ok(None),
            Err(e) => Err(map_sdk_err(e)),
        }
    }

    /// 갱신. **`owner` 와 `epoch` 가 모두 일치할 때만** 성공한다.
    ///
    /// `epoch` 를 조건에 넣는 이유: 우리가 만료를 눈치채지 못한 사이에 다른 워커가
    /// 잡았다가 놓았을 수 있다. 그때 `owner` 만 비교하면 우리 것으로 착각한다.
    /// 실패하면 그 리스를 **즉시 포기**해야 한다 — 계속 쓰면 중복 수집이다.
    async fn renew(&self, lease: &Lease, now_ms: EpochMs) -> Result<Option<Lease>> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(Self::pk(&lease.key)))
            .key("SK", AttributeValue::S(SK.to_string()))
            .update_expression("SET expires_at_ms = :exp")
            .condition_expression("#o = :owner AND epoch = :epoch")
            .expression_attribute_names("#o", "owner")
            .expression_attribute_values(":owner", AttributeValue::S(lease.owner.clone()))
            .expression_attribute_values(":epoch", AttributeValue::N(lease.epoch.to_string()))
            .expression_attribute_values(
                ":exp",
                AttributeValue::N((now_ms + LEASE_TTL_MS).to_string()),
            )
            .return_values(ReturnValue::AllNew)
            .send()
            .await;

        match res {
            Ok(out) => Ok(out.attributes.as_ref().and_then(Self::to_lease)),
            Err(e) if super::is_conditional_failure(&e) => Ok(None),
            Err(e) => Err(map_sdk_err(e)),
        }
    }

    /// 명시적 반납 — 즉시 재분배된다(그레이스풀 셧다운).
    ///
    /// 항목을 지우지 않고 **만료시킨다.** 지우면 `epoch` 가 사라져 펜싱 근거가 없어진다.
    async fn release(&self, lease: &Lease) -> Result<()> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(Self::pk(&lease.key)))
            .key("SK", AttributeValue::S(SK.to_string()))
            .update_expression("SET expires_at_ms = :zero")
            .condition_expression("#o = :owner AND epoch = :epoch")
            .expression_attribute_names("#o", "owner")
            .expression_attribute_values(":owner", AttributeValue::S(lease.owner.clone()))
            .expression_attribute_values(":epoch", AttributeValue::N(lease.epoch.to_string()))
            .expression_attribute_values(":zero", AttributeValue::N("0".into()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            // 이미 남의 것이면 반납할 것이 없다.
            Err(e) if super::is_conditional_failure(&e) => Ok(()),
            Err(e) => Err(map_sdk_err(e)),
        }
    }

    /// 접두로 리스를 나열한다.
    ///
    /// **`Scan` 을 쓰지 않는다** — IAM 이 `Deny dynamodb:Scan` 이다. 대신 키를 알고
    /// 있으므로(`SHARD#00`..`SHARD#63`, 리더 키 2개) 각각 `GetItem` 한다.
    /// 66회 왕복이지만 리스 스캔은 20초 주기이므로 문제가 되지 않는다.
    async fn list(&self, key_prefix: &str) -> Result<Vec<Lease>> {
        let keys: Vec<String> = if key_prefix.starts_with("SHARD") {
            (0..SHARD_COUNT)
                .map(dbmon_core::ports::shard_key)
                .filter(|k| k.starts_with(key_prefix))
                .collect()
        } else {
            [COLLECT_LEADER_KEY, dbmon_core::ports::CRON_LEADER_KEY]
                .into_iter()
                .filter(|k| k.starts_with(key_prefix))
                .map(str::to_string)
                .collect()
        };

        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let res = self
                .client
                .get_item()
                .table_name(&self.table)
                .key("PK", AttributeValue::S(Self::pk(&key)))
                .key("SK", AttributeValue::S(SK.to_string()))
                .send()
                .await
                .map_err(map_sdk_err)?;
            if let Some(l) = res.item.as_ref().and_then(Self::to_lease) {
                out.push(l);
            }
        }
        Ok(out)
    }
}

/// 이 워커가 소유해야 하는 샤드 수 (F1).
///
/// **리더가 아니면 0 이다.** 이 함수가 F1 불변식의 유일한 근거다:
///
/// ```text
/// 모든 워커의 합계 = SHARD_COUNT 또는 0
/// ```
///
/// 리더 하나만 `SHARD_COUNT` 를 반환하므로 합계가 그것을 넘을 수 없다.
/// 2단계(다중 active collector)로 가려면 펜싱 토큰이 먼저 필요하다(ADR-018).
pub fn target_shard_count(is_collect_leader: bool) -> u32 {
    if is_collect_leader { SHARD_COUNT } else { 0 }
}

/// 이 워커가 이 인스턴스를 수집해야 하는가.
pub fn owns_instance(is_collect_leader: bool, instance: &dbmon_core::ids::InstanceId) -> bool {
    if !is_collect_leader {
        return false;
    }
    // 리더는 전 샤드를 소유하므로 해시를 볼 필요가 없다. 2단계에서 이 함수가 바뀐다.
    let _ = dbmon_core::ports::shard_of(instance);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::ids::InstanceId;

    /// **F1 불변식.** 리더가 아니면 0 이어야 한다 — 그게 중복 수집을 막는 유일한 장치다.
    #[test]
    fn non_leader_targets_zero_shards() {
        assert_eq!(target_shard_count(false), 0, "리더가 아니면 0 이다");
        assert_eq!(target_shard_count(true), SHARD_COUNT);
    }

    /// 워커가 몇 대든 **합계가 `SHARD_COUNT` 를 넘지 않는다.**
    #[test]
    fn total_owned_shards_never_exceeds_the_shard_count() {
        for workers in 1..=10usize {
            // 리스는 하나뿐이므로 리더도 하나다.
            let total: u32 = (0..workers).map(|i| target_shard_count(i == 0)).sum();
            assert_eq!(
                total, SHARD_COUNT,
                "{workers}대일 때 합계가 {total} 이다 — SHARD_COUNT 여야 한다"
            );
        }
        // 리더가 없는 순간(재분배 중)에는 0 이다. 수집이 멈추는 것이 중복보다 낫다.
        let none: u32 = (0..5).map(|_| target_shard_count(false)).sum();
        assert_eq!(none, 0);
    }

    #[test]
    fn only_the_leader_owns_instances() {
        let i = InstanceId::new("123456789012", "ap-northeast-2", "orders-prd-01").expect("id");
        assert!(owns_instance(true, &i));
        assert!(!owns_instance(false, &i), "비리더가 인스턴스를 소유했다");
    }

    /// **`Scan` 을 쓰지 않는다.** IAM 이 Deny 하므로 코드에 있으면 운영에서 처음 드러난다.
    #[test]
    fn lease_store_never_scans() {
        let src = include_str!("lease.rs");
        let code = src.split("#[cfg(test)]").next().expect("본문");
        assert!(!code.contains(".scan()"), "리스 어댑터가 Scan 을 쓴다");
    }
}
