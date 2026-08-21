//! 리전별 CloudWatch 클라이언트.
//!
//! # 왜 리전마다 필요한가
//!
//! **메트릭은 대상 인스턴스의 리전에 있다.** 배포 리전 클라이언트로 다른 리전 인스턴스를
//! 물으면 오류가 아니라 **빈 값**이 오면서 과금된다 — 화면에는 `—` 로 보이고, 그건
//! "값이 0" 이나 "수집이 죽었다" 로 오해된다(탐색·슬로우로그에서 같은 함정을 이미 밟았다).
//!
//! 탐색 범위가 운영 설정이라 리전 목록이 **재시작 없이 늘어난다.** 그래서 기동 시 한 번
//! 만들 수 없고, 처음 필요할 때 만들어 들고 있는다.
//!
//! # 계정도 키에 들어간다
//!
//! 크로스 계정 인스턴스의 메트릭은 **그 계정의 CloudWatch 에 있다.** 우리 자격증명으로
//! 물으면 오류가 아니라 빈 값이 오고(교차 리뷰가 high 로 잡았다), 게다가 같은 이름의
//! 인스턴스가 두 계정에 있으면 **다른 계정의 지표가 붙는다.**
//!
//! 그래서 키가 `(계정, 리전)` 이고, 다른 계정이면 탐색과 같은 역할을 맡아
//! (`sts:AssumeRole`) 그 계정의 클라이언트를 만든다.
//!
//! # 실패를 캐시하지 않는다
//!
//! 클라이언트 생성은 자격증명 공급자 구성이고, 그 자체로는 실패하지 않는다(호출 시점에
//! 실패한다). 그래서 이 캐시에는 성공만 들어간다.

use std::collections::BTreeMap;
use std::sync::Arc;

use aws_config::BehaviorVersion;
use tokio::sync::Mutex;

use super::cloudwatch::MetricFetcher;

/// `(계정, 리전, 역할)` → 클라이언트. 처음 쓸 때 만든다.
///
/// **역할도 키다.** 설정에서 역할 이름을 바꿨는데 캐시가 옛 역할의 클라이언트를 계속
/// 주면, 바꾼 이유(권한 축소·역할 교체)가 반영되지 않는다(2차 교차 리뷰가 medium 으로
/// 잡았다). 옛 항목은 그대로 남지만 쓰이지 않고, 항목 하나는 몇 KB 다.
pub struct MetricFetchers {
    cache: Mutex<BTreeMap<(String, String, String), Arc<MetricFetcher>>>,
    /// 우리 계정. 이 계정이면 역할을 맡지 않는다.
    own_account: String,
}

impl MetricFetchers {
    pub fn new(own_account: impl Into<String>) -> Self {
        Self {
            cache: Mutex::new(BTreeMap::new()),
            own_account: own_account.into(),
        }
    }

    /// 이 `(계정, 리전)` 의 클라이언트. 없으면 만들어 캐시한다.
    ///
    /// `role_name` 은 다른 계정일 때 맡을 역할이다. `None` 이면(설정에 그 계정이 없다)
    /// **우리 자격증명으로 시도한다** — 그러면 빈 값이 오겠지만, 여기서 실패를 만들면
    /// 목록 전체가 막힌다. 빈 값과 그 사유는 상위 계층이 표시한다.
    pub async fn for_target(
        &self,
        account: &str,
        region: &str,
        role_name: Option<&str>,
    ) -> Arc<MetricFetcher> {
        let key = (
            account.to_string(),
            region.to_string(),
            role_name.unwrap_or("").to_string(),
        );
        // **락을 잡은 채 SDK 를 만든다.** 같은 대상에 동시 요청이 오면 클라이언트가
        // 둘 만들어지는 것을 막는다 — 만드는 비용은 ms 단위라 이 직렬화가 싸다.
        let mut cache = self.cache.lock().await;
        if let Some(hit) = cache.get(&key) {
            return Arc::clone(hit);
        }
        let aws_region = aws_config::Region::new(region.to_string());
        let sdk = match role_name.filter(|_| account != self.own_account) {
            None => {
                aws_config::defaults(BehaviorVersion::latest())
                    .region(aws_region)
                    .load()
                    .await
            }
            Some(role) => {
                let role_arn = format!("arn:aws:iam::{account}:role/{role}");
                let provider = aws_config::sts::AssumeRoleProvider::builder(role_arn.clone())
                    .session_name("dbmon-metrics")
                    .region(aws_region.clone())
                    .build()
                    .await;
                tracing::info!(role = %role_arn, %region, "크로스 계정 CloudWatch 클라이언트");
                aws_config::defaults(BehaviorVersion::latest())
                    .region(aws_region)
                    .credentials_provider(provider)
                    .load()
                    .await
            }
        };
        let fetcher = Arc::new(MetricFetcher::new(aws_sdk_cloudwatch::Client::new(&sdk)));
        tracing::info!(%account, %region, "CloudWatch 클라이언트를 만들었다");
        cache.insert(key, Arc::clone(&fetcher));
        fetcher
    }
}
