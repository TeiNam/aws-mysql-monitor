//! 수집 태스크 관리 (M2-6) — 인스턴스마다 태스크 하나.
//!
//! # 왜 인스턴스마다 태스크인가
//!
//! 한 루프에서 500대를 순회하면 tick 하나가 500 × 쿼리 시간이 되어 **1초 케이던스가
//! 무너진다.** 게다가 한 대가 느리면 나머지 499대의 탐지 해상도가 함께 떨어진다.
//! [`InstanceCollector`] 는 인스턴스별 상태(진행 중 캐시, 서킷)를 들고 있어
//! 태스크 단위로 나누는 것이 자연스럽다.
//!
//! # 태스크 집합을 매 탐색마다 맞춘다
//!
//! 등록부가 변한다 — 인스턴스가 생기고, 정지되고, 필터에서 제외되고, 삭제된다.
//! **무엇이 돌아야 하는가** 는 순수 함수로 판정하고([`desired_ids`], [`task_delta`]),
//! 태스크를 띄우고 죽이는 일만 런타임이 한다. 이 분리 덕에 "정지된 인스턴스에
//! 태스크가 남는가" 같은 질문을 AWS 없이 답할 수 있다.
//!
//! [`InstanceCollector`]: crate::collector::InstanceCollector

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use dbmon_core::instance::Instance;
use dbmon_core::pause::PauseSet;
use dbmon_core::time::EpochMs;

/// 수집 태스크가 있어야 하는 인스턴스들.
///
/// [`Instance::is_collectible`] 하나로 판정한다 — 그 함수가 상태·버전·엔드포인트·삭제를
/// 모두 본다. 여기서 조건을 다시 적으면 두 곳이 갈린다.
///
/// ⚠ `is_collectible()` 은 이 파일이 만들어질 때까지 **비테스트 호출부가 없었다**
/// (2차 리뷰가 지적). 이 함수가 그 유일한 호출부다.
///
/// # 정지 스코프도 여기서만 적용한다
///
/// 사람이 멈춘 인스턴스는 **목표 집합에서 빠진다** — 그러면 태스크 정리(중지·진행 중
/// 레코드 확정)가 "인스턴스가 사라졌다" 와 완전히 같은 경로를 탄다. 태스크 쪽에
/// "멈췄나?" 검사를 심으면 태스크는 살아 있는데 아무 일도 안 하는 상태가 되고,
/// 화면의 `collecting` 수가 거짓말을 한다.
pub fn desired_ids(instances: &[Instance], pause: &PauseSet) -> BTreeSet<String> {
    instances
        .iter()
        .filter(|i| i.is_collectible() && !pause.is_paused(i))
        .map(|i| i.id.as_str().to_string())
        .collect()
}

/// 지금 멈춰 있는 인스턴스들. **[`desired_ids`] 의 여집합 중 "멈춰서 빠진" 것**이다.
///
/// 이 목록이 필요한 이유는 태스크를 내린 뒤 **진행 중 레코드를 확정해야** 하기
/// 때문이다. 남겨 두면 고아 스윕이 임계(`orphan::STALE_THRESHOLD_MS`) 뒤에 닫지만
/// 사유가 `owner_lost` 가 된다 — **워커가 사라진 것과 사람이 멈춘 것은 다른 사실**이고,
/// 그동안 화면에는 유령 "실행 중" 이 남는다.
pub fn paused_ids(instances: &[Instance], pause: &PauseSet) -> BTreeSet<String> {
    instances
        .iter()
        .filter(|i| pause.is_paused(i))
        .map(|i| i.id.as_str().to_string())
        .collect()
}

/// 띄울 것과 죽일 것.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TaskDelta {
    pub to_start: Vec<String>,
    pub to_stop: Vec<String>,
}

