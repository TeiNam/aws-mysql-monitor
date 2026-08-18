//! 시간 롤업: 임계값 → 상위 N → `_other`
//! ([04 §2.3](../../../docs/04-data-model.md), FR-DGS-01b/01f, F6, F21, R9).
//!
//! # 왜 `_other` 가 필요한가
//!
//! 1분 해상도를 그대로 적재하면 500대 × 1440분 × 활성 다이제스트 300개 ≈ **일 2억 행**이다.
//! 시간 롤업 + 상위 200개면 **일 241만 행**으로 떨어진다. 잘려나간 롱테일을 `_other`
//! 한 행으로 합산해 "총 실행시간 중 관측된 비율"을 항상 알 수 있게 한다.
//!
//! # `_other` 의 정의 (F6)
//!
//! ```text
//! _other.total_time_ms = 전체 델타 합 − 저장된 상위 N 의 합
//! ```
//!
//! **임계값 미만도 반드시 포함된다.** 임계값은 "상위 N 후보를 고르는 필터"이고
//! `_other` 는 "저장되지 않은 전부"다. 초기 설계는 `_other` 를 "상위 N 외 롱테일"이라고만
//! 적어 임계값 미만이 포함되는지가 정의되지 않았고, 그러면 커버리지 계산의 분모가 이미
//! 필터링된 부분집합이 되어 **커버리지가 체계적으로 과대평가**된다.
//!
//! 이렇게 정의하면 `상위 N 합 + _other = total_time_all_ms` 가 항상 참이다(R9).

use crate::digest::{DigestDelta, OTHER_DIGEST};
use crate::time::HourBucket;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// 롤업 설정. **버킷마다 함께 저장한다** (F21) — 설정이 바뀌면 `_other` 의 의미가
/// 시점마다 달라지고, 사후 판별이 불가능하면 커버리지 추세와 설정 변경을 구분할 수 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupConfig {
    pub top_n: usize,
    /// 이 값 미만인 다이제스트는 상위 N **후보에서** 제외한다(`_other` 에는 들어간다).
    pub threshold_ms: i64,
}

impl Default for RollupConfig {
    fn default() -> Self {
        Self {
            top_n: 200,
            threshold_ms: 100,
        }
    }
}

impl RollupConfig {
    /// 설정 검증 ([14 §8.9](../../../docs/14-infrastructure.md)).
    pub fn validate(&self) -> Result<(), String> {
        if !(10..=2000).contains(&self.top_n) {
            return Err(format!(
                "digest_top_n 은 10~2000 이어야 한다: {}",
                self.top_n
            ));
        }
        if !(0..=60_000).contains(&self.threshold_ms) {
            return Err(format!(
                "digest_threshold_ms 는 0~60000 이어야 한다: {}",
                self.threshold_ms
            ));
        }
        Ok(())
    }
}

/// 저장 대상 한 행.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestRollupRow {
    pub hour: HourBucket,
    pub app_digest: String,
    pub schema_name: Option<String>,
    pub delta: DigestDelta,
    /// 워커 재시작 등으로 그 시간의 일부만 관측했다.
    pub partial: bool,
    /// 그 시간 중 실제 관측된 분 수 (0~60). 총량 정규화용 (F13).
    pub partial_minutes: u32,
    pub reset_detected: bool,
    /// 재시도를 소진한 뒤 강제 기록됐다 (F20).
    pub flush_failed: bool,
    pub config: RollupConfig,
    pub digest_algo_version: u32,

    // ── `_other` 행에만 채운다 ────────────────────────────────────────────────
    pub other_digest_count: Option<u32>,
    /// 임계값은 넘었지만 상위 N 에서 잘린 수 (FR-DGS-01f).
    pub truncated_candidates: Option<u32>,
    /// 그 시간의 전체 총 실행시간 (커버리지 분모, 검증용 중복 저장).
    pub total_time_all_ms: Option<i64>,
}

impl DigestRollupRow {
    pub fn is_other(&self) -> bool {
        self.app_digest == OTHER_DIGEST
    }
}

/// 한 (인스턴스, 시간) 버킷의 누산기. **워커 메모리에만 있다.**
#[derive(Debug)]
pub struct HourAccumulator {
    pub hour: HourBucket,
    per_digest: BTreeMap<String, DigestDelta>,
    schema_by_digest: BTreeMap<String, Option<String>>,
    /// 관측한 분(0~59). 재시작으로 빠진 구간을 `partial_minutes` 로 표현한다.
    observed_minutes: BTreeSet<u32>,
    reset_detected: bool,
    digest_algo_version: u32,
}

