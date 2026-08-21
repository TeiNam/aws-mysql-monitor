//! 대상 MySQL 어댑터 — [`TargetDb`] 구현.
//!
//! **이 모듈은 AWS 를 모른다.** 접속 비밀은 [`dbmon_core::ports::AuthTokenProvider`] 가 준다
//! ([02 §5](../../../../docs/02-architecture.md) F31).
//!
//! # 풀을 두 개로 나눈다 (F7)
//!
//! 초기 설계는 인스턴스당 풀 하나에 "최대 3(detect 1 + 심층 1 + 플랜 1)"이라고 적었지만
//! 루프는 **5개**다(`detect`·`digest`·`status`·`health`·`daily`).
//! `daily`(쿼리 5~10건, 타임아웃 30초)가 도는 동안 `health`·`digest` 가 겹치면
//! **`detect` 가 커넥션 대기에 들어가** 1초 케이던스가 무너진다.
//! 이건 조용한 탐지 해상도 저하로만 나타난다.
//!
//! | 풀 | 용도 | 크기 |
//! |---|---|---|
//! | `hot` | `detect` **전용 예약** + 심층 조회 + 플랜 | 2~4 |
//! | `bulk` | `digest`·`status`·`health`·`daily` | 1~2 |

pub mod connect;
pub mod sql;

use std::time::Duration;

use dbmon_core::digest::DigestSnapshotRow;
use dbmon_core::error::{DomainError, Result};
use dbmon_core::ports::target_db::{
    DigestSnapshot, DigestTextRow, Excludes, ExplainOutcome, FullSqlRow, PlanFailure, ProbeResult,
    ProcessRow, StmtCurrentRow, TargetDb,
};
use dbmon_core::time::EpochMs;
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, Pool, PoolConstraints, PoolOpts, Row, Value};

use crate::telemetry::Scrubbed;
use sql::ExplainFormat;

/// `information_schema.PROCESSLIST.INFO` 의 절단 지점 (바이트).
///
/// `varchar(21845)` × utf8mb3 3바이트 = 65,535. 컬럼 정의에서 오는 값이라 설정으로
/// 바뀌지 않는다([19 §A](../../../../docs/19-m1-findings.md)).
pub const IS_PROCESSLIST_INFO_MAX_BYTES: usize = 65_535;

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    /// **탐지 쿼리 전용.** tick 예산(폴링 주기의 80%) 안에 들어야 한다.
    /// 심층 조회와 같은 값을 쓰면 3초 타임아웃이 1초 케이던스를 깨뜨린다.
    pub detect: Duration,
    /// **detect 경로 전체**(커넥션 획득 + 조회)의 상한.
    ///
    /// `detect` 만 제한하면 총 소요가 `connect + detect` 까지 늘어난다(5,000 + 800ms).
    /// 그러면 1초 케이던스가 무너지고 tick 이 겹쳐 쌓인다. 콜드 경로에 5초가 실제로
    /// 필요하므로 `connect` 를 줄이는 대신 **경로 전체**를 한 interval 안으로 묶는다 —
    /// # 두 요구가 충돌하고, 해법은 연결 수립을 tick 밖으로 빼는 것이다
    ///
    /// | 요구 | 값 |
    /// |---|---|
    /// | tick 케이던스를 지켜야 한다 | 예산 800ms (1초 주기의 80%) |
    /// | 콜드 연결 수립(TLS + IAM 토큰)에 필요한 시간 | 최대 5초 |
    ///
    /// 연결 수립이 tick **안에서** 일어나면 이 둘은 화해할 수 없다:
    ///
    /// - 상한을 5.8초로 두면 tick 이 주기의 6배가 되어 케이던스가 무너진다.
    /// - 상한을 1초로 두면 `GetConn::drop` 이 `pool.cancel_connection()` 을 호출해
    ///   진행 중인 핸드셰이크가 **폐기**되고, 콜드 상태에서 매 tick "시작 → 컷 → 취소" 를
    ///   반복해 `probe` 가 한 번도 성공하지 못한다.
    ///
    /// 그래서 **연결 수립을 tick 밖으로 뺀다** ([`TargetMysql::warm`]). 기동 시와 실패 후에
    /// `connect` 예산을 온전히 써서 풀을 채워 두면, tick 은 항상 warm 커넥션만 집는다.
    /// 이 값은 그때 **tick 예산**이면 된다.
    ///
    /// 조회 도중 취소는 안전하다(recycler 가 `cleanup_for_pool` 로 정리한다).
    pub detect_total: Duration,
    /// 심층 조회·플랜. tick 밖의 병렬 태스크에서 돈다.
    pub query: Duration,
    /// `daily` 루프처럼 무거운 조회.
    pub bulk_query: Duration,
}

