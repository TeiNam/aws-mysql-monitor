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

pub mod aggregate;
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
const MAX_LIMIT: usize = 500;
const DEFAULT_LIMIT: usize = 50;

/// 집계(다이제스트·통계)가 한 번에 접을 레코드 상한.
///
/// **천장을 숨기지 않는다.** 여기에 걸리면 응답이 `truncated=true` 로 말하고 화면이
/// 그걸 표시한다 — 조용히 자르면 "그만큼만 실행됐다" 로 읽힌다. 월 단위 대량
/// 집계가 필요해지면 M12 롤업(수집 시점 스냅샷)으로 바꾼다.
const MAX_AGGREGATE_SCAN: usize = 20_000;

/// 인스턴스 하나에서 한 번에 읽을 레코드 수. 집계는 인스턴스별로 이만큼씩 모은다.
const AGGREGATE_PAGE: usize = 5_000;

/// 기본 조회 구간 (24시간). 시간 범위를 주지 않았을 때.
const DEFAULT_RANGE_MS: i64 = 24 * 60 * 60 * 1000;

/// 조회 구간 상한 (400일 — 아카이브 티어와 같다).
///
/// **없으면 인증된 사용자가 저장소를 때릴 수 있다.** `from_ms=0` 을 주면
/// `date_parts()` 가 상한(1만)까지 날짜를 만들고, 그건 인스턴스마다 1만 번의
/// DynamoDB 질의다. 게다가 그 1만 개는 **1970년부터**이므로 최근 데이터가 통째로
/// 빠진다 — 비싸고 틀린 응답이다. 구간을 넘기면 조용히 좁히지 않고 거부한다.
const MAX_RANGE_DAYS: i64 = 400;
const MAX_RANGE_MS: i64 = MAX_RANGE_DAYS * 24 * 60 * 60 * 1000;

/// 한 요청이 저장소에 던질 파티션 질의 총량 상한 (`인스턴스 수 × 일수 × 읽기 파도`).
///
/// [`MAX_RANGE_DAYS`] 는 **일수만** 막는다. 인스턴스 수가 곱해지는 것은 막지 못해서
/// 400일 × 500대 = 20만 회가 여전히 가능했다. 예산을 넘으면 일수를 줄이고
/// `truncated=true` 로 말한다([`clip_to_partition_budget`]).
///
/// **재분배 라운드도 같은 파티션을 다시 읽는다**(4라운드 지적). 그래서 예산은
/// 파도 수([`READ_WAVES`])로 나눠 잡는다 — 안 그러면 이 상한이 실제로는 3배다.
///
/// 값의 근거: 현재 규모(약 100대)에서 **월 단위 통계가 온전히 돌아야 한다**
/// (12,000 ÷ 3 ÷ 100 = 40일 ≥ 31일). 500대 목표에서는 8일까지만 되고, 월 집계는
/// 이 경로가 아니라 M12 롤업(수집 시점 스냅샷)의 몫이다.
const MAX_PARTITION_QUERIES: usize = 12_000;

/// 남은 예산을 다시 나눠 주는 최대 라운드 수.
///
/// 1회로는 부족하다 — 재분배받은 인스턴스가 덜 채우면 그 몫이 또 남는다. 무한히
/// 돌리지 않는 이유는 조회 하나가 저장소를 계속 때리지 않아야 하기 때문이다.
const MAX_REDISTRIBUTE_ROUNDS: usize = 2;

/// 읽기 파도 수 = 1차 + 재분배. 파티션 예산을 이 수로 나눈다.
const READ_WAVES: usize = 1 + MAX_REDISTRIBUTE_ROUNDS;

/// 인스턴스 조회를 동시에 몇 개까지 던지나. 저장소 조절(throttle)을 부르지 않는 선.
const READ_CONCURRENCY: usize = 16;

/// 구간을 만들고 상한을 검사한다. 모든 조회 경로가 이걸 쓴다.
fn checked_range(from_ms: i64, to_ms: i64) -> Result<TimeRange, ApiError> {
    let range = TimeRange::new(from_ms, to_ms)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid_range"))?;
    if range.to_ms().saturating_sub(range.from_ms()) > MAX_RANGE_MS {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "range_too_long"));
    }
    Ok(range)
}

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
    /// 화면 머리말에 띄우는 배포 사실. **비밀이 아니다** — 계정 ID·리전은 공개 값이고,
    /// 운영자가 "지금 어느 계정을 보고 있나" 를 화면에서 확인해야 한다.
    pub aws_account_id: String,
    pub aws_region: String,
    /// 수집 제어. 리더 루프와 공유한다 (`crate::control`).
    pub controls: Arc<crate::control::Controls>,
    /// 이 워커의 식별자. **어느 워커를 멈췄는지** 화면이 알아야 한다.
    pub worker_id: String,
    /// 이 프로세스가 수집 루프를 도는가 (`role` 이 collector·all).
    ///
    /// **`false` 면 제어를 받지 않는다.** 제어 플래그는 프로세스 원자값이라
    /// `role=api` 워커에서 눌러도 수집 워커는 모른다 — 성공을 돌려주면 화면이
    /// "멈췄다" 고 거짓말한다.
    pub runs_collector: bool,
}

