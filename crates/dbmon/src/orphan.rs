//! 고아 `in_flight` 레코드 정리 (F4, [05 §4.5](../../../docs/05-collector.md)).
//!
//! # 왜 필요한가
//!
//! "1시간 초과 시 강제 확정" 은 **살아 있는 워커의 메모리 캐시에 의존**한다. 워커가
//! 급사하거나 리더가 바뀌면 캐시가 사라지고 강제 확정 주체도 사라진다. 그러면
//! `state=in_flight` 레코드가 **TTL(35일)까지 남고**, 화면의 "현재 실행 중" 목록에
//! 며칠째 도는 유령 쿼리가 표시된다(클라이언트가 매초 경과 시간을 올린다).
//!
//! 실제로 이 프로젝트에서 셧다운 경로가 그 상태였다 — `drain()` 에 도달하지 못해
//! 종료마다 유령이 하나씩 쌓였다(7차 리뷰). `drain()` 은 **정상 종료**만 덮는다.
//! 급사·SIGKILL·리더 교체는 이 스윕만 덮는다.
//!
//! # 판정을 순수 함수로 둔다
//!
//! "이 레코드는 고아인가" 는 시각 비교뿐이다. 저장소 없이 전수 검증할 수 있고,
//! 잘못 판정하면 **살아 있는 쿼리를 `abandoned` 로 확정**하므로 위험하다.

use std::sync::Arc;

use dbmon_core::error::Result;
use dbmon_core::ports::SlowQueryStore;
use dbmon_core::slow_query::{SlowQuery, SlowQueryState};
use dbmon_core::time::EpochMs;

/// 관측이 끊겼다고 볼 여유 (05 §4.5: `3 × poll_interval + 30초`).
///
/// 여유가 없으면 tick 한 번 밀린 것을 고아로 판정한다 — 살아 있는 쿼리를
/// `abandoned` 로 확정하는 쪽이 유령을 남기는 것보다 나쁘다.
pub const GRACE_MS: i64 = 30_000;

/// 고아 판정 임계. `poll_interval` 의 3배 + 여유.
pub fn stale_threshold_ms(poll_interval_ms: u64) -> i64 {
    (poll_interval_ms as i64) * 3 + GRACE_MS
}

/// 이 레코드를 어떻게 할 것인가.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 관측이 계속되고 있다. 건드리지 않는다.
    Alive,
    /// 관측이 끊겼다 — `abandoned` 로 확정한다.
    Orphaned { silent_for_ms: i64 },
    /// **내가 소유한 레코드다.** 재수화를 시도한다(기동 시 경로).
    Mine,
    /// `in_flight` 가 아니다. 스윕 대상이 아니다.
    NotInFlight,
}

/// 고아인가. **순수 함수** — `now_ms` 와 임계를 받는다.
///
/// # `Mine` 은 이름만으로 판정하지 않는다 (epoch 를 함께 본다)
///
/// `worker_id` 는 `{role}-{HOSTNAME}` 이고, docker compose·k8s StatefulSet·로컬은
/// **재시작해도 같은 문자열**이다. 이름만 보면 급사한 프로세스가 남긴 진행 중
/// 레코드를 같은 이름의 새 프로세스가 `Mine` 으로 보고 **매 스윕 건너뛴다** —
/// F4 가 없애려던 유령이 정확히 그 경로로 TTL(35일)까지 남는다.
///
/// 리스 획득은 항상 `epoch + 1` 이므로(`store::lease`), **`owner_epoch` 가 현재
/// epoch 보다 작으면 그 레코드는 이전 생애의 것**이다 — 지금 살아 있을 수 없다.
/// 그 근거는 이미 레코드에 저장돼 있는데 판정이 쓰지 않고 있었다.
pub fn judge(
    q: &SlowQuery,
    me: &str,
    my_epoch: Option<u64>,
    now_ms: EpochMs,
    threshold_ms: i64,
) -> Verdict {
    if q.state != SlowQueryState::InFlight {
        return Verdict::NotInFlight;
    }
    // 이름이 같고 **epoch 도 같을 때만** 내 것이다.
    if q.owner_worker.as_deref() == Some(me) && q.owner_epoch == my_epoch {
        return Verdict::Mine;
    }
    // `last_seen_at_ms` 가 없으면 선행 저장 직후다 — `started_at_ms` 를 쓴다.
    let last_seen = q.last_seen_at_ms.unwrap_or(q.started_at_ms);
    let silent_for_ms = now_ms - last_seen;
    if silent_for_ms > threshold_ms {
        Verdict::Orphaned { silent_for_ms }
    } else {
        Verdict::Alive
    }
}

