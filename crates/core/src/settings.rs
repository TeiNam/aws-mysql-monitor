//! 운영자가 **화면에서 바꾸는 설정** ([04 §3](../../../docs/04-data-model.md) `CFG/GLOBAL`).
//!
//! # 파일 설정과 무엇이 다른가
//!
//! `dbmon.toml`(`dbmon::config`)은 **배포 단위로 고정되는 것**이다 — 테이블 이름, 바인드
//! 주소, 리터럴 정책처럼 재시작이 전제인 값. 이 모듈은 **재시작 없이 바뀌어야 하는 것**을
//! 담는다: 어느 리전·계정을 탐색할지, 알림을 어디로 보낼지, 어느 모델을 쓸지.
//!
//! 둘을 섞지 않는 이유는 되돌리는 비용이 다르기 때문이다. 파일 설정이 틀리면 배포로
//! 고치고, 이 설정이 틀리면 화면에서 고친다. 후자를 파일에 두면 리전 하나를 추가하려고
//! 배포 파이프라인을 돌려야 한다(FR-OPS-05).
//!
//! # 비밀을 담지 않는다
//!
//! Slack 토큰·웹훅 URL 은 **Secrets Manager 참조(ARN)만** 저장한다
//! ([10 §3.4](../../../docs/10-alerting.md)). 이 항목은 viewer 도 읽을 수 있고
//! DynamoDB 백업·CloudTrail 로도 흐르므로, 값 자체를 두면 통제할 수 없는 곳으로 퍼진다.
//!
//! # 낙관적 잠금
//!
//! `version` 은 저장할 때마다 1 오른다. 두 관리자가 같은 화면을 열어 두고 각자 저장하면
//! **나중 저장이 앞선 변경을 조용히 지운다** — 그래서 저장은 "내가 읽은 버전" 을 함께
//! 보내고, 어긋나면 거부한다(HTTP 409).

use crate::time::EpochMs;
use serde::{Deserialize, Serialize};

/// 알림 문구의 기본값. 자리표시자는 `{}` 로 감싼다.
pub const DEFAULT_MESSAGE_TEMPLATE: &str = "{emoji} [{env}] {title} · {instance}\n{detail}";

/// 문구에서 쓸 수 있는 자리표시자 전체.
///
/// **여기 없는 이름은 오타로 본다.** `{instnace}` 를 그대로 렌더하면 알림에 그 글자가
/// 박혀 나가고, 아무도 그걸 "설정 오류" 로 읽지 않는다.
pub const TEMPLATE_PLACEHOLDERS: &[&str] = &[
    "emoji",
    "severity",
    "env",
    "instance",
    "title",
    "detail",
    "link",
    "at",
];

/// AssumeRole 대상 역할의 기본 이름. 계정마다 같은 이름으로 만드는 것이 관례다.
pub const DEFAULT_DISCOVERY_ROLE: &str = "dbmon-discovery";

/// 설정 문서 전체. **한 항목에 담는다** — 화면이 한 번에 읽고 한 번에 쓴다.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// 낙관적 잠금용. 저장 성공마다 +1.
    pub version: u32,
    pub notify: NotifySettings,
    pub discovery: DiscoverySettings,
    pub auth: AuthSettings,
    pub ai: AiSettings,
    pub updated_at_ms: EpochMs,
    /// 마지막으로 저장한 주체(감사용). 비어 있으면 아직 저장된 적이 없다.
    pub updated_by: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// 알림
// ─────────────────────────────────────────────────────────────────────────────

/// Slack 연결 방식 ([10 §3.1](../../../docs/10-alerting.md)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackMode {
    /// Incoming Webhook. URL 하나, 채널 고정.
    #[default]
    Webhook,
    /// Bot Token + 채널 ID. 채널 선택·스레드가 가능하다.
    BotToken,
}

impl SlackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::BotToken => "bot_token",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifySettings {
    pub slack_enabled: bool,
    pub slack_mode: SlackMode,
    /// Bot 방식의 채널 ID(`C01234ABCDE`) 또는 이름(`#dba-alerts`). Webhook 방식에서는
    /// 표시용이다 — 채널은 URL 에 박혀 있다.
    pub slack_channel: String,
    /// **Secrets Manager 참조.** 웹훅 URL 또는 봇 토큰이 그 안에 있다.
    /// ARN 전체이거나 시크릿 이름이다. **값 자체를 여기 두지 않는다.**
    pub slack_secret: String,
    /// 알림 문구. 자리표시자는 [`TEMPLATE_PLACEHOLDERS`].
    pub message_template: String,
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self {
            slack_enabled: false,
            slack_mode: SlackMode::default(),
            slack_channel: String::new(),
            slack_secret: String::new(),
            message_template: DEFAULT_MESSAGE_TEMPLATE.to_string(),
        }
    }
}

