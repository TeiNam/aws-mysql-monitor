//! 탐색 재조정 — 탐색 결과를 등록부에 반영한다 (M2-5, FR-DSC-07).
//!
//! # 여기가 위험한 지점이다
//!
//! 등록부에 있는데 이번 탐색 결과에 없는 인스턴스를 "사라졌다" 로 판정한다. 그런데
//! **결과에 없는 이유는 세 가지**고, 하나만 진짜 삭제다:
//!
//! | 왜 없는가 | 판정 |
//! |---|---|
//! | AWS 가 주지 않았다 (진짜 삭제) | 미발견 → 2회 연속이면 `deleted_at` |
//! | API 가 부분 실패했다 (`truncated`) | **아무것도 하지 않는다** |
//! | 필터·매핑이 제외했다 (`excluded`) | `Excluded` 상태로 전환 (비파괴) |
//!
//! 셋을 섞으면 **API 가 완전히 정상인데도 등록부가 비워진다.** 실제 방아쇠:
//!
//! 1. RDS 응답에서 `DBSubnetGroup` 이 빠진다 → `vpc_id=None` → 전건 필터 거부
//! 2. AWS 가 새 버전 문자열을 낸다 → 전건 `UnparsableVersion`
//! 3. 태그 일괄 변경이 `required_tags` 를 깨뜨린다 → 전건 `MissingTag`
//!
//! 세 경우 모두 `discovered` 가 비고 `truncated` 는 false 다. 구분하지 않으면
//! 2라운드(기본 10분) 뒤 500대 전부에 `deleted_at` 이 찍힌다.
//!
//! # 왜 별 파일인가
//!
//! `aws::discovery` 는 순수 판정(SDK 응답 → 도메인), 이 파일은 **저장소를 건드리는
//! 조정**이다. 섞으면 순수 판정을 AWS 없이 전수 검증한다는 성질이 깨진다.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dbmon_core::error::Result;
use dbmon_core::instance::{Instance, InstanceState};
use dbmon_core::ports::InstanceRegistry;
use dbmon_core::time::EpochMs;
use futures::stream::{self, StreamExt};

use crate::store::registry::merge_discovered;

/// 등록·갱신 동시 실행 수.
///
/// 순차로 돌면 500대 × 왕복 40ms(조절 시 p99) ≈ 20초가 되어 리스 갱신 예산을
/// 넘긴다(2차 리뷰가 지적). 동시성을 두면 같은 조건에서 1.5초 안에 끝난다.
/// `BatchWriteItem` 대신 이걸 쓰는 이유는 미처리 항목(`UnprocessedItems`) 재시도
/// 로직이 필요 없어서다 — 실패한 항목은 다음 라운드에 다시 온다.
const WRITE_CONCURRENCY: usize = 16;

/// 한 라운드의 탐색 결과. **"없다" 의 이유를 구분해서 담는다.**
#[derive(Debug, Default, Clone)]
pub struct RoundOutcome {
    /// 필터·매핑을 통과한 인스턴스.
    pub discovered: Vec<Instance>,
    /// **봤지만 제외된 인스턴스의 `instance_id`.**
    ///
    /// 필터 거부·매핑 실패가 여기 온다. 미발견으로 취급하면 안 된다 —
    /// AWS 는 이 인스턴스를 정상적으로 반환했다.
    pub excluded_ids: BTreeSet<String>,
    /// 부분 결과. 미발견 판정을 **건너뛴다.**
    pub truncated: bool,
    /// 필터가 거부한 수 (관측용).
    pub filtered: usize,
    /// 도메인 매핑에 실패한 수 (관측용).
    pub unmappable: usize,
    /// **이번 라운드가 실제로 들여다본 `(계정, 리전)` 조합.**
    ///
    /// # 왜 필요한가 (실측으로 찾은 파괴적 결함)
    ///
    /// 탐색 범위는 운영 설정이고 **줄어들 수 있다** — 운영자가 리전 하나를 목록에서
    /// 지우거나 계정 토글을 끈다. 그때 그 범위의 인스턴스는 결과에 없는데, 재조정은
    /// "결과에 없으면 사라졌다" 로 읽어 2라운드 뒤 `deleted_at` 을 찍는다.
    ///
    /// **보지 않은 것과 없는 것은 다르다.** 리전을 다시 넣어도 등록부의 그 행은 이미
    /// 삭제 판정을 받은 상태다. 그래서 라운드가 자기 시야를 함께 보고하고, 재조정은
    /// 시야 밖 인스턴스를 건드리지 않는다.
    ///
    /// 비어 있으면 "시야를 알 수 없다" 는 뜻이고, 그때는 판정을 하지 않는다.
    ///
    /// **성공적으로 끝까지 읽은 범위만** 들어간다. 조회가 실패했거나 목록이 잘린
    /// 범위는 빠지므로, 그 범위의 인스턴스는 "보지 않은 것" 으로 취급된다 — 계정
    /// 하나의 실패가 다른 계정의 삭제 감지를 막지 않는다.
    pub scanned_scope: BTreeSet<String>,
}

