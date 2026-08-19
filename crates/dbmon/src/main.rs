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
        tracing::info!(%url, "DynamoDB 엔드포인트 재지정 (로컬 개발 — 더미 자격증명)");
        loader = loader.endpoint_url(url).credentials_provider(
            aws_sdk_dynamodb::config::Credentials::new("local", "local", None, None, "dbmon-local"),
        );
    }
    let sdk = loader.load().await;
    let client = aws_sdk_dynamodb::Client::new(&sdk);

    Ok(Stores {
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

/// 한 번의 탐색: 리전별 `DescribeDBInstances` → T-37 필터 → 도메인 매핑 → 재조정.
///
/// # 필터를 통과하지 못한 것은 등록부에 넣지 않는다
///
/// prd 인스턴스를 등록부에 넣고 나중에 걸러도 되지만, 그러면 **한 곳이라도 필터를
/// 잊으면 prd 를 수집한다.** 애초에 들이지 않는 것이 방어선을 하나로 만든다.
async fn discovery_round(
    sources: &[dbmon::aws::rds::RdsDiscovery],
    registry: &Arc<dbmon::store::registry::DynamoInstanceRegistry>,
    config: &Config,
    now_ms: i64,
) -> anyhow::Result<dbmon::discovery::DiscoveryStats> {
    use dbmon::aws::discovery::to_instance;
    use dbmon::aws::filter::Filter;
    use dbmon_core::env::EnvMapping;

    let filter = Filter {
        allowed_vpc_ids: config.discovery.allowed_vpc_ids.clone(),
        required_tags: config.discovery.required_tags.clone(),
        denied_name_substrings: config.discovery.denied_name_substrings.clone(),
    };
    let mapping = EnvMapping::default();

    let mut discovered = Vec::new();
    // **한 리전이라도 부분 결과면 전체를 부분 결과로 본다.** 리전별로 나눠 판정하면
    // 실패한 리전의 인스턴스가 "사라졌다" 로 판정된다.
    let mut truncated = false;

    for source in sources {
        let page = match source.describe().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %telemetry::Scrubbed(&e), "리전 탐색 실패");
                truncated = true;
                continue;
            }
        };
        truncated |= page.truncated;

        for raw in &page.instances {
            let verdict = filter.judge(&raw.candidate());
            if !verdict.is_accept() {
                // 사유를 남긴다 — 조용히 거부하면 "왜 안 보이나" 를 추적할 수 없다.
                tracing::debug!(
                    instance = %raw.identifier,
                    reason = %verdict.reason(),
                    "탐색 필터가 거부했다"
                );
                continue;
            }
            match to_instance(raw, &config.aws.account_id, &mapping, now_ms) {
                Ok(i) => discovered.push(i),
                // MySQL 이 아닌 엔진은 정상적으로 흔하다. `debug` 로만 남긴다.
                Err(dbmon::aws::discovery::Unmappable::NotMysql { engine }) => {
                    tracing::debug!(instance = %raw.identifier, %engine, "MySQL 계열이 아니다");
                }
                Err(e) => {
                    tracing::warn!(instance = %raw.identifier, reason = ?e, "인스턴스 매핑 실패");
                }
            }
        }
    }

    Ok(dbmon::discovery::reconcile(registry, &discovered, truncated, now_ms).await?)
}

