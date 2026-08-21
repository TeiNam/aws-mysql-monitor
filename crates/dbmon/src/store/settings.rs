//! DynamoDB 설정 어댑터 — `config_table` 의 `CFG/GLOBAL`
//! ([04 §3](../../../../docs/04-data-model.md)).
//!
//! # 문서 하나에 담는다
//!
//! 설정을 속성별로 쪼개 두면 화면이 부분 저장을 할 수 있지만, **부분 저장이 곧 부분
//! 적용**이다 — 리전을 바꾸다 실패하면 계정 목록만 저장된 상태가 남는다. 화면은 한 번에
//! 읽고 한 번에 쓰므로 JSON 문서 한 덩이가 맞다.
//!
//! # `version` 은 문서 **밖에도** 둔다
//!
//! 두 가지 때문이다:
//!
//! 1. 조건부 쓰기(`ConditionExpression`)가 속성으로 비교해야 한다 — JSON 안의 값은
//!    DynamoDB 가 읽지 못한다.
//! 2. **문서가 깨져도 버전은 읽을 수 있다.** 오류 메시지가 어느 버전이 깨졌는지
//!    말할 수 있고, 복구 저장(같은 버전으로 덮어쓰기)의 조건도 성립한다.
//!
//! # 손상된 문서는 오류다
//!
//! 처음에는 기본값으로 접었다(자기 치유 의도). 그건 캐시가 그 기본값을 "성공한 조회" 로
//! 기록해 마지막 값 보호를 우회하고, 화면이 빈 설정을 정상으로 보여주므로 관리자가
//! **저장을 누르면 실제 설정이 지워진다.** 오류로 올리면 화면이 사유를 표시한다.
//!
//! # 3계층 병합은 아직 없다
//!
//! 04 §3 은 `GLOBAL` → `ENV#<env>` → `INST#<id>` 병합을 규정한다. 지금 필요한 값들은
//! 전부 배포 전역(탐색 범위·알림 채널·모델)이라 `GLOBAL` 하나만 쓴다. SK 를 남겨 뒀으니
//! 계층이 필요해지면 항목을 더 읽어 병합하면 된다.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use dbmon_core::error::{DomainError, Result};
use dbmon_core::ports::SettingsStore;
use dbmon_core::settings::AppSettings;
use dbmon_core::time::EpochMs;

use super::{is_conditional_failure, map_sdk_err};

const PK: &str = "CFG";
const SK: &str = "GLOBAL";

pub struct DynamoSettingsStore {
    client: Client,
    table: String,
}

impl DynamoSettingsStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }
}

#[async_trait::async_trait]
impl SettingsStore for DynamoSettingsStore {
    async fn load(&self) -> Result<AppSettings> {
        let res = self
            .client
            .get_item()
            .table_name(&self.table)
            // **강한 일관성으로 읽는다.** 저장 직후 화면이 다시 읽는데 옛 값이 오면
            // "저장이 안 됐다" 로 보이고, 그 상태에서 다시 저장하면 버전이 어긋난다.
            .consistent_read(true)
            .key("PK", AttributeValue::S(PK.to_string()))
            .key("SK", AttributeValue::S(SK.to_string()))
            .send()
            .await
            .map_err(map_sdk_err)?;

        let Some(item) = res.item else {
            // **없는 것은 오류가 아니다.** 처음 뜬 배포다.
            return Ok(AppSettings::default());
        };
        let version = item
            .get("version")
            .and_then(|v| v.as_n().ok())
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0);
        let doc = item.get("doc").and_then(|v| v.as_s().ok());

