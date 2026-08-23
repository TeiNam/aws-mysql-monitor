//! 대상 MySQL 읽기 포트.
//!
//! **이 포트는 AWS 를 모른다.** IAM 토큰 발급은 [`super::AuthTokenProvider`] 로 분리했다
//! ([02 §5](../../../docs/02-architecture.md) F31 정정).
//! 어댑터(`dbmon::mysql`)는 "비밀번호 문자열을 주는 무언가"만 알고, 그게 IAM 토큰인지
//! Secrets Manager 값인지 모른다.
//!
//! **모든 문장은 읽기 전용이다.** DDL·DML 을 실행하는 메서드가 하나도 없다
//! (FR-CAP-09, FR-AI-10 — `ANALYZE TABLE`·`FLUSH`·`TRUNCATE` 금지).

use crate::digest::DigestSnapshotRow;
use crate::error::Result;
use crate::time::EpochMs;
use async_trait::async_trait;
use std::collections::BTreeMap;

/// `performance_schema.processlist` 한 행 (경량 탐지 쿼리).
///
/// **`INFO` 를 읽지 않는다.** 여기의 `INFO` 는 1024바이트 절단본이므로 무의미하다
/// ([ADR-005](../../../docs/03-decisions.md)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub id: u64,
    pub user: Option<String>,
    pub host: Option<String>,
    pub db: Option<String>,
    pub command: Option<String>,
    /// **초 단위 정수다.** 임계값 2초면 실제 탐지 시점은 2.0~3.0초 사이.
    pub time_secs: i64,
    pub state: Option<String>,
}

/// 탐지 쿼리 결과. `db_now_ms` 를 **같은 왕복으로** 받아 시계 오프셋을 추정한다 (F14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub rows: Vec<ProcessRow>,
    /// 대상 DB 의 `NOW(6)`. 탐지 쿼리에 컬럼으로 붙이므로 추가 왕복이 없다.
    ///
    /// **행이 0개면 `None` 이다** — 정상 상태의 99% 가 그렇다. 컬럼으로 붙인 값은
    /// 행이 있어야 돌아온다. 초기 설계는 "매 tick 에 함께 읽으므로 추가 왕복이 없다"고
    /// 적었는데, 0행일 때 시각을 못 받는다는 점을 놓쳤다.
    /// → 오프셋 갱신은 [`TargetDb::db_now_ms`] 를 낮은 빈도로 호출해 보완한다.
    pub db_now_ms: Option<EpochMs>,
    /// `LIMIT` 에 걸려 잘렸다 → `detect_overflow` 메트릭.
    pub truncated: bool,
}

/// `information_schema.PROCESSLIST` 타깃 조회 — 전문 SQL.
///
/// **`INFO` 는 `varchar(21845)` 이고 65,535바이트에서 절단된다** (M1-1 실측,
/// [19 §A](../../../docs/19-m1-findings.md)). `LONGTEXT` 가 아니다.
/// `performance_schema` 의 1,024바이트보다 64배 넉넉하지만 무제한은 아니므로,
/// 정확히 65,535바이트를 받으면 `sql_text_truncated = true` 로 표시한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullSqlRow {
    pub id: u64,
    pub db: Option<String>,
    pub user: Option<String>,
    pub host: Option<String>,
    pub time_secs: i64,
    pub info: Option<String>,
}

/// `events_statements_current` 정확 지표. 타이머는 **피코초**다.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StmtCurrentRow {
    pub processlist_id: u64,
    pub thread_id: u64,
    pub event_name: Option<String>,
    pub current_schema: Option<String>,
    pub digest: Option<String>,
    pub digest_text: Option<String>,
    /// `events_statements_current.SQL_TEXT` — **무손실 `utf8mb4`** 원문.
    /// `performance_schema_max_sql_text_length`(기본 1024)로 잘린다.
    /// `information_schema.PROCESSLIST.INFO` 와 상보적이다: 이쪽은 짧지만 정확하고,
    /// 저쪽은 길지만 4바이트 문자를 잃는다 (19 §A-2).
    pub sql_text: Option<String>,
    pub timer_wait_ps: Option<u64>,
    pub lock_time_ps: Option<u64>,
    pub rows_examined: Option<u64>,
    pub rows_sent: Option<u64>,
    pub rows_affected: Option<u64>,
    pub created_tmp_tables: Option<u64>,
    pub created_tmp_disk_tables: Option<u64>,
    pub select_full_join: Option<u64>,
    pub sort_merge_passes: Option<u64>,
    pub no_index_used: Option<bool>,
    pub no_good_index_used: Option<bool>,
    /// `STATEMENT` 면 프로시저 내부 문장이다 — 양쪽 SQL 을 모두 저장하고 `is_nested=true`.
    pub nesting_event_type: Option<String>,
    /// `END_EVENT_ID` — **없으면 아직 실행 중**이다.
    ///
    /// 이 값이 이벤트 카운터의 유효성을 가른다([`Self::counters_are_final`]).
    pub end_event_id: Option<u64>,
}

