//! 응답 직렬화 (M6) — **리터럴 노출 통제가 여기 한 곳에 있다.**
//!
//! # 왜 직렬화 계층에서 강제하는가
//!
//! [08 §6.1](../../../../docs/08-security-auth.md) 이 `full_restricted` 를 안전하다고
//! 보는 근거가 "**응답 직렬화 계층에서 강제하므로 새 엔드포인트에서 빠뜨릴 수
//! 없다**" 다. 핸들러마다 판정하면 새 엔드포인트가 하나 생길 때마다 빠뜨릴 기회가
//! 생기고, 그 실수는 개인정보 유출이다.
//!
//! 그래서 **도메인 레코드를 그대로 직렬화하지 않는다.** 이 모듈의 뷰 타입을
//! 반드시 거치고, 그 변환이 [`AuthContext::may_view_literals`] 를 묻는다.

use dbmon_core::rbac::AuthContext;
use dbmon_core::slow_query::{LiteralPolicy, SlowQuery};
use dbmon_core::time::EpochMs;
use serde::Serialize;

/// 목록·상세에 쓰는 슬로우 쿼리 뷰.
///
/// **`SlowQuery` 를 직접 반환하지 않는다** — 그러면 새 필드가 추가될 때마다
/// 노출 여부를 다시 판단해야 하고, 판단을 잊는 쪽이 기본값이 된다.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SlowQueryView {
    pub record_id: String,
    pub instance_id: String,
    pub env: String,
    pub state: String,
    pub thread_id: u64,
    pub started_at_ms: i64,
    pub duration_ms: i64,
    pub duration_source: String,
    /// 이 소요를 **관측한 시각**(수집기 시계). 진행 중 행의 경과 계산에 쓴다.
    ///
    /// `started_at_ms` 는 **대상 DB 시계**이므로 여기서 빼면 두 기계의 시계를 섞는다 —
    /// DB 시계가 5분 어긋난 환경에서 2초 쿼리가 5분으로 보인다(교차 리뷰 26라운드).
    /// `duration_ms + (server_now_ms - last_seen_at_ms)` 는 **둘 다 우리 시계**다.
    ///
    /// 하트비트가 이 값과 `duration_ms` 를 **함께** 올리므로 짝이 어긋나지 않는다.
    pub last_seen_at_ms: Option<EpochMs>,
    pub capture_source: String,
    pub app_digest: String,
    pub statement_type: String,
    pub schema_name: Option<String>,
    pub db_user: Option<String>,
    /// **열람 권한이 없으면 `None` 이다.** 정책이 원문을 저장했더라도.
    pub sql_text: Option<String>,
    /// SQL 이 가려진 이유. UI 가 "권한 없음" 과 "정책상 미저장" 을 구분해야 한다.
    pub sql_redacted_reason: Option<&'static str>,
    pub sql_text_truncated: bool,
    pub rows_examined: Option<u64>,
    pub rows_sent: Option<u64>,
    pub lock_time_ms: Option<i64>,
    pub has_plan: bool,
    pub abandoned_reason: Option<String>,
}

impl SlowQueryView {
    /// 도메인 레코드를 뷰로. **권한을 반드시 묻는다.**
    pub fn from_record(q: &SlowQuery, ctx: &AuthContext) -> Self {
        // 저장된 정책이 원문을 담고 있는데 열람 권한이 없으면 가린다.
        let stores_raw = matches!(
            q.literal_policy,
            LiteralPolicy::Full | LiteralPolicy::FullRestricted
        );
        let (sql_text, sql_redacted_reason) = if q.sql_text.is_none() {
            // 애초에 저장되지 않았다 (`off` 정책이거나 마스킹 실패).
            (None, Some("not_stored"))
        } else if stores_raw && !ctx.may_view_literals(q.literal_policy) {
            // 원문이 저장돼 있지만 이 사용자는 볼 수 없다.
            (None, Some("insufficient_role"))
        } else {
            (q.sql_text.clone(), None)
        };

        Self {
            record_id: q.record_id.as_str().to_string(),
            instance_id: q.instance_id.as_str().to_string(),
            env: q.env.as_str().to_string(),
            state: format!("{:?}", q.state).to_lowercase(),
            thread_id: q.thread_id,
            started_at_ms: q.started_at_ms,
            duration_ms: q.duration_ms,
            duration_source: format!("{:?}", q.duration_source).to_lowercase(),
            last_seen_at_ms: q.last_seen_at_ms,
            capture_source: format!("{:?}", q.capture_source).to_lowercase(),
            app_digest: q.app_digest.clone(),
            statement_type: format!("{:?}", q.statement_type).to_lowercase(),
            schema_name: q.schema_name.clone(),
            db_user: q.db_user.clone(),
            sql_text,
            sql_redacted_reason,
            sql_text_truncated: q.sql_text_truncated,
            rows_examined: q.stats.rows_examined,
            rows_sent: q.stats.rows_sent,
            lock_time_ms: q.stats.lock_time_ms,
            has_plan: q.plan.has_plan(),
            abandoned_reason: q.abandoned_reason.clone(),
        }
    }
}