impl HourAccumulator {
    pub fn new(hour: HourBucket, digest_algo_version: u32) -> Self {
        Self {
            hour,
            per_digest: BTreeMap::new(),
            schema_by_digest: BTreeMap::new(),
            observed_minutes: BTreeSet::new(),
            reset_detected: false,
            digest_algo_version,
        }
    }

    /// 델타를 가산한다. `minute` 은 그 시간 안의 분(0~59)이다.
    pub fn add(
        &mut self,
        app_digest: &str,
        schema_name: Option<&str>,
        delta: &DigestDelta,
        minute: u32,
        reset_detected: bool,
    ) {
        self.per_digest
            .entry(app_digest.to_string())
            .or_default()
            .add(delta);
        self.schema_by_digest
            .entry(app_digest.to_string())
            .or_insert_with(|| schema_name.map(str::to_string));
        self.observed_minutes.insert(minute.min(59));
        self.reset_detected |= reset_detected;
    }

    pub fn distinct_digests(&self) -> usize {
        self.per_digest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.per_digest.is_empty()
    }

    /// 저장할 행 목록을 만든다. 마지막 원소가 `_other` 다(있는 경우).
    ///
    /// **누산기를 소비하지 않는다** — 플러시가 실패하면 다음 정시에 다시 시도해야 하므로
    /// 호출자가 성공을 확인한 뒤에 버린다 (F20).
    pub fn build_rows(&self, cfg: RollupConfig) -> Vec<DigestRollupRow> {
        let partial_minutes = self.observed_minutes.len() as u32;
        let partial = partial_minutes < 60;

        // 전체 합 — `_other` 계산의 기준이며 커버리지의 분모다.
        let mut total = DigestDelta::default();
        for d in self.per_digest.values() {
            total.add(d);
        }

        // 임계값으로 후보를 먼저 걸러낸 뒤 그중 상위 N. `OR` 가 아니라 hard cap 이다.
        let mut candidates: Vec<(&String, &DigestDelta)> = self
            .per_digest
            .iter()
            .filter(|(_, d)| d.total_time_ms >= cfg.threshold_ms)
            .collect();
        // 동점에서도 결정론적이어야 한다 (같은 입력 → 같은 출력).
        candidates.sort_by(|a, b| b.1.total_time_ms.cmp(&a.1.total_time_ms).then(a.0.cmp(b.0)));

        let kept = candidates.len().min(cfg.top_n);
        let truncated_candidates = (candidates.len() - kept) as u32;

        let mut rows: Vec<DigestRollupRow> = Vec::with_capacity(kept + 1);
        let mut stored = DigestDelta::default();
        for (digest, delta) in candidates.into_iter().take(kept) {
            stored.add(delta);
            rows.push(DigestRollupRow {
                hour: self.hour.clone(),
                app_digest: digest.clone(),
                schema_name: self.schema_by_digest.get(digest).cloned().flatten(),
                delta: *delta,
                partial,
                partial_minutes,
                reset_detected: self.reset_detected,
                flush_failed: false,
                config: cfg,
                digest_algo_version: self.digest_algo_version,
                other_digest_count: None,
                truncated_candidates: None,
                total_time_all_ms: None,
            });
        }

        let other_digest_count = (self.per_digest.len() - rows.len()) as u32;
        if other_digest_count > 0 {
            rows.push(DigestRollupRow {
                hour: self.hour.clone(),
                app_digest: OTHER_DIGEST.to_string(),
                schema_name: None,
                delta: subtract(&total, &stored),
                partial,
                partial_minutes,
                reset_detected: self.reset_detected,
                flush_failed: false,
                config: cfg,
                digest_algo_version: self.digest_algo_version,
                other_digest_count: Some(other_digest_count),
                truncated_candidates: Some(truncated_candidates),
                total_time_all_ms: Some(total.total_time_ms),
            });
        } else if !rows.is_empty() {
            // `_other` 가 없어도 분모는 필요하다. 마지막 행에 총량을 기록한다.
            if let Some(last) = rows.last_mut() {
                last.total_time_all_ms = Some(total.total_time_ms);
                last.truncated_candidates = Some(0);
                last.other_digest_count = Some(0);
            }
        }
        rows
    }
}