impl StmtCurrentRow {
    pub fn is_nested_statement(&self) -> bool {
        self.nesting_event_type.as_deref() == Some("STATEMENT")
    }

    /// 이벤트 카운터를 **사실로 쓸 수 있는가.**
    ///
    /// # 실행 중에는 전부 0 이다 (실측)
    ///
    /// Aurora MySQL 8.0, 12만 행을 훑는 조인. 같은 문장을 실행 중과 완료 후에 읽었다:
    ///
    /// ```text
    ///                    실행 중(current)   완료 후(history)
    /// ROWS_EXAMINED               0            120,005
    /// ROWS_SENT                   0                  5
    /// NO_INDEX_USED               1                  1     ← 옵티마이즈 시점, 유효하다
    /// END_EVENT_ID             NULL                  4
    /// ```
    ///
    /// `0` 을 그대로 저장하면 **"모른다" 가 "0행을 훑었다" 는 사실이 된다.** 실제로 그렇게
    /// 저장돼 있었고(전체 4,814건 중 2,016건 = 42%), 튜닝 모델이 "검사 행 0 은 실행계획과
    /// 모순" 이라며 신뢰도를 내렸다.
    ///
    /// `CREATED_TMP_TABLES`·`SELECT_FULL_JOIN`·`SORT_MERGE_PASSES` 는 실측 쿼리에서 완료
    /// 후에도 0 이라 **시점을 증명하지 못했다.** 같은 이벤트 카운터 계열이므로 안전한 쪽
    /// (모름)으로 둔다 — "임시 테이블을 쓰지 않았다" 를 근거 없이 단정하는 것이 더 나쁘다.
    ///
    /// `NO_INDEX_USED`·`NO_GOOD_INDEX_USED`·`TIMER_WAIT`·`LOCK_TIME` 은 실행 중에도
    /// 유효하므로 이 판정과 무관하게 쓴다.
    pub fn counters_are_final(&self) -> bool {
        self.end_event_id.is_some()
    }
}

/// 다이제스트 텍스트 2차 조회 결과 (캐시에 없는 다이제스트만).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestTextRow {
    pub digest: String,
    pub digest_text: Option<String>,
    /// `QUERY_SAMPLE_TEXT` — 실시간 캡처에 안 걸린 다이제스트의 유일한 실행 가능 샘플.
    /// **리터럴을 포함하므로 정책 적용 대상이다.**
    pub query_sample_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestSnapshot {
    pub rows: Vec<DigestSnapshotRow>,
    /// 다음 스냅샷의 `LAST_SEEN` 필터 기준이 된다. **대상 DB 시각이다.**
    pub db_now_ms: EpochMs,
    /// `DIGEST IS NULL` 오버플로 집계 행이 존재한다
    /// → `performance_schema_digests_size` 상향 권고.
    pub overflow_detected: bool,
}

