//! 설정 조회·저장 API.
//!
//! # 읽기는 viewer, 쓰기는 admin
//!
//! 설정은 "이 도구가 무엇을 보고 어디로 알리는가" 다. 읽기를 막으면 뷰어가 **왜 어떤
//! 인스턴스가 목록에 없는지** 알 수 없다(탐색 범위 밖이라는 사실이 설정에 있다).
//! 쓰기는 탐색 범위·인증 방식을 바꾸므로 admin 만이다.
//!
//! # 비밀 참조는 가려서 내보내고, 되돌아오면 원래 값을 지킨다
//!
//! 화면은 받은 값을 그대로 되돌려 보낸다. 가려진 문자열을 저장하면 참조가 파괴되고
//! **알림이 조용히 죽는다** — [`AppSettings::merge_secrets_from`] 가 그걸 막는다.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use dbmon_core::rbac::Role;
use dbmon_core::settings::{AppSettings, AuthModeSetting, SettingsProblem};
use dbmon_core::time::{Clock, SystemClock};

use super::{ApiError, ApiState, context_of, require_control_header};

/// 설정 화면이 필요한 전부. **한 번에 준다** — 조각내면 화면이 세 번 물어야 한다.
#[derive(Debug, serde::Serialize)]
pub struct SettingsView {
    /// 저장된 설정(비밀 참조는 가려져 있다).
    pub settings: AppSettings,
    /// 지금 저장된 값의 문제. 있으면 화면이 그 필드에 표시한다.
    ///
    /// **저장을 막지는 않는다** — 이미 저장된 값이므로 알려주는 것이 전부다.
    /// (저장 시점 검증은 `PUT` 이 거부한다.)
    pub problems: Vec<SettingsProblem>,
    /// 이 사용자가 저장할 수 있는가(admin).
    pub can_edit: bool,
    /// 파일 설정이 **인증 끄기**를 허용하는가. 거짓이면 화면이 그 선택을 잠근다.
    pub allow_auth_disable: bool,
    /// Cognito 검증기가 실제로 배선돼 있는가.
    ///
    /// **거짓이면 `cognito` 를 골라도 적용되지 않는다.** 검증 없이 통과시키는 것보다
    /// 낫고, 화면이 그 사실을 말해야 사용자가 "저장했는데 왜 그대로냐" 를 묻지 않는다.
    pub cognito_ready: bool,
    /// 실제로 적용 중인 로그인 방식(`off` / `token` / `cognito`).
    pub effective_auth_mode: String,
    /// 이 워커가 사는 리전. 설정이 비었을 때 탐색하는 리전이다.
    pub own_region: String,
    /// 화면의 리전 선택에 쓸 목록.
    pub known_regions: Vec<String>,
    /// **설정을 읽지 못했다면 그 사유.**
    ///
    /// 이 값이 있으면 화면이 보여주는 설정은 **마지막으로 읽은 값**(또는 기본값)이다.
    /// 그 사실을 말하지 않으면 관리자가 빈 설정을 정상으로 보고 저장을 눌러 실제
    /// 설정을 지울 수 있다(낙관적 잠금이 막지만, 화면은 사실을 말해야 한다).
    pub load_error: Option<String>,
}

fn view(state: &ApiState, settings: &AppSettings, can_edit: bool) -> SettingsView {
    SettingsView {
        problems: settings.validate(),
        effective_auth_mode: super::effective_auth_mode(state, settings)
            .as_str()
            .to_string(),
        // **탐색 루프와 같은 기본값을 쓴다.** 다른 값을 쓰면 화면이 실제로 탐색되지
        // 않는 리전을 선택기에 띄우고, 그걸 골랐을 때 목록이 빈다.
        known_regions: settings
            .discovery
            .known_regions(&state.discovery_fallback_regions),
        own_region: state.aws_region.clone(),
        can_edit,
        allow_auth_disable: state.allow_auth_disable,
        cognito_ready: super::auth::COGNITO_READY,
        load_error: state.settings.last_error(),
        settings: settings.redacted(),
    }
}

pub async fn get_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<SettingsView>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    let now_ms = SystemClock.now_ms();
    let settings = state.settings.load(now_ms).await;
    let can_edit = ctx.has_role(Role::Admin);
    Ok(Json(view(&state, &settings, can_edit)))
}

#[derive(Debug, serde::Deserialize)]
pub struct PutSettingsBody {
    /// 화면이 읽었던 버전. 어긋나면 409 다.
    pub expected_version: u32,
    pub settings: AppSettings,
}

pub async fn put_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<PutSettingsBody>,
) -> Result<Json<SettingsView>, ApiError> {
    let ctx = context_of(&state, &headers)?;
    require_control_header(&headers)?;
    if !ctx.has_role(Role::Admin) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "admin_required"));
    }
    let now_ms = SystemClock.now_ms();

    // **현재 값을 강제로 다시 읽는다.** 캐시된 값으로 병합하면 다른 워커가 방금 바꾼
    // 비밀 참조를 옛 값으로 되돌려 쓸 수 있다.
    let current = state.settings.refresh(now_ms).await;

    let mut next = body.settings;
    next.merge_secrets_from(&current);

    let problems = next.validate();
    if !problems.is_empty() {
        // 검증 실패는 **저장하지 않는다.** 어느 필드가 왜 틀렸는지 함께 돌려준다 —
        // "invalid" 하나만 주면 화면이 무엇을 고칠지 모른다.
        return Err(ApiError::with_body(
            StatusCode::BAD_REQUEST,
            "invalid_settings",
            serde_json::json!({ "problems": problems }),
        ));
    }

    let saved = state
        .settings
        .save(&next, body.expected_version, &ctx.subject, now_ms)
        .await
        .map_err(|e| match e {
            dbmon_core::error::DomainError::Conflict(_) => {
                ApiError::new(StatusCode::CONFLICT, "version_conflict")
            }
            _ => ApiError::new(StatusCode::BAD_GATEWAY, "store_unavailable"),
        })?;

    // **감사 로그.** 탐색 범위·인증 방식이 바뀌는 것은 나중에 반드시 질문거리가 된다.
    // 값 자체는 남기지 않는다(비밀 참조가 섞여 있다) — 무엇이 바뀌었는지만 남긴다.
    tracing::warn!(
        subject = %ctx.subject,
        worker = %state.worker_id,
        version = saved.version,
        auth_mode = %saved.auth.mode.as_str(),
        effective_auth_mode = %super::effective_auth_mode(&state, &saved).as_str(),
        regions = saved.discovery.regions.len(),
        accounts = saved.discovery.accounts.len(),
        multi_account = saved.discovery.multi_account_enabled,
        slack = saved.notify.slack_enabled,
        ai = saved.ai.enabled,
        "설정을 저장했다"
    );
    if super::effective_auth_mode(&state, &saved) == AuthModeSetting::Off {
        tracing::error!(
            subject = %ctx.subject,
            "⚠ 인증이 꺼졌다 — 이 배포에 접근할 수 있는 누구나 admin 이다"
        );
    }

    // **탐색을 즉시 다시 돌린다.** 리전·계정을 바꾼 사람은 그 결과를 지금 보고 싶다.
    // 다음 주기(기본 5분)를 기다리면 "저장이 안 먹었다" 로 읽힌다.
    if saved.discovery != current.discovery {
        state.controls.request_discovery();
    }

    Ok(Json(view(&state, &saved, true)))
}
