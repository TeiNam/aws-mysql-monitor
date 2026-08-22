//! 설정의 **공유 캐시**. [`crate::control::PauseState`] 와 같은 모양이다.
//!
//! # 왜 캐시인가
//!
//! 설정을 읽는 곳이 셋이다: 탐색 루프(리전·계정), 인증 계층(로그인 방식), 화면.
//! 앞의 둘은 **요청마다·tick 마다** 읽으므로 매번 DynamoDB 를 때리면 tick 안에 왕복이
//! 하나 늘고, 리더 루프가 늦으면 리스를 놓친다([05 §7](../../../docs/05-collector.md)).
//!
//! 04 §3 이 규정한 폴링 주기(30초)를 캐시 TTL 로 쓴다. 저장 직후에는 캐시를 무시하고
//! 다시 읽으므로(`save`), **누른 사람에게는 즉시 반영**된다. 다른 워커는 최대 30초
//! 늦는데, 리전 하나를 추가하는 일에 30초는 보이지 않는다.
//!
//! # 왜 실패하면 마지막 값을 쓰는가
//!
//! 조회 실패를 기본값으로 접으면 **탐색 범위가 갑자기 자기 리전 하나로 줄고** 다른
//! 리전·계정의 인스턴스가 등록부에서 사라진 것처럼 보인다(수집도 멈춘다). 저장소가
//! 흔들리는 것과 "운영자가 리전을 지웠다" 는 전혀 다른 사실이므로 섞지 않는다.

use std::sync::{Arc, Mutex};

use dbmon_core::ports::SettingsStore;
use dbmon_core::settings::AppSettings;
use dbmon_core::time::EpochMs;

/// 캐시 유효 기간. [04 §3](../../../docs/04-data-model.md) 의 폴링 주기와 같다.
pub const CACHE_TTL_MS: i64 = 30_000;

#[derive(Default)]
struct Cached {
    settings: AppSettings,
    /// 마지막으로 **성공한** 조회 시각. `None` 이면 아직 한 번도 못 읽었다.
    loaded_ms: Option<EpochMs>,
    /// 마지막 조회가 실패한 사유. **화면이 이걸 표시해야 한다** — 실패를 조용히
    /// 마지막 값으로 덮으면, 손상된 문서 위에서 관리자가 "정상" 을 보고 저장을 누른다.
    last_error: Option<String>,
}

pub struct SettingsState {
    store: Arc<dyn SettingsStore>,
    cache: Mutex<Cached>,
}

impl SettingsState {
    pub fn new(store: Arc<dyn SettingsStore>) -> Self {
        Self {
            store,
            cache: Mutex::new(Cached::default()),
        }
    }

    /// 캐시가 유효하면 그대로, 아니면 저장소에서 읽는다.
    pub async fn load(&self, now_ms: EpochMs) -> AppSettings {
        if let Some(fresh) = self.fresh(now_ms) {
            return fresh;
        }
        self.refresh(now_ms).await
    }

    /// 캐시를 무시하고 읽는다.
    pub async fn refresh(&self, now_ms: EpochMs) -> AppSettings {
        match self.store.load().await {
            Ok(s) => self.apply(s, now_ms),
            Err(e) => {
                let last = self.cached();
                tracing::warn!(
                    error = %crate::telemetry::Scrubbed(&e),
                    version = last.version,
                    "설정을 읽을 수 없다 — 마지막으로 읽은 값을 유지한다"
                );
                if let Ok(mut c) = self.cache.lock() {
                    // 사유는 스크럽한 것만 남긴다 — 화면에 그대로 나가는 문자열이다.
                    c.last_error = Some(crate::telemetry::scrub(&e.to_string()));
                }
                last
            }
        }
    }