/// `EXPLAIN ... FOR CONNECTION` 실패 사유.
///
/// **에러 메시지 문자열이 아니라 에러 코드로 분류한다.** `lc_messages` 설정에 따라
/// 메시지가 번역되므로 문자열 매칭은 깨진다 ([05 §2.4](../../../docs/05-collector.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFailure {
    /// 1094 `ER_NO_SUCH_THREAD` — 스레드가 이미 종료됨. 폴백 시도 가능.
    ThreadGone,
    /// 3012 `ER_EXPLAIN_NOT_SUPPORTED` — CALL·SET·DDL 등. 폴백 없음.
    NotExplainable,
    /// 1044 / 1045 / 1227 — 타인 커넥션을 explain 할 권한이 없다.
    ///
    /// **RDS 에서는 항상 이 결과다** (M1-4 실측). `PROCESS` 도 `SUPER` 도 부족하고
    /// 정적 전역 권한 전체가 필요하다 → `Rerun` 폴백으로 전환한다.
    /// 자가진단 실패로 승격하지 **않는다** — 고칠 수 있는 설정이 아니다.
    Denied,
    /// 1142 `ER_TABLEACCESS_DENIED_ERROR` — 읽기 전용 계정으로 DML 을 `EXPLAIN` 했다.
    /// → `RerunAsSelect`(DML 을 SELECT 로 변환) 경로로 전환한다.
    DmlPrivilegeMissing,
    /// 1046 `ER_NO_DB_ERROR` — **기본 스키마 없이** `EXPLAIN` 을 보냈다.
    ///
    /// 애플리케이션은 보통 기본 스키마를 잡고 접속하므로 SQL 원문에 스키마가 없다
    /// (`FROM orders`). 그 문장을 우리 커넥션에서 그대로 재실행하면 이 오류다 —
    /// **권한 문제가 아니다.** 실측으로 갈랐다: 같은 계정이 `USE shop` 뒤에는 성공하고
    /// (1046 없음), 스키마 권한이 없는 계정은 `USE` 를 해도 1142 다.
    ///
    /// 레코드에 처리목록에서 받은 `schema_name` 이 있으므로 재실행 전에 그것을 지정한다.
    /// 이 값이 없는 경우(스키마 없이 접속한 세션)에만 남는 실패다.
    NoSchemaSelected,
    /// 1046 이 아니라 **대상 스키마에 대한 `SELECT` 권한이 없다** (1142 를 `USE` 이후에
    /// 만났다). 운영자가 고칠 수 있는 유일한 종류다 — `GRANT SELECT ON <schema>.*`.
    ///
    /// `DmlPrivilegeMissing` 과 코드가 같아서(1142) 문장 종류로 구분한다: `SELECT` 를
    /// explain 하다 1142 면 스키마 권한, DML 이면 그쪽이다.
    SchemaPrivilegeMissing,
    /// `EXPLAIN` 은 성공했지만 결과가 비어 있다 (유휴 커넥션이었다).
    ///
    /// 실측: 유휴 커넥션에 `FOR CONNECTION` 을 걸면 **에러가 아니라 빈 결과**다.
    /// 에러로 취급하면 정상 상태를 실패로 센다.
    NoStatement,
    /// 클라이언트 타임아웃. 연결을 폐기한다.
    Timeout,
    /// 분류되지 않은 코드. 기록해 두고 나중에 표에 추가한다.
    Other(u16),
}

impl PlanFailure {
    /// 실측으로 확정한 매핑 (MySQL 8.4.11 / 8.0.46,
    /// [19 §B](../../../docs/19-m1-findings.md)).
    pub fn from_mysql_error_code(code: u16) -> Self {
        match code {
            1094 => Self::ThreadGone,
            3012 => Self::NotExplainable,
            1046 => Self::NoSchemaSelected,
            1142 => Self::DmlPrivilegeMissing,
            1044 | 1045 | 1227 => Self::Denied,
            other => Self::Other(other),
        }
    }

    /// `plan_error` 에 저장하는 문자열.
    pub fn as_str(&self) -> String {
        match self {
            Self::ThreadGone => "thread_gone".into(),
            Self::NotExplainable => "not_explainable".into(),
            Self::Denied => "denied".into(),
            Self::DmlPrivilegeMissing => "dml_privilege_missing".into(),
            Self::NoSchemaSelected => "no_schema_selected".into(),
            Self::SchemaPrivilegeMissing => "schema_privilege_missing".into(),
            Self::NoStatement => "no_statement".into(),
            Self::Timeout => "timeout".into(),
            Self::Other(c) => format!("other:{c}"),
        }
    }

    /// 원문 그대로 `EXPLAIN` 재실행을 시도할 가치가 있는가.
    ///
    /// `Denied` 도 포함한다 — 타인 커넥션 explain 권한이 없어도 **우리 커넥션에서
    /// 재실행**하는 것은 `SELECT` 권한만으로 된다(M1-4b).
    /// `NotExplainable` 은 재실행해도 같은 결과다.
    ///
    /// 스키마 관련 실패도 제외한다 — 같은 커넥션에서 같은 문장을 다시 보내면 같은
    /// 오류이고, 그걸 시도 횟수로 세면 고칠 수 있는 실패(권한)가 **재시도 소진으로
    /// 덮인다.**
    pub fn allows_rerun_fallback(&self) -> bool {
        !matches!(
            self,
            Self::NotExplainable
                | Self::NoStatement
                | Self::NoSchemaSelected
                | Self::SchemaPrivilegeMissing
        )
    }

