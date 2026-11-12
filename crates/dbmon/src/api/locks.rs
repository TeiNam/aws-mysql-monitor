//! `GET /api/instances/{id}/locks` — 지금 누가 누구를 막고 있나.
//!
//! **읽기만 한다.** 대상 DB 에 붙지만 상태를 바꾸지 않으므로 제어 헤더를 요구하지 않는다.
//! 대신 서비스가 인스턴스마다 한 연결만 쓰고 결과를 짧게 다시 써서, 화면이 몰려도 대상 DB 에
//! 연결이 쏟아지지 않는다(`crate::locks`).
//!
//! 권한은 인스턴스 조회와 같다 — 스코프 밖이면 존재를 알려주지 않는다(404). SQL 이 보이는지는
//! 슬로우 쿼리 SQL 과 같은 규칙이다([`view::sql_hidden_reason`]).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use dbmon_core::instance::Instance;
use dbmon_core::pause::PauseSet;
use dbmon_core::ports::InstanceRegistry as _;
use dbmon_core::time::{Clock, SystemClock};

use super::{ApiError, ApiState, context_of, view};
use crate::locks::{LockFailure, LockSnapshot};

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
    let pause = state.pause.load(SystemClock.now_ms()).await;
    if let Some(code) = refusal(&found, &pause) {
        return Err(ApiError::new(StatusCode::CONFLICT, code));
    }
    // **스텁으로 통과시키지 않는다.** 자격증명이 없는 구성이면 화면이 사유를 말한다.
    let Some(svc) = state.locks.as_ref() else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "locks_unavailable",
        ));
    };

    let snap = svc.snapshot(&found).await.map_err(|f| match f {
        // **4xx 로 준다** — GRANT·로그인을 고쳐야 하는 일이라 화면이 재시도해도 낫지 않는다.
        LockFailure::Forbidden => ApiError::new(StatusCode::FAILED_DEPENDENCY, "target_forbidden"),
        LockFailure::Unavailable => ApiError::new(StatusCode::BAD_GATEWAY, "target_unavailable"),
        LockFailure::Timeout => ApiError::new(StatusCode::GATEWAY_TIMEOUT, "target_timeout"),
    })?;
    // 캐시는 사람을 가리지 않는다 — 이 사람에게 보일 수 없는 SQL 은 여기서 뺀다.
    Ok(Json(
        match view::sql_hidden_reason(svc.literal_policy(), &ctx) {
            Some(reason) => snap.without_sql(reason),
            None => (*snap).clone(),
        },
    ))
}

/// **수집하지 않는 인스턴스에는 붙지 않는다.** 제외·삭제된 대상에는 모니터링 계정이 없을 수
/// 있고(반복되는 인증 실패는 대상 쪽 보안 경보가 된다), 정지는 사람이 "이 DB 에서 손을 떼라" 고
/// 한 것이다 — 락 탭이 그 결정을 뒤집으면 안 된다.
fn refusal(instance: &Instance, pause: &PauseSet) -> Option<&'static str> {
    if !instance.is_collectible() {
        Some("instance_not_collectible")
    } else if pause.is_paused(instance) {
        Some("instance_paused")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::EnvMapping;
    use dbmon_core::instance::InstanceState;
    use dbmon_core::pause::PauseScope;

    fn instance(state: InstanceState) -> Instance {
        let raw = crate::aws::discovery::RawDbInstance {
            identifier: "orders-db".into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: "ap-northeast-2".into(),
            endpoint_address: Some("orders-db.rds.amazonaws.com".into()),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        let mut i =
            crate::aws::discovery::to_instance(&raw, "123456789012", &EnvMapping::default(), 1_000)
                .expect("매핑");
        i.state = state;
        i
    }

    #[test]
    fn only_collected_unpaused_instances_are_queried() {
        let live = instance(InstanceState::Collecting);
        assert_eq!(refusal(&live, &PauseSet::default()), None);

        for state in [InstanceState::Disabled, InstanceState::Excluded] {
            assert_eq!(
                refusal(&instance(state), &PauseSet::default()),
                Some("instance_not_collectible"),
                "{state:?}"
            );
        }
        let mut deleted = instance(InstanceState::Collecting);
        deleted.deleted_at_ms = Some(2_000);
        assert_eq!(
            refusal(&deleted, &PauseSet::default()),
            Some("instance_not_collectible")
        );

        // 인스턴스·환경·전체 — 어느 스코프로 멈췄든 붙지 않는다.
        for scope in [
            PauseScope::Instance(live.id.clone()),
            PauseScope::Env(live.env.effective),
            PauseScope::All,
        ] {
            let paused = PauseSet::from_entries([(scope.as_key(), 1_000)]);
            assert_eq!(
                refusal(&live, &paused),
                Some("instance_paused"),
                "{scope:?}"
            );
        }
    }
}
