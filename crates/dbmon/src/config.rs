//! 설정 로더 (M0-4, M2-18, FR-OPS-05).
//!
//! # 우선순위
//!
//! ```text
//! 환경변수  >  파일(TOML)  >  기본값
//! ```
//!
//! DynamoDB 3계층 병합(`GLOBAL` → `ENV#<env>` → `INST#<id>`)은 **런타임 설정**이며
//! 이 모듈이 다루는 **기동 설정**과 다르다. 기동 설정은 재시작 없이 바뀌지 않는다
//! (버킷 이름·리전·역할). 런타임 설정은 30초 폴링으로 반영된다.
//!
//! # 기동 시 검증에서 죽는다
//!
//! 필수값이 없으면 **즉시 종료**한다. 반쯤 동작하는 프로세스가 가장 나쁘다 —
//! 수집은 되지만 저장이 안 되는 상태로 몇 시간을 보내는 것보다 부팅에 실패하는 게 낫다.

use dbmon_core::env::Env;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 환경변수 접두. `DBMON__COLLECTOR__DETECT_INTERVAL_MS` 처럼 `__` 로 계층을 구분한다.
const ENV_PREFIX: &str = "DBMON__";
const ENV_SEPARATOR: &str = "__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// api + collector + scheduler + evaluator. 기본값(소규모).
    #[default]
    All,
    /// API/WS 만.
    Api,
    /// 수집만 (샤딩 대상).
    Collector,
    /// scheduler + evaluator (단일 리더).
    Control,
}

impl Role {
    /// 로그·리스 소유자 문자열에 쓴다.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Api => "api",
            Self::Collector => "collector",
            Self::Control => "control",
        }
    }

    pub fn runs_api(self) -> bool {
        matches!(self, Self::All | Self::Api)
    }
    pub fn runs_collector(self) -> bool {
        matches!(self, Self::All | Self::Collector)
    }
    pub fn runs_control(self) -> bool {
        matches!(self, Self::All | Self::Control)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub role: Role,
    /// 배포 환경. `dev` 면 탐색 필터가 **필수**가 된다 (T-37).
    pub deployment_env: Env,
    #[serde(default)]
    pub http: HttpConfig,
    pub aws: AwsConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub collector: CollectorConfig,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub bind: String,
    pub port: u16,
    /// 그레이스풀 셧다운 예산. 로드밸런서 등록 해제 + 진행 중 작업 정리.
    ///
    /// ECS 태스크 정의의 `stopTimeout` 보다 **작아야** 한다. 크면 SIGKILL 이 먼저 온다.
    pub shutdown_grace_secs: u64,
    /// 종료 시 로드밸런서 등록 해제를 기다리는 시간.
    ///
    /// 로드밸런서가 없으면(로컬 개발·collector 전용 워커) 0 이 맞다.
    /// 0이 아니면 이만큼 기다린 뒤에야 나머지 정리가 시작된다.
    pub deregistration_wait_secs: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        // 0.0.0.0 이 아니라 명시적으로 둔다 — 로컬 개발에서 127.0.0.1 로 좁힐 수 있게.
        Self {
            bind: "0.0.0.0".into(),
            port: 8080,
            shutdown_grace_secs: 45,
            deregistration_wait_secs: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    /// 앱이 배포된 리전.
    pub region: String,
    /// 탐색 대상 리전. 비어 있으면 `region` 만.
    #[serde(default)]
    pub target_regions: Vec<String>,
    /// **키에 들어가는 계정 ID** ([`dbmon_core::InstanceId`]). 필수다.
    pub account_id: String,
}
// ⚠ `aws.endpoint_url` 을 두지 않는다. 선언만 있고 읽는 곳이 없어 **조용히 무시되는
// 설정**이었고(2차 리뷰가 지적), `deny_unknown_fields` 로 그런 부류와 싸우는 코드가
// 정작 자기 필드로 그걸 만들고 있었다. 엔드포인트 재지정은 `storage.endpoint_url`
// 하나뿐이고 거기에만 게이트가 걸린다.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub data_table: String,
    pub config_table: String,
    #[serde(default)]
    pub plan_bucket: Option<String>,
    /// DynamoDB 엔드포인트 재지정. **로컬 개발 전용.**
    ///
    /// `http://127.0.0.1:18000` 을 주면 DynamoDB Local 에 붙는다. SSO 가 만료돼도
    /// 저장 경로를 돌릴 수 있다 — 이 프로젝트의 로컬 우선 원칙이다.
    /// 프로덕션에서는 비워 둔다(SDK 가 리전에서 엔드포인트를 결정한다).
    #[serde(default)]
    pub endpoint_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectorConfig {
    pub detect_interval_ms: u64,
    pub digest_interval_ms: u64,
    pub status_interval_ms: u64,
    pub health_interval_ms: u64,
    /// 슬로우 쿼리로 볼 실행시간(초). `PROCESSLIST.TIME` 이 초 단위라 정수다.
    pub slow_threshold_secs: u32,
    /// **탐지 쿼리 전용** 타임아웃. tick 예산(폴링 주기의 80%) 안에 들어야 한다
    /// ([02 §7](../../../docs/02-architecture.md)).
    ///
    /// 심층 조회·플랜과 **다른 값이다.** 초기 구현은 하나로 뭉쳤는데, 그러면
    /// 3초 타임아웃이 1초 케이던스를 넘어 tick 이 겹친다 (교차 검증이 잡았다).
    pub detect_timeout_ms: u64,
    /// 심층 조회·플랜 수집 타임아웃. tick 밖의 병렬 태스크에서 돈다.
    pub query_timeout_ms: u64,
    /// `daily` 루프처럼 무거운 조회.
    pub bulk_query_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    /// `detect` 쿼리 `LIMIT`. 폭주 방어.
    pub detect_limit: u32,
    /// 심층 조회 대상 상한 (느린 순 상위 N).
    pub deep_probe_limit: u32,
    pub digest_top_n: usize,
    pub digest_threshold_ms: i64,
    /// 모니터링 DB 계정명. 자기 제외의 기준 ([05 §10](../../../docs/05-collector.md)).
    pub monitor_db_user: String,
    /// 고아 `in_flight` 스윕 주기 (초). 기본 5분 ([05 §4.5](../../../docs/05-collector.md) F4).
    #[serde(default = "default_orphan_sweep_secs")]
    pub orphan_sweep_secs: u64,
    /// 슬로우로그 백필 주기 (초). 기본 1분.
    ///
    /// 실행별 정확 지표의 유일한 출처이므로([19 §G2]) 너무 느리면 화면의 지표가
    /// 오래 비어 있다. 너무 빠르면 CloudWatch Logs API 레이트 리밋에 걸린다.
    #[serde(default = "default_backfill_secs")]
    pub backfill_secs: u64,
    /// 리터럴 저장 정책 (FR-CAP-07, [OPEN-Q-15](../../../docs/OPEN-QUESTIONS.md)).
    ///
    /// # 왜 기본값이 `masked` 인가 — 의도된 이탈이다
    ///
    /// FR-CAP-07 은 `prd → full_restricted` 를 규정한다. 근거는 타당하다: **저장 시점
    /// 마스킹은 되돌릴 수 없고**, 노출 시점 통제는 되돌릴 수 있으면서 동등하게 안전하다.
    ///
    /// 그런데 그 선택은 "운영 데이터의 리터럴을 이 시스템에 저장해도 되는가" 라는
    /// **조직 규정 판단**에 달려 있고(OPEN-Q-15), 그건 코드가 답할 수 없다.
    /// 그리고 두 방향의 실수 비용이 비대칭이다:
    ///
    /// | 잘못된 기본값 | 되돌리는 방법 |
    /// |---|---|
    /// | `masked` 인데 `full_restricted` 여야 했다 | 설정 한 줄 → **이후 데이터는 원문** |
    /// | `full_restricted` 인데 `masked` 여야 했다 | **불가능** — 이미 저장된 리터럴은 남는다 |
    ///
    /// 그래서 **되돌릴 수 있는 쪽을 기본값으로** 둔다. 기제는 준비돼 있고 전환은
    /// 실제로 한 줄이다. 대가는 FR-DGS-06(복사해서 바로 실행하는 샘플 쿼리)이
    /// prd 에서 동작하지 않는 것이고, 기동 로그가 그 사실을 매번 알린다.
    #[serde(default = "default_literal_policy")]
    pub literal_policy: dbmon_core::slow_query::LiteralPolicy,
    /// 로컬 슬로우로그 파일 경로. **개발 전용** — 있으면 CloudWatch 대신 이걸 읽는다.
    ///
    /// SSO 가 만료돼도 백필 경로를 끝까지 돌릴 수 있게 한다.
    #[serde(default)]
    pub slowlog_file: Option<String>,
}

fn default_literal_policy() -> dbmon_core::slow_query::LiteralPolicy {
    dbmon_core::slow_query::LiteralPolicy::Masked
}

fn default_orphan_sweep_secs() -> u64 {
    300
}

fn default_backfill_secs() -> u64 {
    60
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            detect_interval_ms: 1_000,
            digest_interval_ms: 60_000,
            status_interval_ms: 5_000,
            health_interval_ms: 30_000,
            slow_threshold_secs: 2,
            detect_timeout_ms: 800,
            query_timeout_ms: 3_000,
            bulk_query_timeout_ms: 30_000,
            connect_timeout_ms: 5_000,
            detect_limit: 500,
            deep_probe_limit: 50,
            digest_top_n: 200,
            digest_threshold_ms: 100,
            monitor_db_user: "dbmon".into(),
            literal_policy: default_literal_policy(),
            orphan_sweep_secs: default_orphan_sweep_secs(),
            backfill_secs: default_backfill_secs(),
            slowlog_file: None,
        }
    }
}

