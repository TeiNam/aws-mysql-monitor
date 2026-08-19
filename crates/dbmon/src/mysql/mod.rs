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
    /// 심층 조회·플랜. tick 밖의 병렬 태스크에서 돈다.
    pub query: Duration,
    /// `daily` 루프처럼 무거운 조회.
    pub bulk_query: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            detect: Duration::from_millis(800),
            query: Duration::from_secs(3),
            bulk_query: Duration::from_secs(30),
        }
    }
}

/// 대상 인스턴스 하나에 대한 연결 집합.
pub struct TargetMysql {
    hot: Pool,
    bulk: Pool,
    timeouts: Timeouts,
    /// 진단 로그에 쓰는 라벨. 엔드포인트를 그대로 쓰지 않는다(호스트명이 사업 정보일 수 있다).
    label: String,
}

impl TargetMysql {
    /// 풀을 만든다. **연결은 지연 생성**이므로 이 호출은 네트워크를 타지 않는다.
    pub fn connect(base: Opts, timeouts: Timeouts, label: impl Into<String>) -> Result<Self> {
        let hot = pool(base.clone(), 2, 4, timeouts.query)?;
        let bulk = pool(base, 1, 2, timeouts.bulk_query)?;
        Ok(Self {
            hot,
            bulk,
            timeouts,
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
        let reason = format!("{}: {}", self.label, Scrubbed(&e));
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
        // 커넥션 수립 직후 세션을 초기화한다. 매 쿼리마다 보내지 않는다.
        .init(vec![sql::session_init(query_timeout.as_millis() as u64)]);
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
        params.push(Value::from(DETECT_LIMIT));

        // **탐지 전용 타임아웃**을 쓴다. tick 예산을 넘기면 케이던스가 무너진다.
        let rows: Vec<Row> = self
            .run(self.hot_conn().await?, stmt, params, self.timeouts.detect)
            .await?;
        let db_now_ms = rows.first().and_then(|r| unix_seconds_to_ms(opt(r, 7)));
        let truncated = rows.len() as u64 >= DETECT_LIMIT;
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

    async fn explain_rerun(&self, sql_text: &str) -> Result<ExplainOutcome> {
        self.explain(sql::explain_rerun(sql_text, ExplainFormat::Json))
            .await
    }

    async fn explain_tree(&self, sql_text: &str) -> Result<Option<String>> {
        match self
            .explain(sql::explain_rerun(sql_text, ExplainFormat::Tree))
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
}

/// `detect` 쿼리의 `LIMIT`. 설정에서 주입하는 것이 맞지만, 포트 시그니처를 넓히지 않기 위해
/// 상한을 상수로 둔다. 설정값이 이보다 작으면 수집기가 결과를 잘라 쓴다.
const DETECT_LIMIT: u64 = 500;

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
