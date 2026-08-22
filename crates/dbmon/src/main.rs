//! `dbmon` 실행 파일. **조립만 한다.**
//!
//! 역할은 기동 플래그로 고른다([02 §2](../../docs/02-architecture.md)).
//! 구현체 주입은 여기서 1회 하고, 이후 모든 코드는 `core` 의 포트만 본다.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::{Parser, Subcommand};
use dbmon::config::{Config, Role};
use dbmon::health::{Readiness, healthz, readyz};
use dbmon::shutdown::{Shutdown, run_stages, stage, wait_for_signal};
use dbmon::telemetry;
use dbmon_core::ports::InstanceRegistry;

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
async fn build_stores(config: &Config, hub: &dbmon::api::hub::Hub) -> anyhow::Result<Stores> {
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

    // **여기서 한 번 감싼다.** 수집기·백필·고아 정리가 모두 이 저장소를 쓰므로
    // 저장 경로 어디서든 방송이 빠질 수 없다. 호출부마다 `publish` 를 넣으면
    // 빠뜨릴 기회가 경로 수만큼 생긴다.
    let slow_query = Arc::new(dbmon::store::broadcast::BroadcastingStore::new(
        Arc::new(dbmon::store::DynamoSlowQueryStore::new(
            client.clone(),
            config.storage.data_table.clone(),
        )),
        hub.clone(),
    ));

    Ok(Stores {
        slow_query,
        lease: Arc::new(dbmon::store::lease::DynamoLeaseStore::new(
            client.clone(),
            config.storage.data_table.clone(),
        )),
        // **config 테이블의 유일한 소비자다.** 정지 스코프는 수집 데이터가 아니라
        // 사람이 정한 상태이므로 데이터 테이블의 TTL·GSI 규칙과 섞지 않는다.
        pause: Arc::new(dbmon::control::PauseState::new(Arc::new(
            dbmon::store::pause::DynamoPauseStore::new(
                client.clone(),
                config.storage.config_table.clone(),
            ),
        ))),
        registry: Arc::new(
            dbmon::store::registry::DynamoInstanceRegistry::new(
                client.clone(),
                config.storage.data_table.clone(),
            )
            // **탐색 주기를 넘긴다.** 중복 미발견 방지 간격이 여기서 유도된다 —
            // 고정 상수면 짧은 주기 배포에서 정상 라운드가 거부된다.
            .with_discovery_interval(config.discovery.interval_secs),
        ),
        checkpoint: Arc::new(dbmon::store::checkpoint::DynamoCheckpointStore::new(
            client.clone(),
            config.storage.data_table.clone(),
        )),
        settings: Arc::new(dbmon::settings_state::SettingsState::new(Arc::new(
            dbmon::store::settings::DynamoSettingsStore::new(
                client.clone(),
                config.storage.config_table.clone(),
            ),
        ))),
        // 튜닝 권고는 **데이터 테이블**이다 — 레코드에 딸린 산출물이므로 TTL 로
        // 함께 사라져야 한다.
        advice: Arc::new(dbmon::store::tuning::DynamoTuningStore::new(
            client,
            config.storage.data_table.clone(),
        )),
    })
}

/// 조립된 저장소들. 인자 5개를 넘기는 대신 묶는다.
struct Stores {
    slow_query: Arc<dbmon::store::AppSlowQueryStore>,
    lease: Arc<dbmon::store::lease::DynamoLeaseStore>,
    /// 정지 스코프. **API 와 리더 루프가 같은 것을 본다** — 그래서 어느 워커에서
    /// 눌러도 같은 상태가 되고, 재시작·리더 교체에도 남는다.
    pause: Arc<dbmon::control::PauseState>,
    registry: Arc<dbmon::store::registry::DynamoInstanceRegistry>,
    /// 백필 재개 지점. **없으면 중단 구간이 영구히 빈다.**
    checkpoint: Arc<dbmon::store::checkpoint::DynamoCheckpointStore>,
    /// 운영 설정(탐색 범위·알림·인증·모델). config 테이블의 `CFG/GLOBAL`.
    settings: Arc<dbmon::settings_state::SettingsState>,
    /// AI 튜닝 권고 저장소.
    advice: Arc<dbmon::store::tuning::DynamoTuningStore>,
}

/// CloudWatch 메트릭 서비스를 만든다.
///
/// **저장소가 로컬인지로 판단하지 않는다.** 처음엔 `endpoint_url` 이 있으면 만들지 않게
/// 했는데, "DynamoDB 는 로컬 + 대상은 실제 AWS" 조합(개발 중 가장 흔한 형태)에서
/// 메트릭이 조용히 꺼졌다. 자격증명이 없으면 조회가 실패하고 그때 **빈 값 + 경고 로그**로
/// 나타난다 — 그게 화면이 503 을 받는 것보다 낫다(다른 값은 계속 보인다).
///
/// **클라이언트는 리전마다 필요하고, 리전 목록은 런타임에 늘어난다**(운영 설정).
/// 그래서 여기서 만들지 않고 [`MetricFetchers`](dbmon::aws::cw_fetchers::MetricFetchers)
/// 가 처음 필요할 때 만들어 들고 있는다 — 기동 시 고정하면 나중에 추가한 리전의 값이
/// 오류 없이 빈 값으로 온다.
fn build_metrics(config: &Config) -> Arc<dbmon::api::metrics::MetricsService> {
    Arc::new(dbmon::api::metrics::MetricsService::new(Arc::new(
        // 우리 계정을 알려 준다 — 이 계정이면 역할을 맡지 않는다.
        dbmon::aws::cw_fetchers::MetricFetchers::new(config.aws.account_id.clone()),
    )))
}

/// 운영 설정을 주기적으로 갱신한다.
///
/// # 왜 별 태스크인가
///
/// 설정을 읽는 세 곳(탐색 루프·인증 계층·화면)의 주기가 전부 다르다. 탐색은 5분,
/// 화면은 사람이 열 때, **인증은 요청마다**다. 인증 판정은 I/O 를 할 수 없으므로
/// (요청 경로에서 저장소를 때리면 안 된다) 캐시가 항상 신선해야 하고, 그걸 유지하는
/// 책임은 아무 요청에도 매달릴 수 없다.
///
/// 주기는 캐시 TTL 의 1/3 이다 — 한 번 실패해도 다음 두 번의 기회 안에 신선함을
/// 회복한다(리스 갱신과 같은 계산).
fn spawn_settings_poller(
    settings: Arc<dbmon::settings_state::SettingsState>,
    shutdown: Arc<dbmon::shutdown::Shutdown>,
) {
    let interval =
        Duration::from_millis((dbmon::settings_state::CACHE_TTL_MS / 3).max(1_000) as u64);
    // **조회에 상한을 둔다.** AWS SDK 에는 요청 전체 상한이 기본으로 없다 — 응답하지
    // 않는 조회가 걸리면 폴러가 그 자리에 멈추고, 30초 뒤 인증이 조용히 잠긴다
    // (3차 교차 리뷰가 medium 으로 잡았다). 주기보다 짧게 잡아 매 주기 기회를 준다.
    let budget = interval
        .saturating_sub(Duration::from_millis(500))
        .max(Duration::from_secs(2));
    tokio::spawn(async move {
        loop {
            // **주기를 고정한다.** 조회에 쓴 시간을 다음 대기에서 빼지 않으면,
            // 예산 직전까지 걸린 조회 뒤에 전체 주기를 다시 자면서 **캐시가 만료된다**
            // (4차 교차 리뷰가 medium 으로 잡았다: 10초 주기 + 9.5초 조회 → 다음 조회가
            // 29.5초, TTL 30초에 0.5초만 남는다).
            let started = tokio::time::Instant::now();
            // **셧다운과 함께 기다린다.** 첫 조회도 예외가 아니다 — 그러지 않으면
            // 기동 직후 종료 신호가 조회 끝까지 막힌다.
            let refreshed = tokio::select! {
                r = tokio::time::timeout(budget, settings.refresh_now()) => Some(r),
                _ = shutdown.wait() => None,
            };
            match refreshed {
                Some(Err(_)) => tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    "설정 조회가 예산을 넘겼다 — 캐시가 낡으면 인증은 켜진 쪽으로 떨어진다"
                ),
                Some(Ok(())) => {}
                None => break,
            }
            let rest = interval.saturating_sub(started.elapsed());
            if rest.is_zero() {
                // 조회가 주기를 다 먹었다 — 쉬지 않고 바로 다시 시도한다(신선함이 우선).
                continue;
            }
            tokio::select! {
                _ = tokio::time::sleep(rest) => {}
                _ = shutdown.wait() => break,
            }
        }
        tracing::debug!("설정 폴러 종료");
    });
}

/// 탐색 대상(리전 × 계정)별 RDS 탐색기.
///
/// **DynamoDB 와 달리 `endpoint_url` 을 적용하지 않는다.** RDS 를 로컬로 흉내낼 방법이
/// 없고, 흉내낸다면 그건 탐색을 검증하는 게 아니라 목(mock)을 검증하는 것이다.
/// 로컬에서 탐색 로직을 검증하는 방법은 순수 함수 전수 테스트다
/// ([`dbmon::aws::discovery`], [`dbmon::discovery`]).
///
/// 범위는 **운영 설정**이 정하고 배포 설정이 기본값이다
/// ([`dbmon_core::settings::DiscoverySettings::targets`]).
async fn build_discovery(
    config: &Config,
    discovery: &dbmon_core::settings::DiscoverySettings,
) -> dbmon::aws::fleet::DiscoveryFleet {
    dbmon::aws::fleet::build(
        discovery,
        &config.aws.account_id,
        // 설정이 비었을 때의 기본값 — 배포 설정의 대상 리전이다.
        &config.target_regions(),
    )
    .await
}

/// 첫 실행이 거슬러 올라갈 구간 (5분).
///
/// 짧으면 기동 직후 구간을 잃는다 — 창보다 오래 걸린 쿼리는 첫 라운드에서 빠지고,
/// 체크포인트가 그 지점을 지나가면 **영구히 백필되지 않는다.**
const BACKFILL_INITIAL_LOOKBACK_MS: i64 = 5 * 60_000;

/// 일시정지 시 드레인 예산 (3초).
///
/// **셧다운 예산보다 훨씬 작다.** 일시정지는 루프를 계속 돌려야 하고, 여기서 오래
/// 막히면 그 동안 `gate.refresh()` 가 안 돌아 **리스를 잃는다** — 멈추려다 리더를
/// 놓치면 다른 워커가 수집을 이어받아 "멈춤" 이 되지 않는다.
const PAUSE_DRAIN_BUDGET: Duration = Duration::from_secs(3);

/// 백필이 한 번에 거슬러 올라갈 최대 구간 (1시간).
///
/// 며칠 멈춘 워커가 며칠치를 한 번에 읽으면 API 조절과 메모리 폭주가 함께 온다.
/// 잘린 구간은 로그로 알린다 — 그만큼 정확 지표가 영구히 없다.
const BACKFILL_MAX_LOOKBACK_MS: i64 = 60 * 60_000;

/// 리더 루프 안의 작업을 **예산 안에서** 돌린다.
///
/// # 왜 모든 블록에 필요한가
///
/// 리더 루프와 같은 태스크에서 도는 작업이 길어지면 그 동안 `gate.refresh()` 가
/// 불리지 않아 **리스가 만료된다.** 그러면 다른 워커가 리더가 되는데, 우리 수집
/// 태스크는 별도 태스크라 계속 돌며 자기 epoch 으로 레코드를 쓴다 —
/// `abort_all()` 은 루프가 이 블록에서 **돌아온 뒤에야** 실행된다.
///
/// 즉 예산이 없는 블록 하나가 "태스크는 자기 epoch 보다 오래 살 수 없다" 는
/// 불변식을 깨뜨린다. 탐색 블록만 예산을 지키고 스윕·백필은 지키지 않았다.
async fn run_in_budget<T>(f: impl std::future::Future<Output = T>) -> std::result::Result<T, ()> {
    tokio::time::timeout(dbmon::worker::work_budget(), f)
        .await
        .map_err(|_| ())
}