/// WebSocket 방송용 슬로우 쿼리 ([09 §4.1] T-22).
///
/// # 왜 별도 타입인가 — `AuthContext` 를 받지 않는다
///
/// 방송은 **여러 사용자에게 같은 바이트를 보낸다.** 그래서 사용자별 권한으로
/// 가릴 수 없다. [`SlowQueryView`] 를 재사용하면 "어떤 문맥을 넘길까" 를 고민하게
/// 되고, 관대한 문맥을 하나 만들면 그게 전원에게 나간다.
///
/// 이 타입은 **문맥을 인자로 받지 않는다** — 그래서 실수로 관대하게 만들 수 없다.
///
/// # `sql_preview` 규칙
///
/// 정책이 `masked` 일 때만 담는다. 마스킹된 텍스트는 정의상 리터럴이 없다.
/// `full`/`full_restricted` 는 원문이므로 방송하지 않고, 클라이언트가 필요하면
/// HTTP 상세를 조회한다 — 그 경로에 사용자별 권한과 감사가 걸려 있다.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SlowQueryBroadcast {
    pub record_id: String,
    pub instance_id: String,
    pub env: String,
    pub state: String,
    pub started_at_ms: i64,
    pub duration_ms: i64,
    pub app_digest: String,
    pub statement_type: String,
    pub schema_name: Option<String>,
    /// 정규화·마스킹된 텍스트만. 원문이면 `None`.
    pub sql_preview: Option<String>,
}

impl SlowQueryBroadcast {
    pub fn from_record(q: &SlowQuery) -> Self {
        Self {
            record_id: q.record_id.as_str().to_string(),
            instance_id: q.instance_id.as_str().to_string(),
            env: q.env.as_str().to_string(),
            state: format!("{:?}", q.state).to_lowercase(),
            started_at_ms: q.started_at_ms,
            duration_ms: q.duration_ms,
            app_digest: q.app_digest.clone(),
            statement_type: format!("{:?}", q.statement_type).to_lowercase(),
            schema_name: q.schema_name.clone(),
            sql_preview: match q.literal_policy {
                LiteralPolicy::Masked => q.sql_text.clone(),
                // 원문 정책이면 방송하지 않는다. `off` 는 애초에 텍스트가 없다.
                LiteralPolicy::Full | LiteralPolicy::FullRestricted | LiteralPolicy::Off => None,
            },
        }
    }
}

/// 목록 응답 ([13 §2](../../../../docs/13-api-spec.md)).
#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<SlowQueryView>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    /// 이 응답에 담긴 건수. 화면이 클라이언트 페이지네이션을 하려면 필요하다.
    ///
    /// **저장소 전체의 건수가 아니다.** 조회 상한 안에서 실제로 읽은 수다 —
    /// 전체 건수를 세려면 구간 전체를 훑어야 하고, 그건 목록 화면이 낼 비용이
    /// 아니다. `has_more` 가 상한에 걸렸는지 말해 준다.
    pub total: usize,
    /// 서버가 이 응답을 만든 시각(epoch ms).
    ///
    /// # 왜 필요한가
    ///
    /// 화면은 진행 중 쿼리의 경과 시간을 **클라이언트에서** 흘린다(09 §3.4). 그 계산이
    /// 브라우저 시계에서 `started_at_ms` 를 빼면, 브라우저가 5분 앞선 기계에서는 2초
    /// 쿼리가 **5분째 실행 중**으로 보인다(교차 리뷰 25라운드). 서버 시각을 함께 주면
    /// 클라이언트가 그 차이를 한 번 재고 자기 시계의 **경과분만** 더할 수 있다.
    pub server_now_ms: EpochMs,
}

