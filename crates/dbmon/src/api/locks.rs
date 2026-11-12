//! `GET /api/instances/{id}/locks` — 지금 누가 누구를 막고 있나.
//!
//! **읽기만 한다.** 대상 DB 에 붙지만 상태를 바꾸지 않으므로 제어 헤더를 요구하지 않는다.
//! 대신 서비스가 짧게 캐시해 화면이 몰아쳐도 대상 DB 에 연결이 쏟아지지 않는다.
//!
//! 권한은 인스턴스 조회와 같다 — 스코프 밖이면 존재를 알려주지 않는다(404). SQL 은 언제나
//! 마스킹된 정규 텍스트라 역할로 가릴 것이 없다(`crate::locks`).

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use dbmon_core::error::DomainError;
use dbmon_core::ports::InstanceRegistry as _;
use dbmon_core::time::{Clock, SystemClock};

use super::{ApiError, ApiState, context_of};
use crate::locks::LockSnapshot;

/// 요청 전체 상한. 연결 수립(TLS + IAM 토큰, 최대 5초) + 조회 셋.
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);

pub async fn get_locks(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<LockSnapshot>, ApiError> {
    let ctx = context_of(&state, &headers).await?;
    let instance_id = dbmon_core::ids::InstanceId::parse(&id)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_instance_id"))?;
    let found = state
        .registry
        .get(&instance_id)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "instance_not_found"))?;
    if !ctx.is_env_allowed(found.env.effective) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "instance_not_found"));
    }
    // **스텁으로 통과시키지 않는다.** 자격증명이 없는 구성이면 화면이 사유를 말한다.
    let Some(svc) = state.locks.as_ref() else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "locks_unavailable",
        ));
    };

    match tokio::time::timeout(REQUEST_DEADLINE, svc.snapshot(&found, SystemClock.now_ms())).await {
        Ok(Ok(snap)) => Ok(Json((*snap).clone())),
        Ok(Err(e)) => {
            // 원인은 로그로만 — 서버 오류 메시지에는 문장 조각이 들어 있다.
            tracing::warn!(
                instance = %found.id.as_str(),
                error = %crate::telemetry::Scrubbed(&e),
                "락 스냅샷 조회 실패"
            );
            let code = match e {
                // GRANT 를 고쳐야 하는 경우를 구분한다 — 재시도로는 낫지 않는다.
                DomainError::Forbidden { .. } => "target_forbidden",
                _ => "target_unavailable",
            };
            Err(ApiError::new(StatusCode::BAD_GATEWAY, code))
        }
        Err(_) => Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "target_timeout")),
    }
}