/// 문구를 렌더한다. **모르는 자리표시자는 그대로 남긴다** — 조용히 지우면 문장이
/// 뒤틀린 채 나가고, 남아 있으면 화면 미리보기에서 눈에 띈다.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// 문구 안의 **모르는 자리표시자**. 저장 전에 경고하는 데 쓴다.
pub fn unknown_placeholders(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '{' {
            i += 1;
            continue;
        }
        // 닫는 괄호를 찾는다. 없으면 자리표시자가 아니다.
        let Some(end) = bytes[i + 1..].iter().position(|c| *c == '}') else {
            break;
        };
        let name: String = bytes[i + 1..i + 1 + end].iter().collect();
        if !name.is_empty() && !TEMPLATE_PLACEHOLDERS.contains(&name.as_str()) {
            out.push(name);
        }
        i += end + 2;
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// 탐색 범위 (리전 · 계정)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverySettings {
    /// 탐색할 리전. **비어 있으면 워커가 사는 리전 하나만** 본다.
    ///
    /// 전체 리전을 훑지 않는 이유: 리전마다 `DescribeDBInstances` 를 부르므로 30개
    /// 리전이면 매 탐색이 30배가 되고, 대부분은 RDS 가 없는 리전이다. "전체" 를
    /// 기본값으로 두면 아무도 그 비용을 의도하지 않은 채 지불한다.
    pub regions: Vec<String>,
    /// 다른 계정도 탐색하는가. 껐으면 `accounts` 는 무시된다 —
    /// **목록을 지우지 않고 끌 수 있어야 한다**(다시 켤 때 다시 입력하게 만들지 않는다).
    pub multi_account_enabled: bool,
    pub accounts: Vec<AccountTarget>,
}

/// 다른 계정 하나. mgmt 계정이 여기 역할을 맡아 목록을 읽는다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountTarget {
    /// 12자리 계정 번호.
    pub account_id: String,
    /// 맡을 역할 이름. ARN 이 아니라 **이름만** 받는다 — 계정 번호와 합쳐 우리가
    /// 만든다. 임의 ARN 을 받으면 다른 계정의 아무 역할이나 시도할 수 있고,
    /// 그건 오타 하나로 남의 계정을 찌르는 경로다.
    pub role_name: String,
    /// 이 계정에서 볼 리전. 비어 있으면 위의 `regions` 를 쓴다.
    pub regions: Vec<String>,
    pub enabled: bool,
    /// 화면 표기용 별칭(`prod-payments`). 계정 번호만으로는 어느 팀인지 모른다.
    pub label: String,
}

impl Default for AccountTarget {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            role_name: DEFAULT_DISCOVERY_ROLE.to_string(),
            regions: Vec::new(),
            enabled: true,
            label: String::new(),
        }
    }
}

impl DiscoverySettings {
    /// 실제로 탐색할 `(계정, 리전)` 조합. 계정이 `None` 이면 **자기 계정**이다.
    ///
    /// `fallback` 은 이 설정의 리전 목록이 비었을 때 쓸 값 — 배포 설정
    /// (`aws.target_regions`, 그것도 비면 `aws.region`)이다. **화면 설정이 배포 설정을
    /// 이긴다**: 운영자가 화면에서 정한 범위가 파일보다 새로운 의도다.
    ///
    /// 여기서 빈 목록을 반환하면 탐색이 아무것도 못 찾고 등록부가 "전부 사라졌다" 로
    /// 판정될 수 있으므로, 최소 한 쌍은 항상 나온다.
    pub fn targets(&self, fallback: &[String]) -> Vec<DiscoveryTarget> {
        let base: Vec<String> = if self.regions.is_empty() {
            if fallback.is_empty() {
                Vec::new()
            } else {
                fallback.to_vec()
            }
        } else {
            self.regions.clone()
        };
        let mut out: Vec<DiscoveryTarget> = base
            .iter()
            .map(|r| DiscoveryTarget {
                account: None,
                region: r.clone(),
            })
            .collect();
        if !self.multi_account_enabled {
            return out;
        }
        for a in self.accounts.iter().filter(|a| a.enabled) {
            let regions = if a.regions.is_empty() {
                base.clone()
            } else {
                a.regions.clone()
            };
            for r in regions {
                out.push(DiscoveryTarget {
                    account: Some(a.clone()),
                    region: r,
                });
            }
        }
        out
    }

