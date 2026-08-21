//! CloudWatch 메트릭 카탈로그와 조회 규칙 ([06 §2](../../../docs/06-discovery-metrics.md)).
//!
//! # 왜 엔진별로 갈라야 하는가 (비용 문제다)
//!
//! **없는 메트릭을 요청하면 요청 수로 과금되면서 결과는 빈 값이다.** `FreeStorageSpace`
//! 는 Aurora 에 없고, `Deadlocks`·`EngineUptime` 은 RDS MySQL 에 없다. 하나의 세트로
//! 통일하면 절반이 빈 값인 요청에 계속 돈을 낸다.
//!
//! 그래서 목록을 [`Engine`] 으로만 얻게 만든다 — 호출부가 섞을 방법이 없다.
//!
//! # 왜 플릿과 상세를 나누는가
//!
//! 순진하게 전량 폴링하면 이 항목이 전체 비용을 지배한다([06 §2.3]):
//!
//! ```text
//! 500대 × 15메트릭 × 60초 폴링 → 월 10.8M 메트릭요청 → $3,240/월
//! ```
//!
//! 그래서 두 계층이다. **플릿은 엔진별 3개 × 15분**($43/월), **상세는 화면을 보고 있을
//! 때만** 60초 주기·캐시. 연결 수·QPS 처럼 자체 수집으로 얻는 값은 **아예 요청하지
//! 않는다** — 그쪽이 더 신선하고(5초 vs CloudWatch 1~3분 지연) 무료다.

use crate::instance::Engine;

/// 이 메트릭을 어느 차원으로 요청하는가.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetricScope {
    /// `DBInstanceIdentifier`
    Instance,
    /// `DBClusterIdentifier` — **클러스터당 1회만** 요청한다. 인스턴스마다 요청하면
    /// 같은 값에 인스턴스 수만큼 과금된다.
    Cluster,
}

/// 통계. `GetMetricData` 의 `Stat` 에 그대로 들어간다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    Average,
    Maximum,
    Minimum,
}

impl Stat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Average => "Average",
            Self::Maximum => "Maximum",
            Self::Minimum => "Minimum",
        }
    }
}

/// 화면이 값을 어떻게 표시할지 정하는 단위.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricUnit {
    Percent,
    Bytes,
    BytesPerSecond,
    Count,
    CountPerSecond,
    Seconds,
    Milliseconds,
}

impl MetricUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Percent => "percent",
            Self::Bytes => "bytes",
            Self::BytesPerSecond => "bytes_per_second",
            Self::Count => "count",
            Self::CountPerSecond => "count_per_second",
            Self::Seconds => "seconds",
            Self::Milliseconds => "milliseconds",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricSpec {
    pub name: &'static str,
    pub unit: MetricUnit,
    pub stat: Stat,
    pub scope: MetricScope,
    /// 화면에 쓰는 짧은 이름. 표 머리에 들어간다.
    pub label: &'static str,
}

const fn m(
    name: &'static str,
    label: &'static str,
    unit: MetricUnit,
    stat: Stat,
) -> MetricSpec {
    MetricSpec {
        name,
        label,
        unit,
        stat,
        scope: MetricScope::Instance,
    }
}

const fn cluster(
    name: &'static str,
    label: &'static str,
    unit: MetricUnit,
    stat: Stat,
) -> MetricSpec {
    MetricSpec {
        name,
        label,
        unit,
        stat,
        scope: MetricScope::Cluster,
    }
}

use MetricUnit::*;
use Stat::{Average, Maximum, Minimum};

