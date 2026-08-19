//! M1-6 / M1-13 — 다이제스트 정규화 검증 스파이크.
//!
//! `normalize` 크레이트의 존재 이유는 `normalize(원문) == normalize(DIGEST_TEXT)` 다
//! ([ADR-011](../../../../docs/03-decisions.md)). 이 등가성이 3소스 조인 키의 근거이므로
//! **실제 MySQL 로 확인해야** 한다. 규칙표는 가설이었다.
//!
//! ```
//! docker compose up -d
//! cargo test -p dbmon --test m1_digest -- --nocapture
//! ```

mod support;

use dbmon_normalize::normalize;
use mysql_async::prelude::*;
use support::*;

/// M1-6 — 코퍼스 전량에서 정규화가 수렴하는가.
#[tokio::test]
async fn m1_6_normalize_converges_with_server_digest_text() {
    let mut conn = conn_or_skip!(MYSQL84, ROOT);
    let corpus = corpus();
    assert!(corpus.len() >= 60, "코퍼스가 너무 작다: {}", corpus.len());

    let mut mismatches: Vec<(String, String, String)> = Vec::new();
    let mut unparsed: Vec<String> = Vec::new();
    let mut golden: Vec<serde_json::Value> = Vec::new();

    for sql in &corpus {
        let Some((digest_text, digest_hash)) = server_digest(&mut conn, sql).await else {
            unparsed.push(sql.clone());
            continue;
        };
        let from_raw = normalize(sql);
        let from_digest = normalize(&digest_text);
        if from_raw.canonical != from_digest.canonical {
            mismatches.push((
                sql.clone(),
                from_raw.canonical.clone(),
                from_digest.canonical.clone(),
            ));
        }
        golden.push(serde_json::json!({
            "sql": sql,
            "digest_text": digest_text,
            "mysql_digest": digest_hash,
            "app_digest": from_raw.app_digest,
            "statement_type": from_raw.statement_type.as_str(),
        }));
    }

    // 오프라인(도커 없는 CI)에서도 같은 검증을 돌릴 수 있게 골든 파일을 갱신한다.
    let out = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/digest_golden_84.json"
    );
    std::fs::write(out, serde_json::to_string_pretty(&golden).unwrap()).unwrap();

    report(
        "M1-6 다이제스트 수렴",
        &[
            ("코퍼스".into(), corpus.len().to_string()),
            ("서버가 해석 못함".into(), unparsed.len().to_string()),
            ("불일치".into(), mismatches.len().to_string()),
            ("골든 파일".into(), out.into()),
        ],
    );
    for s in &unparsed {
        println!("  [서버 해석 실패] {s}");
    }
    for (sql, raw, dig) in &mismatches {
        println!("\n  [불일치] {sql}\n    원문 정규화   : {raw}\n    다이제스트 정규화: {dig}");
    }

    assert!(
        unparsed.is_empty(),
        "코퍼스에 유효하지 않은 SQL 이 있다: {unparsed:?}"
    );
    assert!(
        mismatches.is_empty(),
        "{}건이 수렴하지 않는다. 위 출력의 두 줄 차이를 보고 정규화 규칙을 고친다.",
        mismatches.len()
    );
}

