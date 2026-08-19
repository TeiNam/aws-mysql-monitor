//! `dbmon` 실행 파일. **조립만 한다.**
//!
//! 역할은 기동 플래그로 고른다([02 §2](../../docs/02-architecture.md)).
//! 구현체 주입은 여기서 1회 하고, 이후 모든 코드는 `core` 의 포트만 본다.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::routing::get;
use clap::{Parser, Subcommand};
use dbmon::config::{Config, Role};
use dbmon::health::{Readiness, healthz, readyz};
use dbmon::shutdown::{Shutdown, run_stages, stage, wait_for_signal};
use dbmon::telemetry;
use dbmon_core::ports::{AuthTokenProvider, InstanceRegistry};

#[derive(Parser, Debug)]
#[command(
    name = "dbmon",
    about = "AWS RDS/Aurora MySQL 슬로우 쿼리 모니터",
    version
)]
struct Cli {
    /// 설정 파일 (TOML). 없으면 환경변수만 쓴다.
    #[arg(long, short = 'c', env = "DBMON_CONFIG")]
    config: Option<PathBuf>,
    /// 사람이 읽는 로그 형식. 기본은 JSON (CloudWatch Logs Insights 용).
    #[arg(long, env = "DBMON_LOG_PRETTY")]
    log_pretty: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 서버를 띄운다 (기본).
    Serve {
        /// 역할 오버라이드. 설정 파일의 `role` 보다 우선한다.
        #[arg(long)]
        role: Option<String>,
    },
    /// 설정을 검증하고 종료한다. 배포 전 게이트로 쓴다.
    Check,
    /// 자기 `/healthz` 를 확인하고 종료 코드로 알린다.
    ///
    /// **컨테이너 이미지에 `curl` 을 넣지 않기 위해** 존재한다. ECS 태스크 정의의
    /// `healthCheck.command` 가 이걸 호출한다:
    /// `["CMD", "/usr/local/bin/dbmon", "healthcheck"]`
    Healthcheck {
        /// 확인할 포트. 기본은 설정의 `http.port`.
        #[arg(long)]
        port: Option<u16>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    telemetry::init(!cli.log_pretty);

    let mut config = Config::load(cli.config.as_deref())
        .context("설정을 읽을 수 없다. 필수값과 형식을 확인한다")?;

    match cli.command.unwrap_or(Command::Serve { role: None }) {
        Command::Check => {
            // validate 는 load 안에서 이미 돌았다. 여기까지 오면 통과다.
            println!(
                "설정 OK — role={:?} env={} region={} account={} data_table={}",
                config.role,
                config.deployment_env,
                config.aws.region,
                config.aws.account_id,
                config.storage.data_table
            );
            Ok(())
        }
        Command::Healthcheck { port } => {
            let port = port.unwrap_or(config.http.port);
            if let Err(e) = healthcheck(port).await {
                // 종료 코드가 계약이다. 메시지는 stderr 로만 남긴다.
                eprintln!("healthcheck 실패: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Serve { role } => {
            if let Some(r) = role {
                config.role = parse_role(&r)?;
                // **오버라이드 후 다시 검증한다.** `Config::load` 안의 검증은 이미
                // 끝났으므로, 역할에 의존하는 규칙이 생기면 `--role` 로 우회된다
                // (2차 리뷰가 지적). 지금은 무해하지만 게이트를 먼저 닫는다.
                config.validate()?;
            }
            serve(config).await
        }
    }
}

/// 이 워커의 식별자. 리스 소유자로 쓰이므로 **워커마다 달라야 한다.**
///
/// ECS 는 태스크마다 `ECS_CONTAINER_METADATA_URI_V4` 를 주지만 그걸 파싱하려면 HTTP
/// 호출이 필요하다. 호스트명이면 충분하다 — Fargate 태스크는 각자 다른 호스트명을 갖는다.
fn worker_id(config: &Config) -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "local".to_string());
    format!("{}-{host}", config.role.as_str())
}

/// DynamoDB 클라이언트를 만들고 저장소 두 개를 조립한다.
///
/// `endpoint_url` 이 설정에 있으면 그걸 쓴다 — DynamoDB Local 로 개발할 때다.
async fn build_stores(config: &Config) -> anyhow::Result<Stores> {
    use aws_config::BehaviorVersion;

    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(config.aws.region.clone()));
    if let Some(url) = &config.storage.endpoint_url {
        // **로컬 개발 경로.** SSO 가 만료돼도 저장 경로를 돌릴 수 있다.
        //
        // ⚠ 더미 자격증명을 함께 넣어야 한다. DynamoDB Local 은 값을 보지 않지만
        // **SDK 는 서명하려면 자격증명이 있어야 한다** — 없으면 모든 요청이
        // `NoCredentialsError` 로 죽는다. 처음 이 부분을 빼놨고, 자격증명을 지운
        // 상태로 실제 바이너리를 돌려서야 드러났다("리스 획득 실패" 15회).
        // 그러면 "SSO 가 만료돼도 로컬로 개발한다" 는 전제가 무너진다.
        //
        // 이 경로는 `endpoint_url` 이 설정된 경우에만 탄다. `deployment_env == prd`
        // 에서는 설정 검증이 `endpoint_url` 자체를 금지한다.
        // **URL 원문을 찍지 않는다.** `http://user:pass@host` 형태면 자격증명이
        // 로그에 남는다. 설정 검증이 루프백만 허용하므로 실질 위험은 낮지만,
        // 로그에 비밀이 들어갈 여지를 남기지 않는다(2차 리뷰가 지적).
        tracing::info!(
            endpoint_host = %url.rsplit('@').next().unwrap_or("?"),
            "DynamoDB 엔드포인트 재지정 (로컬 개발 — 더미 자격증명)"
        );
        loader = loader.endpoint_url(url).credentials_provider(
            aws_sdk_dynamodb::config::Credentials::new("local", "local", None, None, "dbmon-local"),
        );
    }
    let sdk = loader.load().await;
    let client = aws_sdk_dynamodb::Client::new(&sdk);

