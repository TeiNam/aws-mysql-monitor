//! 인스턴스 등록부 어댑터 (M2-5) — [`InstanceRegistry`] 구현.
//!
//! # 단일 파티션이다
//!
//! `PK = "INST"`, `SK = "<region>#<instance_id>"` ([04 §7](../../../../docs/04-data-model.md)).
//! 500대 규모이므로 파티션 하나로 충분하고, **`Scan` 없이 전체 목록을 얻는 유일한 방법**
//! 이다(IAM 이 `dynamodb:Scan` 을 Deny 한다).
//!
//! `SK` 에 리전을 앞세우는 이유는 `begins_with(SK, "<region>#")` 로 리전별 조회를
//! 할 수 있게 하기 위해서다. `instance_id` 안에도 리전이 있지만 그건 정렬 키의
//! 접두가 아니라 중간이라 범위 조건으로 쓸 수 없다.
//!
//! # 탐색이 등록부를 지우지 않는다 (FR-DSC-07)
//!
//! 사라진 인스턴스는 `deleted_at_ms` 만 찍는다. 지우면 과거 슬로우 쿼리의 인스턴스
//! 메타 참조가 끊긴다 — "이 쿼리가 어느 인스턴스였나" 를 영구히 알 수 없게 된다.

use std::collections::HashMap;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{AttributeValue, ReturnValue};
use dbmon_core::error::{DomainError, Result};
use dbmon_core::ids::InstanceId;
use dbmon_core::instance::Instance;
use dbmon_core::ports::InstanceRegistry;
use dbmon_core::time::EpochMs;

use dbmon_core::instance::should_mark_deleted;

use super::map_sdk_err;

/// 등록부의 단일 파티션 키.
const PK: &str = "INST";

/// 사라진 인스턴스를 보존하는 기간 (일). FR-DSC-07.
///
/// 과거 슬로우 쿼리가 인스턴스 메타를 참조하므로 즉시 지우지 않는다. 30일 뒤에는
/// DynamoDB TTL 이 지운다 — 정리 잡을 따로 두면 그 잡이 죽었을 때 조용히 자란다.
const DELETED_RETENTION_DAYS: i64 = 30;

pub struct DynamoInstanceRegistry {
    client: Client,
    table: String,
}

