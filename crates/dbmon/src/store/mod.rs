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

pub mod broadcast;
pub mod checkpoint;
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

/// 낙관적 잠금 카운터 속성 이름.
///
/// # 왜 `SK = <읽은 SK>` 만으로는 부족했나 (7라운드 지적)
///
/// `PutItem` 은 이미 그 `PK`/`SK` 를 겨냥하므로 `SK = :sk` 조건은 **"그 자리에 항목이
/// 있다" 만 확인한다** — 항진명제에 가깝다. 그래서 두 워커가 같은 항목을 동시에
/// 읽고 각자 병합해 쓰면 **둘 다 성공하고 나중 것이 앞의 것을 덮는다.** 재시도
/// 경로가 아예 돌지 않으므로 병합의 교환법칙도 소용이 없다 — 실행계획이나 정확
/// 지표가 **조용히 사라질 수 있다.**
///
/// 이 카운터를 조건에 넣으면 하나만 통과하고, 진 쪽은 다시 읽어 병합한다(그 재시도
/// 루프는 이미 있었다).
const REV_ATTR: &str = "rev";

/// 저장소에서 읽은 항목. **물리 키를 함께 들고 온다.**
///
/// 레코드의 `started_at_ms` 필드로 키를 다시 만들 수 없기 때문이다
/// ([`DynamoSlowQueryStore::to_item_at`] 의 주석 참고) — 병합이 시작 시각을 앞당기면
/// 필드와 키가 어긋나고, 그때부터 그 레코드는 **영구히 갱신 불가**가 된다.
#[derive(Clone)]
struct Stored {
    record: SlowQuery,
    pk: String,
    sk: String,
    /// 낙관적 잠금 카운터. **옛 항목에는 없다**(`None`).
    rev: Option<u64>,
}

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

    /// 테이블에 실제로 닿는지 확인한다. **기동 시 준비 상태의 근거다.**
    ///
    /// `build_stores` 는 클라이언트만 조립하고 네트워크를 건드리지 않는다. 그것만으로
    /// `storage_ok = true` 를 세우면 **자격증명이 틀렸거나 테이블 이름이 틀렸을 때도
    /// `/readyz` 가 정상을 보고한다** — 모든 쓰기가 실패하는데 헬스체크는 초록이다
    /// (2차 리뷰가 지적). `DescribeTable` 한 번으로 그 거짓 보고를 없앤다.
    pub async fn probe(&self) -> Result<()> {
        self.client
            .describe_table()
            .table_name(&self.table)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
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
        self.to_item_at(
            q,
            &keys::slow_query_pk(&q.instance_id, q.started_at_ms),
            &keys::slow_query_sk(q.started_at_ms, q.thread_id),
            1,
        )
    }

    /// 레코드를 항목으로 만들되 **키는 주어진 물리 키를 그대로** 쓴다.
    ///
    /// # 왜 키를 분리해야 하는가
    ///
    /// `merge` 는 `started_at_ms` 를 **더 이른 쪽**으로 정한다(더 정확한 추정을
    /// 채택한다). 그런데 `SK` 가 `started_at_ms` 에서 파생되므로, 병합이 시작 시각을
    /// 앞당기면 **항목의 키가 이동한다.** 그러면 조건부 쓰기(`SK = <읽은 SK>`)가
    /// 가리키는 자리에 항목이 없어 조건 실패 → 재시도 → 5회 초과로 죽는다.
    /// 실제로 그렇게 죽었다("낙관적 잠금 재시도 5회 초과").
    ///
    /// 그래서 **항목은 처음 저장된 자리에 머문다.** 더 정확한 시작 시각은 속성으로
    /// 남고(`started_at_ms`), 키는 움직이지 않는다.
    ///
    /// 대가: `SK` 가 실제 시작 시각과 최대 1초 어긋날 수 있어 시간 범위 조회의
    /// 경계가 그만큼 부정확해진다. 한 실행이 두 레코드로 갈리는 것보다 훨씬 낫다.
    ///
    /// # ⚠ 왜 `key_source: &SlowQuery` 로는 안 되는가 (6라운드 지적, 실측 확인)
    ///
    /// 예전에는 읽어온 레코드의 `started_at_ms` **필드**로 키를 다시 만들었다. 그런데
    /// 위 규칙 때문에 **필드와 물리 키는 첫 병합 이후 어긋난다** — 항목은 416에 있고
    /// 필드는 413이다. 그 다음 갱신이 필드로 키를 만들면 413을 겨냥하고,
    ///
    /// - 413에 아무것도 없으면 조건(`SK = 413`)이 실패해 **재시도 5회 초과로 죽는다**
    ///   (로컬에서 그 오류를 실제로 봤다),
    /// - 413에 쌍둥이가 있으면 **그쪽을 고치고 416의 유령은 그대로 남는다.**
    ///
    /// 즉 "자리에 머문다" 는 첫 병합까지만 성립했다. 물리 키를 들고 다녀야 한다.
    fn to_item_at(
        &self,
        q: &SlowQuery,
        pk: &str,
        sk: &str,
        next_rev: u64,
    ) -> Result<HashMap<String, AttributeValue>> {
        let mut item: HashMap<String, AttributeValue> = serde_dynamo::to_item(q)
            .map_err(|e| DomainError::Internal(format!("레코드 직렬화 실패: {e}")))?;

        let (g1pk, g1sk) = keys::gsi1(q);
        let (g2pk, g2sk) = keys::gsi2(q.env, q.duration_ms, q.started_at_ms);

        for (k, v) in [
            ("PK", pk.to_string()),
            ("SK", sk.to_string()),
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
        // **낙관적 잠금 카운터.** 조건이 이 값을 보므로 동시 쓰기 중 하나만 통과한다.
        item.insert(
            REV_ATTR.to_string(),
            AttributeValue::N(next_rev.to_string()),
        );
        Ok(item)
    }

    fn from_item(item: HashMap<String, AttributeValue>) -> Result<SlowQuery> {
        serde_dynamo::from_item(item)
            .map_err(|e| DomainError::Internal(format!("레코드 역직렬화 실패: {e}")))
    }

    /// 항목을 **물리 키와 함께** 읽는다. 갱신 경로가 쓴다.
    fn stored_from_item(item: HashMap<String, AttributeValue>) -> Result<Stored> {
        let key = |name: &str| -> Result<String> {
            item.get(name)
                .and_then(|v| v.as_s().ok())
                .cloned()
                .ok_or_else(|| DomainError::Internal(format!("항목에 {name} 가 없다")))
        };
        let pk = key("PK")?;
        let sk = key("SK")?;
        let rev = item
            .get(REV_ATTR)
            .and_then(|v| v.as_n().ok())
            .and_then(|n| n.parse::<u64>().ok());
        Ok(Stored {
            record: Self::from_item(item)?,
            pk,
            sk,
            rev,
        })
    }

    /// `(instance, thread_id, ±window, app_digest)` 보조 조회.
    ///
    /// `record_id` 가 1초 어긋났을 때 후보를 찾는다 ([05 §8.2](../../../../docs/05-collector.md)).
    /// 시작 시각이 날짜 경계를 걸칠 수 있으므로 **양쪽 날짜 파티션을 본다.**
    async fn find_merge_candidate_stored(
        &self,
        instance: &InstanceId,
        thread_id: u64,
        app_digest: &str,
        around_ms: dbmon_core::time::EpochMs,
        duration_ms: i64,
        window_ms: i64,
    ) -> Result<Option<Stored>> {
        let (lo, hi) = (around_ms - window_ms, around_ms + window_ms);
        let mut best: Option<Stored> = None;

        // 창이 자정을 걸치면 파티션이 둘이다. 중복은 아래에서 걸러진다.
        let mut dates = vec![DatePart::from_epoch_ms(lo)];
        let hi_date = DatePart::from_epoch_ms(hi);
        if hi_date != dates[0] {
            dates.push(hi_date);
        }

        for date in dates {
            let pk = format!("SQ#{}#{}", instance.as_str(), date.as_str());
            let mut start_key: Option<HashMap<String, AttributeValue>> = None;
            loop {
                let out = self
                    .client
                    .query()
                    .table_name(&self.table)
                    .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
                    .expression_attribute_values(":pk", AttributeValue::S(pk.clone()))
                    .expression_attribute_values(":lo", AttributeValue::S(sort_key_ms(lo)))
                    // `#` 를 붙여 같은 밀리초의 모든 스레드를 포함한다.
                    .expression_attribute_values(
                        ":hi",
                        AttributeValue::S(format!("{}#\u{10FFFF}", sort_key_ms(hi))),
                    )
                    // 읽고-병합하고-쓰는 경로다 (위 `candidates_by_record_id` 주석 참고).
                    .consistent_read(true)
                    .set_exclusive_start_key(start_key)
                    .send()
                    .await
                    .map_err(map_sdk_err)?;

                for item in out.items.unwrap_or_default() {
                    let cand = Self::stored_from_item(item)?;
                    if cand.record.thread_id != thread_id
                        || !same_execution(&cand.record, app_digest, around_ms, duration_ms)
                    {
                        continue;
                    }
                    // 가장 가까운 것을 고른다 — 창 안에 둘 이상이면 결정론적이어야 한다.
                    let closer = best.as_ref().is_none_or(|b| {
                        (cand.record.started_at_ms - around_ms).abs()
                            < (b.record.started_at_ms - around_ms).abs()
                    });
                    if closer {
                        best = Some(cand);
                    }
                }
                start_key = out.last_evaluated_key;
                if start_key.is_none() {
                    break;
                }
            }
        }
        Ok(best)
    }

    /// 같은 초 버킷의 **모든** 후보를 모은다.
    ///
    /// # 왜 첫 일치로는 안 되는가 (실측으로 드러난 결함)
    ///
    /// 같은 실행에 항목이 **두 개** 생길 수 있다 — 실시간 캡처와 슬로우로그가 처음
    /// 쓰기를 동시에 하면 둘 다 "없다" 를 보고, `SK` 의 밀리초가 3ms 달라
    /// `attribute_not_exists(PK)` 조건이 **양쪽 다 통과**한다. 로컬에서 그 상태를 봤다:
    ///
    /// ```text
    /// SK 1787241482413#10912  state=finalized  capture=merged
    /// SK 1787241482416#10912  state=in_flight  capture=processlist   ← 유령
    /// ```
    ///
    /// 첫 일치(작은 SK)를 고치면 유령은 그대로 남고 스윕은 **성공을 보고한다** —
    /// 30초마다 영구히 반복됐다. 그래서 후보를 다 모아 [`pick_target`] 이 고른다.
    async fn candidates_by_record_id(&self, id: &RecordId) -> Result<Vec<Stored>> {
        let (instance, thread_id, sec) = id.parts().map_err(|e| DomainError::InvalidInput {
            field: "record_id".into(),
            reason: e.to_string(),
        })?;
        let ms = sec * 1000;
        let padded = sort_key_ms(ms);
        // 13자리 중 앞 10자리가 초까지다. 뒤 3자리(밀리초)는 무엇이든 매칭한다.
        let second_prefix = &padded[..padded.len() - 3];

        let mut found = Vec::new();
        let mut start_key: Option<HashMap<String, AttributeValue>> = None;
        loop {
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
                // **강한 일관성으로 읽는다.** 읽고-병합하고-쓰는 경로이므로 결과적
                // 일관성 읽기는 **직전에 성공한 쓰기를 못 볼 수 있다** — 그러면 조건부
                // 쓰기가 계속 실패해 재시도 5회를 소진한다(8라운드 지적).
                .consistent_read(true)
                .set_exclusive_start_key(start_key)
                .send()
                .await
                .map_err(map_sdk_err)?;
            for item in out.items.unwrap_or_default() {
                let cand = Self::stored_from_item(item)?;
                if cand.record.thread_id == thread_id {
                    found.push(cand);
                }
            }
            // **페이지를 따라간다.** 바쁜 1초는 1MB 를 넘길 수 있고, 그러면 "모든
            // 후보" 가 실제로는 첫 페이지뿐이다 — 대상이 빠지면 쌍둥이를 새로 만든다.
            start_key = out.last_evaluated_key;
            if start_key.is_none() {
                break;
            }
        }
        Ok(found)
    }

    /// `record_id` 로 항목 하나를 찾는다 (`get()` 이 쓴다).
    ///
    /// # `GetItem` 을 쓸 수 없다
    ///
    /// `record_id` 는 `started_at_ms` 를 **초 단위로 절단**해서 담는다(멱등 키가 ±1초
    /// 흔들림을 흡수하도록 의도된 설계다). `SK` 는 밀리초를 담으므로 `record_id` 에서
    /// 복원할 수 없다. 그래서 초 버킷을 조회한다.
    ///
    /// 쌍둥이가 있으면 후보가 둘 이상이다. 조회는 **가장 정보가 많은 쪽**(확정·정확
    /// 지표)을 주는 것이 옳다 — 상세 화면이 "진행 중" 을 보여 주고 끝나면 안 된다.
    async fn find_by_record_id(&self, id: &RecordId) -> Result<Option<Stored>> {
        let mut found = self.candidates_by_record_id(id).await?;
        found.sort_by_key(|s| std::cmp::Reverse(read_rank(&s.record)));
        Ok(found.into_iter().next())
    }
}