    /// 이 계정에서 맡을 역할 이름. 우리 계정이거나 목록에 없으면 `None`.
    ///
    /// **꺼진 계정도 답한다** — 토글을 끈 뒤에도 이미 등록된 인스턴스의 메트릭은
    /// 보여야 하고, 그 조회에는 여전히 그 계정의 역할이 필요하다.
    pub fn role_for(&self, account_id: &str) -> Option<&str> {
        self.accounts
            .iter()
            .find(|a| a.account_id == account_id)
            .map(|a| a.role_name.as_str())
    }

    /// 화면이 리전 선택에 쓸 목록. 탐색하는 리전 전부의 합집합이다.
    pub fn known_regions(&self, fallback: &[String]) -> Vec<String> {
        let mut v: Vec<String> = self
            .targets(fallback)
            .into_iter()
            .map(|t| t.region)
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// 탐색 한 단위. 어댑터가 이걸 보고 클라이언트를 만든다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryTarget {
    /// `None` = 워커 자신의 계정(역할을 맡지 않는다).
    pub account: Option<AccountTarget>,
    pub region: String,
}

impl DiscoveryTarget {
    /// 맡을 역할 ARN. 자기 계정이면 `None`.
    pub fn role_arn(&self) -> Option<String> {
        self.account
            .as_ref()
            .map(|a| format!("arn:aws:iam::{}:role/{}", a.account_id, a.role_name))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 인증
// ─────────────────────────────────────────────────────────────────────────────

/// 로그인 방식.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthModeSetting {
    /// **인증 없음.** 루프백 개발용이고, prd 에서는 적용되지 않는다(`effective_mode`).
    Off,
    /// 공유 토큰(`Authorization: Bearer`). 파일 설정의 토큰을 쓴다.
    #[default]
    Token,
    /// Cognito 사용자 풀 (JWT 검증).
    Cognito,
}

impl AuthModeSetting {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Token => "token",
            Self::Cognito => "cognito",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    pub mode: AuthModeSetting,
    pub cognito: CognitoSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CognitoSettings {
    /// `ap-northeast-2_AbCdEf123`. 리전 접두어가 붙어 있다.
    pub user_pool_id: String,
    pub client_id: String,
    /// 사용자 풀이 사는 리전. 비어 있으면 풀 ID 접두어에서 읽는다.
    pub region: String,
    /// 호스팅 UI 도메인(`https://dbmon.auth.ap-northeast-2.amazoncognito.com`).
    /// 로그인 화면으로 보내는 데 쓴다.
    pub domain: String,
}

impl CognitoSettings {
    /// 풀 ID 접두어에서 리전을 읽는다 — `ap-northeast-2_AbC` → `ap-northeast-2`.
    /// **따로 입력한 리전이 있으면 그것을 쓴다**(교차 리전 풀을 막지 않는다).
    pub fn effective_region(&self) -> Option<String> {
        if !self.region.is_empty() {
            return Some(self.region.clone());
        }
        self.user_pool_id
            .split_once('_')
            .map(|(r, _)| r.to_string())
            .filter(|r| !r.is_empty())
    }