/// 현재 돌고 있는 집합과 목표 집합의 차이.
///
/// **죽일 것을 먼저 계산한다.** 같은 인스턴스를 죽이고 다시 띄우면 진행 중 캐시가
/// 사라져 그 순간 실행 중이던 쿼리들이 `in_flight` 고아가 된다. 그래서 교집합은
/// 건드리지 않는다.
pub fn task_delta(running: &BTreeSet<String>, desired: &BTreeSet<String>) -> TaskDelta {
    TaskDelta {
        to_start: desired.difference(running).cloned().collect(),
        to_stop: running.difference(desired).cloned().collect(),
    }
}

/// 인스턴스별 마지막 수집 성공 시각.
///
/// # 왜 전역 값 하나로는 안 되는가
///
/// `Readiness` 는 `last_collect_ok_ms` 를 `AtomicI64` 하나로 들고 있다. 모든 태스크가
/// 거기에 쓰면 **정상인 1대가 나머지 499대의 실패를 가린다** — 만료 토큰으로 새
/// 커넥션이 전부 실패하는 인스턴스가 있어도 `CollectStaleness`(FR-OPS-09)는 끝까지
/// 초록이다. 헬스체크가 감시하려는 바로 그 상황을 놓친다.
///
/// 그래서 인스턴스별로 기록하고 **최솟값**을 신선도로 올린다. 한 대라도 밀리면
/// 신선도가 밀린다.
#[derive(Debug, Default, Clone)]
pub struct CollectFreshness {
    per_instance: Arc<Mutex<BTreeMap<String, EpochMs>>>,
}

impl CollectFreshness {
    pub fn new() -> Self {
        Self::default()
    }

    /// 이 인스턴스의 tick 이 성공했다.
    pub fn record(&self, instance_id: &str, now_ms: EpochMs) {
        if let Ok(mut m) = self.per_instance.lock() {
            m.insert(instance_id.to_string(), now_ms);
        }
    }

    /// 더 이상 수집하지 않는 인스턴스를 잊는다.
    ///
    /// **없으면 최솟값이 영구히 과거에 고정된다** — 삭제된 인스턴스의 마지막 성공
    /// 시각이 계속 최솟값이 되어 신선도가 회복되지 않는다.
    pub fn retain(&self, live: &BTreeSet<String>) {
        if let Ok(mut m) = self.per_instance.lock() {
            m.retain(|id, _| live.contains(id.as_str()));
        }
    }

    /// **가장 오래된** 성공 시각. 수집 중인 인스턴스가 없으면 `None`.
    pub fn oldest(&self) -> Option<EpochMs> {
        self.per_instance
            .lock()
            .ok()
            .and_then(|m| m.values().copied().min())
    }
}