    /// 캐시에 값을 적용한다. **모든 경로가 이 함수를 지난다** (조회·폴러·저장).
    ///
    /// # 왜 한 곳인가 (교차 리뷰가 세 번 지적했다)
    ///
    /// 세 경로가 각자 캐시를 쓰면 순서가 어긋난다:
    ///
    /// | 상황 | 각자 쓸 때 |
    /// |---|---|
    /// | 폴러가 v1 을 읽는 중 관리자가 v2 저장 | 늦게 끝난 v1 이 캐시를 되돌린다 |
    /// | 이 워커가 v2 저장, 다른 워커가 v3 저장 → 폴러가 v3 캐시 | 늦은 v2 저장 응답이 v3 을 덮는다 |
    /// | 같은 버전의 늦은 조회 | `loaded_ms` 를 과거로 되돌려 TTL 여유가 준다 |
    ///
    /// **인증이 그 캐시를 보므로** 되돌아간 값은 곧 되돌아간 인증이다. 버전은 저장마다
    /// 오르므로 비교만으로 순서를 회복할 수 있고, 같은 버전이면 **더 늦은 시각**을 남긴다.
    fn apply(&self, next: AppSettings, now_ms: EpochMs) -> AppSettings {
        let mut c = self.cache.lock().expect("settings cache");
        if next.version < c.settings.version {
            tracing::debug!(
                stale = next.version,
                current = c.settings.version,
                "늦게 도착한 설정을 버린다 (더 새 값이 이미 있다)"
            );
            return c.settings.clone();
        }
        // 같은 버전이면 내용도 같다(버전은 저장마다 오른다). 그래도 덮어쓴다 —
        // "같으면 같다" 를 가정하는 대신 **더 오래된 것만** 버리는 규칙 하나로 둔다.
        c.settings = next;
        // 시각은 **더 늦은 쪽**이다 — 늦게 도착한 조회가 TTL 여유를 되돌리면 안 된다.
        c.loaded_ms = Some(c.loaded_ms.map_or(now_ms, |prev| prev.max(now_ms)));
        c.last_error = None;
        c.settings.clone()
    }

    /// 지금 읽고 **끝난 시각으로** 기록한다.
    ///
    /// `refresh(now)` 는 호출부가 준 시각을 쓰는데, 조회가 오래 걸리면 그 시각이
    /// 이미 과거다 — 30초 걸린 조회는 완료 즉시 낡은 값이 된다(3차 교차 리뷰).
    /// 폴러는 이걸 쓴다.
    pub async fn refresh_now(&self) {
        use dbmon_core::time::Clock as _;
        // 조회를 먼저 하고, 끝난 뒤의 시각으로 캐시에 기록한다.
        let loaded = self.store.load().await;
        let now_ms = dbmon_core::time::SystemClock.now_ms();
        match loaded {
            Ok(s) => {
                self.apply(s, now_ms);
            }
            Err(e) => {
                tracing::warn!(
                    error = %crate::telemetry::Scrubbed(&e),
                    "설정을 읽을 수 없다 — 마지막으로 읽은 값을 유지한다"
                );
                if let Ok(mut c) = self.cache.lock() {
                    c.last_error = Some(crate::telemetry::scrub(&e.to_string()));
                }
            }
        }
    }

    /// 지금 들고 있는 값. **I/O 를 하지 않는다.**
    ///
    /// ⚠ **인증 판정에 쓰지 마라.** 이 값은 오래됐을 수 있고(저장소 장애 시 무기한),
    /// "인증 없음" 이 캐시돼 있으면 인증을 다시 켜도 전파되지 않는다.
    /// 그 판정에는 [`Self::cached_fresh`] 를 쓴다.
    pub fn cached(&self) -> AppSettings {
        self.cache.lock().expect("settings cache").settings.clone()
    }

    /// **신선한** 값만. TTL 이 지났거나 한 번도 읽지 못했으면 `None`.
    ///
    /// # 왜 인증에는 이것만 쓰는가 (교차 리뷰가 critical 로 잡았다)
    ///
    /// `cached()` 는 조회 실패 때 마지막 값을 유지한다 — 탐색 범위에는 그게 맞다
    /// (범위가 갑자기 줄면 인스턴스가 사라진 것처럼 보인다). **인증은 반대다.**
    /// `off` 가 캐시된 워커에서 관리자가 인증을 다시 켰는데 그 뒤로 저장소가 죽으면,
    /// 마지막 값을 믿는 한 익명 admin 접근이 **무기한** 계속된다.
    ///
    /// 그래서 인증 판정은 신선한 값만 보고, 없으면 켜진 쪽(토큰)으로 떨어진다.
    /// 대가: 저장소가 죽은 동안 "인증 없음" 배포는 토큰을 요구하게 된다 — 접근이
    /// 막히는 것이 열리는 것보다 낫다.
    pub fn cached_fresh(&self, now_ms: EpochMs) -> Option<AppSettings> {
        self.fresh(now_ms)
    }

    /// 한 번이라도 읽은 적이 있는가. 기동 직후 기본값과 "정말 비어 있음" 을 구분한다.
    pub fn is_loaded(&self) -> bool {
        self.cache
            .lock()
            .expect("settings cache")
            .loaded_ms
            .is_some()
    }

    /// 마지막 조회가 실패했으면 그 사유. 성공했으면 `None`.
    ///
    /// **화면이 이 값을 보여줘야 한다.** 실패를 마지막 값으로 조용히 덮으면, 손상된
    /// 문서 위에서 관리자가 "빈 설정" 을 정상으로 보고 저장을 누른다.
    pub fn last_error(&self) -> Option<String> {
        self.cache
            .lock()
            .expect("settings cache")
            .last_error
            .clone()
    }