/// **플릿 표에 쓰는 최소 세트.** 15분 주기로 전 인스턴스에 돈다 → 개수가 곧 비용이다.
///
/// 연결 수·QPS·스레드·락 대기는 여기 없다. 자체 수집(`global_status`)이 더 신선하고
/// 무료다 ([06 §2.3] "자체 수집으로 대체한 것").
const FLEET_RDS: &[MetricSpec] = &[
    m("CPUUtilization", "CPU", Percent, Average),
    m("FreeableMemory", "여유 메모리", Bytes, Minimum),
    // Aurora 에는 없다. 이 한 줄이 엔진을 갈라야 하는 이유다.
    m("FreeStorageSpace", "여유 스토리지", Bytes, Minimum),
];

const FLEET_AURORA: &[MetricSpec] = &[
    m("CPUUtilization", "CPU", Percent, Average),
    m("FreeableMemory", "여유 메모리", Bytes, Minimum),
    // 볼륨 한도는 클러스터 레벨이다 — 클러스터당 1회.
    cluster("AuroraVolumeBytesLeftTotal", "볼륨 여유", Bytes, Minimum),
];

/// 두 엔진에 **모두 있는** 메트릭. 상세 화면의 기본 블록이다.
const DETAIL_COMMON: &[MetricSpec] = &[
    m("CPUUtilization", "CPU", Percent, Average),
    m("DatabaseConnections", "연결 수", Count, Maximum),
    m("FreeableMemory", "여유 메모리", Bytes, Minimum),
    m("ReadIOPS", "읽기 IOPS", CountPerSecond, Average),
    m("WriteIOPS", "쓰기 IOPS", CountPerSecond, Average),
    m("ReadLatency", "읽기 지연", Seconds, Average),
    m("WriteLatency", "쓰기 지연", Seconds, Average),
    m("ReadThroughput", "읽기 처리량", BytesPerSecond, Average),
    m("WriteThroughput", "쓰기 처리량", BytesPerSecond, Average),
    m("NetworkReceiveThroughput", "수신", BytesPerSecond, Average),
    m("NetworkTransmitThroughput", "송신", BytesPerSecond, Average),
    m("DiskQueueDepth", "디스크 큐", Count, Average),
    m("SwapUsage", "스왑", Bytes, Average),
];

/// RDS MySQL 에만 있는 것.
const DETAIL_RDS: &[MetricSpec] = &[
    m("FreeStorageSpace", "여유 스토리지", Bytes, Minimum),
    m("BinLogDiskUsage", "바이너리 로그", Bytes, Average),
    m("ReplicaLag", "복제 지연", Seconds, Maximum),
    m("BurstBalance", "버스트 잔량", Percent, Minimum),
];

/// Aurora MySQL 에만 있는 것.
///
/// `Deadlocks`·`EngineUptime` 이 여기 있다 — **RDS MySQL 에는 없다.** 데드락은
/// `SHOW ENGINE INNODB STATUS` 파싱으로, 재시작은 자체 수집 `Uptime` 델타로 얻는다.
const DETAIL_AURORA: &[MetricSpec] = &[
    m("EngineCPUUtilization", "엔진 CPU", Percent, Average),
    m("BufferCacheHitRatio", "버퍼 캐시 적중", Percent, Average),
    m("SelectLatency", "SELECT 지연", Milliseconds, Average),
    m("DMLLatency", "DML 지연", Milliseconds, Average),
    m("CommitLatency", "커밋 지연", Milliseconds, Average),
    m("DDLLatency", "DDL 지연", Milliseconds, Average),
    m("SelectThroughput", "SELECT 처리량", CountPerSecond, Average),
    m("DMLThroughput", "DML 처리량", CountPerSecond, Average),
    m("CommitThroughput", "커밋 처리량", CountPerSecond, Average),
    m("Deadlocks", "데드락", CountPerSecond, Average),
    m("EngineUptime", "엔진 가동시간", Seconds, Maximum),
    m("AuroraReplicaLag", "리더 지연", Milliseconds, Maximum),
    m("RollbackSegmentHistoryListLength", "퍼지 지연", Count, Maximum),
    cluster("AuroraVolumeBytesLeftTotal", "볼륨 여유", Bytes, Minimum),
];