/// 탐색 필터 — **prd 혼재 계정 격리의 마지막 방어선** (T-37, M0-13f).
///
/// 개발계 계정에 프로덕션 워크로드가 함께 있다([18 §6](../../../docs/18-dev-environment.md)).
/// 네트워크 격리가 1차 방어선이지만, IAM 의 `rds:DescribeDBInstances` 는 `Resource:"*"` 라
/// prd 인스턴스도 **보인다.** 여기서 걸러야 레지스트리에 등록되지 않는다.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// 탐색 주기 (초). 기본 5분 (FR-DSC-02).
    ///
    /// `detect_interval_ms`(1초)와 **다른 시간축이다.** 탐색은 AWS API 를 치므로
    /// 조절(throttling) 대상이고, 인스턴스 목록은 초 단위로 바뀌지 않는다.
    #[serde(default = "default_discovery_interval_secs")]
    pub interval_secs: u64,
    /// 이 VPC 안의 인스턴스만 수집한다. **AND 조건**이다.
    #[serde(default)]
    pub allowed_vpc_ids: Vec<String>,
    /// 이 태그를 가진 인스턴스만. `키=값` 형태.
    #[serde(default)]
    pub required_tags: Vec<String>,
    /// 이름에 이 문자열이 들어가면 제외한다. 화이트리스트가 통과시켜도 거부한다.
    ///
    /// 비프로덕션 배포에서는 기본값이 **합쳐진다**(대체되지 않는다) —
    /// [`Config::apply_derived_defaults`] 참고.
    #[serde(default)]
    pub denied_name_substrings: Vec<String>,
    /// 환경 태그가 prd 인 인스턴스를 거부한다. 비프로덕션 배포에서 기본 `true`.
    #[serde(default)]
    pub reject_production_tags: bool,
}

fn default_discovery_interval_secs() -> u64 {
    300
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_discovery_interval_secs(),
            allowed_vpc_ids: Vec::new(),
            required_tags: Vec::new(),
            denied_name_substrings: Vec::new(),
            reject_production_tags: false,
        }
    }
}

impl DiscoveryConfig {
    /// 기본 거부 문자열. dev 배포가 prd 를 집어올 가능성을 줄인다.
    pub fn default_denied_for_dev() -> Vec<String> {
        ["prd", "prod", "production"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub field: String,
    pub reason: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "설정 오류 [{}]: {}", self.field, self.reason)
    }
}
impl std::error::Error for ConfigError {}

/// 탐색 주기 하한 (초). AWS API 조절을 피하는 최소값.
pub const MIN_DISCOVERY_INTERVAL_SECS: u64 = 30;

/// 주기 잡(고아 스윕·백필)의 하한. `0` 은 매 tick 반복이라 API 폭주가 된다.
pub const MIN_JOB_INTERVAL_SECS: u64 = 5;
/// 주기 잡의 상한 (24시간). `secs * 1000` 오버플로도 함께 막는다.
pub const MAX_JOB_INTERVAL_SECS: u64 = 86_400;

/// 로컬 개발용 엔드포인트인가. **호스트가 루프백이어야 한다.**
///
/// URL 파서 의존성을 넣지 않는다 — 스킴과 호스트만 보면 충분하다.
///
/// ⚠ **접두 비교로 판정하지 않는다.** 처음에는 `host.starts_with("127.")` 을 썼는데
/// `127.0.0.1.attacker.example` 이 통과했다(테스트가 잡았다). 주소를 실제로 파싱해
/// `is_loopback()` 에 맡긴다 — 판정을 직접 적으면 이런 구멍이 계속 생긴다.
fn is_loopback_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // `host:port/path` 에서 호스트만 뗀다. IPv6 은 `[::1]:8000` 형태다.
    let host = if rest.starts_with('[') {
        match rest.find(']') {
            Some(end) => &rest[1..end],
            None => return false,
        }
    } else {
        rest.split([':', '/']).next().unwrap_or("")
    };

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// ECS 태스크 안에서 돌고 있는가.
///
/// ECS 에이전트가 태스크마다 `ECS_CONTAINER_METADATA_URI_V4` 를 주입한다.
/// **부재를 조건으로 쓴다** — 위험한 방향은 "프로덕션에서 로컬 예외가 켜지는"
/// 것이므로, 프로덕션에서 항상 존재하는 값의 부재로 판정해야 안전하다.
pub fn on_ecs() -> bool {
    std::env::var_os("ECS_CONTAINER_METADATA_URI_V4").is_some()
}

/// 로컬 컨테이너 샌드박스인가 — **ECS 가 아닌 컨테이너 안.**
///
/// ECS 태스크도 `/.dockerenv` 를 가지므로 그것만으로는 판정할 수 없다.
/// 두 사실의 곱이어야 한다.
pub fn is_local_container_sandbox() -> bool {
    std::path::Path::new("/.dockerenv").exists() && !on_ecs()
}

