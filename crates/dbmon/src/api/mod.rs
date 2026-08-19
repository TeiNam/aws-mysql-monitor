//! 조회 API (M6, [13](../../../../docs/13-api-spec.md)).
//!
//! # 무엇이 있고 무엇이 없는가
//!
//! | 엔드포인트 | 상태 |
//! |---|---|
//! | `GET /api/slow-queries` | 핫 티어(DynamoDB) |
//! | `GET /api/queries/{id}` | 있음 |
//! | `GET /api/auth/config` | 있음 (프론트가 로그인 방식을 알아야 한다) |
//! | 콜드 티어(Athena) 페이지네이션 | **없음** — 아카이브 경로가 아직 없다 |
//! | `POST /api/queries` (샘플 실행) | **없음** |
//! | 메트릭·부트스트랩·설정 | **없음** |
//!
//! 없는 것은 라우트를 만들지 않는다. 404 가 "아직 없다" 를 정확히 말한다 —
//! 빈 응답을 주면 "데이터가 없다" 로 오해된다.
//!
//! # 인증은 우회할 수 없다
//!
//! 모든 핸들러가 [`auth::authenticate`] 를 지난다. `/healthz`·`/readyz` 는 이
//! 라우터에 없다(T-01 이 그 둘만 예외로 둔다).

pub mod auth;
pub mod cursor;
pub mod hub;
pub mod topic;
pub mod view;
pub mod ws;

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use dbmon_core::env::Env;
use dbmon_core::ports::SlowQueryStore;
use dbmon_core::rbac::AuthContext;
use dbmon_core::time::{Clock, SystemClock, TimeRange};

use crate::store::AppSlowQueryStore;
use auth::{AuthPolicy, authenticate};
use view::{ListResponse, SlowQueryView};

/// 목록 조회 상한. 클라이언트가 더 요구해도 여기서 잘린다.
const MAX_LIMIT: usize = 200;
const DEFAULT_LIMIT: usize = 50;

/// 기본 조회 구간 (24시간). 시간 범위를 주지 않았을 때.
const DEFAULT_RANGE_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<AppSlowQueryStore>,
    pub registry: Arc<crate::store::registry::DynamoInstanceRegistry>,
    pub policy: AuthPolicy,
    /// 커서 서명 키. **프로세스마다 다르다** — 재시작하면 기존 커서가 무효해진다.
    ///
    /// 다중 워커에서 커서를 공유하려면 공용 비밀이 필요하다(M6 의 남은 작업).
    /// 지금은 커서가 한 워커에만 유효하고, 그게 조용한 오동작이 아니라 명시적
    /// `invalid_cursor` 로 나타난다.
    pub cursor_key: Arc<Vec<u8>>,
    /// 실시간 방송 허브. 수집 경로가 여기에 넣고 WS 가 여기서 꺼낸다.
    pub hub: hub::Hub,
}

pub fn router(state: ApiState) -> axum::Router {
    axum::Router::new()
        .route("/api/auth/config", get(auth_config))
        .route("/api/slow-queries", get(list_slow_queries))
        .route("/api/queries/{id}", get(get_query))
        .route("/api/instances", get(list_instances))
        .route("/api/ws", get(ws::handler))
        .with_state(state)
}

/// API 오류. **내부 사정을 노출하지 않는다.**
struct ApiError {
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.code }))).into_response()
    }
}

/// `Authorization: Bearer …` 에서 토큰을 뽑고 인증한다.
fn context_of(state: &ApiState, headers: &HeaderMap) -> Result<AuthContext, ApiError> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());

    authenticate(&state.policy, bearer).map_err(|e| ApiError::new(e.status(), "unauthorized"))
}

