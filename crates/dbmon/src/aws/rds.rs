//! RDS 탐색 어댑터 — `DescribeDBInstances` (M2-5, FR-DSC-01).
//!
//! # 이 파일은 판정하지 않는다
//!
//! SDK 응답을 [`RawDbInstance`] 로 옮기는 일만 한다. "수집해도 되는가"([`filter`])와
//! "어떤 상태인가"([`discovery`])는 순수 함수가 판정하고, 그쪽은 AWS 없이 전수 검증한다.
//!
//! 이 분리가 없으면 위험한 판정을 SSO 세션이 살아 있을 때만 검증할 수 있다.
//!
//! # 페이지네이션을 직접 돌지 않는다
//!
//! `into_paginator()` 를 쓴다. 손으로 `marker` 를 돌리면 **마지막 페이지에서 멈추지
//! 않는 실수**가 나오고, 그건 500대 계정에서 무한 루프가 된다.
//!
//! [`filter`]: super::filter
//! [`discovery`]: super::discovery

use std::collections::BTreeMap;

use aws_sdk_rds::Client;
use aws_sdk_rds::types::DbInstance;
use dbmon_core::error::{DomainError, Result};

use super::discovery::RawDbInstance;

/// 탐색 결과 한 리전분.
#[derive(Debug, Default)]
pub struct DiscoveryPage {
    pub instances: Vec<RawDbInstance>,
    /// 페이지네이션 중 끊긴 경우. **부분 결과를 성공으로 보고하지 않기 위해** 남긴다 —
    /// 부분 결과를 전체로 취급하면 FR-DSC-07 이 나머지를 "사라졌다" 로 판정한다.
    pub truncated: bool,
}

pub struct RdsDiscovery {
    client: Client,
    region: String,
}

impl RdsDiscovery {
    pub fn new(client: Client, region: impl Into<String>) -> Self {
        Self {
            client,
            region: region.into(),
        }
    }

    /// 이 리전의 DB 인스턴스를 모두 나열한다.
    ///
    /// **엔진 필터를 API 에 걸지 않는다.** `--filters engine=mysql` 을 쓰면 새 엔진
    /// 이름(예: 향후 `mysql-*` 변형)이 조용히 빠진다. 전부 받아서
    /// [`super::discovery::to_instance`] 가 `NotMysql` 로 분류하게 둔다 — 그쪽은
    /// 사유를 남기므로 "왜 안 보이나" 를 추적할 수 있다.
    pub async fn describe(&self) -> Result<DiscoveryPage> {
        let mut page = DiscoveryPage::default();
        let mut stream = self
            .client
            .describe_db_instances()
            .into_paginator()
            .items()
            .send();

        while let Some(next) = stream.next().await {
            match next {
                Ok(db) => page.instances.push(self.to_raw(&db)),
                Err(e) => {
                    // **여기서 `Err` 를 반환하지 않는다.** 3페이지 중 2페이지를 받았다면
                    // 그 결과는 쓸 수 있다 — 단, `truncated` 로 표시해 재조정이
                    // 나머지를 "사라졌다" 로 판정하지 않게 한다.
                    tracing::warn!(
                        region = %self.region,
                        error = %crate::telemetry::Scrubbed(&e),
                        "DescribeDBInstances 페이지 실패 — 부분 결과로 표시한다"
                    );
                    page.truncated = true;
                    break;
                }
            }
        }

        if page.instances.is_empty() && page.truncated {
            // 아무것도 못 받았으면 실패다. 빈 성공으로 보고하면 전체가 삭제 판정된다.
            return Err(DomainError::Unavailable {
                dependency: "rds",
                reason: format!("{}: DescribeDBInstances 실패", self.region),
            });
        }
        Ok(page)
    }