/// 엔드포인트 재지정이 허용되는가. **순수 함수** — 프로세스 환경을 읽지 않는다.
///
/// # 왜 컨테이너에 예외가 필요한가
///
/// 컨테이너 안에서 `127.0.0.1` 은 **컨테이너 자신**이다. 호스트에 띄운 DynamoDB
/// Local 에 닿으려면 컴포즈 네트워크의 서비스 이름(`http://dynamodb:8000`)을 써야
/// 하는데 그건 루프백이 아니다. 즉 루프백만 허용하면 **컨테이너로 로컬 개발을 할
/// 수 없다** — 그리고 그게 이 프로젝트의 전제(SSO 만료와 무관하게 개발)를 깬다.
///
/// # 그래도 프로덕션이 안전한 이유
///
/// `deployment_env == dev` 가 여전히 필수다. prd·stg·unknown 은 URL 이 무엇이든,
/// 컨테이너 안이든 밖이든 거부된다. 그리고 [`is_local_container_sandbox`] 가 ECS 를
/// 배제하므로 dev ECS 태스크도 이 예외를 얻지 못한다.
pub fn endpoint_override_allowed(env: Env, url: &str, in_local_container: bool) -> bool {
    env == Env::Dev && (is_loopback_url(url) || in_local_container)
}

fn err(field: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError {
        field: field.to_string(),
        reason: reason.into(),
    }
}