    pub fn is_complete(&self) -> bool {
        !self.user_pool_id.is_empty()
            && !self.client_id.is_empty()
            && self.effective_region().is_some()
    }
}

impl AuthSettings {
    /// **실제로 적용할 방식.** `allow_off` 는 파일 설정이 "인증 끄기" 를 허용하는가다.
    ///
    /// 화면에서 인증을 끄는 것은 편리하지만, 그 화면 자체가 인증 뒤에 있다 —
    /// 즉 **끄는 순간 아무나 들어올 수 있는 상태**가 되고 되돌리려면 다시 켤 권한이
    /// 필요하다. 그래서 prd 에서는 파일 설정(`auth.allow_disable`)이 명시적으로
    /// 허용하지 않는 한 무시하고 토큰 방식으로 떨어뜨린다(fail closed).
    pub fn effective_mode(&self, allow_off: bool) -> AuthModeSetting {
        match self.mode {
            AuthModeSetting::Off if !allow_off => AuthModeSetting::Token,
            // 설정이 불완전한 Cognito 는 **모든 요청을 거부**하게 되므로 토큰으로 둔다.
            AuthModeSetting::Cognito if !self.cognito.is_complete() => AuthModeSetting::Token,
            m => m,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AI (Bedrock)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    pub enabled: bool,
    /// Bedrock 모델 ID 또는 추론 프로파일 ID.
    ///
    /// **코드에 박지 않는다** ([11 §7](../../../docs/11-ai-advisor.md)) — 모델 교체가
    /// 배포 없이 되어야 하고, 리전마다 쓸 수 있는 프로파일이 다르다.
    pub model_id: String,
    /// Bedrock 을 부를 리전. 비어 있으면 워커 리전.
    pub region: String,
    /// 출력 상한. 너무 작으면 문서가 중간에서 끊긴다.
    pub max_output_tokens: u32,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            model_id: String::new(),
            region: String::new(),
            max_output_tokens: 4_000,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 검증
// ─────────────────────────────────────────────────────────────────────────────

/// 설정 하나가 잘못됐다는 사실. **필드를 가리켜야** 화면이 그 자리에 표시할 수 있다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingsProblem {
    /// `notify.slack_secret` 처럼 점으로 이은 경로.
    pub field: String,
    pub message: String,
}

fn problem(field: &str, message: impl Into<String>) -> SettingsProblem {
    SettingsProblem {
        field: field.to_string(),
        message: message.into(),
    }
}

/// Secrets Manager 참조처럼 생겼는가 — 이름(`dbmon/channel/slack`) 또는 ARN.
///
/// **허용 목록 방식이다.** 값 자체(URL·토큰)가 들어오는 것을 막는 것이 목적이고,
/// 금지어 방식은 형식이 바뀔 때마다 뚫린다.
///
/// AWS 시크릿 이름 규칙: 영숫자와 `/_+=.@-`. ARN 은 그 앞에 `arn:…:secret:` 이 붙는다.
pub fn looks_like_secret_ref(s: &str) -> bool {
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    // URL 의 특징을 먼저 배제한다 — 이름 규칙만으로는 `https:` 가 통과할 수 있다.
    if s.contains("//") || s.contains('@') || s.contains(' ') {
        return false;
    }
    // **Slack 토큰 접두어는 따로 막는다.**
    //
    // `xoxb-…` 는 AWS 시크릿 **이름 규칙을 만족한다**(영숫자 + `-`). 그래서 모양만으로는
    // 구분할 수 없고, 이 검사가 흔한 실수(토큰을 그대로 붙여넣기)를 잡는다.
    // 보안 경계는 위의 모양 검사이고, 이건 사용성 방어다 — 새 접두어가 나오면 뚫리지만
    // 그때도 값이 URL 이 아니면 "이름" 으로는 유효하므로 최악이 아니다.
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("xox") || lower.starts_with("xapp-") {
        return false;
    }
    let body = match s.strip_prefix("arn:") {
        // ARN 이면 `secret:` 조각이 반드시 있어야 한다.
        Some(rest) => {
            if !rest.contains(":secret:") {
                return false;
            }
            rest
        }
        None => {
            // 이름 형태에는 콜론이 없다. 있으면 `scheme:` 이거나 잘린 ARN 이다.
            if s.contains(':') {
                return false;
            }
            s
        }
    };
    body
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/_+=.@-:".contains(c))
}

/// 리전 코드처럼 생겼는가 — `ap-northeast-2`, `us-east-1`, `il-central-1`.
///
/// 허용 목록을 두지 않는다: AWS 가 리전을 추가할 때마다 코드를 고쳐야 하고, 빠뜨리면
/// **실재하는 리전을 거부**한다. 모양만 본다.
pub fn looks_like_region(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() < 3 {
        return false;
    }
    let last = parts[parts.len() - 1];
    if last.is_empty() || !last.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    parts[..parts.len() - 1]
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase()))
}

impl AppSettings {
    /// 저장 전에 반드시 부른다. **빈 벡터가 아니면 저장하지 않는다.**
    ///
    /// 여기서 막지 않으면 잘못된 값이 조용히 저장되고, 증상은 몇 분 뒤 다른 곳에서
    /// 나타난다(탐색이 0건, 알림이 안 감, 모델 호출이 400). 그때는 원인이 설정이라는
    /// 것부터 의심해야 한다.
    pub fn validate(&self) -> Vec<SettingsProblem> {
        let mut p = Vec::new();

        // ── 알림
        if self.notify.slack_enabled {
            if self.notify.slack_secret.trim().is_empty() {
                p.push(problem(
                    "notify.slack_secret",
                    "Secrets Manager 참조가 필요하다 — 웹훅 URL·봇 토큰을 여기 직접 넣지 않는다",
                ));
            }
            if self.notify.slack_mode == SlackMode::BotToken
                && self.notify.slack_channel.trim().is_empty()
            {
                p.push(problem(
                    "notify.slack_channel",
                    "봇 방식은 채널이 필요하다 (`C01234ABCDE` 또는 `#dba-alerts`)",
                ));
            }
        }
        if self.notify.message_template.trim().is_empty() {
            p.push(problem("notify.message_template", "문구가 비어 있다"));
        }
        for name in unknown_placeholders(&self.notify.message_template) {
            p.push(problem(
                "notify.message_template",
                format!(
                    "모르는 자리표시자 `{{{name}}}` — 쓸 수 있는 것: {}",
                    TEMPLATE_PLACEHOLDERS.join(", ")
                ),
            ));
        }
        // **비밀이 섞여 들어오는 것을 막는다.**
        //
        // 처음에는 `http`·`xoxb-` 접두어만 걸렀다(denylist). 그건 Slack 이 토큰 형식을
        // 바꾸거나(`xapp-`·`xoxe-`), 사용자가 URL 을 다른 형태로 붙여넣으면 통과한다 —
        // 그리고 통과한 값은 **viewer 도 읽는 설정 항목에 평문으로 남는다**.
        //
        // 그래서 **허용 형태를 정한다**: Secrets Manager 의 이름 또는 ARN 처럼 생긴
        // 것만 받는다. 그 문법에 콜론·슬래시가 나오지만 공백·`?`·`#` 는 없고, URL 의
        // `//` 나 `@` 도 없다.
        let secret = self.notify.slack_secret.trim();
        if !secret.is_empty() && !looks_like_secret_ref(secret) {
            p.push(problem(
                "notify.slack_secret",
                "Secrets Manager 의 **이름 또는 ARN** 이어야 한다 — 웹훅 URL·봇 토큰 자체를 여기 적지 않는다",
            ));
        }

        // ── 탐색
        for (i, r) in self.discovery.regions.iter().enumerate() {
            if !looks_like_region(r) {
                p.push(problem(
                    &format!("discovery.regions[{i}]"),
                    format!("리전 코드 형식이 아니다: `{r}`"),
                ));
            }
        }
        for (i, a) in self.discovery.accounts.iter().enumerate() {
            if a.account_id.len() != 12 || !a.account_id.chars().all(|c| c.is_ascii_digit()) {
                p.push(problem(
                    &format!("discovery.accounts[{i}].account_id"),
                    "계정 번호는 숫자 12자리다",
                ));
            }
            if a.role_name.trim().is_empty() {
                p.push(problem(
                    &format!("discovery.accounts[{i}].role_name"),
                    "맡을 역할 이름이 필요하다",
                ));
            }
            // ARN 을 넣으면 우리가 만드는 ARN 이 이중이 된다.
            if a.role_name.contains(':') || a.role_name.contains('/') {
                p.push(problem(
                    &format!("discovery.accounts[{i}].role_name"),
                    "역할 **이름만** 적는다 (ARN 아님)",
                ));
            }
            for (j, r) in a.regions.iter().enumerate() {
                if !looks_like_region(r) {
                    p.push(problem(
                        &format!("discovery.accounts[{i}].regions[{j}]"),
                        format!("리전 코드 형식이 아니다: `{r}`"),
                    ));
                }
            }
        }
        // 같은 계정을 두 번 등록하면 인스턴스가 두 번 탐색된다(등록부는 같은 키로
        // 덮어쓰므로 결과는 같지만 호출은 두 배다).
        let mut ids: Vec<&str> = self
            .discovery
            .accounts
            .iter()
            .map(|a| a.account_id.as_str())
            .collect();
        ids.sort_unstable();
        if let Some(dup) = ids.windows(2).find(|w| w[0] == w[1]).map(|w| w[0]) {
            p.push(problem(
                "discovery.accounts",
                format!("계정 `{dup}` 이 두 번 등록됐다"),
            ));
        }

        // ── 인증
        if self.auth.mode == AuthModeSetting::Cognito {
            let c = &self.auth.cognito;
            if c.user_pool_id.trim().is_empty() {
                p.push(problem("auth.cognito.user_pool_id", "사용자 풀 ID 가 필요하다"));
            }
            if c.client_id.trim().is_empty() {
                p.push(problem("auth.cognito.client_id", "앱 클라이언트 ID 가 필요하다"));
            }
            if c.effective_region().is_none() {
                p.push(problem(
                    "auth.cognito.region",
                    "리전을 알 수 없다 — 풀 ID 가 `ap-northeast-2_...` 형식이 아니면 직접 적는다",
                ));
            }
            if !c.domain.is_empty() && !c.domain.starts_with("https://") {
                p.push(problem("auth.cognito.domain", "https:// 로 시작해야 한다"));
            }
        }

        // ── AI
        if self.ai.enabled {
            if self.ai.model_id.trim().is_empty() {
                p.push(problem("ai.model_id", "모델 ID 가 필요하다"));
            }
            if !self.ai.region.is_empty() && !looks_like_region(&self.ai.region) {
                p.push(problem("ai.region", "리전 코드 형식이 아니다"));
            }
            if self.ai.max_output_tokens < 500 {
                p.push(problem(
                    "ai.max_output_tokens",
                    "500 미만이면 문서가 중간에서 끊긴다",
                ));
            }
        }
        p
    }

