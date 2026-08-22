//! CloudWatch 메트릭 조회 — 플릿 표와 인스턴스 상세.
//!
//! # 이 모듈의 존재 이유는 **비용**이다
//!
//! 순진하게 구현하면 이 경로가 전체 청구서를 지배한다([06 §2.3]):
//!
//! ```text
//! 500대 × 15메트릭 × 60초 폴링 → 월 $3,240
//! ```
//!
//! 그래서 세 가지를 강제한다:
//!
//! 1. **엔진별 최소 세트** — 없는 메트릭은 요청하지 않는다(빈 값에도 과금된다)
//! 2. **정렬된 창(aligned window) 캐시** — 여러 사용자가 같은 화면을 봐도 1회만 호출한다.
//!    키에 `창 시작`을 넣으므로 창이 바뀔 때까지 재사용된다
//! 3. **클러스터 메트릭은 클러스터당 1회** — 인스턴스마다 요청하면 같은 값에 중복 과금
//!
//! # 왜 연결 수를 CloudWatch 에서 가져오지 않는가
//!
//! `DatabaseConnections` 는 플릿 세트에 없다. 자체 수집(`global_status.Threads_connected`)이
//! **더 신선하고(5초 vs CloudWatch 1~3분 지연) 무료**다. 화면은 그 값을 WS 스냅샷으로
//! 이미 받고 있으므로 표에서 두 출처를 나란히 놓는다.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use dbmon_core::cw_metrics::{
    DETAIL_CACHE_SECS, FLEET_PERIOD_SECS, MetricScope, MetricSpec, PeriodChoice, detail_metrics,
    fleet_metrics, period_for,
};
use dbmon_core::instance::Instance;
use dbmon_core::time::EpochMs;

use crate::aws::cloudwatch::{Series, Target, regional_series_key, series_key};
use crate::aws::cw_fetchers::MetricFetchers;

/// 상세 조회의 데이터포인트 상한. 응답 크기를 묶는다.
const MAX_DATAPOINTS: i32 = 1_440;

/// 플릿 표는 **최근 한 창**만 본다. period 의 3배를 조회 범위로 잡는다 —
/// CloudWatch 는 1~3분 지연되므로 창 하나만 보면 방금 창이 비어 있을 수 있다.
const FLEET_LOOKBACK_MULT: i64 = 3;

/// 조회 결과 캐시.
///
/// **키에 정렬된 창 시작을 넣는다.** 그래서 같은 창 안에서는 몇 명이 보든 1회만 호출하고,
/// 창이 넘어가면 자동으로 무효해진다 — TTL 타이머가 필요 없다.
/// 캐시 항목. **실패한 범위를 함께 들고 있는다.**
///
/// 처음엔 실패했으면 캐시하지 않았다(복구되면 바로 보이게). 그런데 그러면 권한 오류나
/// 조절(throttle)이 났을 때 **모든 요청이 CloudWatch 를 다시 때린다** — 사람이 화면을
/// 열어 둔 만큼 재시도가 는다. 부분 결과와 실패 목록을 함께 캐시하면 재시도가 창당
/// 1회로 묶이고, 화면은 "못 읽었다" 를 계속 표시한다.
#[derive(Clone, Default)]
struct CachedFleet {
    series: BTreeMap<String, Series>,
    failed_scopes: Vec<String>,
}

/// 상세 조회 캐시 항목. **잘렸는지 함께 들고 있는다.**
///
/// 잘린 결과를 캐시하지 않으면 긴 구간 조회가 **매 요청마다 20페이지를 다시 부른다** —
/// 60초 자동 새로고침 × 사용자 수만큼 비용이 는다(3차 교차 리뷰가 medium 으로 잡았다).
/// 잘렸다는 사실을 함께 캐시하면 화면은 계속 그 사실을 말하고 호출은 창당 1회다.
#[derive(Clone, Default)]
struct CachedDetail {
    series: BTreeMap<String, Series>,
    truncated: bool,
}

#[derive(Default)]
struct Cache {
    entries: BTreeMap<String, CachedDetail>,
    fleet: BTreeMap<String, CachedFleet>,
}

