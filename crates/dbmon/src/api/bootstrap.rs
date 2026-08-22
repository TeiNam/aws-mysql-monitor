//! 부트스트랩 API (M3-6·M3-7·M3-14,
//! [13 §2.7](../../../../docs/13-api-spec.md), [07 §2.7](../../../../docs/07-credentials-bootstrap.md)).
//!
//! # 세 엔드포인트
//!
//! | 경로 | 하는 일 | 대상 DB 를 |
//! |---|---|---|
//! | `POST /api/instances/{id}/bootstrap/plan` | 계획 + 수동 스크립트 | **읽는다** |
//! | `POST /api/instances/{id}/bootstrap/apply` | 실행 | **바꾼다** |
//! | `POST /api/instances/{id}/bootstrap/verify` | 권한 재점검 (계획과 같지만 마스터 없이) | 읽는다 |
//!
//! # 권한
//!
//! 셋 다 **admin** 이고 `x-dbmon-control: 1` 헤더를 요구한다. 정지·재개와 같은 규칙에
//! 역할 조건을 더한 것이다 — 대상 DB 에 `CREATE USER` 를 실행하는 동작이므로 조회
//! 권한과 같은 문턱에 둘 수 없다.
//!
//! # `verify` 가 따로 있는 이유
//!
//! 수동으로 계정을 만든 뒤(문서 07 §5 경로) "제대로 됐나" 를 확인해야 한다. 그때
//! 마스터 자격증명을 다시 꺼낼 이유가 없다 — **모니터링 계정으로** 자기 권한을 읽으면
//! 된다(`SHOW GRANTS FOR CURRENT_USER`). 마스터를 꺼내지 않는 경로를 남겨 두는 것이
//! 최소 권한이다.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use dbmon_core::bootstrap::{AuthMethod, Desired, PrivilegeMode};
use dbmon_core::instance::Instance;
use dbmon_core::rbac::Role;
use serde::Deserialize;

use super::{ApiError, ApiState};

/// 계획 요청 본문.
#[derive(Debug, Deserialize)]
pub struct PlanRequest {
    /// 권한 모드. 없으면 설정값(기본 `broad`).
    #[serde(default)]
    pub privilege_mode: Option<PrivilegeMode>,
    /// 모드 `least` 의 스키마 화이트리스트.
    #[serde(default)]
    pub schemas: Vec<String>,
}

// **`auth_method` 를 받지 않는다** (교차 리뷰 2차가 잡았다).
//
// 받았을 때 `password` 를 고르면 `sql::create_user` 가 즉시 오류를 내고 화면은
// `invalid_bootstrap` 을 본다 — **선택할 수 있는데 실행되지 않는 옵션**이었다.
// 그건 없는 옵션보다 나쁘다.
//
// 비밀번호 폴백(M3-10)은 CSPRNG 생성 + Secrets Manager 저장 + 로테이션 Lambda 가
// 함께 있어야 완성된다. 도메인 계층에는 그 조각들이 있고(`create_user_with_password`,
// `PASSWORD_ALPHABET`, `PasswordAccountAlreadyExists`) 테스트도 있지만, 조립되지
// 않았으므로 API 는 제공하지 않는다. 조립되면 이 필드를 되살린다.

/// 실행 요청 본문.
#[derive(Debug, Deserialize)]
pub struct ApplyRequest {
    pub plan_id: String,
    /// prd 면 인스턴스 식별자를 정확히 타이핑한 값.
    #[serde(default)]
    pub confirmation: Option<String>,
}

/// 이 배포에서 부트스트랩을 할 수 있는가 — 화면이 버튼을 숨길 근거.
#[derive(Debug, serde::Serialize)]
pub struct BootstrapCapability {
    /// 마스터 자격증명 경로가 구성돼 있는가.
    pub can_create_account: bool,
    /// 왜 못 하는가 (구성돼 있으면 `None`).
    pub reason: Option<String>,
    /// 이 인스턴스의 마스터 시크릿 경로.
    pub credential_source: Option<&'static str>,
    /// 기본 권한 모드.
    pub default_privilege_mode: &'static str,
    /// 모니터링 계정 이름·호스트 패턴.
    pub monitor_user: String,
    pub monitor_host: String,
}

/// 요청의 설정을 [`Desired`] 로 옮긴다.
///
/// 기본값은 **운영 설정**에서 온다 — 요청이 매번 모드를 지정해야 하면 화면이 그 값을
/// 들고 다녀야 하고, 그러면 설정 화면과 어긋난다.
pub fn desired_from(
    req: &PlanRequest,
    monitor_user: &str,
    monitor_host: &str,
    default_mode: PrivilegeMode,
) -> Desired {
    Desired {
        user: monitor_user.to_string(),
        host: monitor_host.to_string(),
        // IAM DB 인증만 제공한다 — 위 주석 참조.
        auth: AuthMethod::IamDbAuth,
        mode: req.privilege_mode.unwrap_or(default_mode),
        schemas: req.schemas.clone(),
    }
}

