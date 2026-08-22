//! DynamoDB 사용자 레코드 어댑터 — `config_table` 의 `USER#<sub>`
//! ([08 §3](../../../../docs/08-security-auth.md) T-20·T-33).
//!
//! # 앱은 이 키 범위에 쓰지 않는다
//!
//! 문서(08 §5.2)가 규정했다: `USER` 레코드와 IdP 그룹 매핑 표에 대한 **앱 Role 의
//! 쓰기를 IAM 에서 Deny** 한다. 앱이 침해돼도 조용한 admin 승격이 불가능해야 하기
//! 때문이다.
//!
//! 그래서 이 어댑터는 **읽기만 구현한다.** 쓰기 함수를 만들어 두면 IAM Deny 에 걸려
//! 실패하는 코드가 되고, 언젠가 누가 Deny 를 풀어 그 경로를 살릴 수 있다. 없는 함수는
//! 살아나지 않는다.
//!
//! 레코드 생성은 별도 관리 경로(admin 전용 핸들러가 `AssumeRole` 로 승격)의 일이며
//! M5 의 남은 작업이다. 그때까지 레코드는 **사람이 콘솔·CLI 로 만든다** —
//! `docs/08-security-auth.md` 에 형식을 적어 둔다.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use dbmon_core::env::Env;
use dbmon_core::error::{DomainError, Result};
use dbmon_core::ports::UserStore;
use dbmon_core::rbac::{Role, UserRecord};

use super::map_sdk_err;

/// 파티션 키 접두어.
const PK_PREFIX: &str = "USER#";
/// 정렬 키 — 레코드가 하나뿐이다.
const SK: &str = "PROFILE";

pub struct DynamoUserStore {
    client: Client,
    table: String,
}

impl DynamoUserStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }

    pub fn pk(subject: &str) -> String {
        format!("{PK_PREFIX}{subject}")
    }
}

#[async_trait::async_trait]
impl UserStore for DynamoUserStore {
    async fn get(&self, subject: &str) -> Result<Option<UserRecord>> {
        // **`sub` 를 그대로 키에 넣지 않는다.** Cognito `sub` 는 UUID 지만, 검증을
        // 통과한 값이라도 키 조립에 쓰기 전에 형태를 본다 — 빈 문자열이나 개행이
        // 든 값이 오면 다른 항목을 가리킬 수 있다.
        if subject.is_empty() || subject.len() > 128 || subject.chars().any(|c| c.is_control()) {
            return Err(DomainError::InvalidInput {
                field: "subject".into(),
                reason: "사용자 식별자 형태가 아니다".into(),
            });
        }

        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(Self::pk(subject)))
            .key("SK", AttributeValue::S(SK.to_string()))
            // **강한 일관성으로 읽는다.** 권한 강등·비활성화가 즉시 반영돼야 한다
            // (T-33). 최종 일관성 읽기는 방금 폐기한 세션을 통과시킬 수 있다.
            .consistent_read(true)
            .send()
            .await
            .map_err(map_sdk_err)?;

        let Some(item) = out.item else {
            return Ok(None);
        };
        Ok(Some(parse_record(subject, &item)?))
    }
}