/// 플릿 표용 세트. **엔진으로만 얻는다.**
pub fn fleet_metrics(engine: Engine) -> &'static [MetricSpec] {
    match engine {
        Engine::Mysql => FLEET_RDS,
        Engine::AuroraMysql => FLEET_AURORA,
    }
}

/// 상세 화면용 전체 세트 (공통 + 엔진 전용).
pub fn detail_metrics(engine: Engine) -> Vec<MetricSpec> {
    let extra = match engine {
        Engine::Mysql => DETAIL_RDS,
        Engine::AuroraMysql => DETAIL_AURORA,
    };
    DETAIL_COMMON.iter().chain(extra).copied().collect()
}

/// `GetMetricData` 한 호출의 쿼리 상한. 넘으면 나눠 보낸다.
pub const MAX_QUERIES_PER_CALL: usize = 500;

/// 플릿 폴링 주기 (15분). **개수 × 주기가 곧 비용이다** ([06 §2.3]).
pub const FLEET_PERIOD_SECS: i64 = 900;

/// 상세 화면 캐시 유효 기간 (60초). 여러 사람이 같은 화면을 봐도 1회만 호출한다.
pub const DETAIL_CACHE_SECS: i64 = 60;

/// period 자동 선택 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodChoice {
    pub period_secs: i64,
    /// 요청 범위 때문에 **올려 잡았다.** 화면이 그 사실을 표시해야 한다.
    pub adjusted: bool,
}