/// 프론트가 로그인 방식을 알기 위한 엔드포인트.
///
/// **이것만 인증 없이 답한다** — 로그인하기 전에 물어야 하는 정보다.
/// 비밀은 담지 않는다(클라이언트 ID·도메인은 공개 값이다).
async fn auth_config(State(state): State<ApiState>) -> Json<serde_json::Value> {
    // **토큰 값을 담지 않는다.** 인증 전에 답하는 엔드포인트이므로 여기에
    // 토큰을 실으면 인증이 없는 것과 같다. 화면은 URL 이나 로그에서 받는다.
    let mode = if state.policy.allows_local_bypass() {
        "local-dev"
    } else if state.policy.dev_token.is_some() {
        "local-token"
    } else {
        "cognito"
    };
    Json(serde_json::json!({
        // Cognito 가 배선되면 issuer·client_id·authorize_url 이 여기 온다.
        "mode": mode,
        "cognito_configured": false,
        "deployment_env": state.policy.deployment_env.as_str(),
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct ListParams {
    limit: Option<usize>,
    cursor: Option<String>,
    /// 인스턴스 id. 없으면 등록부 전체를 훑는다.
    instance: Option<String>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    env: Option<String>,
}

async fn list_slow_queries(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> Result<Json<ListResponse>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let now_ms = SystemClock.now_ms();

    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let to_ms = p.to_ms.unwrap_or(now_ms);
    let from_ms = p.from_ms.unwrap_or(to_ms - DEFAULT_RANGE_MS);
    let range = TimeRange::new(from_ms, to_ms)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid_range"))?;

    // **환경 스코프를 여기서 교집합한다.** 요청이 무엇을 요구하든 사용자의
    // 스코프 밖은 볼 수 없다.
    let requested_envs: Vec<Env> = match p.env.as_deref() {
        Some(e) => vec![
            parse_env(e).ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid_env"))?,
        ],
        None => Env::ALL.to_vec(),
    };
    let allowed_envs = ctx.scope_intersection(&requested_envs);
    if allowed_envs.is_empty() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "env_not_allowed"));
    }

    // **커서를 검증한다.** 필터는 요청에서 다시 유도해 해시를 비교한다 —
    // 커서에 담긴 필터를 그대로 쓰면 위 스코프 검사가 건너뛰어진다.
    let filters = cursor::filters_hash(&[
        ("instance", p.instance.as_deref().unwrap_or("")),
        ("from", &from_ms.to_string()),
        ("to", &to_ms.to_string()),
        ("env", p.env.as_deref().unwrap_or("")),
        ("limit", &limit.to_string()),
    ]);
    if let Some(raw) = p.cursor.as_deref() {
        cursor::Cursor::decode(raw, &state.cursor_key, &ctx.subject, &filters, now_ms)
            .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_cursor"))?;
        // 위치를 실제 페이지네이션에 쓰는 것은 M6 의 남은 작업이다 —
        // `list_by_instance` 가 `LastEvaluatedKey` 를 아직 노출하지 않는다.
    }

    // 대상 인스턴스를 정한다.
    let instances = self_instances(&state, p.instance.as_deref()).await?;

    let mut items = Vec::new();
    for inst in &instances {
        if !allowed_envs.contains(&inst.env.effective) {
            continue;
        }
        let found = state
            .store
            .list_by_instance(&inst.id, range, limit)
            .await
            .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?;
        for q in &found {
            // **뷰를 반드시 거친다.** 리터럴 통제가 그 안에 있다.
            items.push(SlowQueryView::from_record(q, &ctx));
        }
        if items.len() >= limit {
            break;
        }
    }
    // 최신순으로 맞춘다 — 인스턴스별 조회를 이어 붙였으므로 전역 정렬이 필요하다.
    items.sort_by_key(|v| std::cmp::Reverse(v.started_at_ms));
    let has_more = items.len() > limit;
    items.truncate(limit);

    Ok(Json(ListResponse {
        items,
        // 페이지네이션 위치가 아직 없으므로 커서를 발급하지 않는다.
        // **빈 커서를 주고 무한 루프를 만들지 않는다.**
        next_cursor: None,
        has_more,
    }))
}

async fn get_query(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SlowQueryView>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    // `TryFrom<String>` 이 형식을 검증한다 — 임의 문자열로 키를 만들 수 없다.
    let record_id = dbmon_core::ids::RecordId::try_from(id)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_record_id"))?;

    let found = state
        .store
        .get(&record_id)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found"))?;

    // **환경 스코프를 상세 조회에도 적용한다.** 목록에서 걸러도 id 를 알면
    // 직접 조회할 수 있으므로, 여기서 다시 확인해야 한다.
    if !ctx.is_env_allowed(found.env) {
        // 존재 여부를 알리지 않는다.
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
    }
    Ok(Json(SlowQueryView::from_record(&found, &ctx)))
}

#[derive(Debug, serde::Serialize)]
struct InstanceView {
    id: String,
    env: String,
    state: String,
    engine: String,
    engine_version: String,
    endpoint: Option<String>,
    collectible: bool,
}

async fn list_instances(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Vec<InstanceView>>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let all = dbmon_core::ports::InstanceRegistry::list(&*state.registry)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?;

    Ok(Json(
        all.iter()
            .filter(|i| ctx.is_env_allowed(i.env.effective))
            .map(|i| InstanceView {
                id: i.id.as_str().to_string(),
                env: i.env.effective.as_str().to_string(),
                state: i.state.as_str().to_string(),
                engine: format!("{:?}", i.engine).to_lowercase(),
                engine_version: i.engine_version.raw.clone(),
                endpoint: i.endpoint.clone(),
                collectible: i.is_collectible(),
            })
            .collect(),
    ))
}

/// 조회 대상 인스턴스를 정한다.
async fn self_instances(
    state: &ApiState,
    requested: Option<&str>,
) -> Result<Vec<dbmon_core::instance::Instance>, ApiError> {
    let all = dbmon_core::ports::InstanceRegistry::list(&*state.registry)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?;
    match requested {
        Some(want) => Ok(all.into_iter().filter(|i| i.id.as_str() == want).collect()),
        None => Ok(all),
    }
}

fn parse_env(s: &str) -> Option<Env> {
    Env::ALL.iter().copied().find(|e| e.as_str() == s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_clamped_to_a_sane_range() {
        // 상한을 넘겨도 잘린다 — 클라이언트가 1만 건을 요구할 수 없다.
        assert_eq!(usize::MAX.clamp(1, MAX_LIMIT), MAX_LIMIT);
        assert_eq!(0usize.clamp(1, MAX_LIMIT), 1);
    }

    #[test]
    fn env_parsing_rejects_unknown_values() {
        assert_eq!(parse_env("prd"), Some(Env::Prd));
        assert_eq!(parse_env("dev"), Some(Env::Dev));
        assert_eq!(
            parse_env("PRD"),
            None,
            "대소문자를 관용하면 스코프가 흐려진다"
        );
        assert_eq!(parse_env("'; DROP"), None);
    }
}