/// 후보가 **같은 실행인가.** 저장소의 ±2초 병합과 조회의 접기가 같은 규칙을 쓴다.
///
/// # 다이제스트 일치만으로는 두 방향으로 틀린다
///
/// - **너무 느슨하다**: `long_query_time` 이 창(2초)보다 작으면 같은 스레드에서 같은
///   쿼리가 창 안에 두 번 시작할 수 있다. 그 둘을 합치면 **실행 하나가 사라진다.**
///   → 커넥션은 한 번에 한 문장만 실행하므로 **구간이 겹칠 것**을 요구한다.
/// - **너무 엄하다**: 심층 조회가 상한에 걸리면 실시간 캡처의 다이제스트가
///   `unknown-<thread>` 자리표다([`UNKNOWN_DIGEST_PREFIX`]). 슬로우로그의 진짜
///   다이제스트와 다르므로 **같은 실행인데 합쳐지지 않는다** — 표에 두 줄, 통계는
///   두 배(8라운드 지적). → 자리표는 "없음" 으로 취급한다(병합 규칙과 같다).
fn same_execution(
    cand: &SlowQuery,
    incoming_digest: &str,
    incoming_started_ms: dbmon_core::time::EpochMs,
    incoming_duration_ms: i64,
) -> bool {
    use dbmon_core::slow_query::UNKNOWN_DIGEST_PREFIX;
    let placeholder = |d: &str| d.starts_with(UNKNOWN_DIGEST_PREFIX);
    let digest_ok = cand.app_digest == incoming_digest
        || placeholder(&cand.app_digest)
        || placeholder(incoming_digest);
    if !digest_ok {
        return false;
    }
    // 구간이 겹치는가. 어느 쪽이 먼저 시작했는지 모르므로 양방향으로 본다.
    let (a_start, a_end) = (
        cand.started_at_ms,
        cand.started_at_ms.saturating_add(cand.duration_ms),
    );
    let (b_start, b_end) = (
        incoming_started_ms,
        incoming_started_ms.saturating_add(incoming_duration_ms),
    );
    a_start < b_end && b_start < a_end
}

