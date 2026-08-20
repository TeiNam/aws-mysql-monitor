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
    TimeRange::new(
        kst.from_local_datetime(&start).single()?.timestamp_millis(),
        kst.from_local_datetime(&end).single()?.timestamp_millis(),
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
    TimeRange::new(from_ms, to_ms)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid_range"))
}

/// 집계용 레코드 수집. **뷰를 거친다** — 리터럴 통제가 그 안에 있다.
///
/// 반환값의 두 번째는 "천장에 걸렸다" 다. 저장소가 **최신 순으로** 돌려주므로
/// (`scan_index_forward(false)`) 잘리는 쪽은 항상 과거다 — 통계가 최근을 놓치지 않는다.
async fn collect_views(
    state: &ApiState,
    ctx: &AuthContext,
    p: &AggregateParams,
    range: TimeRange,
) -> Result<(Vec<SlowQueryView>, bool), ApiError> {
    let requested_envs: Vec<Env> = match p.env.as_deref() {
        Some(e) => vec![
            parse_env(e).ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid_env"))?,
        ],
        None => Env::ALL.to_vec(),
    };
    let allowed = ctx.scope_intersection(&requested_envs);
    if allowed.is_empty() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "env_not_allowed"));
    }

    let instances = self_instances(state, p.instance.as_deref()).await?;
    let ceiling = p.scan.unwrap_or(MAX_AGGREGATE_SCAN).min(MAX_AGGREGATE_SCAN);

    let mut views = Vec::new();
    let mut truncated = false;
    for inst in &instances {
        if !allowed.contains(&inst.env.effective) {
            continue;
        }
        if views.len() >= ceiling {
            truncated = true;
            break;
        }
        let page = AGGREGATE_PAGE.min(ceiling - views.len());
        let found = state
            .store
            .list_by_instance(&inst.id, range, page)
            .await
            .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?;
        // 페이지를 꽉 채웠다는 것은 더 있을 수 있다는 뜻이다. 저장소가
        // `LastEvaluatedKey` 를 노출하지 않으므로 이게 유일한 신호다.
        if found.len() == page {
            truncated = true;
        }
        views.extend(found.iter().map(|q| SlowQueryView::from_record(q, ctx)));
    }
    Ok((views, truncated))
}

async fn list_digests(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(p): Query<AggregateParams>,
) -> Result<Json<AggregateEnvelope<aggregate::DigestRow>>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let range = aggregate_range(&p, SystemClock.now_ms())?;
    let (views, truncated) = collect_views(&state, &ctx, &p, range).await?;
    Ok(Json(AggregateEnvelope {
        items: aggregate::digest_rows(&views),
        scanned: views.len(),
        truncated,
        from_ms: range.from_ms(),
        to_ms: range.to_ms(),
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
    let (views, truncated) = collect_views(&state, &ctx, &p, range).await?;
    Ok(Json(AggregateEnvelope {
        items: aggregate::instance_stats(&views),
        scanned: views.len(),
        truncated,
        from_ms: range.from_ms(),
        to_ms: range.to_ms(),
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
    let (views, truncated) = collect_views(&state, &ctx, &p, range).await?;
    Ok(Json(AggregateEnvelope {
        items: aggregate::user_stats(&views),
        scanned: views.len(),
        truncated,
        from_ms: range.from_ms(),
        to_ms: range.to_ms(),
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
    let (mut views, truncated) = collect_views(&state, &ctx, &p, range).await?;

    // **플랜이 있는 것만.** 없는 레코드를 섞으면 "플랜을 눌렀는데 아무것도 없다" 가 된다.
    views.retain(|v| v.has_plan);
    views.sort_by_key(|v| std::cmp::Reverse(v.started_at_ms));
    views.truncate(MAX_LIMIT);
    let total = views.len();
    Ok(Json(ListResponse {
        items: views,
        next_cursor: None,
        has_more: truncated,
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
// 그 사이의 슬로우 쿼리는 영구히 없다. `operator` 이상만 허용한다.
// ═════════════════════════════════════════════════════════════════════════════

/// 조작 권한. `viewer` 는 볼 수만 있다.
fn require_operator(ctx: &AuthContext) -> Result<(), ApiError> {
    match ctx.role {
        dbmon_core::rbac::Role::Operator | dbmon_core::rbac::Role::Admin => Ok(()),
        dbmon_core::rbac::Role::Viewer => {
            Err(ApiError::new(StatusCode::FORBIDDEN, "role_required"))
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct CollectorStatus {
    #[serde(flatten)]
    snapshot: crate::control::ControlSnapshot,
    worker_id: String,
    /// 이 플래그가 어디까지 적용되는가. 여러 워커를 띄웠으면 **이 프로세스뿐**이다.
    scope: &'static str,
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
        can_control: require_operator(&ctx).is_ok(),
        role: ctx.role.as_str().to_string(),
    }))
}

async fn collector_pause(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<CollectorStatus>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    require_operator(&ctx)?;
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
    require_operator(&ctx)?;
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
    require_operator(&ctx)?;
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
    require_operator(&ctx)?;
    state.controls.request_backfill();
    tracing::info!(subject = %ctx.subject, "백필을 요청했다 (화면 조작)");
    collector_status(State(state), headers).await
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
        assert!(require_operator(&ctx(Role::Admin)).is_ok());
        assert!(require_operator(&ctx(Role::Operator)).is_ok());
        assert!(require_operator(&ctx(Role::Viewer)).is_err());
    }

    /// 월 경계는 **KST** 다. UTC 로 자르면 매월 초 9시간이 이전 달로 간다.
    #[test]
    fn month_boundaries_are_kst() {
        let r = month_range("2026-08").expect("유효한 월");
        // 2026-08-01 00:00 KST = 2026-07-31 15:00 UTC
        assert_eq!(r.from_ms(), 1_785_510_000_000);
        // 2026-09-01 00:00 KST
        assert_eq!(r.to_ms(), 1_788_188_400_000);
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
