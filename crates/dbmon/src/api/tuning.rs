//! 튜닝 권고 API.
//!
//! | 메서드 | 권한 | 하는 일 |
//! |---|---|---|
//! | `GET /api/queries/{id}/tuning` | viewer | 저장된 권고 (없으면 `advice: null`) |
//! | `POST /api/queries/{id}/tuning` | **operator** | 새로 만든다 (모델 호출) |
//!
//! # 왜 생성은 operator 인가
//!
//! 대상 DB 에 쿼리를 던지고 토큰을 쓴다 — **비용과 부하가 발생하는 조작**이다.
//! 조회는 이미 만들어진 결과를 읽는 것이므로 viewer 도 본다.
//!
//! # 왜 GET 이 만들지 않는가
//!
//! 화면이 열릴 때마다 새로 만들면 (a) 청구서가 화면 방문 수에 비례하고 (b) 새로고침이
//! 곧 재생성이 된다. 버튼을 누르는 것과 화면을 보는 것은 다른 의도다.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use dbmon_core::error::DomainError;
use dbmon_core::rbac::Role;
use dbmon_core::time::{Clock, SystemClock};
use dbmon_core::tuning::TuningAdvice;

use super::{ApiError, ApiState, context_of, record_for, require_control_header};

#[derive(Debug, serde::Serialize)]
pub struct TuningView {
    /// 없으면 `null` — **빈 객체를 주지 않는다.** 화면이 "권고가 비어 있다" 와
    /// "아직 만들지 않았다" 를 구분해야 한다.
    pub advice: Option<TuningAdvice>,
    /// 이 배포에서 생성이 가능한가(설정이 켜져 있고 모델이 지정됐는가).
    pub enabled: bool,
    /// 이 사용자가 생성할 수 있는가(operator 이상).
    pub can_generate: bool,
    /// 설정된 모델. 화면이 "무엇으로 분석하는가" 를 보여준다.
    pub model_id: String,
    /// 저장된 권고를 **읽지 못했다**. `advice: null` 과 구분해야 한다 —
    /// 전자는 "고장", 후자는 "아직 안 만들었다" 다.
    pub read_failed: bool,
}

/// 저장된 권고를 읽는다.
pub async fn get_tuning(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<TuningView>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    // **레코드를 먼저 확인한다** — 환경 스코프 밖이면 권고의 존재도 알리지 않는다.
    let record = record_for(&state, &ctx, id).await?;
    let now_ms = SystemClock.now_ms();
    let settings = state.settings.load(now_ms).await;

    let (advice, read_failed) = match &state.tuning {
        Some(svc) => match svc.stored(&record.record_id).await {
            Ok(a) => (a, false),
            Err(e) => {
                tracing::warn!(
                    error = %crate::telemetry::Scrubbed(&e),
                    "저장된 튜닝 권고를 읽지 못했다"
                );
                (None, true)
            }
        },
        None => (None, false),
    };
    Ok(Json(TuningView {
        advice,
        read_failed,
        enabled: state.tuning.is_some()
            && settings.ai.enabled
            && !settings.ai.model_id.trim().is_empty(),
        can_generate: ctx.has_role(Role::Operator),
        model_id: settings.ai.model_id,
    }))
}

/// 새로 만든다. **동기 응답이다** — 모델 호출이 수십 초 걸릴 수 있다.
///
/// 비동기 잡으로 만들면 진행 상태 저장·폴링·정리가 따라온다. 사람이 버튼을 누르고
/// 기다리는 흐름에서는 요청 하나가 정직하고, 실패 사유도 그 자리에서 보인다.
pub async fn post_tuning(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<TuningView>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    require_control_header(&headers)?;
    if !ctx.has_role(Role::Operator) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "operator_required"));
    }
    let record = record_for(&state, &ctx, id).await?;
    let Some(svc) = state.tuning.as_ref() else {
        // 이 워커가 대상 DB·모델에 닿을 수 없는 구성이다(자격증명이 없다).
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "tuning_unavailable",
        ));
    };
    let now_ms = SystemClock.now_ms();

    let generated = svc.generate(&record, now_ms).await.map_err(|e| {
        // **사유를 구분해 코드로 준다.** "실패했다" 하나로는 사용자가 무엇을 고칠지
        // 알 수 없다 — 설정을 켜는 일과 권한을 얻는 일은 완전히 다르다.
        let (status, code, detail) = match &e {
            DomainError::Unsupported { reason, .. } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "ai_disabled",
                Some(reason.clone()),
            ),
            // **모델·설정 진단은 화면까지 올린다.**
            //
            // `model_failed` 만 주면 운영자가 로그를 봐야 한다. 이 사유는 우리 요청의
            // 형태에 대한 것이고(모델 ID 오타, 리전에 없는 모델, 거부된 파라미터)
            // 고칠 사람이 그 화면에 있다 — 실측: "`temperature` is deprecated for this
            // model" 을 찾는 데 CLI 를 따로 불러야 했다.
            DomainError::Unavailable { dependency, reason } if *dependency == "bedrock" => (
                StatusCode::BAD_GATEWAY,
                "model_failed",
                Some(reason.clone()),
            ),
            _ => (StatusCode::BAD_GATEWAY, "tuning_failed", None),
        };
        tracing::warn!(
            subject = %ctx.subject,
            record = %record.record_id.as_str(),
            error = %crate::telemetry::Scrubbed(&e),
            code,
            "튜닝 권고 생성 실패"
        );
        match detail {
            Some(reason) => ApiError::with_body(
                status,
                code,
                serde_json::json!({ "reason": reason }),
            ),
            None => ApiError::new(status, code),
        }
    })?;

    tracing::info!(
        subject = %ctx.subject,
        record = %record.record_id.as_str(),
        input_tokens = generated.input_tokens,
        output_tokens = generated.output_tokens,
        "튜닝 권고를 만들었다"
    );
    let settings = state.settings.load(now_ms).await;
    Ok(Json(TuningView {
        advice: Some(generated.advice),
        enabled: true,
        can_generate: true,
        model_id: settings.ai.model_id,
        read_failed: false,
    }))
}