pub fn router(state: ApiState) -> axum::Router {
    axum::Router::new()
        .route("/api/auth/config", get(auth_config))
        .route("/api/aws/info", get(aws_info))
        .route("/api/slow-queries", get(list_slow_queries))
        .route("/api/queries/{id}", get(get_query))
        .route("/api/queries/{id}/plan", get(get_query_plan))
        .route("/api/queries/{id}/markdown", get(get_query_markdown))
        .route("/api/plans", get(list_plans))
        .route("/api/digests", get(list_digests))
        .route("/api/statistics", get(instance_statistics))
        .route("/api/statistics/users", get(user_statistics))
        .route("/api/instances", get(list_instances))
        .route("/api/collector/status", get(collector_status))
        .route("/api/collector/pause", axum::routing::post(collector_pause))
        .route(
            "/api/collector/resume",
            axum::routing::post(collector_resume),
        )
        .route("/api/discovery/run", axum::routing::post(discovery_run))
        .route("/api/backfill/run", axum::routing::post(backfill_run))
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
    /// 인스턴스 **이름 조각**. 여러 대를 한 묶음으로 본다(`orders` → `orders-*`).
    instance_like: Option<String>,
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
    let range = checked_range(from_ms, to_ms)?;

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
        // **필터가 바뀌면 커서는 무효다.** 빠뜨리면 `orders` 로 만든 커서를 다른
        // 이름 조각에 그대로 쓸 수 있고, 그러면 위치가 다른 집합을 가리킨다.
        ("instance_like", p.instance_like.as_deref().unwrap_or("")),
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

    // **인스턴스마다 같은 몫을 읽고 전역 정렬한다.**
    //
    // 예전에는 인스턴스를 순회하며 상한에 닿으면 중단했다. 그러면 등록부 순서가
    // 앞선 인스턴스가 상한을 다 먹고, **뒤 인스턴스의 더 새로운 행이 통째로 빠진다** —
    // 정렬은 그 뒤에 하므로 화면은 그 사실을 알 수 없다.
    let got = collect_views(
        &state,
        &ctx,
        &ReadFilter {
            allowed: &allowed_envs,
            instance: p.instance.as_deref(),
            instance_like: p.instance_like.as_deref(),
        },
        range,
        // 하나 더 읽어 "더 있다" 를 정확히 판정한다. `>` 만 쓰면 정확히 상한일 때
        // `has_more=false` 가 되어 마지막 페이지가 끝인 것처럼 보인다.
        limit + 1,
        limit + 1,
    )
    .await?;
    let mut items = got.views;

    let has_more = got.truncated || items.len() > limit;
    items.truncate(limit);
    let total = items.len();
    Ok(Json(ListResponse {
        items,
        // 페이지네이션 위치가 아직 없으므로 커서를 발급하지 않는다.
        // **빈 커서를 주고 무한 루프를 만들지 않는다.**
        next_cursor: None,
        has_more,
        total,
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

/// 인스턴스 목록 행.
///
/// 참조 대시보드의 `RDS Instance Management` 표가 요구하는 열을 담는다 —
/// 태그(수집 대상 분류의 근거), 엔드포인트, 생성 시각, 실시간 수집 여부.
#[derive(Debug, serde::Serialize)]
struct InstanceView {
    id: String,
    /// 마지막 성분(RDS 식별자). 표에서 계정·리전을 반복하지 않기 위해.
    name: String,
    env: String,
    /// 태그에서 유도한 환경. **사용자 지정이 있으면 `env` 와 다르다** — 화면이
    /// "태그는 dev 인데 prd 로 지정됨" 을 설명할 수 있어야 한다.
    env_from_tags: String,
    env_override: Option<String>,
    state: String,
    engine: String,
    engine_version: String,
    endpoint: Option<String>,
    port: u16,
    instance_class: Option<String>,
    cluster_id: Option<String>,
    is_cluster_writer: bool,
    iam_auth_enabled: bool,
    vpc_id: Option<String>,
    availability_zone: Option<String>,
    /// **수집 대상인가.** 참조 대시보드의 `REAL-TIME` 열과 같은 뜻이다.
    collectible: bool,
    tags: std::collections::BTreeMap<String, String>,
    first_seen_ms: i64,
    last_seen_ms: i64,
    /// 탐색에서 사라진 시각. 있으면 화면이 "삭제됨" 으로 표시한다.
    deleted_at_ms: Option<i64>,
    cert_valid_till_ms: Option<i64>,
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
                name: i
                    .id
                    .as_str()
                    .rsplit('/')
                    .next()
                    .unwrap_or(i.id.as_str())
                    .to_string(),
                env: i.env.effective.as_str().to_string(),
                env_from_tags: i.env.from_tags.as_str().to_string(),
                env_override: i.env.override_value.map(|e| e.as_str().to_string()),
                state: i.state.as_str().to_string(),
                engine: format!("{:?}", i.engine).to_lowercase(),
                engine_version: i.engine_version.raw.clone(),
                endpoint: i.endpoint.clone(),
                port: i.port,
                instance_class: i.instance_class.clone(),
                cluster_id: i.cluster_id.as_ref().map(|c| c.as_str().to_string()),
                is_cluster_writer: i.is_cluster_writer,
                iam_auth_enabled: i.iam_auth_enabled,
                vpc_id: i.vpc_id.clone(),
                availability_zone: i.availability_zone.clone(),
                collectible: i.is_collectible(),
                tags: i.tags.clone(),
                first_seen_ms: i.first_seen_ms,
                last_seen_ms: i.last_seen_ms,
                deleted_at_ms: i.deleted_at_ms,
                cert_valid_till_ms: i.cert_valid_till_ms,
            })
            .collect(),
    ))
}

/// 조회 대상 인스턴스를 정한다.
async fn self_instances(
    state: &ApiState,
    requested: Option<&str>,
    name_like: Option<&str>,
) -> Result<Vec<dbmon_core::instance::Instance>, ApiError> {
    let all = dbmon_core::ports::InstanceRegistry::list(&*state.registry)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?;
    let like = name_like.map(str::trim).filter(|s| !s.is_empty());
    Ok(all
        .into_iter()
        .filter(|i| requested.is_none_or(|want| i.id.as_str() == want))
        .filter(|i| like.is_none_or(|pat| name_matches(i.id.identifier(), pat)))
        .collect())
}

/// 인스턴스 **이름**에 패턴이 들어 있나 (대소문자 무시).
///
/// # 왜 서버에서 거르는가
///
/// 화면에서 이름 조각으로 여러 인스턴스를 묶어 보려면 그 집합이 **읽기 상한을
/// 나눠 갖는 단위**여야 한다. 클라이언트에서 걸러내면 서버는 등록부 전체에 몫을
/// 뿌리므로, `orders-*` 5대를 보려는데 상한은 500대에 나뉘어 정작 그 5대의 최근
/// 데이터가 빠진다([`collect_views`] 의 균등 분배).
///
/// # 왜 이름만 보는가
///
/// `InstanceId` 는 `계정/리전/이름` 이다. 전체를 매칭하면 `ap-northeast-2` 같은 조각이
/// 모든 인스턴스에 걸려 필터가 아무 일도 하지 않는다.
fn name_matches(identifier: &str, pattern: &str) -> bool {
    // ASCII 소문자 비교로 충분하다 — RDS 식별자는 `[a-z0-9-]` 다.
    identifier
        .to_ascii_lowercase()
        .contains(&pattern.to_ascii_lowercase())
}

fn parse_env(s: &str) -> Option<Env> {
    Env::ALL.iter().copied().find(|e| e.as_str() == s)
}

// ═════════════════════════════════════════════════════════════════════════════
// 다이제스트·통계·실행계획 (M0-12 화면이 요구하는 조회 경로)
//
// 참조 구현(`my_slow_query_scraper`)은 MongoDB 에 월간 다이제스트를 따로 쌓아 두고
// 화면에 냈다. 여기서는 **이미 저장된 실행 레코드를 조회 시점에 접는다** — 같은 표를
// 만들 수 있고, 수집 경로에 새 쓰기를 추가하지 않는다. 천장은 [`MAX_AGGREGATE_SCAN`].
// ═════════════════════════════════════════════════════════════════════════════