/// `detect` 조회의 폭주 방어 상한 (`LIMIT`).
///
/// 설정(`collector.detect_limit`)에서 온다. 예전에는 모듈 상수가 항상 이겨서 설정값이
/// 조용히 무시됐고, `truncated` 판정도 상수 기준으로 나왔다.
pub const DEFAULT_DETECT_LIMIT: u64 = 500;

impl Default for Timeouts {
    fn default() -> Self {
        Self::derive(5_000, 800, 3_000, 30_000, 1_000)
    }
}

impl Timeouts {
    /// 설정값에서 만든다. **`detect_total` 은 파생값이다** — 상수로 두면
    /// `detect_timeout_ms` 설정이 조용히 무시된다(M27 과 같은 부류).
    ///
    /// `detect_total`(tick 예산)이 `connect` 보다 **작은 것이 정상**이다. 연결 수립은
    /// [`TargetMysql::warm`] 이 tick 밖에서 처리하므로 tick 안에서는 warm 커넥션만 집는다.
    pub fn derive(
        connect_ms: u64,
        detect_ms: u64,
        query_ms: u64,
        bulk_query_ms: u64,
        detect_interval_ms: u64,
    ) -> Self {
        Self {
            connect: Duration::from_millis(connect_ms),
            detect: Duration::from_millis(detect_ms),
            // **tick 예산이다.** 연결 수립은 `warm()` 이 tick 밖에서 처리한다.
            detect_total: Duration::from_millis(detect_interval_ms * 80 / 100),
            query: Duration::from_millis(query_ms),
            bulk_query: Duration::from_millis(bulk_query_ms),
        }
    }
}

/// 대상 인스턴스 하나에 대한 연결 집합.
pub struct TargetMysql {
    hot: Pool,
    bulk: Pool,
    timeouts: Timeouts,
    detect_limit: u64,
    /// 진단 로그에 쓰는 라벨. 엔드포인트를 그대로 쓰지 않는다(호스트명이 사업 정보일 수 있다).
    label: String,
}

impl TargetMysql {
    /// 풀을 만든다. **연결은 지연 생성**이므로 이 호출은 네트워크를 타지 않는다.
    ///
    /// ⚠ **프로덕션 조립은 [`Self::from_config`] 를 쓴다.** 이 생성자는
    /// `detect_limit` 을 기본값으로 두고 타임아웃을 호출자가 만들게 하므로,
    /// 설정을 조용히 무시하는 경로가 된다 — 실제로 그런 상태였다(2차 M2).
    /// 테스트에서만 쓴다.
    #[cfg(any(test, feature = "testing"))]
    pub fn connect(base: Opts, timeouts: Timeouts, label: impl Into<String>) -> Result<Self> {
        Self::connect_with_limit(base, timeouts, DEFAULT_DETECT_LIMIT, label)
    }

    /// **설정에서 만든다. 프로덕션 조립은 이것만 쓴다.**
    ///
    /// `connect` / `connect_with_limit` 을 직접 부르면 `collector.detect_limit` 과
    /// 타임아웃 설정이 조용히 무시된다 — 실제로 그런 상태였다(M27 을 고쳤는데 호출부가
    /// 없어서 무효였다). 설정에서 파생시키는 경로를 하나로 만들어 그 실수를 막는다.
    pub fn from_config(
        base: Opts,
        cfg: &crate::config::CollectorConfig,
        label: impl Into<String>,
    ) -> Result<Self> {
        let timeouts = Timeouts::derive(
            cfg.connect_timeout_ms,
            cfg.detect_timeout_ms,
            cfg.query_timeout_ms,
            cfg.bulk_query_timeout_ms,
            cfg.detect_interval_ms,
        );
        Self::connect_with_limit(base, timeouts, u64::from(cfg.detect_limit), label)
    }

