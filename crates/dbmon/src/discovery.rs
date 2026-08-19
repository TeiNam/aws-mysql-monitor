//! 탐색 재조정 — 탐색 결과를 등록부에 반영한다 (M2-5, FR-DSC-07).
//!
//! # 여기가 위험한 지점이다
//!
//! `DescribeDBInstances` 가 500대를 주다가 한 번 300대만 주면, 남은 200대는
//! "사라졌다" 로 보인다. 그 판정을 그대로 믿으면:
//!
//! 1. 200대에 `deleted_at` 이 찍힌다 (2회 연속이면)
//! 2. UI 목록에서 사라진다
//! 3. 수집이 멈춘다
//!
//! 그래서 **부분 결과로는 미발견 판정을 하지 않는다.** 등록·갱신만 한다.
//! 이 판단이 [`reconcile`] 의 존재 이유다.
//!
//! # 왜 별 파일인가
//!
//! `aws::discovery` 는 순수 판정(SDK 응답 → 도메인), 이 파일은 **저장소를 건드리는
//! 조정**이다. 섞으면 순수 판정을 AWS 없이 전수 검증한다는 성질이 깨진다.

use std::collections::BTreeSet;
use std::sync::Arc;

use dbmon_core::error::Result;
use dbmon_core::instance::Instance;
use dbmon_core::ports::InstanceRegistry;
use dbmon_core::time::EpochMs;

use crate::store::registry::merge_discovered;

/// 재조정 결과. 로그·메트릭에 그대로 쓴다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoveryStats {
    pub inserted: usize,
    pub updated: usize,
    /// 미발견 카운터가 올라간 수.
    pub missing: usize,
    /// `deleted_at` 이 찍힌 수.
    pub deleted: usize,
    /// **부분 결과라 미발견 판정을 건너뛴 수.** 0이 아니면 로그에 남아야 한다 —
    /// 조용히 건너뛰면 "왜 삭제 안 되나" 를 추적할 수 없다.
    pub skipped_missing_check: usize,
    /// 저장 실패. 탐색 전체를 실패시키지 않는다(한 대 때문에 499대를 잃지 않는다).
    pub errors: usize,
}

