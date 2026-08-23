//! 다이제스트 델타 계산과 시간 롤업
//! ([05 §2.5.2](../../../.claude/docs/05-collector.md), [04 §2.3](../../../.claude/docs/04-data-model.md)).
//!
//! # 단위 주의
//!
//! `events_statements_summary_by_digest` 의 타이머 컬럼은 **피코초**다.
//! 1세대의 흔한 버그가 마이크로초로 착각하는 것이었다 — 1000배 틀린다.
//! 여기서는 델타를 만들 때 **한 번만** ms 로 바꾸고 이후는 ms 로만 다룬다.
//! 그래야 `_other = 전체 − 저장분` 이 같은 단위에서 계산되어 총량 보존이 정확히 성립한다(R9).

use crate::time::EpochMs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 피코초 → 밀리초.
pub const PS_PER_MS: u64 = 1_000_000_000;

/// 상위 N 에 들지 못한 전부를 합산하는 가상 다이제스트.
pub const OTHER_DIGEST: &str = "_other";

/// `events_statements_summary_by_digest` 한 행의 누적 스냅샷.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DigestSnapshotRow {
    pub schema_name: Option<String>,
    /// MySQL 이 계산한 `DIGEST` (64 hex).
    pub mysql_digest: String,
    pub count_star: u64,
    /// 피코초.
    pub sum_timer_wait_ps: u64,
    pub min_timer_wait_ps: u64,
    pub max_timer_wait_ps: u64,
    /// 피코초.
    pub sum_lock_time_ps: u64,
    pub sum_errors: u64,
    pub sum_warnings: u64,
    pub sum_rows_affected: u64,
    pub sum_rows_sent: u64,
    pub sum_rows_examined: u64,
    pub sum_created_tmp_tables: u64,
    pub sum_created_tmp_disk_tables: u64,
    pub sum_select_full_join: u64,
    pub sum_select_scan: u64,
    pub sum_sort_merge_passes: u64,
    pub sum_no_index_used: u64,
    pub sum_no_good_index_used: u64,
    pub first_seen_ms: EpochMs,
    pub last_seen_ms: EpochMs,
    /// `QUANTILE_95` — **누적 히스토그램에서 유도된 현재 추정값이다. 델타를 계산하면 안 된다.**
    pub quantile_95_ps: Option<u64>,
    pub quantile_99_ps: Option<u64>,
}

impl DigestSnapshotRow {
    fn key(&self) -> (Option<String>, String) {
        (self.schema_name.clone(), self.mysql_digest.clone())
    }
}

/// 두 스냅샷 사이의 변화량. **모든 시간 단위는 ms 다.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DigestDelta {
    pub exec_count: u64,
    pub total_time_ms: i64,
    pub total_lock_time_ms: i64,
    pub errors: u64,
    pub warnings: u64,
    pub rows_affected: u64,
    pub rows_sent: u64,
    pub rows_examined: u64,
    pub tmp_tables: u64,
    pub tmp_disk_tables: u64,
    pub full_join: u64,
    pub select_scan: u64,
    pub sort_merge_passes: u64,
    pub no_index_used: u64,
    pub no_good_index_used: u64,
    /// 이 구간에서 관측된 최대 실행시간이 **아니다.** 서버 시작 이후 전체 기간 최대다.
    /// 라벨을 다르게 붙여야 리포트 수치가 틀리지 않는다.
    pub max_time_ms_all_time: i64,
    /// 스냅샷 시점의 P95 추정값. 델타가 아니다.
    pub p95_ms_snapshot: Option<i64>,
}

impl DigestDelta {
    pub fn avg_time_ms(&self) -> f64 {
        if self.exec_count == 0 {
            0.0
        } else {
            self.total_time_ms as f64 / self.exec_count as f64
        }
    }