    fn connect_with_limit(
        base: Opts,
        timeouts: Timeouts,
        detect_limit: u64,
        label: impl Into<String>,
    ) -> Result<Self> {
        let hot = pool(base.clone(), 2, 4, timeouts.query)?;
        let bulk = pool(base, 1, 2, timeouts.bulk_query)?;
        Ok(Self {
            hot,
            bulk,
            timeouts,
            detect_limit,
            label: label.into(),
        })
    }

    async fn hot_conn(&self) -> Result<Conn> {
        self.acquire(&self.hot).await
    }

    async fn bulk_conn(&self) -> Result<Conn> {
        self.acquire(&self.bulk).await
    }

    async fn acquire(&self, pool: &Pool) -> Result<Conn> {
        match tokio::time::timeout(self.timeouts.connect, pool.get_conn()).await {
            Ok(Ok(c)) => Ok(c),
            Ok(Err(e)) => Err(self.map_err(e)),
            Err(_) => Err(DomainError::Unavailable {
                dependency: "target-mysql",
                reason: format!("{}: 연결 획득 타임아웃", self.label),
            }),
        }
    }

    /// MySQL 오류를 도메인 오류로 옮긴다.
    ///
    /// **메시지를 마스킹한다** — 서버 오류 메시지에는 실패한 문장의 조각과 리터럴이
    /// 들어 있다(`Duplicate entry 'kim@example.com' for key …`).
    fn map_err(&self, e: mysql_async::Error) -> DomainError {
        // **에러 코드를 마스킹 밖에 둔다.** `scrub` 은 3자리 이상 숫자를 `?` 로 지우므로
        // 메시지 안의 `1045`·`1146` 도 사라진다. 그런데 그 코드가 운영자에게 가장 필요한
        // 정보다 — "연결 거부(2003)"·"접근 거부(1045)"·"테이블 없음(1146)" 을 구분하는
        // 유일한 단서다. `Scrubbed` 는 자기 내용만 마스킹하므로 밖에 붙인 코드는 남는다.
        let reason = match server_error_code(&e) {
            Some(code) => format!("{}: [mysql {code}] {}", self.label, Scrubbed(&e)),
            None => format!("{}: {}", self.label, Scrubbed(&e)),
        };
        match server_error_code(&e) {
            // 권한 문제는 재시도가 무의미하다. 자가진단으로 승격해 GRANT 를 고치게 한다.
            Some(1044 | 1045 | 1142 | 1227) => DomainError::Forbidden { action: reason },
            _ => DomainError::Unavailable {
                dependency: "target-mysql",
                reason,
            },
        }
    }

