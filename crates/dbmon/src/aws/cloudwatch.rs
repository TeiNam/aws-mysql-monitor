//! CloudWatch 메트릭 어댑터 (`GetMetricData`) — [06 §2](../../../../docs/06-discovery-metrics.md).
//!
//! # `GetMetricStatistics` 를 쓰지 않는다
//!
//! 그건 **메트릭당 1호출**이다. 500대 × 3메트릭이면 호출 1,500회고, 조절(throttle)에
//! 걸린다. `GetMetricData` 는 한 호출에 최대 500 쿼리를 담는다.
//!
//! # 클러스터 메트릭은 클러스터당 1회다
//!
//! `AuroraVolumeBytesLeftTotal` 은 클러스터 레벨이다. 인스턴스마다 요청하면 **같은 값에
//! 인스턴스 수만큼 과금된다.** 그래서 요청을 만들 때 클러스터 차원은 중복을 제거한다.
//!
//! # 없는 메트릭은 요청하지 않는다
//!
//! 빈 값이 와도 **요청 수로 과금된다.** 어떤 메트릭을 물어볼지는
//! [`dbmon_core::cw_metrics`] 가 엔진으로 정하고, 이 어댑터는 그 목록을 그대로 보낸다.

use std::collections::BTreeMap;

use aws_sdk_cloudwatch::Client;
use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat, ScanBy};
use dbmon_core::cw_metrics::{MAX_QUERIES_PER_CALL, MetricScope, MetricSpec};
use dbmon_core::error::Result;
use dbmon_core::time::EpochMs;

/// SDK 오류를 도메인 오류로. **의존 서비스 이름을 정확히 적는다** — `dynamodb` 로
/// 뭉개면 장애 원인이 저장소처럼 보인다(F25 분류가 그 이름을 본다).
fn map_cw_err<E: std::fmt::Debug, R: std::fmt::Debug>(
    e: aws_sdk_cloudwatch::error::SdkError<E, R>,
) -> dbmon_core::error::DomainError {
    dbmon_core::error::DomainError::Unavailable {
        dependency: "cloudwatch",
        reason: crate::telemetry::scrub(&format!("{e:?}")),
    }
}

/// RDS 메트릭 네임스페이스. Aurora MySQL 도 같은 네임스페이스를 쓴다.
const NAMESPACE: &str = "AWS/RDS";

/// 한 메트릭의 시계열. **비어 있을 수 있다** — 그 인스턴스에 아직 데이터가 없거나
/// period 가 보관 정책과 맞지 않는 경우다.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// 요청할 때 쓴 키 (`<대상>|<메트릭>`). 응답 순서에 의존하지 않기 위해 되돌려 받는다.
    pub key: String,
    pub timestamps_ms: Vec<EpochMs>,
    pub values: Vec<f64>,
}

impl Series {
    /// 가장 최근 값. 플릿 표는 이 한 점만 쓴다.
    pub fn latest(&self) -> Option<f64> {
        self.values.first().copied()
    }
}

/// 하나의 조회 대상. 인스턴스 식별자 또는 클러스터 식별자다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    pub scope: MetricScope,
    /// `DBInstanceIdentifier` 또는 `DBClusterIdentifier` 값.
    pub identifier: String,
}

pub struct MetricFetcher {
    client: Client,
}