    /// 화면에 내보낼 때 비밀 **참조**를 가린다.
    ///
    /// ARN 자체는 비밀이 아니지만 계정 번호를 담고 있고, 이 응답은 viewer 도 받는다.
    /// 뒤 네 글자만 남겨 "설정돼 있다" 는 사실만 전한다.
    pub fn redacted(&self) -> Self {
        let mut out = self.clone();
        out.notify.slack_secret = mask_tail(&self.notify.slack_secret);
        out
    }

    /// 가려진 값이 되돌아왔을 때 **원래 값을 지키기 위한** 병합.
    ///
    /// 화면은 가려진 문자열(`…abcd`)을 그대로 되돌려 보낸다. 그걸 저장하면 참조가
    /// 파괴되고 알림이 조용히 죽는다 — 그래서 마스킹 형태가 오면 기존 값을 유지한다.
    pub fn merge_secrets_from(&mut self, current: &AppSettings) {
        if is_masked(&self.notify.slack_secret) {
            self.notify.slack_secret = current.notify.slack_secret.clone();
        }
    }
}

const MASK_PREFIX: &str = "••••";

fn mask_tail(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let keep = s.chars().count().saturating_sub(4);
    let tail: String = s.chars().skip(keep).collect();
    format!("{MASK_PREFIX}{tail}")
}

fn is_masked(s: &str) -> bool {
    s.starts_with(MASK_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 설정이 비어 있어도 **탐색은 한 쌍을 낸다.** 빈 목록을 반환하면 탐색이 조용히
    /// 아무것도 하지 않고, 그건 "RDS 가 없다" 와 구분되지 않는다.
    #[test]
    fn empty_settings_still_scan_the_workers_own_region() {
        let d = DiscoverySettings::default();
        let t = d.targets(&["ap-northeast-2".to_string()]);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].region, "ap-northeast-2");
        assert!(t[0].account.is_none(), "자기 계정이어야 역할을 맡지 않는다");
        assert!(t[0].role_arn().is_none());
    }

