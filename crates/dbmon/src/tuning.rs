//! 튜닝 권고 생성 — 컨텍스트 수집 → Bedrock → 검증 → 저장.
//!
//! # 사람이 누를 때만 돈다
//!
//! 이 경로는 대상 DB 에 쿼리 12회(테이블 10개 기준)를 던지고 모델을 부른다. 자동으로
//! 돌리면 청구서와 대상 DB 부하가 함께 는다([11 §3.4]). 그래서 버튼 하나에만 매달려
//! 있고, 결과를 저장해 두 번째 사람이 같은 것을 다시 만들지 않게 한다.
//!
//! # 실패를 단계별로 구분한다
//!
//! | 단계 | 실패하면 |
//! |---|---|
//! | 설정이 꺼져 있다 | 422 `unsupported` — 화면이 설정으로 안내한다 |
//! | 대상 DB 접속·조회 | **계속 간다.** 스키마 없이 플랜만으로 분석하고 그 사실을 권고에 적는다 |
//! | 모델 호출 | 503 — 모델 ID·리전·권한 문제다. 사유를 스크럽해 로그에 남긴다 |
//! | 응답 형식 | 1회 재시도 후 502. **빈 권고를 만들지 않는다** |
//!
//! 두 번째 줄이 중요하다: 대상 DB 가 프라이빗 서브넷에 있어 API 워커에서 안 닿는 배포도
//! 있다(피어링 전 크로스 계정). 거기서 기능이 통째로 죽는 것보다 "플랜만으로 분석했다" 가
//! 낫고, 화면이 그 한계를 표시한다.

use std::sync::Arc;

use dbmon_core::error::{DomainError, Result};
use dbmon_core::ids::RecordId;
use dbmon_core::instance::Instance;
use dbmon_core::ports::InstanceRegistry;
use dbmon_core::settings::AiSettings;
use dbmon_core::slow_query::SlowQuery;
use dbmon_core::time::EpochMs;
use dbmon_core::tuning::{
    self, MAX_TABLES, QualifiedTable, RawAdvice, TableSpec, TuningAdvice, TuningContext,
};

use crate::aws::auth_token::TargetAuth;
use crate::aws::bedrock::BedrockClient;
use crate::config::Config;
use crate::mysql::TargetMysql;
use crate::store::tuning::DynamoTuningStore;

/// 형식 위반 재시도 횟수. **1회다** — 두 번 실패하면 프롬프트나 모델의 문제이고,
/// 계속 시도하면 토큰만 쓴다.
const FORMAT_RETRIES: usize = 1;

/// **요청 전체** 상한.
///
/// # 시도별이 아니라 전체다 (3차 교차 리뷰)
///
/// 처음엔 시도당 120초로 뒀는데, 형식 위반 재시도가 붙으면 모델에만 238초가 되고
/// 앞단(설정·스키마 조회·SDK 구성)까지 합치면 **ALB 유휴 타임아웃 300초를 넘긴다** —
/// 그러면 화면은 504 를 받고, 504 는 아무것도 말해 주지 않는다.
///
/// 그래서 데드라인을 하나 두고 각 시도에 **남은 시간만** 준다. 실측은 30초이므로
/// 정상 경로는 이 상한에 닿지 않는다.
const TUNING_DEADLINE: std::time::Duration = std::time::Duration::from_secs(150);

/// **요청 경로 전체**의 상한. API 핸들러가 이 값으로 감싼다.
///
/// 모델 데드라인([`TUNING_DEADLINE`])보다 조금 크다 — 앞뒤의 스키마 조회·저장에
/// 여유를 주되 ALB 유휴 타임아웃(300초)보다는 넉넉히 작아야 한다.
pub const REQUEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(200);

pub struct TuningService {
    pub registry: Arc<crate::store::registry::DynamoInstanceRegistry>,
    pub advice: Arc<DynamoTuningStore>,
    pub settings: Arc<crate::settings_state::SettingsState>,
    pub auth: Arc<TargetAuth>,
    pub config: Arc<Config>,
}