    /// SDK 타입 → 원시 필드. **여기서 판정하지 않는다.**
    fn to_raw(&self, db: &DbInstance) -> RawDbInstance {
        RawDbInstance {
            identifier: db.db_instance_identifier().unwrap_or_default().to_string(),
            dbi_resource_id: db.dbi_resource_id().unwrap_or_default().to_string(),
            engine: db.engine().unwrap_or_default().to_string(),
            engine_version: db.engine_version().unwrap_or_default().to_string(),
            status: db.db_instance_status().unwrap_or_default().to_string(),
            endpoint_address: db.endpoint().and_then(|e| e.address()).map(str::to_string),
            // SDK 는 i32 다. 포트 범위를 벗어나면 버린다 — 잘라 쓰면 엉뚱한 포트로 붙는다.
            endpoint_port: db
                .endpoint()
                .and_then(|e| e.port())
                .and_then(|p| u16::try_from(p).ok()),
            // **VPC 를 여기서 추측하지 않는다.** 없으면 `None` 이고, 필터가 거부한다.
            vpc_id: db
                .db_subnet_group()
                .and_then(|g| g.vpc_id())
                .map(str::to_string),
            availability_zone: db.availability_zone().map(str::to_string),
            instance_class: db.db_instance_class().map(str::to_string),
            cluster_identifier: db.db_cluster_identifier().map(str::to_string),
            // Aurora 라이터 여부는 클러스터 응답에만 있다. 여기서는 알 수 없다(M2-5b).
            is_cluster_writer: false,
            iam_auth_enabled: db.iam_database_authentication_enabled().unwrap_or(false),
            cert_valid_till_ms: db
                .certificate_details()
                .and_then(|c| c.valid_till())
                .map(|t| t.to_millis().unwrap_or(0))
                .filter(|ms| *ms > 0),
            tags: tag_map(db),
            region: self.region.clone(),
        }
    }
}