/// 여러 후보 중 **고칠 항목**을 고른다. 순수 함수 — 규칙을 테스트로 고정한다.
///
/// 쌍둥이(같은 실행의 항목 둘)가 있을 때 아무거나 고치면 나머지가 유령으로 남는다.
/// 순서대로 본다:
///
/// 1. 들어온 레코드가 **닫는 쓰기**(진행 중이 아니다)면 **저장된 진행 중 항목** —
///    고아 스윕·일시정지 확정·정상 확정이 이 경로다. **닫으려는 대상은 열려 있는
///    쪽이다.** 이 규칙이 물리 키 일치보다 앞서야 한다: 병합이 시각을 앞당긴
///    레코드는 필드로 계산한 키가 **쌍둥이를 가리킬 수 있고**, 그러면 유령이 남는다
///    (7라운드 지적 — 테스트가 이 순서를 고정한다).
/// 2. **물리 `SK` 가 들어온 레코드의 시작 시각과 일치** — 정상 경로다. 진행 중 갱신은
///    매 tick 같은 자리를 쓴다.
/// 3. 필드 시작 시각이 일치 — 읽어서 고치는 경로(병합이 시각을 앞당긴 항목).
/// 4. 시작 시각이 가장 가까운 것 — 남은 경우를 결정론적으로 만든다.
fn pick_target<'a>(candidates: &'a [Stored], incoming: &SlowQuery) -> Option<&'a Stored> {
    if candidates.is_empty() {
        return None;
    }
    // 닫는 쓰기는 **열려 있는 후보 안에서** 고른다. 열린 것이 여럿이면(쌍둥이 둘이
    // 다 진행 중) 아무거나 고르면 안 된다 — 같은 규칙을 그 부분집합에 적용한다
    // (8라운드 지적).
    if incoming.state != dbmon_core::slow_query::SlowQueryState::InFlight {
        let open: Vec<&Stored> = candidates
            .iter()
            .filter(|c| c.record.state == dbmon_core::slow_query::SlowQueryState::InFlight)
            .collect();
        if !open.is_empty() {
            return closest_of(&open, incoming);
        }
    }
    let all: Vec<&Stored> = candidates.iter().collect();
    closest_of(&all, incoming)
}