/// 생성 결과. 어디까지 갔는지 화면이 알아야 한다.
#[derive(Debug, Clone)]
pub struct Generated {
    pub advice: TuningAdvice,
    pub input_tokens: i32,
    pub output_tokens: i32,
}

impl TuningService {
    /// 저장된 권고. 없으면 `None`.
    pub async fn stored(&self, record_id: &RecordId) -> Result<Option<TuningAdvice>> {
        self.advice.get(record_id).await
    }

    /// 새로 만든다. **호출부가 이미 권한·환경 스코프를 확인한 레코드를 넘긴다.**
    ///
    /// 전체가 [`TUNING_DEADLINE`] 안에 끝난다 — 스키마 조회와 모델 호출이 그 예산을
    /// 나눠 쓴다.
    pub async fn generate(&self, record: &SlowQuery, now_ms: EpochMs) -> Result<Generated> {
        let deadline = tokio::time::Instant::now() + TUNING_DEADLINE;
        let settings = self.settings.load(now_ms).await;
        let ai = settings.ai;
        if !ai.enabled {
            return Err(DomainError::Unsupported {
                what: "ai_tuning".into(),
                reason: "설정에서 AI 튜닝이 꺼져 있다".into(),
            });
        }
        if ai.model_id.trim().is_empty() {
            return Err(DomainError::Unsupported {
                what: "ai_tuning".into(),
                reason: "모델 ID 가 설정되지 않았다".into(),
            });
        }

        let instance = self.registry.get(&record.instance_id).await?;
        let tables = self.collect_specs(record, instance.as_ref()).await;
        let context = build_context(record, instance.as_ref(), tables);

        let client = self.bedrock(&ai).await?;
        let prompt = tuning::build_prompt(&context);
        let advice = self
            .ask(&client, &ai, &prompt, &context, now_ms, deadline)
            .await?;

        self.advice.put(&record.record_id, &advice.advice).await?;
        Ok(advice)
    }

    /// 스키마 명세. **실패는 빈 목록이다** — 그 사실은 검증이 권고에 적는다.
    async fn collect_specs(
        &self,
        record: &SlowQuery,
        instance: Option<&Instance>,
    ) -> Vec<TableSpec> {
        let Some(instance) = instance else {
            tracing::warn!(
                instance = %record.instance_id.as_str(),
                "등록부에 없다 — 스키마 없이 분석한다"
            );
            return Vec::new();
        };
        let wanted = wanted_tables(record);
        if wanted.is_empty() {
            return Vec::new();
        }
        match self.connect(instance).await {
            Ok(db) => match db.table_specs(&wanted).await {
                Ok(specs) => specs,
                Err(e) => {
                    tracing::warn!(
                        instance = %instance.id.as_str(),
                        error = %crate::telemetry::Scrubbed(&e),
                        "스키마 명세 조회 실패 — 플랜만으로 분석한다"
                    );
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::warn!(
                    instance = %instance.id.as_str(),
                    error = %crate::telemetry::Scrubbed(&e),
                    "대상 DB 에 접속하지 못했다 — 플랜만으로 분석한다"
                );
                Vec::new()
            }
        }
    }

    /// 대상 DB 연결. 수집 태스크와 **같은 경로**를 쓴다(IAM 토큰 + 터널 오버라이드).
    async fn connect(&self, instance: &Instance) -> Result<TargetMysql> {
        let db_user = self.config.collector.monitor_db_user.clone();
        let host = instance
            .endpoint
            .clone()
            .ok_or_else(|| DomainError::InvalidInput {
                field: "endpoint".into(),
                reason: "엔드포인트가 없다".into(),
            })?;
        let provider = self
            .auth
            .for_instance(&instance.id, &self.config.aws.account_id)
            .ok_or_else(|| DomainError::Unavailable {
                dependency: "target-auth",
                // **다른 리전·계정 공급자로 대신하지 않는다** — 서명이 틀린 토큰은 IAM
                // 정책 오류처럼 보여 추적이 오래 걸린다.
                reason: format!(
                    "{}/{} 에 쓸 인증 공급자가 없다",
                    instance.id.account(),
                    instance.id.region()
                ),
            })?;
        let secret = provider.token(&host, instance.port, &db_user).await?;
        let via = self.config.collector.tunnel_for(instance.id.identifier());
        let opts = crate::mysql::connect::target_opts(
            instance,
            &db_user,
            secret.expose(),
            self.config.deployment_env,
            via,
        )?;
        TargetMysql::from_config(
            opts,
            &self.config.collector,
            format!("tuning:{}", instance.id.identifier()),
        )
    }

    async fn bedrock(&self, ai: &AiSettings) -> Result<BedrockClient> {
        use aws_config::BehaviorVersion;
        // 리전을 비워 두면 배포 리전이다. **모델이 없는 리전을 부르면 400** 이므로
        // 설정으로 바꿀 수 있어야 한다(교차 리전 프로파일은 `global.` 접두어를 쓴다).
        let region = if ai.region.trim().is_empty() {
            self.config.aws.region.clone()
        } else {
            ai.region.clone()
        };
        let sdk = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new(region.clone()))
            .load()
            .await;
        Ok(BedrockClient::new(
            aws_sdk_bedrockruntime::Client::new(&sdk),
            region,
        ))
    }