pub struct MetricsService {
    /// **리전별** 클라이언트. 메트릭은 대상 인스턴스의 리전에 있다 — 하나로 돌리면
    /// 다른 리전은 오류 없이 빈 값이 오고 화면에는 `—` 로 보인다.
    fetchers: Arc<MetricFetchers>,
    cache: Mutex<Cache>,
}

/// 화면에 내보내는 한 메트릭의 값 하나.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricPoint {
    pub name: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub stat: &'static str,
    /// **없을 수 있다.** 방금 뜬 인스턴스나 CloudWatch 지연 구간이다 — `0` 으로 접으면
    /// "CPU 0%" 라는 거짓을 표시한다.
    pub value: Option<f64>,
}

/// 플릿 조회 결과. **실패한 범위를 함께 준다** — 빈 값과 조회 실패는 화면에서
/// 구분돼야 한다.
#[derive(Debug, Clone, Default)]
pub struct FleetOutcome {
    pub rows: Vec<FleetRow>,
    /// 조회가 실패한 `계정/리전`. 비어 있으면 전부 성공했다.
    pub failed_scopes: Vec<String>,
}

/// 인스턴스 한 대의 플릿 행.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FleetRow {
    pub instance_id: String,
    pub metrics: Vec<MetricPoint>,
}

/// 상세 화면의 시계열 하나.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricSeries {
    pub name: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub stat: &'static str,
    pub timestamps_ms: Vec<EpochMs>,
    pub values: Vec<f64>,
}

