//! 탐색 판정 — **SDK 응답을 도메인 인스턴스로 옮기는 순수 로직** (M2-5).
//!
//! # 왜 SDK 타입을 여기 들이지 않는가
//!
//! `filter.rs` 와 같은 이유다. AWS 호출은 로컬에서 검증할 수 없지만(SSO 만료 시 아예 못
//! 돈다) **판정은 전수 검증할 수 있다.** 그리고 여기서 틀리면:
//!
//! | 잘못된 판정 | 결과 |
//! |---|---|
//! | 버전 미달을 `Collecting` 으로 | 지원하지 않는 인스턴스에 매초 쿼리를 보낸다 |
//! | `dbmon:enabled=false` 를 무시 | 사용자가 끈 인스턴스를 계속 수집한다 |
//! | 태그 없음을 `dev` 로 추측 | 프로덕션에 느슨한 리터럴 정책이 적용된다 |
//!
//! 마지막 것이 가장 나쁘고, 되돌릴 수 없다. 그래서 [`Env::Unknown`] 은 추측하지 않는다.
//!
//! SDK 어댑터([`super::rds`])는 응답을 [`RawDbInstance`] 로 옮기는 일만 한다.

use std::collections::BTreeMap;

use dbmon_core::env::{EnvMapping, EnvResolution};
use dbmon_core::ids::{ClusterId, InstanceId};
use dbmon_core::instance::{Engine, EngineVersion, Instance, InstanceState};
use dbmon_core::time::EpochMs;

use super::filter::Candidate;

/// `DescribeDBInstances` 응답에서 뽑은 원시 필드.
///
/// **SDK 타입이 아니다.** 이 경계 덕분에 아래 판정을 AWS 없이 돌릴 수 있다.
#[derive(Debug, Clone, Default)]
pub struct RawDbInstance {
    pub identifier: String,
    /// 이름 변경에도 불변인 RDS 내부 ID. `renamed_*` 추적의 기준 (FR-DSC-14).
    pub dbi_resource_id: String,
    /// `mysql` / `aurora-mysql` / `postgres` …
    pub engine: String,
    pub engine_version: String,
    /// `available` / `creating` / `stopped` …
    pub status: String,
    pub endpoint_address: Option<String>,
    pub endpoint_port: Option<u16>,
    pub vpc_id: Option<String>,
    pub availability_zone: Option<String>,
    pub instance_class: Option<String>,
    /// Aurora 클러스터 식별자. RDS MySQL 이면 `None`.
    pub cluster_identifier: Option<String>,
    pub is_cluster_writer: bool,
    pub iam_auth_enabled: bool,
    /// `CertificateDetails.ValidTill` (FR-DSC-13).
    pub cert_valid_till_ms: Option<EpochMs>,
    pub tags: BTreeMap<String, String>,
    pub region: String,
}

impl RawDbInstance {
    /// T-37 필터에 넘길 후보로 좁힌다.
    pub fn candidate(&self) -> Candidate {
        Candidate {
            identifier: self.identifier.clone(),
            vpc_id: self.vpc_id.clone(),
            tags: self.tags.clone(),
        }
    }
}

/// 수집 활성/비활성을 태그로 제어한다 (FR-DSC-05). UI 오버라이드가 이보다 우선한다.
pub const ENABLED_TAG: &str = "dbmon:enabled";

/// 인스턴스로 변환할 수 없는 이유.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unmappable {
    /// MySQL 계열이 아니다 (postgres, sqlserver …). 정상적으로 흔하다.
    NotMysql { engine: String },
    /// 버전 문자열을 해석할 수 없다. **버려서는 안 된다** — 새 버전 형식일 수 있다.
    UnparsableVersion { raw: String },
    /// 식별자·계정·리전이 키 규칙을 위반한다.
    InvalidId { reason: String },
}

