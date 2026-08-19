//! 탐색 필터 — **T-37 프로덕션 격리** (M2-5).
//!
//! # 이 파일이 막는 실패
//!
//! `rds:DescribeDBInstances` 는 리소스 수준 권한을 지원하지 않으므로 IAM 이
//! `Resource: "*"` 다([19 §B](../../../../docs/19-m1-findings.md)). 즉 dev 배포도
//! **계정 안의 prd 인스턴스를 다 본다.** 이 계정에는 프로덕션 워크로드가 함께 있다
//! ([18 §1](../../../../docs/18-dev-environment.md)).
//!
//! 그래서 필터가 최후 방어선이다. 설정 검증이 `deployment_env != prd` 에서
//! `allowed_vpc_ids` 를 필수로 만들고(`config.rs`), 이 파일이 그 필터를 적용한다.
//!
//! # 순수 함수로 두는 이유
//!
//! AWS 호출은 로컬에서 검증할 수 없다(SSO 만료 시 아예 못 돈다). 그런데 **위험한
//! 부분은 호출이 아니라 판정**이다 — prd 인스턴스를 하나라도 통과시키면 그걸로 끝이다.
//! 판정을 순수 함수로 분리하면 AWS 없이 전수 검증할 수 있다.

/// 탐색 후보의 최소 정보. SDK 타입에 의존하지 않는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub identifier: String,
    pub vpc_id: Option<String>,
    pub tags: std::collections::BTreeMap<String, String>,
    /// Aurora 클러스터 식별자.
    ///
    /// **이름 거부는 이것도 봐야 한다.** Aurora 멤버 인스턴스 이름은 자유롭게 정할 수
    /// 있어서 `orders-prd-cluster` 의 멤버가 `orders-writer` 일 수 있다 — 인스턴스
    /// 이름만 보면 prd 라는 증거를 놓친다(2차 리뷰가 지적).
    pub cluster_identifier: Option<String>,
}

/// 필터 판정 결과. **왜 거부됐는지 남긴다** — 조용히 거부하면 "왜 안 보이나" 를
/// 디버깅할 수 없고, 조용히 통과시키면 prd 를 수집한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    /// `allowed_vpc_ids` 에 없다. **VPC 를 모르는 인스턴스도 여기로 온다.**
    VpcNotAllowed {
        got: Option<String>,
    },
    /// `required_tags` 를 만족하지 않는다.
    MissingTag {
        want: String,
    },
    /// 이름에 거부 문자열이 있다. `field` 는 `identifier` 또는 `cluster`.
    NameDenied {
        matched: String,
        field: &'static str,
    },
    /// **태그가 프로덕션이라고 말한다.** 비프로덕션 배포에서만 적용된다 (T-37).
    ProductionTag {
        key: String,
        value: String,
    },
}

impl Verdict {
    pub fn is_accept(&self) -> bool {
        matches!(self, Self::Accept)
    }

    pub fn reason(&self) -> String {
        match self {
            Self::Accept => "accept".into(),
            Self::VpcNotAllowed { got } => format!("vpc_not_allowed({got:?})"),
            Self::MissingTag { want } => format!("missing_tag({want})"),
            Self::NameDenied { matched, field } => format!("name_denied({field}:{matched})"),
            Self::ProductionTag { key, value } => format!("production_tag({key}={value})"),
        }
    }
}

/// 필터 설정. `DiscoveryConfig` 에서 만든다.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub allowed_vpc_ids: Vec<String>,
    /// `키=값` 형태.
    pub required_tags: Vec<String>,
    pub denied_name_substrings: Vec<String>,
    /// **환경 태그가 prd 인 인스턴스를 거부한다.** 비프로덕션 배포에서 켠다.
    ///
    /// 이름 거부만으로는 부족하다 — `Environment=production` 태그가 붙었지만 이름에
    /// prd 문자열이 없는 인스턴스가 허용 VPC 안에 있으면 통과한다(2차 리뷰가 지적).
    /// `EnvMapping` 이 이미 그 판정을 하고 있는데 필터가 쓰지 않고 있었다.
    pub reject_production_tags: bool,
}