    /// DML 을 `SELECT` 로 변환해 근사 플랜을 얻어야 하는가.
    pub fn requires_select_conversion(&self) -> bool {
        *self == Self::DmlPrivilegeMissing
    }

    /// **운영자가 조치할 수 있는 실패인가.** 화면·로그가 이걸 구분해야 "권한을 주면
    /// 된다" 와 "구조적으로 안 된다"(RDS 의 `Denied`)가 섞이지 않는다.
    pub fn is_actionable(&self) -> bool {
        matches!(self, Self::SchemaPrivilegeMissing | Self::NoSchemaSelected)
    }

    /// 자가진단 실패로 승격해야 하는가.
    ///
    /// **`Denied` 는 승격하지 않는다.** RDS 에서는 정상이며 사용자가 고칠 수 없다.
    /// 초기 설계는 이걸 "`PROCESS` 미부여"로 해석해 승격시켰는데, 그러면 모든 RDS
    /// 인스턴스가 상시 자가진단 실패로 표시된다.
    pub fn escalates_to_diagnostic(&self) -> bool {
        false
    }

    /// 연결을 폐기해야 하는가 (취소해도 서버 측 작업이 남을 수 있다).
    pub fn requires_connection_drop(&self) -> bool {
        *self == Self::Timeout
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainOutcome {
    Plan(String),
    Failed(PlanFailure),
}

/// 수집 제외 규칙 중 **SQL 로 밀어넣을 수 있는 것** (M4-13).
///
/// 정규식 제외는 SQL 로 표현하지 않고 수집기에서 적용한다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Excludes {
    pub schemas: Vec<String>,
    pub users: Vec<String>,
}

impl Excludes {
    /// 시스템 스키마 + 우리 계정을 기본으로 제외한다.
    ///
    /// **자기 제외가 실패하면** 1초 주기 `detect` 쿼리가 상위 N 후보·`_other`·
    /// 커버리지·계정 롤업을 전부 오염시킨다 ([05 §10](../../../docs/05-collector.md)).
    pub fn with_defaults(monitor_user: &str) -> Self {
        Self {
            // **목록을 여기 리터럴로 두지 않는다.** 부트스트랩과 화면이 같은 판정을
            // 필요로 하므로 `bootstrap::schemas` 가 유일한 정의다 — 두 벌이 되면
            // 한쪽만 갱신되는 결함이 생긴다.
            schemas: crate::bootstrap::schemas::SYSTEM_SCHEMAS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            users: vec![monitor_user.to_string()],
        }
    }
}

#[async_trait]
pub trait TargetDb: Send + Sync {
    /// 1초 주기 경량 탐지. **정상 상태에서 tick 당 이 쿼리 1건만 나간다.**
    async fn probe(&self, threshold_secs: u32, excludes: &Excludes) -> Result<ProbeResult>;

    /// 임계값 초과 스레드의 전문 SQL.
    async fn full_sql(&self, ids: &[u64]) -> Result<Vec<FullSqlRow>>;

    /// 임계값 초과 스레드의 정확 지표 + 다이제스트.
    async fn stmt_current(&self, ids: &[u64]) -> Result<Vec<StmtCurrentRow>>;

    /// **끝난** 문장의 지표 (`events_statements_history`). 확정 시점에 부른다.
    ///
    /// # 왜 이게 있어야 하는가
    ///
    /// 실행 중 카운터는 전부 0 이므로([`StmtCurrentRow::counters_are_final`]) 실시간
    /// 캡처만으로는 `rows_examined` 를 영구히 알 수 없다. 슬로우로그가 병합되면 채워지지만
    /// 도착하지 않으면 그대로다 — 실측 4,814건 중 2,016건(42%)이 그 상태였다.
    ///
    /// 확정 시점 주석은 "스레드가 이미 사라졌으므로 새로 조회할 수 없다" 고 적혀 있었다.
    /// **그건 커넥션이 닫혔을 때만 참이다.** 풀링 커넥션은 살아 있고, 그 스레드의
    /// `events_statements_history` 가 방금 끝난 문장을 **실제 값으로** 들고 있다:
    ///
    /// ```text
    /// 실행 중(current):  ROWS_EXAMINED=0        END_EVENT_ID=NULL
    /// 완료 후(history):  ROWS_EXAMINED=120,005  END_EVENT_ID=4
    /// ```
    ///
    /// `events_statements_history` consumer 는 MySQL 8 기본 활성이고 스레드당 5개를 남긴다
    /// (`performance_schema_events_statements_history_size`). 실측으로 확인했다.
    ///
    /// 끝난 행만(`END_EVENT_ID IS NOT NULL`) 최신순으로 돌려준다. 어느 행이 우리 것인지는
    /// 호출부가 다이제스트로 가른다 — 모르면 추측하지 않는다.
    async fn stmt_history(&self, ids: &[u64]) -> Result<Vec<StmtCurrentRow>>;

