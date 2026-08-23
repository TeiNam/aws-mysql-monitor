//! DynamoDB 감사 어댑터 — `config_table` 의 `AUDIT#<yyyy-mm>` 파티션
//! (FR-CRD-10, [07 §4](../../../../.claude/docs/07-credentials-bootstrap.md)).
//!
//! # 왜 조건부 쓰기를 쓰는가
//!
//! 감사 레코드는 **덮어써지면 안 된다.** 같은 (시각, 인스턴스) 키가 두 번 오는 것은
//! 우리 키 조립이 충돌한 것이고, 조용히 덮으면 한 건이 사라진다. `attribute_not_exists`
//! 로 막고 충돌을 오류로 올린다 — 사라진 감사 기록은 없는 것보다 나쁘다(있다고
//! 믿게 만든다).
//!
//! # TTL 을 걸지 않는다
//!
//! 슬로우 쿼리는 35일 TTL 이지만 감사는 **3년 보관**이다. 아카이브 잡이 월 단위로
//! Iceberg 로 옮기고 나서 지우는 것이 순서이고, TTL 로 먼저 지우면 아카이브가 빈
//! 파티션을 읽는다. TTL 속성을 넣지 않는 것이 그 순서를 코드로 표현한다.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use dbmon_core::error::{DomainError, Result};

use crate::bootstrap::AuditSink;
use crate::bootstrap::audit::AuditRecord;

use super::{is_conditional_failure, map_sdk_err};

pub struct DynamoAuditSink {
    client: Client,
    table: String,
}

impl DynamoAuditSink {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }
}

#[async_trait::async_trait]
impl AuditSink for DynamoAuditSink {
    async fn write(&self, pk: &str, sk: &str, record: &AuditRecord) -> Result<()> {
        // **레코드 전체를 JSON 문서 한 덩이로 넣는다.**
        //
        // 속성별로 쪼개면 스키마가 레코드 구조에 묶이고, 필드를 추가할 때마다
        // 어댑터를 고쳐야 한다. 감사 레코드는 읽는 쪽이 사람·아카이브 잡뿐이라
        // 문서 하나가 맞다 — `settings` 어댑터와 같은 판단이다.
        let body = serde_json::to_string(record)
            .map_err(|e| DomainError::Internal(format!("감사 레코드를 직렬화할 수 없다: {e}")))?;

        let result = self
            .client
            .put_item()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(pk.to_string()))
            .item("SK", AttributeValue::S(sk.to_string()))
            .item("event", AttributeValue::S(format!("{:?}", record.event)))
            .item("actor", AttributeValue::S(record.actor.clone()))
            // **워커는 별도 속성이다.** `actor` 에 섞으면 사용자별 조회가 깨진다.
            .item("worker_id", AttributeValue::S(record.worker_id.clone()))
            .item("at_ms", AttributeValue::N(record.at_ms.to_string()))
            .item("instance_id", AttributeValue::S(record.instance_id.clone()))
            .item("body", AttributeValue::S(body))
            // 같은 키가 두 번 오면 덮지 않고 실패한다.
            .condition_expression("attribute_not_exists(PK) AND attribute_not_exists(SK)")
            .send()
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(e) if is_conditional_failure(&e) => Err(DomainError::Conflict(format!(
                "감사 레코드 키가 충돌했다 ({pk} / {sk}) — 덮어쓰지 않았다"
            ))),
            Err(e) => Err(map_sdk_err(e)),
        }
    }
}
