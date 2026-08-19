//! 인스턴스 하나의 수집 루프 ([05 §1](../../../../docs/05-collector.md)).
//!
//! # 정상 상태에서 tick 당 쿼리 1건
//!
//! ```text
//! tick(1s)
//!   ├─ Q1: performance_schema.processlist          ← 항상
//!   ├─ 후보가 없으면 여기서 끝 (정상 상태의 99%)
//!   └─ 후보마다
//!        ├─ Q2: information_schema.PROCESSLIST     → 전문 SQL
//!        ├─ Q3: events_statements_current          → 정확 지표
//!        └─ Q4: EXPLAIN ... (재실행)                → 실행계획
//! ```
//!
//! 이게 "대상 DB 에 부하를 주지 않는다"의 근거다. 비싼 조회는 실제로 느린 쿼리가
//! 있을 때만 발생한다.
//!
//! # 플랜 수집 경로가 실측으로 뒤집혔다
//!
//! `EXPLAIN ... FOR CONNECTION` 은 RDS 에서 쓸 수 없다([19 §B](../../../../docs/19-m1-findings.md)).
//! 그래서 순서가 이렇게 된다:
//!
//! 1. `FOR CONNECTION` 시도 — 자체 관리 MySQL 이면 성공한다 (가장 정확)
//! 2. `Denied` 면 원문 `EXPLAIN` 재실행 (`SELECT` 권한만 필요)
//! 3. DML 이라 `1142` 면 조건절을 `SELECT` 로 변환해 근사 플랜
//!
//! 1번이 실패하는 것이 RDS 의 **정상**이므로 실패 카운터를 올리지 않는다.

pub mod build;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dbmon_core::clock_offset::ClockOffset;
use dbmon_core::error::Result;
use dbmon_core::inflight::{Identity, InFlightTracker, Observation};
use dbmon_core::instance::Instance;
use dbmon_core::ports::target_db::{Excludes, ExplainOutcome, PlanFailure, TargetDb};
use dbmon_core::ports::{SlowQueryStore, target_db::FullSqlRow, target_db::StmtCurrentRow};
use dbmon_core::slow_query::{LiteralPolicy, PlanSource, SlowQueryState};
use dbmon_core::time::Clock;

use build::{CaptureInput, build};

/// 한 tick 의 관측 결과. 메트릭으로 내보낸다.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickStats {
    pub candidates: usize,
    /// `LIMIT` 에 걸려 잘렸다 → `detect_overflow`.
    pub detect_truncated: bool,
    pub deep_probed: usize,
    /// 심층 조회 상한에 걸려 지표만 받은 후보 수.
    pub deep_probe_skipped: usize,
    /// **심층 조회 자체가 실패했다** (권한·타임아웃). 0 이 아니면 이 tick 의 레코드에는
    /// SQL·지표가 없다 — "SQL 을 못 읽는 느린 쿼리" 와 "권한이 빠진 상태" 는 다르다.
    pub deep_probe_failed: usize,
    pub prefetch_saved: usize,
    pub finalized: usize,
    pub plans_for_connection: usize,
    pub plans_rerun: usize,
    pub plans_rerun_as_select: usize,
    pub plans_failed: usize,
    /// 마스킹 후조건 실패로 `off` 로 강등된 레코드 수 (T-36).
    pub masking_degraded: usize,
    pub plan_redactions: usize,
    pub store_errors: usize,
}

/// 수집 파라미터. 설정에서 온다.
#[derive(Debug, Clone)]
pub struct CollectParams {
    pub slow_threshold_secs: u32,
    /// 심층 조회 대상 상한 (느린 순 상위 N). 폭주 방어.
    pub deep_probe_limit: usize,
    /// `FOR CONNECTION` 을 시도할 것인가.
    ///
    /// RDS 에서는 항상 실패하므로 한 번 `Denied` 를 받으면 **자동으로 끈다** —
    /// 매 tick 실패하는 쿼리를 보내는 것은 낭비다.
    pub try_for_connection: bool,
    /// 리터럴 저장 정책. 레코드마다 고정된다 (A3-2).
    pub literal_policy: LiteralPolicy,
    /// 모니터링 DB 계정명. **자기 제외의 기준**이다 — 비면 통계가 오염된다.
    pub monitor_db_user: String,
    /// 이 워커의 식별자. 고아 `in_flight` 정리에 쓴다 (F4).
    pub worker_id: String,
}