/// 실행계획 응답.
///
/// # 리터럴이 없다
///
/// 저장된 플랜 JSON 은 **정규화·마스킹된 것**이다(FR-PLN-09). 그래서 이 응답에는
/// 사용자별 리터럴 통제가 걸리지 않는다 — 대신 환경 스코프는 호출부가 검사한다.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlanView {
    pub record_id: String,
    pub instance_id: String,
    pub started_at_ms: i64,
    pub duration_ms: i64,
    pub statement_type: String,
    /// 플랜 JSON 원문(마스킹됨). 없으면 `s3_key` 나 `error` 를 본다.
    pub normalized_json: Option<String>,
    pub tree_text: Option<String>,
    pub format_version: Option<String>,
    /// 300KB 초과로 S3 에 오프로드된 경우의 키. **본문은 여기 없다** —
    /// 조회 경로가 아직 없으므로 화면이 그 사실을 말해야 한다.
    pub s3_key: Option<String>,
    pub fingerprint: Option<String>,
    pub referenced_tables: Vec<String>,
    /// 어떻게 얻은 플랜인가 (`current` 실행 중 캡처, `rerun` 사후 재실행 등).
    pub source: String,
    /// 수집 실패 사유. **있으면 화면에 그대로 보여준다** — 빈 화면으로 두면
    /// "플랜이 없다" 와 "플랜 수집이 실패했다" 가 구분되지 않는다.
    pub error: Option<String>,
}

