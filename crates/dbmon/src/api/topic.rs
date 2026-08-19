//! WebSocket 구독 토픽의 파싱과 인가 ([09 §4.1], [13 §3] 인가 매트릭스).
//!
//! # 왜 별도 모듈인가
//!
//! 구독 인가가 **HTTP 인가와 별개의 우회 경로**다. `GET /api/slow-queries` 는
//! 환경 스코프를 교집합하는데, WS 가 `slowq:env=prd` 를 그냥 받아 주면 dev 만
//! 볼 수 있는 사용자가 prd 스트림을 받는다. 같은 판정을 두 번 구현하면 한쪽만
//! 고치는 일이 생기므로, 판정을 순수 함수로 떼어 놓고 양쪽이 이걸 쓴다.
//!
//! # 와일드카드를 거부한다
//!
//! `slowq:*` 이나 `slowq:env=*` 를 허용하면 "이 사용자가 볼 수 있는 환경" 을
//! 열거해 검사해야 하는데, 그 열거를 빠뜨리면 전체 구독이 된다. 문법 자체에서
//! 막는 편이 확실하다 — 넓은 구독이 필요하면 클라이언트가 여러 토픽을 보낸다.

use dbmon_core::env::Env;
use dbmon_core::rbac::AuthContext;

/// 클라이언트당 구독 토픽 상한 ([09 §4.1] 레이트 리밋).
pub const MAX_TOPICS: usize = 50;