    Ok(Stores {
        sdk: sdk.clone(),
        slow_query: Arc::new(dbmon::store::DynamoSlowQueryStore::new(
            client.clone(),
            config.storage.data_table.clone(),
        )),
        lease: Arc::new(dbmon::store::lease::DynamoLeaseStore::new(
            client.clone(),
            config.storage.data_table.clone(),
        )),
        registry: Arc::new(dbmon::store::registry::DynamoInstanceRegistry::new(
            client,
            config.storage.data_table.clone(),
        )),
    })
}

/// 조립된 저장소들. 인자 5개를 넘기는 대신 묶는다.
struct Stores {
    /// 인증 공급자 조립에 쓴다. **DynamoDB 로컬 경로의 더미 자격증명이 아니라**
    /// 대상 리전 자격증명이 필요하므로 `build_auth` 가 리전을 다시 지정한다.
    sdk: aws_config::SdkConfig,
    slow_query: Arc<dbmon::store::DynamoSlowQueryStore>,
    lease: Arc<dbmon::store::lease::DynamoLeaseStore>,
    registry: Arc<dbmon::store::registry::DynamoInstanceRegistry>,
}

/// 리전별 RDS 탐색기.
///
/// **DynamoDB 와 달리 `endpoint_url` 을 적용하지 않는다.** RDS 를 로컬로 흉내낼 방법이
/// 없고, 흉내낸다면 그건 탐색을 검증하는 게 아니라 목(mock)을 검증하는 것이다.
/// 로컬에서 탐색 로직을 검증하는 방법은 순수 함수 전수 테스트다
/// ([`dbmon::aws::discovery`], [`dbmon::discovery`]).
async fn build_discovery(config: &Config) -> Vec<dbmon::aws::rds::RdsDiscovery> {
    use aws_config::BehaviorVersion;

    let mut out = Vec::new();
    for region in config.target_regions() {
        let sdk = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new(region.clone()))
            .load()
            .await;
        out.push(dbmon::aws::rds::RdsDiscovery::new(
            aws_sdk_rds::Client::new(&sdk),
            region,
        ));
    }
    out
}