/// 배포 사실. 화면 머리말이 "어느 계정·리전을 보고 있나" 를 말하려면 필요하다.
async fn aws_info(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // **인증을 요구한다.** 계정 ID 는 비밀이 아니지만 인증 없이 답할 이유도 없다
    // (`/api/auth/config` 만 그 예외를 갖는다).
    let _ctx = context_of(&state, &headers)?;
    Ok(Json(serde_json::json!({
        "account_id": state.aws_account_id,
        "region": state.aws_region,
        "deployment_env": state.policy.deployment_env.as_str(),
    })))
}

#[derive(Debug, serde::Deserialize)]
pub struct AggregateParams {
    /// `YYYY-MM`. 주면 그 달 전체를 본다(KST 기준). `from_ms`/`to_ms` 보다 우선한다.
    month: Option<String>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    instance: Option<String>,
    /// 인스턴스 **이름 조각**. 여러 대를 한 묶음으로 집계한다(`orders` → `orders-*`).
    instance_like: Option<String>,
    env: Option<String>,
    /// **접을 레코드 수의 상한이다** — 반환 행 수가 아니다.
    ///
    /// `/api/slow-queries` 의 `limit` 은 페이지 크기라서 이름이 겹치면 `?limit=10` 이
    /// "10건만 집계" 로 읽히고, 그러면 통계가 조용히 틀린다. 그래서 `scan` 이다.
    scan: Option<usize>,
}

/// 집계 응답 공통 머리. **천장에 걸렸는지 말한다.**
#[derive(Debug, serde::Serialize)]
struct AggregateEnvelope<T> {
    items: Vec<T>,
    /// 접기에 쓴 실행 레코드 수.
    scanned: usize,
    /// 상한에 걸려 일부만 접었다. 화면이 이 사실을 표시해야 한다.
    truncated: bool,
    from_ms: i64,
    to_ms: i64,
    /// `month` 파라미터로 조회한 경우 그 값.
    month: Option<String>,
}

/// KST 기준 월 경계.
///
/// 왜 KST 인가: 이 프로젝트의 리포트 시간대 기본값이 `Asia/Seoul` 이고, 참조
/// 대시보드도 KST 로 월을 잘랐다. **UTC 로 자르면 매월 초 9시간이 이전 달로 간다.**
fn month_range(month: &str) -> Option<TimeRange> {
    use chrono::{FixedOffset, NaiveDate, TimeZone};

    let (y, m) = month.split_once('-')?;
    let year: i32 = y.parse().ok()?;
    let mon: u32 = m.parse().ok()?;
    if !(1..=12).contains(&mon) {
        return None;
    }
    let kst = FixedOffset::east_opt(9 * 3600)?;
    let start = NaiveDate::from_ymd_opt(year, mon, 1)?.and_hms_opt(0, 0, 0)?;
    let (next_year, next_mon) = if mon == 12 {
        (year + 1, 1)
    } else {
        (year, mon + 1)
    };
    let end = NaiveDate::from_ymd_opt(next_year, next_mon, 1)?.and_hms_opt(0, 0, 0)?;
    // **끝을 1ms 당긴다.** `TimeRange` 와 DynamoDB `BETWEEN` 은 양끝을 포함하므로,
    // 다음 달 0시를 그대로 두면 정확히 그 시각의 레코드가 **두 달에 모두** 잡힌다.
    TimeRange::new(
        kst.from_local_datetime(&start).single()?.timestamp_millis(),
        kst.from_local_datetime(&end).single()?.timestamp_millis() - 1,
    )
}

/// 집계 파라미터 → 시간 범위.
fn aggregate_range(p: &AggregateParams, now_ms: i64) -> Result<TimeRange, ApiError> {
    if let Some(month) = p.month.as_deref() {
        return month_range(month)
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid_month"));
    }
    let to_ms = p.to_ms.unwrap_or(now_ms);
    let from_ms = p.from_ms.unwrap_or(to_ms - DEFAULT_RANGE_MS);
    checked_range(from_ms, to_ms)
}