    async fn query_hot<T: FromRow + Send + 'static>(
        &self,
        stmt: String,
        params: Vec<Value>,
    ) -> Result<Vec<T>> {
        self.run(self.hot_conn().await?, stmt, params, self.timeouts.query)
            .await
    }

    async fn query_bulk<T: FromRow + Send + 'static>(
        &self,
        stmt: String,
        params: Vec<Value>,
    ) -> Result<Vec<T>> {
        self.run(
            self.bulk_conn().await?,
            stmt,
            params,
            self.timeouts.bulk_query,
        )
        .await
    }

    async fn run<T: FromRow + Send + 'static>(
        &self,
        mut conn: Conn,
        stmt: String,
        params: Vec<Value>,
        budget: Duration,
    ) -> Result<Vec<T>> {
        let fut = async {
            if params.is_empty() {
                conn.query(stmt).await
            } else {
                conn.exec(stmt, mysql_async::Params::Positional(params))
                    .await
            }
        };
        match tokio::time::timeout(budget, fut).await {
            Ok(Ok(rows)) => Ok(rows),
            Ok(Err(e)) => Err(self.map_err(e)),
            Err(_) => Err(DomainError::Unavailable {
                dependency: "target-mysql",
                reason: format!("{}: 쿼리 타임아웃 ({}ms)", self.label, budget.as_millis()),
            }),
        }
    }

    /// `EXPLAIN` 계열을 실행한다. 실패를 **에러가 아니라 분류로** 돌려준다.
    ///
    /// 플랜 수집 실패는 정상 운영의 일부다(스레드 종료·권한·EXPLAIN 불가 문장).
    /// 에러로 올리면 서킷 브레이커가 잘못 발화한다.
    async fn explain(&self, stmt: String) -> Result<ExplainOutcome> {
        let mut conn = self.hot_conn().await?;
        let fut = conn.query_first::<String, _>(stmt);
        match tokio::time::timeout(self.timeouts.query, fut).await {
            Ok(Ok(Some(plan))) => Ok(ExplainOutcome::Plan(plan)),
            // 실측: 유휴 커넥션은 **에러가 아니라 빈 결과**다. 실패로 세면 정상을 실패로 센다.
            Ok(Ok(None)) => Ok(ExplainOutcome::Failed(PlanFailure::NoStatement)),
            Ok(Err(e)) => match server_error_code(&e) {
                Some(code) => Ok(ExplainOutcome::Failed(PlanFailure::from_mysql_error_code(
                    code,
                ))),
                None => Err(self.map_err(e)),
            },
            Err(_) => {
                // 타임아웃 시 연결을 폐기한다 — 취소해도 서버 측 작업이 남을 수 있다.
                drop(conn);
                Ok(ExplainOutcome::Failed(PlanFailure::Timeout))
            }
        }
    }

    /// 재실행 경로. **기본 스키마를 먼저 지정하고** 같은 커넥션에서 `EXPLAIN` 한다.
    ///
    /// # 왜 같은 커넥션이어야 하는가
    ///
    /// `USE` 는 세션 상태다. 다른 커넥션에서 걸면 아무 효과가 없고, 그러면 1046 이
    /// 그대로 남는다 — "고쳤는데 안 고쳐진" 종류의 결함이다.
    ///
    /// # 1142 를 여기서는 다르게 읽는다
    ///
    /// `plan_query` 는 **항상 `SELECT` 를 만든다**(DML 은 조건절만 뽑아 SELECT 로 바꾼다).
    /// 그래서 이 경로의 1142 는 "DML 권한 없음" 이 아니라 **대상 테이블 `SELECT` 권한
    /// 없음** 이다 — 운영자가 `GRANT SELECT ON <schema>.*` 로 고칠 수 있는 유일한 종류다.
    /// `FOR CONNECTION` 경로의 1142(원문 DML 을 explain)와 구분해야 조치가 달라진다.
    async fn explain_rerun_in(&self, stmt: String, schema: Option<&str>) -> Result<ExplainOutcome> {
        let mut conn = self.hot_conn().await?;

        if let Some(use_stmt) = schema.and_then(sql::use_schema) {
            let fut = conn.query_drop(use_stmt);
            match tokio::time::timeout(self.timeouts.query, fut).await {
                Ok(Ok(())) => {}
                // 스키마 자체에 접근 권한이 없으면 1044 다. 그 사실이 결과다 —
                // 이어서 EXPLAIN 을 보내면 1046 으로 덮여 원인이 사라진다.
                Ok(Err(e)) => match server_error_code(&e) {
                    Some(code) => {
                        return Ok(ExplainOutcome::Failed(PlanFailure::from_mysql_error_code(
                            code,
                        )));
                    }
                    None => return Err(self.map_err(e)),
                },
                Err(_) => {
                    drop(conn);
                    return Ok(ExplainOutcome::Failed(PlanFailure::Timeout));
                }
            }
        }

        let fut = conn.query_first::<String, _>(stmt);
        match tokio::time::timeout(self.timeouts.query, fut).await {
            Ok(Ok(Some(plan))) => Ok(ExplainOutcome::Plan(plan)),
            Ok(Ok(None)) => Ok(ExplainOutcome::Failed(PlanFailure::NoStatement)),
            Ok(Err(e)) => match server_error_code(&e) {
                Some(1142) => Ok(ExplainOutcome::Failed(PlanFailure::SchemaPrivilegeMissing)),
                Some(code) => Ok(ExplainOutcome::Failed(PlanFailure::from_mysql_error_code(
                    code,
                ))),
                None => Err(self.map_err(e)),
            },
            Err(_) => {
                drop(conn);
                Ok(ExplainOutcome::Failed(PlanFailure::Timeout))
            }
        }
    }

    pub async fn disconnect(self) {
        let _ = self.hot.disconnect().await;
        let _ = self.bulk.disconnect().await;
    }
}