/// 탐색 **조회** 단계 — `DescribeDBInstances` → T-37 필터 → 도메인 매핑.
///
/// # 이 단계만 취소해도 안전하다
///
/// 읽기와 순수 판정뿐이라 중간에 잘려도 남는 상태가 없다. 반대로 재조정
/// ([`dbmon::discovery::reconcile`])은 **취소하면 안 된다** — `mark_missing` 이
/// 비멱등이라 적용된 증가분이 남은 채 라운드가 실패로 보고되고, 그러면
/// "2회 **연속**" 이라는 FR-DSC-07 의 성질이 깨진다.
///
/// 그래서 예산·셧다운 취소는 이 함수에만 건다.
///
/// # 필터를 통과하지 못한 것은 등록부에 넣지 않는다
///
/// prd 인스턴스를 등록부에 넣고 나중에 걸러도 되지만, 그러면 **한 곳이라도 필터를
/// 잊으면 prd 를 수집한다.** 애초에 들이지 않는 것이 방어선을 하나로 만든다.
/// 단, "봤지만 제외" 와 "사라졌다" 는 구분해서 넘긴다 — 섞으면 필터 사고 하나로
/// 등록부가 비워진다.
async fn discover(
    sources: Arc<Vec<dbmon::aws::rds::RdsDiscovery>>,
    config: Arc<Config>,
    now_ms: i64,
) -> dbmon::discovery::RoundOutcome {
    use dbmon::aws::discovery::{Unmappable, to_instance};
    use dbmon::aws::filter::Filter;
    use dbmon::discovery::RoundOutcome;
    use dbmon_core::env::EnvMapping;
    use dbmon_core::ids::InstanceId;

    let filter = Filter::from_config(&config.discovery);
    let mapping = EnvMapping::default();
    let mut outcome = RoundOutcome::default();

    // **탐색기가 없으면 부분 결과로 본다.** 빈 결과를 온전한 결과로 취급하면
    // 등록부의 전 인스턴스가 "사라졌다" 로 판정된다.
    if sources.is_empty() {
        tracing::error!("탐색 대상 리전이 없다 — 부분 결과로 처리한다");
        outcome.truncated = true;
        return outcome;
    }

    // 제외된 인스턴스의 id 를 만든다. 만들 수 없으면(식별자 규칙 위반) 등록부에도
    // 있을 수 없으므로 `None` 이어도 안전하다.
    //
    // 클로저가 아니라 함수로 둔다 — `&mut outcome` 을 잡는 클로저를 `.await` 를 넘어
    // 쓰면 future 가 `Send` 가 아니게 되고, `tokio::spawn` 이 거부한다.
    fn excluded_id(account: &str, region: &str, identifier: &str) -> Option<String> {
        InstanceId::new(account, region, identifier)
            .ok()
            .map(|id| id.as_str().to_string())
    }

    for source in sources.iter() {
        let page = match source.describe().await {
            Ok(p) => p,
            Err(e) => {
                // **한 리전이라도 실패하면 전체를 부분 결과로 본다.** 리전별로 나눠
                // 판정하면 실패한 리전의 인스턴스가 "사라졌다" 로 판정된다.
                tracing::warn!(error = %telemetry::Scrubbed(&e), "리전 탐색 실패");
                outcome.truncated = true;
                continue;
            }
        };
        outcome.truncated |= page.truncated;

        for raw in &page.instances {
            let verdict = filter.judge(&raw.candidate());
            if !verdict.is_accept() {
                // 사유를 남긴다 — 조용히 거부하면 "왜 안 보이나" 를 추적할 수 없다.
                tracing::debug!(
                    instance = %raw.identifier,
                    reason = %verdict.reason(),
                    "탐색 필터가 거부했다"
                );
                outcome.filtered += 1;
                if let Some(id) = excluded_id(&config.aws.account_id, &raw.region, &raw.identifier)
                {
                    outcome.excluded_ids.insert(id);
                }
                continue;
            }
            match to_instance(raw, &config.aws.account_id, &mapping, now_ms) {
                Ok(i) => outcome.discovered.push(i),
                // MySQL 이 아닌 엔진은 정상적으로 흔하다 — 등록부에 있을 수 없으므로
                // 제외 목록에 넣지 않는다(넣어도 무해하지만 통계를 흐린다).
                Err(Unmappable::NotMysql { engine }) => {
                    tracing::debug!(instance = %raw.identifier, %engine, "MySQL 계열이 아니다");
                }
                Err(e) => {
                    // **버리지 않고 제외로 기록한다.** AWS 가 새 버전 문자열을 내면
                    // 전건 매핑 실패가 되는데, 그걸 미발견으로 처리하면 Aurora 인스턴스
                    // 전부가 2라운드 뒤 삭제 판정된다.
                    tracing::warn!(
                        instance = %raw.identifier,
                        reason = %telemetry::Scrubbed(&e),
                        "인스턴스 매핑 실패 — 제외로 기록한다(삭제하지 않는다)"
                    );
                    outcome.unmappable += 1;
                    if let Some(id) =
                        excluded_id(&config.aws.account_id, &raw.region, &raw.identifier)
                    {
                        outcome.excluded_ids.insert(id);
                    }
                }
            }
        }
    }
    outcome
}

/// 대상 접속 비밀을 발급하는 주체. 배포 환경에 따라 갈린다.
///
/// **`dev` + 환경변수가 있을 때만 고정 비밀번호를 쓴다.** 두 조건이 모두 필요하다 —
/// 환경변수만 보면 prd 태스크에 그 변수가 새어 들어갔을 때 IAM 대신 비밀번호로
/// 붙으려 하고, 실패 원인이 "인증 실패" 로만 보인다.
fn build_auth(
    config: &Config,
    region: &str,
    sdk: &aws_config::SdkConfig,
) -> Arc<dyn AuthTokenProvider> {
    use dbmon::aws::auth_token::{IamAuthTokenProvider, StaticPasswordProvider};

    if config.deployment_env == dbmon_core::env::Env::Dev {
        if let Some(p) = StaticPasswordProvider::from_env() {
            tracing::info!("대상 인증: 고정 비밀번호 (dev 폴백)");
            return Arc::new(p);
        }
    }
    tracing::info!(%region, "대상 인증: IAM DB Auth");
    // **대상 인스턴스의 리전으로 서명한다.** 앱 배포 리전으로 서명하면 거부된다.
    Arc::new(IamAuthTokenProvider::new(
        sdk.credentials_provider()
            .expect("SDK 자격증명 공급자")
            .clone(),
        region,
    ))
}

