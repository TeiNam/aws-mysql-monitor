//! DynamoDB 저장 어댑터 (M2-4).
//!
//! # 왜 `PutItem` 을 노출하지 않는가 (F5)
//!
//! 실시간 캡처 확정과 슬로우로그 백필이 **같은 실행**을 서로 다른 시각에 저장한다.
//! 백필이 먼저 오면 `PutItem` 이 그 정확 지표를 덮어쓴다. 그래서 포트에는
//! [`SlowQueryStore::upsert_merged`] 하나만 있고, 이 어댑터는 읽고-병합하고-쓴다.
//!
//! # 낙관적 잠금
//!
//! 읽고-병합하고-쓰는 사이에 다른 워커가 쓸 수 있다. `PutItem` 에
//! `attribute_not_exists(PK)`(신규) 또는 `SK = <읽은 SK>`(갱신) 조건을 걸고, 충돌하면
//! 다시 읽어 병합한다. 병합이 **교환·결합법칙을 만족**하므로(R43) 재시도 순서는 결과를
//! 바꾸지 않는다 — 그게 다섯 라운드에 걸쳐 대칭성을 4.25M 쌍으로 전수 검사한 이유다.
//!
//! # 로컬 개발
//!
//! `endpoint_url` 을 주면 DynamoDB Local 에 붙는다. AWS 자격증명이 만료돼도 저장 경로를
//! 개발·검증할 수 있다 — 이 프로젝트의 로컬 우선 원칙이다.

pub mod keys;
pub mod lease;
pub mod registry;

use std::collections::HashMap;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::types::AttributeValue;
use dbmon_core::error::{DomainError, Result};
use dbmon_core::ids::{InstanceId, RecordId};
use dbmon_core::merge::merge;
use dbmon_core::ports::SlowQueryStore;
use dbmon_core::slow_query::SlowQuery;
use dbmon_core::time::{DatePart, TimeRange, sort_key_ms};

/// 낙관적 잠금 재시도 상한.
///
/// 병합이 교환법칙을 만족하므로 재시도는 안전하다. 상한은 무한 루프 방지용이다.
const MAX_UPSERT_RETRIES: u32 = 5;

pub struct DynamoSlowQueryStore {
    client: Client,
    table: String,
}