/// `전체 − 저장분`. 음수가 나오지 않도록 포화 연산을 쓴다.
fn subtract(total: &DigestDelta, stored: &DigestDelta) -> DigestDelta {
    DigestDelta {
        exec_count: total.exec_count.saturating_sub(stored.exec_count),
        total_time_ms: (total.total_time_ms - stored.total_time_ms).max(0),
        total_lock_time_ms: (total.total_lock_time_ms - stored.total_lock_time_ms).max(0),
        errors: total.errors.saturating_sub(stored.errors),
        warnings: total.warnings.saturating_sub(stored.warnings),
        rows_affected: total.rows_affected.saturating_sub(stored.rows_affected),
        rows_sent: total.rows_sent.saturating_sub(stored.rows_sent),
        rows_examined: total.rows_examined.saturating_sub(stored.rows_examined),
        tmp_tables: total.tmp_tables.saturating_sub(stored.tmp_tables),
        tmp_disk_tables: total.tmp_disk_tables.saturating_sub(stored.tmp_disk_tables),
        full_join: total.full_join.saturating_sub(stored.full_join),
        select_scan: total.select_scan.saturating_sub(stored.select_scan),
        sort_merge_passes: total
            .sort_merge_passes
            .saturating_sub(stored.sort_merge_passes),
        no_index_used: total.no_index_used.saturating_sub(stored.no_index_used),
        no_good_index_used: total
            .no_good_index_used
            .saturating_sub(stored.no_good_index_used),
        // `_other` 의 최대·P95 는 의미가 없다. 0으로 둔다.
        max_time_ms_all_time: 0,
        p95_ms_snapshot: None,
    }
}