impl Config {
    /// 파일 + 환경변수로 설정을 만든다.
    ///
    /// # 3단 병합
    ///
    /// ```text
    /// 기본값 문서  ←  파일(TOML)  ←  환경변수
    /// ```
    ///
    /// **기본값을 먼저 완전한 문서로 만든다.** 그래야 (a) 부분 섹션 오버라이드가 되고
    /// (`DBMON__COLLECTOR__DETECT_INTERVAL_MS` 하나만 줘도 나머지가 채워진다),
    /// (b) 환경변수의 **타입을 기존 값에서 추론**할 수 있다.
    ///
    /// (b)가 중요한 이유: `DBMON__AWS__ACCOUNT_ID=123456789012` 을 휴리스틱으로 파싱하면
    /// 정수가 되어 `String` 역직렬화가 실패한다. 계정 ID 를 환경변수로 주는 것은
    /// 컨테이너 배포의 기본 형태이므로 이건 프로덕션을 막는 결함이다.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut root = default_document();
        if let Some(p) = path {
            let text = std::fs::read_to_string(p)
                .map_err(|e| err("file", format!("{}: {e}", p.display())))?;
            let from_file: toml::Value =
                toml::from_str(&text).map_err(|e| err("file", format!("TOML 파싱 실패: {e}")))?;
            merge(&mut root, from_file);
        }
        apply_env_overrides(&mut root, std::env::vars());
        Self::from_value(root)
    }

    fn from_value(root: toml::Value) -> Result<Self, ConfigError> {
        let mut cfg: Config = root.try_into().map_err(|e| {
            err(
                "deserialize",
                format!("{e} — 값의 타입이 맞지 않거나 알 수 없는 키가 있다"),
            )
        })?;
        cfg.apply_derived_defaults();
        cfg.validate()?;
        Ok(cfg)
    }

    /// 다른 값에서 유도되는 기본값을 채운다. **검증 전에** 부른다.
    ///
    /// # 왜 serde `default` 로 안 되는가
    ///
    /// `deployment_env` 를 봐야 정할 수 있는 값이라 필드 단위 기본값으로 표현할 수 없다.
    fn apply_derived_defaults(&mut self) {
        // **dev·stg 배포는 이름 거부 목록을 기본으로 켠다** (T-37 심층 방어).
        //
        // VPC 필터가 1차 방어선이지만, prd 인스턴스가 dev VPC 안에 잘못 배치되면
        // VPC 조건을 통과한다. 이름 거부가 그걸 잡는다 — `filter.rs` 의
        // `name_deny_overrides_vpc_allow` 가 그 동작을 고정한다.
        //
        // ⚠ 이 호출부가 없어서 **기본 거부 목록이 한 번도 적용되지 않았다.**
        // 함수는 있었고 테스트도 있었지만 프로덕션 경로가 부르지 않았다 —
        // 이 프로젝트에서 다섯 번째로 재발한 부류다.
        if self.deployment_env != Env::Prd {
            // **대체가 아니라 합친다.** 비어 있을 때만 채우면, 운영자가
            // `denied_name_substrings = ["canary"]` 한 줄을 더하는 순간 `prd`/`prod`/
            // `production` 이 조용히 사라진다 — 설정을 좁히려는 행위가 방어선을 넓힌다
            // (2차 리뷰가 지적).
            for d in DiscoveryConfig::default_denied_for_dev() {
                if !self
                    .discovery
                    .denied_name_substrings
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(&d))
                {
                    self.discovery.denied_name_substrings.push(d);
                }
            }
            // 태그 기반 prd 거부도 기본으로 켠다. 이름 거부만으로는
            // `Environment=production` + 무해한 이름 조합을 잡지 못한다.
            self.discovery.reject_production_tags = true;
        }
    }

    /// 값 범위와 **교차 검증** ([14 §8.9](../../../docs/14-infrastructure.md)).
    ///
    /// 필수값 확인도 여기서 한다. 기본값 문서에 빈 자리표가 들어 있으므로 serde 의
    /// "missing field" 대신 **어느 값이 왜 필요한지** 말해 줄 수 있다.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.aws.account_id.is_empty() {
            return Err(err(
                "aws.account_id",
                "필수다. instance_id 의 첫 성분이므로 나중에 바꿀 수 없다 (M0-2a)",
            ));
        }
        if self.aws.account_id.len() != 12
            || !self.aws.account_id.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(err("aws.account_id", "12자리 숫자여야 한다"));
        }
        if self.aws.region.is_empty() {
            return Err(err("aws.region", "필수다"));
        }
        if self.storage.data_table.is_empty() || self.storage.config_table.is_empty() {
            return Err(err("storage", "data_table 과 config_table 은 필수다"));
        }
        let c = &self.collector;
        if !(200..=60_000).contains(&c.detect_interval_ms) {
            return Err(err("collector.detect_interval_ms", "200~60000 이어야 한다"));
        }
        if !(1..=3_600).contains(&c.slow_threshold_secs) {
            return Err(err("collector.slow_threshold_secs", "1~3600 이어야 한다"));
        }
        if !(1..=10_000).contains(&c.detect_limit) {
            return Err(err("collector.detect_limit", "1~10000 이어야 한다"));
        }
        if c.deep_probe_limit > c.detect_limit {
            return Err(err(
                "collector.deep_probe_limit",
                "detect_limit 보다 클 수 없다 — 탐지되지 않은 후보를 심층 조회할 수 없다",
            ));
        }
        dbmon_core::rollup::RollupConfig {
            top_n: c.digest_top_n,
            threshold_ms: c.digest_threshold_ms,
        }
        .validate()
        .map_err(|e| err("collector.digest_*", e))?;
        if c.monitor_db_user.is_empty() {
            return Err(err(
                "collector.monitor_db_user",
                "비어 있으면 자기 제외가 동작하지 않아 통계가 오염된다",
            ));
        }

        // **교차 검증** — 탐지 타임아웃이 tick 예산(주기의 80%)을 넘으면 tick 이 겹친다.
        let tick_budget = c.detect_interval_ms * 80 / 100;
        // `connect_timeout_ms + detect_timeout_ms <= tick_budget` 은 **검증하지 않는다.**
        // 커넥션 획득 5초는 콜드 경로(TLS 핸드셰이크 + IAM 토큰 인증)에 실제로 필요하고,
        // 합을 강제하면 detect 예산이 200ms 로 밀려 정상 설정이 거부된다.
        // 대신 `probe` 가 **획득+조회 전체**를 한 interval 안으로 묶는다
        // (`crates/dbmon/src/mysql/mod.rs`) — tick 이 겹쳐 쌓이지 않게 하는 것이 목적이다.
        // **모든 루프 주기를 검증한다.** `detect_interval_ms` 만 검사하면 나머지가 0 일 때
        // 핫 루프가 되어 대상 DB 와 CPU 를 태운다. 0 은 "비활성" 이 아니라 "즉시 반복" 이다.
        for (name, value, min, max) in [
            (
                "collector.digest_interval_ms",
                c.digest_interval_ms,
                10_000,
                3_600_000,
            ),
            (
                "collector.status_interval_ms",
                c.status_interval_ms,
                1_000,
                600_000,
            ),
            (
                "collector.health_interval_ms",
                c.health_interval_ms,
                5_000,
                3_600_000,
            ),
        ] {
            if !(min..=max).contains(&value) {
                return Err(err(
                    name,
                    format!("{min}~{max}ms 이어야 한다 (받은 값 {value})"),
                ));
            }
        }

        // `detect_timeout_ms` 가 tick 예산보다 크면 **바깥 상한이 항상 먼저 발동**해
        // 설정이 조용히 무시된다. 아래 검증이 그걸 막는다 (M27/M3 와 같은 부류).
        if c.detect_timeout_ms > tick_budget {
            return Err(err(
                "collector.detect_timeout_ms",
                format!(
                    "tick 예산 {tick_budget}ms(detect_interval_ms {} 의 80%) 안에 들어야 한다. \
                     넘으면 tick 이 겹쳐 탐지 해상도가 조용히 떨어진다",
                    c.detect_interval_ms
                ),
            ));
        }
        if self.http.deregistration_wait_secs >= self.http.shutdown_grace_secs {
            return Err(err(
                "http.deregistration_wait_secs",
                "shutdown_grace_secs 보다 작아야 한다 — 등록 해제만 기다리다 \
                 버퍼 플러시를 못 하면 데이터를 잃는다",
            ));
        }
        if c.bulk_query_timeout_ms < c.query_timeout_ms {
            return Err(err(
                "collector.bulk_query_timeout_ms",
                "query_timeout_ms 보다 작을 수 없다 — daily 루프가 더 무겁다",
            ));
        }

        // **T-37 게이트** — 탐색 필터 없이 기동할 수 있는 것은 `prd` 하나뿐이다.
        //
        // 이전 구현은 `== Env::Dev` 만 막았다. 그런데 기본값은 `unknown` 이고
        // (`Env::Unknown.treat_as_production() == true`), `unknown` 으로 뜬 배포는
        // 다른 모든 곳에서 프로덕션 취급을 받으면서 **이 게이트만 통과했다.**
        // `rds:DescribeDBInstances` 는 `Resource:"*"` 라 계정 내 prd 인스턴스가 다 보이므로,
        // 안전해 보이는 기본값이 최후 방어선을 무력화하고 있었다.
        //
        // `prd` 를 예외로 두는 이유: prd 배포는 계정 전체를 수집하는 것이 의도다.
        // 그 의도를 밝히려면 `deployment_env` 를 명시적으로 `prd` 로 적어야 한다.
        // **`monitor_db_user` 는 영숫자·`_`·`-` 만 허용한다.**
        //
        // 이 값은 IAM 토큰의 **서명 대상 URI** 에 보간된다
        // (`?Action=connect&DBUser=<user>`). `#` 이 들어가면 그 뒤가 프래그먼트로
        // 취급돼 `DBUser` 가 서명 대상에서 빠지고, `&` 는 파라미터를 주입한다.
        // 증상은 언제나 `Access denied` 이고 원인이 IAM 정책처럼 보인다.
        //
        // MySQL 사용자명은 `@`·`#`·`%`·공백·비ASCII 를 허용하므로 경계에서 좁힌다.
        if !self
            .collector
            .monitor_db_user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(err(
                "collector.monitor_db_user",
                "영숫자·`_`·`-` 만 쓴다 — IAM 토큰의 서명 대상 URI 에 보간되므로 \
                 예약문자가 서명을 깨뜨린다",
            ));
        }

        // **비-dev 배포에 폴백 비밀번호가 주입돼 있으면 거부한다.**
        //
        // 게이트 자체는 정확하다(`build_auth` 가 dev 를 요구한다). 문제는 **관측**이다 —
        // 조용히 무시하면 그 변수가 거기 있는 줄 아무도 모르고, 아무도 로테이션하지
        // 않는다. 기동을 막아 존재를 드러낸다.
        if self.deployment_env != Env::Dev
            && std::env::var_os(crate::aws::auth_token::TARGET_PASSWORD_ENV).is_some()
        {
            return Err(err(
                crate::aws::auth_token::TARGET_PASSWORD_ENV,
                "deployment_env=dev 가 아닌 배포에 폴백 비밀번호가 주입돼 있다 — \
                 태스크 정의에서 제거하고 값을 로테이션한다",
            ));
        }

        // **새 주기 필드도 같은 규칙을 받는다.**
        //
        // `0` 은 "비활성" 이 아니라 "매 tick 반복" 이다. `backfill_secs = 0` 이면
        // 창이 비어 **백필이 조용히 아무것도 하지 않으면서** 인스턴스당 초당 1회
        // CloudWatch 를 때린다(500대면 확정적으로 조절당하고 그 오류는 debug 다).
        // 상한도 둔다 — `secs * 1000` 오버플로를 막는다.
        for (name, value) in [
            (
                "collector.orphan_sweep_secs",
                self.collector.orphan_sweep_secs,
            ),
            ("collector.backfill_secs", self.collector.backfill_secs),
        ] {
            if !(MIN_JOB_INTERVAL_SECS..=MAX_JOB_INTERVAL_SECS).contains(&value) {
                return Err(err(
                    name,
                    format!(
                        "{MIN_JOB_INTERVAL_SECS}~{MAX_JOB_INTERVAL_SECS} 이어야 한다 \
                         (받은 값: {value}). 0 은 비활성이 아니라 매 tick 반복이다"
                    ),
                ));
            }
        }

        // **로컬 슬로우로그 파일은 `dev` 에서만 허용한다.**
        //
        // prd 에서 켜지면 CloudWatch 대신 파일을 읽어 **백필이 조용히 아무것도
        // 하지 않는다** — 실행별 정확 지표가 영구히 비고, 원인은 "지표가 없다" 로만
        // 보인다.
        if self.deployment_env != Env::Dev && self.collector.slowlog_file.is_some() {
            return Err(err(
                "collector.slowlog_file",
                "로컬 슬로우로그 파일은 deployment_env=dev 에서만 쓴다 \
                 (prd 는 CloudWatch Logs 를 읽는다)",
            ));
        }

        // **엔드포인트 재지정은 `dev` + 루프백에서만 허용한다.**
        //
        // `endpoint_url` 이 설정되면 조립부가 더미 자격증명을 넣는다(로컬 개발 경로).
        // 그게 프로덕션에서 켜지면 태스크 롤이 무시되고, 실패는 "저장이 안 된다" 로만
        // 나타나 원인을 찾기 어렵다.
        //
        // ⚠ 처음에는 `deployment_env == Prd` 만 막았다. 그런데 **기본값이 `unknown`**
        // 이고 `Env::Unknown.treat_as_production()` 은 `true` 다 — 다른 모든 곳이
        // 프로덕션으로 취급하는 값을 이 게이트만 통과시켰다. `DEPLOYMENT_ENV` 를
        // 빠뜨린 배포가 조용히 더미 자격증명으로 뜬다(2차 리뷰가 지적).
        // 그래서 **허용 목록 방식으로 뒤집는다.**
        if let Some(url) = &self.storage.endpoint_url {
            if self.deployment_env != Env::Dev {
                return Err(err(
                    "storage.endpoint_url",
                    format!(
                        "엔드포인트 재지정은 deployment_env=dev 에서만 쓴다 (현재: {}). \
                         더미 자격증명이 태스크 롤을 가린다",
                        self.deployment_env
                    ),
                ));
            }
            // **루프백만 허용한다.** 없으면 비프로덕션 배포가 임의의 외부 주소로
            // 인스턴스 메타데이터를 보낼 수 있다. 로컬 컨테이너는 예외 —
            // 아래 판정 함수의 문서에 이유를 적었다.
            if !endpoint_override_allowed(self.deployment_env, url, is_local_container_sandbox()) {
                return Err(err(
                    "storage.endpoint_url",
                    "루프백 주소만 허용한다 (127.0.0.1 / localhost / [::1]). \
                     컨테이너 안에서는 컴포즈 서비스 이름도 허용한다",
                ));
            }
        }

        // **탐색 주기 하한.** 0 이면 API 를 핫 루프로 때려 조절당하고, 조절은 부분
        // 결과로 나타나 FR-DSC-07 판정을 흔든다.
        if self.discovery.interval_secs < MIN_DISCOVERY_INTERVAL_SECS {
            return Err(err(
                "discovery.interval_secs",
                format!(
                    "{MIN_DISCOVERY_INTERVAL_SECS}초 이상이어야 한다 (받은 값: {})",
                    self.discovery.interval_secs
                ),
            ));
        }
        if self.deployment_env != Env::Prd && self.discovery.allowed_vpc_ids.is_empty() {
            return Err(err(
                "discovery.allowed_vpc_ids",
                "deployment_env 가 prd 가 아니면 필수다. 개발계 계정에 프로덕션 워크로드가 \
                 함께 있을 수 있고 IAM 의 rds:DescribeDBInstances 는 Resource:\"*\" 이므로 \
                 prd 인스턴스도 보인다. 계정 전체를 수집할 의도라면 deployment_env=prd 로 \
                 명시한다 (T-37)",
            ));
        }
        Ok(())
    }

    pub fn target_regions(&self) -> Vec<String> {
        if self.aws.target_regions.is_empty() {
            vec![self.aws.region.clone()]
        } else {
            self.aws.target_regions.clone()
        }
    }
}