/// 조회·집계가 **공유하는 단 하나의 읽기 경로**. 뷰를 거치므로 리터럴 통제를
/// 우회할 수 없다.
///
/// # 왜 인스턴스마다 같은 몫인가
///
/// 등록부 순서대로 읽고 천장에서 멈추면 **바쁜 인스턴스 하나가 천장을 다 먹고**
/// 다른 인스턴스의 더 새로운 데이터가 통째로 빠진다. 그 뒤에 정렬해도 없는 행은
/// 나타나지 않는다 — 통계가 조용히 한 인스턴스만 반영한다.
///
/// # 환경 판정은 **요청 교집합**으로 한다
///
/// `allowed` 는 `사용자 스코프 ∩ 요청 env` 다. 레코드를 사용자 스코프로만 검사하면
/// `?env=dev` 를 줬는데 prd 레코드가 섞인다(환경이 바뀐 인스턴스의 과거 행).
///
/// # 균등 분배만으로는 표본이 왜곡된다 (그래서 재분배가 있다)
///
/// 균등 분배는 굶기지는 않지만 **몫이 1까지 줄 수 있다**(500대에 천장 501). 그러면
/// 바쁜 1대가 만든 최근 50건을 요청했는데 그 중 1건만 보이고, 나머지 자리는 다른
/// 인스턴스의 오래된 최신값이 채운다 — 표는 "최신순" 이라고 말하면서 가운데가 빈다.
/// 그래서 **몫을 꽉 채운 인스턴스에만 남은 예산을 다시 나눠 준다.** 한 번으로는
/// 부족하다 — 재분배받은 쪽이 덜 채우면 그 남은 몫이 또 놀기 때문에
/// [`MAX_REDISTRIBUTE_ROUNDS`] 회까지 반복한다. 무한히 돌리지 않는 이유는
/// 요청 하나가 저장소를 계속 때리지 않아야 하기 때문이다.
async fn collect_views(
    state: &ApiState,
    ctx: &AuthContext,
    filter: &ReadFilter<'_>,
    range: TimeRange,
    per_instance: usize,
    ceiling: usize,
) -> Result<Collected, ApiError> {
    let allowed = filter.allowed;
    let instances = self_instances(state, filter.instance, filter.instance_like).await?;
    if instances.is_empty() {
        return Ok(Collected {
            views: Vec::new(),
            truncated: false,
            range,
        });
    }
    let ids: Vec<_> = instances.iter().map(|i| i.id.clone()).collect();

    // ① 파티션 예산에 맞춰 구간을 좁힌다.
    let (range, clipped) = clip_to_partition_budget(range, ids.len());
    let mut truncated = clipped;

    // ② 1차: 천장을 균등 분배.
    let mut share = even_share(ceiling, ids.len(), per_instance);
    let mut rows = read_share(state, &ids, range, share).await?;
    let mut asked = vec![share; ids.len()];

    // ③ 꽉 채운 인스턴스에만 남은 예산을 재분배한다. **한 번으로는 부족하다** —
    //    재분배받은 쪽이 덜 채우면 그 몫이 또 남는다.
    for _ in 0..MAX_REDISTRIBUTE_ROUNDS {
        let used: usize = rows.iter().map(Vec::len).sum();
        // 몫을 꽉 채운 것들. **`asked[i]` 로 본다** — 라운드마다 몫이 달라진다.
        let hungry: Vec<usize> = (0..rows.len())
            .filter(|&i| rows[i].len() >= asked[i])
            .collect();
        let Some(bigger) = redistributed(ceiling, used, hungry.len(), share, per_instance) else {
            break;
        };
        // 최신순이므로 더 큰 limit 의 결과는 앞 라운드의 상위집합이다 — 이어 붙이지 않고 갈아낀다.
        let hungry_ids: Vec<_> = hungry.iter().map(|&i| ids[i].clone()).collect();
        // **버릴 앞 라운드 결과를 미리 놓는다.** 안 그러면 새 결과를 받는 동안 두 벌이
        // 함께 메모리에 있고, 집계 천장(2만 건)에서 그건 그대로 두 배다.
        for &slot in &hungry {
            rows[slot] = Vec::new();
        }
        let refetched = read_share(state, &hungry_ids, range, bigger).await?;
        for (&slot, found) in hungry.iter().zip(refetched) {
            rows[slot] = found;
            asked[slot] = bigger;
        }
        share = bigger;
    }

    let mut views = Vec::new();
    for (found, want) in rows.iter().zip(&asked) {
        // 몫을 꽉 채웠다는 것은 더 있다는 뜻이다. 저장소가 1MB 페이지를 따라가므로
        // (`list_by_instance`) 이 판정이 성립한다.
        if found.len() >= *want {
            truncated = true;
        }
        views.extend(
            found
                .iter()
                // **요청 교집합으로 검사한다.** 사용자 스코프만 보면 `?env=dev` 에
                // prd 행이 섞이고(환경이 바뀐 인스턴스의 과거 행), 인스턴스의 현재
                // 환경만 보면 그 반대로 볼 수 있는 행이 빠진다.
                .filter(|q| allowed.contains(&q.env))
                .map(|q| SlowQueryView::from_record(q, ctx)),
        );
    }

    // **같은 실행이 두 행으로 온 것을 접는다.** 저장소 경합으로 항목이 둘 생길 수
    // 있고(실측), 그대로 내보내면 표가 두 줄·통계가 두 배·유령 줄이 영원히 진행 중이다.
    let (deduped, collapsed) = aggregate::dedupe_executions(views);
    let mut views = deduped;
    if collapsed > 0 {
        // **조용히 접지 않는다.** 이 수가 0 이 아니면 저장소에 쌍둥이가 있다는 뜻이고,
        // 그건 조회가 고칠 수 없는 별도 결함이다.
        tracing::warn!(
            collapsed,
            "같은 record_id 의 행을 접었다 — 저장소에 쌍둥이 항목이 있다"
        );
    }

    // 전역 최신순. 인스턴스별 결과를 이어 붙였으므로 여기서 한 번 맞춘다.
    views.sort_by_key(|v| std::cmp::Reverse(v.started_at_ms));
    if views.len() > ceiling {
        views.truncate(ceiling);
        truncated = true;
    }
    Ok(Collected {
        views,
        truncated,
        range,
    })
}

/// 어디를 읽을지 정하는 필터.
///
/// 인자 수를 줄이려는 것만이 아니다 — **세 값은 함께 움직인다.** 환경 스코프를 좁히면
/// 인스턴스 집합이 바뀌고, 이름 조각은 그 집합을 다시 좁힌다. 따로 넘기면 호출부가
/// 하나를 빼먹어도 컴파일이 통과한다(실제로 `env` 만 넘기고 `instance_like` 를 잊는
/// 실수를 이 구조가 막는다).
struct ReadFilter<'a> {
    /// `사용자 스코프 ∩ 요청 env`. 레코드마다 이 집합으로 검사한다.
    allowed: &'a [Env],
    /// 정확한 인스턴스 id.
    instance: Option<&'a str>,
    /// 인스턴스 **이름 조각** — 여러 대를 한 묶음으로 본다.
    instance_like: Option<&'a str>,
}

/// 읽기 결과. **좁힌 구간을 함께 돌려준다** — 이게 없으면 8일치 표본을 화면이
/// "이번 달" 이라고 이름 붙인다(4라운드 지적). 조사 도구에서 구간 표시는 데이터의
/// 일부가 아니라 데이터의 정의다.
struct Collected {
    views: Vec<SlowQueryView>,
    /// 천장이나 파티션 예산에 걸려 **일부만** 읽었다.
    truncated: bool,
    /// 실제로 읽은 구간. 요청 구간과 다를 수 있다.
    range: TimeRange,
}

/// 1차 몫. **인스턴스가 많으면 1까지 준다** — 그 자체가 정상이고, 표본 왜곡은
/// [`redistributed`] 가 되돌린다.
fn even_share(ceiling: usize, instances: usize, per_instance: usize) -> usize {
    // `clamp` 은 min > max 면 패닉한다. 지금 호출부는 둘 다 1 이상이지만, 상한을
    // 0 으로 넘기는 호출이 생기면 조회 하나가 프로세스를 죽인다.
    (ceiling / instances.max(1)).clamp(1, per_instance.max(1))
}

/// 2차 몫. 1차에서 몫을 꽉 채운 인스턴스가 있고 예산이 남았을 때만 값이 있다.
///
/// 이 산술이 바로 왜곡의 원인이었다 — 500대에 천장 501이면 1차 몫이 **1** 이라
/// 바쁜 1대가 만든 최근 50건 중 1건만 표에 오른다. 남은 500을 바쁜 쪽에 몰아주면
/// 활동이 몇 대에 몰린 실제 상황에서 정확한 답이 나온다.
fn redistributed(
    ceiling: usize,
    used: usize,
    hungry: usize,
    share: usize,
    per_instance: usize,
) -> Option<usize> {
    if hungry == 0 || used >= ceiling {
        return None;
    }
    let bigger = (share + (ceiling - used) / hungry).min(per_instance);
    (bigger > share).then_some(bigger)
}

