//! 개별 슬로우 쿼리 레코드 ([04 §2.3](../../../docs/04-data-model.md)).

use crate::env::Env;
use crate::ids::{ClusterId, DurBucket, InstanceId, RecordId};
use crate::instance::Engine;
use crate::time::EpochMs;
use dbmon_normalize::StatementType;
use serde::{Deserialize, Serialize};

/// 리터럴 저장·노출 정책 ([08 §6.1](../../../docs/08-security-auth.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteralPolicy {
    /// 원문 저장, 권한에 따라 노출.
    Full,
    /// 원문 저장, operator 이상 + 감사 로그. **prd 기본값** (OPEN-Q-15).
    FullRestricted,
    /// `?` 치환 저장. **되돌릴 수 없다.**
    Masked,
    /// SQL 텍스트 미저장.
    Off,
}

impl LiteralPolicy {
    /// 제한 강도. 큰 값이 더 제한적이다.
    ///
    /// **선언 순서에 의존하지 않는다.** derive 한 `Ord` 는 선언 순서를 따르는데
    /// 이 enum 은 `Full` 이 먼저라 `min()` 이 **가장 느슨한** 값을 준다. 보안 결정을
    /// 선언 순서에 맡기면 누가 variant 를 재배치하는 순간 fail-open 이 된다.
    pub fn restrictiveness(self) -> u8 {
        match self {
            Self::Full => 0,
            Self::FullRestricted => 1,
            Self::Masked => 2,
            Self::Off => 3,
        }
    }

    /// 둘 중 **더 제한적인** 쪽. 정책이 갈렸을 때의 안전한 방향이다.
    pub fn more_restrictive(self, other: Self) -> Self {
        if other.restrictiveness() > self.restrictiveness() {
            other
        } else {
            self
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::FullRestricted => "full_restricted",
            Self::Masked => "masked",
            Self::Off => "off",
        }
    }

    /// 원문(리터럴 포함)을 저장하는 정책인가.
    pub fn stores_literals(self) -> bool {
        matches!(self, Self::Full | Self::FullRestricted)
    }

    /// SQL 텍스트를 아예 저장하지 않는가.
    pub fn stores_nothing(self) -> bool {
        self == Self::Off
    }

    /// 마스킹 후조건 검증이 필수인 정책인가.
    ///
    /// `masked` 는 보안 통제이므로 리터럴 잔존 0 을 강제해야 한다(T-36).
    pub fn requires_postcondition(self) -> bool {
        self == Self::Masked
    }

    /// 후조건 검증 실패 시 강등 대상. 마스킹을 신뢰할 수 없으면 저장하지 않는다.
    pub fn degrade(self) -> Self {
        Self::Off
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    /// 실시간 processlist 캡처.
    Processlist,
    /// CloudWatch 슬로우로그.
    Slowlog,
    /// 두 소스가 병합됨.
    Merged,
    /// 31일 초과 백필 — DynamoDB 를 거치지 않고 Iceberg 로 직접 간 행 (F12).
    Backfill,
}

impl CaptureSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Processlist => "processlist",
            Self::Slowlog => "slowlog",
            Self::Merged => "merged",
            Self::Backfill => "backfill",
        }
    }
}

/// 실행계획 출처. 순서가 **우선순위**다 (병합 시 큰 쪽이 이긴다).
///
/// # M1-4 실측이 우선순위를 뒤집었다
///
/// `ForConnection` 이 가장 정확하지만 **RDS 에서는 쓸 수 없다.**
/// `EXPLAIN ... FOR CONNECTION` 은 타인 커넥션에 대해 **정적 전역 권한 전체**를 요구하고,
/// RDS 는 마스터 유저에게도 `SUPER`·`FILE`·`SHUTDOWN` 을 주지 않는다
/// ([19-m1-findings.md](../../../docs/19-m1-findings.md) B).
/// → 실질 기본 경로는 `Rerun` 이다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSource {
    /// 플랜 없음.
    #[default]
    None,
    /// **근사 플랜.** DML 의 대상·조건절을 `SELECT` 로 바꿔 얻었다.
    ///
    /// 읽기 전용 계정은 `EXPLAIN UPDATE` 를 실행할 수 없다(`ERROR 1142`).
    /// 행을 찾아가는 접근 경로는 같으므로 튜닝에 필요한 정보는 보존되지만,
    /// 쓰기 단계(인덱스 갱신·트리거)는 플랜에 나타나지 않는다.
    RerunAsSelect,
    /// 원문을 그대로 `EXPLAIN` 재실행. `SELECT` 권한만으로 된다. **기본 경로.**
    Rerun,
    /// 실행 중 `EXPLAIN FOR CONNECTION`. 가장 정확하지만 RDS 에서는 권한이 나오지 않는다.
    ForConnection,
}