/// 부트스트랩 요청의 공통 관문 — **admin + 제어 헤더 + 환경 권한.**
///
/// 세 검사를 한 함수에 두는 이유: 엔드포인트마다 흩어 두면 하나를 빠뜨리고, 빠뜨린
/// 검사가 곧 구멍이다. `instance_start` 가 같은 구조를 쓴다.
pub async fn authorize(
    state: &ApiState,
    headers: &HeaderMap,
    id: &str,
) -> Result<(dbmon_core::ids::InstanceId, Instance), ApiError> {
    use dbmon_core::ports::InstanceRegistry as _;

    let ctx = super::context_of(state, headers).await?;
    super::require_control_header(headers)?;

    // **admin 만.** 조회 권한과 같은 문턱에 둘 수 없다 — 대상 DB 를 바꾼다.
    if !ctx.has_role(Role::Admin) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "admin_required"));
    }

    let instance_id = dbmon_core::ids::InstanceId::parse(id)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_instance_id"))?;
    super::require_scope_control(
        state,
        &ctx,
        &dbmon_core::pause::PauseScope::Instance(instance_id.clone()),
    )
    .await?;

    let instance = state
        .registry
        .get(&instance_id)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "instance_not_found"))?;

    Ok((instance_id, instance))
}

/// `POST /api/instances/{id}/bootstrap/plan`.
pub async fn plan(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<PlanRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (_, instance) = authorize(&state, &headers, &id).await?;
    let svc = require_service(&state)?;
    let settings = state
        .settings
        .load(dbmon_core::time::Clock::now_ms(
            &dbmon_core::time::SystemClock,
        ))
        .await;

    let desired = desired_from(
        &req,
        &settings.bootstrap.monitor_user,
        &settings.bootstrap.monitor_host,
        settings.bootstrap.privilege_mode,
    );
    desired.validate().map_err(|e| {
        ApiError::with_body(
            StatusCode::BAD_REQUEST,
            "invalid_bootstrap",
            serde_json::json!({"reason": e.to_string()}),
        )
    })?;

    let outcome = svc
        .plan(&instance, &desired)
        .await
        .map_err(bootstrap_error)?;
    Ok(Json(serde_json::to_value(outcome).map_err(|_| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "serialize_failed")
    })?))
}

/// `POST /api/instances/{id}/bootstrap/apply`.
pub async fn apply(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ApplyRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (_, instance) = authorize(&state, &headers, &id).await?;
    let svc = require_service(&state)?;

    let outcome = svc
        .apply(&instance, &req.plan_id, req.confirmation.as_deref())
        .await
        .map_err(bootstrap_error)?;
    Ok(Json(serde_json::to_value(outcome).map_err(|_| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "serialize_failed")
    })?))
}

