//! 수집 정지 스코프 — 전체 / 환경 / 인스턴스 개별.
//!
//! # 왜 스위치 하나로는 안 되는가
//!
//! 처음에는 `paused: bool` 하나였다. 그러면 "prd 만 멈춰 두고 dev 는 계속 본다" 를
//! 표현할 방법이 없어 **운영 중 유지보수 창구가 전부-아니면-전무**가 된다. 실제로
//! 필요한 것은 스코프별 정지이고, 여러 스코프가 동시에 멈춰 있을 수 있다
//! (`prd` 정지 + `orders-dev-01` 정지).
//!
//! # 왜 키 문자열을 여기서만 만드는가
//!
//! 저장소(DynamoDB SK)·API 본문·화면 칩이 **같은 문자열**을 쓴다. 각자 만들면 한쪽만
//! 바뀔 때 조회가 조용히 0건이 된다 — 이 프로젝트에서 키 불일치는 이미 반복된
//! 실패 유형이다([`crate::ids`] · `store::keys`).
//!
//! # 정지는 수집기 상태가 아니라 **운영자의 의도**다
//!
//! 그래서 프로세스 원자값이 아니라 저장소에 둔다. 재시작·리더 교체에도 남아야
//! "prd 정지" 가 유지된다. 이 모듈은 그 의도를 표현하는 순수 타입만 담고,
//! 저장은 [`crate::ports::PauseStore`] 구현이 한다.

use std::collections::BTreeMap;

use crate::env::Env;
use crate::ids::InstanceId;
use crate::instance::Instance;
use crate::time::EpochMs;

/// 정지 대상.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseScope {
    /// 이 수집기가 보는 전부.
    All,
    /// 한 환경의 인스턴스 전부.
    Env(Env),
    /// 인스턴스 하나.
    Instance(InstanceId),
}

/// 전체 정지 키. 와일드카드 의미를 그대로 쓴다.
pub const ALL_KEY: &str = "*";
const ENV_PREFIX: &str = "env:";
const ID_PREFIX: &str = "id:";

fn env_key(env: Env) -> String {
    format!("{ENV_PREFIX}{}", env.as_str())
}

fn id_key(id: &InstanceId) -> String {
    format!("{ID_PREFIX}{}", id.as_str())
}

impl PauseScope {
    /// 저장·전송에 쓰는 정규 키.
    pub fn as_key(&self) -> String {
        match self {
            Self::All => ALL_KEY.to_string(),
            Self::Env(e) => env_key(*e),
            Self::Instance(id) => id_key(id),
        }
    }

    /// 키를 스코프로 되돌린다. **모르는 키는 `None`** — 추측하지 않는다.
    ///
    /// 저장소에 사람이 손으로 넣은 키가 있을 수 있고, 그걸 임의로 해석하면
    /// 의도하지 않은 인스턴스가 멈춘다.
    pub fn parse(key: &str) -> Option<Self> {
        if key == ALL_KEY {
            return Some(Self::All);
        }
        if let Some(rest) = key.strip_prefix(ENV_PREFIX) {
            return Env::parse(rest).map(Self::Env);
        }
        if let Some(rest) = key.strip_prefix(ID_PREFIX) {
            return InstanceId::parse(rest).ok().map(Self::Instance);
        }
        None
    }
}

/// 멈춰 있는 스코프 집합. **키 → 멈춘 시각.**
///
/// 시각을 함께 드는 이유는 화면이 "3분 전부터 멈춤" 을 말할 수 있어야 하기
/// 때문이다. 같은 스코프를 다시 눌러도 시각은 덮이지 않는다(그 규칙은 저장소가
/// `if_not_exists` 로 지킨다).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PauseSet {
    since: BTreeMap<String, EpochMs>,
}