impl MetricsService {
    pub fn new(fetchers: Arc<MetricFetchers>) -> Self {
        Self {
            fetchers,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// 플릿 표. **엔진별 최소 세트만** 15분 창으로 가져온다.
    pub async fn fleet(
        &self,
        instances: &[Instance],
        discovery: &dbmon_core::settings::DiscoverySettings,
        now_ms: EpochMs,
    ) -> FleetOutcome {
        let window = align(now_ms, FLEET_PERIOD_SECS);
        let cache_key = format!("fleet@{window}");
        if let Some(hit) = self.cached_fleet(&cache_key) {
            return FleetOutcome {
                rows: rows_from(instances, &hit.series),
                failed_scopes: hit.failed_scopes,
            };
        }

        let from = now_ms - FLEET_PERIOD_SECS * FLEET_LOOKBACK_MULT * 1000;
        let mut merged: BTreeMap<String, Series> = BTreeMap::new();
        let mut failed_scopes: Vec<String> = Vec::new();

        // **계정·리전마다 따로 조회한다.** `GetMetricData` 는 클라이언트의 계정·리전만
        // 본다 — 하나로 돌리면 다른 계정·리전은 오류 없이 빈 값이 온다.
        for ((account, region), region_instances) in by_scope(instances) {
            let pairs = fleet_pairs(&region_instances);
            if pairs.is_empty() {
                continue;
            }
            // ⚠ **`MaxDatapoints` 는 쿼리별이 아니라 응답 전체 상한이다.**
            //
            // 처음에 `3` 을 넣었더니(창 3개니까) 인스턴스 9대 중 **1대만 값이 왔다** — 첫
            // 쿼리가 3점을 다 써버린 것이다. 오류도 경고도 없이 나머지가 빈 값이었다.
            // 그래서 쌍 수 × 창 수로 잡는다.
            let cap = (pairs.len() as i32).saturating_mul(FLEET_LOOKBACK_MULT as i32);
            let fetcher = self
                .fetchers
                .for_target(&account, &region, discovery.role_for(&account))
                .await;
            match fetcher
                .fetch(&pairs, FLEET_PERIOD_SECS, from, now_ms, cap.max(3))
                .await
                .map(|f| f.series)
            {
                Ok(series) => {
                    // **키에 리전을 붙여 합친다.** 식별자는 리전 안에서만 고유하므로
                    // 그냥 합치면 같은 이름의 인스턴스가 서로의 값을 덮는다.
                    for (key, value) in series {
                        merged.insert(format!("{region}|{key}"), value);
                    }
                }
                Err(e) => {
                    // **한 범위의 실패가 다른 범위를 막지 않는다.** 다만 조용히 넘기지
                    // 않는다 — 실패를 빈 값으로만 두면 화면이 "데이터 없음" 으로 보여
                    // 모니터링 장애가 정상으로 읽힌다(교차 리뷰가 medium 으로 잡았다).
                    tracing::warn!(
                        %account, %region,
                        error = %crate::telemetry::Scrubbed(&e),
                        "플릿 메트릭 조회 실패 — 이 범위는 값 없이 표를 낸다"
                    );
                    failed_scopes.push(format!("{account}/{region}"));
                }
            }
        }

        // **부분 실패도 캐시한다.** 실패 목록을 함께 담으므로 화면은 계속 사실을
        // 말하고, 재시도는 창당 1회로 묶인다(창이 넘어가면 자동으로 다시 시도한다).
        self.store_fleet(
            cache_key,
            CachedFleet {
                series: merged.clone(),
                failed_scopes: failed_scopes.clone(),
            },
        );
        FleetOutcome {
            rows: rows_from(instances, &merged),
            failed_scopes,
        }
    }

    /// 인스턴스 상세. 엔진에 맞는 전체 세트를 시계열로.
    pub async fn detail(
        &self,
        instance: &Instance,
        discovery: &dbmon_core::settings::DiscoverySettings,
        from_ms: EpochMs,
        to_ms: EpochMs,
        now_ms: EpochMs,
    ) -> (Vec<MetricSeries>, PeriodChoice, bool) {
        let choice = period_for(now_ms, from_ms, to_ms);
        // **시간 범위를 해상도 경계로 맞춘다.**
        //
        // 화면은 `Date.now()` 로 ms 단위 범위를 보낸다. 그걸 그대로 키에 넣으면 두
        // 사용자가 같은 분에 같은 인스턴스를 열어도 키가 달라 **각자 CloudWatch 를
        // 부른다**(교차 리뷰가 medium 으로 잡았다). period 로 내림하면 같은 창을 보는
        // 사람들이 같은 키를 쓴다 — 데이터 자체도 그 경계로 집계되므로 잃는 것이 없다.
        let bucket = choice.period_secs * 1000;
        let from_bucket = (from_ms / bucket) * bucket;
        let to_bucket = (to_ms / bucket) * bucket;
        // **정렬된 창으로 캐시한다.** 60초 안에 여러 번 열어도 1회만 호출한다.
        let window = align(now_ms, DETAIL_CACHE_SECS);
        let cache_key = format!(
            "detail@{window}|{}|{from_bucket}|{to_bucket}|{}",
            instance.id.as_str(),
            choice.period_secs
        );

        let specs = detail_metrics(instance.engine);
        let mut truncated = false;
        let series = match self.cached(&cache_key) {
            Some(hit) => {
                truncated = hit.truncated;
                hit.series
            }
            None => {
                let pairs = detail_pairs(instance, &specs);
                // **이 인스턴스의 계정·리전**으로 묻는다. 우리 계정·배포 리전으로
                // 물으면 오류 없이 빈 값이 온다.
                let fetcher = self
                    .fetchers
                    .for_target(
                        instance.id.account(),
                        instance.id.region(),
                        discovery.role_for(instance.id.account()),
                    )
                    .await;
                match fetcher
                    .fetch(&pairs, choice.period_secs, from_ms, to_ms, MAX_DATAPOINTS)
                    .await
                {
                    Ok(f) => {
                        truncated = f.truncated;
                        // **잘려도 캐시한다.** 잘린 사실을 함께 담으므로 화면은 계속
                        // 그 사실을 말하고, 재조회는 창당 1회로 묶인다.
                        self.store(
                            cache_key,
                            CachedDetail {
                                series: f.series.clone(),
                                truncated: f.truncated,
                            },
                        );
                        f.series
                    }
                    Err(e) => {
                        tracing::warn!(
                            instance = %instance.id.as_str(),
                            error = %crate::telemetry::Scrubbed(&e),
                            "인스턴스 메트릭 조회 실패"
                        );
                        BTreeMap::new()
                    }
                }
            }
        };

        let out = specs
            .iter()
            .map(|spec| {
                let key = series_key(&target_for(instance, spec), spec);
                let found = series.get(&key);
                MetricSeries {
                    name: spec.name,
                    label: spec.label,
                    unit: spec.unit.as_str(),
                    stat: spec.stat.as_str(),
                    // **오래된 순으로 뒤집는다.** 어댑터는 최신부터 받지만 차트는
                    // 시간순이어야 한다.
                    timestamps_ms: found
                        .map(|s| s.timestamps_ms.iter().rev().copied().collect())
                        .unwrap_or_default(),
                    values: found
                        .map(|s| s.values.iter().rev().copied().collect())
                        .unwrap_or_default(),
                }
            })
            .collect();
        (out, choice, truncated)
    }

    fn cached(&self, key: &str) -> Option<CachedDetail> {
        self.cache.lock().ok()?.entries.get(key).cloned()
    }

    fn cached_fleet(&self, key: &str) -> Option<CachedFleet> {
        self.cache.lock().ok()?.fleet.get(key).cloned()
    }

    fn store_fleet(&self, key: String, value: CachedFleet) {
        if let Ok(mut c) = self.cache.lock() {
            // **창이 넘어간 항목을 버린다** — 안 버리면 창마다 쌓인다.
            c.fleet.retain(|k, _| k == &key);
            c.fleet.insert(key, value);
        }
    }

    fn store(&self, key: String, value: CachedDetail) {
        if let Ok(mut c) = self.cache.lock() {
            // **창이 넘어간 항목을 버린다.** 안 버리면 화면을 오래 열어 둔 동안
            // 창마다 항목이 쌓여 메모리가 계속 는다.
            let prefix = key.split('|').next().unwrap_or("").to_string();
            c.entries
                .retain(|k, _| k.split('|').next().unwrap_or("") == prefix || !k.contains('@'));
            c.entries.insert(key, value);
        }
    }
}

/// 이 메트릭을 어떤 대상으로 물어볼지.
fn target_for(instance: &Instance, spec: &MetricSpec) -> Target {
    match spec.scope {
        MetricScope::Instance => Target {
            scope: MetricScope::Instance,
            identifier: instance.id.identifier().to_string(),
        },
        MetricScope::Cluster => Target {
            scope: MetricScope::Cluster,
            // 클러스터가 없으면(단독 인스턴스) 이 메트릭은 애초에 세트에 없다.
            // **`identifier()` 다 (`as_str()` 이 아니다).** 후자는 `계정/리전/이름`
            // 정규화 형태라 CloudWatch 차원과 맞지 않는다.
            identifier: instance
                .cluster_id
                .as_ref()
                .map(|c| c.identifier().to_string())
                .unwrap_or_default(),
        },
    }
}

/// 인스턴스를 `(계정, 리전)` 별로 나눈다. **정렬된 맵**이라 로그가 안정적이다.
fn by_scope(instances: &[Instance]) -> BTreeMap<(String, String), Vec<Instance>> {
    let mut out: BTreeMap<(String, String), Vec<Instance>> = BTreeMap::new();
    for i in instances {
        out.entry((i.id.account().to_string(), i.id.region().to_string()))
            .or_default()
            .push(i.clone());
    }
    out
}

/// 플릿 요청 쌍. **클러스터 메트릭은 클러스터당 1회로 접는다.**
fn fleet_pairs(instances: &[Instance]) -> Vec<(Target, MetricSpec)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for i in instances {
        for spec in fleet_metrics(i.engine) {
            let t = target_for(i, spec);
            // 클러스터 대상이 비어 있으면(클러스터 없는 인스턴스) 요청하지 않는다 —
            // 빈 차원으로 물어보면 빈 값이 오면서 과금된다.
            if t.identifier.is_empty() {
                continue;
            }
            if seen.insert((t.clone(), spec.name)) {
                out.push((t, *spec));
            }
        }
    }
    out
}

fn detail_pairs(instance: &Instance, specs: &[MetricSpec]) -> Vec<(Target, MetricSpec)> {
    specs
        .iter()
        .filter_map(|spec| {
            let t = target_for(instance, spec);
            (!t.identifier.is_empty()).then_some((t, *spec))
        })
        .collect()
}

fn rows_from(instances: &[Instance], series: &BTreeMap<String, Series>) -> Vec<FleetRow> {
    instances
        .iter()
        .map(|i| FleetRow {
            instance_id: i.id.as_str().to_string(),
            metrics: fleet_metrics(i.engine)
                .iter()
                .map(|spec| MetricPoint {
                    name: spec.name,
                    label: spec.label,
                    unit: spec.unit.as_str(),
                    stat: spec.stat.as_str(),
                    value: series
                        .get(&regional_series_key(
                            i.id.region(),
                            &target_for(i, spec),
                            spec,
                        ))
                        .and_then(Series::latest),
                })
                .collect(),
        })
        .collect()
}

/// 창 시작으로 내림. 캐시 키를 정렬해 같은 창을 공유한다.
fn align(now_ms: EpochMs, period_secs: i64) -> i64 {
    let p = period_secs * 1000;
    (now_ms / p) * p
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::instance::Engine;

    fn instance(name: &str, engine: Engine, cluster: Option<&str>) -> Instance {
        instance_in("ap-northeast-2", name, engine, cluster)
    }

    fn instance_in(region: &str, name: &str, engine: Engine, cluster: Option<&str>) -> Instance {
        let raw = crate::aws::discovery::RawDbInstance {
            identifier: name.into(),
            engine: match engine {
                Engine::Mysql => "mysql".into(),
                Engine::AuroraMysql => "aurora-mysql".into(),
            },
            engine_version: match engine {
                Engine::Mysql => "8.4.6".into(),
                Engine::AuroraMysql => "8.0.mysql_aurora.3.08.0".into(),
            },
            region: region.to_string(),
            endpoint_address: Some(format!("{name}.rds.amazonaws.com")),
            endpoint_port: Some(3306),
            cluster_identifier: cluster.map(str::to_string),
            ..Default::default()
        };
        crate::aws::discovery::to_instance(
            &raw,
            "123456789012",
            &dbmon_core::env::EnvMapping::default(),
            1_000,
        )
        .expect("매핑")
    }

    /// **같은 클러스터의 인스턴스가 여럿이면 볼륨 메트릭은 1회만 요청해야 한다.**
    /// 인스턴스마다 요청하면 같은 값에 인스턴스 수만큼 과금된다.
    #[test]
    fn cluster_metrics_are_requested_once_per_cluster() {
        let a = instance("aur-1", Engine::AuroraMysql, Some("aur"));
        let b = instance("aur-2", Engine::AuroraMysql, Some("aur"));
        let pairs = fleet_pairs(&[a, b]);

        let vol: Vec<_> = pairs
            .iter()
            .filter(|(_, s)| s.name == "AuroraVolumeBytesLeftTotal")
            .collect();
        assert_eq!(vol.len(), 1, "클러스터 메트릭이 {}번 요청됐다", vol.len());
        assert_eq!(vol[0].0.scope, MetricScope::Cluster);
        assert_eq!(vol[0].0.identifier, "aur");

        // 인스턴스 메트릭은 인스턴스마다 요청한다.
        let cpu = pairs
            .iter()
            .filter(|(_, s)| s.name == "CPUUtilization")
            .count();
        assert_eq!(cpu, 2);
    }

    /// 엔진이 섞여 있어도 각자 자기 세트만 요청한다 — RDS 에 `AuroraVolume…` 을,
    /// Aurora 에 `FreeStorageSpace` 를 묻지 않는다(빈 값 + 과금).
    #[test]
    fn mixed_engines_request_only_their_own_metrics() {
        let pairs = fleet_pairs(&[
            instance("rds-1", Engine::Mysql, None),
            instance("aur-1", Engine::AuroraMysql, Some("aur")),
        ]);
        let names: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(t, s)| (t.identifier.as_str(), s.name))
            .collect();
        assert!(names.contains(&("rds-1", "FreeStorageSpace")));
        assert!(!names.contains(&("aur-1", "FreeStorageSpace")));
        assert!(!names.contains(&("rds-1", "AuroraVolumeBytesLeftTotal")));
    }