    /// 모델을 부르고 형식을 검증한다. 형식 위반은 [`FORMAT_RETRIES`]회 재시도.
    async fn ask(
        &self,
        client: &BedrockClient,
        ai: &AiSettings,
        prompt: &str,
        context: &TuningContext,
        now_ms: EpochMs,
        deadline: tokio::time::Instant,
    ) -> Result<Generated> {
        let mut last_error = String::new();
        for attempt in 0..=FORMAT_RETRIES {
            let call = client.converse(
                &ai.model_id,
                tuning::SYSTEM_PROMPT,
                prompt,
                ai.max_output_tokens,
            );
            // **남은 예산만 준다.** 시도마다 상한을 새로 주면 재시도가 붙을 때 전체가
            // ALB 유휴 타임아웃을 넘긴다.
            let reply = match tokio::time::timeout_at(deadline, call).await {
                Ok(r) => r?,
                Err(_) => {
                    return Err(DomainError::Unavailable {
                        dependency: "bedrock",
                        reason: format!(
                            "모델이 {}초 안에 응답하지 않았다 — 출력 상한을 줄이거나 다른 모델을 쓴다",
                            TUNING_DEADLINE.as_secs()
                        ),
                    });
                }
            };

            let raw = tuning::extract_json(&reply.text)
                .ok_or_else(|| "응답에 JSON 객체가 없다".to_string())
                .and_then(|json| {
                    serde_json::from_str::<RawAdvice>(json)
                        .map_err(|e| format!("JSON 파싱 실패: {e}"))
                });

            match raw {
                Ok(raw) => {
                    let advice =
                        tuning::validate(raw, context, &ai.model_id, now_ms).map_err(|p| {
                            DomainError::Unavailable {
                                dependency: "bedrock",
                                reason: format!("모델 응답이 쓸 수 없다: {p:?}"),
                            }
                        })?;
                    tracing::info!(
                        model = %ai.model_id,
                        region = %client.region(),
                        input_tokens = reply.input_tokens,
                        output_tokens = reply.output_tokens,
                        tables = advice.tables_analyzed,
                        indexes = advice.indexes.len(),
                        attempt,
                        "튜닝 권고 생성"
                    );
                    return Ok(Generated {
                        advice,
                        input_tokens: reply.input_tokens,
                        output_tokens: reply.output_tokens,
                    });
                }
                Err(reason) => {
                    // **잘렸는지 구분한다.** 출력 상한이 원인이면 사용자가 고칠 수 있다.
                    last_error = if reply.truncated {
                        format!(
                            "{reason} (출력 토큰 상한 {} 에 걸려 잘렸다)",
                            ai.max_output_tokens
                        )
                    } else {
                        reason
                    };
                    tracing::warn!(attempt, reason = %last_error, "모델 응답 형식 위반 — 재시도");
                }
            }
        }
        Err(DomainError::Unavailable {
            dependency: "bedrock",
            reason: last_error,
        })
    }
}