impl DynamoInstanceRegistry {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }

    /// `<region>#<instance_id>`.
    ///
    /// 리전은 `InstanceId` 에서 뽑는다 — 호출자가 따로 넘기면 두 값이 어긋날 수 있다.
    fn sk(id: &InstanceId) -> String {
        format!("{}#{}", id.region(), id.as_str())
    }

    fn key(&self, id: &InstanceId) -> Vec<(&'static str, AttributeValue)> {
        vec![
            ("PK", AttributeValue::S(PK.to_string())),
            ("SK", AttributeValue::S(Self::sk(id))),
        ]
    }

    fn from_item(item: HashMap<String, AttributeValue>) -> Result<Instance> {
        serde_dynamo::from_item(item)
            .map_err(|e| DomainError::Internal(format!("인스턴스 역직렬화 실패: {e}")))
    }

    /// `deleted_at_ms` 를 찍는다. **지우지 않는다.**
    ///
    /// # ⚠ `if_not_exists` 를 쓰면 동작하지 않는다
    ///
    /// `serde_dynamo` 는 `Option::None` 을 **속성 부재가 아니라 `NULL` 타입으로** 저장한다.
    /// 그래서 `deleted_at_ms` 는 `None` 일 때도 `attribute_exists` 가 참이고,
    /// `if_not_exists(deleted_at_ms, :now)` 는 `NULL` 을 그대로 돌려준다 — 삭제 도장이
    /// **영구히 찍히지 않는다.** 처음 이렇게 썼고, DynamoDB Local 통합 테스트가 잡았다.
    /// 단위 테스트로는 볼 수 없는 부류다.
    ///
    /// 처음 사라진 시각을 유지하는 것은 호출부가 보장한다 —
    /// `mark_missing` 이 `deleted_at_ms.is_none()` 일 때만 이 함수를 부른다.
    async fn stamp_deleted(&self, id: &InstanceId, now_ms: EpochMs) -> Result<Instance> {
        let mut req = self.client.update_item().table_name(&self.table);
        for (k, v) in self.key(id) {
            req = req.key(k, v);
        }
        let out = req
            // **`ttl` 은 삭제 도장을 찍을 때만 넣는다** (FR-DSC-07 의 30일 보존).
            //
            // 살아 있는 인스턴스에 TTL 을 걸면 30일 뒤 등록부에서 사라진다. 그래서
            // `upsert` 는 `ttl` 을 쓰지 않고, 여기서만 쓴다 — 사라진 인스턴스는
            // 30일 뒤 DynamoDB 가 스스로 지운다. 정리 잡을 따로 만들 필요가 없다.
            .update_expression("SET deleted_at_ms = :now, #s = :st, #ttl = :ttl")
            // **항목이 없으면 만들지 않는다.** `mark_missing` 과 이 호출 사이에 항목이
            // 사라지면(콘솔 수동 삭제, TTL) PK/SK/deleted_at/state 만 있는 손상 항목이
            // 생기고, 그 하나가 `list()` 역직렬화를 깨뜨려 **모든 탐색 라운드가 영구히
            // 실패한다**(2차 리뷰가 지적한 CRITICAL).
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_names("#s", "state")
            .expression_attribute_names("#ttl", "ttl")
            .expression_attribute_values(
                ":ttl",
                AttributeValue::N(
                    (now_ms.div_euclid(1000) + DELETED_RETENTION_DAYS * 86_400).to_string(),
                ),
            )
            .expression_attribute_values(":now", AttributeValue::N(now_ms.to_string()))
            // **`as_str()` 을 쓰지 않는다.** 지금은 우연히 serde 표현과 같지만
            // (`snake_case` → `"deleted"`), 한쪽만 바뀌면 역직렬화가 조용히 깨진다.
            .expression_attribute_values(
                ":st",
                serde_dynamo::to_attribute_value(dbmon_core::instance::InstanceState::Deleted)
                    .map_err(|e| DomainError::Internal(format!("상태 직렬화 실패: {e}")))?,
            )
            .return_values(ReturnValue::AllNew)
            .send()
            .await;
        match out {
            Ok(o) => Self::from_item(o.attributes.unwrap_or_default()),
            // 그 사이에 항목이 사라졌다. 손상 항목을 만들지 않은 것이 성공이다.
            Err(e) if super::is_conditional_failure(&e) => Err(DomainError::NotFound {
                kind: "instance",
                id: id.as_str().to_string(),
            }),
            Err(e) => Err(map_sdk_err(e)),
        }
    }
}