/// 항목을 레코드로 옮긴다.
///
/// # 필드가 없으면 **가장 낮은 권한**으로 떨어진다
///
/// `role` 이 없거나 읽을 수 없으면 `viewer` 다. 기본값을 admin 쪽으로 두면 손상된
/// 항목 하나가 권한 상승이 된다.
///
/// # `env_scope` 는 예외다 — **없으면 비운다**
///
/// 비어 있으면 아무것도 못 본다([`AuthContext`](dbmon_core::rbac::AuthContext) 문서).
/// 전체 허용으로 접으면 스코프를 지정하지 않은 레코드가 전 환경을 보게 된다.
fn parse_record(
    subject: &str,
    item: &std::collections::HashMap<String, AttributeValue>,
) -> Result<UserRecord> {
    let s = |k: &str| item.get(k).and_then(|v| v.as_s().ok()).map(String::as_str);
    let n = |k: &str| {
        item.get(k)
            .and_then(|v| v.as_n().ok())
            .and_then(|v| v.parse::<i64>().ok())
    };
    let b = |k: &str| item.get(k).and_then(|v| v.as_bool().ok()).copied();

    let role = s("role").and_then(Role::parse).unwrap_or(Role::Viewer);
    let env_scope: Vec<Env> = item
        .get("env_scope")
        .and_then(|v| v.as_ss().ok())
        .map(|list| list.iter().filter_map(|e| Env::parse(e)).collect())
        .unwrap_or_default();

    Ok(UserRecord {
        subject: subject.to_string(),
        role,
        env_scope,
        can_see_literals: b("can_see_literals").unwrap_or(false),
        // **버전이 없으면 0 이다.** 토큰의 값과 같아야 통과하므로, 레코드를 만들 때
        // 명시하지 않으면 `claims_version=0` 인 토큰만 들어온다 — 안전한 방향이다.
        claims_version: n("claims_version").unwrap_or(0).max(0) as u32,
        revoked_after_ms: n("revoked_after_ms"),
        disabled: b("disabled").unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn item(pairs: Vec<(&str, AttributeValue)>) -> HashMap<String, AttributeValue> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn the_partition_key_is_prefixed() {
        assert_eq!(DynamoUserStore::pk("aaaa-bbbb"), "USER#aaaa-bbbb");
    }

    #[test]
    fn a_complete_record_parses() {
        let r = parse_record(
            "sub-1",
            &item(vec![
                ("role", AttributeValue::S("operator".into())),
                (
                    "env_scope",
                    AttributeValue::Ss(vec!["dev".into(), "stg".into()]),
                ),
                ("can_see_literals", AttributeValue::Bool(true)),
                ("claims_version", AttributeValue::N("3".into())),
                (
                    "revoked_after_ms",
                    AttributeValue::N("1700000000000".into()),
                ),
                ("disabled", AttributeValue::Bool(false)),
            ]),
        )
        .expect("파싱");
        assert_eq!(r.role, Role::Operator);
        assert_eq!(r.env_scope, vec![Env::Dev, Env::Stg]);
        assert!(r.can_see_literals);
        assert_eq!(r.claims_version, 3);
        assert_eq!(r.revoked_after_ms, Some(1_700_000_000_000));
        assert!(!r.disabled);
    }

    /// **필드가 없으면 가장 낮은 권한이다.** 손상된 항목 하나가 권한 상승이 되면 안 된다.
    #[test]
    fn missing_fields_fall_to_the_lowest_privilege() {
        let r = parse_record("sub-1", &item(vec![])).expect("파싱");
        assert_eq!(r.role, Role::Viewer);
        assert!(!r.can_see_literals);
        assert_eq!(r.claims_version, 0);
        assert!(r.revoked_after_ms.is_none());
        assert!(!r.disabled);
        // **스코프는 비어 있다** — 전체 허용이 아니다.
        assert!(
            r.env_scope.is_empty(),
            "빈 스코프가 전체 허용이 되면 안 된다"
        );
    }

    /// 읽을 수 없는 역할 이름도 `viewer` 다.
    #[test]
    fn an_unknown_role_falls_to_viewer() {
        for bad in ["superuser", "ADMIN", "", "root"] {
            let r = parse_record("s", &item(vec![("role", AttributeValue::S(bad.into()))]))
                .expect("파싱");
            assert_eq!(r.role, Role::Viewer, "{bad:?} 가 승격됐다");
        }
    }

    /// 읽을 수 없는 환경 이름은 **버린다** (전체 허용으로 접지 않는다).
    #[test]
    fn unknown_environments_are_dropped_not_expanded() {
        let r = parse_record(
            "s",
            &item(vec![(
                "env_scope",
                AttributeValue::Ss(vec!["dev".into(), "nonsense".into()]),
            )]),
        )
        .expect("파싱");
        assert_eq!(r.env_scope, vec![Env::Dev]);
    }

    /// 음수 `claims_version` 은 0 으로 접는다 — `u32` 캐스팅이 감싸지 않게.
    #[test]
    fn a_negative_claims_version_does_not_wrap() {
        let r = parse_record(
            "s",
            &item(vec![("claims_version", AttributeValue::N("-1".into()))]),
        )
        .expect("파싱");
        assert_eq!(r.claims_version, 0, "음수가 u32 로 감쌌다");
    }

    /// `disabled` 가 참이면 [`AuthContext::intersect`] 가 거부한다 — 여기서는
    /// 값을 정확히 옮기는 것만 확인한다.
    #[test]
    fn the_disabled_flag_is_carried_through() {
        let r =
            parse_record("s", &item(vec![("disabled", AttributeValue::Bool(true))])).expect("파싱");
        assert!(r.disabled);
        let token = dbmon_core::rbac::TokenClaims {
            subject: "s".into(),
            groups: vec!["dbmon-admin".into()],
            env_scope: vec![],
            issued_at_ms: 1,
            claims_version: 0,
        };
        assert!(
            dbmon_core::rbac::AuthContext::intersect(&token, &r).is_none(),
            "비활성 사용자가 통과했다"
        );
    }
}