/// 분석할 테이블. 플랜의 참조 목록을 **SQL 의 별칭 표로 풀어서** 만든다.
///
/// # 플랜에는 별칭만 있다
///
/// `EXPLAIN FORMAT=JSON` 의 `table_name` 은 별칭이고, `attached_condition` 의 3단 참조조차
/// `스키마 . 별칭 . 컬럼` 이다(실측). 그래서 `referenced_tables` 가 `["shop.a", "shop.c"]`
/// 로 저장돼 있었고, `information_schema` 에서 `shop.a` 를 찾다 못 찾아 **별칭을 쓴 쿼리는
/// 전부 스키마·인덱스·카디널리티 없이 분석됐다.** 실제 SQL 은 대부분 별칭을 쓴다.
///
/// 조용히 degrade 되는 것이 이 결함의 성질이었다 — 모델이 SQL 원문과 플랜의 인덱스 이름
/// 으로 그럴듯한 권고를 만들어 냈고, 화면은 "스키마를 못 가져왔다" 만 작게 적었다.
///
/// # SQL 이 없으면 예전처럼 동작한다
///
/// 리터럴 정책이 `off` 면 `sql_text` 가 없다. 그때는 플랜 이름을 그대로 쓴다 — 별칭이면
/// 명세가 비고 그 사실이 권고에 적힌다(지금까지의 동작).
fn wanted_tables(record: &SlowQuery) -> Vec<QualifiedTable> {
    let mut out = dbmon_core::tuning::resolved_tables(record);
    // 토큰이 선형으로 늘기 때문에 상한을 둔다. 걸리면 `tables_truncated` 로 말한다.
    out.truncate(MAX_TABLES);
    out
}

/// 모델에 보낼 SQL. **저장된 원문을 그대로 보내지 않는다** (FR-AI-04).
///
/// 리터럴 정책이 `full`·`full_restricted` 면 `sql_text` 에 운영 데이터가 들어 있다.
/// 그건 조직 밖(Bedrock)으로 나가면 안 되는 값이므로 **여기서 다시 정규화한다** —
/// 정책이 `masked` 인 배포에서는 이미 정규 텍스트라 이 호출이 사실상 무해하다.
///
/// 반환값의 두 번째 항목은 "리터럴이 남아 있을 수 있는가" 다. 마스킹 검사가 통과하지
/// 못하면(인용부호 미종료 등) **SQL 을 통째로 버린다** — 리터럴을 흘리는 것보다
/// 플랜만으로 분석하는 편이 낫다.
fn model_safe_sql(record: &SlowQuery) -> (String, bool) {
    let Some(raw) = record.sql_text.as_deref() else {
        return (String::new(), false);
    };
    let normalized = dbmon_normalize::normalize(raw);
    if normalized.unterminated_quote || normalized.check_no_literals().is_err() {
        tracing::warn!(
            record = %record.record_id.as_str(),
            "SQL 을 안전하게 마스킹할 수 없다 — 모델에 보내지 않는다"
        );
        return (String::new(), false);
    }
    // **힌트·주석을 지운다.** 정규화는 리터럴만 바꾸고 `/*+ QB_NAME(…) */` 안은
    // 그대로 두므로, 그 이름에 비밀이나 지시문이 있으면 모델로 나간다.
    (
        tuning::strip_hints_and_comments(&normalized.canonical),
        false,
    )
}