    pub fn add(&mut self, o: &DigestDelta) {
        self.exec_count += o.exec_count;
        self.total_time_ms += o.total_time_ms;
        self.total_lock_time_ms += o.total_lock_time_ms;
        self.errors += o.errors;
        self.warnings += o.warnings;
        self.rows_affected += o.rows_affected;
        self.rows_sent += o.rows_sent;
        self.rows_examined += o.rows_examined;
        self.tmp_tables += o.tmp_tables;
        self.tmp_disk_tables += o.tmp_disk_tables;
        self.full_join += o.full_join;
        self.select_scan += o.select_scan;
        self.sort_merge_passes += o.sort_merge_passes;
        self.no_index_used += o.no_index_used;
        self.no_good_index_used += o.no_good_index_used;
        self.max_time_ms_all_time = self.max_time_ms_all_time.max(o.max_time_ms_all_time);
        self.p95_ms_snapshot = match (self.p95_ms_snapshot, o.p95_ms_snapshot) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

/// 델타를 어떻게 산출했는가. 이벤트 발행 판단에 쓴다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// 정상 차분.
    Incremental,
    /// 처음 본 다이제스트. 기준선만 세우고 델타는 0.
    NewBaseline,
    /// 처음 봤지만 `FIRST_SEEN` 이 이전 스냅샷보다 나중 → 전량이 이 구간에서 발생했다.
    NewWithinWindow,
    /// 카운터 리셋 (서버 재시작 / 테이블 오버플로 / 수동 TRUNCATE).
    Reset,
}

impl DeltaKind {
    pub fn is_reset(self) -> bool {
        self == Self::Reset
    }
}

/// 델타를 계산한다.
///
/// `prev_snapshot_ms` 는 **대상 DB 시각** 기준의 이전 스냅샷 시각이다
/// (우리 시계를 쓰지 않는다 — [05 §2.5.3](../../../.claude/docs/05-collector.md)).
pub fn compute_delta(
    prev: Option<&DigestSnapshotRow>,
    cur: &DigestSnapshotRow,
    prev_snapshot_ms: EpochMs,
) -> (DigestDelta, DeltaKind) {
    let full = |row: &DigestSnapshotRow| DigestDelta {
        exec_count: row.count_star,
        total_time_ms: ps_to_ms(row.sum_timer_wait_ps),
        total_lock_time_ms: ps_to_ms(row.sum_lock_time_ps),
        errors: row.sum_errors,
        warnings: row.sum_warnings,
        rows_affected: row.sum_rows_affected,
        rows_sent: row.sum_rows_sent,
        rows_examined: row.sum_rows_examined,
        tmp_tables: row.sum_created_tmp_tables,
        tmp_disk_tables: row.sum_created_tmp_disk_tables,
        full_join: row.sum_select_full_join,
        select_scan: row.sum_select_scan,
        sort_merge_passes: row.sum_sort_merge_passes,
        no_index_used: row.sum_no_index_used,
        no_good_index_used: row.sum_no_good_index_used,
        max_time_ms_all_time: ps_to_ms(row.max_timer_wait_ps),
        p95_ms_snapshot: row.quantile_95_ps.map(ps_to_ms),
    };

    match prev {
        None => {
            // 이전 스냅샷 이후에 처음 실행된 다이제스트면 전량이 이 구간의 것이다.
            if cur.first_seen_ms > prev_snapshot_ms {
                (full(cur), DeltaKind::NewWithinWindow)
            } else {
                // 워커 재시작 직후 등. 기준선만 세우고 0을 반환한다.
                (
                    DigestDelta {
                        max_time_ms_all_time: ps_to_ms(cur.max_timer_wait_ps),
                        p95_ms_snapshot: cur.quantile_95_ps.map(ps_to_ms),
                        ..Default::default()
                    },
                    DeltaKind::NewBaseline,
                )
            }
        }
        // 리셋: 카운터가 줄었다. 음수 델타를 만들면 집계가 오염된다 (R8).
        Some(p) if cur.count_star < p.count_star => (full(cur), DeltaKind::Reset),
        Some(p) => (
            DigestDelta {
                exec_count: cur.count_star - p.count_star,
                total_time_ms: ps_to_ms(cur.sum_timer_wait_ps.saturating_sub(p.sum_timer_wait_ps)),
                total_lock_time_ms: ps_to_ms(
                    cur.sum_lock_time_ps.saturating_sub(p.sum_lock_time_ps),
                ),
                errors: cur.sum_errors.saturating_sub(p.sum_errors),
                warnings: cur.sum_warnings.saturating_sub(p.sum_warnings),
                rows_affected: cur.sum_rows_affected.saturating_sub(p.sum_rows_affected),
                rows_sent: cur.sum_rows_sent.saturating_sub(p.sum_rows_sent),
                rows_examined: cur.sum_rows_examined.saturating_sub(p.sum_rows_examined),
                tmp_tables: cur
                    .sum_created_tmp_tables
                    .saturating_sub(p.sum_created_tmp_tables),
                tmp_disk_tables: cur
                    .sum_created_tmp_disk_tables
                    .saturating_sub(p.sum_created_tmp_disk_tables),
                full_join: cur
                    .sum_select_full_join
                    .saturating_sub(p.sum_select_full_join),
                select_scan: cur.sum_select_scan.saturating_sub(p.sum_select_scan),
                sort_merge_passes: cur
                    .sum_sort_merge_passes
                    .saturating_sub(p.sum_sort_merge_passes),
                no_index_used: cur.sum_no_index_used.saturating_sub(p.sum_no_index_used),
                no_good_index_used: cur
                    .sum_no_good_index_used
                    .saturating_sub(p.sum_no_good_index_used),
                max_time_ms_all_time: ps_to_ms(cur.max_timer_wait_ps),
                p95_ms_snapshot: cur.quantile_95_ps.map(ps_to_ms),
            },
            DeltaKind::Incremental,
        ),
    }
}

pub fn ps_to_ms(ps: u64) -> i64 {
    (ps / PS_PER_MS) as i64
}

/// 이전 스냅샷 보관 — 워커 메모리에만 둔다([02 §6](../../../.claude/docs/02-architecture.md)).
#[derive(Debug, Default)]
pub struct SnapshotCache {
    rows: BTreeMap<(Option<String>, String), DigestSnapshotRow>,
    /// 대상 DB 시각 기준의 마지막 **성공** 스냅샷 시각.
    /// 실패해도 갱신하지 않아 구간이 이어진다 (M4-14a).
    pub last_success_db_ms: Option<EpochMs>,
}

impl SnapshotCache {
    /// 다음 `LAST_SEEN` 필터 값. 안전 여유 5초를 뺀다 — 중복 관측은 델타 0으로 흡수되지만
    /// 누락은 복구 불가다.
    pub const SAFETY_MARGIN_MS: i64 = 5_000;