/// 후보 중 하나를 **결정론적으로** 고른다: 물리 키 일치 → 필드 일치 → 가장 가까운 것.
fn closest_of<'a>(candidates: &[&'a Stored], incoming: &SlowQuery) -> Option<&'a Stored> {
    let want_sk = keys::slow_query_sk(incoming.started_at_ms, incoming.thread_id);
    if let Some(hit) = candidates.iter().find(|c| c.sk == want_sk) {
        return Some(hit);
    }
    if let Some(same) = candidates
        .iter()
        .find(|c| c.record.started_at_ms == incoming.started_at_ms)
    {
        return Some(same);
    }
    candidates
        .iter()
        .min_by_key(|c| {
            (
                (c.record.started_at_ms - incoming.started_at_ms).abs(),
                // 동거리면 SK 로 확정한다 — 순서가 응답마다 달라지면 안 된다.
                c.sk.clone(),
            )
        })
        .copied()
}

/// 조회가 여러 후보 중 하나를 보여줄 때의 우선순위. 큰 값이 이긴다.
///
/// 화면이 **정보가 더 많은 쪽**을 봐야 한다 — 확정된 값이 있는데 "진행 중" 을
/// 보여주면 실행시간을 하한으로 읽게 된다.
fn read_rank(q: &SlowQuery) -> (u8, u8, u8, i64) {
    use dbmon_core::slow_query::{CaptureSource, SlowQueryState};
    let closed = match q.state {
        SlowQueryState::Finalized => 2,
        SlowQueryState::Abandoned => 1,
        SlowQueryState::InFlight => 0,
    };
    let exact = match q.capture_source {
        CaptureSource::Merged | CaptureSource::Slowlog => 1,
        _ => 0,
    };
    // **집계의 `rank` 와 같은 축이어야 한다.** 목록은 계획이 붙은 쌍둥이를 고르고
    // 상세는 다른 쪽을 고르면, 계획을 눌렀을 때 빈 상세가 나온다(8라운드 지적).
    let plan = u8::from(q.plan.has_plan());
    (closed, exact, plan, q.duration_ms)
}

