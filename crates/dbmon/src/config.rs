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
    /// 로컬 개발용 엔드포인트 오버라이드 (DynamoDB Local 등).
    #[serde(default)]
    pub endpoint_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub data_table: String,
    pub config_table: String,
    #[serde(default)]
    pub plan_bucket: Option<String>,
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
        }
    }
}

/// 탐색 필터 — **prd 혼재 계정 격리의 마지막 방어선** (T-37, M0-13f).
///
/// 개발계 계정에 프로덕션 워크로드가 함께 있다([18 §6](../../../docs/18-dev-environment.md)).
/// 네트워크 격리가 1차 방어선이지만, IAM 의 `rds:DescribeDBInstances` 는 `Resource:"*"` 라
/// prd 인스턴스도 **보인다.** 여기서 걸러야 레지스트리에 등록되지 않는다.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// 이 VPC 안의 인스턴스만 수집한다. **AND 조건**이다.
    #[serde(default)]
    pub allowed_vpc_ids: Vec<String>,
    /// 이 태그를 가진 인스턴스만. `키=값` 형태.
    #[serde(default)]
    pub required_tags: Vec<String>,
    /// 이름에 이 문자열이 들어가면 제외한다. 화이트리스트가 통과시켜도 거부한다.
    #[serde(default)]
    pub denied_name_substrings: Vec<String>,
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
        let cfg: Config = root.try_into().map_err(|e| {
            err(
                "deserialize",
                format!("{e} — 값의 타입이 맞지 않거나 알 수 없는 키가 있다"),
            )
        })?;
        cfg.validate()?;
        Ok(cfg)
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
            endpoint_url: None,
        }),
    );
    t.insert(
        "storage".into(),
        to_value(StorageConfig {
            data_table: String::new(),
            config_table: String::new(),
            plan_bucket: None,
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
