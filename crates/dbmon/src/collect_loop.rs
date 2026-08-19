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

use dbmon_core::instance::Instance;

/// 수집 태스크가 있어야 하는 인스턴스들.
///
/// [`Instance::is_collectible`] 하나로 판정한다 — 그 함수가 상태·버전·엔드포인트·삭제를
/// 모두 본다. 여기서 조건을 다시 적으면 두 곳이 갈린다.
///
/// ⚠ `is_collectible()` 은 이 파일이 만들어질 때까지 **비테스트 호출부가 없었다**
/// (2차 리뷰가 지적). 이 함수가 그 유일한 호출부다.
pub fn desired_ids(instances: &[Instance]) -> BTreeSet<String> {
    instances
        .iter()
        .filter(|i| i.is_collectible())
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
        let desired = desired_ids(&instances);
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
            desired_ids(&instances),
            ids(&[&id_of("down")]),
            "태스크를 내리면 서킷이 열린 채로 영구히 복구되지 않는다"
        );
    }

    /// 부분 장애(`Degraded`)도 수집을 계속한다.
    #[test]
    fn degraded_instances_keep_collecting() {
        let instances = vec![instance("half", InstanceState::Degraded, true)];
        assert_eq!(desired_ids(&instances), ids(&[&id_of("half")]));
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

    #[test]
    fn index_finds_the_original_instance() {
        let instances = vec![instance("ok", InstanceState::Collecting, true)];
        let idx = index_by_id(&instances);
        assert!(idx.contains_key(&id_of("ok")));
        assert_eq!(idx.len(), 1);
    }
}
