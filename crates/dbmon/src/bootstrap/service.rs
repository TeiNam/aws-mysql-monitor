//! 부트스트랩 서비스 — API 가 부르는 조립 지점.
//!
//! # 무엇을 조립하는가
//!
//! | 조각 | 출처 |
//! |---|---|
//! | 마스터 자격증명 | [`super::secret::MasterSecretFetcher`] — 경로 a 우선, 없으면 b |
//! | IAM DB 인증 상태 | `DescribeDBInstances` / `DescribeDBClusters` |
//! | 접속 옵션 | [`crate::mysql::connect::target_opts`] (TLS·CA 검증 포함) |
//! | 판정 | [`dbmon_core::bootstrap`] |
//! | 감사 | [`super::audit`] → `dbmon-config` |
//!
//! # 왜 `Option<Arc<BootstrapService>>` 인가
//!
//! 이 서비스는 Secrets Manager 자격증명을 요구한다. 없는 배포(로컬 도커, 권한을 주지
//! 않은 조직)에서는 `None` 이고, API 가 422 로 거부하면서 **수동 스크립트 경로를
//! 안내**한다. 스텁을 만들어 통과시키면 있는 것처럼 보이면서 없는 상태가 된다 —
//! 이 프로젝트가 Cognito 에서 내린 것과 같은 판단이다.

use std::sync::Arc;

use aws_sdk_rds::Client as RdsClient;
use aws_sdk_secretsmanager::Client as SecretsClient;
use dbmon_core::bootstrap::Desired;
use dbmon_core::env::Env;
use dbmon_core::instance::Instance;
use dbmon_core::time::{Clock as _, SystemClock};

use super::audit::{AuditRecord, audit_pk, audit_sk};
use super::run::{ApplyOutcome, BootstrapError, Bootstrapper, PlanOutcome};
use super::secret::{MasterCredentials, MasterSecretFetcher, SecretError};

/// 감사 레코드를 쓰는 곳.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync {
    async fn write(
        &self,
        pk: &str,
        sk: &str,
        record: &AuditRecord,
    ) -> dbmon_core::error::Result<()>;
}

/// 한 리전의 클라이언트 묶음.
struct RegionClients {
    rds: RdsClient,
    fetcher: MasterSecretFetcher,
}

/// **리전별로 클라이언트를 갖는다.**
///
/// # 왜 홈 리전 하나로 안 되는가
///
/// `DescribeDBInstances` 와 `GetSecretValue` 는 **리전 API** 다. 홈 리전 클라이언트로
/// 타 리전 인스턴스를 조회하면 "찾을 수 없다" 가 되고, 더 나쁜 경우는 **같은 식별자가
/// 홈 리전에 있을 때** 다 — 다른 DB 의 사실로 계획을 만든다.
///
/// 이 프로젝트는 슬로우로그 페처에서 정확히 그 결함을 겪었다
/// (`build_slowlog_fetcher` 의 주석). 같은 실수를 반복하지 않는다.
pub struct BootstrapService {
    by_region: std::collections::BTreeMap<String, RegionClients>,
    settings: Arc<crate::settings_state::SettingsState>,
    audit: Arc<dyn AuditSink>,
    planner: Bootstrapper,
    deployment_env: Env,
    /// 감사 레코드의 주체. Cognito 가 붙으면 요청의 `sub` 로 바뀐다.
    ///
    /// 지금은 워커 식별자를 쓴다 — 공유 토큰 배포에서는 주체가 하나뿐이고, 그
    /// 사실을 감사 로그가 정직하게 말해야 한다.
    actor: String,
}

impl BootstrapService {
    /// 리전마다 클라이언트를 만든다.
    ///
    /// `regions` 는 탐색 대상 리전 목록이다. 목록에 없는 리전의 인스턴스는 부트스트랩
    /// 대상이 아니므로 [`BootstrapError::Invalid`] 가 된다 — 그 사실을 조용히
    /// 홈 리전으로 대체하지 않는다.
    pub async fn build(
        regions: &[String],
        settings: Arc<crate::settings_state::SettingsState>,
        audit: Arc<dyn AuditSink>,
        deployment_env: Env,
        actor: impl Into<String>,
    ) -> Self {
        use aws_config::BehaviorVersion;
        let mut by_region = std::collections::BTreeMap::new();
        for region in regions {
            let sdk = aws_config::defaults(BehaviorVersion::latest())
                .region(aws_config::Region::new(region.clone()))
                .load()
                .await;
            by_region.insert(
                region.clone(),
                RegionClients {
                    rds: RdsClient::new(&sdk),
                    fetcher: MasterSecretFetcher::new(SecretsClient::new(&sdk)),
                },
            );
        }
        Self {
            by_region,
            settings,
            audit,
            planner: Bootstrapper::new(),
            deployment_env,
            actor: actor.into(),
        }
    }

