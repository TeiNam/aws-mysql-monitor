//! `/healthz` · `/readyz` 와 준비 상태 (M0-8).
//!
//! # 두 엔드포인트의 의미가 다르다
//!
//! | 경로 | 의미 | 실패 시 |
//! |---|---|---|
//! | `/healthz` | **프로세스가 살아 있다.** 항상 200 | 컨테이너 재시작 |
//! | `/readyz` | **트래픽을 받을 준비가 됐다.** 의존성 + 리더 상태 | 로드밸런서 대상에서 제외 |
//!
//! `/healthz` 를 의존성과 묶으면 DynamoDB 스로틀에 컨테이너가 재시작되는 참사가 난다.
//! 살아 있는 것과 일할 수 있는 것은 다르다.
//!
//! # standby 는 503 이다 (F1)
//!
//! 수집 리더(`LEADER#collect`)를 못 잡은 워커는 목표 샤드 수가 0 이고 데이터를 갖고 있지
//! 않다. 실시간 지표·`in_flight` 방송이 워커 메모리에 있으므로 클라이언트는 **active 에
//! 붙어야 한다**([05 §7.1](../../../.claude/docs/05-collector.md)).
//!
//! ⚠ `/readyz` 503 은 **라우팅만** 막는다. 수집 소유권과는 무관하다 —
//! 그 연결을 만드는 것은 샤드 리스의 리더 게이트다. 초기 설계는 이걸 혼동해
//! ADR-018 이 금지한 다중 active 상태를 1단계 기본값으로 만들었다.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// 워커의 준비 상태. 각 서브시스템이 자기 플래그를 갱신한다.
#[derive(Debug)]
pub struct Readiness {
    /// 설정을 읽었다. 기동 직후 false.
    config_loaded: AtomicBool,
    /// 저장소(DynamoDB)에 접근할 수 있다.
    storage_ok: AtomicBool,
    /// **KMS 접근 거부 상태** (F25). 재시도가 무의미하므로 즉시 503 + 알림.
    kms_denied: AtomicBool,
    /// **이 워커가 저장된 인증 모드를 수행할 수 있는가.**
    ///
    /// # 왜 준비 상태에 넣나 (교차 리뷰 5차)
    ///
    /// 롤링 배포 중에는 Cognito 검증기가 있는 워커와 없는 워커가 섞인다. 전역 설정이
    /// `cognito` 로 바뀌면 **검증기 없는 워커는 모든 요청을 거부**하는데, 그 워커도
    /// 계속 트래픽을 받는다 — 사용자는 간헐적 401 을 보고 화면이 토큰을 지운다.
    ///
    /// 준비 상태에 넣으면 ECS·ALB 가 그 워커를 빼고 새 워커로 교체한다.
    /// 저장된 모드가 `cognito` 가 아니면 항상 참이다.
    auth_mode_supported: AtomicBool,
    /// 수집 리더를 보유했다. `false` 면 standby.
    collect_leader: AtomicBool,
    /// 이 워커가 API 역할을 수행하는가. 보고용이다.
    serves_api: bool,
    /// 이 워커가 수집을 하는가. `collect_stale` 판정에 쓴다 — api 전용 워커는 수집
    /// 성공 시각이 늘 0 이므로 그걸 정지로 읽으면 안 된다.
    runs_collector: bool,
    /// **준비 판정에 리더 여부를 넣는가.**
    ///
    /// FR-OPS-08: 1단계는 active 1대 + standby 로 운영하고 standby 는 `/readyz` 503 으로
    /// ALB 에서 빠진다. 그건 **한 워커가 API 와 수집을 겸할 때**만 성립한다.
    ///
    /// ⚠ 예전에는 `serves_api` 만 봤다. 그러면 `role=api` 워커(수집을 하지 않으므로
    /// 리스를 잡지 않는다)가 **영원히 준비되지 않아 ALB 가 트래픽을 보내지 않는다** —
    /// API 가 전면 중단된다. 역할 분리(2단계)를 켜는 순간 드러나는 결함이었다.
    requires_leadership: bool,
    /// 종료 절차가 시작됐다. 이후 항상 503 (등록 해제를 먼저 유도한다).
    draining: AtomicBool,
    /// 마지막으로 수집 tick 이 성공한 시각.
    last_collect_ok_ms: AtomicI64,
    /// 수집 리더가 된 시각. **유예 기간의 기준이다.**
    ///
    /// 성공 기록이 없는 것(`0`)을 그대로 정지로 보면 리더 취임 직후·배포 직후·수집 대상이
    /// 0개인 배포에서 즉시 경보가 뜬다(교차 리뷰 4회차). 취임부터 유예를 준다.
    became_leader_ms: AtomicI64,
}