impl Default for CollectParams {
    fn default() -> Self {
        Self {
            slow_threshold_secs: 2,
            deep_probe_limit: 50,
            try_for_connection: true,
            literal_policy: LiteralPolicy::Masked,
            monitor_db_user: "dbmon".into(),
            worker_id: "unknown".into(),
        }
    }
}

/// 인스턴스 하나의 수집기.
pub struct InstanceCollector<D, S, C> {
    instance: Instance,
    db: D,
    store: Arc<S>,
    clock: C,
    tracker: InFlightTracker,
    offset: ClockOffset,
    excludes: Excludes,
    params: CollectParams,
    epoch: Option<u64>,
    /// 다음 기회에 연결 풀을 다시 채워야 한다. 기동 직후에도 참이다.
    needs_warm: bool,
    /// `FOR CONNECTION` 이 권한 부족으로 실패했다. 이후 시도하지 않는다.
    for_connection_denied: bool,
    /// `probe` 가 0행이라 DB 시각을 못 받은 연속 횟수.
    ticks_without_db_time: AtomicU64,
}

/// 이 횟수만큼 DB 시각을 못 받으면 전용 쿼리로 오프셋을 갱신한다.
///
/// `probe` 는 0행일 때 시각을 주지 않는다(초기 설계가 놓친 지점). 정상 상태의 99% 가
/// 0행이므로 이 보완이 없으면 오프셋이 영구히 추정되지 않는다.
const DB_TIME_REFRESH_TICKS: u64 = 30;