/// 기본값으로 채운 완전한 문서. **필수값은 빈 자리표**로 둔다.
///
/// 자리표를 두는 이유는 두 가지다.
/// 1. 환경변수 오버라이드가 타입을 추론할 수 있다 (`account_id` 는 문자열이다).
/// 2. `validate()` 가 serde 보다 나은 오류 메시지를 낼 수 있다.
fn default_document() -> toml::Value {
    let mut t = toml::map::Map::new();
    t.insert("role".into(), toml::Value::String("all".into()));
    // 배포 환경은 기본값을 두지 않는다 — 잘못 추측하면 prd 를 dev 로 다룬다.
    // 자리표로 `unknown` 을 넣으면 `treat_as_production() == true` 라 안전한 쪽이다.
    t.insert(
        "deployment_env".into(),
        toml::Value::String("unknown".into()),
    );
    t.insert("http".into(), to_value(HttpConfig::default()));
    t.insert("collector".into(), to_value(CollectorConfig::default()));
    t.insert("discovery".into(), to_value(DiscoveryConfig::default()));
    t.insert(
        "aws".into(),
        to_value(AwsConfig {
            region: String::new(),
            target_regions: Vec::new(),
            account_id: String::new(),
        }),
    );
    t.insert(
        "storage".into(),
        to_value(StorageConfig {
            data_table: String::new(),
            config_table: String::new(),
            plan_bucket: None,
            endpoint_url: None,
        }),
    );
    toml::Value::Table(t)
}

fn to_value<T: Serialize>(v: T) -> toml::Value {
    toml::Value::try_from(v).expect("기본값은 항상 직렬화된다")
}

/// `overlay` 를 `base` 에 덮어쓴다. 테이블은 재귀 병합, 그 외는 교체.
fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// `DBMON__A__B=1` → `a.b = 1`. **타입은 기존 값에서 추론한다.**
fn apply_env_overrides(root: &mut toml::Value, vars: impl Iterator<Item = (String, String)>) {
    for (key, value) in vars {
        let Some(path) = key.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let segments: Vec<String> = path
            .split(ENV_SEPARATOR)
            .map(|s| s.to_lowercase())
            .collect();
        if segments.iter().any(|s| s.is_empty()) {
            continue;
        }
        set_path(root, &segments, &value);
    }
}

/// 기존 값의 타입에 맞춰 문자열을 변환한다.
///
/// 타입 힌트가 없으면(새 키) 문자열로 둔다 — **추측하지 않는다.**
/// 초기 구현은 "숫자로 파싱되면 정수"라는 휴리스틱을 썼는데, 그러면
/// `DBMON__AWS__ACCOUNT_ID=123456789012` 이 정수가 되어 역직렬화가 실패한다.
fn coerce(existing: Option<&toml::Value>, raw: &str) -> toml::Value {
    match existing {
        Some(toml::Value::Integer(_)) => raw
            .parse::<i64>()
            .map(toml::Value::Integer)
            .unwrap_or_else(|_| toml::Value::String(raw.to_string())),
        Some(toml::Value::Float(_)) => raw
            .parse::<f64>()
            .map(toml::Value::Float)
            .unwrap_or_else(|_| toml::Value::String(raw.to_string())),
        Some(toml::Value::Boolean(_)) => raw
            .parse::<bool>()
            .map(toml::Value::Boolean)
            .unwrap_or_else(|_| toml::Value::String(raw.to_string())),
        // 배열은 쉼표로 나눈다. **원소 1개도 배열이다** — 단일 VPC 를 지정하는 것이
        // 가장 흔한 경우이므로 이걸 문자열로 두면 T-37 게이트를 켤 수 없다.
        Some(toml::Value::Array(_)) => toml::Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_string()))
                .collect(),
        ),
        _ => toml::Value::String(raw.to_string()),
    }
}