/// 수집이 **멈춘 것으로 볼** 무응답 시간 (밀리초).
///
/// 탐지 주기(기본 1초)와 무관하게 넉넉히 잡는다 — 잠깐의 실패는 정지가 아니다.
/// 5분은 탐색 주기와 같은 크기이고, 그 안에 한 번도 성공하지 못했다면 손볼 일이다.
pub const COLLECT_STALE_AFTER_MS: i64 = 300_000;

impl Readiness {
    /// `serves_api` — API 트래픽을 받는가. `runs_collector` — 수집을 하는가.
    ///
    /// 두 값을 따로 받는 이유는 위 [`Self::requires_leadership`] 주석에 있다.
    pub fn new_for_role(serves_api: bool, runs_collector: bool) -> Arc<Self> {
        Arc::new(Self {
            config_loaded: AtomicBool::new(false),
            // 기본은 참이다 — 저장된 모드가 `cognito` 일 때만 갱신된다.
            auth_mode_supported: AtomicBool::new(true),
            storage_ok: AtomicBool::new(false),
            kms_denied: AtomicBool::new(false),
            collect_leader: AtomicBool::new(false),
            serves_api,
            // API 와 수집을 **겸하는** 워커만 리더 여부로 ALB 등록을 가른다.
            runs_collector,
            requires_leadership: serves_api && runs_collector,
            draining: AtomicBool::new(false),
            last_collect_ok_ms: AtomicI64::new(0),
            became_leader_ms: AtomicI64::new(0),
        })
    }

    pub fn set_config_loaded(&self, v: bool) {
        self.config_loaded.store(v, Ordering::Relaxed);
    }

    /// 저장된 인증 모드를 이 워커가 수행할 수 있는가.
    ///
    /// 설정을 읽는 쪽(`settings_state` 폴러)이 부른다: 모드가 `cognito` 인데 이 워커에
    /// 검증기가 없으면 `false` 다.
    pub fn set_auth_mode_supported(&self, v: bool) {
        self.auth_mode_supported.store(v, Ordering::Relaxed);
    }
    pub fn set_storage_ok(&self, v: bool) {
        self.storage_ok.store(v, Ordering::Relaxed);
    }
    /// KMS 거부는 **사람이 고칠 때까지** 유지된다. 자동 해제하지 않는다.
    pub fn set_kms_denied(&self, v: bool) {
        self.kms_denied.store(v, Ordering::Relaxed);
    }
    pub fn set_collect_leader(&self, v: bool) {
        self.set_collect_leader_at(v, 0);
    }