/// 조회 범위에 맞는 period 를 고른다.
///
/// # 두 규칙을 함께 봐야 한다
///
/// 1. **데이터포인트 수**를 적당히 유지하는 규칙(범위가 넓으면 period 를 키운다)
/// 2. **CloudWatch 의 최소 period 제약** — 이건 *조회 시작 시각*이 정한다:
///
/// | 시작 시각 | 최소 period |
/// |---|---|
/// | 3시간 ~ 15일 전 | 60초 |
/// | 15일 ~ 63일 전 | **300초** |
/// | 63일 초과 | **3600초** |
///
/// 초기 설계는 "63일 초과는 5분, 15개월은 1시간" 이라고만 적어 **15~63일 구간의 300초
/// 제약을 빠뜨렸다.** 그대로 구현하면 30일 전 구간에 60초를 요청해 **빈 결과**를 받는다
/// (오류가 아니라 빈 값이라 원인을 찾기 어렵다). 그래서 두 규칙 중 **큰 쪽**을 쓴다.
pub fn period_for(now_ms: i64, from_ms: i64, to_ms: i64) -> PeriodChoice {
    const MIN: i64 = 60;
    let span_secs = ((to_ms - from_ms).max(0)) / 1000;
    // ① 범위 기반 (데이터포인트 수 통제)
    let by_span = match span_secs {
        s if s <= 3 * 3600 => 60,
        s if s <= 12 * 3600 => 60,
        s if s <= 3 * 86_400 => 300,
        s if s <= 14 * 86_400 => 900,
        s if s <= 92 * 86_400 => 3600,
        _ => 21_600,
    };
    // ② 시작 시각 기반 (CloudWatch 제약)
    let age_secs = ((now_ms - from_ms).max(0)) / 1000;
    let by_age = match age_secs {
        a if a <= 15 * 86_400 => MIN,
        a if a <= 63 * 86_400 => 300,
        _ => 3600,
    };
    let period_secs = by_span.max(by_age);
    PeriodChoice {
        period_secs,
        adjusted: period_secs > by_span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_787_000_000_000;

    /// **엔진에 없는 메트릭을 세트에 넣으면 빈 값에 과금된다.**
    #[test]
    fn engine_sets_do_not_include_metrics_the_engine_lacks() {
        let rds: Vec<&str> = fleet_metrics(Engine::Mysql)
            .iter()
            .chain(detail_metrics(Engine::Mysql).iter())
            .map(|s| s.name)
            .collect();
        for absent in [
            "AuroraVolumeBytesLeftTotal",
            "Deadlocks",
            "EngineUptime",
            "BufferCacheHitRatio",
            "AuroraReplicaLag",
        ] {
            assert!(!rds.contains(&absent), "RDS 세트에 {absent} 가 들어 있다");
        }

        let aurora: Vec<&str> = fleet_metrics(Engine::AuroraMysql)
            .iter()
            .chain(detail_metrics(Engine::AuroraMysql).iter())
            .map(|s| s.name)
            .collect();
        for absent in ["FreeStorageSpace", "BinLogDiskUsage", "BurstBalance"] {
            assert!(!aurora.contains(&absent), "Aurora 세트에 {absent} 가 들어 있다");
        }
    }

    /// 플릿 세트는 **작아야 한다** — 개수 × 500대 × 15분이 곧 청구서다.
    #[test]
    fn fleet_sets_stay_small() {
        for e in [Engine::Mysql, Engine::AuroraMysql] {
            let n = fleet_metrics(e).len();
            assert!(n <= 3, "{e:?} 플릿 세트가 {n}개다 — 3개를 넘으면 비용 설계가 깨진다");
        }
    }

    /// 클러스터 레벨 메트릭은 **클러스터당 1회**여야 한다. 인스턴스 차원으로 요청하면
    /// 빈 값이 오고, 인스턴스마다 요청하면 같은 값에 중복 과금된다.
    #[test]
    fn volume_metric_is_cluster_scoped() {
        let vol = detail_metrics(Engine::AuroraMysql)
            .into_iter()
            .find(|s| s.name == "AuroraVolumeBytesLeftTotal")
            .expect("Aurora 세트에 있어야 한다");
        assert_eq!(vol.scope, MetricScope::Cluster);
        // 인스턴스 차원 메트릭은 전부 Instance 다.
        assert!(
            detail_metrics(Engine::Mysql)
                .iter()
                .all(|s| s.scope == MetricScope::Instance)
        );
    }

    /// **15~63일 구간의 300초 제약.** 문서가 빠뜨렸던 규칙이고, 어기면 오류가 아니라
    /// **빈 결과**가 온다.
    #[test]
    fn period_respects_cloudwatch_minimums_by_age() {
        // 최근 1시간 → 60초
        let p = period_for(NOW, NOW - 3_600_000, NOW);
        assert_eq!((p.period_secs, p.adjusted), (60, false));

        // 30일 전의 1시간 구간: 범위로는 60초지만 **300초로 올려야 한다**
        let start = NOW - 30 * DAY;
        let p = period_for(NOW, start, start + 3_600_000);
        assert_eq!(p.period_secs, 300, "15~63일 구간은 300초 이상이다");
        assert!(p.adjusted, "올려 잡았으면 화면에 알려야 한다");

        // 100일 전 → 3600초
        let start = NOW - 100 * DAY;
        let p = period_for(NOW, start, start + 3_600_000);
        assert_eq!(p.period_secs, 3600);
        assert!(p.adjusted);
    }

    /// 범위가 넓으면 데이터포인트를 줄인다 — 그건 조정이 아니라 정상 선택이다.
    #[test]
    fn wide_ranges_choose_a_coarser_period_without_flagging() {
        let p = period_for(NOW, NOW - 2 * DAY, NOW);
        assert_eq!((p.period_secs, p.adjusted), (300, false));
        let p = period_for(NOW, NOW - 10 * DAY, NOW);
        assert_eq!((p.period_secs, p.adjusted), (900, false));
    }

    /// 뒤집힌 구간에도 패닉하지 않는다 — 화면이 잘못 보내도 오류로 다루면 된다.
    #[test]
    fn inverted_range_does_not_panic() {
        let p = period_for(NOW, NOW, NOW - DAY);
        assert!(p.period_secs >= 60);
    }
}