/// 인스턴스별 수집 태스크 집합.
struct CollectTasks {
    handles: std::collections::BTreeMap<String, tokio::task::JoinHandle<()>>,
}

impl CollectTasks {
    fn new() -> Self {
        Self {
            handles: std::collections::BTreeMap::new(),
        }
    }

    fn running(&self) -> std::collections::BTreeSet<String> {
        self.handles.keys().cloned().collect()
    }

    /// 목표 집합에 맞춰 태스크를 띄우고 죽인다.
    ///
    /// **교집합은 건드리지 않는다.** 죽이고 다시 띄우면 진행 중 캐시가 사라져
    /// 그 순간 실행 중이던 쿼리가 `in_flight` 고아가 된다.
    async fn reconcile(
        &mut self,
        instances: &[dbmon_core::instance::Instance],
        deps: &CollectDeps,
    ) {
        use dbmon::collect_loop::{desired_ids, index_by_id, task_delta};

        let desired = desired_ids(instances);
        let delta = task_delta(&self.running(), &desired);
        let by_id = index_by_id(instances);

        for id in &delta.to_stop {
            if let Some(h) = self.handles.remove(id) {
                // `abort()` 는 다음 await 지점에서 태스크를 끊는다. 수집 tick 은
                // 읽기뿐이고 저장은 멱등(`upsert_merged`)이라 중간에 끊겨도 안전하다.
                h.abort();
                tracing::info!(instance = %id, "수집 태스크 중지");
            }
        }
        for id in &delta.to_start {
            let Some(instance) = by_id.get(id) else {
                continue;
            };
            let handle = spawn_instance_collector((*instance).clone(), deps.clone());
            self.handles.insert(id.clone(), handle);
            tracing::info!(instance = %id, "수집 태스크 시작");
        }
    }

    /// 전부 중지한다 (셧다운).
    fn abort_all(&mut self) {
        for (id, h) in std::mem::take(&mut self.handles) {
            h.abort();
            tracing::debug!(instance = %id, "수집 태스크 중지 (종료)");
        }
    }
}

/// 수집 태스크가 필요한 의존성.
#[derive(Clone)]
struct CollectDeps {
    store: Arc<dbmon::store::DynamoSlowQueryStore>,
    auth: Arc<dyn AuthTokenProvider>,
    config: Arc<Config>,
    worker_id: String,
}