/// 인스턴스별 조회를 **동시에** 던진다. 순차로 돌면 왕복 지연이 그대로 곱해진다 —
/// 500대 × 5ms 면 응답 하나가 2.5초다. 상한을 두는 이유는 저장소 조절(throttle)이다.
async fn read_share(
    state: &ApiState,
    ids: &[dbmon_core::ids::InstanceId],
    range: TimeRange,
    limit: usize,
) -> Result<Vec<Vec<dbmon_core::slow_query::SlowQuery>>, ApiError> {
    use futures::stream::{StreamExt, TryStreamExt};

    // **`id` 와 저장소를 각 future 로 옮긴다.** 참조를 빌리는 클로저로 쓰면 상위
    // 랭크 수명(HRTB)이 맞지 않아 핸들러가 `Send` 를 잃는다 — 컴파일러가 route 등록
    // 지점에서 그걸 알려준다.
    let tasks: Vec<_> = ids
        .iter()
        .map(|id| {
            let store = Arc::clone(&state.store);
            let id = id.clone();
            async move {
                store
                    .list_by_instance(&id, range, limit)
                    .await
                    .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))
            }
        })
        .collect();

    futures::stream::iter(tasks)
        // **순서를 유지한다.** 호출부가 인덱스로 인스턴스를 되찾으므로 `buffer_unordered`
        // 를 쓰면 결과가 다른 인스턴스에 붙는다.
        .buffered(READ_CONCURRENCY)
        .try_collect()
        .await
}

/// 파티션 예산에 맞춰 구간을 좁힌다. **최신 쪽을 남긴다** — 조사 도구에서 과거를
/// 남기고 현재를 버리는 것은 거의 항상 틀린 선택이다.
///
/// 파티션은 `인스턴스 × 날짜` 다. 400일 구간을 500대에 물으면 20만 회 질의가 되고,
/// 그건 **인증된 사용자 한 명이 저장소를 마비시킬 수 있다는 뜻이다.** 구간 상한
/// ([`MAX_RANGE_DAYS`])만으로는 인스턴스 수가 곱해지는 것을 막지 못한다.
fn clip_to_partition_budget(range: TimeRange, instances: usize) -> (TimeRange, bool) {
    const DAY_MS: i64 = 86_400_000;
    // 파도 수로 나눈다 — 재분배 라운드가 같은 파티션을 다시 읽는다.
    let max_days = (MAX_PARTITION_QUERIES / READ_WAVES / instances.max(1)).max(1) as i64;
    let to_day = range.to_ms().div_euclid(DAY_MS);
    let parts = to_day - range.from_ms().div_euclid(DAY_MS) + 1;
    if parts <= max_days {
        return (range, false);
    }
    let from_ms = (to_day - (max_days - 1)) * DAY_MS;
    match TimeRange::new(from_ms, range.to_ms()) {
        Some(clipped) => (clipped, true),
        // 좁히기가 실패하면(있을 수 없다) 원본을 쓴다 — 조용히 빈 결과를 주지 않는다.
        None => (range, false),
    }
}

/// 집계 파라미터에서 `사용자 스코프 ∩ 요청 env` 를 구한다.
fn allowed_envs_of(ctx: &AuthContext, env: Option<&str>) -> Result<Vec<Env>, ApiError> {
    let requested: Vec<Env> = match env {
        Some(e) => {
            vec![
                parse_env(e)
                    .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid_env"))?,
            ]
        }
        None => Env::ALL.to_vec(),
    };
    let allowed = ctx.scope_intersection(&requested);
    if allowed.is_empty() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "env_not_allowed"));
    }
    Ok(allowed)
}

/// 집계 호출부의 공통 준비 — 스코프·몫.
async fn collect_for_aggregate(
    state: &ApiState,
    ctx: &AuthContext,
    p: &AggregateParams,
    range: TimeRange,
) -> Result<Collected, ApiError> {
    let allowed = allowed_envs_of(ctx, p.env.as_deref())?;
    let ceiling = p.scan.unwrap_or(MAX_AGGREGATE_SCAN).min(MAX_AGGREGATE_SCAN);
    collect_views(
        state,
        ctx,
        &ReadFilter {
            allowed: &allowed,
            instance: p.instance.as_deref(),
            instance_like: p.instance_like.as_deref(),
        },
        range,
        AGGREGATE_PAGE,
        ceiling,
    )
    .await
}

async fn list_digests(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(p): Query<AggregateParams>,
) -> Result<Json<AggregateEnvelope<aggregate::DigestRow>>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let range = aggregate_range(&p, SystemClock.now_ms())?;
    let got = collect_for_aggregate(&state, &ctx, &p, range).await?;
    let (views, truncated) = (got.views, got.truncated);
    Ok(Json(AggregateEnvelope {
        items: aggregate::digest_rows(&views),
        scanned: views.len(),
        truncated,
        // **좁힌 구간을 보고한다.** 요청한 구간을 그대로 돌려주면 8일치 표본이
        // 화면에서 "이번 달" 이 된다(4라운드 지적).
        from_ms: got.range.from_ms(),
        to_ms: got.range.to_ms(),
        month: p.month.clone(),
    }))
}

async fn instance_statistics(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(p): Query<AggregateParams>,
) -> Result<Json<AggregateEnvelope<aggregate::InstanceStats>>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let range = aggregate_range(&p, SystemClock.now_ms())?;
    let got = collect_for_aggregate(&state, &ctx, &p, range).await?;
    let (views, truncated) = (got.views, got.truncated);
    Ok(Json(AggregateEnvelope {
        items: aggregate::instance_stats(&views),
        scanned: views.len(),
        truncated,
        // **좁힌 구간을 보고한다.** 요청한 구간을 그대로 돌려주면 8일치 표본이
        // 화면에서 "이번 달" 이 된다(4라운드 지적).
        from_ms: got.range.from_ms(),
        to_ms: got.range.to_ms(),
        month: p.month.clone(),
    }))
}

async fn user_statistics(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(p): Query<AggregateParams>,
) -> Result<Json<AggregateEnvelope<aggregate::UserStats>>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let range = aggregate_range(&p, SystemClock.now_ms())?;
    let got = collect_for_aggregate(&state, &ctx, &p, range).await?;
    let (views, truncated) = (got.views, got.truncated);
    Ok(Json(AggregateEnvelope {
        items: aggregate::user_stats(&views),
        scanned: views.len(),
        truncated,
        // **좁힌 구간을 보고한다.** 요청한 구간을 그대로 돌려주면 8일치 표본이
        // 화면에서 "이번 달" 이 된다(4라운드 지적).
        from_ms: got.range.from_ms(),
        to_ms: got.range.to_ms(),
        month: p.month.clone(),
    }))
}