/// 인스턴스 id → 인스턴스. 태스크를 띄울 때 원본이 필요하다.
pub fn index_by_id(instances: &[Instance]) -> BTreeMap<String, &Instance> {
    instances
        .iter()
        .map(|i| (i.id.as_str().to_string(), i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::EnvMapping;
    use dbmon_core::instance::InstanceState;

    fn instance(identifier: &str, state: InstanceState, endpoint: bool) -> Instance {
        let raw = crate::aws::discovery::RawDbInstance {
            identifier: identifier.into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: "ap-northeast-2".into(),
            endpoint_address: endpoint.then(|| format!("{identifier}.rds.amazonaws.com")),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        let mut i =
            crate::aws::discovery::to_instance(&raw, "123456789012", &EnvMapping::default(), 1_000)
                .expect("매핑");
        i.state = state;
        i
    }

    fn ids(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn id_of(identifier: &str) -> String {
        format!("123456789012/ap-northeast-2/{identifier}")
    }

    /// **수집 가능한 것만 태스크를 갖는다.**
    #[test]
    fn only_collectible_instances_get_tasks() {
        let instances = vec![
            instance("ok", InstanceState::Collecting, true),
            instance("pending", InstanceState::Pending, true),
            instance("disabled", InstanceState::Disabled, true),
            instance("excluded", InstanceState::Excluded, true),
            instance("unsupported", InstanceState::Unsupported, true),
            instance("deleted", InstanceState::Deleted, true),
            instance("no-endpoint", InstanceState::Collecting, false),
        ];
        let desired = desired_ids(&instances, &PauseSet::default());
        assert_eq!(desired, ids(&[&id_of("ok")]), "{desired:?}");
    }

    /// **`Unreachable` 도 태스크를 갖는다.**
    ///
    /// 태스크를 내리면 서킷 브레이커의 half-open 재시도 주체가 사라져 영구히
    /// 복구되지 않는다 ([05 §6]). 실제 쿼리 빈도는 태스크 안의 서킷이 정한다.
    #[test]
    fn unreachable_instances_keep_their_task_for_recovery() {
        let instances = vec![instance("down", InstanceState::Unreachable, true)];
        assert_eq!(
            desired_ids(&instances, &PauseSet::default()),
            ids(&[&id_of("down")]),
            "태스크를 내리면 서킷이 열린 채로 영구히 복구되지 않는다"
        );
    }

    /// 부분 장애(`Degraded`)도 수집을 계속한다.
    #[test]
    fn degraded_instances_keep_collecting() {
        let instances = vec![instance("half", InstanceState::Degraded, true)];
        assert_eq!(
            desired_ids(&instances, &PauseSet::default()),
            ids(&[&id_of("half")])
        );
    }

    /// **멈춘 스코프는 목표 집합에서 빠진다** — 스코프별로 정확히 그것만.
    ///
    /// 여기가 틀리면 두 방향으로 틀린다: 멈추라고 한 것이 계속 수집되거나(관측을
    /// 멈춘 이유가 무의미해진다), 멈추지 않은 것이 함께 멈춘다(구간이 영구히 빈다).
    #[test]
    fn a_paused_scope_drops_exactly_its_instances() {
        use dbmon_core::env::Env;
        use dbmon_core::pause::PauseScope;

        // 태그가 없으므로 셋 다 `Unknown` 환경이다. 환경별 정지는 그 사실로 검증한다.
        let instances = vec![
            instance("orders", InstanceState::Collecting, true),
            instance("billing", InstanceState::Collecting, true),
        ];
        let all_ids = ids(&[&id_of("orders"), &id_of("billing")]);

        let none = PauseSet::default();
        assert_eq!(desired_ids(&instances, &none), all_ids);
        assert!(paused_ids(&instances, &none).is_empty());

        let all = PauseSet::from_entries([(PauseScope::All.as_key(), 1)]);
        assert!(desired_ids(&instances, &all).is_empty());
        assert_eq!(paused_ids(&instances, &all), all_ids);

        let by_env = PauseSet::from_entries([(PauseScope::Env(Env::Unknown).as_key(), 1)]);
        assert!(
            desired_ids(&instances, &by_env).is_empty(),
            "환경 전체가 멈춘다"
        );

        let other_env = PauseSet::from_entries([(PauseScope::Env(Env::Prd).as_key(), 1)]);
        assert_eq!(
            desired_ids(&instances, &other_env),
            all_ids,
            "다른 환경의 정지는 이 인스턴스들에 닿지 않는다"
        );

        let one = PauseSet::from_entries([(format!("id:{}", id_of("orders")), 1)]);
        assert_eq!(desired_ids(&instances, &one), ids(&[&id_of("billing")]));
        assert_eq!(paused_ids(&instances, &one), ids(&[&id_of("orders")]));
    }

    /// **수집 불가 인스턴스는 "멈춘 것" 이 아니다.**
    ///
    /// 둘을 섞으면 정지 때문에 내려간 태스크와 인스턴스가 죽어서 내려간 태스크를
    /// 구분할 수 없고, 진행 중 레코드에 `collector_paused` 사유가 잘못 붙는다.
    #[test]
    fn a_stopped_instance_is_not_reported_as_paused() {
        let instances = vec![instance("gone", InstanceState::Deleted, true)];
        let all = PauseSet::from_entries([(dbmon_core::pause::ALL_KEY.to_string(), 1)]);
        assert!(desired_ids(&instances, &all).is_empty());
        // 전체 정지면 "멈춘 것" 으로 센다 — 진행 중 레코드가 남아 있을 수 있으므로
        // 확정 대상이어야 한다.
        assert_eq!(paused_ids(&instances, &all), ids(&[&id_of("gone")]));
        // 정지가 없으면 멈춘 것도 없다.
        assert!(paused_ids(&instances, &PauseSet::default()).is_empty());
    }

    #[test]
    fn delta_starts_new_and_stops_removed() {
        let running = ids(&["a", "b"]);
        let desired = ids(&["b", "c"]);
        let d = task_delta(&running, &desired);
        assert_eq!(d.to_start, vec!["c".to_string()]);
        assert_eq!(d.to_stop, vec!["a".to_string()]);
    }

    /// **변화가 없으면 아무것도 하지 않는다.**
    ///
    /// 같은 인스턴스를 죽이고 다시 띄우면 진행 중 캐시가 사라져 그 순간 실행 중이던
    /// 쿼리가 `in_flight` 고아가 된다. 매 탐색(5분)마다 그러면 안 된다.
    #[test]
    fn an_unchanged_set_produces_no_churn() {
        let same = ids(&["a", "b", "c"]);
        assert_eq!(task_delta(&same, &same), TaskDelta::default());
    }

    /// 등록부가 비면 전부 내린다 — 남겨 두면 사라진 인스턴스에 계속 쿼리를 보낸다.
    #[test]
    fn an_empty_desired_set_stops_everything() {
        let d = task_delta(&ids(&["a", "b"]), &BTreeSet::new());
        assert_eq!(d.to_stop, vec!["a".to_string(), "b".to_string()]);
        assert!(d.to_start.is_empty());
    }

    /// **한 대라도 밀리면 신선도가 밀려야 한다.**
    ///
    /// 전역 값 하나면 정상인 1대가 나머지의 실패를 가린다 — FR-OPS-09 가 감시하려는
    /// 상황을 놓친다.
    #[test]
    fn freshness_reports_the_slowest_instance() {
        let f = CollectFreshness::new();
        f.record("a", 1_000);
        f.record("b", 5_000);
        assert_eq!(f.oldest(), Some(1_000), "가장 오래된 성공을 보고해야 한다");

        // 빠른 쪽이 계속 성공해도 느린 쪽이 밀려 있으면 신선도는 밀린 값이다.
        f.record("b", 9_000);
        assert_eq!(f.oldest(), Some(1_000));

        // 느린 쪽이 회복되면 신선도도 회복된다.
        f.record("a", 8_000);
        assert_eq!(f.oldest(), Some(8_000));
    }

    /// **사라진 인스턴스를 잊어야 한다.**
    ///
    /// 안 잊으면 삭제된 인스턴스의 마지막 성공 시각이 영구히 최솟값이 되어
    /// 신선도가 회복되지 않는다 — 헬스체크가 영구히 실패한다.
    #[test]
    fn freshness_forgets_instances_that_are_no_longer_collected() {
        let f = CollectFreshness::new();
        f.record("gone", 1_000);
        f.record("live", 9_000);
        f.retain(&ids(&["live"]));
        assert_eq!(
            f.oldest(),
            Some(9_000),
            "사라진 인스턴스가 신선도를 영구히 잡아 둔다"
        );
    }

    /// 수집 중인 인스턴스가 없으면 신선도를 말할 수 없다.
    #[test]
    fn freshness_is_none_when_nothing_is_collected() {
        assert_eq!(CollectFreshness::new().oldest(), None);
    }

    #[test]
    fn index_finds_the_original_instance() {
        let instances = vec![instance("ok", InstanceState::Collecting, true)];
        let idx = index_by_id(&instances);
        assert!(idx.contains_key(&id_of("ok")));
        assert_eq!(idx.len(), 1);
    }
}