impl Filter {
    /// **설정에서 만드는 유일한 경로.**
    ///
    /// 호출부가 각자 필드를 옮기면 한 곳이 빠져도 컴파일된다 — 실제로
    /// `denied_name_substrings` 기본값이 그렇게 한 번 죽었다(호출부 없음).
    /// 여기 하나만 두면 테스트가 프로덕션과 같은 경로를 지난다.
    pub fn from_config(cfg: &crate::config::DiscoveryConfig) -> Self {
        Self {
            allowed_vpc_ids: cfg.allowed_vpc_ids.clone(),
            required_tags: cfg.required_tags.clone(),
            denied_name_substrings: cfg.denied_name_substrings.clone(),
            reject_production_tags: cfg.reject_production_tags,
        }
    }

    /// 후보를 판정한다.
    ///
    /// **순서가 의미를 갖는다.** 거부 규칙(`denied_name_substrings`)을 마지막에 두면
    /// 허용 규칙이 통과시킨 것도 거부한다 — 그게 의도다("화이트리스트가 통과시켜도 거부").
    pub fn judge(&self, c: &Candidate) -> Verdict {
        // ① VPC. 설정이 비어 있으면 이 조건을 적용하지 않는다 —
        //    `deployment_env == prd` 만 그 상태로 기동할 수 있다(`config.rs` 검증).
        if !self.allowed_vpc_ids.is_empty() {
            // **VPC 를 모르는 인스턴스는 거부한다.** `None` 을 통과시키면 필터에 구멍이 난다.
            let ok = c
                .vpc_id
                .as_ref()
                .is_some_and(|v| self.allowed_vpc_ids.iter().any(|a| a == v));
            if !ok {
                return Verdict::VpcNotAllowed {
                    got: c.vpc_id.clone(),
                };
            }
        }

        // ② 필수 태그. **AND 조건**이다.
        //
        // 키 조회는 **대소문자를 무시한다.** `Environment` 와 `environment` 를 다르게
        // 보면 `required_tags` 오타 하나로 전 인스턴스가 거부되고, 거부는 미발견으로
        // 이어져 등록부 전체가 삭제 판정된다(2차 리뷰가 지적). 이 크레이트의 다른
        // 태그 조회(`tag_disables_collection`, `EnvMapping::classify`)도 무시한다.
        for want in &self.required_tags {
            let (k, v) = match want.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                // 값 없이 키만 요구할 수도 있다.
                None => (want.trim(), None),
            };
            let matched = match (lookup_tag(&c.tags, k), v) {
                (Some(actual), Some(expected)) => actual == expected,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if !matched {
                return Verdict::MissingTag { want: want.clone() };
            }
        }

        // ③ **태그가 프로덕션이라고 말하면 거부한다.**
        //
        // `Env::Prd` 만 본다. `Env::Unknown` 까지 거부하면 태그를 안 붙인 dev
        // 인스턴스가 전부 사라진다 — 그건 T-37 이 막으려는 실패가 아니다.
        if self.reject_production_tags {
            let mapping = dbmon_core::env::EnvMapping::default();
            if mapping.classify(&c.tags) == dbmon_core::env::Env::Prd {
                let (key, value) = production_tag_evidence(&c.tags, &mapping);
                return Verdict::ProductionTag { key, value };
            }
        }

        // ④ 이름 거부 — 마지막이다. 위를 통과했어도 거부한다.
        //
        // **클러스터 이름도 본다.** Aurora 멤버 이름은 자유라 `orders-prd-cluster` 의
        // 멤버가 `orders-writer` 일 수 있다.
        for (field, name) in [
            ("identifier", Some(&c.identifier)),
            ("cluster", c.cluster_identifier.as_ref()),
        ] {
            let Some(name) = name else { continue };
            let lower = name.to_ascii_lowercase();
            for deny in &self.denied_name_substrings {
                let d = deny.to_ascii_lowercase();
                if !d.is_empty() && lower.contains(&d) {
                    return Verdict::NameDenied {
                        matched: deny.clone(),
                        field,
                    };
                }
            }
        }

        Verdict::Accept
    }
}