/// 관측 커버리지 = 저장된 다이제스트가 설명하는 총 실행시간의 비율.
///
/// 분모는 `total_time_all_ms` 여야 한다. 저장분 합을 분모로 쓰면 항상 100% 가 된다.
pub fn coverage_ratio(rows: &[DigestRollupRow]) -> Option<f64> {
    let total = rows.iter().find_map(|r| r.total_time_all_ms)?;
    if total <= 0 {
        return None;
    }
    let stored: i64 = rows
        .iter()
        .filter(|r| !r.is_other())
        .map(|r| r.delta.total_time_ms)
        .sum();
    Some(stored as f64 / total as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(count: u64, ms: i64) -> DigestDelta {
        DigestDelta {
            exec_count: count,
            total_time_ms: ms,
            rows_examined: count * 10,
            ..Default::default()
        }
    }

    fn acc_with(n: usize, base_ms: i64) -> HourAccumulator {
        let mut a = HourAccumulator::new(HourBucket::from_epoch_ms(1_787_203_391_500), 1);
        for i in 0..n {
            a.add(
                &format!("d{i:04}"),
                Some("shop"),
                &delta(10, base_ms + i as i64),
                (i % 60) as u32,
                false,
            );
        }
        a
    }

    /// R9 — 상위 N + `_other` = 전체. **임계값 미만을 포함해서** 성립해야 한다.
    #[test]
    fn r9_total_is_conserved_including_below_threshold() {
        let mut a = HourAccumulator::new(HourBucket::from_epoch_ms(0), 1);
        // 임계값(100ms) 초과 3개
        a.add("big1", None, &delta(10, 5_000), 0, false);
        a.add("big2", None, &delta(10, 3_000), 0, false);
        a.add("big3", None, &delta(10, 1_000), 0, false);
        // 임계값 미만 5개 — 후보에서 빠지지만 `_other` 에는 들어가야 한다
        for i in 0..5 {
            a.add(&format!("tiny{i}"), None, &delta(100, 20), 0, false);
        }

        let rows = a.build_rows(RollupConfig {
            top_n: 2,
            threshold_ms: 100,
        });
        let other = rows
            .iter()
            .find(|r| r.is_other())
            .expect("_other 행이 있어야 한다");

        let stored: i64 = rows
            .iter()
            .filter(|r| !r.is_other())
            .map(|r| r.delta.total_time_ms)
            .sum();
        let total = other.total_time_all_ms.unwrap();
        assert_eq!(total, 5_000 + 3_000 + 1_000 + 5 * 20);
        assert_eq!(
            stored + other.delta.total_time_ms,
            total,
            "총량 보존이 깨졌다"
        );

        // 임계값 미만 5개 + 상위 N 에서 잘린 1개 = 6개가 _other 로 갔다.
        assert_eq!(other.other_digest_count, Some(6));
        assert_eq!(
            other.truncated_candidates,
            Some(1),
            "임계값은 넘었지만 잘린 수"
        );
        assert_eq!(other.delta.exec_count, 10 + 500, "실행 횟수도 보존된다");
    }

    #[test]
    fn threshold_then_top_n_is_not_or() {
        // FR-DGS-01b vs 01f 모순의 회귀. `OR` 였다면 임계값 미만도 저장됐을 것이다.
        let mut a = HourAccumulator::new(HourBucket::from_epoch_ms(0), 1);
        a.add("small", None, &delta(1, 50), 0, false); // 임계값 미만
        let rows = a.build_rows(RollupConfig {
            top_n: 200,
            threshold_ms: 100,
        });
        assert!(
            rows.iter().all(|r| r.is_other()),
            "임계값 미만이 저장됐다: {rows:#?}"
        );
    }

    #[test]
    fn hard_cap_is_respected() {
        let a = acc_with(500, 1_000);
        let rows = a.build_rows(RollupConfig {
            top_n: 200,
            threshold_ms: 100,
        });
        assert_eq!(rows.len(), 201, "상위 200 + _other");
        assert_eq!(rows.iter().filter(|r| !r.is_other()).count(), 200);
    }

    #[test]
    fn ordering_is_deterministic_on_ties() {
        let mut a = HourAccumulator::new(HourBucket::from_epoch_ms(0), 1);
        for d in ["zz", "aa", "mm"] {
            a.add(d, None, &delta(1, 1_000), 0, false); // 전부 동점
        }
        let cfg = RollupConfig {
            top_n: 2,
            threshold_ms: 100,
        };
        let first: Vec<String> = a
            .build_rows(cfg)
            .iter()
            .map(|r| r.app_digest.clone())
            .collect();
        let second: Vec<String> = a
            .build_rows(cfg)
            .iter()
            .map(|r| r.app_digest.clone())
            .collect();
        assert_eq!(first, second);
        assert_eq!(first[0], "aa", "동점은 다이제스트 사전순");
        assert_eq!(first[1], "mm");
    }

    #[test]
    fn partial_minutes_tracked() {
        let mut a = HourAccumulator::new(HourBucket::from_epoch_ms(0), 1);
        for m in 0..15 {
            a.add("d", None, &delta(1, 1_000), m, false);
        }
        let rows = a.build_rows(RollupConfig::default());
        assert_eq!(rows[0].partial_minutes, 15);
        assert!(rows[0].partial, "60분을 다 못 봤으면 partial");

        let full = acc_with(60, 1_000);
        assert!(!full.build_rows(RollupConfig::default())[0].partial);
    }

    #[test]
    fn config_is_recorded_per_bucket() {
        // F21 — 어떤 설정으로 만든 행인지 사후 판별이 가능해야 한다.
        let a = acc_with(3, 1_000);
        let cfg = RollupConfig {
            top_n: 10,
            threshold_ms: 250,
        };
        for r in a.build_rows(cfg) {
            assert_eq!(r.config, cfg);
            assert_eq!(r.digest_algo_version, 1);
        }
    }

    #[test]
    fn total_recorded_even_without_other_row() {
        // `_other` 가 없어도 커버리지 분모는 있어야 한다.
        let a = acc_with(3, 1_000);
        let rows = a.build_rows(RollupConfig {
            top_n: 200,
            threshold_ms: 100,
        });
        assert!(rows.iter().all(|r| !r.is_other()));
        assert_eq!(coverage_ratio(&rows), Some(1.0));
    }

    #[test]
    fn coverage_uses_total_as_denominator() {
        let mut a = HourAccumulator::new(HourBucket::from_epoch_ms(0), 1);
        a.add("big", None, &delta(1, 900), 0, false);
        for i in 0..10 {
            a.add(&format!("t{i}"), None, &delta(1, 10), 0, false);
        }
        let rows = a.build_rows(RollupConfig {
            top_n: 1,
            threshold_ms: 100,
        });
        // 전체 1000ms 중 900ms 관측 → 90%
        assert_eq!(coverage_ratio(&rows), Some(0.9));
    }

    #[test]
    fn empty_accumulator_produces_nothing() {
        let a = HourAccumulator::new(HourBucket::from_epoch_ms(0), 1);
        assert!(a.is_empty());
        assert!(a.build_rows(RollupConfig::default()).is_empty());
        assert_eq!(coverage_ratio(&[]), None);
    }

    #[test]
    fn build_rows_does_not_consume_accumulator() {
        // 플러시 실패 시 다시 시도해야 한다 (F20).
        let a = acc_with(5, 1_000);
        let first = a.build_rows(RollupConfig::default());
        let again = a.build_rows(RollupConfig::default());
        assert_eq!(first, again);
        assert_eq!(a.distinct_digests(), 5);
    }

    #[test]
    fn config_validation_bounds() {
        assert!(RollupConfig::default().validate().is_ok());
        assert!(
            RollupConfig {
                top_n: 9,
                threshold_ms: 100
            }
            .validate()
            .is_err()
        );
        assert!(
            RollupConfig {
                top_n: 2001,
                threshold_ms: 100
            }
            .validate()
            .is_err()
        );
        assert!(
            RollupConfig {
                top_n: 200,
                threshold_ms: -1
            }
            .validate()
            .is_err()
        );
    }
}
