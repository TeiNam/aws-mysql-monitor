//! 그레이스풀 셧다운 (M0-8, M4-23).
//!
//! # ECS 로 옮기면서 단순해졌다
//!
//! 초기 설계는 EC2 ASG + systemd 였고 그래서 `sd_notify(READY=1)`·`WatchdogSec`·
//! ASG Lifecycle Hook(`Terminating:Wait`)·`SetInstanceHealth` 가 필요했다.
//! **ECS 에서는 이 네 가지가 전부 불필요하다**:
//!
//! | EC2 + systemd | ECS |
//! |---|---|
//! | `sd_notify(READY=1)` | 태스크 정의의 `healthCheck` 가 `/healthz` 를 본다 |
//! | `WatchdogSec` | 컨테이너 헬스체크 실패 → ECS 가 교체 |
//! | Lifecycle Hook `Terminating:Wait` | ALB 등록 해제 → `SIGTERM` → `stopTimeout` 대기 |
//! | `SetInstanceHealth` | 프로세스가 죽으면 태스크가 죽고 ECS 가 교체 |
//!
//! 우리가 할 일은 **`SIGTERM` 을 받아 순서대로 정리하고 `stopTimeout` 안에 끝내는 것**뿐이다.
//!
//! # 정리 순서가 중요하다
//!
//! ```text
//! 1. /readyz → 503        로드밸런서가 등록 해제하도록 유도
//! 2. 리스 반납 대기        다른 워커가 즉시 인수하도록 (TTL 60초를 기다리지 않게)
//! 3. 누산기 플러시         다이제스트 시간 누산기를 partial=true 로 저장
//! 4. 쓰기 버퍼 플러시      드롭하지 않는다
//! 5. 종료
//! ```
//!
//! 순서가 뒤바뀌면: 리스를 먼저 반납하면 다른 워커가 인수한 뒤 우리가 같은 hour 를
//! 플러시해 **누산기가 이중 계산**된다.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// 셧다운 조정자. 서브시스템은 [`Shutdown::wait`] 로 신호를 기다린다.
#[derive(Debug)]
pub struct Shutdown {
    notify: Notify,
    /// `stopTimeout` 안에 끝내야 하는 총 예산.
    grace: Duration,
}

impl Shutdown {
    pub fn new(grace: Duration) -> Arc<Self> {
        Arc::new(Self {
            notify: Notify::new(),
            grace,
        })
    }

    /// 셧다운을 시작한다. 여러 번 호출해도 안전하다.
    pub fn trigger(&self) {
        self.notify.notify_waiters();
    }

    /// 셧다운 신호를 기다린다.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// 정리 단계에 배분할 예산. 로드밸런서 등록 해제에 절반을 쓴다.
    ///
    /// ECS 의 `deregistration_delay` 가 기본 300초인데 우리 `stopTimeout` 은 그보다 짧다.
    /// 등록 해제를 **기다리지 않고** 우리 쪽 정리를 마치는 것이 맞다 — 남은 요청은
    /// 로드밸런서가 처리하고, 우리가 데이터를 잃는 것이 더 나쁘다.
    pub fn drain_budget(&self) -> Duration {
        self.grace / 2
    }
}

/// `SIGTERM` / `SIGINT` 를 기다린다.
///
/// ECS 는 태스크 중지 시 `SIGTERM` 을 보내고 `stopTimeout` 후 `SIGKILL` 한다.
/// 로컬 개발에서는 Ctrl-C(`SIGINT`)를 쓴다.
pub async fn wait_for_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "SIGTERM 핸들러를 등록할 수 없다");
                // 핸들러를 못 걸면 Ctrl-C 만이라도 받는다.
                let _ = tokio::signal::ctrl_c().await;
                return "sigint";
            }
        };
        tokio::select! {
            _ = term.recv() => "sigterm",
            _ = tokio::signal::ctrl_c() => "sigint",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "sigint"
    }
}

/// 정리 단계를 예산 안에서 순서대로 실행한다.
///
/// 한 단계가 예산을 넘기면 **다음 단계를 건너뛰지 않고** 남은 예산으로 계속한다.
/// 마지막 단계(버퍼 플러시)가 데이터 유실과 직결되므로 앞 단계가 시간을 다 먹으면 안 된다.
pub async fn run_stages(total: Duration, stages: Vec<(&'static str, ShutdownStage)>) {
    let started = tokio::time::Instant::now();
    let count = stages.len().max(1) as u32;
    for (name, stage) in stages {
        let elapsed = started.elapsed();
        let remaining = total.saturating_sub(elapsed);
        if remaining.is_zero() {
            tracing::warn!(stage = name, "셧다운 예산 소진 — 이 단계를 건너뛴다");
            continue;
        }
        // 남은 단계 수로 예산을 나눠 한 단계가 전부 먹지 못하게 한다.
        let budget = remaining / count.max(1);
        match tokio::time::timeout(budget.max(Duration::from_millis(100)), stage).await {
            Ok(()) => {
                tracing::info!(stage = name, elapsed_ms = %started.elapsed().as_millis(), "정리 완료")
            }
            Err(_) => {
                tracing::warn!(stage = name, budget_ms = %budget.as_millis(), "정리 시간 초과")
            }
        }
    }
    tracing::info!(total_ms = %started.elapsed().as_millis(), "셧다운 완료");
}

/// 정리 작업 하나. 박싱해 두면 단계 목록을 데이터로 다룰 수 있다.
pub type ShutdownStage = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

/// 정리 작업을 [`ShutdownStage`] 로 만든다.
pub fn stage(f: impl Future<Output = ()> + Send + 'static) -> ShutdownStage {
    Box::pin(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn trigger_wakes_all_waiters() {
        let s = Shutdown::new(Duration::from_secs(45));
        let hits = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let (s, hits) = (s.clone(), hits.clone());
            handles.push(tokio::spawn(async move {
                s.wait().await;
                hits.fetch_add(1, Ordering::SeqCst);
            }));
        }
        // 대기자가 등록될 시간을 준다.
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.trigger();
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn stages_run_in_order() {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mk = |name: &'static str, order: Arc<std::sync::Mutex<Vec<&'static str>>>| {
            stage(async move {
                order.lock().unwrap().push(name);
            })
        };
        run_stages(
            Duration::from_secs(5),
            vec![
                ("readyz", mk("readyz", order.clone())),
                ("lease", mk("lease", order.clone())),
                ("flush", mk("flush", order.clone())),
            ],
        )
        .await;
        assert_eq!(*order.lock().unwrap(), vec!["readyz", "lease", "flush"]);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_stage_does_not_starve_later_stages() {
        // 앞 단계가 늦어져도 마지막 단계(버퍼 플러시)가 실행돼야 한다.
        let flushed = Arc::new(AtomicUsize::new(0));
        let f = flushed.clone();
        run_stages(
            Duration::from_secs(6),
            vec![
                (
                    "slow",
                    stage(async { tokio::time::sleep(Duration::from_secs(60)).await }),
                ),
                (
                    "flush",
                    stage(async move {
                        f.fetch_add(1, Ordering::SeqCst);
                    }),
                ),
            ],
        )
        .await;
        assert_eq!(
            flushed.load(Ordering::SeqCst),
            1,
            "마지막 단계가 실행되지 않았다"
        );
    }

    #[test]
    fn drain_budget_is_half_the_grace() {
        let s = Shutdown::new(Duration::from_secs(45));
        assert_eq!(s.drain_budget(), Duration::from_millis(22_500));
        assert_eq!(s.grace(), Duration::from_secs(45));
    }
}
