//! **실제 AWS 응답을 우리 코드로 통과시킨다.** 읽기 전용이다.
//!
//! # 이 테스트가 답하는 질문
//!
//! 나머지 통합 테스트는 로컬 도커(MySQL·DynamoDB Local)와 손으로 만든 픽스처를 쓴다.
//! 그러면 **로컬과 실제가 다른 자리**를 아무도 못 본다 — 이 프로젝트에서 그 부류가 두 번
//! 배포 차단이었다:
//!
//! - GSI 사영이 로컬 `ALL` / 프로덕션 `INCLUDE` 였고, 인덱스 항목을 완전한 레코드로
//!   역직렬화하는 코드가 통합 테스트 33개를 통과했다(교차 리뷰 22라운드).
//! - Aurora 버전 문자열 체계가 바뀌었다(`8.0.mysql_aurora.3.12.0` →
//!   `8.4.mysql_aurora.8.4.7`). 픽스처만 보면 파서가 새 형식을 못 읽는 것을 모른다.
//!
//! # 어떻게 도는가
//!
//! `AWS_PROFILE`(또는 표준 자격증명 체인)이 살아 있을 때만 돈다. 없으면 **건너뛴다** —
//! CI 는 AWS 자격증명 없이 돌아야 한다. 강제로 실행하려면 `DBMON_REQUIRE_AWS=1`.
//!
//! ```bash
//! AWS_PROFILE=<프로파일> cargo test -p dbmon --test it_aws_readonly -- --nocapture
//! ```
//!
//! # 무엇을 하지 않는가
//!
//! **쓰기·변경 API 를 부르지 않는다.** `Describe*`/`FilterLogEvents` 만 쓴다. 이 계정에는
//! 이 프로젝트와 무관한 실서비스 리소스가 함께 있으므로 그 원칙이 특히 중요하다.

use dbmon::aws::discovery::to_instance;
use dbmon::aws::rds::RdsDiscovery;
use dbmon_core::env::EnvMapping;
use dbmon_core::instance::{Engine, InstanceState};

const REGION: &str = "ap-northeast-2";

/// 이 프로젝트가 만든 시드 리소스의 접두어. **다른 리소스는 건드리지 않는다.**
const SEED_PREFIX: &str = "dbmon-seed-";

fn required() -> bool {
    std::env::var("DBMON_REQUIRE_AWS").as_deref() == Ok("1")
}

/// SDK 설정 + 계정 번호. 자격증명이 없거나 만료면 `None`.
///
/// 계정 번호는 **RDS ARN 에서 뽑는다** — STS 를 의존성에 추가하지 않기 위해서다
/// (`DescribeDBInstances` 는 어차피 이 테스트가 부른다). ARN 형식은
/// `arn:aws:rds:<region>:<account>:db:<id>` 다.
async fn aws_target() -> Option<(aws_config::SdkConfig, String)> {
    // **자격증명 힌트가 없으면 SDK 를 건드리지 않는다.**
    //
    // CI 에는 AWS 자격증명이 없다. 그냥 호출하면 SDK 가 IMDS(EC2 메타데이터)를 찾아
    // 타임아웃까지 기다린다 — 자격증명이 없다는 사실을 **환경에서 먼저** 판정하면
    // CI 가 몇 초를 낭비하지 않는다.
    let has_hint = [
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_ROLE_ARN",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    ]
    .iter()
    .any(|k| std::env::var(k).is_ok());
    if !has_hint {
        if required() {
            panic!("DBMON_REQUIRE_AWS=1 인데 자격증명 환경변수가 없다");
        }
        eprintln!("건너뜀: AWS 자격증명 환경변수가 없다 (AWS_PROFILE 등)");
        return None;
    }
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(REGION))
        .load()
        .await;
    let rds = aws_sdk_rds::Client::new(&cfg);
    // **자격증명을 실제로 한 번 확인한다.** 설정 조립만으로는 만료를 알 수 없다.
    let out = match rds.describe_db_instances().max_records(20).send().await {
        Ok(o) => o,
        Err(e) => {
            if required() {
                panic!("DBMON_REQUIRE_AWS=1 인데 RDS 를 읽을 수 없다: {e}");
            }
            eprintln!("건너뜀: AWS 자격증명이 없거나 권한이 없다 ({e})");
            return None;
        }
    };
    let account = out
        .db_instances()
        .iter()
        .filter_map(|d| d.db_instance_arn())
        .filter_map(|arn| arn.split(':').nth(4))
        .find(|a| a.len() == 12)
        .map(str::to_string);
    match account {
        Some(a) => Some((cfg, a)),
        None => {
            if required() {
                panic!("DBMON_REQUIRE_AWS=1 인데 RDS 인스턴스가 없어 계정을 알 수 없다");
            }
            eprintln!("건너뜀: 이 리전에 RDS 인스턴스가 없다");
            None
        }
    }
}