/// 내가 소유한 **진행 중 레코드를 사유와 함께 확정한다.**
///
/// # 왜 필요한가
///
/// 태스크를 내리기만 하면 그 레코드는 이 워커·이 epoch 소유로 남고, 고아 스윕은
/// 그것을 [`dbmon::orphan::Verdict::Mine`] 으로 보고 **매번 건너뛴다** — 리더가
/// 바뀌거나 TTL(35일)이 지날 때까지 화면에 유령 "실행 중" 으로 남는다(F4 가 막으려던
/// 상태). `collector_paused` 로 적어 두면 화면이 "사람이 멈췄다" 를 말한다.
///
/// # 왜 전역 목록을 훑는가
///
/// GSI1 은 워커별로 나뉘어 있지 않고 **오래된 순**으로 온다. 그래서 다른 워커의
/// 레코드가 앞자리를 차지할 수 있다. 상한을 스윕과 같게 두고, 꽉 찼으면 말한다 —
/// 그 뒤에 내 레코드가 남았을 수 있다는 뜻이다.
///
/// `only` 가 `None` 이면 내 것 전부(전체 정지), `Some(ids)` 면 그 인스턴스 것만
/// 확정한다(부분 정지 — 멈추지 않은 인스턴스의 실행 중 쿼리를 끊으면 안 된다).
/// 반환값은 **아직 못 닫은 인스턴스 집합**이다. 비어 있으면 끝났다.
///
/// # 왜 bool 이 아닌가
///
/// 처음에는 "완전히 끝냈는가" 를 bool 로 돌렸다. 그러면 호출부가 재시도할 때 **그 시점의
/// 정지 집합**을 다시 쓰는데, 실패한 인스턴스를 그 사이 재개하면 대상에서 빠져 **영구히
/// 안 닫힌다** — 이미 끝난 실행은 다시 관측되지 않고, 같은 워커·epoch 레코드는 고아
/// 스윕도 `Mine` 으로 건너뛴다(교차 리뷰 5회차). 남은 **대상**을 그대로 들고 있어야 한다.
/// `targets` 는 **인스턴스별 상한**이다 (`인스턴스 id → 그 인스턴스가 멈춘 시각`).
///
/// `None` 이면 전체 정지 — 내 것 전부를 끊는다(태스크를 모두 내렸으므로 새로 만들어질
/// 레코드도 없다).
///
/// # 왜 상한이 인스턴스별인가
///
/// 확정이 실패하면 대상을 들고 다음 라운드에 재시도한다. 그 사이 그 인스턴스를 **재개하면
/// 새 수집 태스크가 새 레코드를 만드는데**, 대상이 id 뿐이면 그 새 레코드까지
/// `abandoned` 로 닫는다 — 병합에서 `Abandoned` 는 나중의 `InFlight` 를 이기므로 되돌릴
/// 수 없다(교차 리뷰 6회차).
///
/// 상한을 **하나의 스칼라**로 두면 그것도 부족하다. A 가 남아 있는 동안 B 가 멈추면 상한이
/// B 의 시각으로 넓어져 A 의 새 레코드가 다시 그 안에 들어온다 — 같은 결함이 한 라운드
/// 뒤에 재발한다. 각 대상이 **자기를 멈춘 시각**을 들고 있어야 한다.
async fn close_in_flight_mine(
    store: &Arc<dbmon::store::AppSlowQueryStore>,
    worker_id: &str,
    epoch: Option<u64>,
    now_ms: i64,
    targets: Option<&std::collections::BTreeMap<String, i64>>,
) -> std::collections::BTreeMap<String, i64> {
    use dbmon_core::ports::SlowQueryStore as _;
    // **실패를 조용히 버리지 않는다.** 여기서 못 닫으면 그 레코드는 진행 중으로 남고,
    // 고아 스윕은 `owner_worker`·`owner_epoch` 가 자기와 같은 것을 건너뛰므로
    // (그게 "내가 돌리는 중" 의 정의다) **아무도 닫지 않는다.** 다음 정지 전이까지
    // 남는다 — 이 함수는 전이에서만 불린다(교차 리뷰 3회차).
    // **상한보다 하나 더 요청한다.**
    //
    // 정확히 상한만큼 오면 "더 있는가" 를 알 수 없어 잘린 것으로 봐야 했고, 그러면 딱
    // 500건인 정상 상태에서 대상이 영구히 남아 매 라운드 다시 조회한다(교차 리뷰 9회차가
    // nit 로 지적). 하나 더 받아 보면 그 모호함이 사라진다.
    let in_flight = match store.list_in_flight(ORPHAN_SWEEP_LIMIT + 1).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %telemetry::Scrubbed(&e),
                "정지 확정을 위한 진행 중 목록을 읽지 못했다 — 다음 tick 에 다시 시도한다"
            );
            // 목록을 못 읽었으면 **대상 전체**가 남은 것으로 본다.
            return targets.cloned().unwrap_or_default();
        }
    };
    // **상한을 채웠으면 관측이 불완전하다.** 그때 빈 `remaining` 을 돌려주면 호출부가
    // "끝났다" 로 읽고 대상을 지운다 — 페이지 밖에 남은 레코드는 그 뒤로 아무도 닫지
    // 않는다(고아 스윕이 같은 워커·epoch 를 `Mine` 으로 건너뛴다). 대상을 유지한다.
    let truncated = in_flight.len() > ORPHAN_SWEEP_LIMIT;
    if truncated {
        tracing::warn!(
            limit = ORPHAN_SWEEP_LIMIT,
            "진행 중 레코드가 상한을 넘었다 — 관측이 불완전하므로 대상을 유지한다"
        );
    }
    let mut closed = 0usize;
    let mut remaining: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for q in &in_flight {
        if q.owner_worker.as_deref() != Some(worker_id) || q.owner_epoch != epoch {
            continue;
        }
        // 대상이 정해져 있으면 그 목록에 있어야 하고, **자기 상한 이전**이어야 한다.
        if let Some(map) = targets {
            let Some(&cutoff) = map.get(q.instance_id.as_str()) else {
                continue;
            };
            // **재개 후 새로 만들어진 레코드는 건드리지 않는다.**
            if q.started_at_ms >= cutoff {
                continue;
            }
        }
        let marked = dbmon::orphan::abandon_with(q, now_ms, "collector_paused");
        match store.upsert_merged(&marked).await {
            Ok(_) => closed += 1,
            // 같은 이유로 개별 실패도 남긴다 — 몇 건이 남았는지 알아야 한다.
            Err(e) => {
                // 상한을 그대로 들고 남긴다 — 재시도가 창을 넓히지 않게.
                let cutoff = targets
                    .and_then(|m| m.get(q.instance_id.as_str()).copied())
                    .unwrap_or(now_ms);
                remaining.insert(q.instance_id.as_str().to_string(), cutoff);
                tracing::warn!(
                    instance = %q.instance_id.as_str(),
                    thread_id = q.thread_id,
                    error = %telemetry::Scrubbed(&e),
                    "정지 확정 쓰기가 실패했다 — 이 레코드는 진행 중으로 남는다"
                );
            }
        }
    }
    if !remaining.is_empty() {
        tracing::warn!(
            failed = remaining.len(),
            closed,
            "정지 확정이 일부 실패했다 — 그 인스턴스를 계속 들고 다음 tick 에 다시 시도한다"
        );
    }
    if closed > 0 {
        tracing::warn!(
            closed,
            "정지로 진행 중 레코드를 추적 끊김으로 확정했다 (collector_paused)"
        );
    }
    if truncated {
        // 관측이 불완전하므로 대상 전체를 남긴다.
        return targets.cloned().unwrap_or_default();
    }
    remaining
}

/// 한 스윕에서 볼 진행 중 레코드 상한. 폭주 방어.
///
/// ⚠ **알려진 천장이다.** GSI1 은 워커별로 나뉘어 있지 않고 오래된 순으로 오므로,
/// 전역 진행 중 레코드가 이 수를 넘으면 그 뒤의 것은 이 스윕에서 보이지 않는다.
/// 500대 규모에서 동시 진행 중이 500건을 넘는 상황은 이미 비정상이지만, 그때
/// **조용히 일부만 처리한다**는 사실은 로그로 남긴다(일시정지 확정 경로).
const ORPHAN_SWEEP_LIMIT: usize = 500;

/// 고아 in_flight 스윕 한 번. **일시정지 중에도 돌아야 한다.**
///
/// `drain()` 은 정상 종료만 덮는다. 급사·SIGKILL·리더 교체로 사라진 워커의 레코드는
/// 이 스윕만 덮는다 — 없으면 TTL(35일)까지 화면에 유령 쿼리로 남는다.
///
/// # 왜 함수로 뺐는가
///
/// 일시정지 분기는 루프 뒤쪽을 건너뛰므로 **스윕도 함께 건너뛰었다**(4라운드 지적).
/// 사람이 며칠 멈춰 두면 다른 워커·이전 epoch 의 고아가 그동안 계속 "진행 중" 으로
/// 남는다 — F4 가 없애려던 그 상태다. 두 경로가 같은 코드를 부르면 다시 어긋나지 않는다.
async fn sweep_orphans(
    store: &Arc<dbmon::store::AppSlowQueryStore>,
    worker_id: &str,
    epoch: Option<u64>,
    now_ms: i64,
    config: &Config,
    // **지금 도는 수집 태스크.** `Mine` 판정의 근거다 — 내 이름·내 epoch 인데 태스크가
    // 없으면 살아 있을 수 없다(교차 리뷰 8·9회차가 지적한 정지 확정 누락의 근본).
    running: &dbmon::orphan::RunningTasks,
) {
    let threshold = dbmon::orphan::stale_threshold_ms(config.collector.detect_interval_ms);
    // **예산 안에서 돈다.** 리더 루프와 같은 태스크이므로 길어지면 `gate.refresh()`
    // 가 불리지 않아 리스가 만료되고, 그 사이 수집 태스크는 계속 돌아 두 리더가
    // 같은 인스턴스를 수집한다. `work_budget()` 의 주석이 설명하는 그 불변식이다.
    match run_in_budget(dbmon::orphan::sweep(
        Arc::clone(store),
        worker_id,
        // **현재 리스 epoch 를 넘긴다.** 이름만 보면 재시작한 자기 유령을 영구히 건너뛴다.
        epoch,
        now_ms,
        threshold,
        ORPHAN_SWEEP_LIMIT,
        Some(running),
    ))
    .await
    {
        Err(()) => tracing::warn!("고아 스윕이 예산을 초과했다 — 중단한다"),
        Ok(Ok(s)) if s.scanned > 0 => tracing::info!(
            scanned = s.scanned,
            abandoned = s.abandoned,
            alive = s.alive,
            mine = s.mine,
            errors = s.errors,
            "고아 스윕"
        ),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(
            error = %telemetry::Scrubbed(&e),
            "고아 스윕 실패 — 다음 주기에 재시도한다"
        ),
    }
}

/// 슬로우로그 소스를 만든다.
///
/// 로컬 파일이 설정돼 있으면(dev 전용) 그걸 쓴다 — SSO 가 만료돼도 백필 경로를
/// 끝까지 돌릴 수 있다. 그 외에는 CloudWatch Logs 다([05 §8.3]).
async fn build_slowlog_fetcher(config: &Config) -> Arc<dyn dbmon::slowlog::SlowLogFetcher> {
    use aws_config::BehaviorVersion;

    if let Some(path) = &config.collector.slowlog_file {
        tracing::info!(%path, "슬로우로그 소스: 로컬 파일 (dev)");
        return Arc::new(dbmon::slowlog::FileFetcher::new(path));
    }
    // **리전별 클라이언트를 만든다.** 로그 그룹 이름에는 리전이 없으므로 리전은
    // 클라이언트가 정한다 — 홈 리전 하나로 돌리면 타 리전 인스턴스는 조용히 결측되고,
    // 같은 식별자가 홈 리전에 있으면 **다른 DB 의 로그를 엉뚱한 인스턴스로 저장한다.**
    let mut by_region = std::collections::BTreeMap::new();
    for region in config.target_regions() {
        let sdk = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new(region.clone()))
            .load()
            .await;
        by_region.insert(
            region.clone(),
            dbmon::slowlog::CloudWatchFetcher::new(
                aws_sdk_cloudwatchlogs::Client::new(&sdk),
                region,
                // **이 클라이언트는 우리 계정 자격증명으로 돈다.** 크로스 계정
                // 인스턴스가 여기로 오면 페처가 거부한다 — 같은 이름의 우리 계정
                // 로그를 읽어 남의 인스턴스로 저장하는 것보다 낫다.
                &config.aws.account_id,
            ),
        );
    }
    tracing::info!(
        regions = ?by_region.keys().collect::<Vec<_>>(),
        "슬로우로그 소스: CloudWatch Logs"
    );
    Arc::new(dbmon::slowlog::fetch::RegionalFetchers::new(by_region))
}

