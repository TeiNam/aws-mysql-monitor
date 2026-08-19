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
//! 붙어야 한다**([05 §7.1](../../../docs/05-collector.md)).
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
    /// 수집 리더를 보유했다. `false` 면 standby.
    collect_leader: AtomicBool,
    /// 이 워커가 API 역할을 수행하는가. 아니면 리더 여부를 준비 판정에 넣지 않는다.
    serves_api: bool,
    /// 종료 절차가 시작됐다. 이후 항상 503 (등록 해제를 먼저 유도한다).
    draining: AtomicBool,
    /// 마지막으로 수집 tick 이 성공한 시각.
    last_collect_ok_ms: AtomicI64,
}

impl Readiness {
    pub fn new(serves_api: bool) -> Arc<Self> {
        Arc::new(Self {
            config_loaded: AtomicBool::new(false),
            storage_ok: AtomicBool::new(false),
            kms_denied: AtomicBool::new(false),
            collect_leader: AtomicBool::new(false),
            serves_api,
            draining: AtomicBool::new(false),
            last_collect_ok_ms: AtomicI64::new(0),
        })
    }

    pub fn set_config_loaded(&self, v: bool) {
        self.config_loaded.store(v, Ordering::Relaxed);
    }
    pub fn set_storage_ok(&self, v: bool) {
        self.storage_ok.store(v, Ordering::Relaxed);
    }
    /// KMS 거부는 **사람이 고칠 때까지** 유지된다. 자동 해제하지 않는다.
    pub fn set_kms_denied(&self, v: bool) {
        self.kms_denied.store(v, Ordering::Relaxed);
    }
    pub fn set_collect_leader(&self, v: bool) {
        self.collect_leader.store(v, Ordering::Relaxed);
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

    pub fn snapshot(&self) -> ReadyReport {
        let draining = self.draining.load(Ordering::Relaxed);
        let config_loaded = self.config_loaded.load(Ordering::Relaxed);
        let storage_ok = self.storage_ok.load(Ordering::Relaxed);
        let kms_denied = self.kms_denied.load(Ordering::Relaxed);
        let collect_leader = self.collect_leader.load(Ordering::Relaxed);

        let ready = !draining
            && config_loaded
            && storage_ok
            && !kms_denied
            // API 를 서비스하는 워커만 리더 여부를 따진다. collector 전용 워커는
            // 리더가 아니어도 "준비됨" 이다 (애초에 트래픽을 받지 않는다).
            && (!self.serves_api || collect_leader);

        ReadyReport {
            ready,
            draining,
            config_loaded,
            storage_ok,
            kms_denied,
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
                    self.serves_api,
                    collect_leader,
                ))
            },
        }
    }

    fn reason(
        draining: bool,
        config_loaded: bool,
        storage_ok: bool,
        kms_denied: bool,
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
    pub draining: bool,
    pub config_loaded: bool,
    pub storage_ok: bool,
    pub kms_denied: bool,
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

    fn ready_worker(serves_api: bool) -> Arc<Readiness> {
        let r = Readiness::new(serves_api);
        r.set_config_loaded(true);
        r.set_storage_ok(true);
        r
    }

    #[test]
    fn starts_not_ready() {
        let r = Readiness::new(true);
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
        let r = Readiness::new(true);
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
}
