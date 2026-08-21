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
            Ok(s) => {
                let mut c = self.cache.lock().expect("settings cache");
                c.settings = s.clone();
                c.loaded_ms = Some(now_ms);
                s
            }
            Err(e) => {
                let last = self.cached();
                tracing::warn!(
                    error = %crate::telemetry::Scrubbed(&e),
                    version = last.version,
                    "설정을 읽을 수 없다 — 마지막으로 읽은 값을 유지한다"
                );
                last
            }
        }
    }

    /// 지금 들고 있는 값. **I/O 를 하지 않는다.**
    pub fn cached(&self) -> AppSettings {
        self.cache.lock().expect("settings cache").settings.clone()
    }

    /// 한 번이라도 읽은 적이 있는가. 기동 직후 기본값과 "정말 비어 있음" 을 구분한다.
    pub fn is_loaded(&self) -> bool {
        self.cache.lock().expect("settings cache").loaded_ms.is_some()
    }

    fn fresh(&self, now_ms: EpochMs) -> Option<AppSettings> {
        let c = self.cache.lock().expect("settings cache");
        let loaded = c.loaded_ms?;
        (now_ms - loaded < CACHE_TTL_MS).then(|| c.settings.clone())
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
        let mut c = self.cache.lock().expect("settings cache");
        c.settings = saved.clone();
        c.loaded_ms = Some(now_ms);
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
        assert_eq!(f.loads.load(Ordering::SeqCst), 2, "TTL 이 지났는데 안 읽었다");
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
            }),
        };
        let after = broken.refresh(CACHE_TTL_MS * 2).await;
        assert_eq!(
            after.discovery.regions,
            vec!["us-east-1", "eu-west-1"],
            "실패했는데 기본값으로 접혔다 — 탐색이 자기 리전 하나로 줄어든다"
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
        assert_eq!(st.load(5_001).await.discovery.regions, vec!["ap-northeast-2"]);
        assert_eq!(f.loads.load(Ordering::SeqCst), 1, "저장 후 불필요한 재조회");
    }

    #[tokio::test]
    async fn a_stale_version_is_rejected() {
        let f = fake(false, AppSettings::default());
        let st = SettingsState::new(f);
        st.save(&AppSettings::default(), 0, "a", 1).await.expect("첫 저장");
        // 같은 버전으로 두 번째 저장 — 다른 관리자가 먼저 저장한 상황이다.
        let err = st.save(&AppSettings::default(), 0, "b", 2).await.expect_err("거부돼야 한다");
        assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
    }
}