/// 슬로우로그 백필 한 라운드.
///
/// **한 인스턴스의 실패가 나머지를 막지 않는다.** 백필은 과거를 채우는 일이고,
/// 한 인스턴스의 로그 그룹이 없다고 전체가 멈추면 안 된다.
async fn backfill_round(
    fetcher: &Arc<dyn dbmon::slowlog::SlowLogFetcher>,
    store: &Arc<dbmon::store::AppSlowQueryStore>,
    checkpoints: &Arc<dbmon::store::checkpoint::DynamoCheckpointStore>,
    instances: &[dbmon_core::instance::Instance],
    config: &Config,
    now_ms: i64,
) -> dbmon::slowlog::source::BackfillStats {
    use dbmon::slowlog::source::{BackfillStats, backfill};
    use dbmon::store::checkpoint::{resume_from, slowlog_job};

    let min_ms = (config.collector.slow_threshold_secs as i64) * 1000;
    // 첫 실행이 볼 구간. **백필 주기의 2배로는 부족하다** — 주기가 60초면 120초이고,
    // 그보다 오래 걸린 쿼리는 첫 라운드에서 창 밖이라 건너뛴 뒤 체크포인트가 그
    // 지점을 지나가 **영구히 백필되지 않는다.** 실측에서 26초 쿼리 3건 중 2건이
    // 그렇게 빠지는 것을 봤다(주기 10초, 창 20초).
    //
    // 첫 실행은 한 번뿐이고 `max_lookback` 으로 상한이 있으므로 넉넉하게 둔다.
    let initial_lookback_ms =
        BACKFILL_INITIAL_LOOKBACK_MS.max((config.collector.backfill_secs as i64) * 2_000);
    let max_lookback_ms = BACKFILL_MAX_LOOKBACK_MS;
    let mut total = BackfillStats::default();

    for instance in instances.iter().filter(|i| i.is_collectible()) {
        let job = slowlog_job(&instance.id);
        // **체크포인트에서 재개한다.** 고정 창만 쓰면 그 창보다 긴 중단이
        // 생겼을 때 그 구간의 정확 지표가 영구히 없다.
        let checkpoint = checkpoints.get(&job).await.unwrap_or_else(|e| {
            tracing::warn!(
                instance = %instance.id.as_str(),
                error = %telemetry::Scrubbed(&e),
                "체크포인트를 읽을 수 없다 — 기본 창으로 진행한다"
            );
            None
        });
        // **토큰이 있으면 그 창을 유지한다.** 창을 바꾸면 CloudWatch 가 토큰을 거부한다.
        let resume_token = checkpoint.as_ref().and_then(|c| c.next_token.clone());
        let (since_ms, skipped_ms) = resume_from(
            checkpoint.as_ref().map(|c| c.position_ms),
            now_ms,
            initial_lookback_ms,
            max_lookback_ms,
        );
        // 창이 잘렸으면 토큰도 버린다 — 다른 `start_time` 의 토큰은 무효다.
        let resume_token = if skipped_ms.is_some() {
            None
        } else {
            resume_token
        };
        if let Some(gap_ms) = skipped_ms {
            // **조용히 건너뛰지 않는다.** 그 구간은 정확 지표가 영구히 없다.
            tracing::warn!(
                instance = %instance.id.as_str(),
                gap_ms,
                "백필 재개 지점이 너무 오래됐다 — 구간을 건너뛴다 (그만큼 정확 지표가 없다)"
            );
        }
        let chunk = match fetcher
            .fetch(&instance.id, since_ms, resume_token.as_deref())
            .await
        {
            Ok(c) => c,
            // **원천이 없는 것과 장애를 구분한다.**
            //
            // 슬로우로그 그룹이 없으면(로그가 아직 안 쓰였거나 내보내기가 꺼졌다) 그건
            // 환경 사실이고 재시도로 해결되지 않는다. 매 주기 `warn` 으로 쌓으면
            // 진짜 장애가 그 소음에 묻힌다 — 대신 사유를 한 번 말하고 세어 둔다.
            Err(dbmon_core::error::DomainError::Unsupported { reason, .. }) => {
                total.no_source += 1;
                tracing::info!(
                    instance = %instance.id.as_str(),
                    %reason,
                    "슬로우로그 원천이 없다 — 이 인스턴스는 정확 지표가 비어 있다(장애가 아니다)"
                );
                continue;
            }
            Err(e) => {
                // **`debug` 가 아니라 세고 보고한다.** 조용히 건너뛰면 "이 인스턴스는
                // 정확 지표가 영구히 없다" 를 아무도 모른다.
                total.fetch_errors += 1;
                tracing::warn!(
                    instance = %instance.id.as_str(),
                    error = %telemetry::Scrubbed(&e),
                    "슬로우로그를 가져올 수 없다 — 이 인스턴스는 정확 지표가 비어 있다"
                );
                continue;
            }
        };
        if chunk.has_more {
            // 한 라운드에 다 못 읽었다. 체크포인트가 있으므로 다음 라운드가 이어받지만,
            // 계속 뒤처지면 그걸 알아야 한다.
            total.incomplete += 1;
        }
        // **우리 계정으로 들어온 문장은 버린다.** 플랜 재실행(`EXPLAIN FORMAT=JSON`)이
        // 임계값을 넘으면 MySQL 이 슬로우로그에 쓰고, 그걸 그대로 저장하면 화면·통계가
        // **우리 자신의 문장으로 오염된다**(실측: 100행 중 34행). 실시간 경로는
        // `Excludes.users` 로 이미 같은 일을 한다.
        let mut parsed =
            dbmon::slowlog::parse(&chunk.text, min_ms, &config.collector.monitor_db_user);

        // **상한에 걸렸으면 마지막 엔트리를 버린다.**
        //
        // 슬로우로그 엔트리 하나가 CloudWatch 이벤트 여러 개에 걸칠 수 있고, 상한이 그
        // 중간에서 멈추면 파서는 청크 끝에서 현재 엔트리를 **그대로 확정한다** — 잘린
        // SQL 이 잘린 다이제스트로 저장되어 사전을 오염시킨다(교차 리뷰 8·9회차).
        //
        // 경계를 **파서의 엔트리**로 잡는다. 바이트에서 `# Time:` 을 찾는 방식은 SQL 본문
        // 안의 그 문자열을 경계로 보므로 같은 오염을 다른 경로로 만든다(9회차가 그걸
        // 실증했다) — 파서만이 무엇이 엔트리인지 안다.
        //
        // 버린 엔트리는 **다시 읽는다**: 체크포인트를 남긴 마지막 엔트리 기준으로 잡고
        // 페이지 토큰을 버린다. 중복은 `record_id` 로 병합되므로 안전하다.
        // 상한에 걸렸으면 **마지막 엔트리를 항상 버린다.**
        //
        // 그게 잘렸을 수 있다 — 엔트리 하나가 이벤트 여러 개에 걸치고 상한이 그 중간에서
        // 멈추면 파서는 청크 끝에서 현재 엔트리를 확정하고, 종결 세미콜론을 요구하지 않는다.
        // **잘린 SQL 은 잘린 다이제스트로 저장되어 사전을 오염시킨다.**
        //
        // 전에는 엔트리가 하나뿐이면 남겼다("버리면 진행이 0 이다"). 그건 오염을 그
        // 경우에만 허용하는 것이었고, 교차 리뷰 11회차가 그걸 지적했다. 진행은 아래에서
        // 위치를 1ms 넘겨 보장한다 — **오염보다 결측이 낫다.**
        let capped = chunk.has_more;
        if capped && let Some(dropped) = parsed.entries.pop() {
            tracing::info!(
                instance = %instance.id.as_str(),
                remaining = parsed.entries.len(),
                dropped_ended_at_ms = dropped.ended_at_ms,
                "상한에서 마지막 엔트리를 버렸다 — 잘렸을 수 있다"
            );
        }
        // **소스가 시간 필터를 못 하는 경우를 여기서 막는다.**
        //
        // 파일 소스는 파일 전체를 준다. 그대로 병합하면 매 라운드 같은 수천 건을
        // 다시 쓴다 — 실제로 10초마다 1,588건을 재병합하는 것을 관측했다.
        // CloudWatch 는 `start_time` 을 적용하지만 경계가 이벤트 단위라 겹칠 수 있다.
        let before = parsed.entries.len();
        parsed.entries.retain(|e| e.ended_at_ms >= since_ms);
        let filtered_out = before - parsed.entries.len();
        if filtered_out > 0 {
            tracing::debug!(
                instance = %instance.id.as_str(),
                filtered_out,
                "재개 지점보다 오래된 엔트리를 건너뛴다"
            );
        }
        if !parsed.skipped.is_empty() {
            tracing::debug!(
                instance = %instance.id.as_str(),
                skipped = ?parsed.skip_counts(),
                "슬로우로그 엔트리를 건너뛰었다"
            );
        }
        match backfill(
            Arc::clone(store),
            &parsed.entries,
            instance,
            // **설정값을 쓴다.** 하드코딩하면 실시간 경로와 백필의 정책이 갈리고,
            // 한쪽만 원문을 저장하는 상태가 조용히 만들어진다.
            config.collector.literal_policy,
            now_ms,
        )
        .await
        {
            Ok(s) => {
                total.merged += s.merged;
                total.unnormalizable += s.unnormalizable;
                total.masking_degraded += s.masking_degraded;
                total.errors += s.errors;

                // **체크포인트를 옮긴다.** 처리한 마지막 엔트리 시각 + 1ms 다.
                //
                // 실패한 엔트리가 있으면 옮기지 않는다 — 옮기면 그 엔트리는 영구히
                // 다시 시도되지 않는다. 병합이 멱등이므로 다시 읽는 비용이 유실보다 싸다.
                if s.errors == 0 {
                    // **상한에 걸렸으면 토큰을 쓰지 않고 경계를 다시 읽는다.**
                    //
                    // 토큰은 우리가 읽은 페이지 **뒤**를 가리키므로 버린 엔트리를 다시
                    // 읽을 수 없다. 대신 마지막으로 처리한 엔트리 시각을 위치로 쓴다 —
                    // **`+1` 하지 않는다.** 버린 엔트리가 그 엔트리와 **같은 밀리초**일
                    // 수 있고(로그 시각은 ms 해상도다), `+1` 하면 재읽기 필터
                    // (`ended_at_ms >= since_ms`)가 그걸 걸러낸다(교차 리뷰 10회차).
                    //
                    // 같은 엔트리를 한 번 더 읽지만 `record_id` 로 병합되므로 안전하다.
                    // 파서가 불완전해서 `skipped` 로 보낸 꼬리도 이 재읽기에 들어온다.
                    let (position, token) = if capped {
                        // **전진을 보장한다.** 상한 안의 엔트리가 전부 같은 밀리초면
                        // 위치가 `since_ms` 와 같아져 다음 라운드가 같은 자리에서
                        // 시작한다 — 진행 0 이다. 그때는 1ms 넘기고 크게 남긴다.
                        // 멈춰 있는 것이 한 밀리초를 건너뛰는 것보다 나쁘다.
                        // **항상 전진한다.**
                        //
                        // 남은 엔트리의 최대 시각을 쓰되, 그게 `since_ms` 보다 크지 않거나
                        // 남은 엔트리가 아예 없으면(전부 걸러졌거나 하나뿐이어서 버렸다)
                        // 1ms 넘긴다. 위치를 안 쓰면 체크포인트가 갱신되지 않아 같은
                        // 청크를 영원히 다시 읽는다 — 폭주가 그 뒤의 모든 데이터를 굶긴다
                        // (교차 리뷰 11회차).
                        let last = parsed.entries.iter().map(|e| e.ended_at_ms).max();
                        let pos = match last {
                            Some(t) if t > since_ms => t,
                            other => {
                                tracing::warn!(
                                    instance = %instance.id.as_str(),
                                    since_ms,
                                    last_entry_ms = ?other,
                                    "상한 라운드가 전진하지 못한다 — 1ms 넘긴다 (그 구간의 정확 지표가 없다)"
                                );
                                since_ms + 1
                            }
                        };
                        (Some(pos), None)
                    } else {
                        (
                            chunk.next_since_ms.or_else(|| {
                                parsed
                                    .entries
                                    .iter()
                                    .map(|e| e.ended_at_ms)
                                    .max()
                                    .map(|t| t + 1)
                            }),
                            chunk.next_token.clone(),
                        )
                    };
                    // **토큰을 함께 저장한다.** 시각만 저장하면 상한에 걸린 자리를
                    // 표현할 수 없어 건너뛰거나 멈춘다(교차 리뷰 4회차).
                    let cursor = position.map(|position_ms| dbmon::store::checkpoint::Cursor {
                        position_ms,
                        next_token: token,
                    });
                    if let Some(c) = cursor
                        && let Err(e) = checkpoints.put(&job, &c).await
                    {
                        tracing::warn!(
                            instance = %instance.id.as_str(),
                            error = %telemetry::Scrubbed(&e),
                            "체크포인트 저장 실패 — 다음 라운드가 같은 구간을 다시 읽는다"
                        );
                    }
                }
            }
            Err(e) => tracing::warn!(
                instance = %instance.id.as_str(),
                error = %telemetry::Scrubbed(&e),
                "백필 실패 — 다음 주기에 재시도한다"
            ),
        }
    }
    total
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
    fleet: Arc<dbmon::aws::fleet::DiscoveryFleet>,
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
    if fleet.is_empty() {
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

    for source in fleet.sources.iter() {
        // **계정은 탐색기가 안다.** 배포 설정의 계정 번호를 쓰면 크로스 계정
        // 인스턴스가 우리 계정 키로 등록되고, 같은 이름의 DB 가 두 계정에 있을 때
        // 하나가 다른 하나를 덮어쓴다(`InstanceId` 가 계정을 포함하는 이유다).
        let account_id = source.account_id();
        let scope = dbmon::discovery::scope_key(account_id, source.region());
        let page = match source.describe().await {
            Ok(p) => p,
            Err(e) => {
                // **실패한 범위만 시야에서 뺀다.**
                //
                // 처음엔 전체를 부분 결과(`truncated`)로 처리했다. 그러면 계정 B 하나가
                // 실패할 때 **계정 A 에서 정말 사라진 인스턴스도 판정되지 않는다** —
                // 삭제 감지가 영구히 멈춘다(2차 교차 리뷰가 medium 으로 잡았다).
                // 시야에서 빼면 B 는 건드리지 않고 A 는 정상 판정된다.
                tracing::warn!(
                    %scope,
                    error = %telemetry::Scrubbed(&e),
                    "이 범위의 탐색이 실패했다 — 시야에서 뺀다(다른 범위는 판정한다)"
                );
                continue;
            }
        };
        if page.truncated {
            // 페이지네이션이 끊겼다 — 이 범위는 **부분 목록**이므로 판정하지 않는다.
            tracing::warn!(%scope, "이 범위의 목록이 잘렸다 — 시야에서 뺀다");
        } else {
            outcome.scanned_scope.insert(scope);
        }

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
                if let Some(id) = excluded_id(account_id, &raw.region, &raw.identifier) {
                    outcome.excluded_ids.insert(id);
                }
                continue;
            }
            match to_instance(raw, account_id, &mapping, now_ms) {
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
                    if let Some(id) = excluded_id(account_id, &raw.region, &raw.identifier) {
                        outcome.excluded_ids.insert(id);
                    }
                }
            }
        }
    }

    // **끝까지 읽은 범위가 하나도 없으면 부분 결과다.**
    //
    // 재조정은 `scanned_scope` 가 비어 있으면 "시야를 알 수 없다" 로 보고 판정을 그대로
    // 한다(옛 호출부 호환). 전 범위가 실패해서 비었을 때도 같은 값이므로, 그대로 두면
    // **등록부 전체가 미발견 → 삭제**로 간다 — 시야 개념을 도입하면서 만든 회귀다.
    //
    // 정상적으로 0대인 리전은 영향받지 않는다: 조회가 성공하면 인스턴스가 없어도
    // 위에서 시야에 들어간다.
    if outcome.scanned_scope.is_empty() {
        tracing::warn!("끝까지 읽은 범위가 없다 — 부분 결과로 처리한다(삭제 판정 없음)");
        outcome.truncated = true;
    }
    outcome
}

/// 인스턴스별 수집 태스크 집합.
struct CollectTasks {
    handles: std::collections::BTreeMap<String, tokio::task::JoinHandle<()>>,
    /// 인스턴스 → **그 태스크가 뜬 시각.**
    ///
    /// 고아 판정이 "지금 도는 태스크가 만진 레코드인가" 를 묻는 데 쓴다. id 만으로는
    /// 같은 이름·같은 epoch 으로 뜬 **새** 태스크가 옛 태스크의 레코드를 영구히 가린다
    /// (교차 리뷰 10회차).
    started_ms: std::collections::BTreeMap<String, i64>,
}

impl CollectTasks {
    fn new() -> Self {
        Self {
            handles: std::collections::BTreeMap::new(),
            started_ms: std::collections::BTreeMap::new(),
        }
    }

    /// 도는 인스턴스 id 집합. **델타 계산용**이다.
    fn running(&self) -> std::collections::BTreeSet<String> {
        self.handles.keys().cloned().collect()
    }

    /// 인스턴스 → 태스크가 뜬 시각. **고아 판정용**이다.
    fn running_since(&self) -> dbmon::orphan::RunningTasks {
        self.handles
            .keys()
            .map(|id| {
                let started = self.started_ms.get(id).copied().unwrap_or(0);
                (id.clone(), started)
            })
            .collect()
    }

