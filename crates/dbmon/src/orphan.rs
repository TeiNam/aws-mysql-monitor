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

/// 관측이 끊겼다고 볼 여유 (05 §4.5).
///
/// 여유가 없으면 tick 한 번 밀린 것을 고아로 판정한다 — 살아 있는 쿼리를
/// `abandoned` 로 확정하는 쪽이 유령을 남기는 것보다 나쁘다.
pub const GRACE_MS: i64 = 30_000;

/// 수집기가 "아직 관측 중" 을 다시 저장하는 주기
/// ([`crate::collector::InstanceCollector::heartbeat`]).
///
/// 이 값은 **주기의 목표**일 뿐이고 실제 상한은 워커의 tick 주기다(하트비트는 tick
/// 안에서만 쓴다). 그래서 [`STALE_THRESHOLD_MS`] 는 이 값이 아니라
/// [`MAX_DETECT_INTERVAL_MS`] 로 하한을 깐다.
pub const HEARTBEAT_INTERVAL_MS: i64 = GRACE_MS / 2;

/// `collector.detect_interval_ms` 의 **설정 상한** (`config.rs` 가 검증한다).
///
/// 하트비트는 tick 안에서만 쓸 수 있으므로 **어떤 워커도 자기 tick 주기보다 자주
/// 갱신할 수 없다.** 임계가 이 값을 고려하지 않으면, 60초 tick 워커의 살아 있는
/// 레코드를 짧은 tick 리더가 버린다(교차 리뷰 23라운드).
pub const MAX_DETECT_INTERVAL_MS: i64 = 60_000;

/// 고아 판정 임계. **설정에 의존하지 않는 상수다.**
///
/// # 왜 리더의 `poll_interval` 로 계산하지 않는가
///
/// 처음에는 `3 × poll + GRACE_MS` 였다. `poll` 은 **스윕하는 리더의** 설정이고 갱신하는
/// 쪽은 **다른 워커**다. 설정은 워커별이므로 리더가 200ms, 수집 워커가 60초일 수 있다 —
/// 그러면 임계 30.6초 안에 갱신할 방법이 워커에게 없다. 살아 있는 쿼리가 `abandoned` 로
/// 확정되고 그건 되돌릴 수 없다(교차 리뷰 23라운드).
///
/// 그래서 **합법 설정의 최악값**으로 계산한다. 가장 느린 워커의 tick 은
/// `MAX_DETECT_INTERVAL_MS` 이고, 원래 설계의 "tick 3번 밀린 것까지는 봐준다" 를
/// 그대로 적용하면 임계는 그 3배 + 여유다. 리더의 `poll` 은 항상 그보다 작으므로 식에서
/// 사라진다 — 인자를 두면 **읽는 사람이 그 값이 영향을 준다고 믿는다.**
///
/// 대가는 유령이 남는 시간(210초)인데, 스윕 주기(`orphan_sweep_secs`, 기본 300초)가
/// 이미 그보다 크다 — 사실상 공짜다.
pub const STALE_THRESHOLD_MS: i64 = 3 * MAX_DETECT_INTERVAL_MS + GRACE_MS;

/// **불변식을 컴파일 시점에 지킨다.** 런타임 테스트로 두면 상수를 바꾼 사람이 테스트를
/// 안 돌릴 수 있고, 그 결과는 "살아 있는 쿼리를 버린다" 다.
///
/// 가장 느린 합법 워커(60초 tick)가 하트비트를 한 번 놓쳐도 임계 안에 들어야 한다.
const _: () = assert!(HEARTBEAT_INTERVAL_MS < MAX_DETECT_INTERVAL_MS);
const _: () = assert!(MAX_DETECT_INTERVAL_MS * 2 + HEARTBEAT_INTERVAL_MS < STALE_THRESHOLD_MS);

/// 이 레코드를 어떻게 할 것인가.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 관측이 계속되고 있다. 건드리지 않는다.
    Alive,
    /// 관측이 끊겼다 — `abandoned` 로 확정한다.
    Orphaned { silent_for_ms: i64 },
    /// `in_flight` 가 아니다. 스윕 대상이 아니다.
    NotInFlight,
}

