//! 시스템 스키마 목록 — **이 프로젝트의 유일한 정의다.**
//!
//! # 왜 한 곳에 두는가
//!
//! 원래 이 목록은 [`Excludes::with_defaults`](crate::ports::target_db::Excludes) 안에
//! 리터럴로 있었다. 부트스트랩과 화면이 같은 판정을 필요로 하면서 목록이 세 벌이 될
//! 수 있었고, 세 벌이 되는 순간 한쪽만 갱신되는 결함이 생긴다.
//!
//! # 권한 모드 A 에서 이 목록이 하는 일
//!
//! 모니터링 계정은 `GRANT SELECT ON *.*` 으로 **전부 읽는다** — MySQL 에는 "전체에서
//! 일부 스키마만 제외" 하는 `GRANT` 문법이 없다. 그래서 제외는 부여 단계가 아니라
//! **수집·표시 단계**에서 한다:
//!
//! | 단계 | 제외하는 방법 |
//! |---|---|
//! | 슬로우 쿼리 탐지 | `Excludes.schemas` 로 `processlist` 에서 걸러낸다 |
//! | 다이제스트 롤업 | 같은 목록으로 걸러낸다 |
//! | 스키마 열거 화면 | [`is_system_schema`] 로 숨긴다 |
//!
//! 즉 계정은 읽을 수 있지만 **우리가 읽지 않는다.** 권한으로 막는 것과 다르다는 점을
//! 분명히 해 둔다 — `rds-db:connect` 를 가진 다른 주체는 이 계정으로 붙어 시스템
//! 스키마를 읽을 수 있다([07 §2.3](../../../../docs/07-credentials-bootstrap.md) 모드 A
//! 의 트레이드오프).

/// MySQL·RDS 가 만드는 스키마. 사용자가 만든 것이 아니다.
///
/// 순서는 알파벳순이고, 판정은 대소문자를 무시한다 — `lower_case_table_names` 설정에
/// 따라 스키마 이름의 대소문자 취급이 갈린다.
pub const SYSTEM_SCHEMAS: &[&str] = &[
    // RDS 의 DMS 가 만든다. 사용자가 만든 것으로 보이지만 아니다.
    "awsdms_control",
    "information_schema",
    // Aurora/RDS 의 memcached 플러그인.
    "innodb_memcache",
    "mysql",
    // MySQL 8 InnoDB Cluster (Aurora 는 안 쓰지만 마이그레이션 원본에 남아 있다).
    "mysql_innodb_cluster_metadata",
    "performance_schema",
    "sys",
];

/// 이 스키마가 시스템 스키마인가.
pub fn is_system_schema(name: &str) -> bool {
    SYSTEM_SCHEMAS.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// 통계 신선도 확인에 필요한 `mysql` 스키마 테이블 (ADR-016).
///
/// 모드 A 는 `*.*` 을 주므로 이 목록이 **부여**에는 쓰이지 않는다. 하지만 모드 B
/// (화이트리스트)에서는 이 두 테이블을 테이블 단위로 열어야 통계 신선도를 볼 수 있고,
/// 그때 필요한 정의다.
pub const STATS_TABLES: &[(&str, &str)] = &[
    ("mysql", "innodb_table_stats"),
    ("mysql", "innodb_index_stats"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_standard_system_schemas_are_covered() {
        for s in ["mysql", "sys", "performance_schema", "information_schema"] {
            assert!(is_system_schema(s), "{s} 가 빠졌다");
        }
    }

    #[test]
    fn user_schemas_are_not_system_schemas() {
        for s in ["shop", "orders", "mysql_app", "sysadmin", "my_sql"] {
            assert!(!is_system_schema(s), "{s} 를 시스템 스키마로 봤다");
        }
    }

    /// `lower_case_table_names` 에 따라 대소문자 취급이 갈리므로 무시하고 비교한다.
    #[test]
    fn the_check_ignores_case() {
        for s in ["MySQL", "PERFORMANCE_SCHEMA", "Sys"] {
            assert!(is_system_schema(s), "{s} 를 놓쳤다");
        }
    }

    /// 목록은 정렬돼 있어야 한다 — 항목을 추가할 때 중복을 눈으로 잡을 수 있다.
    #[test]
    fn the_list_is_sorted_and_unique() {
        let mut sorted = SYSTEM_SCHEMAS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, SYSTEM_SCHEMAS, "정렬·중복을 확인한다");
    }

    #[test]
    fn stats_tables_live_in_a_system_schema() {
        for (db, _) in STATS_TABLES {
            assert!(is_system_schema(db));
        }
    }
}