impl<D, S, C> InstanceCollector<D, S, C>
where
    D: TargetDb,
    S: SlowQueryStore,
    C: Clock,
{
    pub fn new(instance: Instance, db: D, store: Arc<S>, clock: C, params: CollectParams) -> Self {
        let excludes = Excludes::with_defaults(&params.monitor_db_user);
        Self {
            instance,
            db,
            store,
            clock,
            tracker: InFlightTracker::default(),
            offset: ClockOffset::default(),
            excludes,
            params,
            epoch: None,
            needs_warm: true,
            for_connection_denied: false,
            ticks_without_db_time: AtomicU64::new(0),
        }
    }

    /// 샤드 인수 시 상태를 계승한다 (F24, F14).
    pub fn restore(&mut self, offset_ms: Option<i64>, epoch: Option<u64>) {
        if let Some(ms) = offset_ms {
            self.offset = ClockOffset::restored(ms);
        }
        self.epoch = epoch;
    }

    pub fn clock_offset(&self) -> &ClockOffset {
        &self.offset
    }

    pub fn tracked_count(&self) -> usize {
        self.tracker.len()
    }

    /// 이 스레드를 추적 중인가. 자기 제외 검증과 진단에 쓴다.
    pub fn is_tracking(&self, thread_id: u64) -> bool {
        self.tracker.get(thread_id).is_some()
    }

    /// 한 tick.
    pub async fn detect_tick(&mut self) -> Result<TickStats> {
        let mut stats = TickStats::default();
        let now_ms = self.clock.now_ms();

        let probe = self
            .db
            .probe(self.params.slow_threshold_secs, &self.excludes)
            .await?;
        stats.candidates = probe.rows.len();
        stats.detect_truncated = probe.truncated;

        // 시계 오프셋 — 같은 왕복에서 받았으면 공짜다.
        match probe.db_now_ms {
            Some(db_now) => {
                self.offset.observe(db_now, now_ms, None);
                self.ticks_without_db_time.store(0, Ordering::Relaxed);
            }
            None => {
                let n = self.ticks_without_db_time.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= DB_TIME_REFRESH_TICKS {
                    // 실패해도 tick 을 실패시키지 않는다 — 오프셋은 보조 정보다.
                    if let Ok(db_now) = self.db.db_now_ms().await {
                        self.offset.observe(db_now, self.clock.now_ms(), None);
                    }
                    self.ticks_without_db_time.store(0, Ordering::Relaxed);
                }
            }
        }

        let observations: Vec<Observation> = probe
            .rows
            .iter()
            .map(|r| Observation {
                thread_id: r.id,
                time_secs: r.time_secs,
                identity: Identity {
                    // 다이제스트는 심층 조회에서 채운다.
                    digest: None,
                    db_user: r.user.clone(),
                    db_host: r.host.clone(),
                },
                schema_name: r.db.clone(),
            })
            .collect();

        let tick = self
            .tracker
            .tick(&observations, now_ms, &self.offset, probe.truncated);

        // ── 심층 조회 ──────────────────────────────────────────────────────
        // 느린 순으로 상한까지만. 나머지는 지표만 남는다.
        let mut targets = tick.needs_deep_probe.clone();
        targets.sort_by_key(|id| {
            std::cmp::Reverse(self.tracker.get(*id).map(|t| t.max_time_secs).unwrap_or(0))
        });
        if targets.len() > self.params.deep_probe_limit {
            stats.deep_probe_skipped = targets.len() - self.params.deep_probe_limit;
            targets.truncate(self.params.deep_probe_limit);
        }
        stats.deep_probed = targets.len();

        if !targets.is_empty() {
            // **실패해도 아래 확정 루프는 반드시 돈다.**
            //
            // `tracker.tick()` 은 이미 캐시에서 엔트리를 제거해 `tick.finalized` 로
            // 넘겼다. 여기서 조기 반환하면 그 Vec 이 버려지고 엔트리는 캐시에도 없으므로
            // **다시는 확정되지 않는다** — `duration_ms`·`ended_at_ms`·`Finalized` 가
            // 영구 유실되고 레코드가 `in_flight` 고아로 남는다.
            //
            // 확정 경로는 `full_sql: None, stmt: None` 이라 이 두 조회를 쓰지도 않는다.
            // 즉 실패의 영향은 **선행 저장에만** 국한돼야 한다.
            if let Err(e) = self.prefetch_save(&targets, now_ms, &mut stats).await {
                stats.deep_probe_failed += targets.len();
                tracing::warn!(
                    instance = %self.instance.id,
                    targets = targets.len(),
                    error = %e,
                    "심층 조회 실패 — 선행 저장만 건너뛴다 (확정은 계속한다)"
                );
                // 연결 문제였다면 다음 tick 도 실패한다. **tick 예산 밖에서** 풀을 다시
                // 채워야 하므로 여기서 표시만 하고 실제 warm 은 루프가 처리한다.
                self.needs_warm = true;
            }
        }

        // ── 확정 ───────────────────────────────────────────────────────────
        for (tracked, reason) in &tick.finalized {
            let out = build(CaptureInput {
                instance: &self.instance,
                tracked,
                // 확정 시점에는 스레드가 이미 사라졌으므로 새로 조회할 수 없다.
                // 선행 저장이 이미 텍스트를 넣었고, `upsert_merged` 가 속성별로 병합한다.
                full_sql: None,
                stmt: None,
                plan_json: None,
                plan_source: PlanSource::None,
                plan_error: None,
                plan_tree: None,
                policy: self.params.literal_policy,
                policy_at_ms: now_ms,
                state: if reason.observed_end() {
                    SlowQueryState::Finalized
                } else {
                    SlowQueryState::Abandoned
                },
                finalize_reason: Some(*reason),
                now_ms,
                offset: &self.offset,
                owner_worker: &self.params.worker_id,
                owner_epoch: self.epoch,
            });
            stats.masking_degraded += usize::from(out.masking_degraded);
            match self.store.upsert_merged(&out.query).await {
                Ok(_) => stats.finalized += 1,
                Err(e) => {
                    stats.store_errors += 1;
                    tracing::warn!(
                        instance = %self.instance.id,
                        thread_id = tracked.thread_id,
                        error = %e,
                        "확정 저장 실패"
                    );
                }
            }
        }

        Ok(stats)
    }

    /// 필요하면 연결 풀을 채운다. **`detect_tick` 밖에서 호출한다** — 여기서
    /// `connect` 예산(기본 5초)을 온전히 쓰기 때문이다.
    ///
    /// 실패는 로그만 남긴다. 다음 tick 이 어차피 실패하면서 다시 표시한다.
    pub async fn warm_if_needed(&mut self) {
        if !self.needs_warm {
            return;
        }
        match self.db.warm().await {
            Ok(()) => {
                self.needs_warm = false;
                tracing::debug!(instance = %self.instance.id, "연결 풀 준비됨");
            }
            Err(e) => {
                tracing::warn!(instance = %self.instance.id, error = %e, "연결 풀 준비 실패");
            }
        }
    }

    /// 대상 DB 어댑터 (읽기).
    pub fn db(&self) -> &D {
        &self.db
    }

    /// 대상 DB 어댑터. **페이크에 실패를 주입하는 테스트가 쓴다.**
    ///
    /// 실패 경로(권한 오류·타임아웃)는 실제 MySQL 로 원하는 순간에 만들 수 없는데,
    /// 이 코드베이스에서 가장 위험한 결함들이 전부 그 경로에 있었다.
    pub fn db_mut(&mut self) -> &mut D {
        &mut self.db
    }

    /// 심층 조회 + 선행 저장. **확정과 분리되어 있다** — 여기서 실패해도 호출자는
    /// 확정 루프를 계속 돌려야 한다 (그러지 않으면 확정이 영구 유실된다).
    async fn prefetch_save(
        &mut self,
        targets: &[u64],
        now_ms: dbmon_core::time::EpochMs,
        stats: &mut TickStats,
    ) -> Result<()> {
        // **`unwrap_or_default()` 로 삼키면 안 된다.** 권한 오류(1142)·타임아웃으로
        // 빈 Vec 이 되면 `sql_text=None`, `app_digest="unknown-<tid>"`, 지표 전부 None 인
        // 레코드가 저장되고 tick 은 `Ok` 를 반환해 헬스·서킷도 정상으로 본다.
        let full = self.db.full_sql(targets).await?;
        let stmts = self.db.stmt_current(targets).await?;

        for id in targets {
            let f = full.iter().find(|r| r.id == *id);
            let s = stmts.iter().find(|r| r.processlist_id == *id);
            self.tracker.record_deep_probe(
                *id,
                s.and_then(|s| s.digest.clone()),
                s.and_then(|s| s.timer_wait_ps),
                now_ms,
                &self.offset,
            );

            let plan = self.collect_plan(*id, f, s, stats).await;
            self.tracker.record_plan_attempt(*id, plan.json.is_some());

            // **선행 저장** — 정규화·마스킹을 거친 형태로 (F2).
            if let Some(tracked) = self.tracker.get(*id) {
                let out = build(CaptureInput {
                    instance: &self.instance,
                    tracked,
                    full_sql: f.and_then(|r| r.info.as_deref()),
                    stmt: s,
                    plan_json: plan.json.as_deref(),
                    plan_source: plan.source,
                    plan_error: plan.error.clone(),
                    plan_tree: None,
                    policy: self.params.literal_policy,
                    policy_at_ms: now_ms,
                    state: SlowQueryState::InFlight,
                    finalize_reason: None,
                    now_ms,
                    offset: &self.offset,
                    owner_worker: &self.params.worker_id,
                    owner_epoch: self.epoch,
                });
                stats.masking_degraded += usize::from(out.masking_degraded);
                stats.plan_redactions += out.plan_redactions;
                match self.store.upsert_merged(&out.query).await {
                    Ok(_) => stats.prefetch_saved += 1,
                    Err(e) => {
                        stats.store_errors += 1;
                        tracing::warn!(
                            instance = %self.instance.id,
                            thread_id = id,
                            error = %e,
                            "선행 저장 실패"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// 플랜을 얻는다. 세 경로를 순서대로 시도한다.
    ///
    /// # 절단된 SQL 로는 재실행하지 않는다
    ///
    /// `information_schema.PROCESSLIST.INFO` 는 65,535바이트에서 잘린다. 잘린 지점이
    /// 우연히 문법적으로 유효하면(`… WHERE a=1 AND b=2` → `… WHERE a=1`)
    /// **다른 쿼리의 플랜이 `is_exact` 로 저장된다.** 그게 최악이다 — 틀린 플랜을
    /// 정확한 플랜이라고 표시하는 것보다 플랜이 없는 편이 낫다.
    async fn collect_plan(
        &mut self,
        thread_id: u64,
        full: Option<&FullSqlRow>,
        stmt: Option<&StmtCurrentRow>,
        stats: &mut TickStats,
    ) -> PlanResult {
        // ① 실행 중 플랜. 가장 정확하지만 RDS 에서는 권한이 나오지 않는다.
        if self.params.try_for_connection && !self.for_connection_denied {
            match self.db.explain_for_connection(thread_id).await {
                Ok(ExplainOutcome::Plan(json)) => {
                    stats.plans_for_connection += 1;
                    return PlanResult::plan(json, PlanSource::ForConnection);
                }
                Ok(ExplainOutcome::Failed(f)) => {
                    if f == PlanFailure::Denied {
                        // **한 번 거부되면 이후 시도하지 않는다.** 매 tick 실패할 쿼리를
                        // 보내는 것은 대상 DB 에 대한 낭비다. RDS 에서는 이게 정상이므로
                        // 실패 카운터를 올리지 않는다.
                        self.for_connection_denied = true;
                        tracing::info!(
                            instance = %self.instance.id,
                            "EXPLAIN FOR CONNECTION 권한이 없다 — 재실행 경로로 전환한다 (RDS 의 정상 동작)"
                        );
                    }
                    if !f.allows_rerun_fallback() {
                        stats.plans_failed += 1;
                        return PlanResult::failed(f);
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "FOR CONNECTION 호출 실패");
                }
            }
        }

        // ② 원문 재실행. `SELECT` 권한만으로 된다.
        //
        // **바이트 절단된 텍스트는 쓰지 않는다.** `plan_query` 는 닫히지 않은 인용부호를
        // 잡지만, 절단 지점이 우연히 유효하면 통과한다 — 그러면 서버가 **다른 쿼리**를
        // 설명하고 우리는 그걸 `is_exact` 로 저장한다.
        let is_sql = full.and_then(|r| r.info.as_deref()).filter(|t| {
            if crate::mysql::is_info_truncated(t) {
                tracing::debug!(
                    thread_id,
                    len = t.len(),
                    "전문 SQL 이 절단됐다 — 재실행하지 않는다"
                );
                false
            } else {
                true
            }
        });
        // PS 의 무손실 텍스트도 절단될 수 있다(1,024바이트). 그건 `plan_query` 가
        // 대부분 걸러내지만, 여기서는 IS 가 없을 때의 차선으로만 쓴다.
        let sql = is_sql.or_else(|| stmt.and_then(|s| s.digest_text.as_deref()));
        let Some(sql) = sql else {
            stats.plans_failed += 1;
            return PlanResult::failed(PlanFailure::NoStatement);
        };
        // 멀티문장·버전 주석·닫히지 않은 인용부호는 여기서 거부된다.
        let Some(pq) = dbmon_normalize::plan_query(sql) else {
            stats.plans_failed += 1;
            return PlanResult::failed(PlanFailure::NotExplainable);
        };

        match self.db.explain_rerun(&pq.sql).await {
            Ok(ExplainOutcome::Plan(json)) => {
                if pq.is_exact {
                    stats.plans_rerun += 1;
                    PlanResult::plan(json, PlanSource::Rerun)
                } else {
                    // ③ DML 을 SELECT 로 변환한 **근사** 플랜.
                    stats.plans_rerun_as_select += 1;
                    PlanResult::plan(json, PlanSource::RerunAsSelect)
                }
            }
            Ok(ExplainOutcome::Failed(f)) => {
                stats.plans_failed += 1;
                PlanResult::failed(f)
            }
            Err(e) => {
                stats.plans_failed += 1;
                tracing::debug!(error = %e, "EXPLAIN 재실행 실패");
                PlanResult::failed(PlanFailure::Other(0))
            }
        }
    }

    /// 그레이스풀 셧다운·리더 상실 시 진행 중 항목을 전부 확정한다.
    pub async fn drain(&mut self) -> TickStats {
        let mut stats = TickStats::default();
        let now_ms = self.clock.now_ms();
        for (tracked, reason) in self.tracker.drain() {
            let out = build(CaptureInput {
                instance: &self.instance,
                tracked: &tracked,
                full_sql: None,
                stmt: None,
                plan_json: None,
                plan_source: PlanSource::None,
                plan_error: None,
                plan_tree: None,
                policy: self.params.literal_policy,
                policy_at_ms: now_ms,
                state: SlowQueryState::Abandoned,
                finalize_reason: Some(reason),
                now_ms,
                offset: &self.offset,
                owner_worker: &self.params.worker_id,
                owner_epoch: self.epoch,
            });
            if self.store.upsert_merged(&out.query).await.is_ok() {
                stats.finalized += 1;
            } else {
                stats.store_errors += 1;
            }
        }
        stats
    }
}

struct PlanResult {
    json: Option<String>,
    source: PlanSource,
    error: Option<String>,
}

impl PlanResult {
    fn plan(json: String, source: PlanSource) -> Self {
        Self {
            json: Some(json),
            source,
            error: None,
        }
    }
    fn failed(f: PlanFailure) -> Self {
        Self {
            json: None,
            source: PlanSource::None,
            error: Some(f.as_str()),
        }
    }
}

/// 시각을 계산할 때 쓰는 상수. 테스트가 참조한다.
pub const fn db_time_refresh_ticks() -> u64 {
    DB_TIME_REFRESH_TICKS
}
