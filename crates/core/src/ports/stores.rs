//! 저장·조정 포트.
//!
//! 여기 있는 trait 는 **구현이 2개 이상**이다: 프로덕션(DynamoDB)과 테스트용 페이크.
//! 테스트가 두 번째 구현이므로 trait 존재가 정당하다
//! ([ADR-002](../../../.claude/docs/03-decisions.md)).

use crate::error::Result;
use crate::ids::{InstanceId, RecordId};
use crate::instance::Instance;
use crate::pause::{PauseScope, PauseSet};
use crate::rollup::DigestRollupRow;
use crate::slow_query::SlowQuery;
use crate::time::{EpochMs, TimeRange};
use async_trait::async_trait;

/// 저장소가 그 레코드를 **실제로 둔 자리**. 도메인은 내용을 해석하지 않는다.
///
/// # 왜 필요한가
///
/// 물리 키는 `started_at_ms` **추정치**에서 나오고 그 추정은 관측자마다 다르다
/// (`PROCESSLIST.TIME` 이 정수 초다). 게다가 병합이 시작 시각을 앞당기면 **필드와 키가
/// 어긋난다** — 항목은 처음 자리에 남는다. 그래서 나중에 같은 레코드를 겨냥하려면
/// 키를 **다시 계산할 수 없고**, 쓰기가 알려 준 자리를 들고 있어야 한다.
///
/// 교차 리뷰 24·25라운드가 이 자리에서 블로커를 세 개 냈다. 처음에는 하트비트가 키를
/// 다시 계산했고(리더가 바뀌면 못 찾는다), 그다음엔 ±2초를 뒤져 **가장 가까운 행**을
/// 골랐다(같은 스레드의 옛 실행을 살려 두고 산 실행을 버릴 수 있다). 추측을 없애는
/// 방법은 추측하지 않는 것이다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoredKey(String);

