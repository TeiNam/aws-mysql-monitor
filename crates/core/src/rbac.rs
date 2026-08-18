//! 역할·스코프 인가 ([08 §3](../../../docs/08-security-auth.md), FR-AUT-08).
//!
//! # 토큰 클레임과 서버 레코드의 **교집합**을 쓴다 (T-20)
//!
//! 토큰의 `cognito:groups` 만 믿으면, 위조된 클레임으로 admin 을 주장할 수 있다
//! (트리거 설정 오류·IdP 속성 매핑 오류로도 발생한다).
//! 서버 측 `USER` 레코드가 권위값이고, 토큰은 **보조 신호**다.
//! 두 값의 교집합(더 낮은 권한)을 쓰면 어느 한쪽이 과대 표기돼도 안전하다.

use crate::env::Env;
use serde::{Deserialize, Serialize};

/// 역할. **순서가 권한 크기다** (`Viewer < Operator < Admin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Self::Viewer),
            "operator" => Some(Self::Operator),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// Cognito 그룹 목록에서 **가장 높은** 역할을 뽑는다.
    /// 인식할 수 없는 그룹은 무시한다(권한을 주지 않는다).
    pub fn highest_from_groups(groups: &[String]) -> Option<Self> {
        groups.iter().filter_map(|g| Self::parse(g)).max()
    }
}

/// 인가된 요청의 문맥. 라우터가 이걸 추출자로 받는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    /// Cognito `sub`. 감사 로그의 주체.
    pub subject: String,
    /// **교집합이 적용된** 실효 역할.
    pub role: Role,
    /// 볼 수 있는 환경. 비어 있으면 아무것도 못 본다(전체 허용이 아니다).
    pub env_scope: Vec<Env>,
    /// 리터럴을 볼 수 있는가. 역할과 **별도**다 — operator 여도 끌 수 있다.
    pub can_see_literals: bool,
    /// 서버가 관리하는 클레임 버전. 강등 시 올려 기존 토큰을 무효화한다(FR-AUT-12).
    pub claims_version: u32,
}

impl AuthContext {
    /// 토큰 클레임과 서버 `USER` 레코드를 교집합해 문맥을 만든다.
    ///
    /// - 역할: **더 낮은 쪽**
    /// - 환경 스코프: **교집합**
    /// - 리터럴 열람: **둘 다 참일 때만**
    ///
    /// 서버 레코드가 없으면 `None` — 등록되지 않은 사용자는 토큰이 유효해도 거부한다
    /// (승인 대기 목록 모델, [OPEN-Q-18](../../../docs/OPEN-QUESTIONS.md)).
    pub fn intersect(token: &TokenClaims, server: &UserRecord) -> Option<Self> {
        if server.disabled {
            return None;
        }
        // 강등 후 발급된 구 토큰을 막는다 (T-33).
        if token.claims_version < server.claims_version {
            return None;
        }
        if let Some(revoked_after) = server.revoked_after_ms {
            if token.issued_at_ms <= revoked_after {
                return None;
            }
        }
        let token_role = Role::highest_from_groups(&token.groups);
        // 토큰에 그룹이 없어도 서버 레코드만으로 동작한다 — 트리거가 불필요해진다.
        let role = match token_role {
            Some(t) => t.min(server.role),
            None => server.role,
        };
        let env_scope: Vec<Env> = server
            .env_scope
            .iter()
            .copied()
            .filter(|e| token.env_scope.is_empty() || token.env_scope.contains(e))
            .collect();
        Some(Self {
            subject: server.subject.clone(),
            role,
            env_scope,
            can_see_literals: server.can_see_literals && role >= Role::Operator,
            claims_version: server.claims_version,
        })
    }

    pub fn has_role(&self, required: Role) -> bool {
        self.role >= required
    }

    pub fn is_env_allowed(&self, env: Env) -> bool {
        self.env_scope.contains(&env)
    }

    /// 요청한 환경 ∩ 사용자 스코프. **빈 결과는 빈 응답을 뜻한다** (403 이 아니다 —
    /// 스코프 밖 자원의 존재를 알려주지 않는다).
    pub fn scope_intersection(&self, requested: &[Env]) -> Vec<Env> {
        if requested.is_empty() {
            return self.env_scope.clone();
        }
        requested
            .iter()
            .copied()
            .filter(|e| self.env_scope.contains(e))
            .collect()
    }

    /// 이 문맥에서 리터럴을 보여줘도 되는가.
    ///
    /// 환경별 정책과 함께 판단한다: `full_restricted` 는 operator 이상만이다.
    pub fn may_view_literals(&self, policy: crate::slow_query::LiteralPolicy) -> bool {
        use crate::slow_query::LiteralPolicy as P;
        match policy {
            P::Full => self.can_see_literals,
            P::FullRestricted => self.can_see_literals && self.role >= Role::Operator,
            // 저장 자체가 마스킹·미저장이므로 볼 것이 없다.
            P::Masked | P::Off => false,
        }
    }
}

/// 검증된 JWT 에서 뽑은 클레임. **권위값이 아니다.**
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenClaims {
    pub subject: String,
    pub groups: Vec<String>,
    pub env_scope: Vec<Env>,
    pub issued_at_ms: i64,
    pub claims_version: u32,
}