fn pool(base: Opts, min: usize, max: usize, query_timeout: Duration) -> Result<Pool> {
    let constraints = PoolConstraints::new(min, max).ok_or_else(|| DomainError::InvalidInput {
        field: "pool".into(),
        reason: format!("풀 제약이 올바르지 않다: min={min} max={max}"),
    })?;
    let mut builder = mysql_async::OptsBuilder::from_opts(base);
    builder = builder
        .pool_opts(PoolOpts::default().with_constraints(constraints))
        // **`setup` 이다. `init` 이 아니다.**
        //
        // `PoolOpts::default()` 는 `reset_connection: true` 이므로 커넥션이 풀로 돌아갈 때마다
        // `COM_RESET_CONNECTION` 이 나간다. 그건 세션 변수를 **전역값으로 되돌리고**,
        // `mysql_async` 는 그 뒤 `setup` 명령만 다시 실행한다 — `init` 은 실행하지 않는다.
        //
        // 로컬 MySQL 8.4.11 에 같은 풀 설정으로 4회 획득/반납한 실측:
        //
        // ```text
        // .init  #1: sql_mode=""                    max_exec=3000 iso=READ-COMMITTED
        // .init  #2: sql_mode="ONLY_FULL_GROUP_BY…" max_exec=0    iso=REPEATABLE-READ
        // .setup #2: sql_mode=""                    max_exec=3000 iso=READ-COMMITTED
        // ```
        //
        // 잃는 것이 세 개다:
        //
        // | 변수 | 잃으면 |
        // |---|---|
        // | `sql_mode` | `NO_BACKSLASH_ESCAPES` 를 상속해 **주입이 다시 열린다** (2차 C-1) |
        // | `max_execution_time` | **0 = 무제한.** 프로덕션 DB 에 서버측 상한이 사라진다 |
        // | `transaction_isolation` | `REPEATABLE-READ` — 긴 스냅샷이 undo 를 붙잡는다 |
        //
        // 즉 2차의 `sql_mode` 고정이 **첫 쿼리에만** 적용되고 있었다.
        // `with_reset_connection(false)` 로도 되지만 그건 커넥션 위생을 포기하는 것이라 나쁘다.
        .setup(vec![sql::session_init(query_timeout.as_millis() as u64)]);
    Ok(Pool::new(Opts::from(builder)))
}

fn server_error_code(e: &mysql_async::Error) -> Option<u16> {
    match e {
        mysql_async::Error::Server(s) => Some(s.code),
        _ => None,
    }
}

/// `Option<T>` 를 안전하게 꺼낸다. 컬럼이 없거나 타입이 다르면 `None`.
fn opt<T: FromValue>(row: &Row, idx: usize) -> Option<T> {
    row.as_ref(idx)
        .and_then(|v| T::from_value_opt(v.clone()).ok())
}

fn num<T: FromValue + Default>(row: &Row, idx: usize) -> T {
    opt(row, idx).unwrap_or_default()
}

/// `UNIX_TIMESTAMP(NOW(6))` 는 소수점이 있는 십진수로 온다. ms 로 바꾼다.
fn unix_seconds_to_ms(v: Option<f64>) -> Option<EpochMs> {
    v.map(|s| (s * 1000.0).round() as EpochMs)
}