/// M1-6b — `app_digest` 는 `mysql_digest` 보다 **거칠다**. 관계가 N:1 이다.
///
/// # 실측으로 뒤집힌 전제
///
/// [04 §2.3](../../../../docs/04-data-model.md) 의 `DigestText.mysql_digests` 는
/// `{instance_id: mysql_digest}` — 인스턴스당 값 **하나**를 가정했다. 그런데
/// MySQL 의 `DIGEST` 는 **식별자 대소문자를 구분한다**(`lower_case_table_names=0`):
///
/// ```text
/// SELECT * FROM orders WHERE id = 1   →  DIGEST_TEXT: SELECT * FROM `orders` WHERE `id` = ?
/// SELECT * FROM ORDERS WHERE ID = 6   →  DIGEST_TEXT: SELECT * FROM `ORDERS` WHERE `ID` = ?
///                                        → 서로 다른 DIGEST
/// ```
///
/// 우리는 수렴성을 위해 식별자를 소문자로 접으므로(`lexer::fold_ident`) 둘이 같은
/// `app_digest` 가 된다. **의도한 동작이지만 매핑이 1:1 이 아니다.**
/// → `mysql_digests` 는 인스턴스당 **집합**이어야 하고, M4-16 의 학습도 N:1 을 다뤄야 한다.
#[tokio::test]
async fn m1_6b_app_digest_is_coarser_than_mysql_digest() {
    let mut conn = conn_or_skip!(MYSQL84, ROOT);
    // MySQL 도 하나로 묶는 변형 — 공백·주석·키워드 대소문자·백틱·리터럴 값.
    let mysql_also_merges = [
        "SELECT * FROM orders WHERE id = 1",
        "select * from orders where id = 2",
        "SELECT  *\n FROM   orders\tWHERE id  =  3",
        "/* c1 */ SELECT * FROM orders WHERE id = 4 -- c2",
        "SELECT * FROM `orders` WHERE `id` = 5",
    ];
    // MySQL 은 **다른 다이제스트로 보지만** 우리는 하나로 묶는 변형.
    let mysql_splits = [
        "SELECT * FROM ORDERS WHERE ID = 7",  // 식별자 대소문자
        "SELECT * FROM Orders WHERE Id = 8",  // 식별자 대소문자
        "SELECT * FROM orders WHERE id = 9;", // 후행 세미콜론
    ];

    let mut app = std::collections::BTreeSet::new();
    let mut server_merged = std::collections::BTreeSet::new();
    let mut server_all = std::collections::BTreeSet::new();
    for v in mysql_also_merges {
        app.insert(normalize(v).app_digest);
        if let Some((_, h)) = server_digest(&mut conn, v).await {
            server_merged.insert(h.clone());
            server_all.insert(h);
        }
    }
    for v in mysql_splits {
        app.insert(normalize(v).app_digest);
        if let Some((_, h)) = server_digest(&mut conn, v).await {
            server_all.insert(h);
        }
    }

    report(
        "M1-6b app_digest 대 mysql_digest",
        &[
            (
                "변형 수".into(),
                (mysql_also_merges.len() + mysql_splits.len()).to_string(),
            ),
            ("app_digest 종류".into(), app.len().to_string()),
            (
                "mysql_digest 종류 (전체)".into(),
                server_all.len().to_string(),
            ),
            (
                "mysql_digest 종류 (MySQL 도 묶는 변형만)".into(),
                server_merged.len().to_string(),
            ),
            (
                "MySQL 이 구분하는 것".into(),
                "식별자 대소문자, 후행 세미콜론".into(),
            ),
            (
                "→ 결론".into(),
                "app_digest 1 : mysql_digest N. mysql_digests 는 집합이어야 한다".into(),
            ),
        ],
    );

    assert_eq!(app.len(), 1, "표기만 다른 쿼리가 갈라졌다: {app:?}");
    assert_eq!(
        server_merged.len(),
        1,
        "공백·주석·키워드 대소문자·백틱은 MySQL 도 흡수해야 한다"
    );
    assert!(
        server_all.len() > 1,
        "MySQL 이 식별자 대소문자·세미콜론을 구분하지 않는 설정이라면 \
         이 환경에서는 N:1 이 관측되지 않는다. 그래도 설계는 집합을 가정해야 한다 \
         — 대상 인스턴스마다 lower_case_table_names 가 다르다."
    );
}