impl PlanSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RerunAsSelect => "rerun_as_select",
            Self::Rerun => "rerun",
            Self::ForConnection => "for_connection",
        }
    }

    /// 실제 실행된 문장의 플랜인가. `false` 면 UI 에 "근사" 배지를 붙여야 한다.
    pub fn is_exact(self) -> bool {
        matches!(self, Self::ForConnection | Self::Rerun)
    }
}

/// `duration_ms` 를 어디서 얻었는가. 정확도 순서가 **우선순위**다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationSource {
    /// `PROCESSLIST.TIME` — 초 단위 근사. 최대 1초 오차.
    Polled,
    /// `events_statements_current.TIMER_WAIT` — 피코초 정밀.
    Timer,
    /// 슬로우로그 `Query_time` — 완료 후 기록된 정확값.
    Slowlog,
}

impl DurationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Polled => "polled",
            Self::Timer => "timer",
            Self::Slowlog => "slowlog",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlowQueryState {
    /// 실행 중. 선행 저장된 상태 ([05 §4.3](../../../docs/05-collector.md)).
    InFlight,
    /// 종료를 관측해 확정됨.
    Finalized,
    /// 소유 워커가 사라져 추적이 끊김 (F4). 유령 쿼리가 아니라 **관측 중단 사실**이다.
    Abandoned,
}

impl SlowQueryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::Finalized => "finalized",
            Self::Abandoned => "abandoned",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finalized | Self::Abandoned)
    }
}

/// `events_statements_current` 에서 온 정확 지표.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExecStats {
    pub rows_examined: Option<u64>,
    pub rows_sent: Option<u64>,
    pub rows_affected: Option<u64>,
    pub lock_time_ms: Option<i64>,
    pub tmp_tables: Option<u64>,
    pub tmp_disk_tables: Option<u64>,
    pub sort_merge_passes: Option<u64>,
    pub no_index_used: Option<bool>,
    pub no_good_index_used: Option<bool>,
    pub full_join: Option<bool>,
}

/// 실행계획 관련 필드 묶음.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlanBundle {
    /// **리터럴이 마스킹된** 플랜 JSON (FR-PLN-09).
    pub normalized_json: Option<String>,
    /// 300KB 초과 시 S3 오프로드 키. 만료 티어를 접두에 포함한다 (F27).
    pub s3_key: Option<String>,
    pub format_version: Option<String>,
    pub tree_text: Option<String>,
    pub source: PlanSource,
    pub error: Option<String>,
    pub fingerprint: Option<String>,
    pub referenced_tables: Vec<String>,
}