/// 고아를 `abandoned` 로 확정한 레코드를 만든다.
///
/// **완료 시각을 만들어 내지 않는다.** 언제 끝났는지 모르므로 마지막 관측값을
/// 소요 시간으로 남기고 사유를 붙인다 — 그게 "관측이 끊겼다" 는 사실을 정직하게
/// 표현하는 유일한 방법이다.
pub fn abandon(q: &SlowQuery, now_ms: EpochMs) -> SlowQuery {
    abandon_with(q, now_ms, "owner_lost")
}

/// 사유를 지정해 `abandoned` 로 확정한다.
///
/// **사유가 다르면 다른 사실이다.** `owner_lost` 는 워커가 사라진 것이고,
/// `collector_paused` 는 사람이 관측을 멈춘 것이다 — 화면과 조사자가 그 둘을
/// 구분할 수 있어야 한다.
pub fn abandon_with(q: &SlowQuery, now_ms: EpochMs, reason: &str) -> SlowQuery {
    let last_seen = q.last_seen_at_ms.unwrap_or(q.started_at_ms);
    let mut out = q.clone();
    out.state = SlowQueryState::Abandoned;
    out.abandoned_reason = Some(reason.to_string());
    // 마지막 관측 시점까지의 소요만 안다. 그 이후는 알 수 없다.
    out.duration_ms = (last_seen - q.started_at_ms).max(q.duration_ms);
    // **`ended_at_ms` 를 채우지 않는다.** 채우면 "이때 끝났다" 는 거짓이 된다.
    out.ended_at_ms = None;
    out.captured_at_ms = now_ms;
    out
}

/// 스윕 결과.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    pub scanned: usize,
    pub abandoned: usize,
    /// 아직 살아 있어 건드리지 않은 수.
    pub alive: usize,
    /// 내 소유라 건너뛴 수.
    pub mine: usize,
    pub errors: usize,
}

