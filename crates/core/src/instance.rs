//! 인스턴스 레지스트리 도메인 타입.

use crate::env::EnvResolution;
use crate::ids::{ClusterId, InstanceId};
use crate::time::EpochMs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    Mysql,
    AuroraMysql,
}

impl Engine {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mysql" => Some(Self::Mysql),
            "aurora-mysql" => Some(Self::AuroraMysql),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mysql => "mysql",
            Self::AuroraMysql => "aurora-mysql",
        }
    }
}

/// 엔진 버전. **문자열 비교로는 판정할 수 없다.**
///
/// Aurora 는 `8.0.mysql_aurora.3.05.2` 형태로 보고한다. 문자열로 비교하면
/// `8.0.mysql_aurora.3.05.2 < 8.4.5` 같은 무의미한 결과가 나오고,
/// 더 나쁘게는 `3.10` 이 `3.05` 보다 작다고 판정된다(사전순).
/// → 커뮤니티 버전과 Aurora 버전을 **분리해 수치로** 비교한다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineVersion {
    /// AWS API 가 준 원문. 표시·감사용으로 보존한다.
    pub raw: String,
    /// 커뮤니티 MySQL 호환 버전 (`8.0.32` → `(8,0,32)`).
    pub community: (u32, u32, u32),
    /// Aurora 자체 버전 (`3.05.2` → `(3,5,2)`). RDS MySQL 이면 `None`.
    pub aurora: Option<(u32, u32, u32)>,
}

/// ADR-019: RDS MySQL 하한.
pub const MIN_MYSQL: (u32, u32, u32) = (8, 4, 0);
/// ADR-019: Aurora MySQL 하한 (3.05 = 커뮤니티 8.0.32 상당).
pub const MIN_AURORA: (u32, u32, u32) = (3, 5, 0);
/// `SHOW BINARY LOG STATUS` 가 도입된 버전. 그 미만은 구 문장을 써야 한다.
const BINLOG_STATUS_RENAMED_AT: (u32, u32, u32) = (8, 4, 0);

/// `explain_json_format_version` 이 도입된 버전 (MySQL 8.3.0).
///
/// 그 미만에서 이 변수를 세우면 **1193 `Unknown system variable`** 이다 —
/// 8.0.46 으로 실측했다. Aurora MySQL 3.x 는 8.0 호환이므로 **Aurora 에서는 쓸 수 없다.**
const EXPLAIN_JSON_V2_AT: (u32, u32, u32) = (8, 3, 0);

impl EngineVersion {
    /// `8.4.5` / `8.0.39` / `8.0.mysql_aurora.3.05.2` / `5.7.mysql_aurora.2.11.4` 를 해석한다.
    pub fn parse(raw: &str) -> Option<Self> {
        const AURORA_MARK: &str = ".mysql_aurora.";
        if let Some(idx) = raw.find(AURORA_MARK) {
            let community = parse_triple(&raw[..idx])?;
            let aurora = parse_triple(&raw[idx + AURORA_MARK.len()..])?;
            return Some(Self {
                raw: raw.to_string(),
                community,
                aurora: Some(aurora),
            });
        }
        Some(Self {
            raw: raw.to_string(),
            community: parse_triple(raw)?,
            aurora: None,
        })
    }

    /// 수집 대상으로 지원하는 버전인가 (FR-DSC-11).
    pub fn is_supported(&self, engine: Engine) -> bool {
        match (engine, self.aurora) {
            // Aurora 는 Aurora 버전으로 판정한다. 커뮤니티 성분(8.0)은 하한을 넘지 못한다.
            (Engine::AuroraMysql, Some(a)) => a >= MIN_AURORA,
            // 엔진은 Aurora 인데 버전 문자열에 Aurora 성분이 없다 → 판정 불가.
            // 지원한다고 단정하지 않는다.
            (Engine::AuroraMysql, None) => false,
            (Engine::Mysql, _) => self.community >= MIN_MYSQL,
        }
    }