/// 인스턴스 하나의 수집 루프를 띄운다.
///
/// # 토큰 갱신 때문에 풀을 다시 만든다
///
/// IAM 토큰은 15분 만료고 **연결 수립 시점**에만 쓰인다. 기존 연결은 살아 있지만
/// 그 뒤 새로 만들어지는 연결은 인증에 실패한다. 그래서 만료 5분 전에 풀을 갈아
/// 끼운다 — [`InstanceCollector::replace_db`] 가 진행 중 캐시를 유지한 채 연결만
/// 교체한다. 수집기를 새로 만들면 15분마다 고아가 생긴다.
fn spawn_instance_collector(
    instance: dbmon_core::instance::Instance,
    deps: CollectDeps,
) -> tokio::task::JoinHandle<()> {
    use dbmon::collector::{CollectParams, InstanceCollector};
    use dbmon::mysql::TargetMysql;
    use dbmon::mysql::connect::target_opts;
    use dbmon_core::time::{Clock, SystemClock};

    let tick = Duration::from_millis(deps.config.collector.detect_interval_ms);
    // 토큰 만료 여유. 이보다 적게 남으면 풀을 갈아 끼운다.
    const REFRESH_MARGIN_MS: i64 = 5 * 60_000;

    tokio::spawn(async move {
        let db_user = deps.config.collector.monitor_db_user.clone();
        let label = instance.id.as_str().to_string();

        // 첫 연결. 실패하면 잠시 뒤 재시도한다 — 태스크를 끝내면 이 인스턴스는
        // 다음 탐색(5분)까지 수집되지 않는다.
        let mut secret = match deps
            .auth
            .token(
                instance.endpoint.as_deref().unwrap_or_default(),
                instance.port,
                &db_user,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(instance = %label, error = %telemetry::Scrubbed(&e), "대상 인증 실패");
                return;
            }
        };
        let make_db = |secret: &dbmon_core::secret::ExpiringSecret| -> anyhow::Result<TargetMysql> {
            let opts = target_opts(
                &instance,
                &db_user,
                secret.expose(),
                deps.config.deployment_env,
            )?;
            Ok(TargetMysql::from_config(
                opts,
                &deps.config.collector,
                label.clone(),
            )?)
        };
        let db = match make_db(&secret) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(instance = %label, error = %telemetry::Scrubbed(&*e), "연결 옵션 구성 실패");
                return;
            }
        };

        let params = CollectParams {
            slow_threshold_secs: deps.config.collector.slow_threshold_secs,
            deep_probe_limit: deps.config.collector.deep_probe_limit as usize,
            try_for_connection: true,
            literal_policy: dbmon_core::slow_query::LiteralPolicy::Masked,
            monitor_db_user: db_user.clone(),
            worker_id: deps.worker_id.clone(),
        };
        let mut collector = InstanceCollector::new(
            instance.clone(),
            db,
            deps.store.clone(),
            SystemClock,
            params,
        );

        loop {
            // 토큰이 만료에 가까우면 연결만 갈아 끼운다.
            let now_ms = SystemClock.now_ms();
            if secret.needs_refresh(now_ms, REFRESH_MARGIN_MS) {
                match deps
                    .auth
                    .token(
                        instance.endpoint.as_deref().unwrap_or_default(),
                        instance.port,
                        &db_user,
                    )
                    .await
                {
                    Ok(fresh) => match make_db(&fresh) {
                        Ok(db) => {
                            collector.replace_db(db);
                            secret = fresh;
                            tracing::debug!(instance = %label, "대상 인증 토큰 갱신");
                        }
                        Err(e) => tracing::warn!(
                            instance = %label,
                            error = %telemetry::Scrubbed(&*e),
                            "토큰 갱신 후 연결 구성 실패 — 기존 풀을 유지한다"
                        ),
                    },
                    // 갱신 실패로 기존 풀을 버리지 않는다 — 살아 있는 연결은 계속 쓴다.
                    Err(e) => tracing::warn!(
                        instance = %label,
                        error = %telemetry::Scrubbed(&e),
                        "토큰 갱신 실패 — 기존 연결을 유지한다"
                    ),
                }
            }

            // 연결 수립은 tick **밖**이다. 안에서 하면 콜드 핸드셰이크(최대 5초)가
            // 1초 케이던스를 깨뜨린다.
            collector.warm_if_needed().await;

            match collector.detect_tick().await {
                Ok(stats) => tracing::trace!(
                    instance = %label,
                    candidates = stats.candidates,
                    finalized = stats.finalized,
                    "수집 tick"
                ),
                Err(e) => tracing::warn!(
                    instance = %label,
                    error = %telemetry::Scrubbed(&e),
                    "수집 tick 실패"
                ),
            }
            tokio::time::sleep(tick).await;
        }
    })
}