/// 시야 키 — `계정/리전`. **인스턴스 id 의 앞 두 조각과 같은 표기여야 한다.**
pub fn scope_key(account: &str, region: &str) -> String {
    format!("{account}/{region}")
}

/// 재조정 결과. 로그·메트릭에 그대로 쓴다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoveryStats {
    pub inserted: usize,
    pub updated: usize,
    /// 미발견 카운터가 올라간 수.
    pub missing: usize,
    /// `deleted_at` 이 찍힌 수.
    pub deleted: usize,
    /// **이번 라운드의 시야 밖이라 건드리지 않은 수.** 미발견이 아니다.
    pub out_of_scope: usize,
    /// **필터가 제외해서 `Excluded` 로 전환한 수.** 미발견이 아니다.
    pub excluded: usize,
    /// **부분 결과라 미발견 판정을 건너뛴 수.** 0이 아니면 로그에 남아야 한다 —
    /// 조용히 건너뛰면 "왜 삭제 안 되나" 를 추적할 수 없다.
    pub skipped_missing_check: usize,
    /// 저장 실패. 탐색 전체를 실패시키지 않는다(한 대 때문에 499대를 잃지 않는다).
    pub errors: usize,
}

impl DiscoveryStats {
    /// 쓰기를 시도한 총 건수. 실패율 판정의 분모다.
    fn attempted(&self) -> usize {
        self.inserted + self.updated + self.missing + self.deleted + self.excluded + self.errors
    }

    /// **대부분이 실패했는가.** 한 대 실패는 정상이지만 전부 실패는 장애다.
    ///
    /// 이 구분이 없으면 500건 전부 실패해도 "탐색 완료" 가 info 로 남는다
    /// (2차 리뷰가 지적).
    pub fn is_mostly_failing(&self) -> bool {
        let total = self.attempted();
        total > 0 && self.errors * 2 > total
    }
}