impl PlanView {
    pub fn from_record(q: &SlowQuery) -> Self {
        Self {
            record_id: q.record_id.as_str().to_string(),
            instance_id: q.instance_id.as_str().to_string(),
            started_at_ms: q.started_at_ms,
            duration_ms: q.duration_ms,
            statement_type: format!("{:?}", q.statement_type).to_lowercase(),
            normalized_json: q.plan.normalized_json.clone(),
            tree_text: q.plan.tree_text.clone(),
            format_version: q.plan.format_version.clone(),
            s3_key: q.plan.s3_key.clone(),
            fingerprint: q.plan.fingerprint.clone(),
            referenced_tables: q.plan.referenced_tables.clone(),
            source: format!("{:?}", q.plan.source).to_lowercase(),
            error: q.plan.error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::Env;
    use dbmon_core::rbac::Role;

    fn ctx(role: Role, can_see_literals: bool) -> AuthContext {
        AuthContext {
            subject: "u".into(),
            role,
            env_scope: Env::ALL.to_vec(),
            can_see_literals,
            claims_version: 0,
        }
    }

    fn record(policy: LiteralPolicy, sql: Option<&str>) -> SlowQuery {
        let mut q = crate::store::tests::sample();
        q.literal_policy = policy;
        q.sql_text = sql.map(str::to_string);
        q
    }

    /// **원문이 저장돼 있어도 권한이 없으면 가린다.**
    ///
    /// `full_restricted` 가 안전하다는 주장의 근거가 정확히 이 동작이다.
    #[test]
    fn raw_literals_are_hidden_without_permission() {
        let q = record(
            LiteralPolicy::FullRestricted,
            Some("SELECT * WHERE email='a@b.c'"),
        );
        let v = SlowQueryView::from_record(&q, &ctx(Role::Viewer, false));
        assert_eq!(v.sql_text, None, "권한 없는 사용자에게 원문이 나갔다");
        assert_eq!(v.sql_redacted_reason, Some("insufficient_role"));
    }

    /// 권한이 있으면 보인다.
    #[test]
    fn operators_with_permission_see_the_raw_text() {
        let q = record(LiteralPolicy::FullRestricted, Some("SELECT 1"));
        let v = SlowQueryView::from_record(&q, &ctx(Role::Operator, true));
        assert_eq!(v.sql_text.as_deref(), Some("SELECT 1"));
        assert_eq!(v.sql_redacted_reason, None);
    }

    /// **마스킹된 텍스트는 권한과 무관하게 보인다** — 리터럴이 없으므로.
    #[test]
    fn masked_text_is_visible_to_everyone() {
        let q = record(LiteralPolicy::Masked, Some("SELECT a FROM t WHERE id = ?"));
        let v = SlowQueryView::from_record(&q, &ctx(Role::Viewer, false));
        assert_eq!(v.sql_text.as_deref(), Some("SELECT a FROM t WHERE id = ?"));
        assert_eq!(v.sql_redacted_reason, None);
    }

    /// 저장 자체가 안 된 경우와 권한 부족을 **구분해서** 알린다.
    ///
    /// UI 가 "정책상 없음" 과 "당신이 볼 수 없음" 을 다르게 표시해야 한다.
    #[test]
    fn absent_and_forbidden_are_distinguishable() {
        let absent = record(LiteralPolicy::Off, None);
        let v = SlowQueryView::from_record(&absent, &ctx(Role::Admin, true));
        assert_eq!(v.sql_redacted_reason, Some("not_stored"));

        let forbidden = record(LiteralPolicy::Full, Some("SELECT 1"));
        let v = SlowQueryView::from_record(&forbidden, &ctx(Role::Viewer, false));
        assert_eq!(v.sql_redacted_reason, Some("insufficient_role"));
    }

    /// **방송은 원문 리터럴을 절대 담지 않는다** (T-22).
    ///
    /// 방송은 여러 사용자에게 같은 바이트를 보내므로 권한으로 가릴 수 없다.
    /// 이게 깨지면 리터럴 열람 권한이 없는 사용자에게도 원문이 흘러간다.
    #[test]
    fn broadcasts_never_carry_raw_literals() {
        for policy in [LiteralPolicy::Full, LiteralPolicy::FullRestricted] {
            let q = record(policy, Some("SELECT * WHERE email='a@b.c'"));
            let b = SlowQueryBroadcast::from_record(&q);
            assert_eq!(b.sql_preview, None, "{policy:?} 원문이 방송됐다");
            let json = serde_json::to_string(&b).expect("직렬화");
            assert!(!json.contains("a@b.c"), "{policy:?}: {json}");
        }
    }

    /// 마스킹된 텍스트는 방송한다 — 리터럴이 없으므로 안전하고, 없으면 화면이 빈다.
    #[test]
    fn masked_text_is_broadcast() {
        let q = record(LiteralPolicy::Masked, Some("SELECT a FROM t WHERE id = ?"));
        let b = SlowQueryBroadcast::from_record(&q);
        assert_eq!(
            b.sql_preview.as_deref(),
            Some("SELECT a FROM t WHERE id = ?")
        );
    }

    /// 방송 타입도 내부 필드를 노출하지 않는다.
    #[test]
    fn the_broadcast_does_not_leak_internal_fields() {
        let q = record(LiteralPolicy::Masked, Some("SELECT 1"));
        let json = serde_json::to_string(&SlowQueryBroadcast::from_record(&q)).expect("직렬화");
        for internal in [
            "owner_worker",
            "owner_epoch",
            "db_user",
            "db_host",
            "s3_key",
        ] {
            assert!(!json.contains(internal), "{internal} 이 방송에 있다");
        }
    }

    /// **뷰가 도메인 레코드의 모든 필드를 노출하지 않는다.**
    ///
    /// `owner_worker`·`owner_epoch`·`plan` 원문 등은 내부 값이다. 통째로
    /// 직렬화하면 새 내부 필드가 자동으로 API 표면이 된다.
    #[test]
    fn the_view_does_not_leak_internal_fields() {
        let q = record(LiteralPolicy::Masked, Some("SELECT 1"));
        let json = serde_json::to_string(&SlowQueryView::from_record(&q, &ctx(Role::Admin, true)))
            .expect("직렬화");
        for internal in ["owner_worker", "owner_epoch", "normalized_json", "s3_key"] {
            assert!(!json.contains(internal), "{internal} 이 응답에 있다");
        }
        // 플랜은 존재 여부만 알린다.
        assert!(json.contains("has_plan"));
    }
}