#[async_trait::async_trait]
impl InstanceRegistry for DynamoInstanceRegistry {
    /// 전체 목록. **`Scan` 이 아니라 단일 파티션 `Query` 다.**
    ///
    /// 페이지네이션을 돈다 — 500대 × 항목 크기가 1MB 를 넘으면 첫 페이지만 받고
    /// 나머지를 "사라졌다" 로 판정하게 된다.
    async fn list(&self) -> Result<Vec<Instance>> {
        let mut out = Vec::new();
        let mut last: Option<HashMap<String, AttributeValue>> = None;
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
                // **항목 하나가 전체 목록을 죽이지 않는다.**
                //
                // `?` 로 두면 손상된 항목(스키마 변경 중 남은 것, 수동 편집) 하나가
                // `list()` 를 영구히 `Err` 로 만들고, `reconcile` 이 첫 줄에서 실패해
                // **탐색이 사람이 그 행을 지울 때까지 복구되지 않는다.**
                match Self::from_item(item) {
                    Ok(i) => out.push(i),
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "등록부 항목을 읽을 수 없다 — 건너뛴다 (수동 확인 필요)"
                        );
                    }
                }
            }
            match res.last_evaluated_key {
                Some(k) if !k.is_empty() => last = Some(k),
                _ => return Ok(out),
            }
        }
    }

    async fn get(&self, id: &InstanceId) -> Result<Option<Instance>> {
        let mut req = self.client.get_item().table_name(&self.table);
        for (k, v) in self.key(id) {
            req = req.key(k, v);
        }
        let res = req.send().await.map_err(map_sdk_err)?;
        match res.item {
            Some(item) => Ok(Some(Self::from_item(item)?)),
            None => Ok(None),
        }
    }

    /// 등록·갱신.
    ///
    /// ⚠ **탐색이 만든 레코드를 그대로 쓰면 사용자 오버라이드가 날아간다.**
    /// `env.override_value` 와 `first_seen_ms` 는 기존 값을 유지해야 한다 —
    /// 호출자가 `get` 후 병합해서 넘기는 것을 전제한다([`merge_discovered`]).
    async fn upsert(&self, instance: &Instance) -> Result<()> {
        let mut item: HashMap<String, AttributeValue> = serde_dynamo::to_item(instance)
            .map_err(|e| DomainError::Internal(format!("인스턴스 직렬화 실패: {e}")))?;
        item.insert("PK".into(), AttributeValue::S(PK.to_string()));
        item.insert("SK".into(), AttributeValue::S(Self::sk(&instance.id)));

        self.client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }

    /// 미발견 1회. **연속 2회일 때만** 삭제로 판정한다 (FR-DSC-07).
    ///
    /// 카운터를 `ADD` 로 올려 읽고-쓰기 경합을 피한다. 임계 판정은
    /// [`should_mark_deleted`] 하나만 쓴다 — 임계값을 두 곳에 적으면 한쪽만 바뀐다.
    async fn mark_missing(&self, id: &InstanceId, now_ms: EpochMs) -> Result<Instance> {
        let mut req = self.client.update_item().table_name(&self.table);
        for (k, v) in self.key(id) {
            req = req.key(k, v);
        }
        let out = req
            .update_expression("ADD missing_count :one")
            // 없는 인스턴스에 카운터만 있는 항목을 만들면 역직렬화가 깨진다.
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .return_values(ReturnValue::AllNew)
            .send()
            .await;

        let updated = match out {
            Ok(o) => Self::from_item(o.attributes.unwrap_or_default())?,
            Err(e) if super::is_conditional_failure(&e) => {
                return Err(DomainError::NotFound {
                    kind: "instance",
                    id: id.as_str().to_string(),
                });
            }
            Err(e) => return Err(map_sdk_err(e)),
        };

        if should_mark_deleted(updated.missing_count) && updated.deleted_at_ms.is_none() {
            return self.stamp_deleted(id, now_ms).await;
        }
        Ok(updated)
    }

    /// 다시 보였다 — 카운터를 0으로 되돌린다.
    ///
    /// **`deleted_at_ms` 도 지운다.** 인스턴스가 되살아났는데(정지 후 재시작) 삭제
    /// 도장이 남아 있으면 목록에서 계속 사라진 것으로 보인다.
    ///
    /// ⚠ **상태는 되돌리지 않는다.** `Deleted` → 무엇으로 갈지는 태그·버전을 봐야
    /// 판정할 수 있고 그건 [`merge_discovered`] 의 일이다. 탐색 루프는 재발견 시
    /// `upsert(merge_discovered(..))` 를 쓴다 — 태그·엔드포인트·버전도 함께 갱신해야
    /// 하므로 그쪽이 본 경로다. 이 함수는 그 외 호출자를 위한 최소 갱신이다.
    async fn mark_seen(&self, id: &InstanceId, now_ms: EpochMs) -> Result<()> {
        let mut req = self.client.update_item().table_name(&self.table);
        for (k, v) in self.key(id) {
            req = req.key(k, v);
        }
        // `REMOVE` 가 아니라 `NULL` 로 되돌린다. `serde_dynamo` 가 `None` 을 `NULL` 로
        // 쓰므로, 그렇게 해야 항목 모양이 `upsert` 가 쓴 것과 같아진다.
        // **`ttl` 도 지운다.** 삭제 도장을 찍을 때 걸어 둔 30일 TTL 이 남아 있으면
        // 되살아난 인스턴스가 30일 뒤 등록부에서 조용히 사라진다.
        req.update_expression(
            "SET missing_count = :zero, last_seen_ms = :now, deleted_at_ms = :null REMOVE #ttl",
        )
        .expression_attribute_names("#ttl", "ttl")
        .condition_expression("attribute_exists(PK)")
        .expression_attribute_values(":zero", AttributeValue::N("0".into()))
        .expression_attribute_values(":null", AttributeValue::Null(true))
        .expression_attribute_values(":now", AttributeValue::N(now_ms.to_string()))
        .send()
        .await
        .map_err(map_sdk_err)?;
        Ok(())
    }
}