    fn fresh(&self, now_ms: EpochMs) -> Option<AppSettings> {
        let c = self.cache.lock().expect("settings cache");
        let loaded = c.loaded_ms?;
        // **시계가 뒤로 가면 신선하지 않다고 본다.** `now < loaded` 면 차이가 음수라
        // TTL 비교를 그냥 통과하고, 그건 오래된 `off` 를 신선한 것으로 만든다
        // (2차 교차 리뷰가 low 로 잡았다 — NTP 보정·컨테이너 시각 점프에서 실제로 난다).
        let age = now_ms.checked_sub(loaded)?;
        (0..CACHE_TTL_MS).contains(&age).then(|| c.settings.clone())
    }

    /// 저장하고 **캐시를 갱신한다.** 저장한 사람의 다음 조회가 옛 값을 보면
    /// "저장이 안 됐다" 로 읽힌다.
    pub async fn save(
        &self,
        settings: &AppSettings,
        expected_version: u32,
        by: &str,
        now_ms: EpochMs,
    ) -> dbmon_core::Result<AppSettings> {
        let saved = self
            .store
            .save(settings, expected_version, by, now_ms)
            .await?;
        // **같은 함수를 지난다.** 저장 응답이 늦게 도착해도 더 새 값을 덮지 않는다.
        self.apply(saved.clone(), now_ms);
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::error::DomainError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 호출 횟수를 세는 가짜 저장소. 실패 모드를 켤 수 있다.
    struct Fake {
        loads: AtomicUsize,
        fail: bool,
        stored: Mutex<AppSettings>,
    }

    #[async_trait::async_trait]
    impl SettingsStore for Fake {
        async fn load(&self) -> dbmon_core::Result<AppSettings> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(DomainError::Unavailable {
                    dependency: "dynamodb",
                    reason: "테스트".into(),
                });
            }
            Ok(self.stored.lock().unwrap().clone())
        }
        async fn save(
            &self,
            s: &AppSettings,
            expected: u32,
            by: &str,
            now_ms: EpochMs,
        ) -> dbmon_core::Result<AppSettings> {
            let mut cur = self.stored.lock().unwrap();
            if cur.version != expected {
                return Err(DomainError::Conflict("버전 불일치".into()));
            }
            let mut next = s.clone();
            next.version = expected + 1;
            next.updated_by = by.to_string();
            next.updated_at_ms = now_ms;
            *cur = next.clone();
            Ok(next)
        }
    }

    fn fake(fail: bool, stored: AppSettings) -> Arc<Fake> {
        Arc::new(Fake {
            loads: AtomicUsize::new(0),
            fail,
            stored: Mutex::new(stored),
        })
    }

    #[tokio::test]
    async fn the_cache_absorbs_repeat_reads_within_the_ttl() {
        let mut stored = AppSettings::default();
        stored.discovery.regions = vec!["us-east-1".into()];
        let f = fake(false, stored);
        let st = SettingsState::new(f.clone());

        assert_eq!(st.load(1_000).await.discovery.regions, vec!["us-east-1"]);
        st.load(1_001).await;
        st.load(CACHE_TTL_MS + 999).await;
        assert_eq!(f.loads.load(Ordering::SeqCst), 1, "TTL 안인데 다시 읽었다");

        st.load(CACHE_TTL_MS + 1_000).await;
        assert_eq!(
            f.loads.load(Ordering::SeqCst),
            2,
            "TTL 이 지났는데 안 읽었다"
        );
    }

    /// **조회 실패가 탐색 범위를 지우면 안 된다.**
    #[tokio::test]
    async fn a_failed_read_keeps_the_last_known_settings() {
        let mut stored = AppSettings::default();
        stored.discovery.regions = vec!["us-east-1".into(), "eu-west-1".into()];
        let ok = fake(false, stored);
        let st = SettingsState::new(ok);
        st.load(0).await;

        // 같은 캐시에 실패하는 저장소를 물릴 수는 없으니, 실패 경로만 직접 부른다.
        let broken = SettingsState {
            store: fake(true, AppSettings::default()),
            cache: Mutex::new(Cached {
                settings: st.cached(),
                loaded_ms: Some(0),
                last_error: None,
            }),
        };
        let after = broken.refresh(CACHE_TTL_MS * 2).await;
        assert_eq!(
            after.discovery.regions,
            vec!["us-east-1", "eu-west-1"],
            "실패했는데 기본값으로 접혔다 — 탐색이 자기 리전 하나로 줄어든다"
        );
        // **실패 사실은 남는다.** 화면이 "저장하지 말라" 를 말할 근거다.
        assert!(broken.last_error().is_some(), "조회 실패를 조용히 삼켰다");
    }

    /// **인증 판정용 접근자는 오래된 값을 주지 않는다.**
    ///
    /// 교차 리뷰가 critical 로 잡은 경로다: `off` 가 캐시된 워커에서 인증을 다시 켠 뒤
    /// 저장소가 죽으면, 마지막 값을 믿는 한 익명 admin 접근이 무기한 계속된다.
    #[tokio::test]
    async fn the_auth_accessor_refuses_stale_values() {
        let mut stored = AppSettings::default();
        stored.auth.mode = dbmon_core::settings::AuthModeSetting::Off;
        let st = SettingsState::new(fake(false, stored));
        st.load(1_000).await;

        assert!(
            st.cached_fresh(1_000 + CACHE_TTL_MS - 1).is_some(),
            "TTL 안인데 신선하지 않다고 했다"
        );
        assert!(
            st.cached_fresh(1_000 + CACHE_TTL_MS).is_none(),
            "TTL 이 지난 값을 인증 판정에 내줬다 — 인증이 무기한 꺼진다"
        );
        // **시계가 뒤로 갔을 때도 신선하다고 하지 않는다.** 음수 차이는 TTL 비교를
        // 통과해 오래된 `off` 를 되살린다.
        assert!(
            st.cached_fresh(0).is_none(),
            "시계가 뒤로 간 상황에서 오래된 값을 신선하다고 했다"
        );
        // 반면 탐색 범위용 접근자는 마지막 값을 계속 준다(범위가 사라지면 안 된다).
        assert_eq!(
            st.cached().auth.mode,
            dbmon_core::settings::AuthModeSetting::Off
        );
    }

    #[tokio::test]
    async fn saving_updates_the_cache_immediately() {
        let f = fake(false, AppSettings::default());
        let st = SettingsState::new(f.clone());
        st.load(0).await;

        let mut next = st.cached();
        next.discovery.regions = vec!["ap-northeast-2".into()];
        let saved = st.save(&next, 0, "admin", 5_000).await.expect("저장");
        assert_eq!(saved.version, 1);
        // TTL 안이지만 방금 저장한 값이 보여야 한다.
        assert_eq!(
            st.load(5_001).await.discovery.regions,
            vec!["ap-northeast-2"]
        );
        assert_eq!(f.loads.load(Ordering::SeqCst), 1, "저장 후 불필요한 재조회");
    }

    /// **늦게 도착한 옛 버전이 새 값을 덮지 않는다.**
    ///
    /// 세 경로(조회·폴러·저장)가 각자 캐시를 쓰면 순서가 어긋나고, 인증이 그 캐시를
    /// 보므로 되돌아간 값은 곧 되돌아간 인증이다(4차 교차 리뷰가 high 로 잡았다).
    #[tokio::test]
    async fn a_late_older_version_never_overwrites_a_newer_one() {
        let st = SettingsState::new(fake(false, AppSettings::default()));

        let v3 = AppSettings {
            version: 3,
            auth: dbmon_core::settings::AuthSettings {
                mode: dbmon_core::settings::AuthModeSetting::Token,
                ..Default::default()
            },
            ..Default::default()
        };
        st.apply(v3, 10_000);

        // 늦게 도착한 v2 (`off`) — 버려야 한다.
        let v2 = AppSettings {
            version: 2,
            auth: dbmon_core::settings::AuthSettings {
                mode: dbmon_core::settings::AuthModeSetting::Off,
                ..Default::default()
            },
            ..Default::default()
        };
        st.apply(v2, 11_000);
        assert_eq!(
            st.cached().auth.mode,
            dbmon_core::settings::AuthModeSetting::Token,
            "늦게 도착한 옛 버전이 인증을 되돌렸다"
        );
        assert_eq!(st.cached().version, 3);

        // 같은 버전의 **더 이른** 시각은 신선함을 되돌리지 않는다.
        let same = AppSettings {
            version: 3,
            ..Default::default()
        };
        st.apply(same, 1_000);
        assert!(
            st.cached_fresh(10_000 + CACHE_TTL_MS - 1).is_some(),
            "늦게 도착한 조회가 TTL 여유를 과거로 되돌렸다"
        );
    }

    #[tokio::test]
    async fn a_stale_version_is_rejected() {
        let f = fake(false, AppSettings::default());
        let st = SettingsState::new(f);
        st.save(&AppSettings::default(), 0, "a", 1)
            .await
            .expect("첫 저장");
        // 같은 버전으로 두 번째 저장 — 다른 관리자가 먼저 저장한 상황이다.
        let err = st
            .save(&AppSettings::default(), 0, "b", 2)
            .await
            .expect_err("거부돼야 한다");
        assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
    }
}