    /// 바이너리 로그 위치 조회 문장. **유일하게 버전 분기가 필요한 문장이다**
    /// ([05 §2.7](../../../docs/05-collector.md)).
    ///
    /// 8.4 에서 구 문장이 제거되었다. 아래 문자열은 MySQL 문법 토큰을 그대로 인용한 것이며
    /// 우리가 고른 이름이 아니다.
    /// v2 계획 형식(`explain_json_format_version=2`)을 쓸 수 있는가.
    ///
    /// # 왜 버전으로 갈라야 하는가
    ///
    /// 8.3 미만에서 그 세션 변수를 세우면 1193 으로 실패한다(8.0.46 실측). 매번 시도해
    /// 보고 실패를 세는 방식은 **정상 상태에서 실패 카운터가 계속 오르는** 결과가 되고,
    /// 그러면 "실패" 가 신호로서 쓸모없어진다.
    ///
    /// **커뮤니티 버전으로 판정한다** — Aurora 3.x 는 자체 버전이 3.x 여도 커뮤니티
    /// 호환은 8.0 이라 v2 가 없다.
    pub fn supports_explain_json_v2(&self) -> bool {
        self.community >= EXPLAIN_JSON_V2_AT
    }

    pub fn binary_log_status_stmt(&self) -> &'static str {
        if self.community >= BINLOG_STATUS_RENAMED_AT {
            "SHOW BINARY LOG STATUS"
        } else {
            "SHOW MASTER STATUS"
        }
    }
}

