//! 탐색 대상별 AWS 클라이언트 묶음 — **리전 여러 개, 계정 여러 개.**
//!
//! # 왜 클라이언트를 미리 만들지 못하는가
//!
//! 탐색 범위가 운영 설정([`dbmon_core::settings::DiscoverySettings`])에 있고, 그건
//! 재시작 없이 바뀐다. 기동 시 한 번 만들면 리전을 추가해도 다음 배포까지 반영되지
//! 않는다 — 그러면 설정을 런타임에 둔 의미가 없다.
//!
//! 그래서 **설정 지문**을 함께 들고, 지문이 바뀔 때만 다시 만든다. 매 탐색마다 다시
//! 만들면 계정마다 `AssumeRole` 을 새로 호출한다(5분 주기라면 무해하지만, 즉시 탐색을
//! 연달아 누르면 STS 조절에 걸린다).
//!
//! # 크로스 계정: 역할을 맡는다
//!
//! mgmt 계정의 태스크 롤이 대상 계정의 역할을 맡아(`sts:AssumeRole`) 그 계정의 RDS 를
//! 나열한다. **VPC 피어링·TGW 는 여기와 무관하다** — 목록 조회는 AWS API 이므로
//! 네트워크 경로가 필요 없다. 피어링이 필요한 것은 그 다음 단계, **DB 에 붙어 수집할
//! 때**다. 그래서 목록에는 뜨지만 수집이 `unreachable` 인 상태가 정상적으로 존재하고,
//! 화면이 그 둘을 구분해 보여준다.
//!
//! # 대상 계정에 필요한 것
//!
//! ```text
//! 역할 dbmon-discovery
//!   신뢰 정책 : mgmt 계정의 태스크 롤만 (ExternalId 권장)
//!   권한      : rds:DescribeDBInstances, rds:DescribeDBClusters,
//!               rds:ListTagsForResource, cloudwatch:GetMetricData
//! ```

use std::collections::BTreeMap;

use aws_config::BehaviorVersion;
use dbmon_core::settings::{DiscoverySettings, DiscoveryTarget};

use super::rds::RdsDiscovery;

/// 세션 이름. CloudTrail 에서 "누가 맡았나" 로 보이는 값이다.
const SESSION_NAME: &str = "dbmon-discovery";

/// 탐색기 묶음 + 그것을 만든 설정의 지문.
pub struct DiscoveryFleet {
    pub sources: Vec<RdsDiscovery>,
    /// CloudWatch 를 부를 리전별 SDK 설정. 메트릭은 **대상 인스턴스의 리전에** 있다.
    pub sdk_by_region: BTreeMap<String, aws_config::SdkConfig>,
    fingerprint: String,
}