/// 실행계획이 있는 최근 레코드. 참조 대시보드의 `Recent Explain Plans` 표.
async fn list_plans(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(p): Query<AggregateParams>,
) -> Result<Json<ListResponse>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let range = aggregate_range(&p, SystemClock.now_ms())?;
    let got = collect_for_aggregate(&state, &ctx, &p, range).await?;
    let (mut views, truncated) = (got.views, got.truncated);

    // **플랜이 있는 것만.** 없는 레코드를 섞으면 "플랜을 눌렀는데 아무것도 없다" 가 된다.
    views.retain(|v| v.has_plan);
    views.sort_by_key(|v| std::cmp::Reverse(v.started_at_ms));
    // 잘렸으면 **그 사실도 `has_more` 다.** 화면이 "플랜이 이게 전부" 로 읽으면 안 된다.
    let capped = views.len() > MAX_LIMIT;
    views.truncate(MAX_LIMIT);
    let total = views.len();
    Ok(Json(ListResponse {
        items: views,
        next_cursor: None,
        has_more: truncated || capped,
        total,
    }))
}

/// 레코드 하나를 읽고 환경 스코프를 검사한다. 상세·플랜·마크다운이 공유한다.
async fn record_for(
    state: &ApiState,
    ctx: &AuthContext,
    id: String,
) -> Result<dbmon_core::slow_query::SlowQuery, ApiError> {
    let record_id = dbmon_core::ids::RecordId::try_from(id)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_record_id"))?;
    let found = state
        .store
        .get(&record_id)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found"))?;
    if !ctx.is_env_allowed(found.env) {
        // 존재 여부를 알리지 않는다.
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
    }
    Ok(found)
}

async fn get_query_plan(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<view::PlanView>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let found = record_for(&state, &ctx, id).await?;
    Ok(Json(view::PlanView::from_record(&found)))
}

/// 실행계획을 마크다운으로. 참조 대시보드의 "Download Markdown" 대응.
///
/// **HTML 이 아니라 텍스트다** — 운영자가 티켓·문서에 붙여 쓰는 용도이고,
/// 그래서 서버가 만드는 편이 낫다(브라우저에서 조립하면 화면마다 형식이 갈린다).
async fn get_query_markdown(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    use std::fmt::Write as _;

    let ctx = context_of(&state, &headers)?;
    let found = record_for(&state, &ctx, id).await?;
    // **뷰를 거친다.** 마크다운도 SQL 을 담으므로 리터럴 통제를 지나야 한다.
    let v = SlowQueryView::from_record(&found, &ctx);

    let mut md = String::new();
    let _ = writeln!(md, "# 슬로우 쿼리 {}", v.record_id);
    let _ = writeln!(md);
    let _ = writeln!(md, "| 항목 | 값 |");
    let _ = writeln!(md, "|---|---|");
    let _ = writeln!(md, "| 인스턴스 | {} ({}) |", v.instance_id, v.env);
    let _ = writeln!(md, "| 상태 | {} |", v.state);
    let _ = writeln!(
        md,
        "| 실행시간 | {} ms ({}) |",
        v.duration_ms, v.duration_source
    );
    let _ = writeln!(md, "| 시작 (epoch ms) | {} |", v.started_at_ms);
    let _ = writeln!(md, "| 스레드 | {} |", v.thread_id);
    let _ = writeln!(
        md,
        "| 스키마 | {} |",
        v.schema_name.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        md,
        "| DB 사용자 | {} |",
        v.db_user.as_deref().unwrap_or("-")
    );
    let _ = writeln!(md, "| 유형 | {} |", v.statement_type);
    let _ = writeln!(md, "| 다이제스트 | `{}` |", v.app_digest);
    let _ = writeln!(
        md,
        "| 조사 행 / 반환 행 | {} / {} |",
        v.rows_examined.map_or("-".to_string(), |n| n.to_string()),
        v.rows_sent.map_or("-".to_string(), |n| n.to_string())
    );
    let _ = writeln!(md);
    let _ = writeln!(md, "## SQL");
    let _ = writeln!(md);
    match (&v.sql_text, v.sql_redacted_reason) {
        (Some(sql), _) => {
            let _ = writeln!(md, "```sql\n{sql}\n```");
        }
        (None, Some(reason)) => {
            let _ = writeln!(md, "> SQL 이 포함되지 않았다: `{reason}`");
        }
        (None, None) => {
            let _ = writeln!(md, "> SQL 이 없다.");
        }
    }

    let plan = view::PlanView::from_record(&found);
    let _ = writeln!(md);
    let _ = writeln!(md, "## 실행계획");
    let _ = writeln!(md);
    if let Some(err) = &plan.error {
        let _ = writeln!(md, "> 수집 실패: `{err}`");
    }
    if let Some(tree) = &plan.tree_text {
        let _ = writeln!(md, "```text\n{tree}\n```");
    }
    if let Some(json) = &plan.normalized_json {
        let _ = writeln!(md, "```json\n{json}\n```");
    } else if let Some(key) = &plan.s3_key {
        let _ = writeln!(
            md,
            "> 300KB 를 넘어 오프로드됐다: `{key}` (조회 경로 미구현)"
        );
    } else if plan.error.is_none() {
        let _ = writeln!(md, "> 저장된 계획이 없다.");
    }

    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"slow-query.md\"",
            ),
        ],
        md,
    )
        .into_response())
}

// ═════════════════════════════════════════════════════════════════════════════
// 수집 제어 (참조 구현의 `POST /mysql/start|stop`, `POST /collectors/rds-instances`)
//
// **쓰기 경로다.** 조회는 `viewer` 도 하지만, 수집을 멈추는 것은 관측을 멈추는 것이고
// 그 사이의 슬로우 쿼리는 영구히 없다.
// ═════════════════════════════════════════════════════════════════════════════

/// 전역 수집 조작 권한.
///
/// # 왜 역할만으로는 부족한가
///
/// 이 제어는 **전역이다** — 멈추면 이 워커가 수집하는 모든 인스턴스가 멈춘다.
/// `dev` 스코프만 가진 `operator` 가 누르면 **prd 관측이 멈춘다.** 그래서 역할과
/// 함께 **전 환경 스코프**를 요구한다. 인스턴스별 제어가 생기면 그때 좁힌다.
fn require_global_control(ctx: &AuthContext) -> Result<(), ApiError> {
    use dbmon_core::rbac::Role;
    if !matches!(ctx.role, Role::Operator | Role::Admin) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "role_required"));
    }
    // 스코프가 전 환경을 덮는가. 부분 스코프로 전역 스위치를 누를 수 없다.
    if !Env::ALL.iter().all(|e| ctx.is_env_allowed(*e)) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "scope_required"));
    }
    Ok(())
}