    /// 실행 중 실행계획. **별도 연결에서** 실행해야 폴링이 밀리지 않는다.
    ///
    /// ⚠ RDS 에서는 항상 [`PlanFailure::Denied`] 다 — 타인 커넥션 explain 은 정적 전역
    /// 권한 전체를 요구한다([19 §B](../../../docs/19-m1-findings.md)).
    /// 실질 기본 경로는 [`TargetDb::explain_rerun`] 이다.
    async fn explain_for_connection(&self, connection_id: u64) -> Result<ExplainOutcome>;

    /// 문장을 `EXPLAIN FORMAT=JSON` 으로 재실행한다. **`SELECT` 권한만으로 동작한다.**
    ///
    /// 호출자는 `dbmon_normalize::plan_query()` 로 만든 문장을 넘긴다 — DML 이면
    /// 조건절이 `SELECT` 로 변환돼 있다. `EXPLAIN` 은 문장을 실행하지 않는다.
    /// 문장을 `EXPLAIN FORMAT=JSON` 으로 재실행한다.
    ///
    /// `schema` 는 **그 문장이 실행됐던 기본 스키마**다(처리목록의 `db`). 애플리케이션은
    /// 보통 스키마를 잡고 접속하므로 SQL 원문에 스키마가 없고(`FROM orders`), 그걸 우리
    /// 커넥션에서 그대로 재실행하면 **1046 `No database selected`** 다 — 실측으로
    /// 확인했고 로컬에 쌓인 플랜이 전부 테이블 없는 쿼리(`SELECT sleep(…)`)뿐이었던
    /// 이유다. 값이 있으면 재실행 전에 그 스키마를 지정한다.
    async fn explain_rerun(&self, sql: &str, schema: Option<&str>) -> Result<ExplainOutcome>;

    /// `FORMAT=TREE` 텍스트. 사람이 읽기 쉬워 UI 가치가 크다. 미지원이면 `None`.
    async fn explain_tree(&self, sql: &str, schema: Option<&str>) -> Result<Option<String>>;

    /// 다이제스트 스냅샷 (지표 컬럼만). `last_seen_gte_ms` 로 활성 다이제스트만 받는다.
    async fn digest_snapshot(&self, last_seen_gte_ms: Option<EpochMs>) -> Result<DigestSnapshot>;

    /// 캐시에 없는 다이제스트의 텍스트만 2차 조회.
    async fn digest_texts(&self, digests: &[String]) -> Result<Vec<DigestTextRow>>;

    /// `performance_schema.global_status` — 실시간 지표.
    async fn global_status(&self) -> Result<BTreeMap<String, String>>;

    /// `SELECT STATEMENT_DIGEST(?)` — 우리 문장의 다이제스트를 계산해 자기 제외에 쓴다.
    async fn statement_digest(&self, sql: &str) -> Result<Option<String>>;

    /// 대상 DB 의 현재 시각. 시계 오프셋 추정용 (F14).
    ///
    /// `probe` 가 0행을 반환하면 시각을 못 얻으므로, 낮은 빈도(기본 30초)로 이걸 호출한다.
    async fn db_now_ms(&self) -> Result<EpochMs>;

    async fn ping(&self) -> Result<()>;

    /// **연결 풀을 미리 채운다. tick 밖에서 호출한다.**
    ///
    /// 콜드 연결 수립(TLS 핸드셰이크 + IAM 토큰 인증)은 최대 5초가 걸리는데 detect tick
    /// 예산은 800ms 다. tick 안에서 수립을 시도하면 예산 초과로 **취소**되고,
    /// `mysql_async` 는 진행 중이던 핸드셰이크를 폐기하므로 콜드 상태에서 영구히
    /// 실패할 수 있다. 그래서 수립을 tick 밖으로 뺀다.
    ///
    /// 기본 구현은 `ping` 이다 — 대부분의 어댑터에는 풀이 없다.
    async fn warm(&self) -> Result<()> {
        self.ping().await
    }