/// 구독 가능한 토픽.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Topic {
    /// 특정 환경의 슬로우 쿼리 스트림.
    SlowQueries(Env),
    /// 특정 인스턴스의 실시간 지표.
    InstanceStatus(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicError {
    /// 문법이 틀렸거나 와일드카드다.
    Malformed,
    /// 문법은 맞지만 이 사용자가 볼 수 없다.
    Denied,
}

impl Topic {
    /// 문자열을 토픽으로. **와일드카드와 접두 구독을 거부한다.**
    pub fn parse(raw: &str) -> Result<Self, TopicError> {
        // 와일드카드를 **파싱 전에** 막는다. 아래 분기 어딘가에서 걸러지길
        // 기대하면 새 토픽 종류를 추가할 때 빠뜨린다.
        if raw.contains('*') || raw.is_empty() {
            return Err(TopicError::Malformed);
        }
        let (kind, arg) = raw.split_once(':').ok_or(TopicError::Malformed)?;
        match kind {
            "slowq" => {
                let env = arg.strip_prefix("env=").ok_or(TopicError::Malformed)?;
                parse_env(env).map(Topic::SlowQueries).ok_or(
                    // 알 수 없는 환경 이름은 문법 오류다 — 존재하지 않는 환경의
                    // 구독을 "권한 없음" 으로 답하면 환경 목록이 새어 나간다.
                    TopicError::Malformed,
                )
            }
            "status" => {
                let id = arg.strip_prefix("inst=").ok_or(TopicError::Malformed)?;
                // 인스턴스 id 형식을 검증한다 — 임의 문자열로 토픽을 만들면
                // 방송 키 공간이 클라이언트 입력으로 오염된다.
                dbmon_core::ids::InstanceId::try_from(id.to_string())
                    .map(|i| Topic::InstanceStatus(i.as_str().to_string()))
                    .map_err(|_| TopicError::Malformed)
            }
            _ => Err(TopicError::Malformed),
        }
    }

    /// 방송 키. 서버가 이 문자열로 수신자를 찾는다.
    pub fn key(&self) -> String {
        match self {
            Self::SlowQueries(env) => format!("slowq:env={}", env.as_str()),
            Self::InstanceStatus(id) => format!("status:inst={id}"),
        }
    }
}

/// 이 문맥이 토픽을 구독할 수 있는가.
///
/// `instance_env` 는 호출부가 등록부에서 조회해 넘긴다 — **`None` 은 거부**다.
/// 모르는 인스턴스를 허용하면 등록부에 없는 id 로 구독해 나중에 등록될 때
/// 자동으로 받게 된다.
pub fn authorize(
    topic: &Topic,
    ctx: &AuthContext,
    instance_env: Option<Env>,
) -> Result<(), TopicError> {
    let env = match topic {
        Topic::SlowQueries(env) => *env,
        Topic::InstanceStatus(_) => instance_env.ok_or(TopicError::Denied)?,
    };
    if ctx.is_env_allowed(env) {
        Ok(())
    } else {
        Err(TopicError::Denied)
    }
}

fn parse_env(s: &str) -> Option<Env> {
    // 대소문자를 관용하지 않는다 — `PRD` 를 받아 주면 스코프 비교가 흐려진다.
    Env::ALL.iter().copied().find(|e| e.as_str() == s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::rbac::Role;

    const INST: &str = "000000000000/ap-northeast-2/orders-prd-01";

    fn ctx(scope: &[Env]) -> AuthContext {
        AuthContext {
            subject: "u".into(),
            role: Role::Viewer,
            env_scope: scope.to_vec(),
            can_see_literals: false,
            claims_version: 0,
        }
    }

    #[test]
    fn parses_the_documented_forms() {
        assert_eq!(
            Topic::parse("slowq:env=prd"),
            Ok(Topic::SlowQueries(Env::Prd))
        );
        assert_eq!(
            Topic::parse(&format!("status:inst={INST}")),
            Ok(Topic::InstanceStatus(INST.to_string()))
        );
    }

    /// **와일드카드·접두 구독을 거부한다.**
    ///
    /// 허용하면 "볼 수 있는 환경" 열거가 필요해지고, 그 열거를 빠뜨리면 전체
    /// 구독이 된다. 문법에서 막는다.
    #[test]
    fn wildcards_are_rejected() {
        for raw in [
            "slowq:*",
            "slowq:env=*",
            "*",
            "status:inst=*",
            "slowq:env=pr*",
        ] {
            assert_eq!(
                Topic::parse(raw),
                Err(TopicError::Malformed),
                "{raw} 가 통과했다"
            );
        }
    }

    #[test]
    fn malformed_topics_are_rejected_without_panicking() {
        for raw in [
            "",
            ":",
            "slowq",
            "slowq:",
            "slowq:env=",
            "slowq:env=PRD",
            "slowq:env=production",
            "unknown:env=prd",
            "status:",
            "status:inst=",
            "status:inst=not-an-instance-id",
            "slowq:env=prd:extra",
            &"x".repeat(10_000),
        ] {
            assert_eq!(
                Topic::parse(raw),
                Err(TopicError::Malformed),
                "{raw:?} 가 통과했다"
            );
        }
    }

    /// **환경 스코프 밖은 구독할 수 없다.**
    ///
    /// 이게 없으면 HTTP 조회는 막히는데 WS 스트림으로는 같은 데이터가 나간다.
    #[test]
    fn subscribing_outside_the_env_scope_is_denied() {
        let dev_only = ctx(&[Env::Dev]);
        assert_eq!(
            authorize(&Topic::SlowQueries(Env::Prd), &dev_only, None),
            Err(TopicError::Denied)
        );
        assert_eq!(
            authorize(&Topic::SlowQueries(Env::Dev), &dev_only, None),
            Ok(())
        );
    }

    /// **모르는 인스턴스는 거부한다.**
    ///
    /// 허용하면 등록부에 없는 id 로 미리 구독해 두고, 그 인스턴스가 나중에
    /// prd 로 등록될 때 자동으로 받게 된다.
    #[test]
    fn an_unknown_instance_is_denied() {
        let all = ctx(&Env::ALL);
        assert_eq!(
            authorize(&Topic::InstanceStatus(INST.into()), &all, None),
            Err(TopicError::Denied)
        );
        assert_eq!(
            authorize(&Topic::InstanceStatus(INST.into()), &all, Some(Env::Prd)),
            Ok(())
        );
    }

    /// 인스턴스 상태 토픽도 **그 인스턴스의 환경**으로 판정한다 — 이름이 아니라.
    #[test]
    fn instance_status_uses_the_registry_env_not_the_name() {
        let dev_only = ctx(&[Env::Dev]);
        // 이름에 `prd` 가 들어 있어도 등록부가 dev 라고 하면 볼 수 있다.
        assert_eq!(
            authorize(
                &Topic::InstanceStatus(INST.into()),
                &dev_only,
                Some(Env::Dev)
            ),
            Ok(())
        );
        // 반대로 등록부가 prd 면 이름과 무관하게 막힌다.
        assert_eq!(
            authorize(
                &Topic::InstanceStatus(INST.into()),
                &dev_only,
                Some(Env::Prd)
            ),
            Err(TopicError::Denied)
        );
    }

    /// 방송 키가 왕복해야 한다 — 어긋나면 구독자가 조용히 아무것도 못 받는다.
    #[test]
    fn the_broadcast_key_round_trips() {
        for t in [
            Topic::SlowQueries(Env::Prd),
            Topic::SlowQueries(Env::Unknown),
            Topic::InstanceStatus(INST.into()),
        ] {
            assert_eq!(Topic::parse(&t.key()), Ok(t.clone()), "{t:?}");
        }
    }
}
