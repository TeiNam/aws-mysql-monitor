//! 튜닝 권고 저장 — 데이터 테이블의 `TUNE#<record_id>`.
//!
//! # 왜 config 테이블이 아닌가
//!
//! 권고는 **그 레코드에 딸린 수집 산출물**이다. 레코드가 TTL(35일)로 사라지면 권고도
//! 함께 사라져야 한다 — 남으면 존재하지 않는 쿼리에 대한 조언이 목록에 남는다.
//! config 테이블에는 TTL 이 없고 "사람이 정한 상태" 만 둔다.
//!
//! # 한 레코드에 하나
//!
//! 같은 레코드를 다시 분석하면 **덮어쓴다.** 이력을 쌓으면 "어느 것이 최신인가" 를
//! 화면이 판단해야 하고, 오래된 권고는 스키마가 바뀐 뒤에 틀린 조언이 된다.
//! 대신 지문(`schema_fingerprint`)을 함께 저장해 화면이 "그 사이 인덱스가 바뀌었다" 를
//! 말할 수 있게 한다.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use dbmon_core::error::{DomainError, Result};
use dbmon_core::ids::RecordId;
use dbmon_core::time::EpochMs;
use dbmon_core::tuning::TuningAdvice;

use super::map_sdk_err;

/// 보관 기간 — 슬로우 쿼리 레코드와 같다(35일).
const TTL_DAYS: i64 = 35;

fn pk(record_id: &RecordId) -> String {
    format!("TUNE#{}", record_id.as_str())
}

pub struct DynamoTuningStore {
    client: Client,
    table: String,
}

impl DynamoTuningStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }

    /// 저장된 권고. **없으면 `None`** (오류가 아니다 — 아직 안 눌렀다는 뜻이다).
    pub async fn get(&self, record_id: &RecordId) -> Result<Option<TuningAdvice>> {
        let res = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(pk(record_id)))
            .key("SK", AttributeValue::S("ADVICE".into()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        let Some(item) = res.item else {
            return Ok(None);
        };
        let Some(doc) = item.get("doc").and_then(|v| v.as_s().ok()) else {
            return Ok(None);
        };
        // **읽지 못한 것과 없는 것을 구분한다.**
        //
        // 처음에는 `None` 으로 접었다(다시 누르면 덮어쓰니까). 그러면 화면이 "아직
        // 분석하지 않았다" 로 보여 주고, 사용자는 이미 만든 권고가 사라졌다고 생각한다
        // (교차 리뷰가 medium 으로 잡았다). 오류로 올리면 화면이 사유를 말하고,
        // 다시 분석하면 덮어쓸 수 있다는 사실은 그대로다.
        serde_json::from_str::<TuningAdvice>(doc)
            .map(Some)
            .map_err(|e| {
                tracing::error!(
                    record = %record_id.as_str(),
                    error = %crate::telemetry::Scrubbed(&e),
                    "튜닝 권고를 읽지 못했다"
                );
                DomainError::Internal("저장된 튜닝 권고를 읽을 수 없다".to_string())
            })
    }

    pub async fn put(&self, record_id: &RecordId, advice: &TuningAdvice) -> Result<()> {
        let doc = serde_json::to_string(advice).map_err(|e| DomainError::InvalidInput {
            field: "advice".into(),
            reason: format!("직렬화 실패: {e}"),
        })?;
        let ttl = ttl_seconds(advice.created_at_ms);
        self.client
            .put_item()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(pk(record_id)))
            .item("SK", AttributeValue::S("ADVICE".into()))
            .item("doc", AttributeValue::S(doc))
            .item("created_at_ms", AttributeValue::N(advice.created_at_ms.to_string()))
            .item("model_id", AttributeValue::S(advice.model_id.clone()))
            .item("ttl", AttributeValue::N(ttl.to_string()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }
}

/// TTL 은 **초 단위 epoch** 다 (DynamoDB 규약). ms 를 그대로 넣으면 5만 년 뒤가 된다.
fn ttl_seconds(created_at_ms: EpochMs) -> i64 {
    created_at_ms / 1000 + TTL_DAYS * 24 * 60 * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ttl_is_in_seconds_not_milliseconds() {
        // 2026-08-21T12:00:00Z
        let created = 1_787_313_600_000;
        let ttl = ttl_seconds(created);
        assert_eq!(ttl, created / 1000 + 3_024_000);
        // 초 단위인지 자리수로 확인한다 — ms 를 넣으면 13자리가 된다.
        assert!(ttl.to_string().len() == 10, "TTL 이 초가 아니다: {ttl}");
    }

    #[test]
    fn the_partition_key_is_scoped_to_the_record() {
        let instance = dbmon_core::ids::InstanceId::new("000000000000", "ap-northeast-2", "db")
            .expect("인스턴스 id");
        let id = RecordId::new(&instance, 12, 1_700_000_000_000);
        // **레코드마다 다른 파티션**이어야 한다 — 같은 키를 쓰면 마지막 권고가 모든
        // 레코드의 권고로 보인다.
        assert!(pk(&id).starts_with("TUNE#000000000000/ap-northeast-2/db"), "{}", pk(&id));
        let other = RecordId::new(&instance, 13, 1_700_000_000_000);
        assert_ne!(pk(&id), pk(&other));
    }
}
