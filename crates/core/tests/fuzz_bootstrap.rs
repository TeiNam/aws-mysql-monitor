//! 부트스트랩 SQL 조립 퍼징 (T-18).
//!
//! # 왜 단위 테스트 밖에 두는가
//!
//! 페이로드 표를 한곳에 모아 **모든 삽입 지점에 교차 적용**한다. 각 모듈의 단위
//! 테스트는 자기 함수만 보므로, "사용자명은 막았는데 스키마명은 안 막았다" 부류를
//! 잡지 못한다 — T-18 이 지적한 것이 정확히 그 부류다(스키마는 막고 사용자명은 뚫림).
//!
//! # 무엇을 실패로 보는가
//!
//! **문장 구조를 깨는 것만** 이다. `GRANT ADMIN ON *.*` 처럼 MySQL 이 거부하는 문장이
//! 만들어지는 것은 결함이 아니다 — 그건 실행 시 오류가 되고 권한을 늘리지 않는다.
//! 처음 이 파일을 쓸 때 그 둘을 섞어 오탐 3건을 만들었다.
#[test]
fn fuzz_bootstrap_sql_assembly() {
    use dbmon_core::bootstrap::{AuthMethod, Desired, PrivilegeMode, sql};
    use dbmon_core::ident::has_statement_break;

    // 사용자·호스트·스키마·권한 이름 각각에 넣어 볼 페이로드.
    let payloads = [
        "a'; DROP DATABASE x; --",
        "a\"; DROP",
        "a`; DROP",
        "a\\'; DROP",
        "a\\",
        "a\\\\",
        "a\n; DROP",
        "a\r\nDROP",
        "a\0",
        "a\tb",
        "a/*",
        "a*/",
        "a--",
        "a#",
        "a;",
        "a' OR '1'='1",
        "a'@'%' IDENTIFIED BY 'p'; GRANT ALL ON *.* TO 'e'@'%'; --",
        "%",
        "%%",
        "_",
        "*",
        "*.*",
        "..",
        "a.b",
        &"a".repeat(65),
        &"a".repeat(200),
        "",
        "한글",
        "🔥",
        "a\u{2028}b",
        "a\u{0085}b",
        "a\u{00a0}b",
        "ADMIN",
        "admin",
        "A",
        "1a",
        "-a",
        "_a",
    ];

    let mut escaped = Vec::new();
    for p in payloads {
        for which in ["user", "host", "schema"] {
            let d = Desired {
                user: if which == "user" {
                    p.into()
                } else {
                    "dbmon".into()
                },
                host: if which == "host" {
                    p.into()
                } else {
                    "10.1.%".into()
                },
                auth: AuthMethod::IamDbAuth,
                mode: if which == "schema" {
                    PrivilegeMode::Least
                } else {
                    PrivilegeMode::Broad
                },
                schemas: if which == "schema" {
                    vec![p.into()]
                } else {
                    vec![]
                },
            };
            // 검증을 통과했다면, 만들어진 문장에 구분자가 없어야 한다.
            if d.validate().is_ok() {
                if let Ok(stmt) = sql::create_user(&d) {
                    let raw = stmt.to_execute();
                    if has_statement_break(raw) {
                        escaped.push(format!("[{which}] {p:?} → {raw}"));
                    }
                    // 인용부호 짝이 맞는지 (문장이 인용 안에서 끝나면 안 된다)
                    let quotes = raw.matches('\'').count();
                    if quotes % 2 != 0 {
                        escaped.push(format!("[{which}] {p:?} → 홀수 인용부호: {raw}"));
                    }
                }
                for (scope, privs) in
                    dbmon_core::bootstrap::grants::GrantSet::default().missing_from(&d.grant_set())
                {
                    if let Ok(stmt) = sql::grant(&d, &scope, &privs) {
                        let raw = stmt.to_execute();
                        if has_statement_break(raw) {
                            escaped.push(format!("[{which}/grant] {p:?} → {raw}"));
                        }
                    }
                }
            }
        }
        // 권한 이름 자리 — **인용할 수 없는 자리다.** 통과하는 이름이 있는 것 자체는
        // 문제가 아니다(`GRANT ADMIN ON *.*` 은 MySQL 이 거부하는 문장이고 구조를
        // 깨지 않는다). 문제는 **문장 구조를 깨는** 이름이 통과하는 것이다.
        //
        // 실제로 이 자리에는 우리 상수만 들어온다(`missing_from` 이 `desired` 만
        // 순회한다). 이 검사는 그 사실이 깨질 때를 대비한 그물이다.
        let d = Desired {
            user: "dbmon".into(),
            host: "10.1.%".into(),
            auth: AuthMethod::IamDbAuth,
            mode: PrivilegeMode::Broad,
            schemas: vec![],
        };
        let privs = vec![p.to_string()];
        if let Ok(stmt) = sql::grant(
            &d,
            &dbmon_core::bootstrap::grants::GrantScope::Global,
            &privs,
        ) {
            let raw = stmt.to_execute();
            if has_statement_break(raw) || raw.matches('\'').count() % 2 != 0 {
                escaped.push(format!("[privilege] {p:?} → 구조를 깼다: {raw}"));
            }
        }
    }
    assert!(
        escaped.is_empty(),
        "우회 발견 {}건:\n{}",
        escaped.len(),
        escaped.join("\n")
    );
}

/// `SHOW GRANTS` 파서가 **권한을 실제보다 넓게** 판정하는 입력을 찾는다.
/// 넓게 판정하면 필요한 GRANT 를 건너뛴다 — 그게 위험한 방향이다.
#[test]
fn fuzz_grant_parser_never_overstates() {
    use dbmon_core::bootstrap::grants::{GrantScope, parse_grants};

    // 이 줄들은 전역 SELECT 를 주지 **않는다.** 파서가 준다고 판정하면 결함이다.
    let not_global_select = [
        "GRANT SELECT ON `db`.* TO `u`@`h`",
        "GRANT SELECT ON `db`.`t` TO `u`@`h`",
        "GRANT USAGE ON *.* TO `u`@`h`",
        "GRANT PROCESS ON *.* TO `u`@`h`",
        "GRANT `role`@`%` TO `u`@`h`",
        // 인용 안에 `*.*` 가 있다.
        "GRANT SELECT ON `*.*`.* TO `u`@`h`",
        "GRANT SELECT ON `x`.`*` TO `u`@`h`",
        // 사용자 이름이 전역처럼 보인다.
        "GRANT SELECT ON `db`.* TO `SELECT ON *.* TO evil`@`h`",
    ];
    for line in not_global_select {
        let (set, unparsed) = parse_grants([line]);
        assert!(
            !set.covers(&GrantScope::Global, "SELECT"),
            "과대 판정: {line}\n  → unparsed={unparsed:?}"
        );
    }

    // 이 줄들은 파싱 실패해야 한다(모르면 차단된다).
    let must_not_parse_as_privileges = [
        "GRANT SELECT ON *.tbl TO `u`@`h`", // 세션 의존
        "GRANT SELECT `db`.* TO `u`@`h`",   // ON 없음 → 롤로 오인?
        "SHOW GRANTS FOR `u`@`h`",
        "REVOKE SELECT ON *.* FROM `u`@`h`",
    ];
    for line in must_not_parse_as_privileges {
        let (set, _) = parse_grants([line]);
        assert!(
            !set.covers(&GrantScope::Global, "SELECT"),
            "이상한 줄에서 전역 SELECT 를 읽었다: {line}"
        );
    }
}