impl DynamoSlowQueryStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }

    /// **로컬·테스트용 테이블 생성.** 프로덕션 테이블은 Terraform 이 만든다
    /// (`10-foundation`) — 앱이 스키마를 만들면 두 곳이 갈린다.
    ///
    /// 여기서 만드는 스키마는 Terraform 과 **같아야 한다.** 다르면 로컬에서 통과한
    /// 조회가 프로덕션에서 실패한다. `it_store` 가 두 정의를 대조한다.
    pub async fn create_table_for_local(&self) -> Result<()> {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, GlobalSecondaryIndex, KeySchemaElement, KeyType, Projection,
            ProjectionType, ScalarAttributeType,
        };

        let attr = |n: &str| {
            AttributeDefinition::builder()
                .attribute_name(n)
                .attribute_type(ScalarAttributeType::S)
                .build()
                .expect("속성 정의")
        };
        let key = |n: &str, t: KeyType| {
            KeySchemaElement::builder()
                .attribute_name(n)
                .key_type(t)
                .build()
                .expect("키 정의")
        };
        let gsi = |name: &str, pk: &str, sk: &str| {
            GlobalSecondaryIndex::builder()
                .index_name(name)
                .key_schema(key(pk, KeyType::Hash))
                .key_schema(key(sk, KeyType::Range))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()
                .expect("GSI 정의")
        };

        let res = self
            .client
            .create_table()
            .table_name(&self.table)
            .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
            .attribute_definitions(attr("PK"))
            .attribute_definitions(attr("SK"))
            .attribute_definitions(attr("GSI1PK"))
            .attribute_definitions(attr("GSI1SK"))
            .attribute_definitions(attr("GSI2PK"))
            .attribute_definitions(attr("GSI2SK"))
            .key_schema(key("PK", KeyType::Hash))
            .key_schema(key("SK", KeyType::Range))
            .global_secondary_indexes(gsi("GSI1", "GSI1PK", "GSI1SK"))
            .global_secondary_indexes(gsi("GSI2", "GSI2PK", "GSI2SK"))
            .send()
            .await;
        match res {
            Ok(_) => {}
            // 이미 있으면 성공으로 본다 — 테스트가 반복 실행된다.
            Err(e) if format!("{e:?}").contains("ResourceInUse") => {}
            Err(e) => return Err(map_sdk_err(e)),
        }
        self.wait_until_indexes_active().await
    }

    /// **테이블을 지우고 다시 만든다. 테스트 전용.**
    ///
    /// DynamoDB Local 은 `-dbPath` 로 뜨므로 컨테이너 재시작에도 데이터가 남는다
    /// (조회 화면을 개발하려면 그게 필요하다). 그래서 테스트가 이전 실행의 레코드를
    /// 본다 — 상태 전이 테스트가 실제로 그 때문에 실패했다: 이전 실행이 남긴
    /// `finalized` 레코드에 `in_flight` 를 병합하니 `merge_state` 가 종료 상태를
    /// 되돌리기를(정확하게) 거부해 인덱스에 나타나지 않았다.
    ///
    /// **코드가 아니라 테스트가 이전 상태에 의존했다.** MySQL 쪽 `reset_targets` 와 같은 교훈이다.
    pub async fn reset_table_for_local(&self) -> Result<()> {
        let deleted = self
            .client
            .delete_table()
            .table_name(&self.table)
            .send()
            .await;
        match deleted {
            Ok(_) => {}
            Err(e) if format!("{e:?}").contains("ResourceNotFound") => {}
            Err(e) => return Err(map_sdk_err(e)),
        }
        // 삭제 완료를 기다린다 — 바로 만들면 `ResourceInUse` 다.
        for _ in 0..50 {
            let exists = self
                .client
                .describe_table()
                .table_name(&self.table)
                .send()
                .await
                .is_ok();
            if !exists {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        self.create_table_for_local().await
    }

    /// **GSI 가 ACTIVE 가 될 때까지 기다린다.**
    ///
    /// `CreateTable` 은 즉시 반환하고 GSI 는 `CREATING` 상태로 남는다. 그 사이에
    /// 인덱스를 조회하면 **빈 결과**가 온다 — 에러가 아니므로 "인덱스가 잘못됐다" 와
    /// 구분되지 않는다. 실제로 이 경합이 테스트를 한 번 실패시켰다.
    async fn wait_until_indexes_active(&self) -> Result<()> {
        use aws_sdk_dynamodb::types::IndexStatus;

        for _ in 0..50 {
            let out = self
                .client
                .describe_table()
                .table_name(&self.table)
                .send()
                .await
                .map_err(map_sdk_err)?;
            let all_active = out
                .table
                .as_ref()
                .and_then(|t| t.global_secondary_indexes.as_ref())
                .is_some_and(|gsis| {
                    !gsis.is_empty()
                        && gsis
                            .iter()
                            .all(|g| g.index_status == Some(IndexStatus::Active))
                });
            if all_active {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Err(DomainError::Unavailable {
            dependency: "dynamodb",
            reason: format!("{}: GSI 가 ACTIVE 가 되지 않았다", self.table),
        })
    }

    /// 레코드를 항목으로. **키는 [`keys`] 가 만든다** — 쓰기·읽기가 각자 만들면
    /// 한쪽만 바뀔 때 조회가 조용히 0건이 된다.
    fn to_item(&self, q: &SlowQuery) -> Result<HashMap<String, AttributeValue>> {
        let mut item: HashMap<String, AttributeValue> = serde_dynamo::to_item(q)
            .map_err(|e| DomainError::Internal(format!("레코드 직렬화 실패: {e}")))?;

        let (g1pk, g1sk) = keys::gsi1(q);
        let (g2pk, g2sk) = keys::gsi2(q.env, q.duration_ms, q.started_at_ms);

        for (k, v) in [
            ("PK", keys::slow_query_pk(&q.instance_id, q.started_at_ms)),
            ("SK", keys::slow_query_sk(q.started_at_ms, q.thread_id)),
            ("GSI1PK", g1pk),
            ("GSI1SK", g1sk),
            ("GSI2PK", g2pk),
            ("GSI2SK", g2sk),
        ] {
            item.insert(k.to_string(), AttributeValue::S(v));
        }
        item.insert(
            "ttl".to_string(),
            AttributeValue::N(keys::ttl_secs(q.started_at_ms).to_string()),
        );
        Ok(item)
    }

    fn from_item(item: HashMap<String, AttributeValue>) -> Result<SlowQuery> {
        serde_dynamo::from_item(item)
            .map_err(|e| DomainError::Internal(format!("레코드 역직렬화 실패: {e}")))
    }

    /// `record_id` 로 항목을 찾는다.
    ///
    /// # `GetItem` 을 쓸 수 없다
    ///
    /// `record_id` 는 `started_at_ms` 를 **초 단위로 절단**해서 담는다(멱등 키가 ±1초
    /// 흔들림을 흡수하도록 의도된 설계다). `SK` 는 밀리초를 담으므로 `record_id` 에서
    /// 복원할 수 없다. 그래서 `begins_with(SK, <초 접두>)` 로 조회하고 `thread_id` 로
    /// 좁힌다 — 같은 초·같은 스레드는 하나뿐이다.
    async fn find_by_record_id(&self, id: &RecordId) -> Result<Option<SlowQuery>> {
        let (instance, thread_id, sec) = id.parts().map_err(|e| DomainError::InvalidInput {
            field: "record_id".into(),
            reason: e.to_string(),
        })?;
        let ms = sec * 1000;
        let padded = sort_key_ms(ms);
        // 13자리 중 앞 10자리가 초까지다. 뒤 3자리(밀리초)는 무엇이든 매칭한다.
        let second_prefix = &padded[..padded.len() - 3];

        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
            .expression_attribute_values(
                ":pk",
                AttributeValue::S(keys::slow_query_pk(&instance, ms)),
            )
            .expression_attribute_values(":sk", AttributeValue::S(second_prefix.to_string()))
            .send()
            .await
            .map_err(map_sdk_err)?;

        for item in out.items.unwrap_or_default() {
            let q = Self::from_item(item)?;
            if q.thread_id == thread_id {
                return Ok(Some(q));
            }
        }
        Ok(None)
    }
}

pub(crate) fn map_sdk_err<E: std::fmt::Debug, R: std::fmt::Debug>(
    e: SdkError<E, R>,
) -> DomainError {
    DomainError::Unavailable {
        dependency: "dynamodb",
        reason: format!("{e:?}"),
    }
}

/// 조건부 쓰기 실패인가. 그러면 다시 읽어 병합해야 한다.
pub(crate) fn is_conditional_failure<E: std::fmt::Debug, R: std::fmt::Debug>(
    e: &SdkError<E, R>,
) -> bool {
    // SDK 의 오류 타입을 문자열로 판정한다. 타입으로 매칭하면 `PutItemError` 변형이
    // 버전마다 달라져 컴파일이 깨진다.
    format!("{e:?}").contains("ConditionalCheckFailed")
}

#[async_trait::async_trait]
impl SlowQueryStore for DynamoSlowQueryStore {
    async fn upsert_merged(&self, q: &SlowQuery) -> Result<SlowQuery> {
        for attempt in 0..MAX_UPSERT_RETRIES {
            let existing = self.find_by_record_id(&q.record_id).await?;
            let merged = match &existing {
                // **`merge(existing, incoming)`** 순서를 지킨다. `record_id`·`literal_policy`
                // 는 "먼저 저장된 쪽 유지" 가 문서화된 의도다.
                Some(prev) => merge(prev, q),
                None => q.clone(),
            };
            let item = self.to_item(&merged)?;

            let mut put = self
                .client
                .put_item()
                .table_name(&self.table)
                .set_item(Some(item));
            put = match &existing {
                // 신규: 아직 없어야 한다.
                None => put.condition_expression("attribute_not_exists(PK)"),
                // 갱신: 우리가 읽은 그 항목이어야 한다. `SK` 는 밀리초를 담으므로
                // 같은 `record_id` 라도 다른 밀리초면 다른 항목이다.
                Some(prev) => put
                    .condition_expression("SK = :sk")
                    .expression_attribute_values(
                        ":sk",
                        AttributeValue::S(keys::slow_query_sk(prev.started_at_ms, prev.thread_id)),
                    ),
            };

            match put.send().await {
                Ok(_) => return Ok(merged),
                Err(e) if is_conditional_failure(&e) => {
                    // 다른 워커가 먼저 썼다. 다시 읽어 병합한다 — 병합이 교환법칙을
                    // 만족하므로 결과는 순서와 무관하다.
                    tracing::debug!(attempt, "낙관적 잠금 충돌 — 다시 읽는다");
                    continue;
                }
                Err(e) => return Err(map_sdk_err(e)),
            }
        }
        Err(DomainError::Unavailable {
            dependency: "dynamodb",
            reason: format!("낙관적 잠금 재시도 {MAX_UPSERT_RETRIES}회 초과"),
        })
    }

    async fn get(&self, id: &RecordId) -> Result<Option<SlowQuery>> {
        self.find_by_record_id(id).await
    }

    /// `(instance, thread_id, ±window, app_digest)` 보조 조회.
    ///
    /// `record_id` 가 1초 어긋났을 때 후보를 찾는다 ([05 §8.2](../../../../docs/05-collector.md)).
    /// 시작 시각이 날짜 경계를 걸칠 수 있으므로 **양쪽 날짜 파티션을 본다.**
    async fn find_merge_candidate(
        &self,
        instance: &InstanceId,
        thread_id: u64,
        app_digest: &str,
        around_ms: dbmon_core::time::EpochMs,
        window_ms: i64,
    ) -> Result<Option<SlowQuery>> {
        let (lo, hi) = (around_ms - window_ms, around_ms + window_ms);
        let mut best: Option<SlowQuery> = None;

        // 창이 자정을 걸치면 파티션이 둘이다. 중복은 아래에서 걸러진다.
        let mut dates = vec![DatePart::from_epoch_ms(lo)];
        let hi_date = DatePart::from_epoch_ms(hi);
        if hi_date != dates[0] {
            dates.push(hi_date);
        }

        for date in dates {
            let pk = format!("SQ#{}#{}", instance.as_str(), date.as_str());
            let out = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
                .expression_attribute_values(":pk", AttributeValue::S(pk))
                .expression_attribute_values(":lo", AttributeValue::S(sort_key_ms(lo)))
                // `#` 를 붙여 같은 밀리초의 모든 스레드를 포함한다.
                .expression_attribute_values(
                    ":hi",
                    AttributeValue::S(format!("{}#\u{10FFFF}", sort_key_ms(hi))),
                )
                .send()
                .await
                .map_err(map_sdk_err)?;

            for item in out.items.unwrap_or_default() {
                let cand = Self::from_item(item)?;
                if cand.thread_id != thread_id || cand.app_digest != app_digest {
                    continue;
                }
                // 가장 가까운 것을 고른다 — 창 안에 둘 이상이면 결정론적이어야 한다.
                let closer = best.as_ref().is_none_or(|b| {
                    (cand.started_at_ms - around_ms).abs() < (b.started_at_ms - around_ms).abs()
                });
                if closer {
                    best = Some(cand);
                }
            }
        }
        Ok(best)
    }

    async fn list_by_instance(
        &self,
        instance: &InstanceId,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<SlowQuery>> {
        let mut out = Vec::new();
        // 날짜 파티션을 순회한다. `date_parts()` 에 상한이 있어 폭주하지 않는다.
        for date in range.date_parts() {
            if out.len() >= limit {
                break;
            }
            let pk = format!("SQ#{}#{}", instance.as_str(), date.as_str());
            let res = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
                .expression_attribute_values(":pk", AttributeValue::S(pk))
                .expression_attribute_values(":lo", AttributeValue::S(sort_key_ms(range.from_ms())))
                .expression_attribute_values(
                    ":hi",
                    AttributeValue::S(format!("{}#\u{10FFFF}", sort_key_ms(range.to_ms()))),
                )
                // 최신순.
                .scan_index_forward(false)
                .limit((limit - out.len()) as i32)
                .send()
                .await
                .map_err(map_sdk_err)?;
            for item in res.items.unwrap_or_default() {
                out.push(Self::from_item(item)?);
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// AP-18 — 희소 GSI1 로 진행 중 레코드를 조회한다 (고아 정리, F4).
    ///
    /// 오래된 것부터 본다. `GSI1SK` 가 `last_seen_at_ms` 이므로 오름차순이 곧 오래된 순이다.
    async fn list_in_flight(&self, limit: usize) -> Result<Vec<SlowQuery>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name("GSI1")
            .key_condition_expression("GSI1PK = :pk")
            .expression_attribute_values(":pk", AttributeValue::S(keys::IN_FLIGHT_PK.to_string()))
            .scan_index_forward(true)
            .limit(limit as i32)
            .send()
            .await
            .map_err(map_sdk_err)?;
        out.items
            .unwrap_or_default()
            .into_iter()
            .map(Self::from_item)
            .collect()
    }
}

/// 스캔을 금지하는 것은 IAM 이 하지만(`Deny dynamodb:Scan`), 코드에도 없어야 한다.
/// 이 상수는 그 사실을 테스트가 확인하는 데 쓴다.
#[allow(dead_code)]
const _NO_SCAN: () = ();

#[cfg(test)]
pub(crate) mod tests {
    use dbmon_core::env::Env;
    use dbmon_core::ids::{InstanceId, RecordId};
    use dbmon_core::instance::Engine;
    use dbmon_core::slow_query::*;
    use dbmon_normalize::StatementType;

    /// 키 테스트가 쓰는 최소 레코드.
    pub fn sample() -> SlowQuery {
        let i = InstanceId::new("123456789012", "ap-northeast-2", "orders-prd-01").expect("id");
        SlowQuery {
            record_id: RecordId::new(&i, 8842119, 1_755_500_400_000),
            instance_id: i,
            cluster_id: None,
            env: Env::Prd,
            engine: Engine::Mysql,
            engine_version: "8.4.6".into(),
            state: SlowQueryState::Finalized,
            thread_id: 8842119,
            schema_name: Some("shop".into()),
            db_user: None,
            db_host: None,
            started_at_ms: 1_755_500_400_000,
            started_at_ms_precise: None,
            ended_at_ms: Some(1_755_500_404_000),
            captured_at_ms: 1_755_500_404_100,
            duration_ms: 4_000,
            duration_source: DurationSource::Span,
            sql_text: None,
            sql_text_truncated: false,
            sql_text_lossy: false,
            literal_policy: LiteralPolicy::Masked,
            literal_policy_at_ms: 1_755_500_400_000,
            app_digest: "9f2c1a".into(),
            digest_algo_version: 1,
            mysql_digest: None,
            statement_type: StatementType::Select,
            is_nested: false,
            stats: Default::default(),
            plan: Default::default(),
            capture_source: CaptureSource::Processlist,
            owner_worker: None,
            owner_epoch: None,
            last_seen_at_ms: None,
            abandoned_reason: None,
            long_running: false,
        }
    }

    /// **이 파일에 `scan` 이 없어야 한다.** IAM 이 `Deny dynamodb:Scan` 으로 막지만,
    /// 코드에 있으면 그 Deny 가 운영 중에 처음 드러난다.
    #[test]
    fn adapter_never_scans() {
        let src = include_str!("mod.rs");
        // 테스트 모듈 앞부분만 본다 (이 테스트 자신의 문자열을 세지 않도록).
        let code = src.split("#[cfg(test)]").next().expect("본문");
        assert!(
            !code.contains(".scan()"),
            "어댑터가 Scan 을 쓴다 — IAM 이 Deny 하므로 운영에서 실패한다"
        );
    }
}