/// **성분이 없는 것**과 **있지만 해석 불가**를 구분한다.
///
/// `unwrap_or(0)` 로 뭉개면 `8.x.1` 이 `(8,0,1)` 로 조용히 통과해 버전 게이팅이
/// 엉뚱한 판정을 내린다. 해석할 수 없으면 실패시키고 `unsupported_version` 으로 남긴다.
fn parse_triple(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = match it.next() {
        Some(v) => v.parse().ok()?,
        None => 0,
    };
    let patch = match it.next() {
        Some(v) => v.parse().ok()?,
        None => 0,
    };
    // 남은 성분이 있으면 형태를 이해하지 못한 것이다 → 실패시킨다.
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

impl fmt::Display for EngineVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// 수집 관점의 인스턴스 상태.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// 정상 수집 중.
    Collecting,
    /// 등록됐지만 아직 부트스트랩·자가진단 전.
    Pending,
    /// 네트워크·인증 문제로 접속 불가.
    Unreachable,
    /// 접속은 되지만 관측 소스가 일부 죽었다 (consumer 꺼짐 등, F15).
    Degraded,
    /// 버전 하한 미달 등으로 수집하지 않는다.
    Unsupported,
    /// 사용자가 수집을 끔.
    Disabled,
    /// **탐색 필터가 제외했다** (T-37). 사용자가 끈 것도, 사라진 것도 아니다.
    ///
    /// 이 상태가 따로 있어야 하는 이유: 필터가 거부한 인스턴스를 "사라졌다" 로
    /// 처리하면 태그 일괄 변경 한 번에 등록부가 비고, `Disabled` 로 처리하면
    /// 사용자가 끈 것과 구분되지 않아 "내가 끄지 않았는데" 가 된다.
    Excluded,
    /// RDS 에서 사라졌다.
    Deleted,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collecting => "collecting",
            Self::Pending => "pending",
            Self::Unreachable => "unreachable",
            Self::Degraded => "degraded",
            Self::Unsupported => "unsupported",
            Self::Disabled => "disabled",
            Self::Excluded => "excluded",
            Self::Deleted => "deleted",
        }
    }
    /// 수집 태스크를 **띄워 둘** 상태인가.
    ///
    /// `Unreachable` 도 포함한다. 이 상태에서 태스크를 내리면 서킷 브레이커의
    /// half-open 재시도 주체가 사라져 **영구히 복구되지 않는다**
    /// ([05 §6](../../../docs/05-collector.md)). 실제 쿼리 빈도는 태스크 안의
    /// 서킷 상태가 정한다 — 열려 있으면 60초에 1회다.
    ///
    /// `Pending` 은 **제외한다** — 등록만 됐고 아직 사람이 켜지 않은 상태다.
    ///
    /// # 왜 자동으로 켜지 않는가
    ///
    /// 수집을 시작하는 것은 **대상 DB 에 매초 쿼리를 날리기 시작하는 것**이다. 새 RDS 가
    /// 생기자마자 자동으로 붙으면 (a) 운영자가 모르는 접속이 생기고 (b) 모니터링 계정이
    /// 아직 없으면 실패 로그가 쏟아진다 — 실측으로 겪었다(계정을 만들지 않은 시드 5대가
    /// `Access denied` 를 반복했다).
    ///
    /// 그래서 등록은 자동, **시작은 사람**이다(`POST /api/instances/{id}/start`).
    /// 켠 뒤에는 상태가 스스로 움직인다: 못 붙으면 `Unreachable`, 되돌아오면 `Collecting`.
    /// 항목별 사유(performance_schema 꺼짐 등)를 함께 보여주는 것은 FR-DSC-10 의 몫이다.
    pub fn should_collect(self) -> bool {
        matches!(self, Self::Collecting | Self::Degraded | Self::Unreachable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub id: InstanceId,
    pub cluster_id: Option<ClusterId>,
    pub engine: Engine,
    pub engine_version: EngineVersion,
    pub env: EnvResolution,
    pub state: InstanceState,
    /// 접속 엔드포인트. 없을 수 있다(생성 중, Aurora Serverless 정지 등).
    pub endpoint: Option<String>,
    pub port: u16,
    /// 이름 변경에도 불변인 RDS 내부 ID. `renamed_*` 추적의 기준
    /// ([04 §1.3](../../../docs/04-data-model.md)).
    pub dbi_resource_id: String,
    pub vpc_id: Option<String>,
    pub availability_zone: Option<String>,
    pub instance_class: Option<String>,
    /// 할당 스토리지(GiB). **RDS 전용, Aurora 는 `None`** (볼륨이 자동 증가한다).
    ///
    /// `FreeStorageSpace` 를 퍼센트로 바꿀 분모다 — 바이트만으로는 "18GB 남음" 이
    /// 위험한지 알 수 없다.
    ///
    /// ⚠ **`serde(default)` 가 필수다.** 이 필드가 없던 시절에 저장된 등록부 레코드가
    /// 이미 있고, 없으면 그 항목을 읽다 실패해 `list()` 가 조용히 건너뛴다.
    #[serde(default)]
    pub allocated_storage_gb: Option<i32>,
    /// Aurora 클러스터에서 라이터인가. 페일오버로 바뀐다.
    pub is_cluster_writer: bool,
    pub iam_auth_enabled: bool,
    pub tags: BTreeMap<String, String>,
    /// 대상 RDS 인증서 만료 시각 (FR-DSC-13).
    pub cert_valid_till_ms: Option<EpochMs>,
    pub first_seen_ms: EpochMs,
    pub last_seen_ms: EpochMs,
    /// 탐색에서 사라진 시각. **2회 연속 미발견 시에만** 설정한다(FR-DSC-07).
    pub deleted_at_ms: Option<EpochMs>,
    /// 연속 미발견 횟수. 1회 API 실패로 삭제 처리되지 않게 한다.
    pub missing_count: u32,
    pub renamed_from: Option<InstanceId>,
    pub renamed_to: Option<InstanceId>,
}

impl Instance {
    /// 수집 대상인가. 상태·버전·삭제 여부를 모두 본다.
    pub fn is_collectible(&self) -> bool {
        self.deleted_at_ms.is_none()
            && self.state.should_collect()
            && self.engine_version.is_supported(self.engine)
            && self.endpoint.is_some()
    }
}

/// 탐색에서 사라진 것으로 판정하기 위한 연속 미발견 횟수 (FR-DSC-07).
pub const MISSING_THRESHOLD: u32 = 2;

/// 카운터를 올린 **뒤** 삭제로 판정할 것인가 (FR-DSC-07).
///
/// **이 판정의 유일한 정의다.** 저장 어댑터와 페이크가 모두 이 함수를 부른다 —
/// 처음에는 상수만 공유하고 비교식은 각자 적었는데, 그러면 **값의 드리프트만 막고
/// 규칙의 드리프트는 막지 못한다.** 술어를 `>` 로 바꿔도 페이크 기반 테스트는 전부
/// 통과했다(2차 리뷰가 지적).
pub fn should_mark_deleted(missing_count_after: u32) -> bool {
    missing_count_after >= MISSING_THRESHOLD
}

#[cfg(test)]
mod tests {

    /// **v2 는 8.3+ 에서만 된다.** 8.0 에서 세션 변수를 세우면 1193 이다(실측).
    /// Aurora 3.x 는 자체 버전이 3.x 여도 커뮤니티 호환이 8.0 이라 v2 가 없다 —
    /// Aurora 버전으로 판정하면 3.5 > 8.3 비교가 성립하지 않아 조용히 틀린다.
    #[test]
    fn explain_json_v2_is_gated_by_the_community_version() {
        let v84 = EngineVersion::parse("8.4.5").expect("8.4");
        assert!(v84.supports_explain_json_v2());
        assert!(
            EngineVersion::parse("8.3.0")
                .expect("8.3")
                .supports_explain_json_v2()
        );

        let v80 = EngineVersion::parse("8.0.46").expect("8.0");
        assert!(!v80.supports_explain_json_v2());

        // ── AWS 가 실제로 보고하는 문자열로 검증한다 ──────────────────────────
        //
        // `aws rds describe-db-engine-versions --engine aurora-mysql` (ap-northeast-2):
        //   … 8.0.mysql_aurora.3.12.0, **8.4.mysql_aurora.8.4.7**
        // 새 Aurora 는 Aurora 성분도 `8.4.7` 이다 — 3.x 를 가정한 비교는 여기서 깨진다.
        for v1_only in ["8.0.mysql_aurora.3.05.2", "8.0.mysql_aurora.3.12.0"] {
            let v = EngineVersion::parse(v1_only).expect(v1_only);
            assert!(
                !v.supports_explain_json_v2(),
                "Aurora 3.x 는 커뮤니티 8.0 이므로 v2 가 없다: {v1_only}"
            );
            // 지원 대상 판정은 그대로 통과해야 한다(엔진 하한과 별개다).
            assert!(v.is_supported(Engine::AuroraMysql), "{v1_only}");
        }

        let aurora84 = EngineVersion::parse("8.4.mysql_aurora.8.4.7").expect("aurora 8.4.7");
        assert_eq!(aurora84.community, (8, 4, 0));
        assert_eq!(aurora84.aurora, Some((8, 4, 7)));
        assert!(aurora84.supports_explain_json_v2());
        // **하한 판정도 통과해야 한다.** Aurora 성분이 3.x 가 아니라 8.x 가 됐으므로
        // `>= (3,5,0)` 비교가 여전히 성립하는지 확인한다 — 여기서 막히면 새 Aurora 가
        // 통째로 "지원하지 않는 엔진" 이 된다.
        assert!(aurora84.is_supported(Engine::AuroraMysql));
        assert_eq!(aurora84.binary_log_status_stmt(), "SHOW BINARY LOG STATUS");

        // RDS MySQL 8.4.x (실제 목록: 8.4.6 … 8.4.11)
        for raw in ["8.4.6", "8.4.11"] {
            let v = EngineVersion::parse(raw).expect(raw);
            assert!(v.supports_explain_json_v2(), "{raw}");
            assert!(v.is_supported(Engine::Mysql), "{raw}");
        }
    }
    use super::*;

    #[test]
    fn parses_rds_mysql_versions() {
        let v = EngineVersion::parse("8.4.5").unwrap();
        assert_eq!(v.community, (8, 4, 5));
        assert_eq!(v.aurora, None);
        assert!(v.is_supported(Engine::Mysql));

        assert!(
            !EngineVersion::parse("8.0.39")
                .unwrap()
                .is_supported(Engine::Mysql)
        );
        assert!(
            !EngineVersion::parse("5.7.44")
                .unwrap()
                .is_supported(Engine::Mysql)
        );
    }

    #[test]
    fn parses_aurora_versions() {
        let v = EngineVersion::parse("8.0.mysql_aurora.3.05.2").unwrap();
        assert_eq!(v.community, (8, 0, 0));
        assert_eq!(v.aurora, Some((3, 5, 2)));
        assert!(v.is_supported(Engine::AuroraMysql));

        let old = EngineVersion::parse("5.7.mysql_aurora.2.11.4").unwrap();
        assert_eq!(old.aurora, Some((2, 11, 4)));
        assert!(!old.is_supported(Engine::AuroraMysql));
    }

    #[test]
    fn string_comparison_would_be_wrong() {
        // 이 테스트가 EngineVersion 이 존재하는 이유다.
        let a = "8.0.mysql_aurora.3.05.2";
        let b = "8.0.mysql_aurora.3.10.0";
        assert!(b > a, "사전순으로는 3.10 > 3.05 (우연히 맞다)");

        let c = "8.0.mysql_aurora.3.9.0";
        assert!(c > b, "사전순으로는 3.9 > 3.10 — 틀렸다");
        // 수치 비교는 올바르다.
        assert!(
            EngineVersion::parse(c).unwrap().aurora < EngineVersion::parse(b).unwrap().aurora,
            "3.9.0 < 3.10.0 이어야 한다"
        );
    }

    #[test]
    fn aurora_engine_without_aurora_version_is_not_supported() {
        // 판정할 수 없으면 지원한다고 단정하지 않는다.
        let v = EngineVersion::parse("8.4.5").unwrap();
        assert!(!v.is_supported(Engine::AuroraMysql));
    }

    #[test]
    fn rejects_unparseable_versions() {
        for bad in ["", "abc", "8.x.1", "8.4.5.1", "8.0.mysql_aurora.junk"] {
            assert!(EngineVersion::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn binary_log_statement_branches_on_84() {
        assert_eq!(
            EngineVersion::parse("8.4.5")
                .unwrap()
                .binary_log_status_stmt(),
            "SHOW BINARY LOG STATUS"
        );
        assert_eq!(
            EngineVersion::parse("8.0.39")
                .unwrap()
                .binary_log_status_stmt(),
            "SHOW MASTER STATUS"
        );
        // Aurora 3.x 는 커뮤니티 8.0 계열이므로 구 문장이다.
        assert_eq!(
            EngineVersion::parse("8.0.mysql_aurora.3.05.2")
                .unwrap()
                .binary_log_status_stmt(),
            "SHOW MASTER STATUS"
        );
    }

    #[test]
    fn engine_roundtrip() {
        for e in [Engine::Mysql, Engine::AuroraMysql] {
            assert_eq!(Engine::parse(e.as_str()), Some(e));
        }
        assert_eq!(Engine::parse("postgres"), None);
        assert_eq!(Engine::parse("aurora-postgresql"), None);
    }

    #[test]
    fn collect_task_states() {
        assert!(InstanceState::Collecting.should_collect());
        assert!(
            InstanceState::Degraded.should_collect(),
            "degraded 는 관측을 계속해야 회복을 안다"
        );
        assert!(
            InstanceState::Unreachable.should_collect(),
            "태스크를 내리면 half-open 재시도 주체가 사라져 영구히 복구되지 않는다"
        );
        for s in [
            InstanceState::Pending,
            InstanceState::Unsupported,
            InstanceState::Disabled,
            InstanceState::Deleted,
        ] {
            assert!(!s.should_collect(), "{s:?}");
        }
    }
}