        // **항목이 있는데 읽을 수 없으면 오류다.** 기본값으로 접지 않는다.
        //
        // 처음에는 기본값을 돌려줬다(자기 치유 의도). 그건 두 가지로 나쁘다
        // (교차 리뷰가 high 로 잡았다):
        //
        // 1. 캐시가 그 기본값을 "성공한 조회" 로 기록해 **마지막 값 보호를 우회**한다 —
        //    탐색 범위가 자기 리전 하나로 줄고, 인증이 토큰으로 되돌아간다.
        // 2. 화면이 빈 설정을 정상으로 보여주므로 관리자가 그 위에 **저장을 누르면
        //    실제 설정이 지워진다.**
        //
        // 오류로 올리면 화면이 사유를 표시하고, 워커는 마지막으로 읽은 값을 유지한다.
        let mut settings = match doc {
            Some(json) => serde_json::from_str::<AppSettings>(json).map_err(|e| {
                tracing::error!(
                    error = %crate::telemetry::Scrubbed(&e),
                    version,
                    "설정 문서를 읽지 못했다 — 기본값으로 접지 않는다"
                );
                DomainError::Internal(format!(
                    "설정 문서(version {version})를 읽을 수 없다 — 손상됐거나 형식이 바뀌었다"
                ))
            })?,
            None => {
                // 속성만 있고 문서가 없다. 이것도 정상 상태가 아니다.
                tracing::error!(version, "설정 항목에 doc 속성이 없다");
                return Err(DomainError::Internal(
                    "설정 항목에 문서가 없다".to_string(),
                ));
            }
        };
        // 문서 안의 버전보다 **속성이 권위값**이다. 조건부 쓰기가 그걸 보기 때문이다.
        settings.version = version;
        Ok(settings)
    }

    async fn save(
        &self,
        settings: &AppSettings,
        expected_version: u32,
        by: &str,
        now_ms: EpochMs,
    ) -> Result<AppSettings> {
        // **포화를 오류로 만든다.** `saturating_add` 로 두면 상한에서 여러 저장이 같은
        // 버전으로 성공하고, 그러면 늦게 도착한 응답이 캐시를 되돌릴 수 있다
        // (5차 교차 리뷰가 low 로 잡았다 — 42억 번 저장해야 닿지만 조용히 깨진다).
        let Some(next_version) = expected_version.checked_add(1) else {
            return Err(DomainError::Conflict(
                "설정 버전이 상한에 닿았다 — 항목을 새로 만들어야 한다".to_string(),
            ));
        };
        let mut next = settings.clone();
        next.version = next_version;
        next.updated_at_ms = now_ms;
        next.updated_by = by.to_string();

        let doc = serde_json::to_string(&next).map_err(|e| DomainError::InvalidInput {
            field: "settings".into(),
            reason: format!("직렬화 실패: {e}"),
        })?;

        let mut req = self
            .client
            .put_item()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(PK.to_string()))
            .item("SK", AttributeValue::S(SK.to_string()))
            .item("version", AttributeValue::N(next.version.to_string()))
            .item("doc", AttributeValue::S(doc))
            .item("updated_at_ms", AttributeValue::N(now_ms.to_string()))
            .item("updated_by", AttributeValue::S(by.to_string()));

        // **버전이 맞을 때만 쓴다.** `expected_version = 0` 은 "항목이 없다" 는 뜻이라
        // 두 조건을 OR 로 묶는다 — 첫 저장에서도 조건이 성립해야 한다.
        req = if expected_version == 0 {
            req.condition_expression("attribute_not_exists(version) OR version = :expected")
                .expression_attribute_values(":expected", AttributeValue::N("0".into()))
        } else {
            req.condition_expression("version = :expected")
                .expression_attribute_values(
                    ":expected",
                    AttributeValue::N(expected_version.to_string()),
                )
        };

        match req.send().await {
            Ok(_) => Ok(next),
            Err(e) if is_conditional_failure(&e) => Err(DomainError::Conflict(format!(
                "설정이 그 사이에 바뀌었다 (내가 읽은 버전 {expected_version}) — 다시 읽고 저장한다"
            ))),
            Err(e) => Err(map_sdk_err(e)),
        }
    }
}
