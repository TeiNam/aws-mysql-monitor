//! DynamoDB 정지 스코프 어댑터 — **`config_table` 의 첫 소비자**다.
//!
//! # 왜 config 테이블인가
//!
//! 이 테이블은 처음부터 "운영자가 정하는 값" 자리로 프로비저닝돼 있었고(IAM·환경변수도
//! 이미 배선돼 있다) 지금까지 소비자가 없었다. 정지 스코프가 정확히 그 성격이다 —
//! 수집 데이터가 아니라 사람이 정한 상태이므로 데이터 테이블의 TTL·GSI 규칙과 섞을
//! 이유가 없다.
//!
//! # 한 파티션에 모아 둔다
//!
//! `PK=CONTROL#PAUSE` 하나에 스코프별 항목을 둔다. 정지 스코프는 많아도 수십 개고,
//! 리더 루프가 **집합 전체**를 필요로 하므로 조회 1회로 끝나는 것이 요점이다.
//! 항목당 GetItem 을 세 번 하는 설계(전체·환경·인스턴스)는 인스턴스마다 3회가 되어
//! tick 예산을 먹는다.
//!
//! # TTL 을 붙이지 않는다
//!
//! 테이블에 `ttl` 속성이 설정돼 있지만 정지 항목에는 쓰지 않는다. **사람이 멈춘 것은
//! 사람이 재개해야 한다** — 자동으로 풀리면 "며칠 멈춰 뒀는데 어느새 다시 돌고 있다"
//! 가 되고, 그건 관측을 멈춘 이유(유지보수·부하)와 무관하게 일어난다.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use dbmon_core::error::Result;
use dbmon_core::pause::{PauseScope, PauseSet};
use dbmon_core::ports::PauseStore;
use dbmon_core::time::EpochMs;

use super::map_sdk_err;

/// 정지 항목의 파티션 키. 스코프 키가 정렬 키다.
const PK: &str = "CONTROL#PAUSE";

pub struct DynamoPauseStore {
    client: Client,
    table: String,
}

impl DynamoPauseStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }
}

#[async_trait::async_trait]
impl PauseStore for DynamoPauseStore {
    async fn list(&self) -> Result<PauseSet> {
        // **페이지네이션을 돌린다.** 스코프가 1MB 를 넘길 일은 없지만, 넘겼을 때
        // 조용히 일부만 반환하면 **멈춰 있어야 할 인스턴스가 수집된다.**
        let mut entries: Vec<(String, EpochMs)> = Vec::new();
        let mut last = None;
        loop {
            let mut req = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk")
                .expression_attribute_values(":pk", AttributeValue::S(PK.to_string()));
            if let Some(start) = last.take() {
                req = req.set_exclusive_start_key(Some(start));
            }
            let res = req.send().await.map_err(map_sdk_err)?;
            for item in res.items.unwrap_or_default() {
                let Some(key) = item.get("SK").and_then(|v| v.as_s().ok()) else {
                    continue;
                };
                // **모르는 키는 버린다.** 손으로 넣은 항목을 임의 해석하면 의도하지
                // 않은 인스턴스가 멈춘다. 버렸다는 사실은 남긴다.
                if PauseScope::parse(key).is_none() {
                    tracing::warn!(scope = %key, "알 수 없는 정지 스코프 — 무시한다");
                    continue;
                }
                // 시각이 없으면 0 이 아니라 **지금 기준으로 쓸 수 없는 값**이므로
                // 1 로 접는다 — "멈춰 있다" 는 사실이 시각 누락으로 사라지면 안 된다.
                let since = item
                    .get("since_ms")
                    .and_then(|v| v.as_n().ok())
                    .and_then(|n| n.parse::<i64>().ok())
                    .unwrap_or(1);
                entries.push((key.to_string(), since));
            }
            match res.last_evaluated_key {
                Some(k) if !k.is_empty() => last = Some(k),
                _ => return Ok(PauseSet::from_entries(entries)),
            }
        }
    }

    /// **`if_not_exists` 로 시작 시각을 지킨다.**
    ///
    /// `PutItem` 으로 덮으면 이미 멈춰 있는 스코프를 다시 누를 때마다 "방금 멈춤" 이
    /// 되어 화면이 "3분 전부터 멈춤" 을 말할 수 없다. 조건을 애플리케이션에서 읽고
    /// 판단하면 두 요청이 겹칠 때 같은 문제가 생긴다 — DynamoDB 에 맡긴다.
    async fn pause(&self, scope: &PauseScope, by: &str, now_ms: EpochMs) -> Result<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(PK.to_string()))
            .key("SK", AttributeValue::S(scope.as_key()))
            // `by` 는 매번 덮는다 — **마지막에 누른 사람**이 알고 싶은 값이다.
            .update_expression("SET since_ms = if_not_exists(since_ms, :now), paused_by = :by")
            .expression_attribute_values(":now", AttributeValue::N(now_ms.to_string()))
            .expression_attribute_values(":by", AttributeValue::S(by.to_string()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }

    /// 멱등하다 — 없는 항목을 지워도 DynamoDB 는 성공이다.
    async fn resume(&self, scope: &PauseScope) -> Result<()> {
        self.client
            .delete_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(PK.to_string()))
            .key("SK", AttributeValue::S(scope.as_key()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }
}