    pub fn last_seen_filter_ms(&self) -> Option<EpochMs> {
        self.last_success_db_ms.map(|t| t - Self::SAFETY_MARGIN_MS)
    }

    pub fn get(&self, row: &DigestSnapshotRow) -> Option<&DigestSnapshotRow> {
        self.rows.get(&row.key())
    }

    /// 성공한 스냅샷을 반영한다.
    pub fn commit(&mut self, rows: Vec<DigestSnapshotRow>, db_now_ms: EpochMs) {
        for r in rows {
            self.rows.insert(r.key(), r);
        }
        self.last_success_db_ms = Some(db_now_ms);
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(digest: &str, count: u64, sum_ms: u64) -> DigestSnapshotRow {
        DigestSnapshotRow {
            mysql_digest: digest.into(),
            count_star: count,
            sum_timer_wait_ps: sum_ms * PS_PER_MS,
            first_seen_ms: 1_000,
            last_seen_ms: 2_000,
            ..Default::default()
        }
    }

    #[test]
    fn picoseconds_convert_correctly() {
        // 1세대 버그: 마이크로초로 착각하면 1000배 틀린다.
        assert_eq!(ps_to_ms(4_213_000_000_000), 4_213);
        assert_eq!(ps_to_ms(PS_PER_MS), 1);
        assert_eq!(ps_to_ms(999_999_999), 0);
    }

    #[test]
    fn incremental_delta() {
        let prev = row("a", 100, 5_000);
        let cur = row("a", 150, 8_000);
        let (d, k) = compute_delta(Some(&prev), &cur, 0);
        assert_eq!(k, DeltaKind::Incremental);
        assert_eq!(d.exec_count, 50);
        assert_eq!(d.total_time_ms, 3_000);
        assert_eq!(d.avg_time_ms(), 60.0);
    }

    /// R8 — 리셋 시 음수 델타가 나오지 않아야 한다.
    #[test]
    fn r8_reset_never_produces_negative_delta() {
        let prev = row("a", 1_000, 50_000);
        let cur = row("a", 7, 300); // 서버 재시작
        let (d, k) = compute_delta(Some(&prev), &cur, 0);
        assert_eq!(k, DeltaKind::Reset);
        assert!(k.is_reset());
        assert_eq!(d.exec_count, 7, "리셋 후 값 전체를 델타로 본다");
        assert_eq!(d.total_time_ms, 300);
        assert!(d.total_time_ms >= 0);
    }

    #[test]
    fn new_digest_sets_baseline_only() {
        let cur = row("new", 42, 9_000); // first_seen = 1_000
        let (d, k) = compute_delta(None, &cur, 5_000); // 이전 스냅샷이 first_seen 보다 나중
        assert_eq!(k, DeltaKind::NewBaseline);
        assert_eq!(d.exec_count, 0, "기준선만 세운다");
        assert_eq!(d.total_time_ms, 0);
    }

    #[test]
    fn new_digest_within_window_counts_fully() {
        let mut cur = row("new", 42, 9_000);
        cur.first_seen_ms = 9_000; // 이전 스냅샷(5_000) 이후에 처음 실행됨
        let (d, k) = compute_delta(None, &cur, 5_000);
        assert_eq!(k, DeltaKind::NewWithinWindow);
        assert_eq!(d.exec_count, 42, "이 구간에서 발생한 전량");
        assert_eq!(d.total_time_ms, 9_000);
    }

    #[test]
    fn quantiles_are_snapshots_not_deltas() {
        let mut prev = row("a", 100, 5_000);
        prev.quantile_95_ps = Some(100 * PS_PER_MS);
        let mut cur = row("a", 150, 8_000);
        cur.quantile_95_ps = Some(120 * PS_PER_MS);
        let (d, _) = compute_delta(Some(&prev), &cur, 0);
        assert_eq!(d.p95_ms_snapshot, Some(120), "차분하면 안 된다");
    }

    #[test]
    fn max_timer_is_labeled_all_time() {
        let mut prev = row("a", 100, 5_000);
        prev.max_timer_wait_ps = 9_000 * PS_PER_MS;
        let mut cur = row("a", 150, 8_000);
        cur.max_timer_wait_ps = 9_000 * PS_PER_MS; // 이 구간에는 9초 쿼리가 없었다
        let (d, _) = compute_delta(Some(&prev), &cur, 0);
        assert_eq!(
            d.max_time_ms_all_time, 9_000,
            "구간 최대가 아니라 전체 기간 최대다"
        );
    }

    #[test]
    fn schema_is_part_of_the_key() {
        // 같은 DIGEST 가 여러 스키마에서 나올 수 있다.
        let mut a = row("same", 100, 1_000);
        a.schema_name = Some("shop".into());
        let mut b = row("same", 200, 2_000);
        b.schema_name = Some("blog".into());
        let mut cache = SnapshotCache::default();
        cache.commit(vec![a.clone(), b.clone()], 10_000);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&a).unwrap().count_star, 100);
        assert_eq!(cache.get(&b).unwrap().count_star, 200);
    }

