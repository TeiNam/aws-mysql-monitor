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
async fn build_stores(
    config: &Config,
) -> anyhow::Result<(
    Arc<dbmon::store::DynamoSlowQueryStore>,
    Arc<dbmon::store::lease::DynamoLeaseStore>,
)> {
    use aws_config::BehaviorVersion;

    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(config.aws.region.clone()));
    if let Some(url) = &config.storage.endpoint_url {
        // **로컬 개발 경로.** SSO 가 만료돼도 저장 경로를 돌릴 수 있다.
        tracing::info!(%url, "DynamoDB 엔드포인트 재지정 (로컬 개발)");
        loader = loader.endpoint_url(url);
    }
    let sdk = loader.load().await;
    let client = aws_sdk_dynamodb::Client::new(&sdk);

    let store = Arc::new(dbmon::store::DynamoSlowQueryStore::new(
        client.clone(),
        config.storage.data_table.clone(),
    ));
    let lease_store = Arc::new(dbmon::store::lease::DynamoLeaseStore::new(
        client,
        config.storage.data_table.clone(),
    ));
    Ok((store, lease_store))
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
    _store: Arc<dbmon::store::DynamoSlowQueryStore>,
    lease_store: Arc<dbmon::store::lease::DynamoLeaseStore>,
    readiness: Arc<Readiness>,
    shutdown: Arc<Shutdown>,
) -> tokio::task::JoinHandle<()> {
    use dbmon::worker::{LeaderGate, tick_interval};
    use dbmon_core::time::SystemClock;

    let interval = tick_interval(config.collector.detect_interval_ms);
    let runs_collector = config.role.runs_collector();

    tokio::spawn(async move {
        let mut gate = LeaderGate::new(lease_store, SystemClock, worker_id);

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
                // TODO(M2-5): 여기서 탐색된 인스턴스마다 `detect_tick()` 을 돈다.
                // 지금은 리더 게이트만 배선됐다 — 인스턴스 레지스트리가 필요하다.
                tracing::trace!(
                    shards = gate.shards_owned(),
                    epoch = ?gate.epoch(),
                    "수집 tick (인스턴스 레지스트리 대기 중)"
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
        Ok((store, lease_store)) => {
            readiness.set_storage_ok(true);
            let worker_id = worker_id(&config);
            tracing::info!(%worker_id, "저장소 연결됨");
            Some(spawn_leader_loop(
                &config,
                worker_id,
                store,
                lease_store,
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
