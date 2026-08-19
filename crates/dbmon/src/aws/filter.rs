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
    /// 이름에 거부 문자열이 있다.
    NameDenied {
        matched: String,
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
            Self::NameDenied { matched } => format!("name_denied({matched})"),
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
}

impl Filter {
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
        for want in &self.required_tags {
            let (k, v) = match want.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                // 값 없이 키만 요구할 수도 있다.
                None => (want.trim(), None),
            };
            let matched = match (c.tags.get(k), v) {
                (Some(actual), Some(expected)) => actual == expected,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if !matched {
                return Verdict::MissingTag { want: want.clone() };
            }
        }

        // ③ 이름 거부 — 마지막이다. 위를 통과했어도 거부한다.
        let lower = c.identifier.to_ascii_lowercase();
        for deny in &self.denied_name_substrings {
            let d = deny.to_ascii_lowercase();
            if !d.is_empty() && lower.contains(&d) {
                return Verdict::NameDenied {
                    matched: deny.clone(),
                };
            }
        }

        Verdict::Accept
    }
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
        }
    }

    fn dev_filter() -> Filter {
        Filter {
            allowed_vpc_ids: vec!["vpc-dev".into()],
            required_tags: vec![],
            denied_name_substrings: vec!["prd".into(), "prod".into()],
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