    #[test]
    fn regions_replace_the_default_not_add_to_it() {
        let d = DiscoverySettings {
            regions: vec!["us-east-1".into(), "eu-west-1".into()],
            ..Default::default()
        };
        let regions: Vec<String> = d
            .targets(&["ap-northeast-2".to_string()])
            .into_iter()
            .map(|t| t.region)
            .collect();
        assert_eq!(regions, vec!["us-east-1", "eu-west-1"]);
        assert!(
            !regions.contains(&"ap-northeast-2".to_string()),
            "설정한 리전만 봐야 한다 — 자기 리전이 몰래 붙으면 목록에 없는 인스턴스가 나온다"
        );
    }

    /// 계정별 리전이 비면 상위 리전을 쓴다. 그리고 **꺼진 계정은 안 본다.**
    #[test]
    fn account_targets_expand_per_region_and_respect_the_toggle() {
        let d = DiscoverySettings {
            regions: vec!["ap-northeast-2".into()],
            multi_account_enabled: true,
            accounts: vec![
                AccountTarget {
                    account_id: "111111111111".into(),
                    regions: vec!["us-west-2".into(), "us-east-1".into()],
                    ..Default::default()
                },
                AccountTarget {
                    account_id: "222222222222".into(),
                    ..Default::default() // 리전 비움 → 상위 것
                },
                AccountTarget {
                    account_id: "333333333333".into(),
                    enabled: false,
                    ..Default::default()
                },
            ],
        };
        let t = d.targets(&["ignored-1".to_string()]);
        // 자기 계정 1 + 111(2) + 222(1) = 4
        assert_eq!(t.len(), 4, "{t:#?}");
        assert_eq!(
            t[1].role_arn().as_deref(),
            Some("arn:aws:iam::111111111111:role/dbmon-discovery")
        );
        assert!(
            !t.iter().any(|x| x.account.as_ref().is_some_and(|a| a.account_id == "333333333333")),
            "꺼진 계정을 탐색했다"
        );

        // 토글을 끄면 목록이 남아 있어도 자기 계정만 본다.
        let off = DiscoverySettings {
            multi_account_enabled: false,
            ..d
        };
        assert_eq!(off.targets(&["ignored-1".to_string()]).len(), 1);
    }