/// 원시 응답을 도메인 인스턴스로. **탐색이 발견한 사실만 담는다** —
/// 자가진단 결과(`Degraded`)나 접속 실패(`Unreachable`)는 수집 루프가 나중에 쓴다.
pub fn to_instance(
    raw: &RawDbInstance,
    account_id: &str,
    mapping: &EnvMapping,
    now_ms: EpochMs,
) -> Result<Instance, Unmappable> {
    let engine = Engine::parse(&raw.engine).ok_or_else(|| Unmappable::NotMysql {
        engine: raw.engine.clone(),
    })?;
    let version =
        EngineVersion::parse(&raw.engine_version).ok_or_else(|| Unmappable::UnparsableVersion {
            raw: raw.engine_version.clone(),
        })?;
    let id = InstanceId::new(account_id, &raw.region, &raw.identifier).map_err(|e| {
        Unmappable::InvalidId {
            reason: e.to_string(),
        }
    })?;
    let cluster_id = raw
        .cluster_identifier
        .as_deref()
        .and_then(|c| ClusterId::new(account_id, &raw.region, c).ok());

    Ok(Instance {
        id,
        cluster_id,
        engine,
        state: initial_state(raw, engine, &version),
        env: EnvResolution::resolve(mapping.classify(&raw.tags), None),
        engine_version: version,
        endpoint: raw.endpoint_address.clone(),
        // 엔드포인트 포트가 없으면(생성 중) 엔진 기본값을 쓴다. 접속은 어차피
        // `endpoint` 가 채워진 뒤에만 시도한다.
        port: raw.endpoint_port.unwrap_or(3306),
        dbi_resource_id: raw.dbi_resource_id.clone(),
        vpc_id: raw.vpc_id.clone(),
        availability_zone: raw.availability_zone.clone(),
        instance_class: raw.instance_class.clone(),
        is_cluster_writer: raw.is_cluster_writer,
        iam_auth_enabled: raw.iam_auth_enabled,
        tags: raw.tags.clone(),
        cert_valid_till_ms: raw.cert_valid_till_ms,
        first_seen_ms: now_ms,
        last_seen_ms: now_ms,
        deleted_at_ms: None,
        missing_count: 0,
        renamed_from: None,
        renamed_to: None,
    })
}

/// 탐색 시점의 상태.
///
/// **`Collecting` 을 여기서 주지 않는다.** 부트스트랩·자가진단(FR-DSC-10)이 먼저다 —
/// [`InstanceState::should_collect`] 가 `Pending` 을 제외하는 이유가 그것이다.
/// 순서가 중요하다: 사용자가 끈 것은 버전보다 먼저 본다(끈 인스턴스에 "버전 미달"
/// 배지를 붙이면 사용자가 자기 설정을 의심한다).
fn initial_state(raw: &RawDbInstance, engine: Engine, version: &EngineVersion) -> InstanceState {
    if tag_disables_collection(&raw.tags) {
        return InstanceState::Disabled;
    }
    if !version.is_supported(engine) {
        return InstanceState::Unsupported;
    }
    InstanceState::Pending
}

/// `dbmon:enabled` 가 수집을 끄는가 (FR-DSC-05).
///
/// **`false` 만 끈다.** 오타(`fasle`)나 빈 값으로 수집이 조용히 멈추면 안 된다 —
/// 그건 "수집되는 줄 알았는데 아니었다" 라는 최악의 실패다.
fn tag_disables_collection(tags: &BTreeMap<String, String>) -> bool {
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(ENABLED_TAG))
        .is_some_and(|(_, v)| matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0"))
}

/// 탐색 결과를 기존 등록부와 맞춘다 (FR-DSC-07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconcile {
    /// 처음 봤다 — 등록한다.
    Insert,
    /// 다시 보였다 — `last_seen_ms` 갱신, `missing_count` 초기화.
    Seen,
    /// 이번 탐색에 없었다. **연속 횟수만 올린다.**
    Missing { consecutive: u32 },
    /// 연속 미발견이 임계에 닿았다 — `deleted_at` 을 찍는다(즉시 삭제하지 않는다).
    MarkDeleted,
}

/// 등록부 하나에 대한 판정.
///
/// # 왜 1회 미발견으로 삭제하지 않는가 (FR-DSC-07)
///
/// `DescribeDBInstances` 는 조절(throttling)·부분 실패·일시적 권한 오류로 **빈 목록에
/// 가까운 응답**을 줄 수 있다. 그 한 번으로 500대를 `deleted` 로 찍으면 UI 가 비고
/// 과거 데이터의 인스턴스 메타 참조가 끊긴다. 그래서 [`MISSING_THRESHOLD`] 회
/// 연속일 때만 삭제로 판정한다.
///
/// [`MISSING_THRESHOLD`]: dbmon_core::instance::MISSING_THRESHOLD
pub fn reconcile_one(known: Option<&Instance>, seen_now: bool) -> Reconcile {
    match (known, seen_now) {
        (None, true) => Reconcile::Insert,
        // 등록부에 없고 이번에도 없다 — 판정할 것이 없다. 호출자가 부르지 않는 경로다.
        (None, false) => Reconcile::Missing { consecutive: 0 },
        (Some(_), true) => Reconcile::Seen,
        (Some(prev), false) => {
            let next = prev.missing_count + 1;
            if should_mark_deleted(next) {
                Reconcile::MarkDeleted
            } else {
                Reconcile::Missing { consecutive: next }
            }
        }
    }
}