/// 제어 요청에 **브라우저가 임의 오리진에서 보낼 수 없는 헤더**를 요구한다.
///
/// # 왜 필요한가
///
/// 로컬 개발에서는 루프백 바인드면 **토큰 없이 통과**한다(그게 그 모드의 목적이다).
/// 그러면 개발자가 방문한 아무 웹페이지가 `fetch("http://127.0.0.1:8080/api/collector/pause",
/// {method:"POST"})` 로 **수집을 멈출 수 있다** — 응답은 CORS 로 못 읽지만 부작용은
/// 이미 일어난다. 전형적인 CSRF 다.
///
/// 커스텀 헤더는 크로스 오리진에서 **preflight 를 통과해야** 보낼 수 있고, 우리는
/// CORS 를 열지 않으므로 preflight 가 실패한다. 그래서 이 헤더 하나가 방어가 된다.
fn require_control_header(headers: &HeaderMap) -> Result<(), ApiError> {
    if headers.get("x-dbmon-control").is_some() {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "control_header_required",
        ))
    }
}

/// 이 프로세스가 실제로 수집을 도는가.
///
/// **아니면 조작을 거부한다.** 플래그는 프로세스 원자값이므로 `role=api` 워커에서
/// 바꿔도 수집 워커는 모른다 — 200 을 돌려주면 화면이 "멈췄다" 고 거짓말한다.
/// 여러 워커에 걸쳐 멈추려면 설정 저장소에 상태를 둬야 하고, 그건 별도 작업이다.
fn require_collector_worker(state: &ApiState) -> Result<(), ApiError> {
    if state.runs_collector {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::CONFLICT, "not_a_collector"))
    }
}

#[derive(Debug, serde::Serialize)]
struct CollectorStatus {
    #[serde(flatten)]
    snapshot: crate::control::ControlSnapshot,
    worker_id: String,
    /// 이 플래그가 어디까지 적용되는가. 여러 워커를 띄웠으면 **이 프로세스뿐**이다.
    scope: &'static str,
    /// 이 워커가 수집 루프를 도는가. `false` 면 제어가 거부된다.
    runs_collector: bool,
    /// 조작할 수 있는 역할인가. 화면이 버튼을 비활성화하는 근거.
    can_control: bool,
    role: String,
}

async fn collector_status(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<CollectorStatus>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    Ok(Json(CollectorStatus {
        snapshot: state.controls.snapshot(),
        worker_id: state.worker_id.clone(),
        scope: "process",
        runs_collector: state.runs_collector,
        can_control: state.runs_collector && require_global_control(&ctx).is_ok(),
        role: ctx.role.as_str().to_string(),
    }))
}

async fn collector_pause(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<CollectorStatus>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    require_control_header(&headers)?;
    require_global_control(&ctx)?;
    require_collector_worker(&state)?;
    state.controls.set_paused(true, SystemClock.now_ms());
    // **감사 로그를 남긴다.** 관측이 멈춘 구간은 나중에 반드시 질문거리가 된다.
    tracing::warn!(
        subject = %ctx.subject,
        worker = %state.worker_id,
        "수집을 멈췄다 (화면 조작) — 이 구간의 슬로우 쿼리는 기록되지 않는다"
    );
    collector_status(State(state), headers).await
}

async fn collector_resume(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<CollectorStatus>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    require_control_header(&headers)?;
    require_global_control(&ctx)?;
    require_collector_worker(&state)?;
    state.controls.set_paused(false, SystemClock.now_ms());
    tracing::warn!(subject = %ctx.subject, worker = %state.worker_id, "수집을 재개했다 (화면 조작)");
    collector_status(State(state), headers).await
}

/// 탐색을 즉시 돌린다. 참조 구현의 "인스턴스 수집" 버튼.
async fn discovery_run(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<CollectorStatus>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    require_control_header(&headers)?;
    require_global_control(&ctx)?;
    require_collector_worker(&state)?;
    state.controls.request_discovery();
    tracing::info!(subject = %ctx.subject, "탐색을 요청했다 (화면 조작)");
    collector_status(State(state), headers).await
}

