//! 계획·실행 오케스트레이션 (M3-6·M3-7, [07 §2.7](../../../../docs/07-credentials-bootstrap.md)).
//!
//! # 계획은 서버가 들고 있는다
//!
//! `plan_id` 로 5분간 보관한다. 클라이언트가 계획을 되돌려 보내게 하면 **사람이 승인한
//! 것과 실행되는 것이 다를 수 있다** — 그게 §2.6.1 (a) 가 막으려는 것이다.
//!
//! 보관하는 값은 계획 자체가 아니라 **지문 + 설정 + 계정 존재 여부**다. 실행 시점에
//! 대상 DB 를 다시 읽어 계획을 재도출하고 지문을 비교한다. 계획을 그대로 보관하면
//! "그 사이 상태가 바뀌었다" 를 판정할 근거가 없어진다.
//!
//! # 왜 큐에 넣지 않는가
//!
//! 문서(07 §1.3)가 규정했다: 부트스트랩은 동기 처리한다. 큐·잡 테이블에 넣으면
//! 마스터 자격증명이 **직렬화되는 경로**가 생긴다. 일괄 처리도 한 요청 안에서 순차로
//! 돈다.

use std::collections::HashMap;
use std::sync::Mutex;

use dbmon_core::bootstrap::{Blocker, Desired, plan as make_plan, revalidate};
use dbmon_core::env::Env;
use dbmon_core::instance::Instance;
use dbmon_core::time::EpochMs;

use super::audit::{self, AuditEvent, AuditRecord, AuditResult, StateSnapshot};
use super::mysql::MasterConn;
use super::secret::{CredentialSource, MasterCredentials};

/// `plan_id` 유효 기간. 문서가 5분으로 규정했다 (07 §2.7).
pub const PLAN_TTL_MS: i64 = 5 * 60 * 1_000;