fn set_path(root: &mut toml::Value, segments: &[String], raw: &str) {
    let Some((head, rest)) = segments.split_first() else {
        return;
    };
    let toml::Value::Table(table) = root else {
        return;
    };
    if rest.is_empty() {
        let value = coerce(table.get(head), raw);
        table.insert(head.clone(), value);
        return;
    }
    let child = table
        .entry(head.clone())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    if !child.is_table() {
        *child = toml::Value::Table(toml::map::Map::new());
    }
    set_path(child, rest, raw);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
deployment_env = "prd"
[aws]
region = "ap-northeast-2"
account_id = "123456789012"
[storage]
data_table = "dbmon-data"
config_table = "dbmon-config"
"#
    }

    /// `Config::load` 와 **같은 3단 병합**을 거친다. 테스트가 실제 경로를 검증해야 한다.
    fn load_str(text: &str, envs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut root = default_document();
        merge(&mut root, toml::from_str(text).expect("테스트 TOML"));
        apply_env_overrides(
            &mut root,
            envs.iter().map(|(k, v)| (k.to_string(), v.to_string())),
        );
        Config::from_value(root)
    }

    #[test]
    fn minimal_config_loads_with_defaults() {
        let c = load_str(minimal_toml(), &[]).unwrap();
        assert_eq!(c.role, Role::All);
        assert_eq!(c.http.port, 8080);
        assert_eq!(c.collector.detect_interval_ms, 1_000);
        assert_eq!(c.collector.digest_top_n, 200);
        assert_eq!(c.target_regions(), vec!["ap-northeast-2"]);
    }

    /// 필수값이 없으면 **어느 값이 왜 필요한지** 말해야 한다.
    #[test]
    fn missing_required_field_fails_loudly() {
        let e = load_str("deployment_env = \"prd\"\n", &[]).unwrap_err();
        assert_eq!(e.field, "aws.account_id");
        assert!(
            e.reason.contains("M0-2a"),
            "왜 바꿀 수 없는지 알려야 한다: {e}"
        );

        // 계정만 채우면 다음 필수값을 지목한다.
        let e2 = load_str(
            "deployment_env = \"prd\"\n",
            &[("DBMON__AWS__ACCOUNT_ID", "123456789012")],
        )
        .unwrap_err();
        assert_eq!(e2.field, "aws.region");
    }

    /// 12자리 계정 ID 를 환경변수로 주는 것은 컨테이너 배포의 기본 형태다.
    /// 휴리스틱 타입 추론은 이걸 정수로 만들어 역직렬화를 깨뜨렸다.
    #[test]
    fn numeric_looking_string_stays_a_string() {
        let c = load_str(
            "deployment_env = \"prd\"\n[storage]\ndata_table = \"d\"\nconfig_table = \"c\"\n",
            &[
                ("DBMON__AWS__ACCOUNT_ID", "123456789012"),
                ("DBMON__AWS__REGION", "ap-northeast-2"),
            ],
        )
        .unwrap();
        assert_eq!(c.aws.account_id, "123456789012");
    }

    #[test]
    fn unknown_field_is_rejected() {
        // 오타를 조용히 무시하면 설정이 반영되지 않은 채로 돈다.
        let text = format!(
            "{}\n[collector]\ndetect_intervall_ms = 500\n",
            minimal_toml()
        );
        assert!(load_str(&text, &[]).is_err());
    }

    #[test]
    fn env_overrides_file_and_nested_paths_work() {
        let c = load_str(
            minimal_toml(),
            &[
                ("DBMON__HTTP__PORT", "9999"),
                ("DBMON__COLLECTOR__DETECT_INTERVAL_MS", "2000"),
                ("DBMON__ROLE", "collector"),
            ],
        )
        .unwrap();
        assert_eq!(c.http.port, 9999);
        assert_eq!(c.collector.detect_interval_ms, 2_000);
        assert_eq!(c.role, Role::Collector);
        assert!(c.role.runs_collector() && !c.role.runs_api());
    }

    #[test]
    fn env_comma_list_becomes_array() {
        let c = load_str(
            minimal_toml(),
            &[("DBMON__DISCOVERY__ALLOWED_VPC_IDS", "vpc-a,vpc-b")],
        )
        .unwrap();
        assert_eq!(c.discovery.allowed_vpc_ids, vec!["vpc-a", "vpc-b"]);
    }

    #[test]
    fn unrelated_env_vars_are_ignored() {
        // `PATH` 같은 값이 설정을 오염시키면 안 된다.
        let c = load_str(minimal_toml(), &[("PATH", "/bin"), ("DBMON__", "x")]).unwrap();
        assert_eq!(c.http.port, 8080);
    }

    /// T-37 — dev 배포는 탐색 필터 없이 기동할 수 없다.
    #[test]
    fn t37_dev_requires_vpc_filter() {
        let dev = minimal_toml().replace("\"prd\"", "\"dev\"");
        let e = load_str(&dev, &[]).unwrap_err();
        assert_eq!(e.field, "discovery.allowed_vpc_ids");
        assert!(e.reason.contains("T-37"), "{e}");

        // 필터를 주면 통과한다.
        assert!(load_str(&dev, &[("DBMON__DISCOVERY__ALLOWED_VPC_IDS", "vpc-0ab")]).is_ok());
        // prd 배포에는 강제하지 않는다 (전 계정을 대상으로 하는 것이 정상).
        assert!(load_str(minimal_toml(), &[]).is_ok());
    }

    /// 탐지 타임아웃이 tick 예산을 넘으면 거부한다.
    ///
    /// 초기 구현은 탐지·심층조회 타임아웃을 하나로 뭉쳐서, 기본값(3초)이 기본 주기(1초)를
    /// 넘는 **자기모순**이 있었다. 이 교차 검증이 그걸 잡았다.
    #[test]
    fn cross_validation_catches_detect_timeout_over_tick_budget() {
        let e = load_str(
            minimal_toml(),
            &[("DBMON__COLLECTOR__DETECT_TIMEOUT_MS", "900")],
        )
        .unwrap_err();
        assert_eq!(e.field, "collector.detect_timeout_ms");
        assert!(e.reason.contains("tick 예산"), "{e}");

        // 800ms 는 1000ms 주기의 80% 이므로 정확히 경계에서 통과한다.
        assert!(
            load_str(
                minimal_toml(),
                &[("DBMON__COLLECTOR__DETECT_TIMEOUT_MS", "800")]
            )
            .is_ok()
        );
        // 주기를 늘리면 더 긴 타임아웃도 허용된다.
        assert!(
            load_str(
                minimal_toml(),
                &[
                    ("DBMON__COLLECTOR__DETECT_INTERVAL_MS", "5000"),
                    ("DBMON__COLLECTOR__DETECT_TIMEOUT_MS", "4000"),
                ]
            )
            .is_ok()
        );
    }

    #[test]
    fn defaults_satisfy_their_own_cross_validation() {
        // 기본값이 검증을 통과해야 한다 — 통과하지 못하면 아무도 기동할 수 없다.
        let c = load_str(minimal_toml(), &[]).unwrap();
        assert!(c.collector.detect_timeout_ms <= c.collector.detect_interval_ms * 80 / 100);
        assert!(c.collector.bulk_query_timeout_ms >= c.collector.query_timeout_ms);
    }

    #[test]
    fn bulk_timeout_cannot_be_shorter_than_query_timeout() {
        let e = load_str(
            minimal_toml(),
            &[("DBMON__COLLECTOR__BULK_QUERY_TIMEOUT_MS", "100")],
        )
        .unwrap_err();
        assert_eq!(e.field, "collector.bulk_query_timeout_ms");
    }

    #[test]
    fn deep_probe_cannot_exceed_detect_limit() {
        let e = load_str(
            minimal_toml(),
            &[("DBMON__COLLECTOR__DEEP_PROBE_LIMIT", "9999")],
        )
        .unwrap_err();
        assert_eq!(e.field, "collector.deep_probe_limit");
    }

    #[test]
    fn range_checks() {
        for (k, v, field) in [
            ("DBMON__AWS__ACCOUNT_ID", "123", "aws.account_id"),
            (
                "DBMON__COLLECTOR__DETECT_INTERVAL_MS",
                "10",
                "collector.detect_interval_ms",
            ),
            (
                "DBMON__COLLECTOR__SLOW_THRESHOLD_SECS",
                "0",
                "collector.slow_threshold_secs",
            ),
            ("DBMON__COLLECTOR__DIGEST_TOP_N", "1", "collector.digest_*"),
            (
                "DBMON__COLLECTOR__MONITOR_DB_USER",
                "",
                "collector.monitor_db_user",
            ),
        ] {
            let e = load_str(minimal_toml(), &[(k, v)]).unwrap_err();
            assert_eq!(e.field, field, "{k}={v}");
        }
    }

    #[test]
    fn dev_default_denylist_covers_production_names() {
        let d = DiscoveryConfig::default_denied_for_dev();
        assert!(d.contains(&"prd".to_string()));
        assert!(d.contains(&"production".to_string()));
    }
}

#[cfg(test)]
mod discovery_and_local_tests {
    use super::*;