/// 서버 측 `USER` 레코드 — **권위값이다.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub subject: String,
    pub role: Role,
    pub env_scope: Vec<Env>,
    pub can_see_literals: bool,
    pub claims_version: u32,
    /// 이 시각 이전에 발급된 토큰을 무효화한다(FR-AUT-12).
    pub revoked_after_ms: Option<i64>,
    pub disabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slow_query::LiteralPolicy;

    fn server(role: Role) -> UserRecord {
        UserRecord {
            subject: "sub-1".into(),
            role,
            env_scope: vec![Env::Dev, Env::Stg],
            can_see_literals: true,
            claims_version: 3,
            revoked_after_ms: None,
            disabled: false,
        }
    }

    fn token(groups: &[&str]) -> TokenClaims {
        TokenClaims {
            subject: "sub-1".into(),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            env_scope: vec![],
            issued_at_ms: 1_000,
            claims_version: 3,
        }
    }

    /// T-20 — 위조된 `cognito:groups` 로 admin 을 주장해도 통하지 않는다.
    #[test]
    fn t20_forged_admin_group_is_capped_by_server_record() {
        let ctx = AuthContext::intersect(&token(&["admin"]), &server(Role::Viewer)).unwrap();
        assert_eq!(ctx.role, Role::Viewer, "서버 레코드가 권위값이다");
        assert!(!ctx.has_role(Role::Operator));
    }

    #[test]
    fn server_role_alone_is_enough() {
        // 토큰에 그룹이 없어도 동작한다 → Pre Token Generation 트리거가 불필요하다.
        let ctx = AuthContext::intersect(&token(&[]), &server(Role::Operator)).unwrap();
        assert_eq!(ctx.role, Role::Operator);
    }

    #[test]
    fn token_role_can_also_lower_the_result() {
        let ctx = AuthContext::intersect(&token(&["viewer"]), &server(Role::Admin)).unwrap();
        assert_eq!(ctx.role, Role::Viewer, "더 낮은 쪽을 쓴다");
    }

    #[test]
    fn unknown_groups_grant_nothing() {
        let ctx = AuthContext::intersect(&token(&["superuser", "root"]), &server(Role::Operator))
            .unwrap();
        assert_eq!(ctx.role, Role::Operator, "인식 못하는 그룹은 무시한다");
        assert_eq!(Role::highest_from_groups(&["nope".into()]), None);
    }

    /// T-33 — 강등 후 기존 토큰이 계속 통하면 안 된다.
    #[test]
    fn t33_stale_claims_version_is_rejected() {
        let mut s = server(Role::Admin);
        s.claims_version = 4; // 강등 등으로 서버가 버전을 올렸다
        let mut t = token(&["admin"]);
        t.claims_version = 3; // 구 토큰
        assert!(AuthContext::intersect(&t, &s).is_none());
    }

    #[test]
    fn revoked_after_blocks_older_tokens() {
        let mut s = server(Role::Admin);
        s.revoked_after_ms = Some(2_000);
        let mut t = token(&["admin"]);
        t.issued_at_ms = 1_500;
        assert!(AuthContext::intersect(&t, &s).is_none());
        t.issued_at_ms = 2_500;
        assert!(AuthContext::intersect(&t, &s).is_some());
    }

    #[test]
    fn disabled_user_is_rejected() {
        let mut s = server(Role::Admin);
        s.disabled = true;
        assert!(AuthContext::intersect(&token(&["admin"]), &s).is_none());
    }

    #[test]
    fn env_scope_is_intersected_not_unioned() {
        let mut t = token(&["operator"]);
        t.env_scope = vec![Env::Prd, Env::Dev]; // 토큰이 prd 를 주장
        let ctx = AuthContext::intersect(&t, &server(Role::Operator)).unwrap();
        assert_eq!(
            ctx.env_scope,
            vec![Env::Dev],
            "서버 스코프에 없는 prd 는 빠진다"
        );
        assert!(!ctx.is_env_allowed(Env::Prd));
    }

    #[test]
    fn scope_intersection_empty_request_means_all_of_mine() {
        let ctx = AuthContext::intersect(&token(&["viewer"]), &server(Role::Viewer)).unwrap();
        assert_eq!(ctx.scope_intersection(&[]), vec![Env::Dev, Env::Stg]);
        assert_eq!(ctx.scope_intersection(&[Env::Prd]), Vec::<Env>::new());
        assert_eq!(
            ctx.scope_intersection(&[Env::Stg, Env::Prd]),
            vec![Env::Stg]
        );
    }

    #[test]
    fn literal_visibility_follows_policy_and_role() {
        let viewer = AuthContext::intersect(&token(&["viewer"]), &server(Role::Viewer)).unwrap();
        let operator =
            AuthContext::intersect(&token(&["operator"]), &server(Role::Operator)).unwrap();

        // viewer 는 can_see_literals=true 여도 operator 미달이라 full_restricted 를 못 본다.
        assert!(
            !viewer.can_see_literals,
            "역할이 낮으면 리터럴 권한도 내려간다"
        );
        assert!(!viewer.may_view_literals(LiteralPolicy::FullRestricted));
        assert!(operator.may_view_literals(LiteralPolicy::FullRestricted));

        // 저장이 마스킹·미저장이면 누구도 볼 것이 없다.
        for p in [LiteralPolicy::Masked, LiteralPolicy::Off] {
            assert!(!operator.may_view_literals(p));
        }
    }

    #[test]
    fn role_ordering_reflects_privilege_size() {
        assert!(Role::Admin > Role::Operator);
        assert!(Role::Operator > Role::Viewer);
        for r in [Role::Viewer, Role::Operator, Role::Admin] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
    }
}