    /// **끝난 태스크를 걷어낸다.** 이게 없으면 인스턴스가 영구히 수집되지 않는다.
    ///
    /// 수집 태스크는 첫 인증 실패·연결 옵션 구성 실패에서 `return` 한다. 그런데
    /// `JoinHandle` 은 맵에 남으므로 `running()` 이 그 인스턴스를 "돌고 있다" 로 보고,
    /// `to_start = desired - running` 에서 빠진다 — **다시 띄울 기회가 영원히 오지
    /// 않는다.** IAM 정책이 잠깐 잘못됐다가 고쳐져도 그 인스턴스는 죽은 채로 남는다.
    async fn reap_finished(&mut self) -> Vec<String> {
        let dead: Vec<String> = self
            .handles
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            // **패닉과 정상 종료를 구분한다.** `is_finished()` 가 참이므로 `await` 는
            // 즉시 반환한다 — 비용 0 인데, 구분하지 않으면 패닉이 tracing(JSON)
            // 스트림에 아예 나타나지 않는다(기본 패닉 훅은 stderr 로만 쓴다).
            self.started_ms.remove(id);
            if let Some(h) = self.handles.remove(id)
                && let Err(e) = h.await
                && e.is_panic()
            {
                tracing::error!(instance = %id, "수집 태스크가 패닉했다");
            }
        }
        dead
    }

    /// 목표 집합에 맞춰 태스크를 띄우고 죽인다.
    ///
    /// **교집합은 건드리지 않는다.** 죽이고 다시 띄우면 진행 중 캐시가 사라져
    /// 그 순간 실행 중이던 쿼리가 `in_flight` 고아가 된다.
    async fn reconcile(
        &mut self,
        instances: &[dbmon_core::instance::Instance],
        pause: &dbmon_core::pause::PauseSet,
        deps: &CollectDeps,
    ) {
        use dbmon::collect_loop::{desired_ids, index_by_id, task_delta};

        // 끝난 태스크를 먼저 걷어낸다 — 그래야 아래 `to_start` 가 그것들을 다시 띄운다.
        let dead = self.reap_finished().await;
        if !dead.is_empty() {
            tracing::warn!(
                count = dead.len(),
                instances = ?dead,
                "수집 태스크가 스스로 종료했다 — 다시 띄운다"
            );
        }

        let desired = desired_ids(instances, pause);
        let delta = task_delta(&self.running(), &desired);
        let by_id = index_by_id(instances);

        for id in &delta.to_stop {
            self.started_ms.remove(id);
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
            // **뜬 시각을 기록한다.** 고아 판정이 "이 태스크가 만진 것인가" 를 묻는다.
            self.started_ms.insert(id.clone(), {
                use dbmon_core::time::Clock as _;
                dbmon_core::time::SystemClock.now_ms()
            });
            tracing::info!(instance = %id, "수집 태스크 시작");
        }
    }

    /// **즉시** 전부 중지한다 — 리더를 잃었을 때 쓴다.
    ///
    /// 협조적 종료를 기다리지 않는다. 리더가 아닌데 계속 수집하면 새 리더와
    /// 중복이고, 그건 진행 중 캐시를 잃는 것보다 나쁘다.
    fn abort_all(&mut self) {
        for (id, h) in std::mem::take(&mut self.handles) {
            h.abort();
            tracing::debug!(instance = %id, "수집 태스크 중지 (리더 상실)");
        }
    }

    /// **협조적으로** 전부 정리한다 — 셧다운에 쓴다.
    ///
    /// # `abort_all` 로는 `drain()` 이 불리지 않는다
    ///
    /// 각 태스크는 루프 top 에서 셧다운을 관측하면 `drain()` 으로 진행 중 레코드를
    /// 확정하고 스스로 끝난다. 그런데 리더 루프가 **먼저 `abort()` 하면 그 경로에
    /// 도달하지 못한다** — 실제로 그렇게 배선해서 종료 후에도 `in_flight` 가 1개
    /// 남는 것을 확인했다. 태스크에 정리할 시간을 주는 것이 이 함수다.
    ///
    /// 예산을 넘긴 태스크는 abort 한다. 유령 몇 개가 남는 것이 종료가 막히는 것보다 낫다.
    async fn drain_all(&mut self, budget: Duration) {
        let handles = std::mem::take(&mut self.handles);
        if handles.is_empty() {
            return;
        }
        let count = handles.len();
        let deadline = tokio::time::Instant::now() + budget;
        let mut aborted = 0usize;
        for (id, mut h) in handles {
            // **`&mut h` 로 기다린다.** `h` 를 그대로 넘기면 소유권이 `timeout_at` 으로
            // 옮겨가고, 만료 시 그 future 가 드롭되면서 `JoinHandle` 도 함께 사라진다.
            // `JoinHandle` 드롭은 **취소가 아니라 분리(detach)** 다 — 태스크는 계속
            // 돈다. 그래서 예산을 넘긴 뒤 abort 할 대상이 남아 있지 않았다.
            match tokio::time::timeout_at(deadline, &mut h).await {
                Ok(_) => {}
                Err(_) => {
                    h.abort();
                    tracing::warn!(
                        instance = %id,
                        "수집 태스크가 정리 예산을 넘겼다 — 중단한다 (in_flight 가 남을 수 있다)"
                    );
                    aborted += 1;
                }
            }
        }
        tracing::info!(count, aborted, "수집 태스크 정리 완료");
    }
}

/// 수집 태스크가 필요한 의존성.
#[derive(Clone)]
struct CollectDeps {
    store: Arc<dbmon::store::AppSlowQueryStore>,
    /// 등록부. **첫 판정을 여기 쓴다** — `Pending` → `Collecting`/`Unreachable`.
    ///
    /// 탐색은 `Pending` 으로만 등록하므로, 붙어 본 결과를 아무도 쓰지 않으면 인스턴스가
    /// 영원히 `Pending` 에 머문다(실측). 항목별 사유까지 판정하는 것은 FR-DSC-10 의 몫이고
    /// 여기서는 **붙었는가/못 붙었는가**만 정한다.
    registry: Arc<dbmon::store::registry::DynamoInstanceRegistry>,
    /// 실시간 지표 방송. 슬로우 쿼리는 저장소 래퍼가 방송하지만 지표는
    /// 저장하지 않으므로(휘발성) 여기서 직접 넣는다.
    hub: dbmon::api::hub::Hub,
    auth: dbmon::aws::auth_token::TargetAuth,
    config: Arc<Config>,
    worker_id: String,
    /// **인스턴스별** 마지막 성공 시각 (FR-OPS-09 `CollectStaleness`).
    ///
    /// `Readiness` 를 직접 들지 않는다 — 거기 쓰면 전역 값 하나에 모든 태스크가
    /// 쓰게 되고 정상인 1대가 나머지 499대의 실패를 가린다. 리더 루프가 최솟값을
    /// 골라 준비 상태에 올린다.
    freshness: dbmon::collect_loop::CollectFreshness,
    /// 이 태스크가 속한 리스의 `epoch`. **저장 레코드의 펜싱 근거다** (F4).
    ///
    /// # 스냅샷이어도 되는 이유
    ///
    /// 리더를 잃으면 리더 루프가 매 tick `abort_all()` 한다. 즉 **태스크는 자기
    /// epoch 보다 오래 살 수 없다** — 리스를 잃고 다시 잡으면 epoch 가 올라가고
    /// 태스크도 새로 뜬다. `LeaderGate::refresh` 가 상실과 재획득을 같은 호출에서
    /// 하지 않으므로(상실 시 `held=None` 으로 반환) 최소 한 tick 의 간격이 보장된다.
    ///
    /// ⚠ 그 불변식이 깨지면(예: 상실 즉시 재획득) 이 값이 낡은다. `abort_all` 이
    /// 리더 상실 경로에 있는 것이 이 스냅샷의 전제다.
    epoch: Option<u64>,
    /// 셧다운 신호. **협조적 종료에 필요하다** — `abort()` 만으로 멈추면
    /// `drain()` 이 불리지 않아 `in_flight` 유령이 남는다.
    shutdown: Arc<Shutdown>,
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
    /// 갱신 **재시도** 하한.
    ///
    /// 갱신이 실패하면 `secret` 이 그대로이므로 `needs_refresh` 는 계속 참이다.
    /// 하한이 없으면 남은 여유(5분) 동안 **매 tick** 토큰을 다시 요청한다 —
    /// 인스턴스 500대면 워커 하나가 초당 500회 `provide_credentials()` 를 부르고,
    /// 그 호출이 IMDS/STS 로 나가면 조절 폭풍이 된다.
    const REFRESH_RETRY_MIN_MS: i64 = 30_000;