/// SDK 오류를 도메인 오류로.
///
/// **`scrub()` 을 여기서 적용한다.** `SdkError` 의 `Debug` 에는 응답 본문과 메타데이터가
/// 전부 들어 있고, 호출부가 `Scrubbed` 로 감싸는 것을 한 곳만 잊어도 그게 CloudWatch 로
/// 나간다(2차 리뷰가 지적). 근원에서 막으면 호출부의 실수와 무관해진다.
pub(crate) fn map_sdk_err<E: std::fmt::Debug, R: std::fmt::Debug>(
    e: SdkError<E, R>,
) -> DomainError {
    DomainError::Unavailable {
        dependency: "dynamodb",
        reason: crate::telemetry::scrub(&format!("{e:?}")),
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
            // ① 같은 초 버킷의 **후보를 다 모아** 고칠 항목을 고른다.
            //    첫 일치를 쓰면 쌍둥이가 있을 때 **다른 항목**을 고치고, 고치려던
            //    레코드는 그대로 남는다(`candidates_by_record_id` 주석의 유령 사례).
            let candidates = self.candidates_by_record_id(&q.record_id).await?;
            let mut existing = pick_target(&candidates, q).cloned();

            // ② **±2초 보조 조회.** 없으면 같은 실행이 두 레코드로 갈린다.
            //
            // # 왜 필수인가
            //
            // 두 경로의 시작 시각 추정이 다르다:
            //
            // | 경로 | 시작 시각 | 정밀도 |
            // |---|---|---|
            // | 실시간 | `now − PROCESSLIST.TIME × 1000` | **정수 초** — 최대 1초 오차 |
            // | 슬로우로그 | `# Time − Query_time` | 밀리초 |
            //
            // `record_id` 는 시작 시각을 **초 버킷**으로 접으므로, 두 추정이 초 경계를
            // 사이에 두면 버킷이 갈린다 — 시작 시각의 밀리초가 균등분포면 **약 50%** 다.
            // 그러면 한 실행이 레코드 2건이 되고, 한쪽은 정확 지표만·다른 쪽은 플랜만
            // 갖는다. **F5 병합의 목적 자체가 달성되지 않는다.**
            //
            // ⚠ `find_merge_candidate` 는 이 문제를 위해 만들어졌는데(05 §8.2)
            // **프로덕션 호출부가 없었다.** 페이크 저장소에는 이 폴백이 있어서
            // 모든 단위 테스트가 통과했다 — 페이크와 실제의 계약이 갈린 상태였다.
            if existing.is_none() {
                existing = self
                    .find_merge_candidate_stored(
                        &q.instance_id,
                        q.thread_id,
                        &q.app_digest,
                        q.started_at_ms,
                        q.duration_ms,
                        dbmon_core::clock_offset::BASE_MERGE_WINDOW_MS,
                    )
                    .await?;
            }
            let merged = match &existing {
                // **`merge(existing, incoming)`** 순서를 지킨다. `record_id`·`literal_policy`
                // 는 "먼저 저장된 쪽 유지" 가 문서화된 의도다.
                Some(prev) => merge(&prev.record, q),
                None => q.clone(),
            };
            // **키는 기존 항목 자리를 유지한다.** 병합이 시작 시각을 앞당기면
            // 키가 이동해 조건부 쓰기가 깨진다(위 `to_item_keyed` 참고).
            let item = match &existing {
                // **읽어온 물리 키를 그대로 쓴다.** 필드로 다시 만들면 첫 병합 이후
                // 어긋나 조건부 쓰기가 영구히 실패한다(`to_item_at` 주석).
                Some(prev) => {
                    self.to_item_at(&merged, &prev.pk, &prev.sk, prev.rev.unwrap_or(0) + 1)?
                }
                None => self.to_item(&merged)?,
            };

            let mut put = self
                .client
                .put_item()
                .table_name(&self.table)
                .set_item(Some(item));
            put = match &existing {
                // 신규: 아직 없어야 한다.
                None => put.condition_expression("attribute_not_exists(PK)"),
                // 갱신: **우리가 읽은 그 판(rev)이어야 한다.** `SK = :sk` 만으로는
                // 항진명제에 가까워 동시 쓰기가 서로를 덮었다(`REV_ATTR` 주석).
                // 옛 항목에는 `rev` 가 없으므로 그 경우는 "없음" 을 조건으로 한다 —
                // 한 번 쓰이면 그 뒤로는 카운터 경로를 탄다.
                Some(prev) => match prev.rev {
                    Some(rev) => put
                        .condition_expression("rev = :rev")
                        .expression_attribute_values(":rev", AttributeValue::N(rev.to_string())),
                    None => put.condition_expression("attribute_not_exists(rev)"),
                },
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
        Ok(self.find_by_record_id(id).await?.map(|s| s.record))
    }

    /// `(instance, thread_id, ±window, app_digest)` 보조 조회 (포트 계약).
    ///
    /// 내부 갱신 경로는 물리 키가 필요하므로 [`Self::find_merge_candidate_stored`] 를
    /// 쓴다. 포트는 레코드만 약속하므로 여기서 벗겨 준다.
    async fn find_merge_candidate(
        &self,
        instance: &InstanceId,
        thread_id: u64,
        app_digest: &str,
        around_ms: dbmon_core::time::EpochMs,
        window_ms: i64,
    ) -> Result<Option<SlowQuery>> {
        Ok(self
            .find_merge_candidate_stored(
                instance, thread_id, app_digest, around_ms,
                // 포트 계약에는 지속시간이 없다. 겹침 판정을 통과시키려면 창만큼을
                // 준다 — 포트 호출부는 페이크 계약 확인용이고 실제 병합은 위 경로다.
                window_ms, window_ms,
            )
            .await?
            .map(|s| s.record))
    }

    /// 구간 안의 레코드를 **최신순으로** 돌려준다.
    ///
    /// # 두 가지를 지킨다
    ///
    /// **① 최신 파티션부터 본다.** `date_parts()` 는 오래된 순이므로 뒤집는다.
    /// 안 뒤집으면 `limit` 에 걸릴 때 **남는 것이 가장 오래된 날**이고, 목록·플랜·
    /// 집계는 전부 최근을 보려는 화면이다 — 조용히 옛날 데이터를 보여주게 된다.
    ///
    /// **② `LastEvaluatedKey` 를 따라간다.** DynamoDB `Query` 는 `Limit` 보다 적게
    /// 돌려줄 수 있다(1MB 페이지 상한). SQL 본문이 큰 레코드는 몇십 건에서 1MB 를
    /// 넘으므로, 페이지를 안 따라가면 "5000건 요청 → 80건 도착 → 이게 전부다" 로
    /// 오판한다. 호출부의 절단 판정(`found.len() == page`)도 그 위에서만 맞다.
    async fn list_by_instance(
        &self,
        instance: &InstanceId,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<SlowQuery>> {
        let mut out = Vec::new();
        for date in range.date_parts().into_iter().rev() {
            if out.len() >= limit {
                break;
            }
            let pk = format!("SQ#{}#{}", instance.as_str(), date.as_str());
            let mut start_key: Option<std::collections::HashMap<String, AttributeValue>> = None;
            loop {
                let remaining = limit - out.len();
                let res = self
                    .client
                    .query()
                    .table_name(&self.table)
                    .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
                    .expression_attribute_values(":pk", AttributeValue::S(pk.clone()))
                    .expression_attribute_values(
                        ":lo",
                        AttributeValue::S(sort_key_ms(range.from_ms())),
                    )
                    .expression_attribute_values(
                        ":hi",
                        AttributeValue::S(format!("{}#\u{10FFFF}", sort_key_ms(range.to_ms()))),
                    )
                    // 최신순.
                    .scan_index_forward(false)
                    .limit(remaining as i32)
                    .set_exclusive_start_key(start_key)
                    .send()
                    .await
                    .map_err(map_sdk_err)?;
                for item in res.items.unwrap_or_default() {
                    out.push(Self::from_item(item)?);
                }
                start_key = res.last_evaluated_key;
                // **끝난다.** `FilterExpression` 이 없으므로 `LastEvaluatedKey` 가
                // 있으면 그 페이지에 항목이 최소 하나 있었다 — `out` 이 매 회 자라고
                // `limit` 에 도달한다. 필터를 붙이면 이 보장이 깨지므로 그때는 페이지
                // 수 상한을 함께 둬야 한다.
                if start_key.is_none() || out.len() >= limit {
                    break;
                }
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// AP-18 — 희소 GSI1 로 진행 중 레코드를 조회한다 (고아 정리, F4).
    ///
    /// 오래된 것부터 본다. `GSI1SK` 가 `last_seen_at_ms` 이므로 오름차순이 곧 오래된 순이다.
    ///
    /// # 페이지를 따라간다
    ///
    /// DynamoDB 는 `Limit` 에 닿기 전에 **1MB 에서 페이지를 끊는다.** 진행 중
    /// 레코드는 SQL 원문을 들고 있어 몇 KB 씩 되므로 `limit=200` 을 줘도 한 페이지가
    /// 그보다 적게 돌아올 수 있다. 그 결과를 "진행 중인 것이 이게 전부" 로 읽으면
    /// 고아 정리와 일시정지 확정이 **조용히 일부만 처리한다.**
    async fn list_in_flight(&self, limit: usize) -> Result<Vec<SlowQuery>> {
        let mut out: Vec<SlowQuery> = Vec::new();
        let mut start_key: Option<std::collections::HashMap<String, AttributeValue>> = None;
        loop {
            let remaining = limit.saturating_sub(out.len());
            let res = self
                .client
                .query()
                .table_name(&self.table)
                .index_name("GSI1")
                .key_condition_expression("GSI1PK = :pk")
                .expression_attribute_values(
                    ":pk",
                    AttributeValue::S(keys::IN_FLIGHT_PK.to_string()),
                )
                .scan_index_forward(true)
                .limit(remaining as i32)
                .set_exclusive_start_key(start_key)
                .send()
                .await
                .map_err(map_sdk_err)?;
            for item in res.items.unwrap_or_default() {
                out.push(Self::from_item(item)?);
            }
            start_key = res.last_evaluated_key;
            if start_key.is_none() || out.len() >= limit {
                break;
            }
        }
        out.truncate(limit);
        Ok(out)
    }
}

/// 스캔을 금지하는 것은 IAM 이 하지만(`Deny dynamodb:Scan`), 코드에도 없어야 한다.
/// 이 상수는 그 사실을 테스트가 확인하는 데 쓴다.
#[allow(dead_code)]
const _NO_SCAN: () = ();

/// 앱이 실제로 쓰는 슬로우 쿼리 저장소 타입.
///
/// **DynamoDB 저장소를 그대로 쓰지 않는다** — 방송 래퍼를 거쳐야 실시간 화면에
/// 나타나기 때문이다. 이 별칭을 쓰면 감싸는 것을 잊을 수 없다.
pub type AppSlowQueryStore = broadcast::BroadcastingStore<DynamoSlowQueryStore>;

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

    /// **쌍둥이가 있을 때 "닫는 쓰기" 는 열려 있는 쪽을 고쳐야 한다.**
    ///
    /// 실측한 상태를 그대로 옮겼다: 같은 실행의 항목이 둘이고 하나는 확정, 하나는
    /// 진행 중이다. 고아 스윕이 진행 중인 쪽을 닫으려 하는데 확정된 쪽을 고치면
    /// 유령은 그대로 남고 스윕은 성공을 보고한다 — 30초마다 영구히 반복됐다.
    #[test]
    fn a_closing_write_targets_the_item_that_is_still_open() {
        let base = sample();
        let twin = super::Stored {
            record: {
                let mut q = base.clone();
                q.started_at_ms = 1_755_500_400_413;
                q.state = SlowQueryState::Finalized;
                q
            },
            pk: "SQ#i#2025-08-18".into(),
            sk: "1755500400413#8842119".into(),
            rev: Some(3),
        };
        let ghost = super::Stored {
            record: {
                let mut q = base.clone();
                q.started_at_ms = 1_755_500_400_416;
                q.state = SlowQueryState::InFlight;
                q
            },
            pk: "SQ#i#2025-08-18".into(),
            sk: "1755500400416#8842119".into(),
            rev: None, // 옛 항목처럼 카운터가 없는 경우도 섞는다
        };
        let candidates = vec![twin.clone(), ghost.clone()]; // 작은 SK 가 먼저 온다

        // ① 유령을 닫는 쓰기: 시작 시각이 유령과 같다 → 물리 SK 일치로 유령을 고른다.
        let mut closing = ghost.record.clone();
        closing.state = SlowQueryState::Abandoned;
        let picked = super::pick_target(&candidates, &closing).expect("후보가 있다");
        assert_eq!(
            picked.sk, ghost.sk,
            "확정된 쌍둥이를 고쳤다 — 유령이 남는다"
        );

        // ② 시작 시각이 병합으로 앞당겨져 물리 SK 와 어긋난 경우에도 열린 쪽을 고른다.
        let mut drifted = ghost.record.clone();
        drifted.started_at_ms = 1_755_500_400_413; // 필드만 앞당겨졌다
        drifted.state = SlowQueryState::Abandoned;
        let picked = super::pick_target(&candidates, &drifted).expect("후보가 있다");
        assert_eq!(
            picked.sk, ghost.sk,
            "닫는 쓰기가 이미 확정된 항목으로 갔다 — 유령이 영구히 남는다"
        );

        // ③ 진행 중 갱신은 자기 자리를 고친다(닫는 쓰기가 아니다).
        let live = ghost.record.clone();
        let picked = super::pick_target(&candidates, &live).expect("후보가 있다");
        assert_eq!(picked.sk, ghost.sk);

        // ④ 후보가 없으면 None — 신규 생성 경로로 간다.
        assert!(super::pick_target(&[], &live).is_none());
    }

    /// **±2초 병합도 구간이 겹칠 때만 같은 실행이다.**
    ///
    /// `long_query_time` 이 창(2초)보다 작으면 같은 스레드에서 같은 쿼리가 창 안에 두
    /// 번 시작할 수 있다. 그 둘을 합치면 **실행 하나가 영구히 사라진다** — 조회 쪽
    /// 접기는 이미 겹침을 보는데 저장소가 먼저 합쳐 버리면 볼 기회조차 없다(8라운드).
    #[test]
    fn the_merge_window_requires_overlapping_intervals() {
        let mut cand = sample();
        cand.started_at_ms = 1_000_000;
        cand.duration_ms = 1_200;
        cand.app_digest = "d1".into();

        // 앞 실행이 끝난 뒤 시작한 실행 → 같은 실행이 아니다.
        assert!(
            !super::same_execution(&cand, "d1", 1_001_300, 1_100),
            "연속한 두 실행을 같은 실행으로 봤다 — 하나가 사라진다"
        );
        // 도는 중에 관측된 것 → 같은 실행이다.
        assert!(super::same_execution(&cand, "d1", 1_000_500, 700));
        // 다이제스트가 다르면 아니다 (중첩 문장).
        assert!(!super::same_execution(&cand, "d2", 1_000_500, 700));
        // **자리표는 "없음" 이다** — 실시간 캡처가 SQL 을 못 얻은 경우.
        assert!(super::same_execution(
            &cand,
            "unknown-8842119",
            1_000_500,
            700
        ));
        let mut placeholder = cand.clone();
        placeholder.app_digest = "unknown-8842119".into();
        assert!(super::same_execution(&placeholder, "d1", 1_000_500, 700));
    }

    /// 조회는 **정보가 더 많은 쪽**을 보여줘야 한다. 확정값이 있는데 진행 중을
    /// 보여주면 실행시간을 하한으로 읽는다.
    #[test]
    fn a_read_prefers_the_row_that_saw_the_end() {
        let mut open = sample();
        open.state = SlowQueryState::InFlight;
        open.capture_source = CaptureSource::Processlist;
        open.duration_ms = 9_000;
        let mut closed = sample();
        closed.state = SlowQueryState::Finalized;
        closed.capture_source = CaptureSource::Merged;
        closed.duration_ms = 2_000;
        assert!(super::read_rank(&closed) > super::read_rank(&open));
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