#[async_trait::async_trait]
impl TargetDb for TargetMysql {
    async fn probe(&self, threshold_secs: u32, excludes: &Excludes) -> Result<ProbeResult> {
        let stmt = sql::detect(excludes.schemas.len(), excludes.users.len());
        let mut params: Vec<Value> = vec![Value::from(threshold_secs)];
        params.extend(excludes.schemas.iter().map(|s| Value::from(s.as_str())));
        params.extend(excludes.users.iter().map(|s| Value::from(s.as_str())));
        // `LIMIT ?` 는 문장 마지막이다. 폭주 방어 상한.
        //
        // **설정값을 쓴다.** 예전에는 `DETECT_LIMIT` 상수가 항상 이겨서
        // `collector.detect_limit` 이 조용히 무시됐고, `truncated` 판정도 500 기준으로
        // 나왔다 — 설정을 200 으로 낮춰도 300건이 돌아오면서 truncated=false 였다.
        let limit = self.detect_limit;
        params.push(Value::from(limit));

        // **탐지 전용 타임아웃**을 쓴다. tick 예산을 넘기면 케이던스가 무너진다.
        // 커넥션 획득까지 포함해 `detect_total` 안으로 묶는다.
        let inner = async {
            let conn = self.hot_conn().await?;
            self.run::<Row>(conn, stmt, params, self.timeouts.detect)
                .await
        };
        let rows: Vec<Row> = match tokio::time::timeout(self.timeouts.detect_total, inner).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(DomainError::Unavailable {
                    dependency: "target-mysql",
                    reason: format!(
                        "{}: detect 경로 전체 타임아웃 ({}ms)",
                        self.label,
                        self.timeouts.detect_total.as_millis()
                    ),
                });
            }
        };
        let db_now_ms = rows.first().and_then(|r| unix_seconds_to_ms(opt(r, 7)));
        let truncated = rows.len() as u64 >= limit;
        Ok(ProbeResult {
            rows: rows
                .iter()
                .map(|r| ProcessRow {
                    id: num(r, 0),
                    user: opt(r, 1),
                    host: opt(r, 2),
                    db: opt(r, 3),
                    command: opt(r, 4),
                    time_secs: num(r, 5),
                    state: opt(r, 6),
                })
                .collect(),
            db_now_ms,
            truncated,
        })
    }

    async fn full_sql(&self, ids: &[u64]) -> Result<Vec<FullSqlRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<Row> = self
            .query_hot(
                sql::full_sql(ids.len()),
                ids.iter().map(|i| Value::from(*i)).collect(),
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| FullSqlRow {
                id: num(r, 0),
                db: opt(r, 1),
                user: opt(r, 2),
                host: opt(r, 3),
                time_secs: num(r, 4),
                info: opt(r, 5),
            })
            .collect())
    }

    async fn stmt_current(&self, ids: &[u64]) -> Result<Vec<StmtCurrentRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<Row> = self
            .query_hot(
                sql::stmt_current(ids.len()),
                ids.iter().map(|i| Value::from(*i)).collect(),
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| StmtCurrentRow {
                processlist_id: num(r, 0),
                thread_id: num(r, 1),
                event_name: opt(r, 2),
                current_schema: opt(r, 3),
                digest: opt(r, 4),
                digest_text: opt(r, 5),
                sql_text: opt(r, 6),
                timer_wait_ps: opt(r, 7),
                lock_time_ps: opt(r, 8),
                rows_examined: opt(r, 9),
                rows_sent: opt(r, 10),
                rows_affected: opt(r, 11),
                created_tmp_tables: opt(r, 12),
                created_tmp_disk_tables: opt(r, 13),
                select_full_join: opt(r, 14),
                sort_merge_passes: opt(r, 15),
                no_index_used: opt::<u8>(r, 16).map(|v| v != 0),
                no_good_index_used: opt::<u8>(r, 17).map(|v| v != 0),
                nesting_event_type: opt(r, 18),
            })
            .collect())
    }

    async fn explain_for_connection(&self, connection_id: u64) -> Result<ExplainOutcome> {
        self.explain(sql::explain_for_connection(
            connection_id,
            ExplainFormat::Json,
        ))
        .await
    }

    async fn explain_rerun(&self, sql_text: &str, schema: Option<&str>) -> Result<ExplainOutcome> {
        self.explain_rerun_in(sql::explain_rerun(sql_text, ExplainFormat::Json), schema)
            .await
    }

    async fn explain_tree(&self, sql_text: &str, schema: Option<&str>) -> Result<Option<String>> {
        match self
            .explain_rerun_in(sql::explain_rerun(sql_text, ExplainFormat::Tree), schema)
            .await?
        {
            ExplainOutcome::Plan(p) => Ok(Some(p)),
            ExplainOutcome::Failed(_) => Ok(None),
        }
    }

    async fn digest_snapshot(&self, last_seen_gte_ms: Option<EpochMs>) -> Result<DigestSnapshot> {
        let stmt = sql::digest_snapshot(last_seen_gte_ms.is_some());
        let params: Vec<Value> = last_seen_gte_ms.map(Value::from).into_iter().collect();
        let rows: Vec<Row> = self.query_bulk(stmt, params).await?;
        let db_now_ms = rows
            .first()
            .and_then(|r| unix_seconds_to_ms(opt(r, 24)))
            // 0행이면 스냅샷 구간을 진전시킬 수 없다. 호출자가 이전 값을 유지한다.
            .unwrap_or(0);

        let overflow: Vec<Row> = self
            .query_bulk(sql::DIGEST_OVERFLOW.to_string(), Vec::new())
            .await
            .unwrap_or_default();

        Ok(DigestSnapshot {
            rows: rows.iter().map(digest_row).collect(),
            db_now_ms,
            overflow_detected: !overflow.is_empty(),
        })
    }

    async fn digest_texts(&self, digests: &[String]) -> Result<Vec<DigestTextRow>> {
        if digests.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<Row> = self
            .query_bulk(
                sql::digest_texts(digests.len()),
                digests.iter().map(|d| Value::from(d.as_str())).collect(),
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| DigestTextRow {
                digest: opt(r, 0).unwrap_or_default(),
                digest_text: opt(r, 1),
                query_sample_text: opt(r, 2),
            })
            .collect())
    }

    async fn global_status(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let rows: Vec<(String, String)> = self
            .query_bulk(sql::GLOBAL_STATUS.to_string(), Vec::new())
            .await?;
        Ok(rows.into_iter().collect())
    }

    async fn statement_digest(&self, sql_text: &str) -> Result<Option<String>> {
        let rows: Vec<Option<String>> = self
            .query_bulk(
                sql::STATEMENT_DIGEST.to_string(),
                vec![Value::from(sql_text)],
            )
            .await?;
        Ok(rows.into_iter().flatten().next())
    }

    async fn db_now_ms(&self) -> Result<EpochMs> {
        let rows: Vec<f64> = self.query_bulk(sql::DB_NOW.to_string(), Vec::new()).await?;
        unix_seconds_to_ms(rows.first().copied()).ok_or(DomainError::Unavailable {
            dependency: "target-mysql",
            reason: "NOW(6) 를 읽을 수 없다".into(),
        })
    }

    async fn ping(&self) -> Result<()> {
        let _: Vec<u8> = self.query_hot(sql::PING.to_string(), Vec::new()).await?;
        Ok(())
    }

    /// 대상의 전역 `sql_mode`. 어휘 발산 판정에 쓴다.
    ///
    /// **포트의 기본 구현(빈 문자열)에 의존하면 통제가 무력화된다** — 빈 문자열은
    /// "위험 없음" 으로 해석되므로, 이 메서드가 없으면 `ANSI_QUOTES` 대상의 플랜이
    /// 계속 "정확" 으로 저장된다. 실제로 3차에서 그 상태였다(편집이 doc 주석에 들어갔다).
    /// **hot 풀을 미리 채운다. tick 밖에서 부른다.**
    ///
    /// ⚠ **트레이트 메서드여야 한다.** 고유 메서드로 두면 `InstanceCollector<D: TargetDb>`
    /// 의 `self.db.warm()` 이 **트레이트 기본 구현(`ping`)** 으로 해소되고 이 코드는
    /// 죽는다 — 제네릭 경계는 고유 메서드를 보지 못한다. 실제로 그 상태였다
    /// (미배선 수정의 네 번째 재발).
    ///
    /// 기동 시 한 번, 그리고 detect 가 연결 문제로 실패한 뒤에 부른다.
    /// `connect` 예산(기본 5초)을 온전히 쓰므로 TLS 핸드셰이크와 IAM 토큰 인증이
    /// 중간에 취소되지 않는다 — `detect_total`(tick 예산) 안에서는 그게 불가능하다.
    ///
    /// 성공하면 이후 tick 은 warm 커넥션만 집으므로 획득이 즉시 끝난다.
    async fn warm(&self) -> Result<()> {
        // 커넥션을 얻어 간단한 쿼리를 돌린다. `init`(세션 설정)도 이때 적용된다.
        let conn = self.hot_conn().await?;
        let _: Vec<u8> = self
            .run(conn, sql::PING.to_string(), Vec::new(), self.timeouts.query)
            .await?;
        Ok(())
    }

    async fn target_sql_mode(&self) -> Result<String> {
        let rows: Vec<String> = self
            .query_hot(sql::GLOBAL_SQL_MODE.to_string(), Vec::new())
            .await?;
        Ok(rows.into_iter().next().unwrap_or_default())
    }
}

