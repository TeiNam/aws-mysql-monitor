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
use dbmon_core::inflight::{FinalizeReason, Identity, InFlightTracker, Observation};
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
    /// 엔트리 수 상한에 걸려 강제 확정된 수.
    ///
    /// 0 이 아니면 동시 슬로우 쿼리가 추적 상한을 넘었다는 뜻이다. 카운터를 여기 두는
    /// 이유: `InFlightTracker` 안에만 있으면 아무도 읽지 않고, 그러면 "관측 가능하다" 는
    /// 주장이 성립하지 않는다.
    pub evicted: usize,
    /// 관측 중인 엔트리만으로 상한을 넘겨 상한을 일시적으로 초과했다.
    ///
    /// 관측 중인 실행을 버리지 않기로 했으므로 이 경우 캐시가 상한을 넘는다.
    /// 참이면 `max_entries` 나 `collector.detect_limit` 설정을 재검토해야 한다.
    pub over_entry_cap: bool,
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
    /// **아직 관측 중이라고 다시 쓴 레코드 수** (고아 오판 방지 — `heartbeat` 참조).
    pub heartbeats: usize,
    /// 하트비트의 **조건이 깨진 수** — 그 자리에 진행 중 레코드가 없었다.
    ///
    /// 정상적으로도 발생한다(슬로우로그가 먼저 확정한 실행의 스레드가 아직 살아 있을 때).
    /// 그런데 **레코드가 우리가 계산한 키에 없는 경우**도 같은 값으로 보인다 — 그때 그
    /// 레코드는 하트비트를 못 받아 임계 뒤에 고아로 확정된다. 세지 않으면 그 차이를
    /// 나중에 조사할 방법이 없다.
    pub heartbeats_absent: usize,
}

/// tick 당 **선행 저장**(자리 얻기) 상한.
///
/// 배치 조회는 목록이 커도 왕복 수가 같으므로 이 상한은 DynamoDB 쓰기 예산이다.
/// `deep_probe_limit`(플랜 예산)와 나눠 두는 이유는 `detect_tick` 의 표에 있다.
const ACQUIRE_LIMIT: usize = 200;