/// 탐색 결과를 등록부에 반영한다.
///
/// **취소하지 말 것.** `mark_missing` 은 이 시스템에서 유일한 비멱등 쓰기
/// (`ADD missing_count :one`)이고, 중간에 잘리면 적용된 증가분이 남은 채로 라운드가
/// 실패로 보고된다. 그러면 "2회 **연속**" 이라는 FR-DSC-07 의 성질이 깨진다.
/// 호출부는 취소 가능한 구간을 조회 단계로 한정한다.
/// ⚠ 인자를 **참조가 아니라 값으로** 받는다. 참조로 받으면 이 함수를 `tokio::spawn`
/// 안에서 부를 때 `implementation of Send is not general enough` 로 컴파일이 깨진다
/// (`async_trait` 이 만든 future 에 대해 `for<'a>` Send 를 증명해야 한다).
/// `Arc` 복제는 값싸고, `RoundOutcome` 은 라운드당 한 번만 만든다.
pub async fn reconcile<R: InstanceRegistry>(
    registry: Arc<R>,
    outcome: RoundOutcome,
    now_ms: EpochMs,
) -> Result<DiscoveryStats> {
    let mut stats = DiscoveryStats::default();

    // 등록부를 한 번 읽는다. 인스턴스마다 `get` 하면 500회 왕복이다.
    let known = registry.list().await?;
    let known_by_id: BTreeMap<&str, &Instance> = known.iter().map(|k| (k.id.as_str(), k)).collect();
    let seen_ids: BTreeSet<&str> = outcome.discovered.iter().map(|i| i.id.as_str()).collect();

    // ── 등록·갱신 ────────────────────────────────────────────────────────────
    //
    // **작업 목록을 먼저 소유값으로 만든다.** `&Instance` 를 받아 async 블록을
    // 돌려주는 클로저를 쓰면 `implementation of FnOnce is not general enough` 로
    // 컴파일이 깨진다(빌린 인자에 대해 `for<'a>` 바운드를 증명해야 한다).
    let jobs: Vec<(Instance, bool, String)> = outcome
        .discovered
        .iter()
        .map(|fresh| {
            let existing = known_by_id.get(fresh.id.as_str()).copied();
            (
                merge_discovered(existing, fresh),
                existing.is_some(),
                fresh.id.as_str().to_string(),
            )
        })
        .collect();

    let results: Vec<_> = stream::iter(jobs.into_iter().map(|(merged, was_known, id)| {
        let registry = Arc::clone(&registry);
        async move {
            match registry.upsert(&merged).await {
                Ok(()) => Ok(was_known),
                Err(e) => {
                    // **한 대의 실패로 나머지를 포기하지 않는다.**
                    tracing::warn!(
                        instance = %id,
                        error = %crate::telemetry::Scrubbed(&e),
                        "인스턴스 등록 실패 — 나머지를 계속한다"
                    );
                    Err(())
                }
            }
        }
    }))
    .buffer_unordered(WRITE_CONCURRENCY)
    .collect()
    .await;
    for r in results {
        match r {
            Ok(true) => stats.updated += 1,
            Ok(false) => stats.inserted += 1,
            Err(()) => stats.errors += 1,
        }
    }

    // ── 결과에 없는 것들 ──────────────────────────────────────────────────────
    for gone in known.iter().filter(|k| !seen_ids.contains(k.id.as_str())) {
        // 이미 삭제 판정된 것은 다시 세지 않는다 — 카운터가 무한히 오른다.
        if gone.deleted_at_ms.is_some() {
            continue;
        }

        // **① 봤지만 제외됐다.** 사라진 것이 아니므로 파괴적 판정을 하지 않는다.
        //
        // 상태만 `Excluded` 로 옮겨 수집에서 빠지게 한다. 이게 없으면 태그가 prd 로
        // 바뀐 인스턴스를 등록부에서 계속 수집하거나(위험), 미발견으로 삭제한다(파괴적).
        if outcome.excluded_ids.contains(gone.id.as_str()) {
            if gone.state == InstanceState::Excluded {
                continue;
            }
            let mut updated = gone.clone();
            updated.state = InstanceState::Excluded;
            updated.last_seen_ms = now_ms;
            match registry.upsert(&updated).await {
                Ok(()) => {
                    stats.excluded += 1;
                    tracing::info!(
                        instance = %gone.id.as_str(),
                        "필터가 제외했다 — Excluded 로 전환한다 (삭제하지 않는다)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        instance = %gone.id.as_str(),
                        error = %crate::telemetry::Scrubbed(&e),
                        "제외 전환 실패"
                    );
                    stats.errors += 1;
                }
            }
            continue;
        }

        // **② 부분 결과다.** 못 받은 것과 사라진 것을 구분할 수 없다.
        if outcome.truncated {
            stats.skipped_missing_check += 1;
            continue;
        }

        // **③ 이번 라운드의 시야 밖이다.** 탐색 범위가 줄었거나(운영자가 리전·계정을
        // 목록에서 뺐다) 애초에 다른 범위의 인스턴스다. **보지 않은 것을 없다고
        // 말하지 않는다** — 그러면 리전을 다시 넣어도 이미 삭제 판정된 행이 남는다.
        if !outcome.scanned_scope.is_empty()
            && !outcome
                .scanned_scope
                .contains(&scope_key(gone.id.account(), gone.id.region()))
        {
            stats.out_of_scope += 1;
            continue;
        }

        // **④ 진짜로 없다.** 2회 연속이면 `deleted_at` 이 찍힌다.
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

    if stats.out_of_scope > 0 {
        // 조용히 넘기지 않는다 — "왜 이 인스턴스가 갱신되지 않나" 의 답이다.
        tracing::info!(
            out_of_scope = stats.out_of_scope,
            scope = ?outcome.scanned_scope,
            "탐색 범위 밖 인스턴스는 그대로 둔다 (삭제 판정하지 않는다)"
        );
    }
    if stats.skipped_missing_check > 0 {
        tracing::warn!(
            skipped = stats.skipped_missing_check,
            "탐색이 부분 결과였다 — 미발견 판정을 건너뛴다"
        );
    }
    if stats.is_mostly_failing() {
        tracing::error!(
            errors = stats.errors,
            attempted = stats.attempted(),
            "탐색 쓰기가 대부분 실패했다 — 등록부가 갱신되지 않는다"
        );
    }
    // 등록부에 있는데 통과한 것이 하나도 없다: 필터 설정 사고의 신호다.
    if outcome.discovered.is_empty() && !known.is_empty() {
        tracing::warn!(
            known = known.len(),
            filtered = outcome.filtered,
            unmappable = outcome.unmappable,
            truncated = outcome.truncated,
            "탐색이 통과시킨 인스턴스가 하나도 없다 — 필터 설정을 확인한다"
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

    const NOW: EpochMs = 1_755_500_400_000;

    fn inst(identifier: &str) -> Instance {
        inst_in("123456789012", "ap-northeast-2", identifier)
    }

    /// 다른 계정·리전의 인스턴스. 탐색 범위 축소를 검증할 때 쓴다.
    fn inst_in(account: &str, region: &str, identifier: &str) -> Instance {
        let raw = RawDbInstance {
            identifier: identifier.into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: region.into(),
            endpoint_address: Some(format!("{identifier}.rds.amazonaws.com")),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        to_instance(&raw, account, &EnvMapping::default(), NOW).expect("매핑")
    }

    fn registry(seed: &[Instance]) -> Arc<FakeInstanceRegistry> {
        let r = Arc::new(FakeInstanceRegistry::new());
        r.seed(seed.to_vec());
        r
    }

    /// 통과한 것만 담은 결과. 대부분의 테스트가 쓴다.
    fn found(instances: &[Instance]) -> RoundOutcome {
        RoundOutcome {
            discovered: instances.to_vec(),
            ..Default::default()
        }
    }

    /// **탐색 범위를 좁히면 그 밖의 인스턴스를 삭제 판정하지 않는다.**
    ///
    /// 운영자가 설정에서 리전 하나를 지우거나 계정 토글을 끄면 그 범위는 조회되지
    /// 않는다. "결과에 없으면 사라졌다" 로 읽으면 2라운드 뒤 `deleted_at` 이 찍히고,
    /// 리전을 다시 넣어도 이미 삭제된 행이 남는다 — **보지 않은 것과 없는 것은 다르다.**
    #[tokio::test]
    async fn narrowing_the_scope_does_not_delete_out_of_scope_instances() {
        let seoul = inst("in-scope");
        let virginia = inst_in("123456789012", "us-east-1", "other-region");
        let other_account = inst_in("111111111111", "ap-northeast-2", "other-account");
        let r = registry(&[seoul.clone(), virginia.clone(), other_account.clone()]);

        // 서울만 훑은 라운드. 나머지 둘은 결과에 없다.
        let outcome = RoundOutcome {
            discovered: vec![seoul.clone()],
            scanned_scope: [scope_key("123456789012", "ap-northeast-2")]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let s = reconcile(Arc::clone(&r), outcome, NOW).await.expect("재조정");

        assert_eq!(s.out_of_scope, 2, "시야 밖 판정이 빠졌다: {s:?}");
        assert_eq!(s.missing, 0, "보지 않은 인스턴스를 미발견으로 찍었다");
        assert_eq!(s.deleted, 0);

        // 같은 계정·리전인데 결과에 없는 것은 여전히 미발견이다 — 시야 안이니까.
        let outcome = RoundOutcome {
            discovered: vec![],
            scanned_scope: [scope_key("123456789012", "ap-northeast-2")]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let s = reconcile(Arc::clone(&r), outcome, NOW + 1_000).await.expect("재조정");
        assert_eq!(s.missing, 1, "시야 안에서 사라진 것을 놓쳤다: {s:?}");
        assert_eq!(s.out_of_scope, 2);
    }

    /// **한 범위의 실패가 다른 범위의 삭제 감지를 막지 않는다.**
    ///
    /// 계정 B 의 조회가 실패했을 때 전체를 부분 결과로 처리하면, 계정 A 에서 정말
    /// 사라진 인스턴스도 판정되지 않아 삭제 감지가 영구히 멈춘다.
    #[tokio::test]
    async fn one_scopes_failure_does_not_block_another() {
        let a_gone = inst_in("111111111111", "ap-northeast-2", "a-gone");
        let b_unseen = inst_in("222222222222", "ap-northeast-2", "b-unseen");
        let r = registry(&[a_gone.clone(), b_unseen.clone()]);

        // A 는 성공적으로 훑었고 결과가 비었다(정말 사라졌다). B 는 조회가 실패해
        // 시야에 없다.
        let outcome = RoundOutcome {
            discovered: vec![],
            scanned_scope: [scope_key("111111111111", "ap-northeast-2")]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let s = reconcile(Arc::clone(&r), outcome, NOW).await.expect("재조정");
        assert_eq!(s.missing, 1, "성공한 범위의 삭제 감지가 막혔다: {s:?}");
        assert_eq!(s.out_of_scope, 1, "실패한 범위를 건드렸다: {s:?}");
    }

    /// 시야를 알 수 없으면(빈 집합) **판정을 그대로 한다** — 옛 호출부가 이 필드를
    /// 채우지 않아도 동작이 바뀌지 않아야 한다.
    #[tokio::test]
    async fn an_unknown_scope_falls_back_to_the_old_behaviour() {
        let r = registry(&[inst("a")]);
        let s = reconcile(Arc::clone(&r), found(&[]), NOW).await.expect("재조정");
        assert_eq!(s.missing, 1);
        assert_eq!(s.out_of_scope, 0);
    }

    #[tokio::test]
    async fn registers_new_instances() {
        let r = registry(&[]);
        let s = reconcile(Arc::clone(&r), found(&[inst("a"), inst("b")]), NOW)
            .await
            .expect("재조정");
        assert_eq!(s.inserted, 2);
        assert_eq!(s.updated, 0);
        assert_eq!(r.list().await.expect("목록").len(), 2);
    }

    #[tokio::test]
    async fn updates_existing_instances_without_inserting() {
        let r = registry(&[inst("a")]);
        let s = reconcile(Arc::clone(&r), found(&[inst("a")]), NOW)
            .await
            .expect("재조정");
        assert_eq!((s.inserted, s.updated), (0, 1));
    }

    /// **1회 미발견으로 삭제되지 않는다.**
    #[tokio::test]
    async fn one_missed_round_only_increments_the_counter() {
        let r = registry(&[inst("a"), inst("b")]);
        let s = reconcile(Arc::clone(&r), found(&[inst("a")]), NOW)
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
        reconcile(Arc::clone(&r), found(&[inst("a")]), NOW)
            .await
            .expect("1회");
        let s = reconcile(Arc::clone(&r), found(&[inst("a")]), NOW + 300_000)
            .await
            .expect("2회");
        assert_eq!((s.missing, s.deleted), (0, 1));

        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(b.state, InstanceState::Deleted);
        assert_eq!(b.deleted_at_ms, Some(NOW + 300_000));
    }

    /// **부분 결과로는 미발견 판정을 하지 않는다.**
    #[tokio::test]
    async fn a_truncated_discovery_never_marks_anything_missing() {
        let r = registry(&[inst("a"), inst("b"), inst("c")]);
        let outcome = RoundOutcome {
            discovered: vec![inst("a")],
            truncated: true,
            ..Default::default()
        };
        let s = reconcile(Arc::clone(&r), outcome.clone(), NOW)
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
        assert_eq!(s.updated, 1);
    }

    /// 부분 결과가 반복돼도 **삭제로 수렴하지 않는다.**
    #[tokio::test]
    async fn repeated_truncated_rounds_never_converge_to_deletion() {
        let r = registry(&[inst("a"), inst("b")]);
        for _ in 0..10 {
            let outcome = RoundOutcome {
                discovered: vec![inst("a")],
                truncated: true,
                ..Default::default()
            };
            reconcile(Arc::clone(&r), outcome.clone(), NOW)
                .await
                .expect("재조정");
        }
        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(b.deleted_at_ms, None, "부분 결과 10회로 삭제됐다");
        assert_eq!(b.missing_count, 0);
    }

    /// 중간에 다시 보이면 **카운터가 리셋된다.**
    #[tokio::test]
    async fn reappearing_before_the_threshold_resets_the_counter() {
        let r = registry(&[inst("a"), inst("b")]);
        reconcile(Arc::clone(&r), found(&[inst("a")]), NOW)
            .await
            .expect("1회");
        reconcile(
            Arc::clone(&r),
            found(&[inst("a"), inst("b")]),
            NOW + 300_000,
        )
        .await
        .expect("재발견");

        let b = r.get(&inst("b").id).await.expect("조회").expect("있음");
        assert_eq!(b.missing_count, 0, "재발견이 카운터를 리셋하지 않았다");
        assert_eq!(b.deleted_at_ms, None);
    }

    /// 이미 삭제 판정된 것은 **다시 세지 않는다.**
    #[tokio::test]
    async fn already_deleted_instances_are_not_recounted() {
        let r = registry(&[inst("a"), inst("b")]);
        reconcile(Arc::clone(&r), found(&[inst("a")]), NOW)
            .await
            .expect("1회");
        reconcile(Arc::clone(&r), found(&[inst("a")]), NOW + 1)
            .await
            .expect("2회");

        let s = reconcile(Arc::clone(&r), found(&[inst("a")]), NOW + 2)
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

    /// **탐색이 진짜 빈 목록을 줘도 2회 규칙은 지켜진다.**
    #[tokio::test]
    async fn an_empty_discovery_still_needs_two_rounds() {
        let r = registry(&[inst("a"), inst("b")]);
        let s = reconcile(Arc::clone(&r), RoundOutcome::default(), NOW)
            .await
            .expect("빈 결과");
        assert_eq!((s.missing, s.deleted), (2, 0));
        assert_eq!(r.list().await.expect("목록").len(), 2, "항목이 지워졌다");
    }

    // ── 제외 ≠ 사라짐 (2차 리뷰 F12 / HIGH 4) ────────────────────────────────

    /// **필터가 거부한 인스턴스는 절대 삭제 판정되지 않는다.**
    ///
    /// 이게 없으면 태그 일괄 변경·VPC id 변경·새 버전 문자열 하나로
    /// 등록부 전체가 2라운드 만에 `Deleted` 가 된다. API 는 완전히 정상인 상태에서.
    #[tokio::test]
    async fn filtered_out_instances_are_never_marked_missing() {
        let r = registry(&[inst("a"), inst("b")]);
        // 둘 다 봤지만 필터가 거부했다 (예: VPC id 가 바뀌었다).
        let outcome = RoundOutcome {
            discovered: vec![],
            excluded_ids: [
                inst("a").id.as_str().to_string(),
                inst("b").id.as_str().to_string(),
            ]
            .into_iter()
            .collect(),
            filtered: 2,
            ..Default::default()
        };

        // 몇 라운드를 돌려도 삭제되지 않아야 한다.
        for _ in 0..5 {
            let s = reconcile(Arc::clone(&r), outcome.clone(), NOW)
                .await
                .expect("재조정");
            assert_eq!(
                (s.missing, s.deleted),
                (0, 0),
                "필터 거부를 미발견으로 처리했다 — 태그 변경 한 번에 등록부가 비워진다"
            );
        }
        for id in ["a", "b"] {
            let got = r.get(&inst(id).id).await.expect("조회").expect("있음");
            assert_eq!(got.missing_count, 0);
            assert_eq!(got.deleted_at_ms, None);
            // 대신 수집에서 빠져야 한다 — prd 로 바뀐 인스턴스를 계속 수집하면 안 된다.
            assert_eq!(got.state, InstanceState::Excluded);
            assert!(!got.is_collectible(), "제외됐는데 수집 대상이다");
        }
    }

    /// 제외 전환은 **한 번만** 센다 — 매 라운드 쓰면 쓰기 증폭이다.
    #[tokio::test]
    async fn excluding_is_idempotent() {
        let r = registry(&[inst("a")]);
        let outcome = RoundOutcome {
            excluded_ids: [inst("a").id.as_str().to_string()].into_iter().collect(),
            filtered: 1,
            ..Default::default()
        };
        assert_eq!(
            reconcile(Arc::clone(&r), outcome.clone(), NOW)
                .await
                .expect("1회")
                .excluded,
            1
        );
        assert_eq!(
            reconcile(Arc::clone(&r), outcome.clone(), NOW + 1)
                .await
                .expect("2회")
                .excluded,
            0,
            "이미 Excluded 인데 또 썼다"
        );
    }

    /// 제외됐다가 다시 통과하면 **되돌아와야** 한다.
    #[tokio::test]
    async fn a_previously_excluded_instance_comes_back_when_it_passes_again() {
        let r = registry(&[inst("a")]);
        let excluded = RoundOutcome {
            excluded_ids: [inst("a").id.as_str().to_string()].into_iter().collect(),
            filtered: 1,
            ..Default::default()
        };
        reconcile(Arc::clone(&r), excluded.clone(), NOW)
            .await
            .expect("제외");
        assert_eq!(
            r.get(&inst("a").id).await.expect("조회").unwrap().state,
            InstanceState::Excluded
        );

        // 설정을 고쳤다 — 다시 통과한다.
        reconcile(Arc::clone(&r), found(&[inst("a")]), NOW + 1)
            .await
            .expect("복귀");
        let back = r.get(&inst("a").id).await.expect("조회").expect("있음");
        assert_eq!(
            back.state,
            InstanceState::Pending,
            "제외가 풀렸는데 Excluded 로 남았다 — 영구히 수집되지 않는다"
        );
    }

    /// **매핑 실패도 제외로 취급한다.** 새 버전 문자열 하나로 삭제되면 안 된다.
    #[tokio::test]
    async fn unmappable_instances_are_excluded_not_deleted() {
        let r = registry(&[inst("a")]);
        let outcome = RoundOutcome {
            excluded_ids: [inst("a").id.as_str().to_string()].into_iter().collect(),
            unmappable: 1,
            ..Default::default()
        };
        reconcile(Arc::clone(&r), outcome.clone(), NOW)
            .await
            .expect("1회");
        reconcile(Arc::clone(&r), outcome.clone(), NOW + 1)
            .await
            .expect("2회");
        let got = r.get(&inst("a").id).await.expect("조회").expect("있음");
        assert_eq!(
            got.deleted_at_ms, None,
            "매핑 실패 2회로 삭제됐다 — AWS 가 새 버전 형식을 내면 전부 사라진다"
        );
    }

    /// 대부분 실패한 라운드는 **성공으로 보고되지 않아야** 한다.
    #[test]
    fn mostly_failing_rounds_are_detectable() {
        let ok = DiscoveryStats {
            inserted: 499,
            errors: 1,
            ..Default::default()
        };
        assert!(!ok.is_mostly_failing(), "한 대 실패는 정상이다");

        let bad = DiscoveryStats {
            inserted: 10,
            errors: 490,
            ..Default::default()
        };
        assert!(bad.is_mostly_failing(), "전부 실패인데 성공으로 보고된다");

        // 아무것도 안 한 라운드는 실패가 아니다.
        assert!(!DiscoveryStats::default().is_mostly_failing());
    }
}