/// `detect` 쿼리의 `LIMIT`. 설정에서 주입하는 것이 맞지만, 포트 시그니처를 넓히지 않기 위해
fn digest_row(r: &Row) -> DigestSnapshotRow {
    DigestSnapshotRow {
        schema_name: opt(r, 0),
        mysql_digest: opt(r, 1).unwrap_or_default(),
        count_star: num(r, 2),
        sum_timer_wait_ps: num(r, 3),
        min_timer_wait_ps: num(r, 4),
        // idx 5 = AVG_TIMER_WAIT — 누적 평균이므로 저장하지 않는다(델타에서 계산한다).
        max_timer_wait_ps: num(r, 6),
        sum_lock_time_ps: num(r, 7),
        sum_errors: num(r, 8),
        sum_warnings: num(r, 9),
        sum_rows_affected: num(r, 10),
        sum_rows_sent: num(r, 11),
        sum_rows_examined: num(r, 12),
        sum_created_tmp_tables: num(r, 13),
        sum_created_tmp_disk_tables: num(r, 14),
        sum_select_full_join: num(r, 15),
        sum_select_scan: num(r, 16),
        sum_sort_merge_passes: num(r, 17),
        sum_no_index_used: num(r, 18),
        sum_no_good_index_used: num(r, 19),
        first_seen_ms: num(r, 20),
        last_seen_ms: num(r, 21),
        quantile_95_ps: opt(r, 22),
        quantile_99_ps: opt(r, 23),
    }
}