    /// 이 인스턴스의 리전 클라이언트. 없으면 오류다.
    fn clients(&self, instance: &Instance) -> Result<&RegionClients, BootstrapError> {
        let region = instance.id.region();
        self.by_region.get(region).ok_or_else(|| {
            BootstrapError::Invalid(format!(
                "{region} 은 이 워커의 대상 리전이 아니다 — aws.target_regions 를 확인한다"
            ))
        })
    }

    /// 이 인스턴스의 자격증명 경로 — 화면이 버튼을 켤 근거.
    ///
    /// **자격증명을 실제로 읽지 않는다.** 경로가 있는지만 본다. 읽으면 화면을 여는
    /// 것만으로 `GetSecretValue` 가 호출되고, 그건 CloudTrail 에 남는 동작이다.
    pub async fn credential_route(&self, instance: &Instance) -> Result<&'static str, SecretError> {
        let managed = self.managed_secret(instance).await;
        if managed.is_some() {
            return Ok("rds_managed_secret");
        }
        let settings = self.settings.load(SystemClock.now_ms()).await;
        if settings
            .bootstrap
            .secret_arn_for(instance.id.as_str())
            .is_some()
        {
            return Ok("tagged_secret");
        }
        Err(SecretError::NoSource {
            instance_id: instance.id.as_str().to_string(),
        })
    }

    /// 계획을 만든다.
    pub async fn plan(
        &self,
        instance: &Instance,
        desired: &Desired,
    ) -> Result<PlanOutcome, BootstrapError> {
        let facts = self.instance_facts(instance).await?;
        let credentials = self.credentials(instance, &facts).await?;
        let opts = self.connect_opts(instance, &credentials)?;
        let now_ms = SystemClock.now_ms();
        let plan_id = mint_plan_id()?;

        let (outcome, mut record) = self
            .planner
            .plan(
                instance,
                desired,
                &credentials,
                opts,
                facts.iam_auth_enabled,
                now_ms,
                &plan_id,
            )
            .await?;

        record.actor = self.actor.clone();
        self.record(&record).await;
        Ok(outcome)
    }

    /// 실행한다.
    pub async fn apply(
        &self,
        instance: &Instance,
        plan_id: &str,
        confirmation: Option<&str>,
    ) -> Result<ApplyOutcome, BootstrapError> {
        let facts = self.instance_facts(instance).await?;
        let credentials = self.credentials(instance, &facts).await?;
        let opts = self.connect_opts(instance, &credentials)?;
        let now_ms = SystemClock.now_ms();

        let result = self
            .planner
            .apply(
                plan_id,
                instance,
                &credentials,
                opts,
                facts.iam_auth_enabled,
                confirmation,
                now_ms,
            )
            .await;

        match result {
            Ok((outcome, mut record)) => {
                record.actor = self.actor.clone();
                self.record(&record).await;
                Ok(outcome)
            }
            Err(e) => Err(e),
        }
    }

    /// `DescribeDB*` 에서 오는 사실 — IAM 인증 상태와 관리형 시크릿.
    async fn instance_facts(&self, instance: &Instance) -> Result<InstanceFacts, BootstrapError> {
        let clients = self.clients(instance)?;
        // Aurora 멤버는 **클러스터**가 판정 단위다 — IAM 인증도, 마스터 시크릿도
        // 클러스터에 붙어 있다. 인스턴스만 보면 둘 다 못 찾는다(실제로 겪었다:
        // `rds-db:connect` 도 클러스터 리소스 id 를 요구한다).
        if let Some(cluster) = instance.cluster_id.as_ref() {
            let out = clients
                .rds
                .describe_db_clusters()
                .db_cluster_identifier(cluster.as_str())
                .send()
                .await
                .map_err(|e| {
                    BootstrapError::Domain(dbmon_core::error::DomainError::Unavailable {
                        dependency: "rds",
                        reason: format!("DescribeDBClusters({}) 실패: {e}", cluster.as_str()),
                    })
                })?;
            let c = out.db_clusters().first().ok_or_else(|| {
                BootstrapError::Invalid(format!("클러스터를 찾을 수 없다: {}", cluster.as_str()))
            })?;
            return Ok(InstanceFacts {
                iam_auth_enabled: c.iam_database_authentication_enabled().unwrap_or(false),
                managed_secret: c.master_user_secret().and_then(|s| {
                    Some((s.secret_arn()?.to_string(), s.secret_status()?.to_string()))
                }),
            });
        }

        let out = clients
            .rds
            .describe_db_instances()
            .db_instance_identifier(instance.id.identifier())
            .send()
            .await
            .map_err(|e| {
                BootstrapError::Domain(dbmon_core::error::DomainError::Unavailable {
                    dependency: "rds",
                    reason: format!(
                        "DescribeDBInstances({}) 실패: {e}",
                        instance.id.identifier()
                    ),
                })
            })?;
        let i = out.db_instances().first().ok_or_else(|| {
            BootstrapError::Invalid(format!("인스턴스를 찾을 수 없다: {}", instance.id.as_str()))
        })?;
        Ok(InstanceFacts {
            iam_auth_enabled: i.iam_database_authentication_enabled().unwrap_or(false),
            managed_secret: i
                .master_user_secret()
                .and_then(|s| Some((s.secret_arn()?.to_string(), s.secret_status()?.to_string()))),
        })
    }

    /// 관리형 시크릿만 확인한다 (자격증명을 읽지 않는다).
    async fn managed_secret(&self, instance: &Instance) -> Option<(String, String)> {
        self.instance_facts(instance).await.ok()?.managed_secret
    }

    /// 마스터 자격증명 — **경로 a 우선, 없으면 b.**
    async fn credentials(
        &self,
        instance: &Instance,
        facts: &InstanceFacts,
    ) -> Result<MasterCredentials, BootstrapError> {
        if let Some((arn, status)) = facts.managed_secret.as_ref() {
            return Ok(self
                .clients(instance)?
                .fetcher
                .from_rds_managed(arn, status)
                .await?);
        }
        let settings = self.settings.load(SystemClock.now_ms()).await;
        let arn = settings
            .bootstrap
            .secret_arn_for(instance.id.as_str())
            .ok_or_else(|| {
                BootstrapError::Credentials(SecretError::NoSource {
                    instance_id: instance.id.as_str().to_string(),
                })
            })?;
        Ok(self.clients(instance)?.fetcher.from_tagged_arn(arn).await?)
    }

    /// 접속 옵션 — 수집 경로와 **같은 함수**를 쓴다.
    ///
    /// 마스터 연결이 TLS·CA 검증·엔드포인트 형태 검사를 우회하면 안 된다. 별도
    /// 조립을 만들면 그 검사가 한쪽에만 남는다.
    fn connect_opts(
        &self,
        instance: &Instance,
        credentials: &MasterCredentials,
    ) -> Result<mysql_async::Opts, BootstrapError> {
        crate::mysql::connect::target_opts(
            instance,
            &credentials.username,
            credentials.password.expose(),
            self.deployment_env,
            None,
        )
        .map_err(BootstrapError::Domain)
    }

    /// 감사 레코드를 쓴다. **실패해도 부트스트랩을 되돌리지 않는다.**
    ///
    /// 이미 대상 DB 는 바뀌었다. 감사 실패로 오류를 내면 화면은 "실패" 로 보는데
    /// 실제로는 계정이 만들어져 있다 — 그게 더 나쁜 상태다. 대신 경고 로그를 남긴다.
    async fn record(&self, record: &AuditRecord) {
        let pk = audit_pk(record.at_ms);
        let sk = audit_sk(record.at_ms, &record.instance_id);
        if let Err(e) = self.audit.write(&pk, &sk, record).await {
            tracing::warn!(
                error = %e,
                instance = %record.instance_id,
                "부트스트랩 감사 레코드를 쓰지 못했다 — 동작은 이미 수행됐다"
            );
        }
    }
}