    #[test]
    fn known_regions_are_deduped_and_sorted() {
        let d = DiscoverySettings {
            regions: vec!["us-east-1".into()],
            multi_account_enabled: true,
            accounts: vec![AccountTarget {
                account_id: "111111111111".into(),
                regions: vec!["us-east-1".into(), "ap-northeast-2".into()],
                ..Default::default()
            }],
        };
        assert_eq!(d.known_regions(&["x-1".to_string()]), vec!["ap-northeast-2", "us-east-1"]);
    }

    #[test]
    fn region_shape_check_accepts_real_regions_and_rejects_junk() {
        for ok in ["ap-northeast-2", "us-east-1", "eu-central-1", "il-central-1", "ap-southeast-4"] {
            assert!(looks_like_region(ok), "{ok} 을 거부했다");
        }
        for bad in ["", "ap-northeast", "AP-NORTHEAST-2", "ap--2", "ap-northeast-x", "seoul"] {
            assert!(!looks_like_region(bad), "{bad} 를 통과시켰다");
        }
    }

    /// **인증 끄기는 허용되지 않으면 토큰으로 떨어진다** (fail closed).
    #[test]
    fn auth_off_is_ignored_unless_the_file_config_allows_it() {
        let s = AuthSettings {
            mode: AuthModeSetting::Off,
            ..Default::default()
        };
        assert_eq!(s.effective_mode(false), AuthModeSetting::Token);
        assert_eq!(s.effective_mode(true), AuthModeSetting::Off);
    }

    /// 불완전한 Cognito 설정으로 전환하면 **모든 요청이 401** 이 된다 — 그건 화면에서
    /// 되돌릴 수도 없다. 그래서 완전해질 때까지 적용하지 않는다.
    #[test]
    fn incomplete_cognito_does_not_lock_everyone_out() {
        let mut s = AuthSettings {
            mode: AuthModeSetting::Cognito,
            ..Default::default()
        };
        assert_eq!(s.effective_mode(true), AuthModeSetting::Token);

        s.cognito = CognitoSettings {
            user_pool_id: "ap-northeast-2_AbCdEf".into(),
            client_id: "1h57kf5cpq".into(),
            ..Default::default()
        };
        assert_eq!(s.effective_mode(true), AuthModeSetting::Cognito);
        assert_eq!(s.cognito.effective_region().as_deref(), Some("ap-northeast-2"));
    }

    #[test]
    fn template_placeholders_are_checked() {
        assert!(unknown_placeholders(DEFAULT_MESSAGE_TEMPLATE).is_empty());
        assert_eq!(unknown_placeholders("{instnace} 느림"), vec!["instnace"]);
        // 닫히지 않은 괄호는 자리표시자가 아니다 (JSON 을 문구에 넣는 경우).
        assert!(unknown_placeholders("{unclosed").is_empty());
    }

    #[test]
    fn template_renders_known_vars_and_leaves_unknown_visible() {
        let out = render_template("[{env}] {title} — {mystery}", &[("env", "prd"), ("title", "지연")]);
        assert_eq!(out, "[prd] 지연 — {mystery}");
    }

    /// **허용 형태만 받는다.** 금지어 방식은 토큰 형식이 바뀌면 뚫리고, 통과한 값은
    /// viewer 도 읽는 설정에 평문으로 남는다(교차 리뷰가 medium 으로 잡았다).
    #[test]
    fn only_secret_references_are_accepted() {
        for ok in [
            "dbmon/channel/slack",
            "dbmon-channel-slack",
            "arn:aws:secretsmanager:ap-northeast-2:123456789012:secret:dbmon/channel/slack-AbCd",
        ] {
            assert!(looks_like_secret_ref(ok), "정상 참조를 거부했다: {ok}");
        }
        for bad in [
            "https://hooks.slack.com/services/T000/B000/xxxx",
            "http://hooks.slack.com/x",
            "xoxb-1234-5678-abcdef",
            "xapp-1-A0-1-abc",           // 형식이 바뀐 토큰도 막힌다
            "slack.com/webhook?x=1",
            "arn:aws:s3:::bucket/key",   // secret ARN 이 아니다
            "some name with spaces",
        ] {
            assert!(!looks_like_secret_ref(bad), "값 자체를 통과시켰다: {bad}");
        }
    }