/// 리더 게이트 루프를 띄운다.
///
/// # 이 루프가 F1 을 강제하는 유일한 지점이다
///
/// `target_shard_count` 가 불변식을 계산하지만 **부르는 곳이 없으면 무효다** —
/// 이 프로젝트에서 "고쳤는데 호출부가 없다" 가 네 번 재발했다.
///
/// 리더가 아닌 동안에는 수집하지 않는다. `desiredCount=2` 로 띄운 두 워커가 모두
/// 수집하면 같은 인스턴스를 중복 수집하고 다이제스트 누산기가 손상된다.
fn spawn_leader_loop(
    config: &Config,
    worker_id: String,
    stores: Stores,
    readiness: Arc<Readiness>,
    shutdown: Arc<Shutdown>,
) -> tokio::task::JoinHandle<()> {
    use dbmon::worker::{LeaderGate, tick_interval};
    use dbmon_core::time::{Clock, SystemClock};

    let interval = tick_interval(config.collector.detect_interval_ms);
    let discovery_interval = Duration::from_secs(config.discovery.interval_secs);
    let runs_collector = config.role.runs_collector();
    let config = config.clone();
    let _slow_query = stores.slow_query;

    tokio::spawn(async move {
        let mut gate = LeaderGate::new(stores.lease, SystemClock, worker_id);
        // 탐색기는 리더가 될 때까지 만들지 않는다 — standby 워커가 AWS 자격증명을
        // 요구하면 로컬 개발(SSO 만료)에서 기동만으로 에러가 난다.
        let mut sources: Option<Vec<dbmon::aws::rds::RdsDiscovery>> = None;
        // **마지막 탐색 시각.** 0 이면 리더가 된 직후 한 번 돈다 — 5분을 기다리면
        // 배포 직후 목록이 비어 있고, 그건 장애로 보인다.
        let mut last_discovery_ms: i64 = 0;

        loop {
            // 셧다운 신호가 오면 리스를 반납하고 나간다.
            if shutdown.is_triggered() {
                gate.release().await;
                readiness.set_collect_leader(false);
                return;
            }

            gate.refresh().await;
            readiness.set_collect_leader(gate.is_leader());

            if runs_collector && gate.is_leader() {
                let now_ms = SystemClock.now_ms();
                if now_ms - last_discovery_ms >= discovery_interval.as_millis() as i64 {
                    last_discovery_ms = now_ms;
                    if sources.is_none() {
                        sources = Some(build_discovery(&config).await);
                    }
                    let srcs = sources.as_deref().unwrap_or_default();
                    match discovery_round(srcs, &stores.registry, &config, now_ms).await {
                        Ok(s) => tracing::info!(
                            inserted = s.inserted,
                            updated = s.updated,
                            missing = s.missing,
                            deleted = s.deleted,
                            skipped = s.skipped_missing_check,
                            errors = s.errors,
                            "탐색 완료"
                        ),
                        // **탐색 실패로 루프를 죽이지 않는다.** 다음 주기에 다시 시도한다 —
                        // 여기서 죽으면 리스도 반납되지 않아 최대 60초 수집 공백이 된다.
                        Err(e) => tracing::warn!(
                            error = %telemetry::Scrubbed(&e),
                            "탐색 실패 — 다음 주기에 재시도한다"
                        ),
                    }
                }

                // TODO(M2-6): 등록부의 수집 대상마다 `detect_tick()` 을 돈다.
                // 대상 접속에는 `AuthTokenProvider`(IAM DB Auth) 가 먼저 필요하다.
                tracing::trace!(
                    shards = gate.shards_owned(),
                    epoch = ?gate.epoch(),
                    "수집 tick (대상 인증 대기 중)"
                );
            }

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.wait() => {
                    gate.release().await;
                    readiness.set_collect_leader(false);
                    return;
                }
            }
        }
    })
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
    let readiness = Readiness::new(config.role.runs_api());
    readiness.set_config_loaded(true);

    let shutdown = Shutdown::new(Duration::from_secs(config.http.shutdown_grace_secs));

    tracing::info!(
        role = ?config.role,
        env = %config.deployment_env,
        region = %config.aws.region,
        account = %config.aws.account_id,
        target_regions = ?config.target_regions(),
        vpc_filter = ?config.discovery.allowed_vpc_ids,
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
            readiness.set_storage_ok(true);
            let worker_id = worker_id(&config);
            tracing::info!(%worker_id, "저장소 연결됨");
            Some(spawn_leader_loop(
                &config,
                worker_id,
                stores,
                readiness.clone(),
                shutdown.clone(),
            ))
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