/// **실제 `DescribeDBInstances` 응답이 전부 매핑된다.**
///
/// 시드 리소스(`dbmon-seed-*`)만 본다. MySQL 계열은 하나도 `Unmappable` 이 되면 안 된다 —
/// 그건 그 인스턴스를 **영구히 수집하지 않는다**는 뜻이다.
#[tokio::test]
async fn every_seed_instance_maps_to_a_domain_instance() {
    let Some((cfg, account)) = aws_target().await else {
        return;
    };
    let disco = RdsDiscovery::new(aws_sdk_rds::Client::new(&cfg), REGION, account.clone());

    let page = disco.describe().await.expect("DescribeDBInstances");
    assert!(
        !page.truncated,
        "페이지가 끊겼다 — 부분 결과로 판정하면 안 된다"
    );

    let seeds: Vec<_> = page
        .instances
        .iter()
        .filter(|r| r.identifier.starts_with(SEED_PREFIX))
        .collect();
    assert!(
        !seeds.is_empty(),
        "시드 인스턴스가 없다 — infra/layers/60-seed 가 배포됐는지 확인한다"
    );

    let mapping = EnvMapping::default();
    let mut mysqls = 0;
    for raw in &seeds {
        match to_instance(raw, &account, &mapping, 0) {
            Ok(inst) => {
                if inst.engine == Engine::Mysql || inst.engine == Engine::AuroraMysql {
                    mysqls += 1;
                }
                // 상태 판정이 `available` 을 수집 대상으로 본다.
                if raw.status == "available" {
                    assert_ne!(
                        inst.state,
                        InstanceState::Unsupported,
                        "{}: available 인데 미지원으로 판정됐다 (버전 {})",
                        raw.identifier,
                        raw.engine_version
                    );
                }
                println!(
                    "  {} → {} {} / {:?}",
                    raw.identifier, raw.engine, raw.engine_version, inst.state
                );
            }
            Err(e) => panic!(
                "{} 를 매핑할 수 없다: {e} (engine={} version={})",
                raw.identifier, raw.engine, raw.engine_version
            ),
        }
    }
    assert!(mysqls >= 1, "MySQL 계열 시드가 하나도 없다");
}

/// **Aurora 의 두 버전 체계를 모두 읽는다.**
///
/// `8.0.mysql_aurora.3.12.0`(구 체계)과 `8.4.mysql_aurora.8.4.7`(신 체계)가 같은 계정에
/// 함께 있다. 파서가 한쪽만 읽으면 나머지는 **수집되지 않는다** — 그리고 그 사실은
/// 픽스처만 보는 테스트로는 드러나지 않는다.
#[tokio::test]
async fn both_aurora_version_schemes_parse() {
    let Some((cfg, account)) = aws_target().await else {
        return;
    };
    let disco = RdsDiscovery::new(aws_sdk_rds::Client::new(&cfg), REGION, account.clone());
    let page = disco.describe().await.expect("DescribeDBInstances");

    let mapping = EnvMapping::default();
    let mut schemes = std::collections::BTreeSet::new();
    for raw in page
        .instances
        .iter()
        .filter(|r| r.identifier.starts_with(SEED_PREFIX) && r.engine == "aurora-mysql")
    {
        let inst = to_instance(raw, &account, &mapping, 0)
            .unwrap_or_else(|e| panic!("{}: {e} ({})", raw.identifier, raw.engine_version));
        // 신 체계는 `8.4.` 로, 구 체계는 `8.0.` 으로 시작한다.
        schemes.insert(
            raw.engine_version
                .split('.')
                .take(2)
                .collect::<Vec<_>>()
                .join("."),
        );
        println!(
            "  {} {} → community={:?} aurora={:?}",
            raw.identifier,
            raw.engine_version,
            inst.engine_version.community,
            inst.engine_version.aurora
        );
    }
    assert!(
        schemes.len() >= 2,
        "Aurora 버전 체계가 한 종류뿐이다 — 두 체계를 함께 검증할 수 없다: {schemes:?}"
    );
}

/// **클러스터 멤버는 클러스터 id 를 갖는다.**
///
/// Aurora 멤버 이름은 클러스터 이름과 무관할 수 있어서, 이름 규칙으로 유추하면 틀린다.
/// `cluster_identifier` 가 비면 라이터 판정과 환경 분류가 함께 어긋난다.
#[tokio::test]
async fn aurora_members_carry_their_cluster_id() {
    let Some((cfg, account)) = aws_target().await else {
        return;
    };
    let disco = RdsDiscovery::new(aws_sdk_rds::Client::new(&cfg), REGION, account.clone());
    let page = disco.describe().await.expect("DescribeDBInstances");

    let mapping = EnvMapping::default();
    let mut checked = 0;
    for raw in page
        .instances
        .iter()
        .filter(|r| r.identifier.starts_with(SEED_PREFIX) && r.engine == "aurora-mysql")
    {
        assert!(
            raw.cluster_identifier.is_some(),
            "{}: Aurora 인데 클러스터 id 가 없다",
            raw.identifier
        );
        let inst = to_instance(raw, &account, &mapping, 0).expect("매핑");
        assert!(
            inst.cluster_id.is_some(),
            "{}: 클러스터 id 를 잃었다",
            raw.identifier
        );
        checked += 1;
    }
    assert!(checked >= 1, "Aurora 시드 멤버가 없다");
}