/// 탐색 결과를 기존 등록부 레코드와 합친다.
///
/// # 사용자가 지정한 것은 탐색이 덮지 않는다 (FR-DSC-04)
///
/// | 필드 | 누가 이긴다 | 왜 |
/// |---|---|---|
/// | `env.override_value` | **기존** | "수동 지정은 태그 재수집에도 유지된다" |
/// | `first_seen_ms` | **기존** | 처음 본 시각은 바뀌지 않는다 |
/// | `state` | 조건부 | 자가진단이 올린 상태를 탐색이 `Pending` 으로 되돌리면 안 된다 |
/// | 나머지 | 탐색 | AWS 가 사실의 출처다 |
///
/// `state` 가 가장 미묘하다. 탐색은 `Pending`/`Unsupported`/`Disabled` 만 만든다.
/// 이미 `Collecting` 인 인스턴스를 매 탐색마다 `Pending` 으로 되돌리면
/// **수집이 5분마다 멈춘다** — `should_collect()` 가 `Pending` 을 제외하기 때문이다.
pub fn merge_discovered(existing: Option<&Instance>, discovered: &Instance) -> Instance {
    let Some(prev) = existing else {
        return discovered.clone();
    };

    let mut merged = discovered.clone();
    merged.first_seen_ms = prev.first_seen_ms;
    // 사용자 오버라이드를 유지하고, 태그에서 온 값만 갱신한다.
    merged.env =
        dbmon_core::env::EnvResolution::resolve(discovered.env.from_tags, prev.env.override_value);
    merged.renamed_from = prev.renamed_from.clone();
    merged.renamed_to = prev.renamed_to.clone();
    merged.state = pick_state(prev.state, discovered.state);
    merged
}