impl DiscoveryFleet {
    /// 이 묶음이 그 설정으로 만들어졌는가. 아니면 다시 만들어야 한다.
    pub fn matches(&self, settings: &DiscoverySettings, fallback_regions: &[String]) -> bool {
        self.fingerprint == fingerprint(settings, fallback_regions)
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

/// 설정 지문. **탐색 범위에 영향을 주는 값만** 넣는다 — 별칭(`label`)이 바뀌었다고
/// 클라이언트를 다시 만들 이유는 없다.
fn fingerprint(settings: &DiscoverySettings, fallback_regions: &[String]) -> String {
    let mut parts: Vec<String> = vec![format!("fallback={}", fallback_regions.join(","))];
    for t in settings.targets(fallback_regions) {
        parts.push(match &t.account {
            Some(a) => format!("{}:{}@{}", a.account_id, a.role_name, t.region),
            None => format!("self@{}", t.region),
        });
    }
    parts.join("|")
}

/// 설정을 보고 클라이언트를 만든다.
///
/// **한 대상이 실패해도 나머지를 만든다.** 계정 하나의 역할이 아직 없다고 전체 탐색이
/// 멈추면, 새 계정을 추가하는 순간 기존 계정의 수집이 함께 죽는다.
pub async fn build(
    settings: &DiscoverySettings,
    own_account: &str,
    fallback_regions: &[String],
) -> DiscoveryFleet {
    let targets = settings.targets(fallback_regions);
    let mut sources = Vec::with_capacity(targets.len());
    let mut sdk_by_region = BTreeMap::new();

    for target in &targets {
        let region = aws_config::Region::new(target.region.clone());
        let sdk = match target.role_arn() {
            None => {
                aws_config::defaults(BehaviorVersion::latest())
                    .region(region)
                    .load()
                    .await
            }
            Some(role_arn) => {
                // **역할을 맡는다.** 자격증명 제공자는 지연 평가되므로 여기서 STS 를
                // 부르지 않는다 — 실제 호출 시점에 맡고, 만료되면 SDK 가 갱신한다.
                let provider = aws_config::sts::AssumeRoleProvider::builder(role_arn.clone())
                    .session_name(SESSION_NAME)
                    // 문서의 신뢰 정책이 요구한다. 조건이 없는 역할에는 무시된다.
                    .external_id(super::ASSUME_ROLE_EXTERNAL_ID)
                    // STS 를 부를 리전. 대상 리전으로 두면 리전 엔드포인트를 쓴다
                    // (글로벌 엔드포인트보다 빠르고, 리전 차단 정책과도 맞다).
                    .region(region.clone())
                    .build()
                    .await;
                tracing::info!(role = %role_arn, region = %target.region, "크로스 계정 탐색 대상");
                aws_config::defaults(BehaviorVersion::latest())
                    .region(region)
                    .credentials_provider(provider)
                    .load()
                    .await
            }
        };
        sources.push(RdsDiscovery::new(
            aws_sdk_rds::Client::new(&sdk),
            target.region.clone(),
            account_of(target, own_account),
        ));
        // 같은 리전을 여러 계정이 공유하면 **계정별로 달라야 한다** — 하지만 지금
        // CloudWatch 는 리전 키로만 찾는다. 자기 계정 것을 우선 남긴다(덮어쓰지 않는다).
        sdk_by_region.entry(target.region.clone()).or_insert(sdk);
    }

    tracing::info!(
        targets = targets.len(),
        regions = ?sdk_by_region.keys().collect::<Vec<_>>(),
        "탐색 대상 준비"
    );
    DiscoveryFleet {
        sources,
        sdk_by_region,
        fingerprint: fingerprint(settings, fallback_regions),
    }
}

/// 이 대상의 계정 번호. 자기 계정이면 설정된 값이다.
fn account_of(target: &DiscoveryTarget, own_account: &str) -> String {
    match &target.account {
        Some(a) => a.account_id.clone(),
        None => own_account.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::settings::AccountTarget;

    /// 배포 설정의 기본 리전(테스트 고정값).
    fn seoul() -> Vec<String> {
        vec!["ap-northeast-2".to_string()]
    }

    /// 지문은 **범위가 바뀔 때만** 달라진다. 별칭·계정 순서에 반응하면 5분마다
    /// 클라이언트를 다시 만들고 STS 를 다시 부른다.
    #[test]
    fn the_fingerprint_tracks_scope_not_cosmetics() {
        let base = DiscoverySettings {
            regions: vec!["ap-northeast-2".into()],
            multi_account_enabled: true,
            accounts: vec![AccountTarget {
                account_id: "111111111111".into(),
                label: "payments".into(),
                ..Default::default()
            }],
        };
        let mut relabeled = base.clone();
        relabeled.accounts[0].label = "결제팀".into();
        assert_eq!(
            fingerprint(&base, &seoul()),
            fingerprint(&relabeled, &seoul()),
            "별칭만 바뀌었는데 클라이언트를 다시 만든다"
        );

        let mut wider = base.clone();
        wider.regions.push("us-east-1".into());
        assert_ne!(
            fingerprint(&base, &seoul()),
            fingerprint(&wider, &seoul()),
            "리전이 늘었는데 지문이 같다 — 새 리전을 영구히 탐색하지 않는다"
        );

        // 토글을 끄면 범위가 줄어든다 → 지문이 달라져야 한다.
        let off = DiscoverySettings {
            multi_account_enabled: false,
            ..base.clone()
        };
        assert_ne!(fingerprint(&base, &seoul()), fingerprint(&off, &seoul()));
    }

    /// 배포 설정의 리전이 바뀌면 지문도 바뀐다 — 운영 설정이 비어 있을 때 그게 탐색 범위다.
    #[test]
    fn the_fallback_regions_are_part_of_the_fingerprint() {
        let s = DiscoverySettings::default();
        assert_ne!(
            fingerprint(&s, &seoul()),
            fingerprint(&s, &["us-east-1".to_string()])
        );
    }

    #[test]
    fn own_account_targets_use_the_configured_account_id() {
        let t = DiscoveryTarget {
            account: None,
            region: "ap-northeast-2".into(),
        };
        assert_eq!(account_of(&t, "123456789012"), "123456789012");

        let cross = DiscoveryTarget {
            account: Some(AccountTarget {
                account_id: "111111111111".into(),
                ..Default::default()
            }),
            region: "us-east-1".into(),
        };
        assert_eq!(
            account_of(&cross, "123456789012"),
            "111111111111",
            "크로스 계정 인스턴스를 우리 계정 키로 저장하면 등록부가 섞인다"
        );
    }
}