/// 태그를 **대소문자 무시로** 찾는다.
fn lookup_tag<'a>(
    tags: &'a std::collections::BTreeMap<String, String>,
    key: &str,
) -> Option<&'a String> {
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// 어느 태그가 prd 판정을 냈는지 찾아 사유에 담는다.
///
/// 판정만 남기고 근거를 버리면 "왜 안 보이나" 를 추적할 수 없다.
fn production_tag_evidence(
    tags: &std::collections::BTreeMap<String, String>,
    mapping: &dbmon_core::env::EnvMapping,
) -> (String, String) {
    for key in &mapping.keys {
        if let Some(v) = lookup_tag(tags, key)
            && mapping.values.get(&v.trim().to_lowercase()) == Some(&dbmon_core::env::Env::Prd)
        {
            return (key.clone(), v.clone());
        }
    }
    ("?".into(), "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, vpc: Option<&str>, tags: &[(&str, &str)]) -> Candidate {
        Candidate {
            identifier: id.into(),
            vpc_id: vpc.map(str::to_string),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cluster_identifier: None,
        }
    }

    fn dev_filter() -> Filter {
        Filter {
            allowed_vpc_ids: vec!["vpc-dev".into()],
            required_tags: vec![],
            denied_name_substrings: vec!["prd".into(), "prod".into()],
            ..Default::default()
        }
    }

    /// **T-37 의 핵심.** dev 필터가 prd VPC 인스턴스를 통과시키면 안 된다.
    #[test]
    fn dev_filter_rejects_instances_in_other_vpcs() {
        let f = dev_filter();
        let v = f.judge(&cand("orders-01", Some("vpc-prd"), &[]));
        assert!(!v.is_accept(), "prd VPC 인스턴스를 통과시켰다: {v:?}");
        assert!(matches!(v, Verdict::VpcNotAllowed { .. }));
    }

    /// **VPC 를 모르는 인스턴스는 거부한다.** `None` 을 통과시키면 필터에 구멍이 난다 —
    /// `DescribeDBInstances` 응답에 `DBSubnetGroup` 이 없는 경우가 실재한다.
    #[test]
    fn unknown_vpc_is_rejected_not_allowed() {
        let f = dev_filter();
        let v = f.judge(&cand("orders-01", None, &[]));
        assert!(
            !v.is_accept(),
            "VPC 를 모르는 인스턴스를 통과시켰다 — 필터에 구멍이 난다"
        );
    }

    /// 이름 거부는 **허용 규칙을 통과한 뒤에도** 적용된다.
    #[test]
    fn name_deny_overrides_vpc_allow() {
        let f = dev_filter();
        // dev VPC 안에 있지만 이름이 prd 다 — 잘못 배치된 인스턴스.
        let v = f.judge(&cand("orders-prd-01", Some("vpc-dev"), &[]));
        assert!(
            matches!(v, Verdict::NameDenied { .. }),
            "허용 VPC 라고 이름 거부를 건너뛰었다: {v:?}"
        );
    }

    /// 대소문자를 구분하지 않아야 한다 — `Orders-PRD-01` 도 거부한다.
    #[test]
    fn name_deny_is_case_insensitive() {
        let f = dev_filter();
        for name in ["orders-PRD-01", "Orders-Prod-02", "ORDERS-PRD"] {
            let v = f.judge(&cand(name, Some("vpc-dev"), &[]));
            assert!(!v.is_accept(), "{name} 을 통과시켰다");
        }
    }

    #[test]
    fn accepts_only_when_everything_passes() {
        let f = dev_filter();
        let v = f.judge(&cand("orders-dev-01", Some("vpc-dev"), &[]));
        assert_eq!(v, Verdict::Accept);
    }

    /// 필수 태그는 **AND** 다.
    #[test]
    fn required_tags_are_all_needed() {
        let f = Filter {
            allowed_vpc_ids: vec!["vpc-dev".into()],
            required_tags: vec!["Team=db".into(), "Monitored=true".into()],
            denied_name_substrings: vec![],
            ..Default::default()
        };
        // 하나만 있으면 거부.
        let v = f.judge(&cand("x", Some("vpc-dev"), &[("Team", "db")]));
        assert!(matches!(v, Verdict::MissingTag { .. }), "{v:?}");
        // 둘 다 있으면 통과.
        let v = f.judge(&cand(
            "x",
            Some("vpc-dev"),
            &[("Team", "db"), ("Monitored", "true")],
        ));
        assert_eq!(v, Verdict::Accept);
        // 값이 다르면 거부.
        let v = f.judge(&cand(
            "x",
            Some("vpc-dev"),
            &[("Team", "web"), ("Monitored", "true")],
        ));
        assert!(matches!(v, Verdict::MissingTag { .. }), "{v:?}");
    }

    /// 키만 요구할 수도 있다 (`값` 없이).
    #[test]
    fn tag_key_only_requirement() {
        let f = Filter {
            allowed_vpc_ids: vec![],
            required_tags: vec!["Monitored".into()],
            denied_name_substrings: vec![],
            ..Default::default()
        };
        assert_eq!(
            f.judge(&cand("x", None, &[("Monitored", "anything")])),
            Verdict::Accept
        );
        assert!(!f.judge(&cand("x", None, &[])).is_accept());
    }

    /// **`allowed_vpc_ids` 가 비면 VPC 조건을 적용하지 않는다.**
    ///
    /// 그 상태로 기동할 수 있는 것은 `deployment_env == prd` 뿐이다 —
    /// `config.rs` 의 T-37 게이트가 그걸 강제한다. 이 테스트는 두 곳의 계약을 고정한다.
    #[test]
    fn empty_vpc_list_means_no_vpc_condition() {
        let f = Filter::default();
        assert_eq!(f.judge(&cand("anything", None, &[])), Verdict::Accept);
        assert_eq!(
            f.judge(&cand("anything", Some("vpc-whatever"), &[])),
            Verdict::Accept
        );
    }

    /// **태그가 프로덕션이라고 말하면 거부한다** (2차 리뷰 F1).
    ///
    /// 이름에 prd 문자열이 없고 허용 VPC 안에 있어도, `Environment=production` 태그가
    /// 붙었으면 프로덕션이다. `EnvMapping` 이 이미 그 판정을 하는데 필터가 쓰지
    /// 않고 있었다 — 판정은 있는데 호출부가 없던 부류다.
    #[test]
    fn production_env_tag_is_rejected_even_with_an_innocuous_name() {
        let f = Filter {
            allowed_vpc_ids: vec!["vpc-dev".into()],
            reject_production_tags: true,
            ..Default::default()
        };
        // 이름은 무해하고 VPC 는 허용 목록에 있다.
        for (k, v) in [
            ("Environment", "production"),
            ("env", "prd"),
            ("ENV", " PROD "),
            ("stage", "live"),
            ("tier", "real"),
        ] {
            let c = cand("orders-01", Some("vpc-dev"), &[(k, v)]);
            let verdict = f.judge(&c);
            assert!(
                matches!(verdict, Verdict::ProductionTag { .. }),
                "{k}={v} 인 인스턴스를 통과시켰다: {verdict:?}"
            );
            // 사유에 근거 태그가 담겨야 한다.
            assert!(
                verdict.reason().contains(&v.trim().to_string()),
                "{verdict:?}"
            );
        }
    }

    /// **태그가 없는 것은 거부하지 않는다.**
    ///
    /// `Env::Unknown` 까지 거부하면 태그를 안 붙인 dev 인스턴스가 전부 사라진다 —
    /// 그건 T-37 이 막으려는 실패가 아니다. `Unknown` 의 보수적 취급은 리터럴 정책
    /// 쪽에서 한다.
    #[test]
    fn untagged_instances_are_not_rejected_as_production() {
        let f = Filter {
            allowed_vpc_ids: vec!["vpc-dev".into()],
            reject_production_tags: true,
            ..Default::default()
        };
        assert_eq!(
            f.judge(&cand("orders-01", Some("vpc-dev"), &[])),
            Verdict::Accept
        );
        assert_eq!(
            f.judge(&cand("orders-01", Some("vpc-dev"), &[("env", "dev")])),
            Verdict::Accept
        );
        // 알 수 없는 값도 통과 — 거부는 명시적 prd 에만.
        assert_eq!(
            f.judge(&cand("orders-01", Some("vpc-dev"), &[("env", "권한없음")])),
            Verdict::Accept
        );
    }

    /// prd 배포에서는 이 규칙을 켜지 않는다 — 자기 인스턴스를 전부 거부한다.
    #[test]
    fn production_deployment_does_not_reject_production_tags() {
        let f = Filter {
            reject_production_tags: false,
            ..Default::default()
        };
        assert_eq!(
            f.judge(&cand("orders-prd", None, &[("env", "prd")])),
            Verdict::Accept
        );
    }

    /// **클러스터 이름도 이름 거부 대상이다** (2차 리뷰 F2).
    ///
    /// Aurora 멤버 인스턴스 이름은 자유라 `orders-prd-cluster` 의 멤버가
    /// `orders-writer` 일 수 있다. 인스턴스 이름만 보면 prd 라는 증거를 놓친다.
    #[test]
    fn cluster_name_is_checked_for_denied_substrings() {
        let f = dev_filter();
        let mut c = cand("orders-writer", Some("vpc-dev"), &[]);
        c.cluster_identifier = Some("orders-prd-cluster".into());

        let verdict = f.judge(&c);
        assert!(
            matches!(
                verdict,
                Verdict::NameDenied {
                    field: "cluster",
                    ..
                }
            ),
            "prd 클러스터의 멤버를 통과시켰다: {verdict:?}"
        );
        // 사유에 어느 필드였는지 남아야 한다.
        assert!(verdict.reason().contains("cluster"), "{verdict:?}");
    }

    /// 클러스터가 없으면(RDS MySQL) 인스턴스 이름만 본다.
    #[test]
    fn standalone_instances_are_unaffected_by_the_cluster_check() {
        let f = dev_filter();
        assert_eq!(
            f.judge(&cand("orders-dev-01", Some("vpc-dev"), &[])),
            Verdict::Accept
        );
    }

    /// **필수 태그 키 조회는 대소문자를 무시해야 한다** (2차 리뷰 F3).
    ///
    /// 구분하면 `required_tags = ["environment=dev"]` + 실제 태그 `Environment` 조합에서
    /// **전 인스턴스가 거부되고**, 거부는 미발견으로 이어져 등록부 전체가 삭제 판정된다.
    /// 오타 하나가 전면 삭제가 되는 실패 방향이다.
    #[test]
    fn required_tag_keys_are_matched_case_insensitively() {
        let f = Filter {
            required_tags: vec!["environment=dev".into()],
            ..Default::default()
        };
        for key in ["Environment", "ENVIRONMENT", "environment"] {
            assert_eq!(
                f.judge(&cand("x", None, &[(key, "dev")])),
                Verdict::Accept,
                "태그 키 {key} 를 찾지 못했다 — 오타 하나가 전면 거부가 된다"
            );
        }
        // 값은 정확 일치를 유지한다 — 값까지 느슨하면 prd 를 dev 로 볼 수 있다.
        assert!(
            !f.judge(&cand("x", None, &[("Environment", "DEV")]))
                .is_accept()
        );
    }

    /// 거부 사유가 로그에 남아야 한다 — 조용히 거부하면 "왜 안 보이나" 를 못 찾는다.
    #[test]
    fn every_rejection_carries_a_reason() {
        let f = dev_filter();
        for c in [
            cand("x", Some("vpc-other"), &[]),
            cand("orders-prd", Some("vpc-dev"), &[]),
        ] {
            let v = f.judge(&c);
            assert!(!v.is_accept());
            assert!(!v.reason().is_empty(), "사유가 비었다: {v:?}");
            assert_ne!(v.reason(), "accept");
        }
    }
}