/// `TagList` → 맵. **`DescribeDBInstances` 응답에 태그가 포함된다** —
/// `ListTagsForResource` 를 인스턴스마다 부르면 500대에서 500회 왕복이다.
fn tag_map(db: &DbInstance) -> BTreeMap<String, String> {
    db.tag_list()
        .iter()
        .filter_map(|t| Some((t.key()?.to_string(), t.value().unwrap_or("").to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_rds::types::{CertificateDetails, DbSubnetGroup, Endpoint, Tag};

    fn client() -> Client {
        // 실제 호출은 하지 않는다. `to_raw` 는 순수 변환이다.
        let cfg = aws_sdk_rds::Config::builder()
            .behavior_version(aws_sdk_rds::config::BehaviorVersion::latest())
            .region(aws_sdk_rds::config::Region::new("ap-northeast-2"))
            .build();
        Client::from_conf(cfg)
    }

    fn disco() -> RdsDiscovery {
        RdsDiscovery::new(client(), "ap-northeast-2")
    }

    /// SDK 응답의 필드가 **하나도 빠지지 않고** 옮겨져야 한다.
    /// 빠지면 조용히 정보를 잃는다 — 예를 들어 `vpc_id` 가 빠지면 필터가 전부 거부한다.
    #[test]
    fn carries_every_field_we_use() {
        let db = DbInstance::builder()
            .db_instance_identifier("orders-01")
            .dbi_resource_id("db-ABCDEFGHIJKLMNOP")
            .engine("mysql")
            .engine_version("8.4.6")
            .db_instance_status("available")
            .endpoint(
                Endpoint::builder()
                    .address("orders-01.abc.ap-northeast-2.rds.amazonaws.com")
                    .port(3306)
                    .build(),
            )
            .db_subnet_group(DbSubnetGroup::builder().vpc_id("vpc-abc123").build())
            .availability_zone("ap-northeast-2a")
            .db_instance_class("db.r6g.large")
            .iam_database_authentication_enabled(true)
            .tag_list(Tag::builder().key("env").value("prd").build())
            .build();

        let raw = disco().to_raw(&db);
        assert_eq!(raw.identifier, "orders-01");
        assert_eq!(raw.dbi_resource_id, "db-ABCDEFGHIJKLMNOP");
        assert_eq!(raw.engine, "mysql");
        assert_eq!(raw.engine_version, "8.4.6");
        assert_eq!(
            raw.endpoint_address.as_deref(),
            Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com")
        );
        assert_eq!(raw.endpoint_port, Some(3306));
        assert_eq!(raw.vpc_id.as_deref(), Some("vpc-abc123"));
        assert_eq!(raw.availability_zone.as_deref(), Some("ap-northeast-2a"));
        assert_eq!(raw.instance_class.as_deref(), Some("db.r6g.large"));
        assert!(raw.iam_auth_enabled);
        assert_eq!(raw.tags.get("env").map(String::as_str), Some("prd"));
        assert_eq!(raw.region, "ap-northeast-2");
    }

    /// **`DBSubnetGroup` 이 없으면 `vpc_id` 는 `None` 이어야 한다.**
    ///
    /// 여기서 빈 문자열("")로 채우면 필터의 "미지 VPC 거부" 가 무력해진다 —
    /// `""` 는 `Some` 이므로 `is_some_and` 를 통과할 뻔한다.
    #[test]
    fn missing_subnet_group_yields_none_not_empty_string() {
        let db = DbInstance::builder()
            .db_instance_identifier("orders-01")
            .engine("mysql")
            .build();
        let raw = disco().to_raw(&db);
        assert_eq!(
            raw.vpc_id, None,
            "VPC 를 빈 문자열로 채우면 필터의 미지 VPC 거부가 무력해진다"
        );
        assert_eq!(raw.endpoint_address, None);
        assert_eq!(raw.endpoint_port, None);
    }

    /// 포트가 `u16` 범위를 벗어나면 **버린다.** 잘라 쓰면 엉뚱한 포트로 붙는다.
    #[test]
    fn out_of_range_port_is_dropped_not_truncated() {
        let db = DbInstance::builder()
            .db_instance_identifier("x")
            .endpoint(Endpoint::builder().address("h").port(70_000).build())
            .build();
        assert_eq!(disco().to_raw(&db).endpoint_port, None);
    }

    /// 태그가 여러 개면 전부 옮긴다. 값 없는 태그도 키는 남긴다(필수 태그 검사용).
    #[test]
    fn maps_all_tags_including_valueless() {
        let db = DbInstance::builder()
            .db_instance_identifier("x")
            .tag_list(Tag::builder().key("env").value("prd").build())
            .tag_list(Tag::builder().key("Monitored").build())
            .build();
        let raw = disco().to_raw(&db);
        assert_eq!(raw.tags.len(), 2);
        assert_eq!(raw.tags.get("Monitored").map(String::as_str), Some(""));
    }

    /// 인증서 만료가 있으면 밀리초로 옮긴다 (FR-DSC-13).
    #[test]
    fn carries_certificate_expiry() {
        let db = DbInstance::builder()
            .db_instance_identifier("x")
            .certificate_details(
                CertificateDetails::builder()
                    .valid_till(aws_sdk_rds::primitives::DateTime::from_millis(
                        1_800_000_000_000,
                    ))
                    .build(),
            )
            .build();
        assert_eq!(
            disco().to_raw(&db).cert_valid_till_ms,
            Some(1_800_000_000_000)
        );

        // 없으면 `None` — 0 으로 채우면 "1970년 만료" 로 보여 경보가 폭발한다.
        let bare = DbInstance::builder().db_instance_identifier("x").build();
        assert_eq!(disco().to_raw(&bare).cert_valid_till_ms, None);
    }

    /// 순수 판정과 이어 붙였을 때 **끝까지 동작해야 한다.**
    /// 어댑터와 판정이 각자 맞아도 이어 붙인 결과가 틀릴 수 있다.
    #[test]
    fn adapter_output_feeds_the_pure_judgment() {
        use crate::aws::discovery::to_instance;
        use crate::aws::filter::Filter;
        use dbmon_core::env::EnvMapping;

        let db = DbInstance::builder()
            .db_instance_identifier("orders-dev-01")
            .dbi_resource_id("db-Z")
            .engine("mysql")
            .engine_version("8.4.6")
            .db_subnet_group(DbSubnetGroup::builder().vpc_id("vpc-dev").build())
            .endpoint(Endpoint::builder().address("h").port(3306).build())
            .tag_list(Tag::builder().key("env").value("dev").build())
            .build();
        let raw = disco().to_raw(&db);

        let f = Filter {
            allowed_vpc_ids: vec!["vpc-dev".into()],
            required_tags: vec![],
            denied_name_substrings: vec!["prd".into()],
        };
        assert!(f.judge(&raw.candidate()).is_accept(), "필터가 거부했다");

        let inst = to_instance(
            &raw,
            "123456789012",
            &EnvMapping::default(),
            1_755_500_400_000,
        )
        .expect("매핑");
        assert_eq!(inst.env.effective, dbmon_core::env::Env::Dev);
    }
}