/// 고아인가. **순수 함수** — 침묵 시간만 본다.
///
/// # 소유자를 보지 않는다
///
/// 교차 리뷰 20~22라운드가 같은 자리를 세 번 지적했다. "내 이름·내 epoch 이면 내
/// 것이니 건너뛴다" 에서 시작해, 그게 유령을 남기니(급사한 태스크의 레코드) "지금 도는
/// 태스크가 만졌는가" 를 벽시계로 추론하는 쪽으로 갔고, 그 추론이 **양방향으로** 틀렸다:
///
/// | 조인 방향 | 열린 반대 방향 |
/// |---|---|
/// | 이름·epoch 이면 건너뛴다 | 급사한 태스크의 레코드를 TTL(35일)까지 가린다 |
/// | 태스크가 만진 것만 내 것 | 갓 뜬 태스크의 **살아 있는** 레코드를 버린다 |
/// | 갓 뜬 태스크에 유예 | 크래시 루프가 유예를 되돌려 무기한 가린다 |
///
/// 근본 원인은 **관측 중이라는 사실을 스윕이 알 수 없다**는 것이었다. 그건 수집기만
/// 안다. 그래서 수집기가 주기적으로 그 사실을 저장하고
/// ([`crate::collector::Collector::heartbeat`]), 여기서는 침묵만 본다 — 침묵이 곧
/// "아무도 관측하지 않는다" 다.
///
/// 저장소가 죽으면 하트비트도 실패하지만 **아래 확정 쓰기도 같은 저장소를 쓰므로**
/// 버려지지도 않는다. 판정이 한쪽으로 치우칠 여지가 없다.
pub fn judge(q: &SlowQuery, now_ms: EpochMs, threshold_ms: i64) -> Verdict {
    if q.state != SlowQueryState::InFlight {
        return Verdict::NotInFlight;
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
    pub errors: usize,
}

/// 고아 스윕을 한 번 돈다. **리더만 부른다.**
///
/// 희소 GSI1(`SQS#in_flight`)로 진행 중 레코드만 조회한다 — `Scan` 이 아니다.
pub async fn sweep<S: SlowQueryStore>(
    store: Arc<S>,
    now_ms: EpochMs,
    threshold_ms: i64,
    limit: usize,
) -> Result<SweepStats> {
    let mut stats = SweepStats::default();
    let in_flight = store.list_in_flight(limit).await?;
    stats.scanned = in_flight.len();

    for q in &in_flight {
        match judge(q, now_ms, threshold_ms) {
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

    /// **소유자와 무관하게 침묵만 본다.**
    ///
    /// 이름·epoch 으로 건너뛰면 급사한 태스크의 레코드가 TTL(35일)까지 유령으로 남고,
    /// "지금 도는 태스크가 만졌는가" 를 벽시계로 추론하면 갓 뜬 태스크의 살아 있는
    /// 레코드를 버린다 — 20~22라운드가 그 두 방향을 번갈아 지적했다. 관측 중이라는
    /// 사실은 수집기가 하트비트로 저장하므로, 여기서는 소유자를 볼 이유가 없다.
    #[test]
    fn ownership_does_not_shield_a_silent_record() {
        // 내 이름·내 epoch 이라도 침묵이 임계를 넘으면 걷는다.
        let mine = in_flight("me", Some(NOW - THRESHOLD - 1_000));
        assert!(
            matches!(judge(&mine, NOW, THRESHOLD), Verdict::Orphaned { .. }),
            "내 소유라고 건너뛰면 확정 실패·정지 경합의 레코드가 영구히 남는다"
        );
        // 남의 이름이어도 관측이 계속되면 건드리지 않는다.
        let other = in_flight("other-worker", Some(NOW - 1_000));
        assert_eq!(judge(&other, NOW, THRESHOLD), Verdict::Alive);
    }

    /// **임계는 가장 느린 합법 워커가 지킬 수 있어야 한다.**
    ///
    /// 하트비트는 tick 안에서만 쓸 수 있으므로 갱신 주기의 실질 상한은 워커의 tick 이다.
    /// 임계를 리더의 `poll` 로 계산하면 60초 tick 워커의 살아 있는 레코드를 200ms tick
    /// 리더가 버린다 — 23라운드가 그 조합을 짚었다. 상수 사이의 관계는 컴파일 시점에
    /// 단정하고, 여기서는 **설정 상한과 실제 값**을 함께 못박는다.
    #[test]
    fn the_threshold_tolerates_the_slowest_legal_worker() {
        // 설정 검증과 상수가 어긋나면 이 단정이 먼저 깨진다.
        assert_eq!(
            MAX_DETECT_INTERVAL_MS, 60_000,
            "config.rs 의 detect_interval_ms 상한과 달라졌다"
        );
        // 가장 느린 워커가 tick 을 두 번 놓쳐도 임계 안이다 — 상수 사이의 관계는
        // 모듈 상단의 `const _: () = assert!(..)` 가 컴파일 시점에 본다.
        assert_eq!(STALE_THRESHOLD_MS, 210_000);
    }

    /// **최근 관측된 레코드를 건드리지 않는다.**
    ///
    /// 잘못 판정하면 살아 있는 쿼리가 `abandoned` 로 확정된다 — 유령을 남기는 것보다 나쁘다.
    #[test]
    fn recently_seen_records_are_left_alone() {
        let q = in_flight("other-worker", Some(NOW - 5_000));
        assert_eq!(judge(&q, NOW, THRESHOLD), Verdict::Alive);
        // 임계 직전도 살아 있다.
        let q = in_flight("other-worker", Some(NOW - THRESHOLD));
        assert_eq!(judge(&q, NOW, THRESHOLD), Verdict::Alive);
    }

    /// 임계를 넘으면 고아다.
    #[test]
    fn silent_records_past_the_threshold_are_orphaned() {
        let q = in_flight("dead-worker", Some(NOW - THRESHOLD - 1));
        assert!(matches!(
            judge(&q, NOW, THRESHOLD),
            Verdict::Orphaned { .. }
        ));
    }

    /// **급사한 프로세스의 레코드를 이름 때문에 건너뛰지 않는다** (2차 리뷰 CRITICAL).
    ///
    /// `worker_id` 는 `{role}-{HOSTNAME}` 이라 재시작해도 같다. 이름으로 건너뛰면 급사한
    /// 프로세스의 레코드를 새 프로세스가 자기 것으로 보고 **영구히 건너뛴다** — F4 가
    /// 없애려던 유령이 그 경로로 TTL(35일)까지 남는다. 이제 소유자를 아예 보지 않으므로
    /// 이름·epoch 이 어떻든 침묵이 판정한다.
    #[test]
    fn a_dead_process_leaves_no_ghost_regardless_of_its_name() {
        for owner in ["me", "other-worker", "dbmon-collector-pod-0"] {
            let q = in_flight(owner, Some(NOW - 10 * 60_000));
            assert!(
                matches!(judge(&q, NOW, THRESHOLD), Verdict::Orphaned { .. }),
                "{owner} 의 유령이 남았다"
            );
        }
    }

    /// `last_seen_at_ms` 가 없으면 `started_at_ms` 를 쓴다 — 선행 저장 직후다.
    #[test]
    fn a_record_without_last_seen_falls_back_to_the_start_time() {
        // 시작이 2분 전이고 임계가 33초 → 고아다.
        let q = in_flight("dead-worker", None);
        assert!(matches!(
            judge(&q, NOW, THRESHOLD),
            Verdict::Orphaned { .. }
        ));
        // 시작이 방금이면 살아 있다.
        let mut fresh = in_flight("dead-worker", None);
        fresh.started_at_ms = NOW - 1_000;
        assert_eq!(judge(&fresh, NOW, THRESHOLD), Verdict::Alive);
    }

    /// 확정된 레코드는 스윕 대상이 아니다.
    #[test]
    fn finalized_records_are_not_swept() {
        let mut q = in_flight("other", Some(NOW - 10 * 60_000));
        q.state = SlowQueryState::Finalized;
        assert_eq!(judge(&q, NOW, THRESHOLD), Verdict::NotInFlight);
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
        // **내 소유이고 침묵한 레코드도 걷는다** — 확정 저장이 실패했거나 정지 확정을
        // 건너뛴 경우가 정확히 이 모양이고, 건너뛰면 아무도 닫지 않는다.
        let mut mine = in_flight("me", Some(NOW - 10 * 60_000));
        mine.thread_id = 3;
        mine.record_id = dbmon_core::ids::RecordId::new(&mine.instance_id, 3, mine.started_at_ms);

        for q in [&orphan, &alive, &mine] {
            store.upsert_merged(q).await.expect("저장");
        }

        let stats = sweep(Arc::clone(&store), NOW, THRESHOLD, 100)
            .await
            .expect("스윕");
        assert_eq!(stats.abandoned, 2, "{stats:?}");
        assert_eq!(stats.alive, 1);
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

        let first = sweep(Arc::clone(&store), NOW, THRESHOLD, 100)
            .await
            .expect("1회");
        assert_eq!(first.abandoned, 1);

        // 확정 후에는 희소 인덱스에서 빠지므로 두 번째 스윕은 아무것도 보지 않는다.
        let second = sweep(Arc::clone(&store), NOW, THRESHOLD, 100)
            .await
            .expect("2회");
        assert_eq!(second.abandoned, 0, "같은 레코드를 또 확정했다");
    }
}