/// 보관 중인 계획 수 상한.
///
/// 계획은 사람이 화면에서 만드는 것이므로 많을 이유가 없다. 상한이 없으면 계획
/// 생성 API 가 메모리 증가 경로가 된다.
const MAX_PENDING_PLANS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("계획을 찾을 수 없거나 만료됐다 (plan_id={0}) — 다시 계획을 만든다")]
    PlanNotFound(String),
    #[error("진행할 수 없다: {}", .0.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(" / "))]
    Blocked(Vec<Blocker>),
    #[error("prd 인스턴스는 식별자를 정확히 타이핑해야 실행된다 (기대: {expected})")]
    ConfirmationRequired { expected: String },
    #[error("마스터 자격증명을 얻을 수 없다: {0}")]
    Credentials(#[from] super::secret::SecretError),
    #[error("설정이 올바르지 않다: {0}")]
    Invalid(String),
    #[error(transparent)]
    Domain(#[from] dbmon_core::error::DomainError),
}

/// 계획 응답.
#[derive(Debug, serde::Serialize)]
pub struct PlanOutcome {
    pub plan_id: String,
    pub instance_id: String,
    pub env: Env,
    /// 실행할 문장 — 마스킹된 형태다.
    pub statements: Vec<String>,
    /// SQL 이 아닌 안내 (IAM 인증 활성화 명령 등).
    pub manual_steps: Vec<String>,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub excess_privileges: Vec<String>,
    /// 실행해도 되는가.
    pub executable: bool,
    /// 할 일이 없는가 (이미 원하는 상태다).
    pub noop: bool,
    /// prd 면 이 값을 타이핑해야 한다.
    pub confirmation_required: Option<String>,
    pub expires_at_ms: EpochMs,
    /// 복사해서 직접 실행할 스크립트 (M3-14). 앱에 권한을 주지 않는 경로다.
    pub manual_script: String,
}

/// 실행 보고 — **감사 레코드가 항상 있다.**
///
/// # 왜 `Result` 로 감싸지 않는가
///
/// 처음에는 `apply` 가 `Result<(ApplyOutcome, AuditRecord), BootstrapError>` 였다.
/// 그러면 **문장 중간에 실패했을 때 레코드가 버려진다** — 계정은 만들어졌고 권한은
/// 반쪽인 상태인데 감사 기록이 없다. 그게 감사 기록이 가장 필요한 순간이다
/// (FR-CRD-10).
///
/// 그래서 대상 DB 에 닿은 뒤의 실패는 `Err` 가 아니라 이 구조체의 `result` 로 온다.
/// DB 에 닿기 **전에** 거부된 경우(계획 만료·확인 불일치)는 여전히 `Err` 다 —
/// 그때는 아무 일도 일어나지 않았으므로 남길 것이 없다.
pub struct ApplyReport {
    /// 무엇을 했는지. 실패해도 채워진다.
    pub record: AuditRecord,
    /// 성공했으면 결과, 실패했으면 사유.
    pub result: Result<ApplyOutcome, BootstrapError>,
}

/// 실행 결과.
#[derive(Debug, serde::Serialize)]
pub struct ApplyOutcome {
    pub instance_id: String,
    pub executed: usize,
    pub total: usize,
    /// 실행 후 다시 읽은 상태.
    pub after: StateSnapshot,
    pub credential_source: CredentialSource,
}

/// 보관 중인 계획.
struct Pending {
    instance_id: String,
    desired: Desired,
    is_production: bool,
    fingerprint: String,
    /// 계획 시점에 계정이 있었는가 — §2.6.1 (b) 판정에 쓴다.
    user_existed: bool,
    /// 계획 시점의 **보안 상태 지문.** 액션 지문이 못 잡는 변화(비밀번호 교체 등)를
    /// 여기서 잡는다 ([`CurrentState::security_digest`]).
    state_digest: String,
    expires_at_ms: EpochMs,
}

/// 계획 보관소 + 실행기.
pub struct Bootstrapper {
    pending: Mutex<HashMap<String, Pending>>,
}

impl Default for Bootstrapper {
    fn default() -> Self {
        Self::new()
    }
}

impl Bootstrapper {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 계획을 만든다 — **대상 DB 를 읽지만 바꾸지 않는다.**
    ///
    /// `plan_id_seed` 는 호출부가 주는 난수다(엔트로피를 이 모듈이 만들지 않는다 —
    /// 테스트가 프로세스 상태를 건드리지 않게).
    #[allow(clippy::too_many_arguments)]
    pub async fn plan(
        &self,
        instance: &Instance,
        desired: &Desired,
        credentials: &MasterCredentials,
        opts: mysql_async::Opts,
        iam_auth_enabled: bool,
        now_ms: EpochMs,
        plan_id_seed: &str,
    ) -> Result<(PlanOutcome, AuditRecord), BootstrapError> {
        let is_production = treat_as_production(&instance.env);
        let mut conn = MasterConn::connect(opts, instance.id.as_str()).await?;
        let current = conn.introspect(desired, iam_auth_enabled).await;
        conn.close().await;
        let current = current?;

        let plan = make_plan(desired, &current, instance.id.as_str(), is_production)
            .map_err(|e| BootstrapError::Invalid(e.to_string()))?;

        let plan_id = plan_id_seed.to_string();
        let expires_at_ms = now_ms + PLAN_TTL_MS;
        self.store(
            plan_id.clone(),
            Pending {
                instance_id: instance.id.as_str().to_string(),
                desired: desired.clone(),
                is_production,
                fingerprint: plan.fingerprint(),
                user_existed: current.user_exists,
                state_digest: current.security_digest(),
                expires_at_ms,
            },
            now_ms,
        );

        let outcome = PlanOutcome {
            plan_id,
            instance_id: instance.id.as_str().to_string(),
            env: instance.env.effective,
            statements: plan
                .statements()
                .map(|s| s.redacted().to_string())
                .collect(),
            manual_steps: plan
                .actions
                .iter()
                .filter_map(|a| match a {
                    dbmon_core::bootstrap::Action::EnableIamAuth { instance_id } => Some(format!(
                        "aws rds modify-db-instance --db-instance-identifier {instance_id} \
                         --enable-iam-database-authentication --apply-immediately"
                    )),
                    dbmon_core::bootstrap::Action::Sql(_) => None,
                })
                .collect(),
            blockers: audit::blocker_messages(&plan.blockers),
            warnings: plan.warnings.clone(),
            excess_privileges: plan.excess.clone(),
            executable: plan.is_executable(),
            noop: plan.is_noop(),
            confirmation_required: is_production.then(|| short_identifier(instance)),
            expires_at_ms,
            manual_script: dbmon_core::bootstrap::sql::manual_script(&plan),
        };

        let record = AuditRecord {
            event: AuditEvent::BootstrapPlan,
            actor: String::new(), // 호출부가 채운다
            worker_id: String::new(),
            at_ms: now_ms,
            instance_id: instance.id.as_str().to_string(),
            env: instance.env.effective,
            credential_source: Some(credentials.source),
            privilege_mode: desired.mode.as_str(),
            monitor_user: desired.user.clone(),
            monitor_host: desired.host.clone(),
            actions: audit::actions_of(&plan, 0),
            before: StateSnapshot::of(&current),
            after: None,
            result: if plan.is_executable() {
                AuditResult::Success
            } else {
                AuditResult::Blocked
            },
            confirmation_typed: false,
            blockers: audit::blocker_messages(&plan.blockers),
            excess_privileges: plan.excess,
        };

        Ok((outcome, record))
    }

    /// 실행한다 (M3-7).
    ///
    /// 순서가 중요하다:
    /// 1. 계획을 찾는다 (만료면 거부)
    /// 2. **prd 면 타이핑 확인을 먼저 본다** — DB 에 붙기 전에 거부한다
    /// 3. 대상 DB 를 다시 읽어 계획을 재도출하고 지문을 비교한다 (§2.6.1 a·b)
    /// 4. 문장을 순서대로 실행한다
    /// 5. 다시 읽어 after 상태를 만든다
    #[allow(clippy::too_many_arguments)]
    pub async fn apply(
        &self,
        plan_id: &str,
        instance: &Instance,
        credentials: &MasterCredentials,
        opts: mysql_async::Opts,
        iam_auth_enabled: bool,
        confirmation: Option<&str>,
        now_ms: EpochMs,
    ) -> Result<ApplyReport, BootstrapError> {
        let pending = self
            .take(plan_id, now_ms)
            .ok_or_else(|| BootstrapError::PlanNotFound(plan_id.to_string()))?;

        // 계획이 다른 인스턴스의 것이면 거부한다 — plan_id 를 다른 인스턴스에
        // 붙여 보내는 것을 막는다.
        if pending.instance_id != instance.id.as_str() {
            return Err(BootstrapError::PlanNotFound(plan_id.to_string()));
        }

        // **DB 에 붙기 전에** 타이핑 확인을 본다.
        if pending.is_production {
            let expected = short_identifier(instance);
            if confirmation != Some(expected.as_str()) {
                return Err(BootstrapError::ConfirmationRequired { expected });
            }
        }

        let mut conn = MasterConn::connect(opts, instance.id.as_str()).await?;
        let before = match conn.introspect(&pending.desired, iam_auth_enabled).await {
            Ok(c) => c,
            Err(e) => {
                conn.close().await;
                return Err(e.into());
            }
        };

        let plan = match revalidate(
            &pending.desired,
            &before,
            instance.id.as_str(),
            pending.is_production,
            &pending.fingerprint,
            pending.user_existed,
            &pending.state_digest,
        ) {
            Ok(p) => p,
            Err(blockers) => {
                conn.close().await;
                // **차단도 감사 대상이다.** 대상 DB 를 읽었고, "왜 실행하지
                // 않았는가" 가 나중에 필요한 정보다 — 특히 T-28 시나리오
                // (계획 이후에 누가 계정을 만들었다)에서 그렇다.
                let record = AuditRecord {
                    event: AuditEvent::BootstrapApply,
                    actor: String::new(),
                    worker_id: String::new(),
                    at_ms: now_ms,
                    instance_id: instance.id.as_str().to_string(),
                    env: instance.env.effective,
                    credential_source: Some(credentials.source),
                    privilege_mode: pending.desired.mode.as_str(),
                    monitor_user: pending.desired.user.clone(),
                    monitor_host: pending.desired.host.clone(),
                    actions: vec![],
                    before: StateSnapshot::of(&before),
                    after: None,
                    result: AuditResult::Blocked,
                    confirmation_typed: pending.is_production,
                    blockers: audit::blocker_messages(&blockers),
                    excess_privileges: vec![],
                };
                return Ok(ApplyReport {
                    record,
                    result: Err(BootstrapError::Blocked(blockers)),
                });
            }
        };

        // ── 실행 직전 재확인 ──
        //
        // # 왜 한 번 더 읽는가 (교차 리뷰 2차가 잡은 경쟁 조건)
        //
        // 위의 `revalidate` 는 **한 번 읽은 스냅샷**을 검증한다. 그 사이에 대상 DB 에
        // `ALTER USER` 권한이 있는 내부자가 계정의 비밀번호를 자기가 아는 값으로
        // 바꾸면, 이어지는 `GRANT` 가 탈취된 계정에 적용된다 — T-28 의 잔여 창이다.
        //
        // 창을 0 으로 만들 수는 없다. MySQL 의 `GRANT` 는 트랜잭션이 아니고,
        // 계정 상태에 대한 compare-and-swap 이 없다. 그래서 **창을 좁힌다**:
        // 같은 연결에서 상태를 다시 읽고 지문을 비교한다. 남는 창은 그 쿼리 하나의
        // 폭(수십 마이크로초)이고, 공격자가 관측할 수 없는 순간이다.
        //
        // 이걸 각 문장 사이마다 하지 않는 이유: 쿼리가 두 배가 되는데 창은
        // 여전히 0 이 아니다. 비용/효과가 맞지 않는다. 실행 후 `after` 상태를 읽어
        // 감사에 남기므로, 사후에는 무엇이 달라졌는지 알 수 있다.
        let recheck = conn.introspect(&pending.desired, iam_auth_enabled).await;
        match recheck {
            Ok(now) if now.security_digest() == pending.state_digest => {}
            other => {
                conn.close().await;
                let found = match &other {
                    Ok(_) => "실행 직전에 계정 상태가 바뀌었다".to_string(),
                    Err(e) => format!("실행 직전 상태를 읽을 수 없다: {e}"),
                };
                let record = AuditRecord {
                    event: AuditEvent::BootstrapApply,
                    actor: String::new(),
                    worker_id: String::new(),
                    at_ms: now_ms,
                    instance_id: instance.id.as_str().to_string(),
                    env: instance.env.effective,
                    credential_source: Some(credentials.source),
                    privilege_mode: pending.desired.mode.as_str(),
                    monitor_user: pending.desired.user.clone(),
                    monitor_host: pending.desired.host.clone(),
                    actions: audit::actions_of(&plan, 0),
                    before: StateSnapshot::of(&before),
                    after: other.as_ref().ok().map(StateSnapshot::of),
                    result: AuditResult::Blocked,
                    confirmation_typed: pending.is_production,
                    blockers: vec![found.clone()],
                    excess_privileges: plan.excess.clone(),
                };
                return Ok(ApplyReport {
                    record,
                    result: Err(BootstrapError::Blocked(vec![Blocker::PlanStale {
                        expected: "계획 시점의 계정 상태".into(),
                        found,
                    }])),
                });
            }
        }

        // ── 실행 ──
        let statements: Vec<_> = plan.statements().cloned().collect();
        let total = statements.len();
        let mut executed = 0usize;
        let mut failure: Option<String> = None;
        for s in &statements {
            match conn.execute(s).await {
                Ok(()) => executed += 1,
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }

        // after 상태는 **실패해도 읽는다** — 반쪽 상태가 무엇인지 알아야 한다.
        let after_state = conn
            .introspect(&pending.desired, iam_auth_enabled)
            .await
            .ok();
        conn.close().await;

        let after = after_state
            .as_ref()
            .map(StateSnapshot::of)
            .unwrap_or_else(|| StateSnapshot::of(&before));

        let result = match &failure {
            None => AuditResult::Success,
            Some(reason) if executed == 0 => AuditResult::Failed {
                reason: reason.clone(),
            },
            Some(_) => AuditResult::Partial {
                completed: executed,
                total,
            },
        };

        let record = AuditRecord {
            event: AuditEvent::BootstrapApply,
            // 호출부(`BootstrapService`)가 요청자와 워커를 채운다.
            actor: String::new(),
            worker_id: String::new(),
            at_ms: now_ms,
            instance_id: instance.id.as_str().to_string(),
            env: instance.env.effective,
            credential_source: Some(credentials.source),
            privilege_mode: pending.desired.mode.as_str(),
            monitor_user: pending.desired.user.clone(),
            monitor_host: pending.desired.host.clone(),
            actions: audit::actions_of(&plan, executed),
            before: StateSnapshot::of(&before),
            after: Some(after.clone()),
            result,
            confirmation_typed: pending.is_production,
            blockers: vec![],
            excess_privileges: plan.excess.clone(),
        };

        // **레코드를 버리지 않는다.** 실패했으면 사유를 `result` 에 담아 함께 돌려준다.
        let result = match failure {
            Some(reason) => Err(BootstrapError::Domain(
                dbmon_core::error::DomainError::Unavailable {
                    dependency: "target-mysql",
                    reason: format!(
                        "{}: 문장 {}/{} 까지 실행하고 실패했다 — {reason}",
                        instance.id.as_str(),
                        executed,
                        total
                    ),
                },
            )),
            None => Ok(ApplyOutcome {
                instance_id: instance.id.as_str().to_string(),
                executed,
                total,
                after,
                credential_source: credentials.source,
            }),
        };
        Ok(ApplyReport { record, result })
    }

    /// 계획을 보관한다. 만료된 것을 먼저 치운다.
    fn store(&self, id: String, pending: Pending, now_ms: EpochMs) {
        let mut map = self.pending.lock().expect("plan lock");
        map.retain(|_, p| p.expires_at_ms > now_ms);
        // **상한을 넘으면 가장 먼저 만료될 것을 버린다.** 새 계획을 거부하면
        // 화면이 막히고, 무한히 쌓으면 메모리 경로가 된다.
        if map.len() >= MAX_PENDING_PLANS {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, p)| p.expires_at_ms)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(id, pending);
    }

    /// 계획을 꺼낸다. **한 번만 쓸 수 있다** — 같은 계획으로 두 번 실행하지 않는다.
    fn take(&self, id: &str, now_ms: EpochMs) -> Option<Pending> {
        let mut map = self.pending.lock().expect("plan lock");
        let p = map.remove(id)?;
        (p.expires_at_ms > now_ms).then_some(p)
    }

    /// 보관 중인 계획 수 (진단용).
    pub fn pending_count(&self, now_ms: EpochMs) -> usize {
        let map = self.pending.lock().expect("plan lock");
        map.values().filter(|p| p.expires_at_ms > now_ms).count()
    }
}

/// **이 인스턴스를 프로덕션으로 취급해야 하는가.**
///
/// # `effective` 만 보면 확인 절차를 우회할 수 있다
///
/// [`EnvResolution`] 은 태그에서 유도한 값(`from_tags`)과 사용자가 UI 에서 지정한 값
/// (`override_value`)을 함께 들고 있고, `effective` 는 오버라이드가 이긴다. 그래서
/// `effective == Prd` 만 보면 이렇게 된다:
///
/// ```text
/// 1. prd 인스턴스(태그가 prd)의 환경을 화면에서 dev 로 오버라이드한다
/// 2. effective = Dev → 타이핑 확인이 사라지고, 초과 권한이 차단에서 경고로 내려간다
/// 3. 프로덕션 DB 에 확인 없이 CREATE USER 가 실행된다
/// ```
///
/// 환경 오버라이드는 **화면 분류를 고치는 기능**이고 안전장치를 끄는 스위치가 아니다.
/// 그래서 둘 중 하나라도 prd 면 prd 로 본다 — 보수적인 방향으로만 틀린다.
pub fn treat_as_production(env: &dbmon_core::env::EnvResolution) -> bool {
    env.effective == Env::Prd || env.from_tags == Env::Prd
}

/// prd 확인에 타이핑할 값 — **인스턴스 식별자만** 이다.
///
/// 전체 `InstanceId`(계정/리전/식별자)를 타이핑하게 하면 아무도 읽지 않고 복사한다.
/// 복사·붙여넣기를 하면 확인 절차가 의미를 잃으므로 짧고 손으로 칠 수 있는 값을 쓴다.
pub fn short_identifier(instance: &Instance) -> String {
    instance
        .id
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or_else(|| instance.id.as_str())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::bootstrap::{AuthMethod, PrivilegeMode};

    /// 탐색 경로와 같은 방법으로 만든다 — 손으로 조립하면 필드가 늘 때 어긋난다.
    fn instance(env: Env, id: &str) -> Instance {
        use crate::aws::discovery::{RawDbInstance, to_instance};
        let raw = RawDbInstance {
            identifier: id.into(),
            engine: "mysql".into(),
            engine_version: "8.4.6".into(),
            region: "ap-northeast-2".into(),
            endpoint_address: Some(format!("{id}.abc123.ap-northeast-2.rds.amazonaws.com")),
            endpoint_port: Some(3306),
            ..Default::default()
        };
        let mut i = to_instance(
            &raw,
            "123456789012",
            &dbmon_core::env::EnvMapping::default(),
            0,
        )
        .expect("매핑");
        i.env = dbmon_core::env::EnvResolution::resolve(env, None);
        i
    }

    fn desired() -> Desired {
        Desired {
            user: "dbmon".into(),
            host: "10.1.%".into(),
            auth: AuthMethod::IamDbAuth,
            mode: PrivilegeMode::Broad,
            schemas: vec![],
        }
    }

    fn pending(instance_id: &str, expires: EpochMs) -> Pending {
        Pending {
            instance_id: instance_id.to_string(),
            desired: desired(),
            is_production: false,
            fingerprint: "fp".into(),
            user_existed: false,
            state_digest: "sd".into(),
            expires_at_ms: expires,
        }
    }

    /// **타이핑할 값은 짧은 식별자다.** 전체 ID 를 요구하면 아무도 읽지 않고 복사한다.
    #[test]
    fn the_confirmation_value_is_the_bare_identifier() {
        let i = instance(Env::Prd, "orders-prd-01");
        assert_eq!(short_identifier(&i), "orders-prd-01");
    }

    /// **환경 오버라이드로 prd 안전장치를 끌 수 없다.**
    ///
    /// 태그가 prd 인 인스턴스를 화면에서 dev 로 오버라이드하면 `effective` 는 dev 가
    /// 된다. `effective` 만 보면 그 순간 (a) 타이핑 확인이 사라지고 (b) 초과 권한이
    /// 차단에서 경고로 내려간다 — 프로덕션 DB 에 확인 없이 `CREATE USER` 가 나간다.
    #[test]
    fn an_env_override_cannot_disable_the_production_guard() {
        use dbmon_core::env::EnvResolution;

        // 태그는 prd, 오버라이드는 dev.
        let sneaky = EnvResolution::resolve(Env::Prd, Some(Env::Dev));
        assert_eq!(sneaky.effective, Env::Dev, "전제: effective 는 dev 다");
        assert!(
            treat_as_production(&sneaky),
            "오버라이드로 prd 안전장치가 꺼졌다"
        );

        // 반대 방향도 prd 로 본다 — 태그가 없는데 사람이 prd 라고 지정한 경우.
        let marked = EnvResolution::resolve(Env::Dev, Some(Env::Prd));
        assert!(treat_as_production(&marked));

        // 어느 쪽도 prd 가 아니면 prd 가 아니다.
        for e in [Env::Dev, Env::Stg, Env::Unknown] {
            assert!(
                !treat_as_production(&EnvResolution::resolve(e, None)),
                "{e} 를 prd 로 봤다"
            );
        }
    }

    /// prd 계획은 **확인 값을 응답에 담아** 화면이 무엇을 타이핑할지 알 수 있게 한다.
    #[test]
    fn a_production_plan_advertises_the_confirmation_value() {
        let i = instance(Env::Prd, "orders-prd-01");
        assert!(treat_as_production(&i.env));
        assert_eq!(short_identifier(&i), "orders-prd-01");
    }

    /// **다른 인스턴스의 plan_id 로 실행할 수 없다.**
    ///
    /// 계획에 인스턴스가 묶여 있지 않으면, dev 인스턴스로 만든 계획(확인 불필요)을
    /// prd 인스턴스에 적용하는 경로가 생긴다.
    #[test]
    fn a_plan_is_bound_to_its_instance() {
        let b = Bootstrapper::new();
        b.store(
            "p1".into(),
            pending("123456789012/ap-northeast-2/dev-01", 10_000),
            0,
        );
        let taken = b.take("p1", 0).expect("보관됨");
        assert_eq!(taken.instance_id, "123456789012/ap-northeast-2/dev-01");
        // `apply` 가 이 값을 대상 인스턴스와 비교한다 (같지 않으면 PlanNotFound).
    }

    /// **차단됐을 때도 감사 레코드가 나온다.**
    ///
    /// 첫 구현은 `Err(Blocked)` 만 돌려주고 레코드를 만들지 않았다. 그러면 T-28
    /// 시나리오(계획 이후에 누가 계정을 만들었다)가 감사 로그에 남지 않는다 —
    /// 그게 가장 남아야 하는 사건이다.
    ///
    /// 여기서는 타입으로 그 성질을 확인한다: `ApplyReport` 는 `record` 를
    /// `Option` 이 아닌 값으로 갖는다.
    #[test]
    fn the_apply_report_always_carries_an_audit_record() {
        fn assert_record_is_not_optional(r: &ApplyReport) -> &AuditRecord {
            &r.record
        }
        let record = AuditRecord {
            event: AuditEvent::BootstrapApply,
            actor: "t".into(),
            worker_id: "w".into(),
            at_ms: 1,
            instance_id: "i".into(),
            env: Env::Dev,
            credential_source: None,
            privilege_mode: "broad",
            monitor_user: "dbmon".into(),
            monitor_host: "10.1.%".into(),
            actions: vec![],
            before: StateSnapshot::of(&dbmon_core::bootstrap::CurrentState::default()),
            after: None,
            result: AuditResult::Blocked,
            confirmation_typed: false,
            blockers: vec!["막혔다".into()],
            excess_privileges: vec![],
        };
        let report = ApplyReport {
            record,
            result: Err(BootstrapError::Blocked(vec![Blocker::SslNotRequired])),
        };
        assert_eq!(assert_record_is_not_optional(&report).blockers.len(), 1);
        assert!(report.result.is_err());
    }

    /// 계획은 **한 번만** 쓸 수 있다.
    #[test]
    fn a_plan_can_only_be_taken_once() {
        let b = Bootstrapper::new();
        b.store("p1".into(), pending("i", 10_000), 0);
        assert!(b.take("p1", 1_000).is_some());
        assert!(b.take("p1", 1_000).is_none(), "두 번 실행할 수 있다");
    }

    /// 만료된 계획은 꺼낼 수 없다 (07 §2.7 — 5분).
    #[test]
    fn an_expired_plan_cannot_be_taken() {
        let b = Bootstrapper::new();
        b.store("p1".into(), pending("i", 5_000), 0);
        assert!(b.take("p1", 5_001).is_none());
        assert_eq!(PLAN_TTL_MS, 300_000);
    }

    #[test]
    fn expired_plans_are_swept_when_new_ones_arrive() {
        let b = Bootstrapper::new();
        b.store("old".into(), pending("i", 1_000), 0);
        b.store("new".into(), pending("i", 999_000), 2_000);
        assert_eq!(b.pending_count(2_000), 1);
        assert!(b.take("old", 2_000).is_none());
    }

    /// **보관 수에 상한이 있다.** 없으면 계획 생성이 메모리 증가 경로가 된다.
    #[test]
    fn pending_plans_are_capped() {
        let b = Bootstrapper::new();
        for n in 0..(MAX_PENDING_PLANS + 20) {
            // 만료 시각을 다르게 둬서 "가장 먼저 만료될 것" 이 결정적이게 한다.
            b.store(format!("p{n}"), pending("i", 100_000 + n as i64), 0);
        }
        assert!(
            b.pending_count(0) <= MAX_PENDING_PLANS,
            "상한을 넘었다: {}",
            b.pending_count(0)
        );
        // 가장 최근 것은 남아 있어야 한다.
        assert!(b.take(&format!("p{}", MAX_PENDING_PLANS + 19), 0).is_some());
    }
}