/// 리더 게이트 루프를 띄운다. **수집 역할이 아니면 `None` 을 반환한다.**
///
/// # 이 루프가 F1 을 강제하는 유일한 지점이다
///
/// `target_shard_count` 가 불변식을 계산하지만 **부르는 곳이 없으면 무효다** —
/// 이 프로젝트에서 "고쳤는데 호출부가 없다" 가 다섯 번 재발했다.
///
/// 리더가 아닌 동안에는 수집하지 않는다. `desiredCount=2` 로 띄운 두 워커가 모두
/// 수집하면 같은 인스턴스를 중복 수집하고 다이제스트 누산기가 손상된다.
///
/// # ⚠ 수집 역할이 아닌 워커는 리스를 **잡아서도 안 된다**
///
/// 처음에는 역할과 무관하게 루프를 띄우고 `runs_collector` 를 `gate.refresh()`
/// **다음에** 검사했다. 그러면 `role=api` 워커가 수집 리더 리스를 따서 영구히
/// 갱신하고, `role=collector` 워커는 영원히 standby 가 된다 — **수집이 한 번도
/// 돌지 않는다.** 게다가 api 워커가 `set_collect_leader(true)` 를 불러 `/readyz` 는
/// "수집 리더 정상" 을 보고한다. 완전히 조용한 실패다(2차 리뷰가 지적한 CRITICAL).
///
/// F1 의 산술("합계가 64 또는 0")은 그 상태에서도 **성립한다** — `shards_owned()` 이
/// `is_leader()` 하나에서 파생되기 때문이다. 불변식을 코드로 강제해도 이 실패는
/// 잡히지 않는다. 그래서 역할 검사를 리스 획득보다 **앞에** 둔다.
fn spawn_leader_loop(
    config: &Config,
    worker_id: String,
    stores: Stores,
    readiness: Arc<Readiness>,
    shutdown: Arc<Shutdown>,
) -> Option<tokio::task::JoinHandle<()>> {
    use dbmon::worker::{LeaderGate, tick_interval};
    use dbmon_core::time::{Clock, SystemClock};

    // **역할 검사가 리스 획득보다 먼저다.** 아래 doc 주석 참고.
    if !config.role.runs_collector() {
        tracing::info!(
            role = ?config.role,
            "수집 역할이 아니다 — 수집 리더 리스를 잡지 않는다"
        );
        return None;
    }

    let interval = tick_interval(config.collector.detect_interval_ms);
    let discovery_interval = Duration::from_secs(config.discovery.interval_secs);
    let deployment_region = config.aws.region.clone();
    // **`Arc` 로 든다.** `select!` 팔에 참조를 넘기면 `implementation of Send is not
    // general enough` 로 컴파일이 깨진다 — 참조 인자에 대해 `for<'a>` Send 를
    // 증명해야 하기 때문이다.
    let config = Arc::new(config.clone());
    let collect_deps = CollectDeps {
        store: Arc::clone(&stores.slow_query),
        auth: build_auth(&config, &deployment_region, &stores.sdk),
        config: Arc::clone(&config),
        worker_id: worker_id.clone(),
    };

    Some(tokio::spawn(async move {
        let mut gate = LeaderGate::new(stores.lease, SystemClock, worker_id);
        // 탐색기는 리더가 될 때까지 만들지 않는다 — standby 워커가 AWS 자격증명을
        // 요구하면 로컬 개발(SSO 만료)에서 기동만으로 에러가 난다.
        let mut sources: Option<Arc<Vec<dbmon::aws::rds::RdsDiscovery>>> = None;
        // **마지막 탐색 시각.** 0 이면 리더가 된 직후 한 번 돈다 — 5분을 기다리면
        // 배포 직후 목록이 비어 있고, 그건 장애로 보인다.
        let mut last_discovery_ms: i64 = 0;
        // 인스턴스별 수집 태스크. **리더만 갖는다.**
        let mut tasks = CollectTasks::new();

        loop {
            // 셧다운 신호가 오면 수집을 멈추고 리스를 반납하고 나간다.
            if shutdown.is_triggered() {
                // **태스크를 먼저 멈춘다.** 리스를 반납한 뒤에도 돌고 있으면
                // 다음 리더와 중복 수집이 된다.
                tasks.abort_all();
                gate.release().await;
                readiness.set_collect_leader(false);
                return;
            }

            gate.refresh().await;
            readiness.set_collect_leader(gate.is_leader());

            // **리더가 아니면 수집 태스크를 즉시 멈춘다.**
            //
            // `LeaderGate` 는 갱신에 실패하면 `held` 를 비우지만, **이미 띄운 태스크는
            // 그것과 무관하게 계속 돈다.** 멈추지 않으면 새 리더와 같은 인스턴스를
            // 중복 수집하고 다이제스트 누산기가 last-writer-wins 로 조용히 손상된다 —
            // 리스 기구가 존재하는 이유 그 자체가 무너진다.
            //
            // 리스를 잃는 것은 정상 경로다(재배포, 일시적 장애). 그래서 매 tick 확인한다.
            if !gate.is_leader() {
                if !tasks.running().is_empty() {
                    tracing::warn!(
                        stopped = tasks.running().len(),
                        "리더가 아니다 — 수집 태스크를 전부 멈춘다 (중복 수집 방지)"
                    );
                    tasks.abort_all();
                }
            } else {
                let now_ms = SystemClock.now_ms();
                if now_ms - last_discovery_ms >= discovery_interval.as_millis() as i64 {
                    // **실행 전에 시각을 찍는다.** 실패해도 다음 주기까지 기다린다 —
                    // 실패 시 즉시 재시도하면 AWS 장애 중에 핫 루프가 된다.
                    last_discovery_ms = now_ms;
                    if sources.is_none() {
                        sources = Some(Arc::new(build_discovery(&config).await));
                    }
                    // `sources` 는 바로 위에서 채워졌다. 빈 벡터여도 `discover` 가
                    // 부분 결과로 처리하므로 등록부가 비워지지 않는다.
                    let srcs = sources.clone().unwrap_or_default();

                    // ① 조회 — 취소 가능하다(읽기와 순수 판정뿐).
                    //
                    // `select!` 를 별도 async 함수로 빼면 `implementation of Send is
                    // not general enough` 로 컴파일이 깨진다(참조 인자에 대해
                    // `for<'a>` Send 를 증명해야 한다). 수명이 구체적인 여기서 한다.
                    let budget = dbmon::worker::work_budget();
                    let outcome = tokio::select! {
                        o = discover(srcs, Arc::clone(&config), now_ms) => Some(o),
                        _ = tokio::time::sleep(budget) => {
                            tracing::warn!(
                                budget_ms = budget.as_millis() as u64,
                                "탐색 조회가 예산을 초과했다 — 부분 결과로 처리한다"
                            );
                            // **라운드를 버리지 않는다.** 버리면 다음 라운드도 같은
                            // 이유로 초과해 등록부가 영구히 갱신되지 않는다.
                            Some(dbmon::discovery::RoundOutcome {
                                truncated: true,
                                ..Default::default()
                            })
                        }
                        // 종료 중이다. 재조정을 시작하지 않는다 —
                        // 쓰기를 벌여 놓고 죽는 것이 최악이다.
                        _ = shutdown.wait() => None,
                    };

                    // ② 재조정 — **취소하지 않는다.** `mark_missing` 이 비멱등이다.
                    // `None` 은 종료 중이라는 뜻이다 — 아래 셧다운 처리로 넘긴다.
                    if let Some(outcome) = outcome {
                        // 관측용 수치를 먼저 읽는다 — `outcome` 은 이동한다.
                        let (filtered, unmappable) = (outcome.filtered, outcome.unmappable);
                        match dbmon::discovery::reconcile(
                            Arc::clone(&stores.registry),
                            outcome,
                            now_ms,
                        )
                        .await
                        {
                            Ok(s) => tracing::info!(
                                inserted = s.inserted,
                                updated = s.updated,
                                missing = s.missing,
                                deleted = s.deleted,
                                excluded = s.excluded,
                                filtered,
                                unmappable,
                                skipped = s.skipped_missing_check,
                                errors = s.errors,
                                "탐색 완료"
                            ),
                            // **탐색 실패로 루프를 죽이지 않는다.** 다음 주기에
                            // 다시 시도한다 — 여기서 죽으면 리스도 반납되지 않아
                            // 최대 60초 수집 공백이 된다.
                            Err(e) => tracing::warn!(
                                error = %telemetry::Scrubbed(&e),
                                "재조정 실패 — 다음 주기에 재시도한다"
                            ),
                        }
                    }

                    // ③ 수집 태스크 집합을 등록부에 맞춘다.
                    //
                    // 재조정 **뒤에** 한다 — 방금 `Excluded` 로 바뀐 인스턴스의
                    // 태스크를 같은 라운드에서 내려야 한다. 앞에 두면 필터에서
                    // 빠진 인스턴스를 5분 더 수집한다.
                    match stores.registry.list().await {
                        Ok(instances) => {
                            tasks.reconcile(&instances, &collect_deps).await;
                            let collecting = tasks.running().len();
                            readiness.record_collect_ok(SystemClock.now_ms());
                            tracing::info!(
                                collecting,
                                registered = instances.len(),
                                "수집 태스크 집합 갱신"
                            );
                        }
                        Err(e) => tracing::warn!(
                            error = %telemetry::Scrubbed(&e),
                            "등록부를 읽을 수 없다 — 태스크 집합을 유지한다"
                        ),
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.wait() => {
                    tasks.abort_all();
                    gate.release().await;
                    readiness.set_collect_leader(false);
                    return;
                }
            }
        }
    }))
}

/// `/healthz` 에 최소 HTTP/1.1 요청을 보낸다.
///
/// **HTTP 클라이언트 의존성을 추가하지 않는다.** 컨테이너 이미지에 `curl` 을 넣거나
/// `reqwest` 를 링크하는 대신 25줄을 쓴다 — 헬스체크는 200 여부만 알면 된다.
async fn healthcheck(port: u16) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .context("연결 타임아웃")??;

    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .context("요청 전송 실패")?;

    let mut buf = Vec::with_capacity(256);
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
        .await
        .context("응답 타임아웃")??;

    let head = String::from_utf8_lossy(&buf[..buf.len().min(64)]);
    if head.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        anyhow::bail!("예상치 못한 응답: {}", head.lines().next().unwrap_or(""))
    }
}