    /// 대상 인스턴스의 **전역** `sql_mode`.
    ///
    /// 우리 세션은 `sql_mode=''` 로 고정한다(주입 방어). 그런데 SQL 은 앱 세션에서
    /// 오고 그 세션의 모드는 우리가 통제하지 않는다. 모드가 다르면 **같은 문자열을
    /// 다르게 파싱한다** — 8.4.11 실측:
    ///
    /// ```text
    /// SET sql_mode='ANSI_QUOTES';
    /// SELECT COUNT(*) FROM orders WHERE "status" = 'PAID';   → 15000  ("status" = 식별자)
    /// SET sql_mode='';
    /// SELECT COUNT(*) FROM orders WHERE "status" = 'PAID';   → 0      ("status" = 문자열)
    /// ```
    ///
    /// 후자를 EXPLAIN 하면 `Zero rows (Impossible WHERE)` 가 나온다. 그걸 정확한 플랜으로
    /// 저장하면 운영자는 **존재하지 않는 쿼리의 플랜**을 보게 된다.
    ///
    /// 기본 구현은 빈 문자열 — 위험 없음으로 본다.
    async fn target_sql_mode(&self) -> Result<String> {
        Ok(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_failure_classified_by_code_not_message() {
        assert_eq!(
            PlanFailure::from_mysql_error_code(1094),
            PlanFailure::ThreadGone
        );
        assert_eq!(
            PlanFailure::from_mysql_error_code(3012),
            PlanFailure::NotExplainable
        );
        for c in [1044, 1045, 1227] {
            assert_eq!(PlanFailure::from_mysql_error_code(c), PlanFailure::Denied);
        }
        assert_eq!(
            PlanFailure::from_mysql_error_code(9999),
            PlanFailure::Other(9999)
        );
    }

    #[test]
    fn fallback_policy_per_failure() {
        assert!(PlanFailure::ThreadGone.allows_rerun_fallback());
        assert!(PlanFailure::Timeout.allows_rerun_fallback());
        assert!(
            !PlanFailure::NotExplainable.allows_rerun_fallback(),
            "재실행해도 같은 결과다"
        );
        assert!(
            PlanFailure::Denied.allows_rerun_fallback(),
            "타인 커넥션 explain 권한이 없어도 우리 커넥션에서 재실행하는 것은 된다 (M1-4b)"
        );
        assert!(
            !PlanFailure::NoStatement.allows_rerun_fallback(),
            "실행 중 문장이 없었다"
        );

        assert!(PlanFailure::DmlPrivilegeMissing.requires_select_conversion());
        assert!(!PlanFailure::Denied.requires_select_conversion());

        assert!(PlanFailure::Timeout.requires_connection_drop());
        assert!(!PlanFailure::ThreadGone.requires_connection_drop());
    }

    /// RDS 에서는 `Denied` 가 상시 발생한다. 승격하면 모든 인스턴스가 자가진단 실패가 된다.
    #[test]
    fn denied_does_not_escalate_to_diagnostic_failure() {
        for f in [
            PlanFailure::Denied,
            PlanFailure::DmlPrivilegeMissing,
            PlanFailure::ThreadGone,
            PlanFailure::NotExplainable,
            PlanFailure::NoStatement,
            PlanFailure::Timeout,
        ] {
            assert!(!f.escalates_to_diagnostic(), "{f:?}");
        }
    }

    #[test]
    fn dml_privilege_error_is_classified() {
        assert_eq!(
            PlanFailure::from_mysql_error_code(1142),
            PlanFailure::DmlPrivilegeMissing
        );
    }

    #[test]
    fn plan_error_strings_are_stable() {
        assert_eq!(PlanFailure::ThreadGone.as_str(), "thread_gone");
        assert_eq!(PlanFailure::Other(1317).as_str(), "other:1317");
    }

    #[test]
    fn default_excludes_cover_system_schemas_and_self() {
        let e = Excludes::with_defaults("dbmon");
        assert!(e.schemas.contains(&"performance_schema".to_string()));
        assert!(
            e.users.contains(&"dbmon".to_string()),
            "자기 제외가 빠지면 통계가 오염된다"
        );
    }

    #[test]
    fn nested_statement_detected() {
        let mut r = StmtCurrentRow::default();
        assert!(!r.is_nested_statement());
        r.nesting_event_type = Some("STATEMENT".into());
        assert!(r.is_nested_statement());
        r.nesting_event_type = Some("TRANSACTION".into());
        assert!(!r.is_nested_statement());
    }
}