/// M1-13 — `DIGEST_TEXT` 절단이 `DIGEST` 해시도 바꾸는가.
///
/// 해시가 **같다면** `app_digest` 의 역할이 크게 줄어든다(크로스 인스턴스 그룹핑을
/// `mysql_digest` 로 할 수 있다). 해시가 **다르다면** [ADR-011](../../../../docs/03-decisions.md)
/// 의 `app_digest` 가 필수다.
#[tokio::test]
async fn m1_13_digest_hash_across_max_digest_length() {
    let mut narrow = conn_or_skip!(MYSQL84, ROOT);
    let mut wide = conn_or_skip!(MYSQL84_WIDE, ROOT);

    // ⚠ 읽어야 할 변수는 `max_digest_length` 다.
    // `performance_schema_max_digest_length` 는 **저장 길이**만 바꾸고 해시를 바꾸지 않는다.
    // 처음에 후자만 올려서 "해시가 같다"는 잘못된 결론을 냈다 (M1-13 1차 실행).
    let narrow_len = var(&mut narrow, "max_digest_length").await.unwrap();
    let wide_len = var(&mut wide, "max_digest_length").await.unwrap();
    assert_ne!(
        narrow_len, wide_len,
        "두 컨테이너의 max_digest_length 가 같다"
    );

    // **다이제스트 텍스트가** 1024바이트를 확실히 넘어야 한다.
    // 긴 `IN` 절은 MySQL 이 `IN (...)` 로 축약해 버려서 절단이 일어나지 않는다.
    let long_sql = wide_digest_sql(300);
    assert!(
        long_sql.len() > 3000,
        "생성된 SQL 이 짧다: {}",
        long_sql.len()
    );

    let (narrow_text, narrow_hash) = server_digest(&mut narrow, &long_sql).await.unwrap();
    let (wide_text, wide_hash) = server_digest(&mut wide, &long_sql).await.unwrap();

    let narrow_limit: Option<u32> = narrow_len.parse().ok();
    let wide_limit: Option<u32> = wide_len.parse().ok();
    let narrow_truncated = dbmon_normalize::looks_truncated_by_server(&narrow_text, narrow_limit);
    let wide_truncated = dbmon_normalize::looks_truncated_by_server(&wide_text, wide_limit);

    // 우리 정규화는 인스턴스 설정과 무관하게 같아야 한다 (결정론적 8192 절단).
    let app_from_raw = normalize(&long_sql).app_digest;

    report(
        "M1-13 절단과 해시",
        &[
            ("SQL 길이".into(), long_sql.len().to_string()),
            (
                format!("max_digest_length ({})", MYSQL84.label),
                narrow_len.clone(),
            ),
            (
                format!("max_digest_length ({})", MYSQL84_WIDE.label),
                wide_len.clone(),
            ),
            (
                "narrow DIGEST_TEXT 길이".into(),
                narrow_text.len().to_string(),
            ),
            ("wide DIGEST_TEXT 길이".into(), wide_text.len().to_string()),
            ("narrow 절단됨(`...`)".into(), narrow_truncated.to_string()),
            ("wide 절단됨(`...`)".into(), wide_truncated.to_string()),
            ("narrow DIGEST".into(), narrow_hash.clone()),
            ("wide DIGEST".into(), wide_hash.clone()),
            (
                "→ 해시 동일?".into(),
                if narrow_hash == wide_hash {
                    "같다 → mysql_digest 를 크로스 인스턴스 키로 쓸 수 있다".into()
                } else {
                    "다르다 → app_digest 필수 (ADR-011 유지)".into()
                },
            ),
            ("app_digest (양쪽 동일해야)".into(), app_from_raw.clone()),
        ],
    );

    // 우리 해시는 서버 설정과 무관해야 한다. 이게 깨지면 app_digest 의 의미가 없다.
    assert_eq!(
        normalize(&long_sql).app_digest,
        app_from_raw,
        "app_digest 는 결정론적이어야 한다"
    );
    assert!(
        narrow_truncated,
        "1024 설정에서 4KB 쿼리가 절단되지 않았다 — 전제가 틀렸다"
    );
}

