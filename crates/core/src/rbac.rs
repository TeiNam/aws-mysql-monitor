//! 역할·스코프 인가 ([08 §3](../../../.claude/docs/08-security-auth.md), FR-AUT-08).
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

/// Cognito 그룹 이름 접두어.
///
/// 사용자 풀이 다른 앱과 공유될 수 있으므로 **접두어가 필수다** — `admin` 이라는
/// 그룹은 어느 앱의 admin 인지 말해 주지 않는다. Terraform 이 이 이름으로 그룹을
/// 만든다(`infra/layers/30-identity/cognito.tf`).
pub const GROUP_PREFIX: &str = "dbmon-";

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
    ///
    /// # `dbmon-` 접두어를 **요구한다**
    ///
    /// 실제 Cognito 그룹 이름은 `dbmon-admin`·`dbmon-operator`·`dbmon-viewer` 다
    /// ([08 §4.1](../../../.claude/docs/08-security-auth.md)). 접두어가 붙는 이유는 사용자 풀이
    /// 다른 앱과 공유될 수 있어서다 — `admin` 이라는 그룹은 어느 앱의 admin 인지
    /// 말해 주지 않는다.
    ///
    /// 이 판정은 **두 방향으로** 틀렸던 적이 있다:
    ///
    /// | 잘못 | 결과 |
    /// |---|---|
    /// | 접두어를 **모른다** | 모든 그룹이 `None` → 토큰이 역할을 좁히지 못한다 (fail-open) |
    /// | 접두어를 **요구하지 않는다** | 공유 풀의 다른 앱 `admin` 이 dbmon admin 이 된다 |
    ///
    /// 두 번째를 서버 레코드 교집합이 막지 못한다 — 서버 레코드가 admin 이면
    /// `min()` 이 아무것도 낮추지 않는다(교차 리뷰 3차).
    pub fn highest_from_groups(groups: &[String]) -> Option<Self> {
        groups
            .iter()
            .filter_map(|g| g.strip_prefix(GROUP_PREFIX))
            .filter_map(Self::parse)
            .max()
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
    /// (승인 대기 목록 모델, [OPEN-Q-18](../../../.claude/docs/OPEN-QUESTIONS.md)).
    pub fn intersect(token: &TokenClaims, server: &UserRecord) -> Option<Self> {
        if server.disabled {
            return None;
        }
        // 강등 후 발급된 구 토큰을 막는다 (T-33).
        //
        // **같지 않으면 거부한다.** `<` 로만 보면 서버보다 **높은** 버전을 주장하는
        // 토큰이 통과한다. 서명이 그 값을 덮으므로 클라이언트가 위조할 수는 없지만,
        // 서버 버전이 되돌아간 상황(백업 복원, 레코드 재생성)에서 폐기했어야 할
        // 토큰이 살아난다. 이 검사의 의도는 "발급 시점과 현재가 같다" 이므로 그대로
        // 쓴다.
        //
        // 토큰이 버전을 **주장하지 않으면**(`None`) 이 검사를 건너뛴다 — 없는 정보로
        // 판정을 흉내내지 않는다. 그때 남는 폐기 수단은 `revoked_after_ms` 다.
        if let Some(v) = token.claims_version
            && v != server.claims_version
        {
            return None;
        }
        if let Some(revoked_after) = server.revoked_after_ms
            && token.issued_at_ms <= revoked_after
        {
            return None;
        }
        // **그룹이 비어 있으면 권한이 없다** (교차 리뷰가 잡은 결함).
        //
        // 처음에는 `None` 을 "토큰이 좁히지 않는다" 로 읽어 서버 역할을 그대로 썼다.
        // 그러면 사용자를 `dbmon-admin` 그룹에서 빼도 **여전히 admin** 이다 —
        // 교집합이라고 적어 놓고 교집합이 아니었다.
        //
        // Cognito 는 네이티브 사용자의 그룹 멤버십을 `cognito:groups` 에 **자동으로**
        // 넣는다(트리거가 필요 없다). 그래서 빈 목록은 "정보 없음" 이 아니라 "그룹이
        // 없다" 이고, 그건 권한이 없다는 뜻이다(FR-AUT-08, fail-closed).
        //
        // IdP 페더레이션은 이 프로젝트가 아직 배선하지 않았다. 붙일 때는 Pre Token
        // Generation 트리거로 그룹을 주입하거나(문서 08 §2.3) 이 판정을 설정으로
        // 갈라야 한다 — 그 사실을 문서에 적어 둔다.
        let role = Role::highest_from_groups(&token.groups)?.min(server.role);
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
    /// 토큰이 주장하는 클레임 버전. **`None` 은 "주장하지 않는다"** 다.
    ///
    /// # 왜 `Option` 인가 (교차 리뷰가 잡은 결함)
    ///
    /// `u32` 였을 때 검증기가 서버 값을 그대로 넣었다 — 그러면 아래 버전 검사가
    /// **항상 통과**하고 T-33 의 독립적인 폐기 수단이 사라진다. 0 을 넣으면 반대로
    /// 모든 토큰이 거부된다(서버가 0이 아니면).
    ///
    /// `Option` 이면 사실을 그대로 표현한다: Pre Token Generation 트리거가 없는
    /// 배포는 `None` 이고 그 검사를 건너뛴다(그때 남는 방어선은
    /// [`UserRecord::revoked_after_ms`] 다). 트리거를 붙이면 `Some` 이 되고 검사가
    /// 살아난다. **가짜로 통과시키지 않는다.**
    pub claims_version: Option<u32>,
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
            claims_version: Some(3),
        }
    }

    /// T-20 — 위조된 `cognito:groups` 로 admin 을 주장해도 통하지 않는다.
    #[test]
    fn t20_forged_admin_group_is_capped_by_server_record() {
        let ctx = AuthContext::intersect(&token(&["dbmon-admin"]), &server(Role::Viewer)).unwrap();
        assert_eq!(ctx.role, Role::Viewer, "서버 레코드가 권위값이다");
        assert!(!ctx.has_role(Role::Operator));
    }

    /// **그룹이 비어 있으면 권한이 없다** (교차 리뷰가 잡은 결함).
    ///
    /// 이전에는 빈 그룹을 "토큰이 좁히지 않는다" 로 읽어 서버 역할을 그대로 썼다.
    /// 그러면 사용자를 Cognito 그룹에서 빼도 여전히 admin 이다 — 교집합이라고
    /// 적어 놓고 교집합이 아니었다.
    ///
    /// Cognito 는 네이티브 사용자의 그룹을 자동으로 클레임에 넣으므로, 빈 목록은
    /// "정보 없음" 이 아니라 "그룹이 없다" 다.
    #[test]
    fn an_empty_group_claim_grants_nothing() {
        assert!(
            AuthContext::intersect(&token(&[]), &server(Role::Admin)).is_none(),
            "그룹이 없는 토큰이 admin 으로 통과했다"
        );
        // 그룹이 있으면 서버 역할과 교집합된다.
        let ctx = AuthContext::intersect(&token(&["dbmon-operator"]), &server(Role::Admin))
            .expect("인가");
        assert_eq!(ctx.role, Role::Operator);
    }

    /// **토큰이 버전을 주장하지 않으면 그 검사를 건너뛴다.**
    ///
    /// 서버 값을 토큰에 채워 넣어 "항상 같다" 로 만들면 검사가 죽은 코드가 된다 —
    /// 교차 리뷰가 잡은 자리다. `None` 은 사실을 그대로 표현한다.
    #[test]
    fn a_token_without_a_claims_version_skips_that_check() {
        let mut t = token(&["dbmon-admin"]);
        t.claims_version = None;
        let s = server(Role::Admin); // claims_version = 3
        let ctx = AuthContext::intersect(&t, &s).expect("인가");
        assert_eq!(ctx.claims_version, 3, "문맥은 서버 값을 쓴다");

        // 주장하면 대조한다.
        t.claims_version = Some(2);
        assert!(AuthContext::intersect(&t, &s).is_none());
    }

    #[test]
    fn token_role_can_also_lower_the_result() {
        let ctx = AuthContext::intersect(&token(&["dbmon-viewer"]), &server(Role::Admin)).unwrap();
        assert_eq!(ctx.role, Role::Viewer, "더 낮은 쪽을 쓴다");
    }

    /// **`dbmon-` 접두어가 붙은 그룹만 인식한다.**
    ///
    /// 두 방향의 결함을 함께 고정한다:
    ///
    /// 1. 접두어를 **모르면** 모든 그룹이 인식되지 않아 토큰이 역할을 좁히지 못한다
    ///    (fail-open, 초기 결함)
    /// 2. 접두어를 **요구하지 않으면** 사용자 풀을 공유하는 다른 앱의 평범한 `admin`
    ///    그룹이 dbmon admin 이 된다 (교차 리뷰 3차)
    ///
    /// 접두어를 붙인 이유가 2번이므로, 접두어 없는 이름을 받으면 그 이유가 사라진다.
    #[test]
    fn only_prefixed_cognito_groups_are_recognized() {
        let ctx =
            AuthContext::intersect(&token(&["dbmon-viewer"]), &server(Role::Admin)).expect("인가");
        assert_eq!(
            ctx.role,
            Role::Viewer,
            "dbmon-viewer 가 admin 을 낮춰야 한다"
        );

        // **접두어 없는 이름은 받지 않는다.**
        for plain in ["viewer", "operator", "admin"] {
            assert!(
                AuthContext::intersect(&token(&[plain]), &server(Role::Admin)).is_none(),
                "접두어 없는 {plain:?} 이 권한을 줬다 — 다른 앱의 그룹이 dbmon 권한이 된다"
            );
        }
        // 다른 접두어도 받지 않는다.
        for other in [
            "otherapp-admin",
            "DBMON-admin",
            "dbmon_admin",
            "xdbmon-admin",
        ] {
            assert_eq!(
                Role::highest_from_groups(&[other.to_string()]),
                None,
                "{other:?} 를 인식했다"
            );
        }

        // 여러 그룹이면 가장 높은 것.
        let both = AuthContext::intersect(
            &token(&["dbmon-viewer", "dbmon-operator"]),
            &server(Role::Admin),
        )
        .expect("인가");
        assert_eq!(both.role, Role::Operator);
    }

    /// **`claims_version` 은 같아야 한다.**
    ///
    /// `<` 로만 보면 서버보다 높은 버전을 주장하는 토큰이 통과한다. 서명이 그 값을
    /// 덮으므로 위조는 아니지만, 서버 버전이 되돌아간 상황(백업 복원)에서 폐기했어야
    /// 할 토큰이 살아난다.
    #[test]
    fn a_claims_version_mismatch_is_rejected_in_both_directions() {
        let mut older = token(&["dbmon-admin"]);
        older.claims_version = Some(2); // 서버는 3
        assert!(
            AuthContext::intersect(&older, &server(Role::Admin)).is_none(),
            "강등 전 토큰이 통과했다"
        );

        let mut newer = token(&["dbmon-admin"]);
        newer.claims_version = Some(4); // 서버보다 높다
        assert!(
            AuthContext::intersect(&newer, &server(Role::Admin)).is_none(),
            "서버보다 높은 버전을 주장하는 토큰이 통과했다"
        );

        let same = token(&["dbmon-admin"]);
        assert!(AuthContext::intersect(&same, &server(Role::Admin)).is_some());
    }

    /// 인식하지 못하는 그룹만 있으면 **권한이 없다.**
    #[test]
    fn unknown_groups_grant_nothing() {
        assert!(
            AuthContext::intersect(&token(&["superuser", "root"]), &server(Role::Operator))
                .is_none(),
            "인식 못하는 그룹이 권한을 줬다"
        );
        assert_eq!(Role::highest_from_groups(&["nope".into()]), None);
        // 접두어가 없으면 인식하지 않는다.
        assert_eq!(Role::highest_from_groups(&["admin".into()]), None);
        assert_eq!(
            Role::highest_from_groups(&["dbmon-admin".into()]),
            Some(Role::Admin)
        );
        // 인식하는 그룹이 하나라도 있으면 그것으로 판정한다.
        let ctx = AuthContext::intersect(
            &token(&["superuser", "dbmon-viewer"]),
            &server(Role::Operator),
        )
        .expect("인가");
        assert_eq!(ctx.role, Role::Viewer);
    }

    /// T-33 — 강등 후 기존 토큰이 계속 통하면 안 된다.
    #[test]
    fn t33_stale_claims_version_is_rejected() {
        let mut s = server(Role::Admin);
        s.claims_version = 4; // 강등 등으로 서버가 버전을 올렸다
        let mut t = token(&["dbmon-admin"]);
        t.claims_version = Some(3); // 구 토큰
        assert!(AuthContext::intersect(&t, &s).is_none());
    }

    #[test]
    fn revoked_after_blocks_older_tokens() {
        let mut s = server(Role::Admin);
        s.revoked_after_ms = Some(2_000);
        let mut t = token(&["dbmon-admin"]);
        t.issued_at_ms = 1_500;
        assert!(AuthContext::intersect(&t, &s).is_none());
        t.issued_at_ms = 2_500;
        assert!(AuthContext::intersect(&t, &s).is_some());
    }

    #[test]
    fn disabled_user_is_rejected() {
        let mut s = server(Role::Admin);
        s.disabled = true;
        assert!(AuthContext::intersect(&token(&["dbmon-admin"]), &s).is_none());
    }

    #[test]
    fn env_scope_is_intersected_not_unioned() {
        let mut t = token(&["dbmon-operator"]);
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
        let ctx = AuthContext::intersect(&token(&["dbmon-viewer"]), &server(Role::Viewer)).unwrap();
        assert_eq!(ctx.scope_intersection(&[]), vec![Env::Dev, Env::Stg]);
        assert_eq!(ctx.scope_intersection(&[Env::Prd]), Vec::<Env>::new());
        assert_eq!(
            ctx.scope_intersection(&[Env::Stg, Env::Prd]),
            vec![Env::Stg]
        );
    }

    #[test]
    fn literal_visibility_follows_policy_and_role() {
        let viewer =
            AuthContext::intersect(&token(&["dbmon-viewer"]), &server(Role::Viewer)).unwrap();
        let operator =
            AuthContext::intersect(&token(&["dbmon-operator"]), &server(Role::Operator)).unwrap();

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