    /// `Config::load` 와 같은 3단 병합을 거친 prd 설정. 기본값 문서를 우회하면
    /// 검증이 실제 경로와 달라진다.
    fn prd_config() -> Config {
        let mut root = default_document();
        merge(
            &mut root,
            toml::from_str(
                r#"
deployment_env = "prd"
[aws]
region = "ap-northeast-2"
account_id = "123456789012"
[storage]
data_table = "dbmon-data"
config_table = "dbmon-config"
"#,
            )
            .expect("테스트 TOML"),
        );
        Config::from_value(root).expect("기본 prd 설정")
    }

    /// **엔드포인트 재지정은 `dev` 에서만 허용한다.**
    ///
    /// 켜지면 조립부가 더미 자격증명을 넣어 태스크 롤이 무시된다. 그 실패는
    /// "저장이 안 된다" 로만 보여 원인 추적이 어렵다.
    ///
    /// ⚠ **`unknown` 도 막아야 한다.** 기본값이 `unknown` 이고
    /// `Env::Unknown.treat_as_production()` 은 `true` 다 — 처음에는 `== Prd` 만 막아서
    /// `DEPLOYMENT_ENV` 를 빠뜨린 배포가 조용히 더미 자격증명으로 떴다.
    #[test]
    fn only_dev_may_override_the_endpoint() {
        for env in [Env::Prd, Env::Stg, Env::Unknown] {
            let mut c = prd_config();
            c.deployment_env = env;
            c.discovery.allowed_vpc_ids = vec!["vpc-a".into()];
            c.storage.endpoint_url = Some("http://127.0.0.1:18000".into());
            let e = c
                .validate()
                .expect_err(&format!("{env} 에서 엔드포인트 재지정이 통과했다"));
            assert_eq!(e.field, "storage.endpoint_url", "{env}: {e}");
        }
    }

    /// **루프백이 아닌 주소는 거부한다.**
    ///
    /// 없으면 dev 배포가 임의의 외부 주소로 인스턴스 메타데이터를 보낼 수 있다.
    #[test]
    fn non_loopback_endpoints_are_rejected() {
        // `validate()` 는 프로세스 환경을 읽으므로 컨테이너 안에서는 판정이 다르다.
        // 그 축은 아래 `endpoint_override_allowed` 순수 함수 테스트가 담당한다 —
        // 여기서 조용히 통과시키면 "컨테이너에서 테스트가 이유 없이 초록" 이 된다.
        if is_local_container_sandbox() {
            return;
        }
        for url in [
            "https://attacker.example",
            "http://10.0.0.5:8000",
            "http://dynamodb.ap-northeast-2.amazonaws.com",
            "http://127.0.0.1.attacker.example",
            "http://localhost.attacker.example:8000",
        ] {
            let mut c = prd_config();
            c.deployment_env = Env::Dev;
            c.discovery.allowed_vpc_ids = vec!["vpc-a".into()];
            c.storage.endpoint_url = Some(url.into());
            let e = c.validate().expect_err(&format!("{url} 이 통과했다"));
            assert_eq!(e.field, "storage.endpoint_url", "{url}");
        }
    }

    /// **엔드포인트 재지정 판정을 전수로 확인한다.**
    ///
    /// 프로세스 환경을 읽지 않는 순수 함수이므로 컨테이너 안·밖 어디서 돌려도
    /// 같은 결과가 나온다 — 그게 이 판정을 함수로 뺀 이유다.
    #[test]
    fn endpoint_override_is_dev_only_regardless_of_container() {
        const REMOTE: &str = "http://dynamodb.ap-northeast-2.amazonaws.com";
        const COMPOSE: &str = "http://dynamodb:8000";
        const LOOPBACK: &str = "http://127.0.0.1:18000";

        // dev: 루프백은 항상, 비루프백은 컨테이너 안에서만.
        assert!(endpoint_override_allowed(Env::Dev, LOOPBACK, false));
        assert!(endpoint_override_allowed(Env::Dev, LOOPBACK, true));
        assert!(!endpoint_override_allowed(Env::Dev, COMPOSE, false));
        assert!(endpoint_override_allowed(Env::Dev, COMPOSE, true));

        // **비-dev 는 무엇이든 거부한다** — 컨테이너 여부가 이 판정을 뒤집지 못한다.
        // 이게 깨지면 prd 태스크가 더미 자격증명으로 뜨고 저장이 조용히 실패한다.
        for env in [Env::Prd, Env::Stg, Env::Unknown] {
            for url in [LOOPBACK, COMPOSE, REMOTE] {
                for in_container in [false, true] {
                    assert!(
                        !endpoint_override_allowed(env, url, in_container),
                        "{env} / {url} / container={in_container} 가 통과했다"
                    );
                }
            }
        }
    }

    /// 루프백 형태는 모두 허용한다 — 로컬 개발이 막히면 안 된다.
    #[test]
    fn loopback_endpoints_are_allowed_in_dev() {
        for url in [
            "http://127.0.0.1:18000",
            "http://localhost:18000",
            "http://[::1]:18000",
            "http://127.0.0.2:18000",
        ] {
            let mut c = prd_config();
            c.deployment_env = Env::Dev;
            c.discovery.allowed_vpc_ids = vec!["vpc-a".into()];
            c.storage.endpoint_url = Some(url.into());
            c.validate()
                .unwrap_or_else(|e| panic!("{url} 이 막혔다 — 로컬 개발을 할 수 없다: {e}"));
        }
    }

    /// dev 에서는 허용한다 — 그게 로컬 우선 개발의 전제다.
    #[test]
    fn dev_allows_the_local_endpoint_override() {
        let mut c = prd_config();
        c.deployment_env = Env::Dev;
        // dev 는 T-37 로 VPC 필터가 필수다.
        c.discovery.allowed_vpc_ids = vec!["vpc-local".into()];
        c.storage.endpoint_url = Some("http://127.0.0.1:18000".into());
        c.validate()
            .expect("dev 에서 막혔다 — 로컬 개발을 할 수 없다");
    }

    /// **`monitor_db_user` 에 예약문자를 막는다.**
    ///
    /// IAM 토큰의 서명 대상 URI 에 보간되므로 `#`·`&`·`%`·공백이 서명을 깨뜨린다.
    /// 증상은 `Access denied` 이고 원인이 IAM 정책처럼 보인다.
    #[test]
    fn monitor_db_user_rejects_uri_reserved_characters() {
        for bad in [
            "db#mon",
            "db&DBUser=root",
            "db%mon",
            "db mon",
            "dbmon@host",
            "디비몬",
        ] {
            let mut c = prd_config();
            c.collector.monitor_db_user = bad.into();
            let Err(e) = c.validate() else {
                panic!("{bad:?} 가 통과했다");
            };
            assert_eq!(e.field, "collector.monitor_db_user", "{bad:?}");
        }
        // 정상 형태는 통과한다.
        for ok in ["dbmon", "db_mon", "db-mon", "dbmon2"] {
            let mut c = prd_config();
            c.collector.monitor_db_user = ok.into();
            c.validate()
                .unwrap_or_else(|e| panic!("{ok:?} 가 막혔다: {e}"));
        }
    }

    /// 탐색 주기는 기본 5분이다 (FR-DSC-02).
    #[test]
    fn discovery_interval_defaults_to_five_minutes() {
        assert_eq!(DiscoveryConfig::default().interval_secs, 300);
    }