/// `GET /api/instances/{id}/bootstrap` — 이 인스턴스에서 무엇이 가능한가.
///
/// 화면이 버튼을 숨길 근거다. 자격증명 경로가 없으면 "계정 생성" 버튼 대신 수동
/// 스크립트만 보여준다.
pub async fn capability(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<BootstrapCapability>, ApiError> {
    let (_, instance) = authorize(&state, &headers, &id).await?;
    let settings = state
        .settings
        .load(dbmon_core::time::Clock::now_ms(
            &dbmon_core::time::SystemClock,
        ))
        .await;

    let (can, reason, source) = match state.bootstrap.as_ref() {
        None => (
            false,
            Some("이 워커에 Secrets Manager 자격증명이 없다 — 수동 스크립트를 쓴다".to_string()),
            None,
        ),
        Some(svc) => match svc.credential_route(&instance).await {
            Ok(src) => (true, None, Some(src)),
            Err(e) => (false, Some(e.to_string()), None),
        },
    };

    Ok(Json(BootstrapCapability {
        can_create_account: can,
        reason,
        credential_source: source,
        default_privilege_mode: settings.bootstrap.privilege_mode.as_str(),
        monitor_user: settings.bootstrap.monitor_user.clone(),
        monitor_host: settings.bootstrap.monitor_host.clone(),
    }))
}

/// 부트스트랩 서비스가 이 워커에 구성돼 있는가.
///
/// **`None` 이면 501 이 아니라 422 다.** 구성 문제이고, 화면이 사유를 말해야 한다 —
/// `capability` 가 같은 판정을 먼저 노출하므로 여기 오는 것은 화면이 상태를 놓친
/// 경우다.
fn require_service(
    state: &ApiState,
) -> Result<&std::sync::Arc<crate::bootstrap::BootstrapService>, ApiError> {
    state.bootstrap.as_ref().ok_or_else(|| {
        ApiError::with_body(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bootstrap_unavailable",
            serde_json::json!({"reason": "이 워커에 Secrets Manager 자격증명이 없다"}),
        )
    })
}

/// 부트스트랩 오류를 HTTP 로 옮긴다.
///
/// 상태 코드가 의미를 담아야 한다 — 화면이 "다시 계획을 만들라" 와 "타이핑하라" 와
/// "권한을 고치라" 를 구분해 안내한다.
fn bootstrap_error(e: crate::bootstrap::BootstrapError) -> ApiError {
    use crate::bootstrap::BootstrapError as E;
    let (status, code) = match &e {
        E::PlanNotFound(_) => (StatusCode::CONFLICT, "plan_stale"),
        E::Blocked(_) => (StatusCode::CONFLICT, "bootstrap_blocked"),
        E::ConfirmationRequired { .. } => (StatusCode::BAD_REQUEST, "confirmation_required"),
        E::Credentials(_) => (StatusCode::UNPROCESSABLE_ENTITY, "master_credentials"),
        E::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_bootstrap"),
        E::Domain(_) => (StatusCode::BAD_GATEWAY, "target_unavailable"),
    };
    ApiError::with_body(status, code, serde_json::json!({"reason": e.to_string()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_falls_back_to_configured_defaults() {
        let req = PlanRequest {
            privilege_mode: None,
            schemas: vec![],
        };
        let d = desired_from(&req, "dbmon", "10.1.%", PrivilegeMode::Broad);
        assert_eq!(d.mode, PrivilegeMode::Broad);
        // **IAM 방식만 나온다.** 요청이 바꿀 수 없다.
        assert_eq!(d.auth, AuthMethod::IamDbAuth);
        assert_eq!(d.user, "dbmon");
        assert!(d.validate().is_ok());
    }

    #[test]
    fn an_explicit_mode_overrides_the_default() {
        let req = PlanRequest {
            privilege_mode: Some(PrivilegeMode::Least),
            schemas: vec!["shop".into()],
        };
        let d = desired_from(&req, "dbmon", "10.1.%", PrivilegeMode::Broad);
        assert_eq!(d.mode, PrivilegeMode::Least);
        assert_eq!(d.schemas, vec!["shop"]);
    }

    /// 요청 본문의 인젝션 페이로드는 **설정에서 오는 값이 아니라** 스키마 목록으로
    /// 들어올 수 있다. `validate` 가 막는다.
    #[test]
    fn injection_in_the_schema_list_is_rejected() {
        let req = PlanRequest {
            privilege_mode: Some(PrivilegeMode::Least),
            schemas: vec!["shop`; DROP DATABASE x; --".into()],
        };
        let d = desired_from(&req, "dbmon", "10.1.%", PrivilegeMode::Broad);
        assert!(d.validate().is_err());
    }

    /// **오류마다 응답 코드가 다르다** — 화면이 안내를 갈라야 한다.
    ///
    /// `plan_stale` → "다시 계획을 만드세요", `confirmation_required` → "이름을
    /// 타이핑하세요", `bootstrap_blocked` → "권한을 고치세요" 로 갈린다. 하나로
    /// 합치면 화면이 무엇을 안내할지 알 수 없다.
    #[test]
    fn each_error_maps_to_a_distinct_response_code() {
        use crate::bootstrap::BootstrapError as E;
        let code = |e: E| format!("{:?}", bootstrap_error(e));
        assert!(code(E::PlanNotFound("p".into())).contains("plan_stale"));
        assert!(
            code(E::ConfirmationRequired {
                expected: "orders-prd-01".into()
            })
            .contains("confirmation_required")
        );
        assert!(code(E::Invalid("bad".into())).contains("invalid_bootstrap"));
        assert!(code(E::Blocked(vec![])).contains("bootstrap_blocked"));
    }

    /// 확인 값이 응답 본문에 들어가야 화면이 무엇을 타이핑할지 말할 수 있다.
    #[test]
    fn the_confirmation_error_names_the_expected_value() {
        let e = bootstrap_error(crate::bootstrap::BootstrapError::ConfirmationRequired {
            expected: "orders-prd-01".into(),
        });
        assert!(format!("{e:?}").contains("orders-prd-01"));
    }
}