/// 절단된 `DIGEST_TEXT` 를 정규화하면 원문 정규화와 **다르다**는 것을 명시적으로 기록한다.
/// 그래서 `mysql_digest ↔ app_digest` 학습 매핑(M4-16)이 필요하다.
#[tokio::test]
async fn m1_13b_truncated_digest_text_needs_mapping() {
    let mut conn = conn_or_skip!(MYSQL84, ROOT);
    let long_sql = long_running_sql(4096, 0.0);
    let (digest_text, _) = server_digest(&mut conn, &long_sql).await.unwrap();

    let from_raw = normalize(&long_sql);
    let from_digest = normalize(&digest_text);

    report(
        "M1-13b 절단본의 app_digest",
        &[
            ("원문 app_digest".into(), from_raw.app_digest.clone()),
            ("절단본 app_digest".into(), from_digest.app_digest.clone()),
            (
                "→".into(),
                if from_raw.app_digest == from_digest.app_digest {
                    "같다 (IN 절 축약이 절단을 흡수했다)".into()
                } else {
                    "다르다 → mysql_digest 매핑 학습이 필요하다 (M4-16)".into()
                },
            ),
        ],
    );
    // 어느 쪽이든 설계가 대응한다. 결과를 문서에 남기는 것이 목적이다.
}

/// M1-16 — `information_schema.PROCESSLIST WHERE ID = ?` 의 비용.
///
/// [ADR-005](../../../../docs/03-decisions.md) 의 대가 항목이다. 비싸면 심층 조회를
/// 후보 전체에 1회로 배치해야 한다.
#[tokio::test]
async fn m1_16_information_schema_processlist_cost() {
    let mut conn = conn_or_skip!(MYSQL84, ROOT);

    // 스레드를 늘려 최악 조건에 가깝게 만든다.
    let mut idle: Vec<mysql_async::Conn> = Vec::new();
    for _ in 0..50 {
        if let Some(c) = connect(MYSQL84, LOADGEN).await {
            idle.push(c);
        }
    }

    let measure = |label: &'static str, sql: String| async move {
        let mut c = connect(MYSQL84, ROOT).await.unwrap();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let _: Vec<mysql_async::Row> = c.query(sql.clone()).await.unwrap();
        }
        let per_call = start.elapsed().as_micros() as f64 / 100.0;
        let _ = c.disconnect().await;
        (label, per_call)
    };

    let ps = measure(
        "performance_schema.processlist (전체)",
        "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE FROM performance_schema.processlist \
         WHERE INFO IS NOT NULL AND TIME >= 2"
            .into(),
    )
    .await;
    let is_targeted = measure(
        "information_schema.PROCESSLIST WHERE ID IN (?)",
        "SELECT ID, DB, USER, HOST, TIME, INFO FROM information_schema.PROCESSLIST WHERE ID IN (1,2,3)"
            .into(),
    )
    .await;
    let is_full = measure(
        "information_schema.PROCESSLIST (전체)",
        "SELECT ID, DB, USER, HOST, TIME, INFO FROM information_schema.PROCESSLIST".into(),
    )
    .await;

    let threads: u64 = conn
        .query_first("SELECT COUNT(*) FROM performance_schema.threads")
        .await
        .unwrap()
        .unwrap_or(0);

    report(
        "M1-16 조회 비용 (호출당 마이크로초)",
        &[
            ("스레드 수".into(), threads.to_string()),
            (ps.0.into(), format!("{:.0}µs", ps.1)),
            (is_targeted.0.into(), format!("{:.0}µs", is_targeted.1)),
            (is_full.0.into(), format!("{:.0}µs", is_full.1)),
            (
                "→ 타깃 조회 / PS 폴링 비율".into(),
                format!("{:.1}배", is_targeted.1 / ps.1),
            ),
        ],
    );

    for c in idle {
        let _ = c.disconnect().await;
    }
    // 판정은 문서에서 한다. 여기서는 수치를 남긴다.
}