impl StoredKey {
    /// 어댑터만 만든다. 형식은 어댑터의 사정이다.
    pub fn new(opaque: impl Into<String>) -> Self {
        Self(opaque.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StoredKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 슬로우 쿼리 저장.
#[async_trait]
pub trait SlowQueryStore: Send + Sync {
    /// **양방향 병합 저장.** `PutItem` 을 노출하지 않는 것이 요점이다 (F5).
    ///
    /// 경로마다 따로 구현하면 다시 비대칭이 되므로, 실시간 캡처 확정 경로와 슬로우로그
    /// 수집 경로가 **이 하나의 메서드만** 쓴다.
    ///
    /// 반환값은 저장된(병합된) 레코드다 — 호출자가 `record_id` 가 유지됐는지 알 수 있다.
    async fn upsert_merged(&self, q: &SlowQuery) -> Result<SlowQuery> {
        Ok(self.upsert_merged_keyed(q).await?.0)
    }

    /// `upsert_merged` + **레코드가 실제로 놓인 자리**([`StoredKey`]).
    ///
    /// 진행 중 레코드의 생존 신호를 나중에 올려야 하는 호출부(수집기)가 쓴다. 키를 다시
    /// 계산하거나 뒤져서 찾으면 **다른 실행의 행을 갱신할 수 있다** — 그래서 쓴 쪽이
    /// 알려 준다.
    async fn upsert_merged_keyed(&self, q: &SlowQuery) -> Result<(SlowQuery, StoredKey)>;

    async fn get(&self, id: &RecordId) -> Result<Option<SlowQuery>>;

    /// `(instance, thread_id, ±window, app_digest)` 보조 조회 — `record_id` 가 1초
    /// 어긋났을 때 후보를 찾는다 ([05 §8.2](../../../.claude/docs/05-collector.md)).
    async fn find_merge_candidate(
        &self,
        instance: &InstanceId,
        thread_id: u64,
        app_digest: &str,
        around_ms: EpochMs,
        window_ms: i64,
    ) -> Result<Option<SlowQuery>>;

    /// 구간 안의 레코드를 **최신순으로**.
    ///
    /// `before` 는 페이지 재개 지점이다 — 그 **순서 키 이하**만 읽는다
    /// ([`crate::slow_query::list_order_key`]). 경계값을 포함하는 이유는 같은 순서 키가
    /// 인스턴스마다 하나씩 있을 수 있어서다. 그 갈래는 호출부가 인스턴스 id 로 가른다.
    async fn list_by_instance(
        &self,
        instance: &InstanceId,
        range: TimeRange,
        limit: usize,
        before: Option<&str>,
    ) -> Result<Vec<SlowQuery>>;

    /// 희소 GSI 로 진행 중 레코드를 조회한다 (고아 정리, F4).
    async fn list_in_flight(&self, limit: usize) -> Result<Vec<SlowQuery>>;

    /// **"아직 관측 중" 만 갱신한다** — 고아 오판을 막는 생존 신호 (F4).
    ///
    /// # 왜 `upsert_merged` 로 하지 않는가
    ///
    /// 처음에는 하트비트도 `upsert_merged` 를 썼다. 그게 세 가지를 깨뜨렸다
    /// (교차 리뷰 23라운드):
    ///
    /// | 문제 | 결과 |
    /// |---|---|
    /// | 레코드가 **없으면 만든다** | 심층 조회 상한 밖의 후보(레코드가 없어야 한다)에 SQL 없는 레코드가 생긴다 |
    /// | SQL 없는 레코드는 정책이 `off` 로 강등된다 | 병합이 **먼저 기록된 정책을 고정**하므로 그 뒤의 SQL 이 영구히 버려진다 |
    /// | 저장 성공은 전부 방송된다 | 브라우저가 15초마다 목록 전체를 무효화한다 |
    ///
    /// 그래서 하트비트는 **속성 하나만 조건부로** 올린다. 만들지 않고, 정책을 싣지
    /// 않고, 방송하지 않는다. 새 사실이 아니므로 방송할 것도 없다 — 화면의 경과
    /// 시간은 클라이언트가 센다.
    ///
    /// # 조건
    ///
    /// 항목이 **있고**, `state = in_flight` 이고, 저장된 값이 `last_seen_at_ms` 보다
    /// 이전일 때만 쓴다. 확정·포기된 레코드를 되살리지 않는다.
    ///
    /// 반환값은 **실제로 갱신했는가**다. `false` 면 그 레코드는 이 워커가 갱신할 대상이
    /// 아니다(없거나 이미 닫혔다) — 호출부가 재시도하면 매 tick 낭비가 되므로 구분한다.
    ///
    /// 인자는 **쓰기가 알려 준 자리와 갱신할 값뿐**이다. `SlowQuery` 를 받으면 그걸
    /// 만들기 위해 정책 판정을 거쳐야 하고, 위 표의 두 번째 줄이 그렇게 들어왔다.
    /// 키를 여기서 계산하거나 뒤져서 찾지 않는 이유는 [`StoredKey`] 에 있다.
    /// `duration_ms` 를 **함께** 올린다.
    ///
    /// 화면은 진행 중 경과를 `duration_ms + (지금 − last_seen_at_ms)` 로 센다. 두 값을
    /// 따로 쓰면 짝이 어긋나 **10분째 도는 쿼리가 2초로** 보인다(하트비트만 올리면
    /// 갱신 시각은 신선한데 소요는 첫 저장에서 굳어 있다). 둘은 같은 관측이므로 같이 쓴다.
    ///
    /// # 소요는 줄어들지 않는다 — 다만 **그 보장은 호출부에 있다**
    ///
    /// 처음에는 `max_time_secs × 1000` 을 넘겼다. `PROCESSLIST.TIME` 은 "현재 상태에 머문
    /// 시간" 이라 상태 전이에서 리셋되므로, 102초 도는 문장의 저장된 정밀값을 **17초로
    /// 줄였다**(교차 리뷰 27라운드). 지금은 추적기가 모든 관측 증거의 최대값
    /// (`Tracked::duration_ms`)을 들고 있고 그 값과 출처를 함께 넘긴다.
    async fn touch_in_flight(
        &self,
        key: &StoredKey,
        last_seen_at_ms: EpochMs,
        duration_ms: i64,
        duration_source: crate::slow_query::DurationSource,
    ) -> Result<bool>;
}

/// 다이제스트 롤업 저장.
#[async_trait]
pub trait DigestStore: Send + Sync {
    /// 배치 쓰기. **부분 실패한 항목을 반환한다** — 호출자가 그 hour 를 큐에 남겨
    /// 다음 정시에 재시도한다 (F20).
    async fn put_rollups(
        &self,
        instance: &InstanceId,
        rows: &[DigestRollupRow],
    ) -> Result<Vec<DigestRollupRow>>;

    /// 다이제스트 사전 upsert. `mysql_digests` · `seen_instances` 는 **원자 경로 갱신**으로
    /// 처리해야 한다 — JSON 문자열로 read-modify-write 하면 서로의 엔트리를 잃는다 (F8).
    async fn upsert_digest_text(&self, entry: &DigestTextEntry) -> Result<()>;
}

/// `DT#<app_digest>` / `META` 항목의 갱신 단위.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestTextEntry {
    pub app_digest: String,
    /// **평문.** 바인드 변수화된 정규화 SQL.
    pub digest_text: String,
    pub digest_text_source: DigestTextSource,
    pub statement_type: String,
    /// 이 인스턴스에서 **이번에** 관측된 `mysql_digest`.
    ///
    /// # 어댑터는 반드시 집합에 **추가**해야 한다 (19 §D)
    ///
    /// `max_digest_length` 가 인스턴스마다 다르면 같은 SQL 이 인스턴스별로 다른
    /// `mysql_digest` 를 갖는다. 한 인스턴스 안에서도 설정 변경 전후로 값이 달라진다.
    /// 따라서 관계는 `app_digest` 1 : `mysql_digest` N 이고, 저장 형태는
    /// `mysql_digests = { <instance_id>: Set<mysql_digest> }` 여야 한다.
    ///
    /// ```text
    /// 올바름:  ADD mysql_digests.#inst :new_set     ← 학습이 누적된다
    /// 틀림:    SET mysql_digests.#inst = :digest    ← 이전 학습을 덮어써 잃는다
    /// ```
    ///
    /// 이 필드가 값 하나인 것은 **한 번의 관측**이 하나라는 뜻이지, 저장 슬롯이
    /// 하나라는 뜻이 아니다. 덮어쓰면 M4-16 무성 유실이 재발한다.
    pub mysql_digest: Option<(InstanceId, String)>,
    pub seen_instance: InstanceId,
    pub referenced_tables: Vec<String>,
    pub digest_algo_version: u32,
    pub observed_at_ms: EpochMs,
    /// `QUERY_SAMPLE_TEXT`. **리터럴 정책이 적용된 형태여야 한다.**
    pub ps_sample_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestTextSource {
    /// MySQL `DIGEST_TEXT` 를 그대로 썼다.
    Mysql,
    /// 우리 정규화 결과를 썼다 (슬로우로그 등 원문만 있는 경로).
    App,
}

/// 인스턴스 레지스트리.
#[async_trait]
pub trait InstanceRegistry: Send + Sync {
    async fn list(&self) -> Result<Vec<Instance>>;
    async fn get(&self, id: &InstanceId) -> Result<Option<Instance>>;
    async fn upsert(&self, instance: &Instance) -> Result<()>;
    /// 탐색에서 발견되지 않았다. **2회 연속일 때만** 삭제로 판정한다(FR-DSC-07).
    async fn mark_missing(&self, id: &InstanceId, now_ms: EpochMs) -> Result<Instance>;
    /// 탐색에서 다시 보였다 — `missing_count` 를 0으로 되돌린다.
    async fn mark_seen(&self, id: &InstanceId, now_ms: EpochMs) -> Result<()>;

    /// **상태만** 갱신한다 (수집 태스크의 첫 판정: `Pending` → `Collecting`/`Unreachable`).
    ///
    /// # 왜 `upsert` 가 아닌가
    ///
    /// 수집 태스크가 든 `Instance` 는 태스크가 뜬 시점의 **사본**이다. 그걸 그대로
    /// `upsert` 하면 그 사이 탐색이 갱신한 값(`last_seen_ms`·태그·버전)을 되돌린다 —
    /// 되돌린 사실이 로그에도 안 남아 "왜 태그가 옛것으로 보이나" 로만 나타난다.
    async fn set_state(&self, id: &InstanceId, state: crate::instance::InstanceState)
    -> Result<()>;
}

/// 수집 정지 스코프 저장 ([`crate::pause`]).
///
/// # 왜 저장소인가
///
/// 정지는 **운영자의 의도**이지 프로세스 상태가 아니다. 프로세스 원자값으로 두면
/// 재배포에 풀리고, 리더가 바뀌면 정지 플래그가 없는 워커가 수집을 이어간다 —
/// 화면에는 "멈춤" 인데 기록은 계속 쌓인다. 저장소에 두면 모든 워커가 같은 것을 본다.
#[async_trait]
pub trait PauseStore: Send + Sync {
    /// 멈춰 있는 스코프 전부. 한 파티션이므로 조회 1회다.
    async fn list(&self) -> Result<PauseSet>;

    /// 멈춘다. **이미 멈춰 있으면 시작 시각을 덮지 않는다** — 화면이 "3분 전부터
    /// 멈춤" 을 말할 수 있어야 한다.
    async fn pause(&self, scope: &PauseScope, by: &str, now_ms: EpochMs) -> Result<()>;

    /// 재개한다. **멈춰 있지 않아도 성공이다**(멱등) — 두 사람이 같이 눌러도
    /// 두 번째가 오류로 보이면 안 된다.
    async fn resume(&self, scope: &PauseScope) -> Result<()>;
}

/// 운영자가 화면에서 바꾸는 설정 ([`crate::settings`]).
///
/// # 왜 낙관적 잠금인가
///
/// 저장은 문서 전체를 덮어쓴다(항목 하나에 담기 때문이다). 두 관리자가 같은 화면을
/// 열어 두고 각자 저장하면 **나중 저장이 앞선 변경을 조용히 지운다** — Slack 참조를
/// 방금 넣었는데 다른 사람이 리전을 저장하면서 그게 사라지는 식이다.
/// 그래서 `save` 는 "내가 읽은 버전" 을 함께 받고, 어긋나면 [`DomainError::Conflict`]
/// 로 거부한다. 화면은 다시 읽어 보여준다.
#[async_trait]
pub trait SettingsStore: Send + Sync {
    /// 저장된 설정. **없으면 기본값이다**(오류가 아니다) — 처음 뜬 배포에서 설정
    /// 화면이 500 이 되면 손댈 방법이 없다.
    async fn load(&self) -> Result<crate::settings::AppSettings>;

    /// 저장한다. `expected_version` 이 저장된 값과 다르면 거부한다.
    /// 반환값은 **저장된 뒤의** 설정이다(버전이 올라간 상태).
    async fn save(
        &self,
        settings: &crate::settings::AppSettings,
        expected_version: u32,
        by: &str,
        now_ms: EpochMs,
    ) -> Result<crate::settings::AppSettings>;
}

/// 리스 — 샤드 소유권과 리더 선출 ([05 §7](../../../.claude/docs/05-collector.md)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub key: String,
    pub owner: String,
    pub expires_at_ms: EpochMs,
    /// 획득할 때마다 증가한다. 2단계 펜싱 토큰의 기반 (M12-21).
    pub epoch: u64,
}

/// 리스 TTL. 갱신 주기는 이 값의 1/3 이다 (3회 갱신 기회).
pub const LEASE_TTL_MS: i64 = 60_000;
/// 리스 갱신 주기.
pub const LEASE_RENEW_INTERVAL_MS: i64 = 20_000;
/// 샤드 수. 고정값이다 — 바꾸면 재할당이 전면 발생한다.
pub const SHARD_COUNT: u32 = 64;
/// 수집 리더 리스 키. 이걸 못 잡은 워커의 목표 샤드 수는 **0** 이다 (F1).
pub const COLLECT_LEADER_KEY: &str = "LEADER#collect";
/// cron 리더 리스 키. 수집 리더와 **별도**다 (스케일 축이 다르다).
pub const CRON_LEADER_KEY: &str = "LEADER#cron";

/// 사용자 레코드 저장소 — **RBAC 의 권위값** (T-20, [08 §3](../../../.claude/docs/08-security-auth.md)).
///
/// # 왜 토큰만으로 인가하지 않는가
///
/// `cognito:groups` 는 IdP 속성 매핑과 Pre Token Generation Lambda 를 거쳐 온다. 그
/// 경로 중 하나라도 오작동하면 임의의 그룹이 클레임에 들어올 수 있다. 서버 레코드와
/// 교집합해야 토큰이 권한을 **올릴 수 없다**.
///
/// # 레코드가 없으면 권한이 없다 (fail-closed)
///
/// IdP 로 처음 로그인한 사용자는 레코드가 없다. 그 상태는 "기본 권한" 이 아니라
/// **승인 대기**다 — admin 이 레코드를 만들어야 들어온다.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// `sub` 로 레코드를 읽는다. 없으면 `None` (권한 없음).
    async fn get(&self, subject: &str) -> Result<Option<crate::rbac::UserRecord>>;
}

#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// 조건부 획득: 소유자가 없거나 만료됐을 때만 성공한다.
    /// 성공 시 `epoch` 를 1 올린다.
    async fn try_acquire(&self, key: &str, owner: &str, now_ms: EpochMs) -> Result<Option<Lease>>;