impl PlanBundle {
    pub fn has_plan(&self) -> bool {
        self.normalized_json.is_some() || self.s3_key.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlowQuery {
    pub record_id: RecordId,
    pub instance_id: InstanceId,
    pub cluster_id: Option<ClusterId>,
    pub env: Env,
    pub engine: Engine,
    pub engine_version: String,
    pub state: SlowQueryState,

    pub thread_id: u64,
    pub schema_name: Option<String>,
    pub db_user: Option<String>,
    pub db_host: Option<String>,

    pub started_at_ms: EpochMs,
    /// `TIMER_WAIT` 기반 정밀 시각. 있으면 표시·정렬에 쓴다.
    pub started_at_ms_precise: Option<EpochMs>,
    pub ended_at_ms: Option<EpochMs>,
    pub captured_at_ms: EpochMs,
    pub duration_ms: i64,
    pub duration_source: DurationSource,

    /// 정책이 적용된 SQL 텍스트. `Off` 정책이면 `None`.
    pub sql_text: Option<String>,
    pub sql_text_truncated: bool,
    /// 4바이트 문자가 `?` 로 **손실됐을 수 있다.**
    ///
    /// `information_schema.PROCESSLIST.INFO` 는 `utf8mb3` 라 이모지·확장 CJK 를
    /// `?` 한 바이트로 치환한다([19 §A-2](../../../docs/19-m1-findings.md) 실측).
    /// 무손실 소스(`events_statements_current.SQL_TEXT`)는 1,024바이트에서 잘리므로,
    /// 긴 SQL 에서는 **손실본을 쓸 수밖에 없다.**
    ///
    /// 이 플래그가 없으면 운영자가 저장된 SQL 을 복사해 재현할 때 **다른 쿼리**가 되고
    /// 왜 다른지 알 수 없다. UI 는 이 값이 참이면 경고를 붙인다.
    pub sql_text_lossy: bool,
    /// **선행 저장 시점의 정책을 고정한다** (F2 / A3-2). 확정 시에도 이 정책을 쓴다.
    pub literal_policy: LiteralPolicy,
    pub literal_policy_at_ms: EpochMs,

    pub app_digest: String,
    pub digest_algo_version: u32,
    pub mysql_digest: Option<String>,
    pub statement_type: StatementType,
    /// 프로시저 내부 문장이다 (`NESTING_EVENT_TYPE = STATEMENT`).
    pub is_nested: bool,

    pub stats: ExecStats,
    pub plan: PlanBundle,
    pub capture_source: CaptureSource,

    /// 소유 워커 — 고아 `in_flight` 정리의 기준 (F4).
    pub owner_worker: Option<String>,
    pub owner_epoch: Option<u64>,
    pub last_seen_at_ms: Option<EpochMs>,
    pub abandoned_reason: Option<String>,
    /// `TRACKING` 최대 지속시간을 넘겨 강제 확정됨.
    pub long_running: bool,
}

impl SlowQuery {
    pub fn dur_bucket(&self) -> DurBucket {
        DurBucket::from_duration_ms(self.duration_ms)
    }

    /// 정렬·표시에 쓰는 시각. 정밀값이 있으면 그쪽을 쓴다.
    pub fn display_started_at_ms(&self) -> EpochMs {
        self.started_at_ms_precise.unwrap_or(self.started_at_ms)
    }
}

/// 다이제스트를 계산할 수 없을 때 쓰는 **자리표** 접두어.
///
/// 심층 조회가 상한(`deep_probe_limit`)에 걸리거나 실패하면 SQL 텍스트가 없어
/// `app_digest` 를 계산할 수 없다. 그때 `unknown-<thread_id>` 를 넣는다.
///
/// **병합에서 이 값은 "없음" 과 같이 취급해야 한다.** 값으로 취급하면 나중에 도착한
/// 슬로우로그의 진짜 다이제스트를 이겨 그 실행이 영구히 어느 그룹에도 속하지 않는다.
pub const UNKNOWN_DIGEST_PREFIX: &str = "unknown-";

/// `duration_source` 별 정확도 순위. 병합 시 큰 쪽이 이긴다.
pub fn duration_rank(s: DurationSource) -> u8 {
    match s {
        DurationSource::Polled => 0,
        DurationSource::Timer => 1,
        DurationSource::Slowlog => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **선언 순서에 의존하지 않는다는 것**을 고정한다.
    #[test]
    fn restrictiveness_order_is_explicit_not_declaration_order() {
        use LiteralPolicy::*;
        assert!(Off.restrictiveness() > Masked.restrictiveness());
        assert!(Masked.restrictiveness() > FullRestricted.restrictiveness());
        assert!(FullRestricted.restrictiveness() > Full.restrictiveness());

        // derive 한 Ord 는 **반대 방향**이다. 이 단정이 그 함정을 기록한다.
        assert!(Full < Masked, "선언 순서상 Full 이 작다");
        assert_eq!(
            Full.min(Masked),
            Full,
            "min() 은 가장 느슨한 값을 준다 — 보안 결정에 쓰면 안 된다"
        );

        // 교환법칙.
        for (a, b) in [
            (Full, Masked),
            (Masked, Full),
            (Off, Full),
            (FullRestricted, Masked),
        ] {
            assert_eq!(a.more_restrictive(b), b.more_restrictive(a));
        }
        assert_eq!(Full.more_restrictive(Masked), Masked);
        assert_eq!(Off.more_restrictive(Full), Off);
    }

    #[test]
    fn literal_policy_semantics() {
        assert!(LiteralPolicy::Full.stores_literals());
        assert!(LiteralPolicy::FullRestricted.stores_literals());
        assert!(!LiteralPolicy::Masked.stores_literals());
        assert!(!LiteralPolicy::Off.stores_literals());

        assert!(LiteralPolicy::Masked.requires_postcondition());
        assert!(!LiteralPolicy::Full.requires_postcondition());
        assert_eq!(LiteralPolicy::Masked.degrade(), LiteralPolicy::Off);
        assert!(LiteralPolicy::Off.stores_nothing());
    }

    #[test]
    fn plan_source_priority_ordering() {
        // 병합이 이 순서에 의존한다.
        assert!(PlanSource::ForConnection > PlanSource::Rerun);
        assert!(PlanSource::Rerun > PlanSource::RerunAsSelect);
        assert!(PlanSource::RerunAsSelect > PlanSource::None);
    }

    #[test]
    fn only_real_statement_plans_are_exact() {
        assert!(PlanSource::ForConnection.is_exact());
        assert!(PlanSource::Rerun.is_exact());
        assert!(
            !PlanSource::RerunAsSelect.is_exact(),
            "DML→SELECT 변환은 근사다"
        );
        assert!(!PlanSource::None.is_exact());
    }

    #[test]
    fn duration_source_priority_ordering() {
        assert!(duration_rank(DurationSource::Slowlog) > duration_rank(DurationSource::Timer));
        assert!(duration_rank(DurationSource::Timer) > duration_rank(DurationSource::Polled));
    }

    #[test]
    fn state_terminality() {
        assert!(!SlowQueryState::InFlight.is_terminal());
        assert!(SlowQueryState::Finalized.is_terminal());
        assert!(SlowQueryState::Abandoned.is_terminal());
    }
}