/// 고아 스윕을 한 번 돈다. **리더만 부른다.**
///
/// 희소 GSI1(`SQS#in_flight`)로 진행 중 레코드만 조회한다 — `Scan` 이 아니다.
pub async fn sweep<S: SlowQueryStore>(
    store: Arc<S>,
    me: &str,
    my_epoch: Option<u64>,
    now_ms: EpochMs,
    threshold_ms: i64,
    limit: usize,
) -> Result<SweepStats> {
    let mut stats = SweepStats::default();
    let in_flight = store.list_in_flight(limit).await?;
    stats.scanned = in_flight.len();

    for q in &in_flight {
        match judge(q, me, my_epoch, now_ms, threshold_ms) {
            Verdict::Orphaned { silent_for_ms } => {
                let abandoned = abandon(q, now_ms);
                match store.upsert_merged(&abandoned).await {
                    Ok(_) => {
                        stats.abandoned += 1;
                        tracing::warn!(
                            instance = %q.instance_id.as_str(),
                            thread_id = q.thread_id,
                            owner = %q.owner_worker.as_deref().unwrap_or("?"),
                            owner_epoch = ?q.owner_epoch,
                            silent_for_ms,
                            "고아 in_flight 레코드를 abandoned 로 확정한다 (F4)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            instance = %q.instance_id.as_str(),
                            error = %crate::telemetry::Scrubbed(&e),
                            "고아 확정 실패 — 다음 스윕에 다시 시도한다"
                        );
                        stats.errors += 1;
                    }
                }
            }
            Verdict::Alive => stats.alive += 1,
            Verdict::Mine => stats.mine += 1,
            // 희소 GSI 가 진행 중만 준다. 여기 오면 인덱스가 갈렸다는 신호다.
            Verdict::NotInFlight => tracing::warn!(
                state = ?q.state,
                "진행 중 인덱스에 확정 레코드가 있다 — GSI1 키 전이를 확인한다"
            ),
        }
    }

    if stats.abandoned > 0 {
        tracing::warn!(
            abandoned = stats.abandoned,
            scanned = stats.scanned,
            "고아 스윕 완료"
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::fakes::FakeSlowQueryStore;

    const NOW: EpochMs = 1_787_147_400_000;
    const THRESHOLD: i64 = 33_000; // 3 × 1초 + 30초

    fn in_flight(owner: &str, last_seen_ms: Option<EpochMs>) -> SlowQuery {
        let mut q = crate::store::tests::sample();
        q.state = SlowQueryState::InFlight;
        q.ended_at_ms = None;
        q.owner_worker = Some(owner.into());
        q.owner_epoch = Some(7);
        q.started_at_ms = NOW - 120_000;
        q.duration_ms = 60_000;
        q.last_seen_at_ms = last_seen_ms;
        q
    }

    #[test]
    fn threshold_is_three_polls_plus_grace() {
        assert_eq!(stale_threshold_ms(1_000), 33_000);
        assert_eq!(stale_threshold_ms(5_000), 45_000);
    }

    /// **최근 관측된 레코드를 건드리지 않는다.**
    ///
    /// 잘못 판정하면 살아 있는 쿼리가 `abandoned` 로 확정된다 — 유령을 남기는 것보다 나쁘다.
    #[test]
    fn recently_seen_records_are_left_alone() {
        let q = in_flight("other-worker", Some(NOW - 5_000));
        assert_eq!(judge(&q, "me", Some(7), NOW, THRESHOLD), Verdict::Alive);
        // 임계 직전도 살아 있다.
        let q = in_flight("other-worker", Some(NOW - THRESHOLD));
        assert_eq!(judge(&q, "me", Some(7), NOW, THRESHOLD), Verdict::Alive);
    }

    /// 임계를 넘으면 고아다.
    #[test]
    fn silent_records_past_the_threshold_are_orphaned() {
        let q = in_flight("dead-worker", Some(NOW - THRESHOLD - 1));
        assert!(matches!(
            judge(&q, "me", Some(7), NOW, THRESHOLD),
            Verdict::Orphaned { .. }
        ));
    }

    /// **현재 생애의 내 레코드는 고아가 아니다.** 내가 관측을 갱신하고 있다.
    #[test]
    fn my_own_records_in_this_lease_are_never_orphaned() {
        let q = in_flight("me", Some(NOW - 10 * 60_000));
        assert_eq!(judge(&q, "me", Some(7), NOW, THRESHOLD), Verdict::Mine);
    }

    /// **이전 생애의 내 레코드는 고아다** (2차 리뷰 CRITICAL).
    ///
    /// `worker_id` 는 `{role}-{HOSTNAME}` 이라 재시작해도 같다. 이름만 보면 급사한
    /// 프로세스의 레코드를 새 프로세스가 `Mine` 으로 보고 **영구히 건너뛴다** —
    /// F4 가 없애려던 유령이 그 경로로 TTL(35일)까지 남는다.
    #[test]
    fn my_records_from_a_previous_lease_are_orphaned() {
        // 레코드의 epoch 는 7, 지금 내 epoch 는 8 (리스를 다시 잡았다).
        let q = in_flight("me", Some(NOW - 10 * 60_000));
        assert!(
            matches!(
                judge(&q, "me", Some(8), NOW, THRESHOLD),
                Verdict::Orphaned { .. }
            ),
            "같은 이름이라고 이전 생애의 유령을 건너뛰었다"
        );
    }

    /// 리더가 아니면(epoch 없음) 이름이 같아도 내 것이 아니다.
    #[test]
    fn without_a_lease_nothing_counts_as_mine() {
        let q = in_flight("me", Some(NOW - 10 * 60_000));
        assert!(matches!(
            judge(&q, "me", None, NOW, THRESHOLD),
            Verdict::Orphaned { .. }
        ));
    }

    /// `last_seen_at_ms` 가 없으면 `started_at_ms` 를 쓴다 — 선행 저장 직후다.
    #[test]
    fn a_record_without_last_seen_falls_back_to_the_start_time() {
        // 시작이 2분 전이고 임계가 33초 → 고아다.
        let q = in_flight("dead-worker", None);
        assert!(matches!(
            judge(&q, "me", Some(7), NOW, THRESHOLD),
            Verdict::Orphaned { .. }
        ));
        // 시작이 방금이면 살아 있다.
        let mut fresh = in_flight("dead-worker", None);
        fresh.started_at_ms = NOW - 1_000;
        assert_eq!(judge(&fresh, "me", Some(7), NOW, THRESHOLD), Verdict::Alive);
    }

    /// 확정된 레코드는 스윕 대상이 아니다.
    #[test]
    fn finalized_records_are_not_swept() {
        let mut q = in_flight("other", Some(NOW - 10 * 60_000));
        q.state = SlowQueryState::Finalized;
        assert_eq!(
            judge(&q, "me", Some(7), NOW, THRESHOLD),
            Verdict::NotInFlight
        );
    }

    /// **완료 시각을 만들어 내지 않는다.**
    ///
    /// 언제 끝났는지 모른다. `ended_at_ms` 를 채우면 거짓이 저장되고, 그 값으로
    /// 계산한 소요 시간이 리포트에 들어간다.
    #[test]
    fn abandoning_never_invents_an_end_time() {
        let q = in_flight("dead-worker", Some(NOW - 60_000));
        let a = abandon(&q, NOW);

        assert_eq!(a.state, SlowQueryState::Abandoned);
        assert_eq!(
            a.abandoned_reason.as_deref(),
            Some("owner_lost"),
            "기본 사유가 바뀌면 조사자가 원인을 구분할 수 없다"
        );
        assert_eq!(a.abandoned_reason.as_deref(), Some("owner_lost"));
        assert_eq!(a.ended_at_ms, None, "완료 시각을 만들어 냈다");
        // 마지막 관측까지의 소요만 남는다.
        assert_eq!(a.duration_ms, q.last_seen_at_ms.unwrap() - q.started_at_ms);
    }

    /// 이미 관측된 소요가 더 크면 그걸 유지한다 — 값을 되돌리지 않는다.
    #[test]
    fn abandoning_never_shrinks_a_known_duration() {
        let mut q = in_flight("dead-worker", Some(NOW - 119_000));
        q.duration_ms = 90_000; // 관측된 소요가 last_seen 간격보다 크다
        let a = abandon(&q, NOW);
        assert_eq!(a.duration_ms, 90_000, "알려진 소요를 줄였다");
    }

    /// **펜싱 근거를 보존한다.** `owner_epoch` 가 남아야 누가 잃었는지 알 수 있다.
    #[test]
    fn abandoning_preserves_the_fencing_evidence() {
        let q = in_flight("dead-worker", Some(NOW - 60_000));
        let a = abandon(&q, NOW);
        assert_eq!(a.owner_epoch, Some(7));
        assert_eq!(a.owner_worker.as_deref(), Some("dead-worker"));
    }

    /// 스윕이 고아만 확정하고 나머지를 남긴다.
    #[tokio::test]
    async fn the_sweep_only_abandons_orphans() {
        let store = Arc::new(FakeSlowQueryStore::default());
        let mut orphan = in_flight("dead-worker", Some(NOW - 10 * 60_000));
        orphan.thread_id = 1;
        orphan.record_id =
            dbmon_core::ids::RecordId::new(&orphan.instance_id, 1, orphan.started_at_ms);
        let mut alive = in_flight("other-worker", Some(NOW - 1_000));
        alive.thread_id = 2;
        alive.record_id =
            dbmon_core::ids::RecordId::new(&alive.instance_id, 2, alive.started_at_ms);
        let mut mine = in_flight("me", Some(NOW - 10 * 60_000));
        mine.thread_id = 3;
        mine.record_id = dbmon_core::ids::RecordId::new(&mine.instance_id, 3, mine.started_at_ms);

        for q in [&orphan, &alive, &mine] {
            store.upsert_merged(q).await.expect("저장");
        }

        let stats = sweep(Arc::clone(&store), "me", Some(7), NOW, THRESHOLD, 100)
            .await
            .expect("스윕");
        assert_eq!(stats.abandoned, 1, "{stats:?}");
        assert_eq!(stats.alive, 1);
        assert_eq!(stats.mine, 1);
        assert_eq!(stats.errors, 0);

        // 고아만 상태가 바뀌었다.
        let got = store
            .get(&orphan.record_id)
            .await
            .expect("조회")
            .expect("있음");
        assert_eq!(got.state, SlowQueryState::Abandoned);
        let got = store
            .get(&alive.record_id)
            .await
            .expect("조회")
            .expect("있음");
        assert_eq!(
            got.state,
            SlowQueryState::InFlight,
            "살아 있는 것을 확정했다"
        );
    }

    /// **스윕이 멱등이다.** 두 번 돌려도 같은 결과여야 한다.
    #[tokio::test]
    async fn the_sweep_is_idempotent() {
        let store = Arc::new(FakeSlowQueryStore::default());
        let orphan = in_flight("dead-worker", Some(NOW - 10 * 60_000));
        store.upsert_merged(&orphan).await.expect("저장");

        let first = sweep(Arc::clone(&store), "me", Some(7), NOW, THRESHOLD, 100)
            .await
            .expect("1회");
        assert_eq!(first.abandoned, 1);

        // 확정 후에는 희소 인덱스에서 빠지므로 두 번째 스윕은 아무것도 보지 않는다.
        let second = sweep(Arc::clone(&store), "me", Some(7), NOW, THRESHOLD, 100)
            .await
            .expect("2회");
        assert_eq!(second.abandoned, 0, "같은 레코드를 또 확정했다");
    }
}