    #[test]
    fn filter_applies_safety_margin_and_survives_failure() {
        let mut cache = SnapshotCache::default();
        assert_eq!(cache.last_seen_filter_ms(), None, "첫 스냅샷은 필터 없음");

        cache.commit(vec![row("a", 1, 1)], 100_000);
        assert_eq!(cache.last_seen_filter_ms(), Some(95_000));

        // 실패는 commit 하지 않는다 → 필터 값이 유지되어 구간이 이어진다.
        assert_eq!(cache.last_seen_filter_ms(), Some(95_000));
        cache.commit(vec![row("a", 2, 2)], 160_000);
        assert_eq!(cache.last_seen_filter_ms(), Some(155_000));
    }

    #[test]
    fn delta_add_accumulates_and_keeps_max_semantics() {
        let mut acc = DigestDelta {
            exec_count: 3,
            total_time_ms: 30,
            max_time_ms_all_time: 10,
            ..Default::default()
        };
        acc.add(&DigestDelta {
            exec_count: 2,
            total_time_ms: 40,
            max_time_ms_all_time: 25,
            ..Default::default()
        });
        assert_eq!(acc.exec_count, 5);
        assert_eq!(acc.total_time_ms, 70);
        assert_eq!(acc.max_time_ms_all_time, 25);
    }
}