    /// 갱신: `owner` 와 `epoch` 가 모두 일치할 때만 성공한다.
    /// 실패하면 그 리스를 포기해야 한다.
    async fn renew(&self, lease: &Lease, now_ms: EpochMs) -> Result<Option<Lease>>;

    /// 명시적 반납 — 즉시 재분배된다(그레이스풀 셧다운 경로).
    async fn release(&self, lease: &Lease) -> Result<()>;

    async fn list(&self, key_prefix: &str) -> Result<Vec<Lease>>;
}

/// `instance_id` → 샤드 번호. **`instance_id` 문자열 전체를 해싱한다**
/// (계정을 포함하므로 계정이 다르면 다른 샤드에 갈 수 있다).
pub fn shard_of(instance: &InstanceId) -> u32 {
    // FNV-1a — 결정론적이어야 하고 워커·버전 간에 같아야 한다.
    // `DefaultHasher` 는 릴리스마다 값이 달라질 수 있어 쓸 수 없다.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in instance.as_str().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    (hash % SHARD_COUNT as u64) as u32
}

pub fn shard_key(shard: u32) -> String {
    format!("SHARD#{shard:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(name: &str) -> InstanceId {
        InstanceId::new("123456789012", "ap-northeast-2", name).unwrap()
    }

    #[test]
    fn shard_is_deterministic_and_in_range() {
        let a = inst("orders-prd-01");
        assert_eq!(shard_of(&a), shard_of(&a));
        for n in ["a", "orders-prd-01", "very-long-instance-name-here", "z9"] {
            assert!(shard_of(&inst(n)) < SHARD_COUNT);
        }
    }

    #[test]
    fn shard_hash_is_stable_across_builds() {
        // 이 값이 바뀌면 배포 시 전 인스턴스가 재할당된다.
        // `DefaultHasher` 를 쓰면 릴리스마다 값이 달라져 이 테스트가 존재할 수 없다.
        //
        // ⚠ **입력 문자열이 바뀌면 이 값도 바뀐다** — `instance_id` 전체를 해싱하므로
        // 계정 번호가 들어간다. 공개 저장소로 옮기며 예시 계정으로 바꿀 때 실제로
        // 깨졌고, 그게 이 테스트가 잡아야 하는 부류(알고리즘 변경)와 구분돼야 한다.
        assert_eq!(shard_of(&inst("orders-prd-01")), 32);
        assert_eq!(shard_of(&inst("orders-prd-02")), 57);
    }

    #[test]
    fn shard_distribution_is_not_degenerate() {
        use std::collections::BTreeSet;
        let used: BTreeSet<u32> = (0..500)
            .map(|i| shard_of(&inst(&format!("db-instance-{i:03}"))))
            .collect();
        // 500대를 64샤드에 뿌리면 거의 모든 샤드가 쓰여야 한다.
        assert!(
            used.len() >= 60,
            "샤드 분포가 편중됐다: {}개만 사용",
            used.len()
        );
    }

    #[test]
    fn account_affects_shard() {
        let a = InstanceId::new("111111111111", "ap-northeast-2", "prod-db-01").unwrap();
        let b = InstanceId::new("222222222222", "ap-northeast-2", "prod-db-01").unwrap();
        assert_ne!(a.as_str(), b.as_str());
        // 같은 샤드일 수도 있지만(64분의 1), 키가 다르므로 리스가 겹치지 않는다.
        assert_ne!(a, b);
    }

    #[test]
    fn lease_timings_allow_three_renewals() {
        assert_eq!(LEASE_TTL_MS / LEASE_RENEW_INTERVAL_MS, 3);
    }

    #[test]
    fn shard_key_is_zero_padded_for_sortability() {
        assert_eq!(shard_key(3), "SHARD#03");
        assert_eq!(shard_key(63), "SHARD#63");
    }
}