impl PauseSet {
    pub fn from_entries(entries: impl IntoIterator<Item = (String, EpochMs)>) -> Self {
        Self {
            since: entries.into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.since.is_empty()
    }

    pub fn len(&self) -> usize {
        self.since.len()
    }

    /// `(키, 멈춘 시각)` — 키 순서다(`*` → `env:*` → `id:*`).
    pub fn entries(&self) -> impl Iterator<Item = (&str, EpochMs)> {
        self.since.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn since_ms(&self, scope: &PauseScope) -> Option<EpochMs> {
        self.since.get(&scope.as_key()).copied()
    }

    pub fn contains(&self, scope: &PauseScope) -> bool {
        self.since.contains_key(&scope.as_key())
    }

    /// 전부 멈춰 있는가. 리더 루프가 **드레인 경로**를 탈 조건이다.
    pub fn is_all_paused(&self) -> bool {
        self.since.contains_key(ALL_KEY)
    }

    /// 가장 먼저 멈춘 시각. 부분 정지에서도 "언제부터" 를 말할 수 있어야 한다.
    pub fn earliest_since_ms(&self) -> Option<EpochMs> {
        self.since.values().copied().min()
    }

    /// 이 인스턴스의 수집이 멈춰 있는가.
    ///
    /// **적용되는 환경(`effective`)으로 본다.** 태그 값이 아니라 오버라이드가 이긴
    /// 값이다 — 화면에서 `dev` 로 지정한 인스턴스는 `env:dev` 정지에 걸려야 한다.
    pub fn is_paused(&self, instance: &Instance) -> bool {
        self.is_paused_id(&instance.id, instance.env.effective)
    }

    pub fn is_paused_id(&self, id: &InstanceId, env: Env) -> bool {
        self.is_all_paused()
            || self.since.contains_key(&env_key(env))
            || self.since.contains_key(&id_key(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(identifier: &str) -> InstanceId {
        InstanceId::new("123456789012", "ap-northeast-2", identifier).expect("id")
    }

    /// **키 왕복이 깨지면 저장한 정지를 다시 읽을 수 없다.**
    #[test]
    fn every_scope_survives_a_key_round_trip() {
        let mut scopes = vec![PauseScope::All, PauseScope::Instance(id("orders-prd-01"))];
        scopes.extend(Env::ALL.iter().copied().map(PauseScope::Env));
        for s in scopes {
            let key = s.as_key();
            assert_eq!(PauseScope::parse(&key), Some(s.clone()), "키={key}");
        }
    }

    /// **모르는 키는 스코프가 되지 않는다.** 임의 해석은 엉뚱한 인스턴스를 멈춘다.
    #[test]
    fn unknown_keys_are_rejected() {
        for key in [
            "",
            "prd",                              // 접두어 없음
            "env:",                             // 값 없음
            "env:production",                   // 매핑 이름이 아니다 (`prd` 만 유효)
            "id:",                              //
            "id:bogus",                         // 3-파트가 아니다
            "id:acct/region",                   // 식별자 없음
            "ID:123456789012/ap-northeast-2/x", // 대문자 접두어
            "*x",
        ] {
            assert_eq!(PauseScope::parse(key), None, "키={key} 는 거부해야 한다");
        }
    }

    /// 빈 집합은 아무것도 멈추지 않는다 — **기본이 수집**이다.
    #[test]
    fn an_empty_set_pauses_nothing() {
        let set = PauseSet::default();
        assert!(set.is_empty());
        assert!(!set.is_all_paused());
        assert!(!set.is_paused_id(&id("orders-prd-01"), Env::Prd));
        assert_eq!(set.earliest_since_ms(), None);
    }

    /// 스코프별 판정 진리표. **여기가 틀리면 안 멈춰야 할 인스턴스가 멈춘다.**
    #[test]
    fn a_scope_pauses_exactly_what_it_names() {
        let prd = (id("orders-prd-01"), Env::Prd);
        let dev = (id("orders-dev-01"), Env::Dev);
        let other_dev = (id("billing-dev-01"), Env::Dev);

        let all = PauseSet::from_entries([(ALL_KEY.to_string(), 1_000)]);
        for (i, e) in [&prd, &dev, &other_dev] {
            assert!(
                all.is_paused_id(i, *e),
                "전체 정지는 {} 도 멈춘다",
                i.as_str()
            );
        }

        let env_dev = PauseSet::from_entries([(PauseScope::Env(Env::Dev).as_key(), 1_000)]);
        assert!(!env_dev.is_all_paused());
        assert!(env_dev.is_paused_id(&dev.0, dev.1));
        assert!(env_dev.is_paused_id(&other_dev.0, other_dev.1));
        assert!(!env_dev.is_paused_id(&prd.0, prd.1), "prd 는 안 멈춘다");

        let one = PauseSet::from_entries([(PauseScope::Instance(dev.0.clone()).as_key(), 1_000)]);
        assert!(one.is_paused_id(&dev.0, dev.1));
        assert!(
            !one.is_paused_id(&other_dev.0, other_dev.1),
            "같은 환경의 다른 인스턴스는 안 멈춘다"
        );
        assert!(!one.is_paused_id(&prd.0, prd.1));
    }

    /// 여러 스코프가 **동시에** 멈춰 있을 수 있다. 화면은 가장 이른 시각을 말한다.
    #[test]
    fn scopes_stack_and_the_earliest_start_wins() {
        let set = PauseSet::from_entries([
            (PauseScope::Env(Env::Prd).as_key(), 5_000),
            (PauseScope::Instance(id("orders-dev-01")).as_key(), 2_000),
        ]);
        assert_eq!(set.len(), 2);
        assert_eq!(set.earliest_since_ms(), Some(2_000));
        assert_eq!(set.since_ms(&PauseScope::Env(Env::Prd)), Some(5_000));
        assert_eq!(set.since_ms(&PauseScope::Env(Env::Dev)), None);
        assert!(set.contains(&PauseScope::Env(Env::Prd)));
        assert!(set.is_paused_id(&id("orders-prd-01"), Env::Prd));
        assert!(set.is_paused_id(&id("orders-dev-01"), Env::Dev));
        assert!(!set.is_paused_id(&id("billing-dev-01"), Env::Dev));
    }
}