/// 슬로우로그 백필을 즉시 돌린다. 참조 구현의 "데이터 수집" 버튼.
///
/// ⚠ **임의 구간을 지정할 수는 없다.** 백필은 체크포인트부터 현재까지를 읽는
/// 구조이고, 지난달을 다시 훑으려면 별도 작업(잡 큐)이 필요하다. 지금 할 수 있는
/// 것은 "다음 주기를 기다리지 말고 지금 돌려라" 다.
async fn backfill_run(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<CollectorStatus>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    require_control_header(&headers)?;
    require_global_control(&ctx)?;
    require_collector_worker(&state)?;
    state.controls.request_backfill();
    tracing::info!(subject = %ctx.subject, "백필을 요청했다 (화면 조작)");
    collector_status(State(state), headers).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **균등 분배만 쓰면 바쁜 인스턴스의 최근 데이터가 표본에서 사라진다.**
    ///
    /// 500대 등록부에 천장 501이면 1차 몫이 1이다. 그 상태로 끝내면 "최근 50건" 을
    /// 요청한 사람은 바쁜 1대의 1건 + 다른 499대의 오래된 최신값을 본다 — 표는
    /// 최신순이라고 말하면서 가운데가 비어 있고, 화면은 그 사실을 알 수 없다.
    #[test]
    fn a_busy_instance_gets_the_leftover_budget_back() {
        // 1차: 500대에 501을 나누면 1건씩이다.
        assert_eq!(even_share(501, 500, 501), 1);
        // 3대만 데이터가 있었다(각 1건). 남은 498을 그 3대에 몰아준다.
        let bigger = redistributed(501, 3, 3, 1, 501).expect("재분배가 있어야 한다");
        assert_eq!(bigger, 1 + 498 / 3);

        // 전부 꽉 찼으면 남는 예산이 없다 — 2차를 돌리지 않는다(무의미한 재조회 금지).
        assert_eq!(redistributed(501, 501, 500, 1, 501), None);
        // 꽉 채운 인스턴스가 없으면(=다 읽었다) 2차가 없다.
        assert_eq!(redistributed(501, 20, 0, 1, 501), None);
        // 인스턴스별 상한을 넘지 않는다.
        assert_eq!(redistributed(20_000, 0, 1, 5_000, 5_000), None);
        // 인스턴스 하나면 1차부터 인스턴스 상한까지 받는다.
        assert_eq!(even_share(20_000, 1, 5_000), 5_000);
    }

    /// **재분배 한 번으로는 부족하다.**
    ///
    /// 재분배받은 인스턴스가 몫을 덜 채우면 그 남은 몫이 또 놀고, 그 사이 다른
    /// 바쁜 인스턴스의 더 새로운 행이 빠진 채로 표가 그려진다. 라운드를 반복하면
    /// 남는 예산이 실제로 줄어든다 — 그 산술을 고정한다.
    #[test]
    fn each_round_hands_the_still_unused_budget_to_whoever_filled_it() {
        // 100대·천장 500 → 1차 몫 5. 2대만 꽉 채웠고(10건) 나머지는 90건뿐이라 치면
        let share = even_share(500, 100, 500);
        assert_eq!(share, 5);
        let round2 = redistributed(500, 100, 2, share, 500).expect("2라운드");
        assert_eq!(round2, 5 + 200); // 남은 400을 2대가 나눈다

        // 2라운드에서 한 대가 덜 채웠다(합계 300) → 3라운드가 남은 200을 준다.
        let round3 = redistributed(500, 300, 1, round2, 500).expect("3라운드");
        assert_eq!(round3, 205 + 200);
        // 예산을 다 쓰면 멈춘다 — 무의미한 재조회를 하지 않는다.
        assert_eq!(redistributed(500, 500, 1, round3, 500), None);
    }

    /// **일수 상한은 인스턴스 수가 곱해지는 것을 막지 못한다.**
    ///
    /// 400일 × 500대 = 20만 회 질의. 예산을 넘으면 **최신 쪽을 남기고** 자른다 —
    /// 조사 도구에서 과거를 남기고 현재를 버리면 쓸 수 없다.
    #[test]
    fn a_wide_fan_out_clips_the_range_to_the_newest_days() {
        const DAY: i64 = 86_400_000;
        let to = 1_760_000_000_000;
        let long = TimeRange::new(to - 399 * DAY, to).expect("구간");

        // 인스턴스 하나면 예산(4000)이 일수 상한(400)보다 크므로 손대지 않는다.
        let (kept, clipped) = clip_to_partition_budget(long, 1);
        assert!(!clipped);
        assert_eq!(kept.from_ms(), long.from_ms());

        // 500대면 예산이 8일이다. 끝은 그대로, 시작만 당긴다.
        let (cut, clipped) = clip_to_partition_budget(long, 500);
        assert!(clipped, "잘랐으면 잘랐다고 말해야 한다");
        assert_eq!(cut.to_ms(), to);
        let parts = cut.date_parts().len();
        assert_eq!(parts, 8, "파티션 수가 예산 몫과 같아야 한다");
        assert!(cut.from_ms() > long.from_ms());

        // 짧은 구간은 인스턴스가 많아도 그대로다 — 기본 24시간 조회가 잘리면 안 된다.
        let day = TimeRange::new(to - DAY, to).expect("구간");
        let (kept, clipped) = clip_to_partition_budget(day, 500);
        assert!(!clipped);
        assert_eq!(kept.from_ms(), day.from_ms());
    }

    /// **이름 조각으로 여러 대를 묶는다.** 계정·리전 조각에는 걸리지 않아야 한다.
    ///
    /// 전체 `InstanceId`(`계정/리전/이름`)를 매칭하면 `ap-northeast-2` 같은 조각이 모든
    /// 인스턴스에 걸려 필터가 아무 일도 하지 않는다 — 그건 필터가 있다고 믿게 만드는
    /// 쪽이 더 나쁘다.
    #[test]
    fn a_name_fragment_groups_instances_without_matching_account_or_region() {
        assert!(name_matches("orders-prd-01", "orders"));
        assert!(name_matches("orders-prd-02", "orders"));
        assert!(
            name_matches("ORDERS-PRD-01", "orders"),
            "대소문자를 무시한다"
        );
        assert!(name_matches("orders-prd-01", "PRD"));
        assert!(!name_matches("billing-prd-01", "orders"));
        // 이름 안의 조각이면 어디든 걸린다(접두만이 아니다).
        assert!(name_matches("api-orders-01", "orders"));
        // 계정·리전은 `identifier()` 에 없으므로 애초에 매칭 대상이 아니다.
        assert!(!name_matches("orders-prd-01", "ap-northeast-2"));
        assert!(!name_matches("orders-prd-01", "123456789012"));
    }

    #[test]
    fn limit_is_clamped_to_a_sane_range() {
        // 상한을 넘겨도 잘린다 — 클라이언트가 1만 건을 요구할 수 없다.
        assert_eq!(usize::MAX.clamp(1, MAX_LIMIT), MAX_LIMIT);
        assert_eq!(0usize.clamp(1, MAX_LIMIT), 1);
    }

    /// **`viewer` 는 수집을 멈출 수 없다.** 관측을 멈추는 것은 그 구간의 슬로우
    /// 쿼리를 영구히 잃는 것이고, 조회 권한과 같은 급이 아니다.
    #[test]
    fn only_operators_may_control_collection() {
        use dbmon_core::rbac::Role;
        let ctx = |role| AuthContext {
            subject: "u".into(),
            role,
            env_scope: vec![Env::Dev],
            can_see_literals: false,
            claims_version: 0,
        };
        // 전 환경 스코프를 가진 operator·admin 만 통과한다.
        let all = |role| AuthContext {
            subject: "u".into(),
            role,
            env_scope: Env::ALL.to_vec(),
            can_see_literals: false,
            claims_version: 0,
        };
        assert!(require_global_control(&all(Role::Admin)).is_ok());
        assert!(require_global_control(&all(Role::Operator)).is_ok());
        assert!(require_global_control(&all(Role::Viewer)).is_err());

        // **부분 스코프는 전역 스위치를 누를 수 없다.** dev 만 보는 operator 가
        // 누르면 prd 관측이 멈춘다.
        assert!(
            require_global_control(&ctx(Role::Operator)).is_err(),
            "dev 스코프 operator 가 전역 수집을 멈출 수 있다"
        );
        assert!(require_global_control(&ctx(Role::Admin)).is_err());
    }

    /// 월 경계는 **KST** 다. UTC 로 자르면 매월 초 9시간이 이전 달로 간다.
    #[test]
    fn month_boundaries_are_kst() {
        let r = month_range("2026-08").expect("유효한 월");
        // 2026-08-01 00:00 KST = 2026-07-31 15:00 UTC
        assert_eq!(r.from_ms(), 1_785_510_000_000);
        // **다음 달 0시의 1ms 전**이다. 그대로 두면 그 시각 레코드가 두 달에 잡힌다.
        assert_eq!(r.to_ms(), 1_788_188_400_000 - 1);

        // 두 달이 겹치지 않는다 — 경계가 딱 맞물린다.
        let next = month_range("2026-09").expect("유효한 월");
        assert_eq!(next.from_ms(), r.to_ms() + 1);
        assert!(month_range("2026-13").is_none());
        assert!(month_range("2026-00").is_none());
        assert!(month_range("nope").is_none());
        assert!(month_range("2026").is_none());
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