    #[test]
    fn validation_catches_a_pasted_secret() {
        for pasted in [
            "https://hooks.slack.com/services/T000/B000/xxxx",
            "xoxb-1234-5678-abcdef",
            "xapp-1-A0-1-abc",
        ] {
            let s = AppSettings {
                notify: NotifySettings {
                    slack_enabled: true,
                    slack_secret: pasted.into(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let p = s.validate();
            assert!(
                p.iter()
                    .any(|x| x.field == "notify.slack_secret" && x.message.contains("Secrets Manager")),
                "{pasted} 를 통과시켰다: {p:#?}"
            );
        }
    }

    #[test]
    fn validation_catches_bad_accounts_and_duplicates() {
        let s = AppSettings {
            discovery: DiscoverySettings {
                multi_account_enabled: true,
                accounts: vec![
                    AccountTarget {
                        account_id: "12345".into(),
                        role_name: "arn:aws:iam::1:role/x".into(),
                        ..Default::default()
                    },
                    AccountTarget {
                        account_id: "111111111111".into(),
                        ..Default::default()
                    },
                    AccountTarget {
                        account_id: "111111111111".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let fields: Vec<String> = s.validate().into_iter().map(|p| p.field).collect();
        assert!(fields.contains(&"discovery.accounts[0].account_id".to_string()));
        assert!(fields.contains(&"discovery.accounts[0].role_name".to_string()));
        assert!(fields.contains(&"discovery.accounts".to_string()), "{fields:#?}");
    }

    #[test]
    fn default_settings_are_valid() {
        assert!(AppSettings::default().validate().is_empty());
    }

    /// **가려진 값이 되돌아와도 참조가 살아 있어야 한다.** 화면은 마스킹된 문자열을
    /// 그대로 다시 보내는데, 그걸 저장하면 알림이 조용히 죽는다.
    #[test]
    fn masked_secret_round_trip_keeps_the_original() {
        let stored = AppSettings {
            notify: NotifySettings {
                slack_secret: "arn:aws:secretsmanager:ap-northeast-2:1:secret:dbmon/slack-AbCd".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let shown = stored.redacted();
        assert!(shown.notify.slack_secret.starts_with(MASK_PREFIX));
        assert!(shown.notify.slack_secret.ends_with("AbCd"));
        assert!(!shown.notify.slack_secret.contains("secretsmanager"));

        let mut incoming = shown.clone();
        incoming.merge_secrets_from(&stored);
        assert_eq!(incoming.notify.slack_secret, stored.notify.slack_secret);

        // 새 값을 실제로 입력하면 그건 그대로 저장된다.
        let mut changed = shown;
        changed.notify.slack_secret = "dbmon/slack-new".into();
        changed.merge_secrets_from(&stored);
        assert_eq!(changed.notify.slack_secret, "dbmon/slack-new");
    }

    /// 옛 항목에 없던 필드가 추가돼도 **읽을 수 있어야 한다** — `serde(default)` 가
    /// 빠지면 설정 화면이 통째로 500 이 된다.
    ///
    /// 컨테이너 수준 `#[serde(default)]` 는 **그 구조체의 `Default` 구현**으로 빈
    /// 필드를 채운다. 그래서 문구처럼 "빈 문자열이 기본값이면 안 되는" 필드도
    /// 옛 문서에서 제대로 살아난다.
    #[test]
    fn old_documents_without_new_fields_still_load() {
        // `"#` 가 들어가므로 `r##"…"##` 다 — `r#"…"#` 는 채널 이름에서 끝나 버린다.
        let json = r##"{"version":3,"notify":{"slack_channel":"#dba-alerts"}}"##;
        let s: AppSettings = serde_json::from_str(json).expect("옛 문서를 읽어야 한다");
        assert_eq!(s.version, 3);
        assert_eq!(s.notify.slack_channel, "#dba-alerts");
        assert_eq!(s.ai.max_output_tokens, 4_000, "빠진 필드는 기본값이어야 한다");
        assert_eq!(
            s.notify.message_template, DEFAULT_MESSAGE_TEMPLATE,
            "문구가 빈 문자열이면 알림이 빈 메시지로 나간다"
        );
        assert!(s.validate().is_empty(), "옛 문서가 검증을 통과해야 화면이 열린다");
    }
}