/// `DescribeDB*` 에서 온 사실.
struct InstanceFacts {
    iam_auth_enabled: bool,
    /// `(ARN, SecretStatus)`.
    managed_secret: Option<(String, String)>,
}

/// `plan_id` 를 만든다 — 128비트 무작위.
///
/// # 왜 무작위여야 하는가
///
/// `plan_id` 는 **실행 권한을 담은 값**이다(계획을 승인한 사람만 안다). 순번이면
/// 다른 사람의 계획을 실행할 수 있다.
///
/// 엔트로피를 못 읽으면 시각 기반으로 떨어지지 않고 **오류를 낸다.** 예측 가능한
/// 식별자는 없는 것보다 나쁘다 — 같은 판단이 `mint_dev_token` 에 이미 있다.
/// 요청 경로이므로 패닉하지 않고 503 으로 거부한다.
fn mint_plan_id() -> Result<String, BootstrapError> {
    crate::entropy::hex128().ok_or_else(|| {
        BootstrapError::Domain(dbmon_core::error::DomainError::Unavailable {
            dependency: "os-entropy",
            reason: "OS 엔트로피를 읽을 수 없어 plan_id 를 만들지 못했다".into(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `plan_id` 는 **무작위이고 매번 다르다.**
    ///
    /// 순번이면 다른 사람이 승인한 계획을 실행할 수 있다.
    #[test]
    fn plan_ids_are_random_and_unique() {
        let a = mint_plan_id().expect("엔트로피");
        let b = mint_plan_id().expect("엔트로피");
        assert_eq!(a.len(), 32, "128비트를 16진수로");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