    /// 리더 여부를 세우고, **새로 리더가 됐으면 그 시각을 기록한다.**
    ///
    /// `collect_stale` 유예의 기준이다 — 취임 직후를 정지로 보면 배포마다 경보가 뜬다.
    /// `now_ms = 0` 이면 시각을 기록하지 않는다(테스트·종료 경로).
    pub fn set_collect_leader_at(&self, v: bool, now_ms: i64) {
        let was = self.collect_leader.swap(v, Ordering::Relaxed);
        if v && !was && now_ms > 0 {
            self.became_leader_ms.store(now_ms, Ordering::Relaxed);
        }
        if !v {
            self.became_leader_ms.store(0, Ordering::Relaxed);
        }
    }
    pub fn begin_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }
    pub fn record_collect_ok(&self, now_ms: i64) {
        self.last_collect_ok_ms.store(now_ms, Ordering::Relaxed);
    }
    pub fn is_collect_leader(&self) -> bool {
        self.collect_leader.load(Ordering::Relaxed)
    }
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    /// 시스템 시계로 스냅샷을 만든다.
    pub fn snapshot(&self) -> ReadyReport {
        use dbmon_core::time::Clock as _;
        self.snapshot_at(dbmon_core::time::SystemClock.now_ms())
    }

    /// # 수집 정지는 `ready` 를 내리지 **않는다**
    ///
    /// 내리면 로드밸런서가 이 태스크를 빼고 ECS 가 교체한다. 수집 실패의 원인은 보통
    /// 환경(대상 DB 접속 불가·IAM)이므로 **교체해도 낫지 않고 무한 교체가 된다** —
    /// 그건 정지보다 나쁘다.
    ///
    /// 대신 `collect_stale` 로 **드러낸다.** `/healthz` 는 프로세스 생존만 보고
    /// (ECS 헬스체크가 그걸 쓴다 — standby 워커를 죽이지 않기 위해서다), 수집이 죽은
    /// 것은 이 필드로 외부에서 경보를 건다. 자체 CloudWatch 지표 발행은 아직 없다
    /// ([17](../../.claude/docs/17-roadmap-tasks.md) FR-OPS-09).
    pub fn snapshot_at(&self, now_ms: i64) -> ReadyReport {
        let draining = self.draining.load(Ordering::Relaxed);
        let config_loaded = self.config_loaded.load(Ordering::Relaxed);
        let storage_ok = self.storage_ok.load(Ordering::Relaxed);
        let kms_denied = self.kms_denied.load(Ordering::Relaxed);
        let collect_leader = self.collect_leader.load(Ordering::Relaxed);
        let auth_mode_supported = self.auth_mode_supported.load(Ordering::Relaxed);

        let ready = !draining
            && config_loaded
            && storage_ok
            && !kms_denied
            // 저장된 인증 모드를 수행할 수 없으면 트래픽을 받지 않는다 — 받으면
            // 모든 요청이 401 이 되고 화면이 토큰을 지운다.
            && auth_mode_supported
            // **API 와 수집을 겸하는 워커만** 리더 여부를 따진다 (FR-OPS-08 의
            // active/standby). collector 전용은 트래픽을 받지 않고, api 전용은
            // 리스를 잡지 않으므로 리더 여부를 물으면 영원히 준비되지 않는다.
            && (!self.requires_leadership || collect_leader);

        // **수집해야 하는 워커에서만** 의미가 있다. api 전용 워커는 성공 시각이 늘 0 이다.
        let last_ok = self.last_collect_ok_ms.load(Ordering::Relaxed);
        // **기준은 "성공 시각과 취임 시각 중 나중" 이다.**
        //
        // 성공 기록이 없을 때 `0` 을 그대로 정지로 보면 취임 직후·배포 직후·수집 대상이
        // 0개인 배포에서 즉시 경보가 뜬다(교차 리뷰 4회차).
        //
        // 그리고 **성공 시각만 보면 재취임이 깨진다**: 5분 넘게 비리더였다가 다시 리더가
        // 되면 이전 임기의 성공 시각이 이미 낡아 있어 첫 수집 전에 정지로 판정된다
        // (교차 리뷰 5회차). 새 임기의 유예는 취임 시각부터다.
        let became = self.became_leader_ms.load(Ordering::Relaxed);
        let since = match (last_ok, became) {
            (0, 0) => None,
            (ok, 0) => Some(ok),
            (0, b) => Some(b),
            (ok, b) => Some(ok.max(b)),
        };
        let collect_stale = self.runs_collector
            && collect_leader
            && !draining
            && since.is_some_and(|t| now_ms - t > COLLECT_STALE_AFTER_MS);

        ReadyReport {
            ready,
            collect_stale,
            draining,
            config_loaded,
            storage_ok,
            kms_denied,
            auth_mode_supported,
            collect_leader,
            serves_api: self.serves_api,
            last_collect_ok_ms: self.last_collect_ok_ms.load(Ordering::Relaxed),
            reason: if ready {
                None
            } else {
                Some(Self::reason(
                    draining,
                    config_loaded,
                    storage_ok,
                    kms_denied,
                    auth_mode_supported,
                    self.requires_leadership,
                    collect_leader,
                ))
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reason(
        draining: bool,
        config_loaded: bool,
        storage_ok: bool,
        kms_denied: bool,
        auth_mode_supported: bool,
        serves_api: bool,
        collect_leader: bool,
    ) -> &'static str {
        // 순서가 진단 우선순위다. 가장 조치가 필요한 것을 먼저 보고한다.
        if kms_denied {
            "kms_access_denied"
        } else if draining {
            "draining"
        } else if !config_loaded {
            "config_not_loaded"
        } else if !storage_ok {
            "storage_unavailable"
        } else if !auth_mode_supported {
            // 이 워커는 저장된 인증 모드를 수행할 수 없다 (예: Cognito 검증기 없음).
            "auth_mode_unsupported"
        } else if serves_api && !collect_leader {
            "standby"
        } else {
            "unknown"
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReadyReport {
    pub ready: bool,
    /// **수집이 멈췄다.** `ready` 를 내리지 않는 이유는 [`Readiness::snapshot_at`] 에 있다.
    ///
    /// 이 값이 참인 채로 유지되면 손볼 일이다 — `/api/collector/status` 로 사유를 본다.
    pub collect_stale: bool,
    pub draining: bool,
    pub config_loaded: bool,
    pub storage_ok: bool,
    pub kms_denied: bool,
    /// 저장된 인증 모드를 이 워커가 수행할 수 있는가.
    ///
    /// 거짓이면 `reason` 이 `auth_mode_unsupported` 다 — 롤링 배포 중 검증기 없는
    /// 워커가 남아 있다는 뜻이고, ECS 가 교체할 때까지 트래픽을 받지 않는다.
    pub auth_mode_supported: bool,
    pub collect_leader: bool,
    pub serves_api: bool,
    pub last_collect_ok_ms: i64,
    /// 준비되지 않은 사유. 운영자가 `curl` 한 번으로 알 수 있어야 한다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// `/healthz` — **항상 200**. 프로세스가 응답하면 살아 있는 것이다.
pub async fn healthz() -> &'static str {
    "ok"
}

/// `/readyz` — 준비 상태. 준비되지 않으면 503 + 사유.
pub async fn readyz(State(r): State<Arc<Readiness>>) -> (StatusCode, Json<ReadyReport>) {
    let report = r.snapshot();
    let code = if report.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `role=all` 상당 워커(API + 수집 겸임). `serves_api=false` 면 collector 전용.
    fn ready_worker(serves_api: bool) -> Arc<Readiness> {
        let r = Readiness::new_for_role(serves_api, true);
        r.set_config_loaded(true);
        r.set_storage_ok(true);
        r
    }

    #[test]
    fn starts_not_ready() {
        let r = Readiness::new_for_role(true, true);
        let s = r.snapshot();
        assert!(!s.ready);
        assert_eq!(s.reason, Some("config_not_loaded"));
    }

    #[test]
    fn api_worker_needs_leader_but_collector_does_not() {
        let api = ready_worker(true);
        assert_eq!(api.snapshot().reason, Some("standby"));
        api.set_collect_leader(true);
        assert!(api.snapshot().ready);

        // collector 전용 워커는 리더가 아니어도 준비됨이다 — 트래픽을 받지 않는다.
        let collector = ready_worker(false);
        assert!(collector.snapshot().ready, "{:?}", collector.snapshot());
    }

    /// **`role=api` 워커는 리더가 아니어도 준비됨이어야 한다.**
    ///
    /// api 전용 워커는 수집을 하지 않으므로 수집 리더 리스를 잡지 않는다. 준비 판정에
    /// 리더 여부를 넣으면 **영원히 준비되지 않아 ALB 가 트래픽을 보내지 않는다** —
    /// API 전면 중단이다. 역할 분리를 켜는 순간 드러나는 결함이었다.
    #[test]
    fn an_api_only_worker_is_ready_without_the_collect_lease() {
        let api_only = Readiness::new_for_role(true, false);
        api_only.set_config_loaded(true);
        api_only.set_storage_ok(true);
        assert!(!api_only.is_collect_leader(), "리스를 잡지 않는다");
        assert!(
            api_only.snapshot().ready,
            "api 전용 워커가 준비되지 않았다 — ALB 가 트래픽을 보내지 않는다: {:?}",
            api_only.snapshot()
        );
    }

    /// 겸임 워커(`role=all`)는 FR-OPS-08 대로 standby 가 ALB 에서 빠져야 한다.
    #[test]
    fn a_combined_worker_standby_stays_out_of_the_load_balancer() {
        let combined = Readiness::new_for_role(true, true);
        combined.set_config_loaded(true);
        combined.set_storage_ok(true);
        assert!(!combined.snapshot().ready, "standby 가 ALB 에 붙는다");
        assert_eq!(combined.snapshot().reason, Some("standby"));
        combined.set_collect_leader(true);
        assert!(combined.snapshot().ready);
    }

    /// F25 — KMS 거부는 다른 어떤 사유보다 먼저 보고돼야 한다. 조치가 다르다.
    #[test]
    fn kms_denial_takes_priority_in_diagnosis() {
        let r = ready_worker(true);
        r.set_collect_leader(true);
        r.set_kms_denied(true);
        let s = r.snapshot();
        assert!(!s.ready);
        assert_eq!(s.reason, Some("kms_access_denied"));

        // draining 과 겹쳐도 KMS 가 먼저다.
        r.begin_draining();
        assert_eq!(r.snapshot().reason, Some("kms_access_denied"));
    }

    #[test]
    fn draining_makes_it_unready_immediately() {
        let r = ready_worker(true);
        r.set_collect_leader(true);
        assert!(r.snapshot().ready);
        r.begin_draining();
        let s = r.snapshot();
        assert!(
            !s.ready,
            "종료 절차가 시작되면 즉시 등록 해제를 유도해야 한다"
        );
        assert_eq!(s.reason, Some("draining"));
    }

    #[test]
    fn storage_failure_is_reported() {
        let r = ready_worker(true);
        r.set_collect_leader(true);
        r.set_storage_ok(false);
        assert_eq!(r.snapshot().reason, Some("storage_unavailable"));
    }

    #[tokio::test]
    async fn healthz_is_unconditional() {
        // 의존성이 전부 죽어도 200 이어야 한다 — 아니면 컨테이너가 재시작 루프에 빠진다.
        assert_eq!(healthz().await, "ok");
    }

    #[tokio::test]
    async fn readyz_returns_503_with_reason() {
        let r = Readiness::new_for_role(true, true);
        let (code, Json(body)) = readyz(State(r.clone())).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!body.ready);
        assert!(body.reason.is_some());

        r.set_config_loaded(true);
        r.set_storage_ok(true);
        r.set_collect_leader(true);
        let (code, Json(body)) = readyz(State(r)).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.ready);
        assert_eq!(body.reason, None);
    }

    #[test]
    fn report_serializes_without_none_reason() {
        let r = ready_worker(false);
        let j = serde_json::to_string(&r.snapshot()).unwrap();
        assert!(!j.contains("reason"), "{j}");
    }
    /// **수집이 멈춘 것을 드러낸다 — 다만 `ready` 를 내리지 않는다.**
    ///
    /// 내리면 로드밸런서가 태스크를 빼고 ECS 가 교체하는데, 수집 실패의 원인은 보통
    /// 환경이라 교체해도 낫지 않고 **무한 교체**가 된다. 그건 정지보다 나쁘다.
    /// 그래서 값으로 드러내고 경보는 외부에 맡긴다(교차 리뷰 3회차).
    #[test]
    fn collection_staleness_is_reported_without_dropping_ready() {
        const NOW: i64 = 1_787_000_000_000;
        let r = ready_worker(true);
        // **취임 직후는 정지가 아니다.** 유예를 취임 시각부터 센다.
        r.set_collect_leader_at(true, NOW);
        let fresh = r.snapshot_at(NOW);
        assert!(
            !fresh.collect_stale,
            "취임 직후를 정지로 봤다 — 배포마다 경보가 뜬다"
        );

        // 취임 뒤 유예를 넘겼는데 성공 기록이 없다 → 정지다.
        let s = r.snapshot_at(NOW + COLLECT_STALE_AFTER_MS + 1);
        assert!(s.collect_stale, "유예를 넘겼는데 정지로 보지 않았다");
        assert!(s.ready, "정지가 ready 를 내렸다 — 무한 교체가 된다");

        // 방금 성공했다 → 정지가 아니다.
        r.record_collect_ok(NOW);
        assert!(!r.snapshot_at(NOW).collect_stale);

        // 상한을 넘겼다 → 다시 정지다.
        assert!(
            r.snapshot_at(NOW + COLLECT_STALE_AFTER_MS + 1)
                .collect_stale,
            "상한을 넘겼는데 정지로 보지 않았다"
        );
        // 경계에서는 아직 아니다.
        assert!(!r.snapshot_at(NOW + COLLECT_STALE_AFTER_MS).collect_stale);
    }

    /// **재취임도 유예를 새로 받는다.**
    ///
    /// 5분 넘게 비리더였다가 다시 리더가 되면 이전 임기의 성공 시각이 이미 낡아 있다.
    /// 성공 시각만 보면 첫 수집 전에 정지로 판정된다 — 리더가 오갈 때마다 경보가 뜬다
    /// (교차 리뷰 5회차).
    #[test]
    fn reacquiring_leadership_restarts_the_grace_period() {
        const NOW: i64 = 1_787_000_000_000;
        let r = ready_worker(true);

        // 1차 임기: 수집이 잘 됐다.
        r.set_collect_leader_at(true, NOW);
        r.record_collect_ok(NOW);

        // 리더를 잃고 오래 지났다.
        r.set_collect_leader_at(false, NOW);
        let much_later = NOW + 10 * COLLECT_STALE_AFTER_MS;

        // 2차 임기 취임 — 이전 임기의 성공 시각은 이미 낡았다.
        r.set_collect_leader_at(true, much_later);
        assert!(
            !r.snapshot_at(much_later).collect_stale,
            "재취임 직후를 정지로 봤다 — 리더가 오갈 때마다 경보가 뜬다"
        );

        // 새 임기에서도 유예를 넘기면 정지다.
        assert!(
            r.snapshot_at(much_later + COLLECT_STALE_AFTER_MS + 1)
                .collect_stale
        );
    }

    /// **api 전용 워커는 정지로 보지 않는다.** 수집 성공 시각이 늘 0 이다.
    #[test]
    fn an_api_only_worker_is_never_collect_stale() {
        const NOW: i64 = 1_787_000_000_000;
        let r = Readiness::new_for_role(true, false);
        r.set_config_loaded(true);
        r.set_storage_ok(true);
        assert!(!r.snapshot_at(NOW).collect_stale);
    }

    /// 종료 중에는 정지로 보지 않는다 — 수집을 멈추는 것이 정상 절차다.
    #[test]
    fn draining_is_not_reported_as_stale() {
        const NOW: i64 = 1_787_000_000_000;
        let r = ready_worker(true);
        r.set_collect_leader_at(true, NOW);
        r.begin_draining();
        assert!(!r.snapshot_at(NOW).collect_stale);
    }
}
