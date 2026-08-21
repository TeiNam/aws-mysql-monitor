//! Bedrock Converse 어댑터 ([11 §7](../../../../docs/11-ai-advisor.md)).
//!
//! # 왜 tool-use 가 아니라 JSON 지시인가
//!
//! 11 §5.3 은 tool-use 로 스키마를 강제하기로 했다. 지금은 **system 프롬프트로 JSON 을
//! 지시하고 응답을 검증**한다:
//!
//! | 방식 | 얻는 것 | 드는 것 |
//! |---|---|---|
//! | tool-use | 모델이 형식을 벗어날 수 없다 | `serde_json → Document` 변환 + `tool_use` 블록 처리 + `stopReason` 분기 |
//! | JSON 지시 + 검증 | 코드가 절반 | 형식 위반을 우리가 감지해 1회 재시도 |
//!
//! **검증은 어느 쪽이든 필요하다** — tool-use 는 형식만 보장하고 "없는 테이블에 인덱스를
//! 걸라" 는 내용은 막지 못한다([`dbmon_core::tuning::validate`]). 형식 위반은 재시도로
//! 흡수되고, 두 번 실패하면 오류로 말한다(조용히 빈 권고를 만들지 않는다).
//!
//! 올라가는 길: 이 파일에서 `tool_config` 를 붙이고 응답의 `tool_use` 입력을 그대로
//! `RawAdvice` 로 역직렬화하면 된다. 호출부는 바뀌지 않는다.
//!
//! # `temperature` 를 보내지 않는다 (실측)
//!
//! 처음엔 0.2 를 보냈다 — 튜닝 권고는 창작이 아니니 결정적에 가까운 편이 낫다는
//! 판단이었다. 그런데 **Claude 5 계열이 그 파라미터를 거부한다**:
//!
//! ```text
//! ValidationException: `temperature` is deprecated for this model.
//! ```
//!
//! 모델 ID 는 설정으로 바뀌므로 "어떤 모델이 무엇을 받는가" 를 코드가 알 수 없다.
//! 그래서 **공통으로 받는 것만 보낸다**(`maxTokens`). 결정성은 프롬프트가 사실만 담고
//! 응답 스키마가 고정돼 있는 쪽으로 얻는다.

use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::ProvideErrorMetadata;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock,
};
use dbmon_core::error::{DomainError, Result};

/// 오류 메시지 길이 상한. 진단에 필요한 문장은 짧다.
const MAX_ERROR_CHARS: usize = 300;

pub struct BedrockClient {
    client: Client,
    region: String,
}

/// 한 번의 호출 결과. 토큰 사용량은 **로그·예산에 쓴다**(FR-AI-08 의 기반).
#[derive(Debug, Clone)]
pub struct ModelReply {
    pub text: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// `max_tokens` 에 걸려 잘렸는가. **잘린 JSON 은 파싱에 실패하므로** 사용자에게
    /// "상한을 올려라" 를 말할 수 있어야 한다.
    pub truncated: bool,
}

impl BedrockClient {
    pub fn new(client: Client, region: impl Into<String>) -> Self {
        Self {
            client,
            region: region.into(),
        }
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    /// system + user 한 쌍으로 한 번 부른다.
    pub async fn converse(
        &self,
        model_id: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<ModelReply> {
        let message = Message::builder()
            .role(ConversationRole::User)
            .content(ContentBlock::Text(user.to_string()))
            .build()
            .map_err(|e| DomainError::InvalidInput {
                field: "bedrock.message".into(),
                reason: format!("메시지 구성 실패: {e}"),
            })?;

        let out = self
            .client
            .converse()
            .model_id(model_id)
            .system(SystemContentBlock::Text(system.to_string()))
            .messages(message)
            .inference_config(
                InferenceConfiguration::builder()
                    .max_tokens(i32::try_from(max_tokens).unwrap_or(4_000))
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                // **서비스 메시지를 그대로 살린다.**
                //
                // 처음엔 `Debug` 를 스크럽해서 넘겼는데, 스크럽이 따옴표 안을 지우므로
                // 정작 필요한 문장이 `Some('?')` 가 됐다 — 실측으로
                // "`temperature` is deprecated for this model" 을 찾는 데 CLI 를 따로
                // 불러야 했다. 이 메시지는 **우리 요청의 형태**에 대한 진단이고
                // (모델 ID·파라미터·길이) 사용자 데이터가 아니므로 살려도 안전하다.
                // 길이는 자른다 — 응답 본문 전체가 로그로 흐르는 것은 막는다.
                let message = e
                    .as_service_error()
                    .and_then(ProvideErrorMetadata::message)
                    .map(|m| m.chars().take(MAX_ERROR_CHARS).collect::<String>())
                    .unwrap_or_else(|| crate::telemetry::scrub(&format!("{e:?}")));
                DomainError::Unavailable {
                    dependency: "bedrock",
                    reason: message,
                }
            })?;

        let truncated = out.stop_reason().as_str() == "max_tokens";
        let (input_tokens, output_tokens) = out
            .usage()
            .map(|u| (u.input_tokens(), u.output_tokens()))
            .unwrap_or((0, 0));

        let text = out
            .output()
            .and_then(|o| o.as_message().ok())
            .map(|m| {
                // 텍스트 블록이 여러 개일 수 있다 — 이어 붙인다. 하나만 쓰면
                // 긴 응답의 뒷부분이 조용히 사라진다.
                m.content()
                    .iter()
                    .filter_map(|c| c.as_text().ok())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        if text.trim().is_empty() {
            return Err(DomainError::Unavailable {
                dependency: "bedrock",
                reason: format!("모델이 빈 응답을 줬다 (stop_reason={})", out.stop_reason().as_str()),
            });
        }
        Ok(ModelReply {
            text,
            input_tokens,
            output_tokens,
            truncated,
        })
    }
}