fn parse_role(s: &str) -> anyhow::Result<Role> {
    match s {
        "all" => Ok(Role::All),
        "api" => Ok(Role::Api),
        "collector" => Ok(Role::Collector),
        "control" => Ok(Role::Control),
        other => anyhow::bail!("알 수 없는 역할 {other:?}. all|api|collector|control"),
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let readiness = Readiness::new_for_role(config.role.runs_api(), config.role.runs_collector());
    readiness.set_config_loaded(true);

    let shutdown = Shutdown::new(Duration::from_secs(config.http.shutdown_grace_secs));

    tracing::info!(
        role = ?config.role,
        env = %config.deployment_env,
        region = %config.aws.region,
        account = %config.aws.account_id,
        target_regions = ?config.target_regions(),
        vpc_filter = ?config.discovery.allowed_vpc_ids,
        // 유도 기본값이 실제로 적용됐는지 **기동 로그에서 보여야 한다.**
        // 필터가 비어 있는데 조용히 뜨면 T-37 방어선이 없는 채로 돌아간다.
        name_deny = ?config.discovery.denied_name_substrings,
        discovery_interval_secs = config.discovery.interval_secs,
        "기동"
    );

    // ── HTTP 서버 ────────────────────────────────────────────────────────────
    // `/healthz` 와 `/readyz` 는 **역할과 무관하게** 항상 띄운다.
    // collector 전용 워커도 컨테이너 헬스체크를 받아야 한다.
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(readiness.clone());

    let addr = format!("{}:{}", config.http.bind, config.http.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("{addr} 에 바인드할 수 없다"))?;
    tracing::info!(%addr, "HTTP 리스닝");

    let http_shutdown = shutdown.clone();
    let server = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move { http_shutdown.wait().await })
            .await;
        if let Err(e) = result {
            tracing::error!(error = %telemetry::Scrubbed(e), "HTTP 서버 종료 오류");
        }
    });

    // ── 저장소와 리더 게이트 ─────────────────────────────────────────────────
    //
    // **저장소에 붙지 못해도 기동은 된다.** `/readyz` 가 `storage_unavailable` 을
    // 정확히 보고하는 것이 맞다 — 기동 실패로 재시작 루프를 만들면 원인을 볼 수 없다.
    let leader_task = match build_stores(&config).await {
        Ok(stores) => {
            // **실제로 닿는지 확인한다.** 클라이언트 조립 성공만으로 준비 완료를
            // 보고하면 자격증명·테이블 이름이 틀려도 `/readyz` 가 초록이다.
            match stores.slow_query.probe().await {
                Ok(()) => {
                    readiness.set_storage_ok(true);
                    tracing::info!(table = %config.storage.data_table, "저장소 확인됨");
                }
                Err(e) => {
                    // 기동은 계속한다 — `/readyz` 가 사유를 보고하는 것이 맞다.
                    // 다만 준비 완료라고 거짓말하지 않는다.
                    tracing::error!(
                        table = %config.storage.data_table,
                        error = %telemetry::Scrubbed(&e),
                        "저장소에 닿을 수 없다 — standby 로 기동한다"
                    );
                }
            }
            let worker_id = worker_id(&config);
            tracing::info!(%worker_id, "워커 식별자");
            spawn_leader_loop(
                &config,
                worker_id,
                stores,
                readiness.clone(),
                shutdown.clone(),
            )
        }
        Err(e) => {
            // 여기서 죽지 않는다. `/readyz` 가 사유를 보고하고 사람이 고친다.
            tracing::error!(error = %telemetry::Scrubbed(&e), "저장소 연결 실패 — standby 로 기동한다");
            None
        }
    };

    let signal = wait_for_signal().await;
    tracing::info!(
        signal,
        grace_secs = config.http.shutdown_grace_secs,
        "종료 신호 수신"
    );

    // ① 먼저 준비 상태를 내려 로드밸런서 등록 해제를 유도한다.
    readiness.begin_draining();

    // 로드밸런서가 없으면 기다리지 않는다. 로컬 개발에서 22초를 버리지 않도록.
    let drain = Duration::from_secs(config.http.deregistration_wait_secs);
    let r = readiness.clone();
    let s = shutdown.clone();
    run_stages(
        shutdown.grace(),
        vec![
            (
                "deregister",
                stage(async move {
                    if drain.is_zero() {
                        tracing::info!("로드밸런서 없음 — 등록 해제 대기 생략");
                    } else {
                        tracing::info!(wait_ms = %drain.as_millis(), "등록 해제 대기");
                        tokio::time::sleep(drain).await;
                    }
                    let _ = r.snapshot();
                }),
            ),
            // **리스를 반납한다.** 그러면 다음 리더가 TTL(60초)을 기다리지 않는다.
            // 순서를 바꾸면 누산기가 이중 계산된다 (shutdown.rs 문서 참조).
            //
            // ⚠ **신호를 여기서 보낸다.** 처음 배선했을 때는 `trigger()` 를 다음 단계에
            // 뒀는데, 그러면 이 단계가 "아직 아무도 멈추라고 하지 않은" 루프를 10초 동안
            // 기다리다 타임아웃하고 리스는 반납되지 않았다 — 실제 프로세스 두 개로
            // 확인했다. 배선하지 않으면 보이지 않는 종류의 결함이다.
            //
            // 이 시점에는 등록 해제 대기가 이미 끝났으므로 HTTP 수신을 멈춰도 된다.
            (
                "lease",
                stage(async move {
                    s.trigger();
                    if let Some(task) = leader_task {
                        // 루프가 리스를 반납하고 나올 때까지 기다린다.
                        let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
                    }
                }),
            ),
        ],
    )
    .await;

    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    Ok(())
}