/// 탐색이 관측한 상태와 기존 상태 중 무엇을 쓸 것인가.
///
/// 탐색이 아는 것은 **태그·버전**뿐이다. 접속 가능성(`Unreachable`)이나 관측 소스
/// 상태(`Degraded`)는 수집 루프만 안다. 그래서:
///
/// - 탐색이 `Disabled`/`Unsupported` 라고 하면 그게 이긴다 (설정·버전이 근거다).
/// - 그 외에는 **기존 상태를 유지한다.** `Pending` 으로 되돌리면 수집이 멈춘다.
fn pick_state(
    prev: dbmon_core::instance::InstanceState,
    discovered: dbmon_core::instance::InstanceState,
) -> dbmon_core::instance::InstanceState {
    use dbmon_core::instance::InstanceState as S;
    match discovered {
        // 설정·버전·**RDS 상태**가 근거인 상태는 탐색이 이긴다.
        //
        // `Unreachable` 이 여기 있는 이유: RDS 가 `stopped` 라고 알려 주면 그건
        // 런타임 추측이 아니라 사실이다. 이게 없으면 정지된 인스턴스가 `Collecting`
        // 으로 남아 매초 접속을 시도한다.
        S::Disabled | S::Unsupported | S::Unreachable => discovered,
        // 기존이 `Disabled`/`Unsupported`/`Excluded` 였는데 그 사유가 사라졌으면
        // (태그 삭제, 버전 업그레이드, 필터 설정 수정) 다시 자가진단부터 시작해야 한다.
        //
        // ⚠ `Excluded` 를 빼먹으면 **한 번 제외된 인스턴스가 영구히 수집되지 않는다.**
        // 필터 설정을 고쳐도 돌아오지 않는다 — 새 상태를 넣으면서 이 목록을 갱신하지
        // 않아 실제로 그렇게 됐고, 테스트가 잡았다.
        _ if matches!(
            prev,
            S::Disabled | S::Unsupported | S::Excluded | S::Deleted
        ) =>
        {
            discovered
        }
        _ => prev,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::{Env, EnvResolution};
    use dbmon_core::instance::InstanceState as S;

    fn instance(state: S) -> Instance {
        let raw = crate::aws::discovery::RawDbInstance {
            identifier: "orders-01".into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: "ap-northeast-2".into(),
            // `is_collectible()` 는 엔드포인트를 요구한다. 없으면 상태와 무관하게 false 다.
            endpoint_address: Some("orders-01.abc.rds.amazonaws.com".into()),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        let mut i = crate::aws::discovery::to_instance(
            &raw,
            "123456789012",
            &dbmon_core::env::EnvMapping::default(),
            1_000,
        )
        .expect("매핑");
        i.state = state;
        i
    }

    #[test]
    fn sort_key_starts_with_the_region() {
        let i = instance(S::Pending);
        assert!(
            DynamoInstanceRegistry::sk(&i.id).starts_with("ap-northeast-2#"),
            "리전 접두가 없으면 리전별 조회를 할 수 없다"
        );
    }

    /// **사용자 오버라이드는 탐색이 덮지 않는다** (FR-DSC-04).
    #[test]
    fn discovery_never_overwrites_the_user_override() {
        let mut prev = instance(S::Collecting);
        prev.env = EnvResolution::resolve(Env::Unknown, Some(Env::Prd));

        let mut fresh = instance(S::Pending);
        fresh.env = EnvResolution::resolve(Env::Dev, None); // 태그가 새로 붙었다

        let m = merge_discovered(Some(&prev), &fresh);
        assert_eq!(
            m.env.override_value,
            Some(Env::Prd),
            "오버라이드가 날아갔다"
        );
        assert_eq!(m.env.effective, Env::Prd);
        assert_eq!(m.env.from_tags, Env::Dev, "태그 값은 갱신돼야 한다");
        assert!(m.env.is_conflicting(), "불일치 배지를 표시해야 한다");
    }

    /// **`Collecting` 을 `Pending` 으로 되돌리면 수집이 5분마다 멈춘다.**
    ///
    /// 탐색은 자가진단 결과를 모른다. 이 테스트가 그 회귀를 막는다.
    #[test]
    fn discovery_does_not_reset_a_collecting_instance_to_pending() {
        let prev = instance(S::Collecting);
        let fresh = instance(S::Pending);
        let m = merge_discovered(Some(&prev), &fresh);
        assert_eq!(
            m.state,
            S::Collecting,
            "탐색이 수집 중 인스턴스를 Pending 으로 되돌렸다 — 수집이 매 탐색마다 멈춘다"
        );
        assert!(m.is_collectible());
    }

    /// 접속 실패·부분 장애 상태도 탐색이 덮지 않는다.
    #[test]
    fn discovery_preserves_runtime_states() {
        for prev_state in [S::Unreachable, S::Degraded] {
            let m = merge_discovered(Some(&instance(prev_state)), &instance(S::Pending));
            assert_eq!(m.state, prev_state, "{prev_state:?} 를 덮었다");
        }
    }

    /// 사용자가 태그로 끄면 **그건 탐색이 이긴다.**
    #[test]
    fn tag_disable_wins_over_the_running_state() {
        let m = merge_discovered(Some(&instance(S::Collecting)), &instance(S::Disabled));
        assert_eq!(m.state, S::Disabled);

        let m = merge_discovered(Some(&instance(S::Collecting)), &instance(S::Unsupported));
        assert_eq!(m.state, S::Unsupported);
    }

    /// 끈 것을 다시 켜면 **자가진단부터** 다시 한다.
    #[test]
    fn re_enabling_goes_back_to_pending() {
        let m = merge_discovered(Some(&instance(S::Disabled)), &instance(S::Pending));
        assert_eq!(m.state, S::Pending, "자가진단을 건너뛰고 수집하면 안 된다");

        // 버전 업그레이드도 같다.
        let m = merge_discovered(Some(&instance(S::Unsupported)), &instance(S::Pending));
        assert_eq!(m.state, S::Pending);
    }

    /// 되살아난 인스턴스는 목록으로 돌아와야 한다.
    #[test]
    fn a_deleted_instance_that_reappears_restarts_diagnosis() {
        let mut prev = instance(S::Deleted);
        prev.deleted_at_ms = Some(500);
        prev.missing_count = 2;
        let m = merge_discovered(Some(&prev), &instance(S::Pending));
        assert_eq!(m.state, S::Pending);
        assert_eq!(
            m.deleted_at_ms, None,
            "삭제 도장이 남으면 목록에서 안 보인다"
        );
        assert_eq!(m.missing_count, 0);
    }

    #[test]
    fn first_seen_is_preserved() {
        let mut prev = instance(S::Collecting);
        prev.first_seen_ms = 1;
        let mut fresh = instance(S::Pending);
        fresh.first_seen_ms = 999;
        fresh.last_seen_ms = 999;
        let m = merge_discovered(Some(&prev), &fresh);
        assert_eq!(m.first_seen_ms, 1, "처음 본 시각이 탐색 시각으로 덮였다");
        assert_eq!(m.last_seen_ms, 999, "마지막 관측 시각은 갱신돼야 한다");
    }

    /// **이 파일에 `scan` 이 없어야 한다.** IAM 이 Deny 하므로 운영에서 처음 드러난다.
    #[test]
    fn registry_never_scans() {
        let src = include_str!("registry.rs");
        let code = src.split("#[cfg(test)]").next().expect("본문");
        assert!(!code.contains(".scan()"), "등록부가 Scan 을 쓴다");
    }
}