impl MetricFetcher {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// `(대상, 메트릭)` 쌍들을 한 번에 조회한다.
    ///
    /// **키가 응답을 짝지어 준다.** `GetMetricData` 는 요청 순서를 보장하지 않고 일부
    /// 쿼리만 결과가 올 수도 있으므로, `Id` 에 우리 인덱스를 심고 그걸로 되돌린다.
    /// (`Id` 는 소문자·숫자·`_` 만 허용해서 식별자를 그대로 쓸 수 없다.)
    pub async fn fetch(
        &self,
        pairs: &[(Target, MetricSpec)],
        period_secs: i64,
        from_ms: EpochMs,
        to_ms: EpochMs,
        max_datapoints: i32,
    ) -> Result<BTreeMap<String, Series>> {
        let mut out = BTreeMap::new();
        // **500개씩 나눈다.** 넘으면 API 가 거부한다.
        for chunk in pairs.chunks(MAX_QUERIES_PER_CALL) {
            let mut req = self
                .client
                .get_metric_data()
                .start_time(to_aws_time(from_ms))
                .end_time(to_aws_time(to_ms))
                // 최신부터 받는다 — 플릿 표는 첫 점만 쓴다.
                .scan_by(ScanBy::TimestampDescending)
                .max_datapoints(max_datapoints);

            for (i, (target, spec)) in chunk.iter().enumerate() {
                let dim = Dimension::builder()
                    .name(match target.scope {
                        MetricScope::Instance => "DBInstanceIdentifier",
                        MetricScope::Cluster => "DBClusterIdentifier",
                    })
                    .value(target.identifier.clone())
                    .build();
                let stat = MetricStat::builder()
                    .metric(
                        Metric::builder()
                            .namespace(NAMESPACE)
                            .metric_name(spec.name)
                            .dimensions(dim)
                            .build(),
                    )
                    .period(period_secs as i32)
                    .stat(spec.stat.as_str())
                    .build();
                req = req.metric_data_queries(
                    MetricDataQuery::builder()
                        .id(format!("q{i}"))
                        // 응답에 담겨 돌아온다 — 이걸로 우리 키를 되찾는다.
                        .label(series_key(target, spec))
                        .metric_stat(stat)
                        .build(),
                );
            }

            let res = req.send().await.map_err(map_cw_err)?;
            for r in res.metric_data_results() {
                // `label` 이 없으면 짝지을 수 없다 — 버리고 로그를 남긴다. 조용히
                // 빈 값으로 두면 화면이 "데이터 없음" 으로 표시해 원인을 가린다.
                let Some(key) = r.label().map(str::to_string) else {
                    tracing::warn!("GetMetricData 결과에 label 이 없다 — 짝지을 수 없다");
                    continue;
                };
                out.insert(
                    key.clone(),
                    Series {
                        key,
                        timestamps_ms: r
                            .timestamps()
                            .iter()
                            .map(|t| t.to_millis().unwrap_or(0))
                            .collect(),
                        values: r.values().to_vec(),
                    },
                );
            }
            // **부분 실패를 삼키지 않는다.** 메시지가 있으면 남긴다(잘못된 period,
            // 없는 메트릭 등이 여기로 온다).
            for m in res.messages() {
                tracing::warn!(
                    code = m.code().unwrap_or("?"),
                    value = m.value().unwrap_or("?"),
                    "GetMetricData 경고"
                );
            }
        }
        Ok(out)
    }
}

/// 응답을 되돌려 짝지을 키. `label` 로 왕복한다.
pub fn series_key(target: &Target, spec: &MetricSpec) -> String {
    format!("{}|{}", target.identifier, spec.name)
}

fn to_aws_time(ms: EpochMs) -> aws_sdk_cloudwatch::primitives::DateTime {
    aws_sdk_cloudwatch::primitives::DateTime::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::cw_metrics::{Stat, detail_metrics, fleet_metrics};
    use dbmon_core::instance::Engine;

    fn target(id: &str, scope: MetricScope) -> Target {
        Target {
            scope,
            identifier: id.to_string(),
        }
    }

    /// 키는 **대상과 메트릭을 함께** 담아야 한다. 메트릭 이름만 쓰면 인스턴스가 여럿일 때
    /// 서로의 결과를 덮는다.
    #[test]
    fn series_key_separates_targets_and_metrics() {
        let specs = fleet_metrics(Engine::Mysql);
        let a = series_key(&target("orders-01", MetricScope::Instance), &specs[0]);
        let b = series_key(&target("orders-02", MetricScope::Instance), &specs[0]);
        let c = series_key(&target("orders-01", MetricScope::Instance), &specs[1]);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, "orders-01|CPUUtilization");
    }

    /// **클러스터 메트릭을 인스턴스마다 요청하면 같은 값에 중복 과금된다.**
    /// 호출부가 중복을 제거해야 하고, 그 판정 근거가 `scope` 다.
    #[test]
    fn cluster_scoped_metrics_are_identifiable() {
        let vol = detail_metrics(Engine::AuroraMysql)
            .into_iter()
            .find(|s| s.scope == MetricScope::Cluster)
            .expect("Aurora 에 클러스터 메트릭이 있다");
        assert_eq!(vol.name, "AuroraVolumeBytesLeftTotal");
        assert_eq!(vol.stat, Stat::Minimum);
    }

    /// 최신 점을 먼저 받는다(`TimestampDescending`) — 플릿 표가 첫 값을 쓴다.
    #[test]
    fn latest_reads_the_first_point() {
        let s = Series {
            key: "x|CPUUtilization".into(),
            timestamps_ms: vec![3_000, 2_000, 1_000],
            values: vec![9.5, 8.0, 7.0],
        };
        assert_eq!(s.latest(), Some(9.5));
        let empty = Series {
            key: "y|CPUUtilization".into(),
            timestamps_ms: vec![],
            values: vec![],
        };
        assert_eq!(empty.latest(), None);
    }
}