    /// **0 초를 막는다.** 핫 루프로 AWS API 를 때리면 조절당하고, 조절은 부분 결과로
    /// 나타나 FR-DSC-07 미발견 판정을 흔든다.
    #[test]
    fn discovery_interval_has_a_floor() {
        for secs in [0u64, 1, MIN_DISCOVERY_INTERVAL_SECS - 1] {
            let mut c = prd_config();
            c.discovery.interval_secs = secs;
            let e = c.validate().expect_err("{secs}초가 통과했다");
            assert_eq!(e.field, "discovery.interval_secs");
        }
        let mut ok = prd_config();
        ok.discovery.interval_secs = MIN_DISCOVERY_INTERVAL_SECS;
        ok.validate().expect("하한값이 막혔다");
    }
}

#[cfg(test)]
mod derived_defaults_tests {
    use super::*;

    fn load(toml_text: &str) -> Config {
        let mut root = default_document();
        merge(&mut root, toml::from_str(toml_text).expect("테스트 TOML"));
        Config::from_value(root).expect("설정")
    }

    const DEV_BASE: &str = r#"
deployment_env = "dev"
[aws]
region = "ap-northeast-2"
account_id = "123456789012"
[storage]
data_table = "d"
config_table = "c"
[discovery]
allowed_vpc_ids = ["vpc-dev"]
"#;

    /// **dev 배포는 이름 거부 목록이 기본으로 켜져야 한다** (T-37 심층 방어).
    ///
    /// 이 테스트가 없었을 때 `default_denied_for_dev()` 는 **테스트에서만** 불렸고,
    /// 프로덕션 경로에는 호출부가 없었다. 기능이 아니라 배선의 부재였다.
    #[test]
    fn dev_deployment_gets_the_default_deny_list() {
        let c = load(DEV_BASE);
        assert!(
            !c.discovery.denied_name_substrings.is_empty(),
            "dev 인데 이름 거부 목록이 비었다 — dev VPC 에 잘못 배치된 prd 를 잡지 못한다"
        );
        for want in ["prd", "prod", "production"] {
            assert!(
                c.discovery.denied_name_substrings.iter().any(|d| d == want),
                "{want} 가 없다: {:?}",
                c.discovery.denied_name_substrings
            );
        }
    }

    /// **명시한 값에 기본값을 합친다 — 대체하지 않는다.**
    ///
    /// 비어 있을 때만 채우면, 운영자가 거부 문자열 하나를 추가하는 순간
    /// `prd`/`prod`/`production` 이 조용히 사라진다.
    #[test]
    fn an_explicit_deny_list_is_unioned_with_the_defaults() {
        let c = load(&format!(
            "{DEV_BASE}\ndenied_name_substrings = [\"canary\"]\n"
        ));
        let deny = &c.discovery.denied_name_substrings;
        assert!(
            deny.iter().any(|d| d == "canary"),
            "명시값이 사라졌다: {deny:?}"
        );
        for want in ["prd", "prod", "production"] {
            assert!(
                deny.iter().any(|d| d == want),
                "명시값을 넣자 기본 거부 {want} 가 사라졌다: {deny:?}"
            );
        }
    }

    /// 중복은 넣지 않는다.
    #[test]
    fn union_does_not_duplicate() {
        let c = load(&format!("{DEV_BASE}\ndenied_name_substrings = [\"PRD\"]\n"));
        let count = c
            .discovery
            .denied_name_substrings
            .iter()
            .filter(|d| d.eq_ignore_ascii_case("prd"))
            .count();
        assert_eq!(count, 1, "{:?}", c.discovery.denied_name_substrings);
    }

    /// **비프로덕션 배포는 태그 기반 prd 거부가 기본으로 켜져야 한다.**
    #[test]
    fn non_prd_deployment_rejects_production_tags_by_default() {
        assert!(load(DEV_BASE).discovery.reject_production_tags);
        // prd 배포에는 켜지 않는다 — 자기 인스턴스를 전부 거부하게 된다.
        let prd = load(
            r#"
deployment_env = "prd"
[aws]
region = "ap-northeast-2"
account_id = "123456789012"
[storage]
data_table = "d"
config_table = "c"
"#,
        );
        assert!(!prd.discovery.reject_production_tags);
    }

    /// prd 배포에는 넣지 않는다 — 자기 이름에 `prd` 가 들어간 인스턴스를 거부하게 된다.
    #[test]
    fn prd_deployment_gets_no_default_deny_list() {
        let c = load(
            r#"
deployment_env = "prd"
[aws]
region = "ap-northeast-2"
account_id = "123456789012"
[storage]
data_table = "d"
config_table = "c"
"#,
        );
        assert!(
            c.discovery.denied_name_substrings.is_empty(),
            "prd 배포가 이름에 prd 가 든 자기 인스턴스를 거부한다"
        );
    }

    /// 유도 기본값이 **실제 필터에 도달해야** 한다 — 설정만 채우고 안 쓰면 무의미하다.
    #[test]
    fn the_derived_deny_list_actually_rejects_a_misplaced_prd_instance() {
        use crate::aws::filter::{Candidate, Filter};

        let c = load(DEV_BASE);
        // **프로덕션과 같은 경로로 만든다.** 필드를 여기서 다시 옮기면 이 테스트는
        // 배선이 아니라 자기 자신을 검증하게 된다.
        let filter = Filter::from_config(&c.discovery);
        // dev VPC 안에 있지만 이름이 prd 다 — 잘못 배치된 프로덕션 인스턴스.
        let misplaced = Candidate {
            identifier: "orders-prd-01".into(),
            vpc_id: Some("vpc-dev".into()),
            tags: Default::default(),
            cluster_identifier: None,
        };
        assert!(
            !filter.judge(&misplaced).is_accept(),
            "dev 배포가 dev VPC 안의 prd 인스턴스를 통과시켰다"
        );
    }
}

#[cfg(test)]
mod job_interval_tests {
    use super::*;

    fn base() -> Config {
        let mut root = default_document();
        merge(
            &mut root,
            toml::from_str(
                r#"
deployment_env = "prd"
[aws]
region = "ap-northeast-2"
account_id = "123456789012"
[storage]
data_table = "d"
config_table = "c"
"#,
            )
            .expect("테스트 TOML"),
        );
        Config::from_value(root).expect("기본 설정")
    }

    /// **`0` 은 비활성이 아니라 매 tick 반복이다.**
    ///
    /// `backfill_secs = 0` 이면 창이 비어 백필이 아무것도 하지 않으면서 인스턴스당
    /// 초당 1회 CloudWatch 를 때린다 — 500대면 확정적으로 조절당한다.
    #[test]
    fn zero_job_intervals_are_rejected() {
        for field in ["orphan_sweep_secs", "backfill_secs"] {
            let mut c = base();
            match field {
                "orphan_sweep_secs" => c.collector.orphan_sweep_secs = 0,
                _ => c.collector.backfill_secs = 0,
            }
            let Err(e) = c.validate() else {
                panic!("{field} = 0 이 통과했다");
            };
            assert!(e.field.ends_with(field), "{field}: {e}");
        }
    }

    /// 상한을 넘으면 거부한다 — `secs * 1000` 오버플로를 막는다.
    #[test]
    fn absurd_job_intervals_are_rejected() {
        let mut c = base();
        c.collector.backfill_secs = u64::MAX;
        assert!(
            c.validate().is_err(),
            "u64::MAX 가 통과했다 — 곱셈이 오버플로한다"
        );
    }

    /// 기본값은 통과한다.
    #[test]
    fn the_defaults_are_valid() {
        base().validate().expect("기본값이 막혔다");
        assert_eq!(CollectorConfig::default().orphan_sweep_secs, 300);
        assert_eq!(CollectorConfig::default().backfill_secs, 60);
    }
}
