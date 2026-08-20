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
        registry: Arc::new(dbmon::store::registry::DynamoInstanceRegistry::new(
            client.clone(),
            config.storage.data_table.clone(),
        )),
        checkpoint: Arc::new(dbmon::store::checkpoint::DynamoCheckpointStore::new(
            client,
            config.storage.data_table.clone(),
        )),
    })
}

/// 조립된 저장소들. 인자 5개를 넘기는 대신 묶는다.
struct Stores {
    slow_query: Arc<dbmon::store::AppSlowQueryStore>,
    lease: Arc<dbmon::store::lease::DynamoLeaseStore>,
    registry: Arc<dbmon::store::registry::DynamoInstanceRegistry>,
    /// 백필 재개 지점. **없으면 중단 구간이 영구히 빈다.**
    checkpoint: Arc<dbmon::store::checkpoint::DynamoCheckpointStore>,
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

/// 한 스윕에서 볼 진행 중 레코드 상한. 폭주 방어.
const ORPHAN_SWEEP_LIMIT: usize = 500;

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
        let (since_ms, skipped_ms) =
            resume_from(checkpoint, now_ms, initial_lookback_ms, max_lookback_ms);
        if let Some(gap_ms) = skipped_ms {
            // **조용히 건너뛰지 않는다.** 그 구간은 정확 지표가 영구히 없다.
            tracing::warn!(
                instance = %instance.id.as_str(),
                gap_ms,
                "백필 재개 지점이 너무 오래됐다 — 구간을 건너뛴다 (그만큼 정확 지표가 없다)"
            );
        }
        let chunk = match fetcher.fetch(&instance.id, since_ms).await {
            Ok(c) => c,
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
        let mut parsed = dbmon::slowlog::parse(&chunk.text, min_ms);
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
                    let position = chunk.next_since_ms.or_else(|| {
                        parsed
                            .entries
                            .iter()
                            .map(|e| e.ended_at_ms)
                            .max()
                            .map(|t| t + 1)
                    });
                    if let Some(pos) = position
                        && let Err(e) = checkpoints.put(&job, pos).await
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

/// 대상 접속 비밀을 발급하는 주체를 **리전별로** 만든다.
///
/// # 왜 리전별인가
///
/// IAM DB Auth 토큰은 **대상 인스턴스의 리전**으로 서명해야 한다([07 §3.1]).
/// 배포 리전으로 서명하면 `Access denied` 가 되고, 원인이 IAM 정책처럼 보여 추적이
/// 오래 걸린다. `target_regions` 가 여러 개면 공급자도 여러 개다.
///
/// 처음에는 `config.aws.region` 하나로 만들었다 — 크로스 리전 대상이 전부 거부되는
/// 배선이었다.
///
/// # dev 폴백
///
/// **`dev` + 환경변수가 있을 때만 고정 비밀번호를 쓴다.** 두 조건이 모두 필요하다 —
/// 환경변수만 보면 prd 태스크에 그 변수가 새어 들어갔을 때 IAM 대신 비밀번호로
/// 붙으려 하고, 실패 원인이 "인증 실패" 로만 보인다.
async fn build_auth(config: &Config) -> TargetAuth {
    use aws_config::BehaviorVersion;
    use dbmon::aws::auth_token::{IamAuthTokenProvider, StaticPasswordProvider};

    if config.deployment_env == dbmon_core::env::Env::Dev {
        if let Some(p) = StaticPasswordProvider::from_env() {
            tracing::info!("대상 인증: 고정 비밀번호 (dev 폴백)");
            return TargetAuth::Shared(Arc::new(p));
        }
    }

    let mut by_region: std::collections::BTreeMap<String, Arc<dyn AuthTokenProvider>> =
        std::collections::BTreeMap::new();
    for region in config.target_regions() {
        // **저장소용 SDK 설정을 재사용하지 않는다.** 로컬 개발 경로에서 그쪽은
        // 더미 자격증명(`local`/`local`)을 들고 있어 토큰이 조용히 무효해진다.
        let sdk = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new(region.clone()))
            .load()
            .await;
        match sdk.credentials_provider() {
            Some(creds) => {
                by_region.insert(
                    region.clone(),
                    Arc::new(IamAuthTokenProvider::new(creds.clone(), region.clone())),
                );
            }
            // 여기서 패닉하지 않는다 — 기동은 되고 `/readyz` 와 로그가 사유를 보고한다.
            None => tracing::error!(
                %region,
                "자격증명 공급자가 없다 — 이 리전의 대상에 접속할 수 없다"
            ),
        }
    }
    tracing::info!(regions = ?by_region.keys().collect::<Vec<_>>(), "대상 인증: IAM DB Auth");
    TargetAuth::PerRegion(by_region)
}

/// 대상 인증 공급자 묶음.
#[derive(Clone)]
enum TargetAuth {
    /// dev 폴백 — 리전과 무관하다.
    Shared(Arc<dyn AuthTokenProvider>),
    /// IAM DB Auth — **리전마다 다른 공급자.**
    PerRegion(std::collections::BTreeMap<String, Arc<dyn AuthTokenProvider>>),
}

impl TargetAuth {
    /// 이 인스턴스의 리전에 맞는 공급자.
    ///
    /// 없으면 `None` 이다 — **다른 리전 공급자로 대신하지 않는다.** 그러면 서명이
    /// 틀린 토큰으로 접속을 시도하고 실패 원인이 IAM 정책처럼 보인다.
    fn for_region(&self, region: &str) -> Option<Arc<dyn AuthTokenProvider>> {
        match self {
            Self::Shared(p) => Some(Arc::clone(p)),
            Self::PerRegion(m) => m.get(region).cloned(),
        }
    }
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
        for (id, h) in handles {
            match tokio::time::timeout_at(deadline, h).await {
                Ok(_) => {}
                Err(_) => {
                    // `timeout_at` 이 만료되면 future 를 드롭한다 — 태스크는 계속
                    // 돌므로 명시적으로 abort 해야 한다.
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
    /// 실시간 지표 방송. 슬로우 쿼리는 저장소 래퍼가 방송하지만 지표는
    /// 저장하지 않으므로(휘발성) 여기서 직접 넣는다.
    hub: dbmon::api::hub::Hub,
    auth: TargetAuth,
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

        // **엔드포인트를 먼저 확인한다.** 없으면 토큰을 요청하지 않는다 —
        // 빈 호스트로 서명하면 STS 왕복만 낭비하고, 이어지는 오류가
        // "연결 옵션 구성 실패" 로만 나와 실제 원인(엔드포인트 없음)을 가린다.
        let Some(host) = instance.endpoint.clone() else {
            tracing::warn!(
                instance = %label,
                "엔드포인트가 없다 — 수집하지 않는다 (생성 중이거나 정지 상태다)"
            );
            return;
        };

        // **인스턴스의 리전으로 서명하는 공급자를 고른다.** 없으면 접속하지 않는다 —
        // 다른 리전 공급자로 대신하면 서명이 틀린 토큰으로 붙으려 하고, 실패 원인이
        // IAM 정책처럼 보여 추적이 오래 걸린다.
        let Some(auth) = deps.auth.for_region(instance.id.region()) else {
            tracing::error!(
                instance = %label,
                region = %instance.id.region(),
                "이 리전의 인증 공급자가 없다 — 수집하지 않는다"
            );
            return;
        };

        // 첫 연결. 실패하면 잠시 뒤 재시도한다 — 태스크를 끝내면 이 인스턴스는
        // 다음 탐색(5분)까지 수집되지 않는다.
        let mut secret = match auth.token(&host, instance.port, &db_user).await {
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
                Err(e) => tracing::warn!(
                    instance = %label,
                    error = %telemetry::Scrubbed(&e),
                    "수집 tick 실패"
                ),
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
    auth: TargetAuth,
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

    let interval = tick_interval(config.collector.detect_interval_ms);
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
        hub,
        auth,
        config: Arc::clone(&config),
        worker_id: worker_id.clone(),
        shutdown: Arc::clone(&shutdown),
        freshness: dbmon::collect_loop::CollectFreshness::new(),
        // 리더가 되기 전에는 epoch 가 없다. 태스크를 띄우는 시점에 채운다.
        epoch: None,
    };

    Some(tokio::spawn(async move {
        let mut gate = LeaderGate::new(stores.lease, SystemClock, worker_id);
        // 탐색기는 리더가 될 때까지 만들지 않는다 — standby 워커가 AWS 자격증명을
        // 요구하면 로컬 개발(SSO 만료)에서 기동만으로 에러가 난다.
        let mut sources: Option<Arc<Vec<dbmon::aws::rds::RdsDiscovery>>> = None;
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
            readiness.set_collect_leader(gate.is_leader());

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

                // ── 화면에서 멈춤을 눌렀다 ────────────────────────────────────
                //
                // **리스는 그대로 쥔다.** 반납하면 다른 워커가 즉시 리더가 되어
                // 수집을 계속하고, 그건 멈춘 것이 아니다. 태스크만 멈춘다.
                if controls.is_paused() {
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
                    // ② 그래도 남은 **진행 중 레코드를 사유와 함께 확정한다.**
                    //
                    // 남겨 두면 그 레코드는 이 워커의 epoch 소유이므로 **고아 스윕이
                    // `Mine` 으로 보고 매번 건너뛴다** — 리더가 바뀌거나 TTL 이 지날
                    // 때까지 영구히 "진행 중" 이다(F4 가 막으려던 유령 상태).
                    // `collector_paused` 로 적어 두면 화면이 "사람이 멈췄다" 를 말한다.
                    use dbmon_core::ports::SlowQueryStore as _;
                    if let Ok(Ok(in_flight)) =
                        run_in_budget(stores.slow_query.list_in_flight(200)).await
                    {
                        let mut closed = 0usize;
                        for q in &in_flight {
                            if q.owner_worker.as_deref() != Some(gate.worker_id())
                                || q.owner_epoch != gate.epoch()
                            {
                                continue;
                            }
                            let marked = dbmon::orphan::abandon_with(q, now_ms, "collector_paused");
                            if stores.slow_query.upsert_merged(&marked).await.is_ok() {
                                closed += 1;
                            }
                        }
                        if closed > 0 {
                            tracing::warn!(
                                closed,
                                "일시정지로 진행 중 레코드를 추적 끊김으로 확정했다 (collector_paused)"
                            );
                        }
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
                                tasks.reconcile(&instances, &deps).await;
                                // 사라진 인스턴스를 신선도 맵에서 잊는다 — 안 잊으면
                                // 최솟값이 영구히 과거에 고정된다.
                                collect_deps.freshness.retain(&tasks.running());
                                tracing::info!(
                                    collecting = tasks.running().len(),
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

                // ── 고아 in_flight 스윕 (F4) ─────────────────────────────────
                //
                // `drain()` 은 **정상 종료**만 덮는다. 급사·SIGKILL·리더 교체로
                // 사라진 워커의 레코드는 이 스윕만 덮는다 — 없으면 TTL(35일)까지
                // 화면에 유령 쿼리로 남는다.
                if now_ms - last_sweep_ms >= (config.collector.orphan_sweep_secs as i64) * 1000 {
                    last_sweep_ms = now_ms;
                    let threshold =
                        dbmon::orphan::stale_threshold_ms(config.collector.detect_interval_ms);
                    // **예산 안에서 돈다.** 리더 루프와 같은 태스크이므로 길어지면
                    // `gate.refresh()` 가 불리지 않아 리스가 만료되고, 그 사이
                    // 수집 태스크는 계속 돌아 두 리더가 같은 인스턴스를 수집한다.
                    // `work_budget()` 의 주석이 설명하는 그 불변식이다.
                    match run_in_budget(dbmon::orphan::sweep(
                        Arc::clone(&stores.slow_query),
                        &collect_deps.worker_id,
                        // **현재 리스 epoch 를 넘긴다.** 이름만 보면 재시작한 자기
                        // 유령을 영구히 건너뛴다.
                        gate.epoch(),
                        now_ms,
                        threshold,
                        ORPHAN_SWEEP_LIMIT,
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
                        if s.merged > 0 || s.errors > 0 || s.fetch_errors > 0 || s.incomplete > 0 {
                            tracing::info!(
                                merged = s.merged,
                                unnormalizable = s.unnormalizable,
                                masking_degraded = s.masking_degraded,
                                fetch_errors = s.fetch_errors,
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
                policy: dbmon::api::auth::AuthPolicy {
                    deployment_env: config.deployment_env,
                    bind_is_loopback,
                    dev_token: dev_access_token(&config, bind_is_loopback),
                },
                // 프로세스마다 다른 키. 재시작하면 기존 커서가 무효해지고, 그게
                // 조용한 오동작이 아니라 명시적 `invalid_cursor` 로 나타난다.
                cursor_key: Arc::new(random_cursor_key()),
                hub: hub.clone(),
                // 화면 머리말이 "지금 어느 계정·리전을 보고 있나" 를 말한다. 참조
                // 대시보드도 이걸 상단에 띄웠다 — 계정을 착각한 채로 조사하는 것이
                // 이런 도구에서 가장 비싼 실수다.
                aws_account_id: config.aws.account_id.clone(),
                aws_region: config.aws.region.clone(),
                controls: Arc::clone(&controls),
                worker_id: worker_id.clone(),
                // 이 프로세스가 수집 루프를 도는가. `role=api` 워커는 제어를 받지
                // 않는다 — 플래그가 프로세스 원자값이라 수집 워커가 모른다.
                runs_collector: config.role.runs_collector(),
            });

            // 인증 공급자를 먼저 만든다 — 리전별 SDK 설정 로드는 await 가 필요하다.
            let auth = build_auth(&config).await;
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
        // **임베드 화면은 로컬 개발에서만 서빙한다.**
        //
        // 정적 HTML 이라 데이터가 새지는 않지만, 서빙하면 T-01 의 인증 예외가
        // `/healthz`·`/readyz`·`/api/auth/config` 에서 하나 더 늘어난다. 게다가
        // prd 에서는 인증을 통과할 수 없으니 **깨진 화면**이다 — 표를 못 채운다.
        // 실제 SPA(M0-12)가 오면 그때 인증을 태워 붙인다.
        let serve_ui = api_state.policy.serves_local_ui();
        app = app.merge(dbmon::api::router(api_state));
        let spa = if serve_ui { spa_dir() } else { None };
        if serve_ui {
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
            serve_ui,
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