    tokio::spawn(async move {
        let db_user = deps.config.collector.monitor_db_user.clone();
        let label = instance.id.as_str().to_string();

        // **첫 판정을 한 번만 쓴다.**
        //
        // 매 tick 쓰면 인스턴스 500대 × 초당 1회의 등록부 쓰기가 된다. 그리고 `Pending`
        // 에서만 움직인다 — 이미 `Collecting` 인 인스턴스를 tick 하나 실패로 내리면
        // 일시적 장애마다 상태가 진동하고, 그 판정(Degraded/Unreachable 세분)은
        // 서킷 브레이커와 FR-DSC-10 의 몫이다.
        let promote = |to: dbmon_core::instance::InstanceState| {
            let registry = Arc::clone(&deps.registry);
            let id = instance.id.clone();
            let label = label.clone();
            async move {
                match registry.set_state(&id, to).await {
                    Ok(()) => {
                        tracing::info!(instance = %label, state = to.as_str(), "첫 판정 기록")
                    }
                    // **덮지 않은 것은 장애가 아니다.** 그 사이 탐색이 이 인스턴스를
                    // 수집 대상에서 뺐다(`Disabled`·`Excluded`·…). 재시도하면 안 되고,
                    // 다음 탐색 라운드의 재조정이 이 태스크를 멈춘다.
                    Err(dbmon_core::error::DomainError::Conflict(reason)) => tracing::info!(
                        instance = %label,
                        state = to.as_str(),
                        %reason,
                        "첫 판정을 쓰지 않았다 — 그 사이 수집 대상에서 빠졌다"
                    ),
                    Err(e) => tracing::warn!(
                        instance = %label,
                        state = to.as_str(),
                        error = %telemetry::Scrubbed(&e),
                        "첫 판정을 등록부에 쓰지 못했다 — 다음 태스크가 다시 시도한다"
                    ),
                }
            }
        };
        // **지금 등록부에 적혀 있다고 아는 상태.** 전이될 때만 쓴다.
        let mut recorded = instance.state;

        // **엔드포인트를 먼저 확인한다.** 없으면 토큰을 요청하지 않는다 —
        // 빈 호스트로 서명하면 STS 왕복만 낭비하고, 이어지는 오류가
        // "연결 옵션 구성 실패" 로만 나와 실제 원인(엔드포인트 없음)을 가린다.
        let Some(host) = instance.endpoint.clone() else {
            tracing::warn!(
                instance = %label,
                "엔드포인트가 없다 — 수집하지 않는다 (생성 중이거나 정지 상태다)"
            );
            if recorded == dbmon_core::instance::InstanceState::Collecting {
                promote(dbmon_core::instance::InstanceState::Unreachable).await;
            }
            return;
        };

        // **인스턴스의 계정·리전에 맞는 공급자를 고른다.** 없으면 접속하지 않는다 —
        // 다른 리전 공급자로 대신하면 서명이 틀린 토큰으로 붙으려 하고, 실패 원인이
        // IAM 정책처럼 보여 추적이 오래 걸린다. 계정이 다르면 애초에 없다(크로스 계정
        // 수집은 배선되지 않았다).
        let Some(auth) = deps
            .auth
            .for_instance(&instance.id, &deps.config.aws.account_id)
        else {
            tracing::error!(
                instance = %label,
                region = %instance.id.region(),
                cross_account = instance.id.account() != deps.config.aws.account_id,
                "이 인스턴스에 쓸 인증 공급자가 없다 — 수집하지 않는다"
            );
            return;
        };

        // 첫 연결. 실패하면 잠시 뒤 재시도한다 — 태스크를 끝내면 이 인스턴스는
        // 다음 탐색(5분)까지 수집되지 않는다.
        let mut secret = match auth.token(&host, instance.port, &db_user).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(instance = %label, error = %telemetry::Scrubbed(&e), "대상 인증 실패");
                if recorded == dbmon_core::instance::InstanceState::Collecting {
                    promote(dbmon_core::instance::InstanceState::Unreachable).await;
                }
                return;
            }
        };
        // **SSM 터널 오버라이드** (dev 전용). 토큰은 위에서 실제 엔드포인트로 이미
        // 서명됐고, 여기서 바뀌는 것은 TCP 주소뿐이다.
        let via = deps.config.collector.tunnel_for(instance.id.identifier());
        if let Some((h, p)) = via {
            tracing::info!(instance = %label, tunnel = %format!("{h}:{p}"), "터널로 접속한다 (dev)");
        }
        let make_db = |secret: &dbmon_core::secret::ExpiringSecret| -> anyhow::Result<TargetMysql> {
            let opts = target_opts(
                &instance,
                &db_user,
                secret.expose(),
                deps.config.deployment_env,
                via,
            )?;
            // **계획 JSON 형식을 버전으로 가른다.** 8.3+ 는 v2, 그 미만(Aurora 3.x 포함)은
            // v1. 판정이 틀려도 v1 로 흘러가므로 최악이 "예전과 같은 형식" 이다.
            Ok(
                TargetMysql::from_config(opts, &deps.config.collector, label.clone())?
                    .with_explain_json_v2(instance.engine_version.supports_explain_json_v2()),
            )
        };
        let db = match make_db(&secret) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(instance = %label, error = %telemetry::Scrubbed(&*e), "연결 옵션 구성 실패");
                if recorded == dbmon_core::instance::InstanceState::Collecting {
                    promote(dbmon_core::instance::InstanceState::Unreachable).await;
                }
                return;
            }
        };

        let params = CollectParams {
            slow_threshold_secs: deps.config.collector.slow_threshold_secs,
            deep_probe_limit: deps.config.collector.deep_probe_limit as usize,
            try_for_connection: true,
            literal_policy: deps.config.collector.literal_policy,
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
        // **리스 epoch 를 계승한다.** 이게 없으면 저장되는 모든 레코드의
        // `owner_epoch` 가 `None` 이고, F4 고아 판정의 펜싱 근거가 비어 있다.
        // `restore` 는 이 배선이 생길 때까지 비테스트 호출부가 없었다.
        collector.restore(None, deps.epoch);

        let mut last_refresh_attempt_ms: i64 = 0;
        // **실시간 지표 샘플러는 인스턴스마다 하나다** — 공유하면 다른 인스턴스의
        // 카운터를 뺀다. 여기가 `global_status()` 의 유일한 호출부다.
        let mut sampler = dbmon::metrics::MetricsSampler::new();

        loop {
            // **셧다운이면 협조적으로 정리하고 나간다.**
            //
            // `abort()` 만으로 멈추면 `drain()` 이 불리지 않아, 선행 저장된 레코드가
            // `state=in_flight` 로 남는다. F4 고아 스윕이 아직 없으므로 TTL(35일)까지
            // 화면에 "실행 중" 으로 남는다 — 배포마다 유령이 쌓인다.
            //
            // 리스 상실 시에는 반대로 즉시 `abort()` 가 맞다(중복 수집 방지).
            if deps.shutdown.is_triggered() {
                let stats = collector.drain().await;
                tracing::info!(
                    instance = %label,
                    finalized = stats.finalized,
                    "종료 — 진행 중 레코드를 정리했다"
                );
                return;
            }

            let tick_started = std::time::Instant::now();
            // 토큰이 만료에 가까우면 연결만 갈아 끼운다.
            let now_ms = SystemClock.now_ms();
            if secret.needs_refresh(now_ms, REFRESH_MARGIN_MS)
                && now_ms - last_refresh_attempt_ms >= REFRESH_RETRY_MIN_MS
            {
                last_refresh_attempt_ms = now_ms;
                match auth
                    .token(
                        instance.endpoint.as_deref().unwrap_or_default(),
                        instance.port,
                        &db_user,
                    )
                    .await
                {
                    Ok(fresh) => match make_db(&fresh) {
                        Ok(db) => {
                            // **이전 풀을 정리한다.** 드롭하면 커넥션이 COM_QUIT 없이
                            // 사라져 감시 대상 DB 의 `Aborted_clients` 가 오른다.
                            let old = collector.replace_db(db);
                            old.disconnect().await;
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

            // ── 실시간 지표 (5초 주기) ──────────────────────────────────────
            //
            // **탐지보다 먼저 실패해도 탐지를 막지 않는다.** 지표는 부가 기능이고
            // 탐지가 본업이다. 그래서 오류를 삼키지 않고 로그만 남기고 넘어간다.
            if sampler.is_due(now_ms) {
                match sampler.sample(collector.db(), now_ms).await {
                    Ok(metrics) => deps.hub.publish_status(instance.id.as_str(), metrics),
                    Err(e) => tracing::debug!(
                        instance = %label,
                        error = %telemetry::Scrubbed(&e),
                        "실시간 지표 조회 실패 — 이 샘플만 버린다"
                    ),
                }
            }

            match collector.detect_tick().await {
                Ok(stats) => {
                    // **복구가 되돌아올 수 있어야 한다.** 못 올리면 일시적으로 못 붙었던
                    // 인스턴스가 영구히 `Unreachable` 에 갇힌다 — 탐색의 `pick_state` 도
                    // 그 상태를 되돌리지 않는다(실측: `dev-mysql-ro` 가 QPS 는 흐르는데
                    // unreachable 로 남았다).
                    //
                    // `Pending` 은 여기 올 수 없다 — 그 상태는 태스크를 갖지 않는다
                    // (`should_collect`). 시작은 사람이 누른다.
                    if recorded != dbmon_core::instance::InstanceState::Collecting {
                        recorded = dbmon_core::instance::InstanceState::Collecting;
                        promote(recorded).await;
                    }
                    // **성공한 tick 만 신선도를 갱신한다.** 그리고 **인스턴스별로** 쓴다 —
                    // 전역 값 하나면 정상인 1대가 나머지 499대의 실패를 가린다.
                    // 준비 상태에 올리는 것은 리더 루프가 최솟값으로 한다.
                    deps.freshness.record(&label, SystemClock.now_ms());
                    tracing::trace!(
                        instance = %label,
                        candidates = stats.candidates,
                        finalized = stats.finalized,
                        "수집 tick"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        instance = %label,
                        error = %telemetry::Scrubbed(&e),
                        "수집 tick 실패"
                    );
                    // **첫 tick 이 실패하면 그 사실을 등록부에 남긴다.** 화면이
                    // `pending` 만 보여 주면 "왜 수집이 안 되나" 의 단서가 로그뿐이다.
                    // **`Collecting` 에서만 내린다.** 이미 `Unreachable` 인 것을 다시
                    // 쓰면 tick 마다 등록부에 쓰게 된다. 세분(Degraded / Unreachable)은
                    // 서킷 브레이커와 FR-DSC-10 의 몫이다.
                    if recorded == dbmon_core::instance::InstanceState::Collecting {
                        recorded = dbmon_core::instance::InstanceState::Unreachable;
                        promote(recorded).await;
                    }
                }
            }
            // **소요 시간을 뺀 만큼만 쉰다.**
            //
            // `sleep(tick)` 을 그냥 쓰면 실제 주기가 `tick + tick 소요` 가 된다 —
            // 800ms 짜리 tick 이면 1초 케이던스가 1.8초로 늘어나고, 탐지 해상도가
            // 조용히 절반이 된다. 그건 "임계값 2초 쿼리를 놓친다" 로 나타난다.
            let elapsed = tick_started.elapsed();
            tokio::time::sleep(tick.saturating_sub(elapsed)).await;
            if elapsed > tick {
                // 예산을 넘겼다. 쉬지 않고 바로 다음 tick 으로 간다 — 다만 조용히
                // 넘어가면 해상도 저하를 알 수 없다.
                tracing::debug!(
                    instance = %label,
                    elapsed_ms = elapsed.as_millis() as u64,
                    tick_ms = tick.as_millis() as u64,
                    "tick 이 주기를 넘겼다 — 탐지 해상도가 떨어진다"
                );
            }
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
/// 리더 루프가 쓰는 런타임 배선. **인자를 여덟 개 넘기지 않기 위해 묶는다.**
struct LoopWiring {
    readiness: Arc<Readiness>,
    shutdown: Arc<Shutdown>,
    hub: dbmon::api::hub::Hub,
    /// 화면에서 온 조작(멈춤·즉시 탐색·즉시 백필). 조회 API 와 공유한다.
    controls: Arc<dbmon::control::Controls>,
}

fn spawn_leader_loop(
    config: &Config,
    worker_id: String,
    stores: Stores,
    auth: dbmon::aws::auth_token::TargetAuth,
    wiring: LoopWiring,
) -> Option<tokio::task::JoinHandle<()>> {
    let LoopWiring {
        readiness,
        shutdown,
        hub,
        controls,
    } = wiring;
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

    // **리스 갱신 주기보다 오래 자지 않는다.**
    //
    // 이 루프는 자는 동안 `gate.refresh()` 를 부르지 않는다. `detect_interval_ms` 는
    // 설정 상한이 60초이므로 그대로 자면 (작업 시간 + 60초) 동안 갱신이 없고, 리스
    // TTL 이 60초라 **잠든 사이에 리더를 잃는다** — 그러면 다른 워커가 리더가 되어
    // 같은 인스턴스를 수집하거나(F1), 멈춤을 눌러 둔 워커가 있으면 그 멈춤이 무의미해진다
    // (6라운드 지적). 수집 자체는 인스턴스별 태스크가 하므로 이 루프를 자주 깨워도
    // 비용은 tick 게시뿐이다.
    let interval = tick_interval(config.collector.detect_interval_ms).min(Duration::from_millis(
        dbmon_core::ports::LEASE_RENEW_INTERVAL_MS as u64,
    ));
    let discovery_interval = Duration::from_secs(config.discovery.interval_secs);
    // **`Arc` 로 든다.** `select!` 팔에 참조를 넘기면 `implementation of Send is not
    // general enough` 로 컴파일이 깨진다 — 참조 인자에 대해 `for<'a>` Send 를
    // 증명해야 하기 때문이다.
    // 태스크 정리 예산. **셧다운 유예보다 작아야** 한다 — 크면 상위 단계가 먼저
    // 타임아웃해 리스가 반납되지 않는다(그 결함을 이미 한 번 만들었다).
    let drain_budget = Duration::from_secs((config.http.shutdown_grace_secs / 3).max(2));
    let config = Arc::new(config.clone());
    let collect_deps = CollectDeps {
        store: Arc::clone(&stores.slow_query),
        registry: Arc::clone(&stores.registry),
        hub,
        auth,
        config: Arc::clone(&config),
        worker_id: worker_id.clone(),
        shutdown: Arc::clone(&shutdown),
        freshness: dbmon::collect_loop::CollectFreshness::new(),
        // 리더가 되기 전에는 epoch 가 없다. 태스크를 띄우는 시점에 채운다.
        epoch: None,
    };

    // **`stores` 가 태스크로 이동하기 전에 뽑아 든다.** 리스 필드가 `LeaderGate` 로
    // 옮겨가면서 부분 이동이 일어나므로 여기서 `Arc` 를 복제하는 편이 읽기 쉽다.
    let pause_state = Arc::clone(&stores.pause);
    // 탐색 범위(리전·계정)의 출처. API 와 **같은 캐시**를 본다 — 화면에서 저장한
    // 것이 그대로 다음 라운드의 범위가 된다.
    let settings_state = Arc::clone(&stores.settings);

    Some(tokio::spawn(async move {
        let mut gate = LeaderGate::new(stores.lease, SystemClock, worker_id);
        // 탐색기는 리더가 될 때까지 만들지 않는다 — standby 워커가 AWS 자격증명을
        // 요구하면 로컬 개발(SSO 만료)에서 기동만으로 에러가 난다.
        //
        // **설정이 바뀌면 다시 만든다**(리전·계정 추가). 지문으로 판정하므로 같은
        // 범위에서는 재사용되고, 크로스 계정 `AssumeRole` 을 매 라운드 다시 하지 않는다.
        let mut fleet: Option<Arc<dbmon::aws::fleet::DiscoveryFleet>> = None;
        // **마지막 탐색 시각.** 0 이면 리더가 된 직후 한 번 돈다 — 5분을 기다리면
        // 배포 직후 목록이 비어 있고, 그건 장애로 보인다.
        let mut last_discovery_ms: i64 = 0;
        // 고아 스윕(F4)·백필은 탐색과 **다른 시간축**이다.
        let mut last_sweep_ms: i64 = 0;
        let mut last_backfill_ms: i64 = 0;
        // 인스턴스별 수집 태스크. **리더만 갖는다.**
        let mut tasks = CollectTasks::new();
        // 슬로우로그 소스는 리더가 될 때까지 만들지 않는다(탐색기와 같은 이유).
        let mut fetcher: Option<Arc<dyn dbmon::slowlog::SlowLogFetcher>> = None;
        // **마지막으로 본 정지 집합.** 바뀐 순간을 알아야 즉시 반영할 수 있다.
        let mut last_pause = dbmon_core::pause::PauseSet::default();
        // **정지 확정이 남은 인스턴스 → 그 인스턴스가 멈춘 시각.**
        //
        // 집합(bool)이면 재시도가 그 시점의 정지 집합을 다시 써서, 그 사이 재개된
        // 인스턴스가 대상에서 빠져 영구히 안 닫힌다(교차 리뷰 5회차). 상한을 **하나의
        // 스칼라**로 두면 다른 인스턴스가 멈출 때 창이 넓어져 재개된 인스턴스의 새
        // 레코드가 다시 그 안에 들어온다(6회차 수정의 반대 방향). 대상마다 자기 시각을
        // 들고 있어야 한다.
        let mut pending_pause_close: std::collections::BTreeMap<String, i64> =
            std::collections::BTreeMap::new();

        loop {
            // 셧다운 신호가 오면 수집을 멈추고 리스를 반납하고 나간다.
            if shutdown.is_triggered() {
                // **태스크를 먼저 정리한다.** 리스를 반납한 뒤에도 돌고 있으면
                // 다음 리더와 중복 수집이 된다.
                //
                // `abort_all` 이 아니라 `drain_all` 이다 — abort 하면 태스크가
                // `drain()` 에 도달하지 못해 진행 중 레코드가 `in_flight` 유령으로
                // 남는다(실제로 그 상태였다).
                tasks.drain_all(drain_budget).await;
                gate.release().await;
                readiness.set_collect_leader(false);
                return;
            }

            gate.refresh().await;
            // 취임 시각을 함께 넘긴다 — `collect_stale` 유예의 기준이다.
            readiness.set_collect_leader_at(gate.is_leader(), {
                use dbmon_core::time::Clock as _;
                dbmon_core::time::SystemClock.now_ms()
            });

            // **가장 오래된 인스턴스의 성공 시각**을 신선도로 올린다 (FR-OPS-09).
            // 한 대라도 밀리면 신선도가 밀린다.
            if let Some(oldest) = collect_deps.freshness.oldest() {
                readiness.record_collect_ok(oldest);
            }

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
                // **리더를 되찾으면 즉시 한 라운드 돈다.**
                //
                // `last_discovery_ms` 를 그대로 두면 다음 탐색 주기(기본 5분)까지
                // `reconcile` 이 불리지 않는다. 리스를 잃을 때 태스크를 전부 멈췄으므로
                // 그 사이 **수집 태스크가 0개인데 `/readyz` 는 리더라고 보고한다.**
                // 리스 다툼이 5분보다 잦으면 수집이 한 번도 돌지 않는다.
                last_discovery_ms = 0;
                // **standby 도 게시한다.** 화면이 "리더가 아니라서 안 돈다" 와
                // "멈춰 있다" 를 구분해야 한다.
                controls.publish_tick(SystemClock.now_ms(), false, 0);
            } else {
                let now_ms = SystemClock.now_ms();

                // ── 정지 스코프를 읽는다 ──────────────────────────────────────
                //
                // 저장소가 진실이므로 **어느 워커에서 눌렀든 여기 반영된다.** 5초
                // 캐시가 붙어 있어 tick 마다 왕복이 생기지는 않는다
                // (`control::PauseState`).
                let pause = pause_state.load(now_ms).await;
                // **바뀌면 즉시 한 라운드 돈다.** 그냥 두면 다음 탐색 주기(기본 5분)
                // 까지 반영되지 않는다 — 누르고 5분간 계속 수집되면 그 버튼은 고장난
                // 것으로 보인다.
                //
                // ponytail: 탐색 한 라운드를 통째로 돌리는 것은 필요보다 크다(RDS
                // Describe 까지 부른다). 토글은 사람 손이고 같은 경로가 이미 "인스턴스
                // 수집" 버튼으로 노출돼 있어 새 분기를 만들지 않았다. 자동 토글이
                // 생기면 태스크 재조정만 떼어낸다.
                // **실패가 남아 있으면 다음 tick 에 다시 시도한다.** 정지 변경에서만
                // 돌리면 일시적 실패가 유령 "실행 중" 으로 굳는다(교차 리뷰 4회차).
                let pause_changed = pause != last_pause;
                if pause_changed {
                    let scopes: Vec<&str> = pause.entries().map(|(k, _)| k).collect();
                    tracing::warn!(
                        ?scopes,
                        "정지 스코프가 바뀌었다 — 태스크 집합을 즉시 다시 맞춘다"
                    );
                    last_pause = pause.clone();
                    last_discovery_ms = 0;
                }

                // ── 전체가 멈춰 있다 ─────────────────────────────────────────
                //
                // **리스는 그대로 쥔다.** 반납하면 다른 워커가 즉시 리더가 되어
                // 수집을 계속하고, 그건 멈춘 것이 아니다. 태스크만 멈춘다.
                if pause.is_all_paused() {
                    if !tasks.running().is_empty() {
                        tracing::warn!(
                            stopped = tasks.running().len(),
                            "수집이 일시정지됐다 (화면 조작) — 이 구간은 기록되지 않는다"
                        );
                        // ① 짧게 드레인한다 — 끝난 쿼리는 정상 확정된다.
                        //
                        // **예산을 작게 쓴다.** 여기서 오래 막히면 그 동안
                        // `gate.refresh()` 가 안 돌아 리스를 잃는다.
                        tasks.drain_all(PAUSE_DRAIN_BUDGET).await;
                    }
                    // ② 정리 작업은 **전체가 하나의 예산 안에서** 돈다.
                    //
                    // 조회 20초 + 쓰기 20초 + 스윕 20초를 각각 재면 합이 리스 TTL(60초)
                    // 을 넘을 수 있다(5라운드 지적). 그러면 **멈추려다 리더를 잃고**,
                    // `paused` 플래그가 없는 다른 프로세스가 리더가 되어 수집을 이어간다 —
                    // 화면에는 "멈춤" 인데 기록은 계속 쌓인다. 하나로 묶으면 그 합이
                    // 예산을 넘지 않는다. 남은 일은 다음 tick 이 같은 목록을 다시 본다.
                    let sweep_due = now_ms - last_sweep_ms
                        >= (config.collector.orphan_sweep_secs as i64) * 1000;
                    // **스윕이 실제로 돌았을 때만 시각을 찍는다.** 앞의 확정 작업이
                    // 예산을 다 쓰면 스윕은 시작조차 못 하는데, 미리 찍어 두면
                    // "돌았다" 로 기록돼 다음 주기까지 건너뛴다 — 예산 초과가 반복되면
                    // **영구히 굶는다**(6라운드 지적).
                    let mut swept = false;
                    let maintenance = run_in_budget(async {
                        // 남은 진행 중 레코드를 확정한다. **전체 정지이므로 내 것 전부다.**
                        //
                        // 남은 집합을 버려도 된다 — 이 분기는 멈춰 있는 **매 tick** 돌므로
                        // 다음 tick 이 자동으로 재시도한다. 부분 정지는 정지 변경에서만
                        // 돌기 때문에 거기서는 집합을 들고 다닌다.
                        let _ = close_in_flight_mine(
                            &stores.slow_query,
                            gate.worker_id(),
                            gate.epoch(),
                            now_ms,
                            // 전체 정지는 내 것 전부다 — 새로 만들어질 레코드도 없다
                            // (모든 태스크를 내렸다).
                            None,
                        )
                        .await;

                        // **멈춰 있어도 고아 스윕은 돈다.**
                        //
                        // 이 분기는 루프 뒤쪽을 건너뛰므로 예전에는 스윕도 함께 멈췄다
                        // (4라운드 지적). 사람이 며칠 멈춰 두면 **다른 워커·이전 epoch 의
                        // 고아가 그동안 계속 "진행 중"** 으로 남는다 — F4 가 없애려던 상태다.
                        if sweep_due {
                            sweep_orphans(
                                &stores.slow_query,
                                &collect_deps.worker_id,
                                gate.epoch(),
                                now_ms,
                                &config,
                                // **전체 정지 상태다** — 도는 태스크가 없다. 그래서 내
                                // 이름·epoch 레코드도 걷어야 한다(그게 정지 확정이
                                // 놓친 것을 자동으로 정리한다).
                                &tasks.running_since(),
                            )
                            .await;
                            swept = true;
                        }
                    })
                    .await;
                    if swept {
                        last_sweep_ms = now_ms;
                    }
                    if maintenance.is_err() {
                        tracing::warn!(
                            "일시정지 정리가 예산을 넘겼다 — 리스를 지키려고 끊는다. 남은 일은 다음 tick 이 이어서 한다"
                        );
                    }

                    controls.publish_tick(now_ms, true, 0);
                    // 재개하면 즉시 한 라운드 돌게 한다.
                    last_discovery_ms = 0;
                    tokio::time::sleep(interval).await;
                    continue;
                }
                controls.publish_tick(now_ms, true, tasks.running().len());

                // 화면에서 "지금 훑어" 를 눌렀으면 주기를 기다리지 않는다.
                if controls.take_discovery_request() {
                    last_discovery_ms = 0;
                }
                if controls.take_backfill_request() {
                    last_backfill_ms = 0;
                }

                if now_ms - last_discovery_ms >= discovery_interval.as_millis() as i64 {
                    // **실행 전에 시각을 찍는다.** 실패해도 다음 주기까지 기다린다 —
                    // 실패 시 즉시 재시도하면 AWS 장애 중에 핫 루프가 된다.
                    last_discovery_ms = now_ms;
                    controls.publish_discovery(now_ms);
                    // **운영 설정을 읽어 범위를 정한다.** 30초 캐시라 매 라운드
                    // 저장소를 때리지 않는다. 읽지 못하면 마지막 값을 쓴다 —
                    // 기본값으로 접히면 타 리전·타 계정 인스턴스가 통째로
                    // "사라졌다" 로 판정된다.
                    let discovery_settings = settings_state.load(now_ms).await.discovery;
                    let fallback = config.target_regions();
                    if !fleet
                        .as_ref()
                        .is_some_and(|f| f.matches(&discovery_settings, &fallback))
                    {
                        fleet = Some(Arc::new(
                            build_discovery(&config, &discovery_settings).await,
                        ));
                    }
                    // 바로 위에서 채워졌다. 비어 있어도 `discover` 가 부분 결과로
                    // 처리하므로 등록부가 비워지지 않는다.
                    let srcs = fleet.clone().expect("탐색 대상은 위에서 만들어진다");

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

                        // ③ 수집 태스크 집합을 등록부에 맞춘다.
                        //
                        // 재조정 **뒤에** 한다 — 방금 `Excluded` 로 바뀐 인스턴스의
                        // 태스크를 같은 라운드에서 내려야 한다. 앞에 두면 필터에서
                        // 빠진 인스턴스를 5분 더 수집한다.
                        //
                        // **`if let Some(outcome)` 안이어야 한다.** 밖에 두면 셧다운으로
                        // 조회가 취소된 라운드에서도 등록부를 읽고 새 태스크를 띄운다 —
                        // 토큰을 발급하고 풀을 만들고 몇 ms 뒤 abort 된다.
                        match stores.registry.list().await {
                            Ok(instances) => {
                                // **현재 리스의 epoch 를 태스크에 심는다** (F4 펜싱).
                                // 저장 레코드의 `owner_epoch` 가 이 값이다.
                                let deps = CollectDeps {
                                    epoch: gate.epoch(),
                                    ..collect_deps.clone()
                                };
                                tasks.reconcile(&instances, &pause, &deps).await;
                                // 사라진 인스턴스를 신선도 맵에서 잊는다 — 안 잊으면
                                // 최솟값이 영구히 과거에 고정된다.
                                collect_deps.freshness.retain(&tasks.running());
                                let paused = dbmon::collect_loop::paused_ids(&instances, &pause);
                                tracing::info!(
                                    collecting = tasks.running().len(),
                                    registered = instances.len(),
                                    paused = paused.len(),
                                    "수집 태스크 집합 갱신"
                                );

                                // **부분 정지도 진행 중 레코드를 확정해야 한다.**
                                //
                                // 방금 태스크를 내린 인스턴스의 레코드를 남겨 두면 고아
                                // 스윕이 `Mine` 으로 보고 건너뛰어 유령 "실행 중" 이 남는다.
                                // 정지 집합이 바뀐 tick 에만 한 번 돈다 — 매 탐색마다
                                // 돌리면 멈춰 있는 동안 같은 조회를 5분마다 반복한다.
                                //
                                // ⚠ `abort()` 는 다음 await 지점에서 끊으므로, 확정과
                                // 태스크의 마지막 쓰기가 겹칠 좁은 틈이 있다. 전체 정지
                                // 경로도 같은 틈을 갖고 있고(먼저 abort 하고 확정한다),
                                // 그 경우 다음 리더 교체 때 epoch 가 달라져 스윕이 걷어간다.
                                // **대상은 "지금 멈춘 것 ∪ 아직 못 닫은 것" 이다.**
                                //
                                // 후자를 빼면, 확정에 실패한 인스턴스를 그 사이 재개했을
                                // 때 대상에서 사라져 **영구히 안 닫힌다** — 이미 끝난
                                // 실행은 다시 관측되지 않고, 같은 워커·epoch 레코드는
                                // 고아 스윕도 `Mine` 으로 건너뛴다(교차 리뷰 5회차).
                                //
                                // 상한은 **대상마다**이고, 규칙은 "지금 멈춰 있는가" 로
                                // 갈린다.
                                //
                                // - **지금 멈춰 있으면** 상한을 지금으로 올린다. 태스크가
                                //   내려가 있으므로 새 레코드가 만들어지지 않는다 —
                                //   올려도 위험이 없고, 재정지(A → 재개 → 다시 A 정지)
                                //   때 그 사이 만들어진 레코드를 닫을 수 있다.
                                // - **재개됐는데 아직 남아 있으면** 옛 상한을 유지한다.
                                //   지금 돌고 있는 태스크의 새 레코드를 지켜야 한다.
                                //
                                // `or_insert` 로 두면 후자만 맞고 전자가 틀린다 — 재정지
                                // 시 `[옛 상한, 재정지)` 구간 레코드를 아무도 닫지 않고
                                // 고아 스윕도 `Mine` 으로 건너뛴다(교차 리뷰 7회차가
                                // 배포 차단으로 잡았다). 반대로 항상 덮으면 재개된
                                // 인스턴스의 새 레코드를 닫는다(6회차).
                                // **상한은 태스크를 내린 뒤의 시각이다.**
                                //
                                // 루프 앞에서 읽은 `now_ms` 를 쓰면 그 사이(탐색·태스크
                                // 종료 중) 시작된 레코드가 상한 밖으로 나가고, 엣지
                                // 트리거였을 때는 아무도 그것을 닫지 않았다
                                // (교차 리뷰 8회차).
                                let close_now_ms = SystemClock.now_ms();
                                let mut close_targets = pending_pause_close.clone();
                                for id in &paused {
                                    close_targets.insert(id.clone(), close_now_ms);
                                }
                                // **레벨 트리거다.** 정지 집합이 바뀐 tick 에만 돌리면
                                // 그 tick 에서 등록부 조회가 실패하거나 상한 밖 레코드가
                                // 생겼을 때 **다시 시도할 계기가 없다** — 그 레코드는
                                // 영구히 진행 중으로 남고 고아 스윕도 같은 워커·epoch 를
                                // `Mine` 으로 건너뛴다(8회차가 배포 차단으로 잡았다).
                                //
                                // 멈춘 인스턴스가 있는 동안 탐색 라운드마다 한 번
                                // `list_in_flight` 를 부른다. 전체 정지 경로가 매 tick
                                // 같은 일을 하고 있으므로 새로운 비용이 아니다.
                                if !close_targets.is_empty() {
                                    let closed = run_in_budget(close_in_flight_mine(
                                        &stores.slow_query,
                                        gate.worker_id(),
                                        gate.epoch(),
                                        now_ms,
                                        Some(&close_targets),
                                    ))
                                    .await;
                                    pending_pause_close = match closed {
                                        Ok(remaining) => remaining,
                                        Err(_) => {
                                            tracing::warn!(
                                                "정지 인스턴스의 진행 중 레코드 확정이 예산을 넘겼다 — 대상을 유지하고 다음 tick 에 다시 시도한다"
                                            );
                                            close_targets
                                        }
                                    };
                                }
                            }
                            Err(e) => tracing::warn!(
                                error = %telemetry::Scrubbed(&e),
                                "등록부를 읽을 수 없다 — 태스크 집합을 유지한다"
                            ),
                        }
                    }
                }

                // ── 고아 in_flight 스윕 (F4) ─────────────────────────────────
                //
                // `drain()` 은 **정상 종료**만 덮는다. 급사·SIGKILL·리더 교체로
                // 사라진 워커의 레코드는 이 스윕만 덮는다 — 없으면 TTL(35일)까지
                // 화면에 유령 쿼리로 남는다.
                if now_ms - last_sweep_ms >= (config.collector.orphan_sweep_secs as i64) * 1000 {
                    last_sweep_ms = now_ms;
                    sweep_orphans(
                        &stores.slow_query,
                        &collect_deps.worker_id,
                        gate.epoch(),
                        now_ms,
                        &config,
                        &tasks.running_since(),
                    )
                    .await;
                }

                // ── 슬로우로그 백필 ──────────────────────────────────────────
                //
                // **실행별 정확 지표의 유일한 출처다** ([19 §G2]). 실시간 경로는
                // `rows_examined` 를 줄 수 없다 — 실행 중에는 0 이기 때문이다.
                if now_ms - last_backfill_ms >= (config.collector.backfill_secs as i64) * 1000 {
                    last_backfill_ms = now_ms;
                    controls.publish_backfill(now_ms);
                    if fetcher.is_none() {
                        fetcher = Some(build_slowlog_fetcher(&config).await);
                    }
                    if let (Some(f), Ok(Ok(instances))) = (
                        fetcher.as_ref(),
                        run_in_budget(stores.registry.list()).await,
                    ) {
                        // 백필도 예산 안에서 돈다 — 인스턴스 N개를 순차로 돌므로
                        // 예산이 없으면 리스 만료까지 갈 수 있다.
                        let s = match run_in_budget(backfill_round(
                            f,
                            &stores.slow_query,
                            &stores.checkpoint,
                            &instances,
                            &config,
                            now_ms,
                        ))
                        .await
                        {
                            Ok(s) => s,
                            Err(()) => {
                                tracing::warn!("슬로우로그 백필이 예산을 초과했다 — 중단한다");
                                Default::default()
                            }
                        };
                        // **`no_source` 만으로는 로그를 내지 않는다.** 슬로우로그가
                        // 꺼진 인스턴스가 있는 배포에서는 그 값이 매 주기 같으므로,
                        // 그것만 보고 줄을 쌓으면 로그가 그 사실로 도배된다.
                        if s.merged > 0 || s.errors > 0 || s.fetch_errors > 0 || s.incomplete > 0 {
                            tracing::info!(
                                merged = s.merged,
                                unnormalizable = s.unnormalizable,
                                masking_degraded = s.masking_degraded,
                                fetch_errors = s.fetch_errors,
                                no_source = s.no_source,
                                incomplete = s.incomplete,
                                errors = s.errors,
                                "슬로우로그 백필"
                            );
                        }
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.wait() => {
                    tasks.drain_all(drain_budget).await;
                    gate.release().await;
                    readiness.set_collect_leader(false);
                    return;
                }
            }
        }
    }))
}

/// 커서 서명 키를 만든다.
///
/// **`Math.random` 같은 약한 소스를 쓰지 않는다.** 위조 가능한 커서는 다른 환경의
/// 데이터를 가리킬 수 있다. OS 엔트로피를 쓴다.
///
/// 다중 워커가 커서를 공유하려면 공용 비밀이 필요하다(M6 의 남은 작업). 지금은
/// 프로세스마다 다르므로 다른 워커의 커서는 `invalid_cursor` 로 거부된다 —
/// 조용히 잘못된 페이지를 주는 것보다 낫다.
fn random_cursor_key() -> Vec<u8> {
    let mut key = vec![0u8; 32];
    match read_entropy(&mut key) {
        Ok(()) => key,
        Err(e) => {
            // 엔트로피를 못 읽으면 커서를 안전하게 만들 수 없다. 약한 키로
            // 계속하면 위조가 가능해지므로, 커서 기능을 쓸 수 없게 만든다.
            tracing::error!(error = %e, "엔트로피를 읽을 수 없다 — 커서 서명 키를 만들 수 없다");
            Vec::new()
        }
    }
}

fn read_entropy(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf))
}

/// 컨테이너에서 화면을 쓰기 위한 로컬 토큰을 발급하고 접속 URL 을 찍는다.
///
/// **우회가 아니라 자격증명이다.** 발급 조건은 [`dbmon::api::auth::mint_dev_token`]
/// 이 판정한다 — dev + 비루프백 + 비ECS 일 때만.
fn dev_access_token(config: &dbmon::config::Config, bind_is_loopback: bool) -> Option<Arc<str>> {
    // ECS 판정은 `config::on_ecs` 한 곳에만 둔다 — 두 곳에서 각자 환경변수를
    // 읽으면 판정이 갈릴 수 있고, 갈리면 어느 쪽이 맞는지 알 수 없다.
    let token = dbmon::api::auth::mint_dev_token(
        config.deployment_env,
        bind_is_loopback,
        dbmon::config::on_ecs(),
        || {
            let mut b = [0u8; 16];
            read_entropy(&mut b).ok().map(|()| b)
        },
    )?;

    // **토큰을 로그에 찍는다** — 이게 유일한 전달 경로다(`docker logs`).
    // dev + 비ECS 에서만 발급되므로 CloudWatch 로 새지 않는다.
    tracing::warn!(
        url = %format!("http://127.0.0.1:{}/?token={token}", config.http.port),
        "로컬 개발 접속 토큰을 발급했다 — 이 URL 로 화면에 들어간다 (dev 전용)"
    );
    Some(token)
}

/// 인증 정책을 만들고, **들어올 방법이 없으면 크게 경고한다.**
///
/// 경고가 필요한 이유: 자격증명이 하나도 없는 배포는 `auth.mode = off` 를 켜지 않는
/// 한 모든 요청이 401 이다. 그건 화면이 흰 채로 뜨는 것으로만 나타나고, 원인은
/// "설정 파일에 한 줄이 없다" 인데 응답은 `401` 뿐이라 아무것도 알려 주지 않는다.
///
/// 기동을 **막지는 않는다** — `role=collector` 워커는 API 를 쓰지 않고, 운영 설정으로
/// 인증을 끈 배포도 정상이다. 판정 근거(DynamoDB 설정)는 여기서 아직 읽지 않았다.
fn auth_policy(
    config: &dbmon::config::Config,
    bind_is_loopback: bool,
) -> dbmon::api::auth::AuthPolicy {
    let policy = dbmon::api::auth::AuthPolicy {
        deployment_env: config.deployment_env,
        bind_is_loopback,
        dev_token: dev_access_token(config, bind_is_loopback),
        shared_token: config.http.auth_token.as_deref().map(Arc::from),
    };
    if !policy.has_any_credential() {
        tracing::warn!(
            allow_auth_disable = config.http.allow_auth_disable,
            concat!(
                "인증 수단이 하나도 없다 — `http.auth_token` 을 넣거나 ",
                "운영 설정에서 인증을 끄지 않으면 모든 API 요청이 401 이 된다"
            )
        );
    }
    // **우회가 켜졌다는 사실을 크게 남긴다.**
    //
    // 우회는 `dev` ∧ 루프백 바인드에서 유도되는데, 그 판정은 **우리가 어디에 바인드
    // 했는지**만 본다. 같은 호스트의 리버스 프록시가 그 포트를 공개하면 외부에서
    // 인증 없이 들어온다 — 프록시의 peer 주소도 루프백이라 요청 출처를 봐도 구분되지
    // 않는다(교차 리뷰가 지적, 그래서 코드로 막을 수 없다). 남는 방어는 이 로그와
    // "dev 배포를 공개하지 않는다" 는 운영 규칙이다.
    if policy.allows_local_bypass() {
        tracing::warn!(
            bind = %config.http.bind,
            concat!(
                "인증 우회가 켜져 있다 (dev + 루프백) — 토큰 없이 admin 으로 들어온다. ",
                "이 포트를 리버스 프록시·터널로 공개하면 그대로 외부에 열린다"
            )
        );
    }
    policy
}

/// SPA 빌드 산출물 디렉터리. 없으면 `None`.
///
/// **런타임 이미지에 노드를 넣지 않는다.** `web/` 은 빌더 스테이지에서 빌드하고
/// `dist/` 만 이미지에 들어온다(`Dockerfile`). 그래서 여기서는 디렉터리가 있는지
/// 만 보고, 없으면 임베드 화면으로 떨어진다 — `npm run build` 를 안 돌린
/// `cargo run` 도 화면을 잃지 않아야 한다.
fn spa_dir() -> Option<PathBuf> {
    // 설정(`HttpConfig`)에 두지 않는 이유: 배포 산출물의 경로는 **이미지 레이아웃**
    // 사실이고 운영자가 튜닝할 값이 아니다. `DBMON_TARGET_PASSWORD`·`DBMON_LOG` 와
    // 같은 부류로 환경변수 하나에 둔다.
    let raw = std::env::var("DBMON_UI_DIR").unwrap_or_else(|_| "web/dist".to_string());
    let dir = PathBuf::from(raw);
    dir.join("index.html").is_file().then_some(dir)
}

/// 없는 API 경로. **SPA 폴백이 이걸 삼키면 안 된다.**
///
/// 폴백이 `index.html` 을 돌려주면 `GET /api/digests` 가 200 에 HTML 을 받고,
/// 그건 "아직 구현되지 않았다" 가 아니라 "응답이 이상하다" 로 나타난다.
async fn api_route_not_found() -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}

/// 임베드된 최소 화면. **SPA 산출물이 없을 때만** 쓰인다.
///
/// `npm run build` 를 돌리지 않은 개발 체크아웃에서 `cargo run` 만으로 API 가
/// 브라우저에서 동작하는지 확인하는 수단이다. SPA 는 `web/` 에 있고 개발 중에는
/// Vite 개발 서버(`npm run dev`, 5173)가 `/api` 를 여기로 프록시한다.
async fn index_page() -> axum::response::Response {
    use axum::http::header;
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../assets/index.html"),
    )
        .into_response()
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

    // **prd 격리를 끈 배포는 그 사실을 경고로 남긴다.**
    //
    // 이 플래그는 T-37 방어선 두 개(이름 거부·태그 거부)를 빼는 것이므로 `info` 로
    // 묻히면 안 된다. 시드 계정에서는 의도된 설정이지만, 남의 프로덕션이 섞인 계정에
    // 같은 파일이 복사되면 그때도 조용히 통과한다.
    if config.discovery.collect_production_targets {
        tracing::warn!(
            vpc_filter = ?config.discovery.allowed_vpc_ids,
            "prd 격리 게이트를 껐다 (collect_production_targets) — 이름·태그가 prd 인 \
             인스턴스도 수집한다. 환경은 태그로만 분류되고, 범위는 VPC 필터가 정한다"
        );
    }
    if !config.collector.target_endpoint_overrides.is_empty() {
        tracing::warn!(
            targets = ?config.collector.target_endpoint_overrides.keys().collect::<Vec<_>>(),
            "대상 엔드포인트를 오버라이드했다 (SSM 터널, dev 전용) — 이 구간은 평문이다"
        );
    }

    // **리터럴 정책을 기동 로그에 알린다** (FR-CAP-07 / OPEN-Q-15).
    //
    // 코드 기본값(`masked`)이 FR-CAP-07 의 `prd → full_restricted` 와 다르다.
    // 의도된 이탈이지만 **조용해서는 안 된다** — prd 에서 샘플 쿼리가 동작하지
    // 않는 이유가 여기 있고, 그걸 모르면 "기능이 깨졌다" 로 오해한다.
    {
        use dbmon_core::slow_query::LiteralPolicy;
        let policy = config.collector.literal_policy;
        if config.deployment_env == dbmon_core::env::Env::Prd && policy == LiteralPolicy::Masked {
            tracing::warn!(
                policy = ?policy,
                "prd 리터럴 정책이 masked 다 — FR-DGS-06(실행 가능한 샘플 쿼리)이 \
                 동작하지 않는다. FR-CAP-07 은 full_restricted 를 규정하지만 \
                 OPEN-Q-15(조직 규정 판단)가 미해소이므로 되돌릴 수 있는 쪽을 기본값으로 둔다"
            );
        } else {
            tracing::info!(policy = ?policy, env = %config.deployment_env, "리터럴 저장 정책");
        }
    }

    // **CA 번들 만료 감시** (07 §3.3).
    //
    // 이 판정 함수는 처음에 호출부가 없었다 — enum·함수·테스트를 다 만들고 아무도
    // 부르지 않았다(2차 리뷰가 지적, 이 부류 9번째). 기동 로그가 유일한 소비자다.
    {
        use dbmon::mysql::connect::{CaBundleHealth, ca_bundle_health};
        use dbmon_core::time::{Clock, SystemClock};
        match ca_bundle_health(SystemClock.now_ms()) {
            CaBundleHealth::Ok { days_left } => {
                tracing::info!(days_left, "RDS CA 번들 유효")
            }
            CaBundleHealth::Warn { days_left } => tracing::warn!(
                days_left,
                "RDS CA 번들 만료가 90일 미만이다 — 번들을 갱신한다"
            ),
            CaBundleHealth::Critical { days_left } => tracing::error!(
                days_left,
                "RDS CA 번들 만료가 30일 미만이다 — 즉시 갱신한다"
            ),
            CaBundleHealth::Expired => {
                tracing::error!("RDS CA 번들이 만료됐다 — 대상 TLS 접속이 실패한다")
            }
        }
    }

    // ── HTTP 서버 ────────────────────────────────────────────────────────────
    // `/healthz` 와 `/readyz` 는 **역할과 무관하게** 항상 띄운다.
    // collector 전용 워커도 컨테이너 헬스체크를 받아야 한다.
    // ── 저장소와 리더 게이트 ─────────────────────────────────────────────────
    //
    // **저장소에 붙지 못해도 기동은 된다.** `/readyz` 가 `storage_unavailable` 을
    // 정확히 보고하는 것이 맞다 — 기동 실패로 재시작 루프를 만들면 원인을 볼 수 없다.
    let mut api_state: Option<dbmon::api::ApiState> = None;
    // 방송 허브를 먼저 만든다 — 저장소가 이걸 물고 조립된다.
    let hub = dbmon::api::hub::Hub::new();
    let leader_task = match build_stores(&config, &hub).await {
        Ok(stores) => {
            // **실제로 닿는지 확인한다.** 클라이언트 조립 성공만으로 준비 완료를
            // 보고하면 자격증명·테이블 이름이 틀려도 `/readyz` 가 초록이다.
            match stores.slow_query.inner().probe().await {
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

            // 화면 조작과 리더 루프가 공유하는 제어 플래그.
            let controls = Arc::new(dbmon::control::Controls::new());

            // 조회 API 상태. **역할과 무관하게 만든다** — `role=collector` 도 저장소를
            // 갖고 있고, 로컬 개발에서는 한 프로세스가 둘을 겸한다.
            let bind_is_loopback = dbmon::api::auth::is_loopback_bind(&config.http.bind);
            api_state = Some(dbmon::api::ApiState {
                store: Arc::clone(&stores.slow_query),
                registry: Arc::clone(&stores.registry),
                policy: auth_policy(&config, bind_is_loopback),
                // 프로세스마다 다른 키. 재시작하면 기존 커서가 무효해지고, 그게
                // 조용한 오동작이 아니라 명시적 `invalid_cursor` 로 나타난다.
                cursor_key: Arc::new(random_cursor_key()),
                hub: hub.clone(),
                // 화면 머리말이 "지금 어느 계정·리전을 보고 있나" 를 말한다. 참조
                // 대시보드도 이걸 상단에 띄웠다 — 계정을 착각한 채로 조사하는 것이
                // 이런 도구에서 가장 비싼 실수다.
                aws_account_id: config.aws.account_id.clone(),
                aws_region: config.aws.region.clone(),
                discovery_fallback_regions: config.target_regions(),
                controls: Arc::clone(&controls),
                pause: Arc::clone(&stores.pause),
                settings: Arc::clone(&stores.settings),
                // **API 워커도 대상 DB 에 붙는다** — 튜닝 컨텍스트(테이블 명세)를
                // 사람이 누를 때 모으기 때문이다. 자격증명이 없으면 `None` 이 되고
                // 화면이 사유를 표시한다(조용히 빈 권고를 만들지 않는다).
                tuning: Some(Arc::new(dbmon::tuning::TuningService {
                    registry: Arc::clone(&stores.registry),
                    advice: Arc::clone(&stores.advice),
                    settings: Arc::clone(&stores.settings),
                    auth: Arc::new(dbmon::aws::auth_token::build_target_auth(&config).await),
                    config: Arc::new(config.clone()),
                })),
                // **파일 설정만이 인증 끄기를 허용할 수 있다.** 화면에서 두 번째
                // 허용을 눌러야 실제로 꺼진다.
                allow_auth_disable: config.http.allow_auth_disable,
                metrics: build_metrics(&config),
                worker_id: worker_id.clone(),
                // 이 프로세스가 수집 루프를 도는가. `role=api` 워커는 제어를 받지
                // 않는다 — 플래그가 프로세스 원자값이라 수집 워커가 모른다.
                runs_collector: config.role.runs_collector(),
            });

            // **설정 폴러.** 이게 없으면 인증 판정이 굶는다.
            //
            // 인증은 신선한 설정만 믿는다(`cached_fresh`, TTL 30초) — 오래된 `off` 를
            // 믿으면 인증을 다시 켜도 전파되지 않기 때문이다. 그런데 요청 경로는 캐시만
            // 보고, 탐색 루프는 5분에 한 번이며 `role=api` 워커에는 그 루프가 아예 없다.
            // 그래서 갱신하는 것이 아무도 없으면 **`auth.mode = off` 배포가 30초 뒤
            // 잠긴다**(2차 교차 리뷰가 high 로 잡았다).
            //
            // TTL 의 1/3 주기로 돌려 항상 신선하게 유지한다. 조회가 실패하면 캐시가
            // 낡고, 그때는 인증이 켜진 쪽으로 떨어진다 — 그게 옳은 방향이다.
            spawn_settings_poller(Arc::clone(&stores.settings), Arc::clone(&shutdown));

            // 인증 공급자를 먼저 만든다 — 리전별 SDK 설정 로드는 await 가 필요하다.
            let auth = dbmon::aws::auth_token::build_target_auth(&config).await;
            spawn_leader_loop(
                &config,
                worker_id,
                stores,
                auth,
                LoopWiring {
                    readiness: readiness.clone(),
                    shutdown: shutdown.clone(),
                    hub: hub.clone(),
                    controls: Arc::clone(&controls),
                },
            )
        }
        Err(e) => {
            // 여기서 죽지 않는다. `/readyz` 가 사유를 보고하고 사람이 고친다.
            tracing::error!(error = %telemetry::Scrubbed(&e), "저장소 연결 실패 — standby 로 기동한다");
            None
        }
    };

    // `/healthz`·`/readyz` 만 인증 예외다 (T-01). 나머지는 API 라우터가 인증한다.
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(readiness.clone());

    // 조회 API 와 화면은 **저장소가 조립된 뒤에만** 붙는다 — 저장소 없이 라우트를
    // 열면 500 을 돌려주는 엔드포인트가 생기고, 그건 "데이터가 없다" 로 오해된다.
    if let Some(api_state) = api_state {
        // **화면은 배포에서도 서빙한다.**
        //
        // 전에는 로컬 개발(`serves_local_ui`)에서만 붙였다. 근거는 "prd 에서는 인증을
        // 통과할 수 없으니 깨진 화면이다" 였는데, 공유 토큰(`http.auth_token`)이
        // 배선되면서 그 전제가 사라졌다. 게이트를 그대로 두면 ECS 배포에서 `/` 가
        // **404** 다 — README 가 "브라우저로 들어와 설정을 마친다" 를 안내하는데
        // 들어갈 화면이 없는 상태였다(교차 리뷰가 잡았다).
        //
        // 정적 자산은 비밀을 담지 않는다. 데이터는 전부 `/api` 뒤에 있고 그건 인증을
        // 탄다. 자격증명이 없는 배포에서도 서빙하는 편이 낫다 — 화면이
        // `mode=unconfigured` 를 받아 "배포 설정에 토큰을 넣어라" 를 말한다. 404 는
        // 아무것도 말해 주지 않는다.
        //
        // `DBMON_UI_DIR` 이 없으면(`spa_dir()` → `None`) 붙지 않는다. API 전용 배포는
        // 그 환경변수를 두지 않으면 된다.
        app = app.merge(dbmon::api::router(api_state));
        let spa = spa_dir();
        {
            match &spa {
                Some(dir) => {
                    use tower_http::services::{ServeDir, ServeFile};
                    use tower_http::set_header::SetResponseHeaderLayer;

                    // 해시가 붙은 산출물. 이름이 내용으로 정해지므로 영구 캐시가 안전하다.
                    //
                    // ⚠ **성공 응답에만 붙인다.** 404 에 `immutable` 을 붙이면, 배포
                    // 중에 새 자산을 먼저 요청한 클라이언트가 그 404 를 1년간 캐시해
                    // **영구히 깨진 화면**을 갖는다.
                    app = app.nest_service(
                        "/assets",
                        axum::routing::any_service(ServeDir::new(dir.join("assets"))).layer(
                            SetResponseHeaderLayer::overriding(
                                axum::http::header::CACHE_CONTROL,
                                |res: &axum::response::Response| {
                                    res.status().is_success().then(|| {
                                        axum::http::HeaderValue::from_static(
                                            "public, max-age=31536000, immutable",
                                        )
                                    })
                                },
                            ),
                        ),
                    );

                    // 클라이언트 라우팅(`/slow-queries/…`)이 새로고침을 견뎌야 하므로
                    // 나머지 경로는 `index.html` 이 받는다. 단 `/api/…` 는 위의
                    // 명시 라우트에 걸리지 않았다면 **404 로 답한다.**
                    //
                    // ⚠ **셸은 캐시하지 않는다.** `index.html` 은 해시가 붙은 자산
                    // 이름을 담으므로, 낡은 셸이 캐시되면 배포 뒤 **삭제된 파일을
                    // 가리켜 빈 화면**이 된다. `ServeFile` 은 `Last-Modified` 만
                    // 주므로 브라우저가 휴리스틱으로 캐시할 수 있다.
                    let shell = axum::routing::any_service(ServeFile::new(dir.join("index.html")))
                        .layer(SetResponseHeaderLayer::overriding(
                            axum::http::header::CACHE_CONTROL,
                            axum::http::HeaderValue::from_static("no-cache"),
                        ));
                    // `/api`·`/api/` 를 따로 적는 이유: `{*rest}` 는 **빈 세그먼트를
                    // 잡지 않는다.** 실측에서 `/api/` 가 HTML 200 을 돌려줬다.
                    app = app
                        .route("/api", axum::routing::any(api_route_not_found))
                        .route("/api/", axum::routing::any(api_route_not_found))
                        .route("/api/{*rest}", axum::routing::any(api_route_not_found))
                        .fallback_service(shell);
                }
                None => app = app.route("/", get(index_page)),
            }
        }
        tracing::info!(
            bind_is_loopback = %dbmon::api::auth::is_loopback_bind(&config.http.bind),
            spa = %spa.as_ref().map_or_else(|| "(임베드 화면)".to_string(), |d| d.display().to_string()),
            "조회 API 를 서비스한다"
        );
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// **정지 확정이 재개 후의 새 레코드를 닫지 않는다.**
    ///
    /// 부분 정지 확정이 실패하면 대상을 들고 다음 라운드에 다시 시도한다. 그 사이 그
    /// 인스턴스를 재개하면 새 수집 태스크가 **새 레코드**를 만드는데, 대상이 인스턴스 id
    /// 뿐이면 그 새 레코드까지 `abandoned` 로 닫는다 — 병합에서 `Abandoned` 는 나중의
    /// `InFlight` 를 이기므로 되돌릴 수 없다(교차 리뷰 6회차).
    ///
    /// 시작 시각 상한이 그걸 막는다. 상한 판정만 순수하게 확인한다 — 저장소 왕복은
    /// `it_store` 가 덮는다.
    #[test]
    fn the_pause_cutoff_protects_records_started_after_resume() {
        const PAUSED_AT: i64 = 1_787_000_000_000;
        // 상한 이전에 시작된 것 → 닫는다.
        assert!(!skips_for_cutoff(PAUSED_AT - 1, Some(PAUSED_AT)));
        // 상한과 같거나 이후 → 건드리지 않는다.
        assert!(skips_for_cutoff(PAUSED_AT, Some(PAUSED_AT)));
        assert!(skips_for_cutoff(PAUSED_AT + 1, Some(PAUSED_AT)));
        // 상한이 없으면(전체 정지) 전부 닫는다.
        assert!(!skips_for_cutoff(PAUSED_AT + 1, None));
    }

    /// **두 번째 정지가 첫 대상의 상한을 넓히지 않는다.**
    ///
    /// 상한을 하나의 스칼라로 두면 A 가 남아 있는 동안 B 가 멈출 때 상한이 B 의 시각으로
    /// 넓어지고, 그 사이 재개된 A 의 새 레코드가 다시 그 안에 들어온다 — 6회차 수정이
    /// 막은 것과 **같은 결함이 한 라운드 뒤에** 재발한다.
    #[test]
    fn a_later_pause_does_not_widen_an_earlier_targets_cutoff() {
        use std::collections::BTreeMap;
        const T1: i64 = 1_787_000_000_000; // A 가 멈춘 시각
        const T4: i64 = T1 + 600_000; // B 가 멈춘 시각

        // 루프의 대상 계산과 **같은 식**이다.
        let mut pending: BTreeMap<String, i64> = BTreeMap::new();
        pending.insert("A".into(), T1); // A 확정 실패로 남았다

        let paused = ["B".to_string()]; // 지금은 B 만 멈춰 있다(A 는 재개됐다)
        let mut targets = pending.clone();
        for id in &paused {
            targets.entry(id.clone()).or_insert(T4);
        }

        assert_eq!(targets.get("A"), Some(&T1), "A 의 상한이 넓어졌다");
        assert_eq!(targets.get("B"), Some(&T4));

        // **그런데 A 를 다시 멈추면 상한이 올라가야 한다.**
        //
        // 안 올리면 `[T1, 재정지)` 구간에 만들어진 레코드를 아무도 닫지 않는다 — 고아
        // 스윕도 같은 워커·epoch 를 `Mine` 으로 건너뛴다(교차 리뷰 7회차).
        let repaused = ["A".to_string(), "B".to_string()];
        let t5 = T4 + 600_000;
        let mut targets2 = pending.clone();
        for id in &repaused {
            targets2.insert(id.clone(), t5);
        }
        assert_eq!(
            targets2.get("A"),
            Some(&t5),
            "재정지에서 상한이 올라가지 않았다"
        );
        // 그 사이(T1 이후, t5 이전)에 만들어진 레코드를 닫는다.
        assert!(!skips_for_cutoff(T1 + 60_000, targets2.get("A").copied()));

        // A 를 재개한 뒤(T2) 만들어진 레코드(T3)는 A 의 상한 밖이다.
        let t3 = T1 + 60_000;
        assert!(
            skips_for_cutoff(t3, targets.get("A").copied()),
            "새 레코드를 닫는다"
        );
        // A 가 멈추기 전에 시작된 레코드는 닫는다.
        assert!(!skips_for_cutoff(T1 - 1, targets.get("A").copied()));
    }

    /// [`close_in_flight_mine`] 의 상한 판정과 **같은 식**이다.
    fn skips_for_cutoff(started_at_ms: i64, started_before_ms: Option<i64>) -> bool {
        started_before_ms.is_some_and(|cut| started_at_ms >= cut)
    }

    /// **예산을 넘긴 태스크가 실제로 멈춰야 한다.**
    ///
    /// `timeout_at(deadline, h)` 로 쓰면 만료 시 `JoinHandle` 이 드롭되고, 드롭은
    /// **취소가 아니라 분리**다 — 태스크는 계속 돈다. 일시정지를 눌렀는데 수집이
    /// 계속되고(상태는 `collecting: 0`), 재개하면 같은 인스턴스에 태스크가 둘
    /// 생긴다. 눈으로는 보이지 않는 종류의 결함이므로 카운터로 확인한다.
    #[tokio::test]
    async fn draining_past_the_budget_actually_stops_the_task() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let mut tasks = CollectTasks::new();
        let counter = Arc::clone(&ticks);
        tasks.handles.insert(
            "never-ends".to_string(),
            tokio::spawn(async move {
                loop {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }),
        );

        tasks.drain_all(Duration::from_millis(30)).await;
        assert!(tasks.running().is_empty(), "정리 후 목록은 비어야 한다");

        // 정리 직후 값을 재고, 태스크가 살아 있다면 늘어날 만큼 기다린다.
        let after_drain = ticks.load(Ordering::SeqCst);
        assert!(
            after_drain > 0,
            "태스크가 한 번은 돌았어야 테스트가 성립한다"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            after_drain,
            "예산을 넘긴 태스크가 계속 돌고 있다 — abort 되지 않았다"
        );
    }
}