/// `information_schema` 가 준 SQL 텍스트가 절단됐는지 판정한다.
///
/// 상한이 컬럼 정의에서 오는 고정값이라 **정확히 그 길이면 절단**이다
/// ([19 §A](../../../../docs/19-m1-findings.md)).
pub fn is_info_truncated(info: &str) -> bool {
    info.len() >= IS_PROCESSLIST_INFO_MAX_BYTES
}

#[cfg(test)]
mod tests {

    /// **설정이 실제로 반영되는지** 확인한다. `from_config` 없이 `connect()` 를 쓰면
    /// `detect_limit` 과 타임아웃이 상수에 밀려 조용히 무시된다.
    #[test]
    fn from_config_propagates_settings() {
        // 설정값을 그대로 파생시킨다.
        let t = Timeouts::derive(4_000, 600, 3_000, 30_000, 1_000);
        assert_eq!(t.detect, Duration::from_millis(600));
        assert_eq!(t.connect, Duration::from_millis(4_000));
        // **`detect_total` 은 tick 예산이다.** 연결 수립은 `warm()` 이 tick 밖에서 한다.
        // 예전 판은 `connect + detect + 20%` = 6,960ms 였는데, 1초 주기의 **8.7배**라
        // 아무것도 묶지 못했다 — M17(케이던스)과 M4(취소) 사이를 한 바퀴 돈 결과였다.
        assert_eq!(
            t.detect_total,
            Duration::from_millis(800),
            "1초 주기의 80% 여야 한다"
        );
        assert!(
            t.detect_total < t.connect,
            "tick 예산이 연결 예산보다 작은 것이 정상이다 — 그래서 warm() 이 필요하다"
        );
    }
    use super::*;

    #[test]
    fn truncation_detected_exactly_at_ceiling() {
        assert!(!is_info_truncated(
            &"a".repeat(IS_PROCESSLIST_INFO_MAX_BYTES - 1)
        ));
        assert!(is_info_truncated(
            &"a".repeat(IS_PROCESSLIST_INFO_MAX_BYTES)
        ));
        assert!(!is_info_truncated("SELECT 1"));
    }

    #[test]
    fn unix_seconds_convert_with_sub_second_precision() {
        assert_eq!(
            unix_seconds_to_ms(Some(1_787_203_391.5)),
            Some(1_787_203_391_500)
        );
        assert_eq!(unix_seconds_to_ms(None), None);
    }

    #[test]
    fn pool_rejects_impossible_constraints() {
        let opts = Opts::from_url("mysql://u:p@127.0.0.1:1/db").unwrap();
        // min > max 는 만들 수 없다.
        assert!(pool(opts, 5, 2, Duration::from_secs(3)).is_err());
    }

    #[test]
    fn timeouts_default_matches_design() {
        let t = Timeouts::default();
        assert_eq!(t.connect, Duration::from_secs(5));
        assert_eq!(t.detect, Duration::from_millis(800), "1초 주기의 80%");
        assert!(t.detect < t.query, "탐지는 심층 조회보다 짧아야 한다");
        assert_eq!(t.query, Duration::from_secs(3));
        assert_eq!(
            t.bulk_query,
            Duration::from_secs(30),
            "daily 루프는 더 길다"
        );
    }
}