/// 카운터를 올린 **뒤** 삭제로 판정할 것인가.
///
/// **이 판정의 유일한 정의다.** 저장 어댑터([`crate::store::registry`])의
/// `mark_missing` 도 이 함수를 부른다 — 임계값을 두 곳에 적으면 한쪽만 바뀐다.
/// 이 프로젝트에서 "고쳤는데 다른 경로가 옛 규칙을 쓴다" 부류가 반복됐다.
pub fn should_mark_deleted(missing_count_after: u32) -> bool {
    missing_count_after >= dbmon_core::instance::MISSING_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::env::Env;

    const ACCOUNT: &str = "123456789012";
    const NOW: EpochMs = 1_755_500_400_000;

    fn raw(engine: &str, version: &str) -> RawDbInstance {
        RawDbInstance {
            identifier: "orders-01".into(),
            dbi_resource_id: "db-ABCDEFGHIJKLMNOP".into(),
            engine: engine.into(),
            engine_version: version.into(),
            status: "available".into(),
            endpoint_address: Some("orders-01.abc.ap-northeast-2.rds.amazonaws.com".into()),
            endpoint_port: Some(3306),
            vpc_id: Some("vpc-dev".into()),
            region: "ap-northeast-2".into(),
            ..Default::default()
        }
    }

    fn with_tags(mut r: RawDbInstance, pairs: &[(&str, &str)]) -> RawDbInstance {
        r.tags = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        r
    }

    fn map(r: &RawDbInstance) -> Result<Instance, Unmappable> {
        to_instance(r, ACCOUNT, &EnvMapping::default(), NOW)
    }

    #[test]
    fn maps_rds_mysql() {
        let i = map(&raw("mysql", "8.4.6")).expect("매핑");
        assert_eq!(i.engine, Engine::Mysql);
        assert_eq!(i.engine_version.community, (8, 4, 6));
        assert_eq!(i.id.as_str(), "123456789012/ap-northeast-2/orders-01");
        assert_eq!(i.port, 3306);
    }

    #[test]
    fn maps_aurora_version_string() {
        let i = map(&raw("aurora-mysql", "8.0.mysql_aurora.3.05.2")).expect("매핑");
        assert_eq!(i.engine, Engine::AuroraMysql);
        assert_eq!(i.engine_version.aurora, Some((3, 5, 2)));
    }

    /// MySQL 이 아닌 엔진은 **정상적으로** 걸러진다 — 계정에 postgres 가 흔하다.
    #[test]
    fn rejects_non_mysql_engines() {
        for engine in [
            "postgres",
            "sqlserver-ex",
            "oracle-se2",
            "aurora-postgresql",
        ] {
            let e = map(&raw(engine, "16.3")).expect_err("걸러야 한다");
            assert!(matches!(e, Unmappable::NotMysql { .. }), "{engine}: {e:?}");
        }
    }

    /// **새 인스턴스는 `Pending` 이다.** 부트스트랩·자가진단 전에 수집하면 안 된다.
    #[test]
    fn new_instance_is_pending_not_collecting() {
        let i = map(&raw("mysql", "8.4.6")).expect("매핑");
        assert_eq!(i.state, InstanceState::Pending);
        assert!(
            !i.is_collectible(),
            "자가진단 전에 수집 대상이 됐다 — 전제조건을 확인하지 않고 쿼리를 보낸다"
        );
    }

    /// 버전 미달은 `Unsupported` 여야 한다 (FR-DSC-11).
    #[test]
    fn below_the_version_floor_is_unsupported() {
        let i = map(&raw("mysql", "8.0.39")).expect("매핑");
        assert_eq!(i.state, InstanceState::Unsupported);
        assert!(!i.is_collectible());

        // Aurora 하한(3.05) 미달도 같다.
        let a = map(&raw("aurora-mysql", "5.7.mysql_aurora.2.11.4")).expect("매핑");
        assert_eq!(a.state, InstanceState::Unsupported);
    }

    /// 해석할 수 없는 버전은 **버리지 않고 사유를 남긴다.** 새 형식이면 우리가 고쳐야 한다.
    #[test]
    fn unparsable_version_is_reported_not_swallowed() {
        let e = map(&raw("mysql", "8.5-preview-xyz")).expect_err("해석 실패");
        assert!(matches!(e, Unmappable::UnparsableVersion { .. }), "{e:?}");
    }

    /// `dbmon:enabled=false` 는 수집을 끈다 (FR-DSC-05). 대소문자를 구분하지 않는다.
    #[test]
    fn enabled_tag_can_disable_collection() {
        for value in ["false", "FALSE", " False ", "0"] {
            let r = with_tags(raw("mysql", "8.4.6"), &[(ENABLED_TAG, value)]);
            assert_eq!(
                map(&r).expect("매핑").state,
                InstanceState::Disabled,
                "{value:?} 로 끄지 못했다"
            );
        }
        // 키 대소문자도 무시한다.
        let r = with_tags(raw("mysql", "8.4.6"), &[("DBMON:ENABLED", "false")]);
        assert_eq!(map(&r).expect("매핑").state, InstanceState::Disabled);
    }

    /// **`false` 가 아닌 값으로 수집이 멈추면 안 된다.**
    ///
    /// 오타로 조용히 멈추는 것은 "수집되는 줄 알았는데 아니었다" 라는 최악의 실패다.
    #[test]
    fn only_explicit_false_disables_collection() {
        for value in ["true", "", "yes", "fasle", "no", "off"] {
            let r = with_tags(raw("mysql", "8.4.6"), &[(ENABLED_TAG, value)]);
            assert_ne!(
                map(&r).expect("매핑").state,
                InstanceState::Disabled,
                "{value:?} 로 수집이 멈췄다 — 오타 하나로 관측이 사라진다"
            );
        }
    }

    /// 사용자가 끈 것은 **버전보다 먼저** 본다 — 배지가 사용자를 헷갈리게 하면 안 된다.
    #[test]
    fn disabled_wins_over_unsupported() {
        let r = with_tags(raw("mysql", "8.0.39"), &[(ENABLED_TAG, "false")]);
        assert_eq!(map(&r).expect("매핑").state, InstanceState::Disabled);
    }

    /// 태그가 없으면 `unknown` 이다. **`dev` 로 추측하지 않는다** (FR-DSC-04) —
    /// 프로덕션을 개발로 오분류하면 리터럴 정책이 느슨해지고, 그건 되돌릴 수 없다.
    #[test]
    fn missing_env_tag_is_unknown_never_guessed() {
        let i = map(&raw("mysql", "8.4.6")).expect("매핑");
        assert_eq!(i.env.effective, Env::Unknown);
        assert!(i.env.effective.treat_as_production());

        let tagged = with_tags(raw("mysql", "8.4.6"), &[("Environment", "PRD")]);
        assert_eq!(map(&tagged).expect("매핑").env.effective, Env::Prd);
    }

    /// 엔드포인트가 없어도(생성 중) 등록은 된다 — 목록에 보여야 한다. 수집은 안 된다.
    #[test]
    fn instance_without_endpoint_registers_but_is_not_collectible() {
        let mut r = raw("mysql", "8.4.6");
        r.endpoint_address = None;
        r.endpoint_port = None;
        let i = map(&r).expect("매핑");
        assert!(i.endpoint.is_none());
        assert!(!i.is_collectible());
        assert_eq!(i.port, 3306, "포트 기본값이 없으면 접속 문자열이 깨진다");
    }

    #[test]
    fn aurora_cluster_membership_is_carried() {
        let mut r = raw("aurora-mysql", "8.0.mysql_aurora.3.05.2");
        r.cluster_identifier = Some("orders-cluster".into());
        r.is_cluster_writer = true;
        let i = map(&r).expect("매핑");
        assert_eq!(
            i.cluster_id.as_ref().map(|c| c.as_str()),
            Some("123456789012/ap-northeast-2/orders-cluster")
        );
        assert!(i.is_cluster_writer);
    }

    // ── FR-DSC-07 재조정 ──────────────────────────────────────────────────────

    fn known(missing_count: u32) -> Instance {
        let mut i = map(&raw("mysql", "8.4.6")).expect("매핑");
        i.missing_count = missing_count;
        i
    }

    /// **1회 미발견으로 삭제하지 않는다.** API 한 번 삐끗해서 500대가 사라지면 안 된다.
    #[test]
    fn one_miss_never_marks_deleted() {
        assert_eq!(
            reconcile_one(Some(&known(0)), false),
            Reconcile::Missing { consecutive: 1 },
            "1회 미발견으로 삭제 판정했다 — 일시적 API 실패가 등록부를 비운다"
        );
    }

    /// 2회 연속이면 삭제로 판정한다.
    #[test]
    fn two_consecutive_misses_mark_deleted() {
        assert_eq!(
            reconcile_one(Some(&known(1)), false),
            Reconcile::MarkDeleted
        );
    }

    /// 중간에 다시 보이면 **연속 카운터가 리셋된다.**
    #[test]
    fn reappearing_resets_the_counter() {
        assert_eq!(reconcile_one(Some(&known(1)), true), Reconcile::Seen);
    }

    #[test]
    fn first_sighting_inserts() {
        assert_eq!(reconcile_one(None, true), Reconcile::Insert);
    }
}
