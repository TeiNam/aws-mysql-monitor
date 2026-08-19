//! 실시간 지표 수집 (FR-MET-01, [06 §2.3](../../../docs/06-discovery-metrics.md)).
//!
//! **CloudWatch 를 쓰지 않는다.** 플릿 카드의 지표는 전부 자체 수집이다 —
//! CloudWatch 는 1분 해상도에 1~3분 지연이고 호출 비용이 붙는다.
//! `performance_schema.global_status` 를 5초 주기로 읽어 유도한다.

pub mod derive;
pub mod sampler;

pub use derive::{LiveMetrics, StatusSample, derive};
pub use sampler::MetricsSampler;