/// 탐색 결과를 등록부에 반영한다.
///
/// `truncated` 가 참이면 **미발견 판정을 건너뛴다.** 부분 결과를 전체로 취급하면
/// 못 받은 인스턴스가 사라진 것으로 판정된다.
pub async fn reconcile<R: InstanceRegistry>(
    registry: &Arc<R>,
    discovered: &[Instance],
    truncated: bool,
    now_ms: EpochMs,
) -> Result<DiscoveryStats> {
    let mut stats = DiscoveryStats::default();

    // 등록부를 한 번 읽는다. 인스턴스마다 `get` 하면 500회 왕복이다.
    let known = registry.list().await?;
    let seen_ids: BTreeSet<&str> = discovered.iter().map(|i| i.id.as_str()).collect();

    for fresh in discovered {
        let existing = known.iter().find(|k| k.id == fresh.id);
        let merged = merge_discovered(existing, fresh);
        match registry.upsert(&merged).await {
            Ok(()) if existing.is_some() => stats.updated += 1,
            Ok(()) => stats.inserted += 1,
            Err(e) => {
                // **한 대의 실패로 나머지를 포기하지 않는다.**
                tracing::warn!(
                    instance = %fresh.id.as_str(),
                    error = %crate::telemetry::Scrubbed(&e),
                    "인스턴스 등록 실패 — 나머지를 계속한다"
                );
                stats.errors += 1;
            }
        }
    }

    for gone in known.iter().filter(|k| !seen_ids.contains(k.id.as_str())) {
        // 이미 삭제 판정된 것은 다시 세지 않는다 — 카운터가 무한히 오른다.
        if gone.deleted_at_ms.is_some() {
            continue;
        }
        if truncated {
            // **부분 결과다.** 못 받은 것과 사라진 것을 구분할 수 없다.
            stats.skipped_missing_check += 1;
            continue;
        }
        match registry.mark_missing(&gone.id, now_ms).await {
            Ok(after) if after.deleted_at_ms.is_some() => stats.deleted += 1,
            Ok(_) => stats.missing += 1,
            Err(e) => {
                tracing::warn!(
                    instance = %gone.id.as_str(),
                    error = %crate::telemetry::Scrubbed(&e),
                    "미발견 기록 실패"
                );
                stats.errors += 1;
            }
        }
    }

    if stats.skipped_missing_check > 0 {
        tracing::warn!(
            skipped = stats.skipped_missing_check,
            "탐색이 부분 결과였다 — 미발견 판정을 건너뛴다"
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::discovery::{RawDbInstance, to_instance};
    use dbmon_core::env::EnvMapping;
    use dbmon_core::fakes::FakeInstanceRegistry;
    use dbmon_core::instance::InstanceState;

    const NOW: EpochMs = 1_755_500_400_000;

    fn inst(identifier: &str) -> Instance {
        let raw = RawDbInstance {
            identifier: identifier.into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: "ap-northeast-2".into(),
            endpoint_address: Some(format!("{identifier}.rds.amazonaws.com")),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        to_instance(&raw, "123456789012", &EnvMapping::default(), NOW).expect("매핑")
    }

    fn registry(seed: &[Instance]) -> Arc<FakeInstanceRegistry> {
        let r = Arc::new(FakeInstanceRegistry::new());
        r.seed(seed.to_vec());
        r
    }

    #[tokio::test]
    async fn registers_new_instances() {
        let r = registry(&[]);
        let s = reconcile(&r, &[inst("a"), inst("b")], false, NOW)
            .await
            .expect("재조정");
        assert_eq!(s.inserted, 2);
        assert_eq!(s.updated, 0);
        assert_eq!(r.list().await.expect("목록").len(), 2);
    }

    #[tokio::test]
    async fn updates_existing_instances_without_inserting() {
        let r = registry(&[inst("a")]);
        let s = reconcile(&r, &[inst("a")], false, NOW)
            .await
            .expect("재조정");
        assert_eq!((s.inserted, s.updated), (0, 1));
    }

    /// **1회 미발견으로 삭제되지 않는다.**
    #[tokio::test]
    async fn one_missed_round_only_increments_the_counter() {
        let r = registry(&[inst("a"), inst("b")]);
        let s = reconcile(&r, &[inst("a")], false, NOW)
            .await
            .expect("재조정");
        assert_eq!((s.missing, s.deleted), (1, 0), "1회로 삭제 판정했다");

        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(b.missing_count, 1);
        assert_eq!(b.deleted_at_ms, None);
    }

    /// 2회 연속이면 삭제로 판정한다.
    #[tokio::test]
    async fn two_missed_rounds_mark_deleted() {
        let r = registry(&[inst("a"), inst("b")]);
        reconcile(&r, &[inst("a")], false, NOW).await.expect("1회");
        let s = reconcile(&r, &[inst("a")], false, NOW + 300_000)
            .await
            .expect("2회");
        assert_eq!((s.missing, s.deleted), (0, 1));

        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(b.state, InstanceState::Deleted);
        assert_eq!(b.deleted_at_ms, Some(NOW + 300_000));
    }

    /// **부분 결과로는 미발견 판정을 하지 않는다.**
    ///
    /// `DescribeDBInstances` 가 500대 중 300대만 주면 나머지 200대는 사라진 것처럼
    /// 보인다. 그걸 믿으면 두 라운드 만에 200대가 목록에서 사라지고 수집이 멈춘다.
    #[tokio::test]
    async fn a_truncated_discovery_never_marks_anything_missing() {
        let r = registry(&[inst("a"), inst("b"), inst("c")]);
        let s = reconcile(&r, &[inst("a")], true, NOW)
            .await
            .expect("재조정");

        assert_eq!(
            (s.missing, s.deleted),
            (0, 0),
            "부분 결과로 미발견 판정을 했다 — 못 받은 것과 사라진 것을 구분하지 못한다"
        );
        assert_eq!(s.skipped_missing_check, 2, "건너뛴 수를 보고해야 한다");
        for id in ["b", "c"] {
            let got = r.get(&inst(id).id).await.expect("조회").expect("있음");
            assert_eq!(got.missing_count, 0, "{id} 의 카운터가 올랐다");
        }
        // 받은 것은 정상 갱신된다.
        assert_eq!(s.updated, 1);
    }

    /// 부분 결과가 반복돼도 **삭제로 수렴하지 않는다.**
    #[tokio::test]
    async fn repeated_truncated_rounds_never_converge_to_deletion() {
        let r = registry(&[inst("a"), inst("b")]);
        for _ in 0..10 {
            reconcile(&r, &[inst("a")], true, NOW)
                .await
                .expect("재조정");
        }
        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(b.deleted_at_ms, None, "부분 결과 10회로 삭제됐다");
        assert_eq!(b.missing_count, 0);
    }

    /// 중간에 다시 보이면 **카운터가 리셋된다** — `merge_discovered` 가 그렇게 만든다.
    #[tokio::test]
    async fn reappearing_before_the_threshold_resets_the_counter() {
        let r = registry(&[inst("a"), inst("b")]);
        reconcile(&r, &[inst("a")], false, NOW).await.expect("1회");
        reconcile(&r, &[inst("a"), inst("b")], false, NOW + 300_000)
            .await
            .expect("재발견");

        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(b.missing_count, 0, "재발견이 카운터를 리셋하지 않았다");
        assert_eq!(b.deleted_at_ms, None);
    }

    /// 이미 삭제 판정된 것은 **다시 세지 않는다** — 카운터가 무한히 오른다.
    #[tokio::test]
    async fn already_deleted_instances_are_not_recounted() {
        let r = registry(&[inst("a"), inst("b")]);
        reconcile(&r, &[inst("a")], false, NOW).await.expect("1회");
        reconcile(&r, &[inst("a")], false, NOW + 1)
            .await
            .expect("2회");

        let s = reconcile(&r, &[inst("a")], false, NOW + 2)
            .await
            .expect("3회");
        assert_eq!((s.missing, s.deleted), (0, 0), "삭제된 것을 또 셌다");
        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(
            b.missing_count, 2,
            "카운터가 계속 올랐다: {}",
            b.missing_count
        );
    }

    /// **탐색이 빈 목록을 줘도 등록부를 지우지 않는다** (`truncated=false` 인 진짜 빈 결과).
    ///
    /// 이건 정상 경로다 — 계정의 인스턴스를 다 지웠을 수 있다. 그래도 2회 규칙이 지켜져야 한다.
    #[tokio::test]
    async fn an_empty_discovery_still_needs_two_rounds() {
        let r = registry(&[inst("a"), inst("b")]);
        let s = reconcile(&r, &[], false, NOW).await.expect("빈 결과");
        assert_eq!((s.missing, s.deleted), (2, 0));
        assert_eq!(r.list().await.expect("목록").len(), 2, "항목이 지워졌다");
    }
}