/// tick 당 하트비트 쓰기 상한. 동시 슬로우 쿼리가 폭주해도 쓰기 예산을 묶는다.
///
/// 주기가 임계의 절반보다 짧으므로(`HEARTBEAT_INTERVAL_MS`) 상한에 걸린 항목도 다음
/// tick 에 다시 후보가 된다 — 한 번 밀린 것이 곧바로 고아가 되지는 않는다.
const HEARTBEAT_LIMIT: usize = 200;

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
    /// 대상의 `sql_mode` 가 **우리와 문자열 경계를 다르게 본다.**
    ///
    /// `ANSI_QUOTES` 또는 `NO_BACKSLASH_ESCAPES` 가 있으면 같은 SQL 문자열을 앱과 우리가
    /// 다르게 파싱한다. 그 상태로 얻은 플랜은 **다른 쿼리의 플랜**이므로 정확하다고
    /// 표시할 수 없다 ([`TargetDb::target_sql_mode`]).
    lexical_divergence: bool,
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
            lexical_divergence: false,
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

        // **실패하면 warm 을 표시한다.** `detect_total`(tick 예산 800ms)이
        // `connect`(5초)보다 작은 것이 정당한 근거는 "연결 수립을 `warm()` 이 tick 밖에서
        // 한다" 는 것이다. 그런데 연결 문제는 바로 이 `probe` 에서 드러나므로, 여기서
        // 표시하지 않으면 콜드 상태가 영구히 회복되지 않는다 — 2차 M4 가 그대로 재발한다.
        let probe = match self
            .db
            .probe(self.params.slow_threshold_secs, &self.excludes)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                // **권한 오류에는 warm 을 표시하지 않는다.** 재연결로 해결되지 않으므로
                // 매 tick `warm()` + `target_sql_mode()` 왕복을 낭비한다.
                // 사람이 GRANT 를 고쳐야 하는 상태다.
                if e.is_retryable() {
                    self.needs_warm = true;
                }
                return Err(e);
            }
        };
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

        stats.evicted = tick
            .finalized
            .iter()
            .filter(|(_, r)| *r == FinalizeReason::Evicted)
            .count();
        stats.over_entry_cap = self.tracker.is_over_cap();
        if stats.evicted > 0 || stats.over_entry_cap {
            tracing::warn!(
                instance = %self.instance.id,
                evicted = stats.evicted,
                over_cap = stats.over_entry_cap,
                tracked = self.tracker.len(),
                "추적 상한에 걸렸다 — 동시 슬로우 쿼리가 상한을 넘었다"
            );
        }

        // ── 심층 조회 ──────────────────────────────────────────────────────
        //
        // **두 예산을 나눈다.**
        //
        // | 무엇 | 비용 | 상한 |
        // |---|---|---|
        // | 자리 얻기(선행 저장) | 배치 조회 2회 + 항목당 쓰기 1회 | `ACQUIRE_LIMIT` |
        // | 플랜 얻기(`EXPLAIN`) | **항목당 왕복** | `deep_probe_limit` |
        //
        // 배치 조회(`full_sql`·`stmt_current`)는 `ID IN (…)` 이라 목록이 커도 왕복 수가
        // 같다. 그런데 전에는 두 일이 `deep_probe_limit` 하나를 나눠 써서, 상한이 작으면
        // **자리를 못 얻은 항목이 210초 안에 저장되지 못했다** — 리더 교체 직후 이전
        // 리더가 남긴 살아 있는 행이 버려진다(교차 리뷰 27라운드가 10행으로 재현했다).
        // 반대로 자리 없는 것을 먼저 보게만 하면 플랜 재시도가 굶는다(같은 라운드).
        let mut plan_targets: Vec<u64> = tick
            .needs_deep_probe
            .iter()
            .copied()
            .filter(|id| self.tracker.wants_plan(*id))
            .collect();
        // 플랜은 비싸므로 느린 것부터.
        plan_targets.sort_by_key(|id| {
            std::cmp::Reverse(self.tracker.get(*id).map(|t| t.max_time_secs).unwrap_or(0))
        });
        if plan_targets.len() > self.params.deep_probe_limit {
            stats.deep_probe_skipped = plan_targets.len() - self.params.deep_probe_limit;
            plan_targets.truncate(self.params.deep_probe_limit);
        }
        // **`deep_probed` 는 이름대로 플랜 조회 수다.** 쓰기 대상 수를 여기 넣으면
        // `deep_probe_limit` 과 비교할 수 없고, 지표가 상한을 넘긴 것처럼 보인다.
        stats.deep_probed = plan_targets.len();

        // 쓰기 대상 = 자리를 얻어야 하는 것 ∪ 플랜을 방금 얻은 것.
        let mut targets: Vec<u64> = tick
            .needs_deep_probe
            .iter()
            .copied()
            .filter(|id| {
                self.tracker
                    .get(*id)
                    .is_some_and(|t| t.storage_key.is_none())
                    || plan_targets.contains(id)
            })
            .collect();
        // 자리 없는 것부터 — 상한에 걸리면 그쪽이 살아 있는 행을 지킨다.
        targets.sort_by_key(|id| {
            let t = self.tracker.get(*id);
            (
                t.is_some_and(|t| t.storage_key.is_some()),
                std::cmp::Reverse(t.map(|t| t.max_time_secs).unwrap_or(0)),
            )
        });
        if targets.len() > ACQUIRE_LIMIT {
            // **조용히 자르지 않는다.** 잘린 항목은 이번 tick 에 자리를 못 얻는다.
            tracing::warn!(
                instance = %self.instance.id,
                pending = targets.len(),
                limit = ACQUIRE_LIMIT,
                "선행 저장 상한에 걸렸다 — 자리 없는 것부터 처리한다"
            );
            targets.truncate(ACQUIRE_LIMIT);
        }
        let plan_targets: std::collections::BTreeSet<u64> = plan_targets.into_iter().collect();

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
            if let Err(e) = self
                .prefetch_save(&targets, &plan_targets, now_ms, &mut stats)
                .await
            {
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

        self.heartbeat(now_ms, &mut stats).await;

        Ok(stats)
    }

    /// **아직 관측 중이라고 저장소에 다시 말한다.**
    ///
    /// # 왜 필요한가
    ///
    /// 고아 스윕(F4)은 저장된 `last_seen_at_ms` 의 침묵으로 판정한다. 그런데 선행 저장은
    /// **심층 조회 대상일 때만** 일어나고, 플랜을 확보하면 그 대상에서 빠진다 — 살아 있는
    /// 긴 쿼리의 저장된 값이 그 시점에 굳고, 임계를 넘기면 **살아 있는 쿼리가
    /// `abandoned` 로 확정된다.** `Abandoned` 는 나중의 `InFlight` 를 이기므로 되돌릴 수
    /// 없다(교차 리뷰 22라운드의 배포 차단 항목).
    ///
    /// 앞선 라운드들은 이걸 스윕 쪽에서 "지금 도는 태스크의 것인가" 를 벽시계로 추론해
    /// 가리려 했는데, 그 추론이 20~22라운드에서 **양방향으로** 틀렸다(살아 있는 것을
    /// 버리거나, 아무도 확정하지 않을 것을 영구히 가렸다). 관측하고 있다는 사실은 여기서만
    /// 알 수 있으므로 **여기서 말한다.** 그러면 침묵이 실제로 "아무도 관측하지 않는다" 를
    /// 뜻하고 스윕은 시각 비교만으로 옳다.
    ///
    /// # 생존 신호만 쓴다
    ///
    /// [`SlowQueryStore::touch_in_flight`] 는 속성 하나를 조건부로 올린다. 처음에는
    /// `upsert_merged` 로 레코드 전체를 다시 썼는데, 그러면 (1) 없는 레코드를 **만들고**
    /// (2) SQL 없는 레코드라 정책이 `off` 로 고정돼 나중 SQL 을 버리고 (3) 모든 저장이
    /// 방송되므로 브라우저가 15초마다 목록을 무효화했다 — 23라운드가 세 개를 함께 잡았다.
    ///
    /// 실패는 삼킨다 — 다음 tick 이 같은 항목을 다시 고른다(`record_saved` 를 부르지
    /// 않으므로). 스윕의 확정 쓰기도 같은 저장소를 쓰므로, 저장소가 죽어 있으면 여기도
    /// 실패하지만 **버려지지도 않는다.**
    async fn heartbeat(&mut self, now_ms: dbmon_core::time::EpochMs, stats: &mut TickStats) {
        let mut stale = self
            .tracker
            .needs_heartbeat(now_ms, crate::orphan::HEARTBEAT_INTERVAL_MS);
        if stale.len() > HEARTBEAT_LIMIT {
            // **조용히 자르지 않는다.** 잘린 항목은 임계를 넘으면 고아로 확정되므로,
            // 왜 그랬는지가 로그에 남아야 한다.
            tracing::warn!(
                instance = %self.instance.id,
                stale = stale.len(),
                limit = HEARTBEAT_LIMIT,
                "하트비트 상한에 걸렸다 — 오래된 것부터만 갱신한다 (남은 것은 고아로 확정될 수 있다)"
            );
            stale.truncate(HEARTBEAT_LIMIT);
        }

        for id in stale {
            // **관측 시각을 쓴다 — `now_ms` 가 아니다.** 관측이 끊긴 항목(잘린 tick 에서
            // 못 본 스레드)에 `now` 를 쓰면 "보고 있다" 는 거짓이 저장되고, 그 레코드는
            // 아무도 확정하지 않는데 영원히 살아 있는 것으로 보인다.
            //
            // 자리는 **쓰기가 알려 준 것**이다(`needs_heartbeat` 가 자리를 아는 항목만 준다).
            let Some((key, last_seen_at_ms, duration_ms, duration_source)) =
                self.tracker.get(id).and_then(|t| {
                    t.storage_key
                        .clone()
                        .map(|k| (k, t.last_seen_at_ms, t.duration_ms(), t.duration_source()))
                })
            else {
                continue;
            };
            // **결과와 무관하게 시도를 기록한다.** 성공으로 기록하면 실패한 항목이 줄의
            // 앞자리를 차지해 갱신이 필요한 항목이 굶는다(25라운드).
            self.tracker.record_touch_attempt(id, now_ms);
            match self
                .store
                .touch_in_flight(&key, last_seen_at_ms, duration_ms, duration_source)
                .await
            {
                Ok(updated) => {
                    stats.heartbeats += usize::from(updated);
                    stats.heartbeats_absent += usize::from(!updated);
                }
                Err(e) => {
                    stats.store_errors += 1;
                    tracing::warn!(
                        instance = %self.instance.id,
                        thread_id = id,
                        error = %e,
                        "하트비트 저장 실패 — 다음 tick 에 다시 시도한다"
                    );
                }
            }
        }
    }

    /// 필요하면 연결 풀을 채운다. **`detect_tick` 밖에서 호출한다** — 여기서
    /// `connect` 예산(기본 5초)을 온전히 쓰기 때문이다.
    ///
    /// 실패는 로그만 남긴다. 다음 tick 이 어차피 실패하면서 다시 표시한다.
    /// 연결 집합을 교체한다. **IAM 토큰 갱신 경로다.**
    ///
    /// # 왜 수집기를 새로 만들지 않는가
    ///
    /// IAM 토큰은 15분 만료다. 토큰은 **연결 수립 시점**에만 쓰이므로 기존 연결은
    /// 살아 있지만, 그 뒤에 만들어지는 새 연결은 만료된 토큰으로 인증에 실패한다.
    /// 그래서 주기적으로 풀을 다시 만들어야 한다.
    ///
    /// 그때 수집기를 통째로 새로 만들면 **진행 중 캐시가 사라진다** — 그 순간 실행
    /// 중이던 쿼리가 전부 `in_flight` 고아가 되고, 고아 정리(F4)가 TTL 까지 그것들을
    /// 화면에 "실행 중" 으로 남긴다. 15분마다 그러면 안 된다.
    ///
    /// 그래서 연결만 갈아 끼우고 추적기·시계 보정은 유지한다. 새 풀은 콜드이므로
    /// `needs_warm` 을 세워 다음 tick 밖에서 채운다.
    ///
    /// # 이전 연결을 **돌려준다** — 호출부가 정리해야 한다
    ///
    /// 그냥 드롭하면 커넥션이 `COM_QUIT` 없이 사라져 **감시 대상 DB 의
    /// `Aborted_clients` 가 오르고 에러 로그가 오염된다.** 인스턴스당 10분마다
    /// 풀 2개씩이면 "대상에 부하를 주지 않는다" 는 주장과 정면으로 부딪친다.
    ///
    /// `TargetDb` 포트에는 close 가 없다(그게 맞다 — 도메인 개념이 아니다).
    /// 그래서 반환해 **구체 타입을 아는 호출부가** `disconnect().await` 하게 한다.
    /// 반환값을 무시하면 `#[must_use]` 가 경고한다.
    #[must_use = "이전 연결을 반드시 정리해야 한다 — 드롭하면 대상 DB 에 Aborted_clients 가 쌓인다"]
    pub fn replace_db(&mut self, db: D) -> D {
        self.needs_warm = true;
        std::mem::replace(&mut self.db, db)
    }

    pub async fn warm_if_needed(&mut self) {
        if !self.needs_warm {
            return;
        }
        match self.db.warm().await {
            Ok(()) => {
                self.needs_warm = false;
                tracing::debug!(instance = %self.instance.id, "연결 풀 준비됨");
                // 같은 기회에 어휘 발산 위험을 갱신한다. tick 밖이므로 비용이 자유롭다.
                self.refresh_lexical_divergence().await;
            }
            Err(e) => {
                tracing::warn!(instance = %self.instance.id, error = %e, "연결 풀 준비 실패");
            }
        }
    }

    /// 대상의 `sql_mode` 를 읽어 어휘 발산 위험을 갱신한다.
    ///
    /// 실패하면 **위험이 있다고 본다** — 모르는 상태에서 정확하다고 표시하는 것보다
    /// 근사로 표시하는 편이 안전하다.
    async fn refresh_lexical_divergence(&mut self) {
        const RISKY: [&str; 2] = ["ANSI_QUOTES", "NO_BACKSLASH_ESCAPES"];
        match self.db.target_sql_mode().await {
            Ok(mode) => {
                let upper = mode.to_ascii_uppercase();
                let risky = RISKY.iter().any(|m| upper.contains(m));
                if risky != self.lexical_divergence {
                    tracing::info!(
                        instance = %self.instance.id,
                        sql_mode = %mode,
                        risky,
                        "대상 sql_mode 어휘 발산 위험 갱신"
                    );
                }
                self.lexical_divergence = risky;
            }
            Err(e) => {
                tracing::warn!(
                    instance = %self.instance.id,
                    error = %e,
                    "sql_mode 를 읽지 못했다 — 플랜을 근사로 표시한다"
                );
                self.lexical_divergence = true;
            }
        }
    }

    /// 추적 엔트리 상한을 바꾼다 (테스트용).
    pub fn set_max_tracked_entries(&mut self, n: usize) {
        self.tracker = InFlightTracker::default().with_max_entries(n);
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
        plan_targets: &std::collections::BTreeSet<u64>,
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

            // **플랜 시도가 끝난 항목에는 다시 시도하지 않는다.**
            //
            // 자리를 얻기 위해 대상에 남은 항목이 있으므로(위 `wants_plan` 참고),
            // 여기서 걸러야 이미 포기한 플랜을 매 tick 다시 시도하지 않는다 — 그건
            // 대상 DB 에 실제 부하다.
            let plan = if plan_targets.contains(id) {
                let plan = self.collect_plan(*id, f, s, stats).await;
                // **시도만 기록한다.** "얻었다" 는 저장이 성공한 뒤에 센다 — 그러지 않으면
                // 플랜을 들고 있는데 저장이 실패한 실행이 계획 없이 남는다(27라운드).
                self.tracker.record_plan_attempt(*id);
                plan
            } else {
                // 자리를 얻으려고 남은 항목이다. 플랜을 이미 얻었으면 사유가 없고,
                // 시도 상한에 걸렸으면 **그 사실을 남긴다** — 비면 화면이 "계획이 없다" 만
                // 말하고 왜 없는지는 아무도 모른다.
                PlanResult::not_attempted(self.tracker.wants_plan(*id))
            };

            // **선행 저장** — 정규화·마스킹을 거친 형태로 (F2).
            //
            // `map` 으로 소유값을 만들어 **추적기 대여를 여기서 끝낸다** — 저장 성공을
            // `record_saved` 로 표시해야 하므로(하트비트 기준) 뒤에서 가변 대여가 필요하다.
            let Some(out) = self.tracker.get(*id).map(|tracked| {
                build(CaptureInput {
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
                })
            }) else {
                continue;
            };
            stats.masking_degraded += usize::from(out.masking_degraded);
            stats.plan_redactions += out.plan_redactions;
            // **키를 받는 형태로 저장한다.** 하트비트는 계산한 키가 아니라 이 자리에만
            // 신호를 올린다 — 추정으로 만든 키는 관측자가 바뀌면 어긋난다.
            match self.store.upsert_merged_keyed(&out.query).await {
                Ok((_, key)) => {
                    stats.prefetch_saved += 1;
                    self.tracker.record_saved(*id, now_ms, key);
                    // **저장이 성공한 뒤에** 플랜을 얻었다고 센다.
                    if plan.json.is_some() {
                        self.tracker.record_plan_saved(*id);
                    }
                }
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
        // **폴백은 PS 의 무손실 `SQL_TEXT` 다.**
        //
        // 예전에는 `digest_text` 로 폴백했는데 거기엔 `?`·`(...)` 자리표가 남아 있어
        // `plan_query` 가 거부한다(2차 M5) → 폴백이 **항상 실패**하면서 `plan_attempts` 를
        // 소모했다. 3회 뒤에는 그 항목이 영구히 "플랜 없음" 으로 굳는다.
        //
        // `stmt.sql_text` 는 실제 실행 가능한 SQL 이다(1,024바이트에서 잘릴 수 있고,
        // 잘리면 `plan_query` 가 대부분 걸러낸다). 이쪽이 의도에 맞다.
        let sql = is_sql.or_else(|| stmt.and_then(|s| s.sql_text.as_deref()));
        let Some(sql) = sql else {
            stats.plans_failed += 1;
            return PlanResult::failed(PlanFailure::NoStatement);
        };
        // 멀티문장·버전 주석·닫히지 않은 인용부호는 여기서 거부된다.
        let Some(pq) = dbmon_normalize::plan_query(sql) else {
            stats.plans_failed += 1;
            return PlanResult::failed(PlanFailure::NotExplainable);
        };

        // **그 문장이 돌던 기본 스키마를 함께 넘긴다.**
        //
        // 애플리케이션은 보통 스키마를 잡고 접속하므로 SQL 원문에 스키마가 없다
        // (`FROM orders`). 그대로 재실행하면 1046 `No database selected` 이고, 실측에서
        // 로컬에 쌓인 플랜이 전부 테이블 없는 쿼리(`SELECT sleep(…)`)뿐이었던 이유다.
        // 처리목록의 `db` 를 먼저 쓰고, 없으면 `events_statements_current` 의 현재 스키마.
        let schema = full
            .and_then(|r| r.db.as_deref())
            .or_else(|| stmt.and_then(|s| s.current_schema.as_deref()));
        match self.db.explain_rerun(&pq.sql, schema).await {
            Ok(ExplainOutcome::Plan(json)) => {
                // **어휘 발산이 있으면 정확하다고 표시할 수 없다.** 앱과 우리가 같은
                // 문자열을 다르게 파싱하므로, 얻은 플랜은 다른 쿼리의 플랜이다.
                if pq.is_exact && !self.lexical_divergence {
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
    /// **플랜을 시도하지 않았다.** 자리를 얻기 위해 대상에 남은 항목이다.
    ///
    /// `still_wants` 가 거짓이면 이미 얻었거나 시도 상한에 걸렸다는 뜻이다. 상한에 걸린
    /// 경우 사유를 비워 두면 화면이 "저장된 계획이 없다" 만 말하고 **왜 없는지**는
    /// 아무도 모른다(교차 리뷰 27라운드).
    fn not_attempted(still_wants: bool) -> Self {
        Self {
            json: None,
            source: PlanSource::None,
            error: if still_wants {
                None
            } else {
                Some("attempts_exhausted".into())
            },
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