    /// 클러스터가 없는 인스턴스에는 클러스터 메트릭을 **요청하지 않는다.** 빈 차원으로
    /// 물어보면 빈 값이 오면서 과금된다.
    #[test]
    fn instances_without_a_cluster_skip_cluster_metrics() {
        // Aurora 인데 클러스터 정보가 없는 경우(탐색 직후 등).
        let pairs = fleet_pairs(&[instance("orphan", Engine::AuroraMysql, None)]);
        assert!(pairs.iter().all(|(t, _)| !t.identifier.is_empty()));
        assert!(
            pairs.iter().all(|(_, s)| s.scope == MetricScope::Instance),
            "클러스터 없는 인스턴스에 클러스터 메트릭을 요청했다"
        );
    }

    /// 값이 없으면 **`null` 이어야 한다.** `0` 으로 접으면 "CPU 0%" 라는 거짓이 된다.
    #[test]
    fn missing_values_stay_null() {
        let rows = rows_from(&[instance("rds-1", Engine::Mysql, None)], &BTreeMap::new());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].metrics.iter().all(|m| m.value.is_none()));
        assert_eq!(rows[0].metrics.len(), 3);
    }

    /// **같은 이름의 인스턴스가 두 리전에 있으면 값이 섞여서는 안 된다.**
    ///
    /// 식별자는 리전 안에서만 고유하다. 계열 키에 리전이 없으면 한 리전의 CPU 가
    /// 다른 리전 인스턴스의 값으로 보인다 — 90% 를 엉뚱한 곳에 붙이는 종류의 오류다.
    #[test]
    fn same_name_in_two_regions_does_not_share_values() {
        let seoul = instance_in("ap-northeast-2", "orders-01", Engine::Mysql, None);
        let virginia = instance_in("us-east-1", "orders-01", Engine::Mysql, None);
        assert_ne!(seoul.id.as_str(), virginia.id.as_str());

        // 서울 CPU 만 담은 응답.
        let spec = fleet_metrics(Engine::Mysql)[0];
        let mut series = BTreeMap::new();
        series.insert(
            crate::aws::cloudwatch::regional_series_key(
                "ap-northeast-2",
                &target_for(&seoul, &spec),
                &spec,
            ),
            Series {
                key: crate::aws::cloudwatch::series_key(&target_for(&seoul, &spec), &spec),
                timestamps_ms: vec![1_000],
                values: vec![42.0],
            },
        );

        let rows = rows_from(&[seoul, virginia], &series);
        assert_eq!(rows[0].metrics[0].value, Some(42.0), "서울 값이 없다");
        assert_eq!(
            rows[1].metrics[0].value, None,
            "버지니아 인스턴스가 서울 값을 가져갔다"
        );
    }

    /// **계정·리전별로 나눠 조회한다.** 그룹화가 어긋나면 한 범위만 조회되고 나머지는
    /// 오류 없이 빈 값이 된다.
    #[test]
    fn instances_are_grouped_by_account_and_region() {
        let groups = by_scope(&[
            instance_in("ap-northeast-2", "a", Engine::Mysql, None),
            instance_in("us-east-1", "b", Engine::Mysql, None),
            instance_in("ap-northeast-2", "c", Engine::Mysql, None),
        ]);
        assert_eq!(groups.len(), 2);
        let account = "123456789012".to_string();
        assert_eq!(groups[&(account.clone(), "ap-northeast-2".into())].len(), 2);
        assert_eq!(groups[&(account, "us-east-1".into())].len(), 1);
    }

    /// 캐시 키는 **창으로 정렬**된다 — 같은 창 안의 여러 요청이 1회로 접힌다.
    #[test]
    fn windows_align_so_requests_collapse() {
        let p = 900;
        let a = align(1_787_000_123_456, p);
        let b = align(1_787_000_123_456 + 60_000, p);
        assert_eq!(a, b, "같은 15분 창인데 키가 다르다");
        let c = align(1_787_000_123_456 + 16 * 60_000, p);
        assert_ne!(a, c, "창이 넘어갔는데 키가 같다");
        assert_eq!(a % (p * 1000), 0);
    }
}