fn build_context(
    record: &SlowQuery,
    instance: Option<&Instance>,
    tables: Vec<TableSpec>,
) -> TuningContext {
    let (sql, has_literals) = model_safe_sql(record);
    let wanted = wanted_tables(record).len();
    TuningContext {
        sql,
        sql_has_literals: has_literals,
        statement_type: format!("{:?}", record.statement_type).to_lowercase(),
        schema_name: record.schema_name.clone(),
        plan_json: record.plan.normalized_json.clone(),
        plan_tree: record.plan.tree_text.clone(),
        duration_ms: record.duration_ms,
        rows_examined: record.stats.rows_examined,
        rows_sent: record.stats.rows_sent,
        lock_time_ms: record.stats.lock_time_ms,
        engine: instance
            .map(|i| format!("{:?}", i.engine).to_lowercase())
            .unwrap_or_else(|| "mysql".into()),
        engine_version: instance
            .map(|i| i.engine_version.raw.clone())
            .unwrap_or_default(),
        tables,
        tables_truncated: record.plan.referenced_tables.len() > wanted.max(MAX_TABLES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::slow_query::PlanBundle;

    fn record(tables: &[&str], schema: Option<&str>) -> SlowQuery {
        // 저장소 테스트의 표본을 재사용한다 — 필드가 서른 개라 손으로 짜면 새 필드가
        // 추가될 때마다 이 파일이 깨진다.
        let mut q = crate::store::tests::sample();
        q.schema_name = schema.map(str::to_string);
        q.plan = PlanBundle {
            referenced_tables: tables.iter().map(|t| (*t).to_string()).collect(),
            ..Default::default()
        };
        q
    }

    /// 스키마 한정자가 없으면 **실행 당시의 기본 스키마**를 쓴다. 그것도 없으면
    /// 조회할 수 없으므로 버린다(`information_schema` 는 스키마를 요구한다).
    #[test]
    fn unqualified_tables_use_the_session_schema() {
        let with = wanted_tables(&record(&["orders", "shop.users"], Some("shop")));
        assert_eq!(
            with,
            vec![
                QualifiedTable::new("shop", "orders"),
                QualifiedTable::new("shop", "users")
            ]
        );

        let without = wanted_tables(&record(&["orders"], None));
        assert!(
            without.is_empty(),
            "스키마를 알 수 없는 테이블을 조회 대상에 넣었다"
        );
    }

    /// **플랜이 준 별칭을 실제 테이블로 옮긴다.**
    ///
    /// 이게 없으면 `information_schema` 에서 `shop.a` 를 찾다 못 찾고, 별칭을 쓴 쿼리는
    /// 전부 스키마·인덱스·카디널리티 **없이** 분석된다. 조용히 degrade 되는 것이 이
    /// 결함의 성질이었다 — 모델이 SQL 원문과 플랜의 인덱스 이름으로 그럴듯한 권고를
    /// 만들어 냈다(실배포에서 그렇게 돌고 있었다).
    #[test]
    fn plan_aliases_resolve_to_the_real_tables() {
        let mut q = record(&["shop.a", "shop.c"], Some("shop"));
        q.sql_text = Some(
            "SELECT count ( * ) FROM order_items a \
             STRAIGHT_JOIN customers c ON c . id % ? = a . id % ?"
                .into(),
        );
        assert_eq!(
            wanted_tables(&q),
            vec![
                QualifiedTable::new("shop", "order_items"),
                QualifiedTable::new("shop", "customers"),
            ]
        );
    }

    /// SQL 이 스키마를 명시하면 **그쪽을 쓴다.** 플랜의 스키마는 별칭이 속한 곳이고,
    /// `FROM other.orders o` 면 둘이 다르다.
    #[test]
    fn the_schema_from_the_sql_wins_over_the_plan() {
        let mut q = record(&["shop.o"], Some("shop"));
        q.sql_text = Some("SELECT * FROM other.orders o WHERE o . id = ?".into());
        assert_eq!(
            wanted_tables(&q),
            vec![QualifiedTable::new("other", "orders")]
        );
    }

    /// SQL 이 없으면(정책 `off`) 예전처럼 플랜 이름을 그대로 쓴다 — 기능이 죽지 않는다.
    #[test]
    fn without_sql_text_the_plan_name_is_used_as_before() {
        let mut q = record(&["shop.a"], Some("shop"));
        q.sql_text = None;
        assert_eq!(wanted_tables(&q), vec![QualifiedTable::new("shop", "a")]);
    }

    #[test]
    fn duplicates_collapse_and_the_cap_holds() {
        let many: Vec<String> = (0..MAX_TABLES + 5).map(|i| format!("shop.t{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let out = wanted_tables(&record(&refs, Some("shop")));
        assert_eq!(out.len(), MAX_TABLES);

        let dup = wanted_tables(&record(&["shop.orders", "shop.orders"], Some("shop")));
        assert_eq!(dup.len(), 1);
    }

    /// **저장된 원문을 모델에 그대로 보내지 않는다** (FR-AI-04).
    ///
    /// 리터럴 정책이 `full` 인 배포에서 `sql_text` 에는 운영 데이터가 있다. 여기서
    /// 다시 정규화하지 않으면 그 값이 Bedrock 으로 나간다.
    #[test]
    fn literals_are_masked_before_the_model_sees_them() {
        let mut q = record(&["shop.orders"], Some("shop"));
        q.sql_text =
            Some("SELECT * FROM orders WHERE email = 'kim@example.com' AND id = 42".into());
        let ctx = build_context(&q, None, Vec::new());
        assert!(
            !ctx.sql.contains("kim@example.com"),
            "리터럴이 새어 나갔다: {}",
            ctx.sql
        );
        assert!(!ctx.sql.contains("42"), "숫자 리터럴이 남았다: {}", ctx.sql);
        assert!(
            ctx.sql.contains("orders"),
            "테이블 이름은 남아야 한다: {}",
            ctx.sql
        );
        assert!(!ctx.sql_has_literals);
    }

    /// **옵티마이저 힌트도 지운다.** 정규화가 힌트 안을 건드리지 않으므로 그 이름에
    /// 비밀이나 지시문이 있으면 모델로 나간다(교차 리뷰가 high 로 잡았다).
    #[test]
    fn optimizer_hints_do_not_reach_the_model() {
        let mut q = record(&["shop.orders"], Some("shop"));
        q.sql_text =
            Some("SELECT /*+ QB_NAME(sk_live_abcdef) */ * FROM orders WHERE id = 1".into());
        let ctx = build_context(&q, None, Vec::new());
        assert!(
            !ctx.sql.contains("sk_live"),
            "힌트 안의 문자열이 새어 나갔다: {}",
            ctx.sql
        );
        assert!(!ctx.sql.contains("QB_NAME"), "{}", ctx.sql);
        assert!(ctx.sql.contains("orders"), "{}", ctx.sql);
    }

    /// 마스킹을 신뢰할 수 없으면(인용부호 미종료) **SQL 을 통째로 버린다.**
    /// 플랜만으로 분석하는 편이 리터럴을 흘리는 것보다 낫다.
    #[test]
    fn unmaskable_sql_is_dropped_not_sent() {
        let mut q = record(&["shop.orders"], Some("shop"));
        q.sql_text = Some("SELECT * FROM orders WHERE memo = 'unterminated".into());
        let ctx = build_context(&q, None, Vec::new());
        assert_eq!(ctx.sql, "", "마스킹할 수 없는 SQL 을 보냈다");
    }
}
