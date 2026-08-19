//! 코드 상수와 Terraform 이 **같은 사실을 두 곳에 적고 있는** 지점을 검사한다.
//!
//! 이 테스트가 없으면 한쪽만 고쳐도 빌드가 통과하고, 그 불일치는 **운영에서만**
//! 드러난다. 아래 항목들은 전부 "조용히 데이터를 잃는" 부류다.
//!
//! Docker 도 AWS 도 필요 없다 — 파일만 읽는다.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` = <root>/crates/dbmon
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("워크스페이스 루트")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{} 를 읽을 수 없다: {e}", p.display()))
}

/// `id = "<name>"` 블록의 `expiration { days = N }` 을 찾는다.
///
/// **`find("days")` 로는 안 된다.** 부분 문자열이라 `noncurrent_days`·`transition { days }`
/// 에 먼저 걸린다. 주입 실험으로 확인: `noncurrent_days = 37` 을 앞에 넣으면 `ttl35 = 37` 로
/// 읽히면서 모든 단정이 통과했다. **조용히 틀린 값을 읽는 게이트는 게이트가 아니다.**
fn lifecycle_days(tf: &str, rule_id: &str) -> u32 {
    // **규칙 블록의 경계 안에서만 찾는다.**
    //
    // 이전 판은 `id = "<rule>"` 뒤에서 첫 `expiration {` 을 찾았다. 그 규칙에
    // `expiration` 이 없으면 **다음 규칙의 것**을 읽는다 — 실측으로 `ttl35` 를 물었을 때
    // `ttl400` 의 405를 돌려주면서 모든 단정이 통과했다. 조용히 틀린 값을 읽는 게이트는
    // 게이트가 아니다.
    let needle = format!("\"{rule_id}\"");
    let id_at = tf
        .find(&needle)
        .unwrap_or_else(|| panic!("lifecycle 규칙 {rule_id} 를 찾을 수 없다"));
    // 규칙 블록의 끝을 찾는다.
    //
    // **리터럴 `"\n  rule {"` 로는 안 된다.** 들여쓰기가 4칸·탭으로 바뀌거나
    // `dynamic "rule"` 로 리팩터링되면 매칭이 사라지고 검색이 파일 끝까지 달려
    // **다음 규칙의 값을 조용히 읽는다** (5차 실측: 4칸 들여쓰기에서 `ttl35` 가 405를 반환).
    //
    // 공백 수에 의존하지 않도록 `rule` 이라는 **단어**를 경계로 본다.
    let after = &tf[id_at..];
    let block_end = after
        .match_indices("rule")
        .find(|(i, _)| {
            let prev_ok = after[..*i]
                .chars()
                .next_back()
                .is_none_or(|c| c.is_whitespace());
            let rest = after[*i + "rule".len()..].trim_start();
            prev_ok && rest.starts_with('{')
        })
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    let block = &after[..block_end];

    // `expiration {` 을 이름 경계로 찾는다 (`noncurrent_version_expiration` 배제).
    let exp = block
        .match_indices("expiration")
        .find(|(i, _)| {
            let prev_ok = block[..*i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            // 뒤에 `{` 가 (공백을 건너뛰고) 와야 한다.
            let rest = block[*i + "expiration".len()..].trim_start();
            prev_ok && rest.starts_with('{')
        })
        .map(|(i, _)| i)
        .unwrap_or_else(|| panic!("{rule_id} 규칙에 expiration 블록이 없다"));

    // **`expiration` 블록이 정확히 하나여야 한다.** 둘 이상이면 어느 것을 읽는지
    // 알 수 없고, 0개면 경계 탐색이 실패한 것이다. 어느 쪽이든 조용히 넘기지 않는다.
    let exp_count = block
        .match_indices("expiration")
        .filter(|(i, _)| {
            let prev_ok = block[..*i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            prev_ok
                && block[*i + "expiration".len()..]
                    .trim_start()
                    .starts_with('{')
        })
        .count();
    assert_eq!(
        exp_count, 1,
        "{rule_id} 규칙의 expiration 블록이 {exp_count}개다 — 경계 탐색이 잘못됐다"
    );

    let open = block[exp..].find('{').expect("확인됨") + exp;
    let close = block[open..]
        .find('}')
        .unwrap_or_else(|| panic!("{rule_id} 의 expiration 블록이 닫히지 않았다"))
        + open;
    let inner = &block[open + 1..close];

    // 정확히 `days` 라는 이름의 인자만. 주석(`#` 뒤)은 제거한다.
    inner
        .lines()
        .filter_map(|l| {
            let code = l.split('#').next().unwrap_or(l);
            let (k, v) = code.split_once('=')?;
            (k.trim() == "days").then(|| v.trim().to_string())
        })
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{rule_id} 의 expiration.days 를 파싱할 수 없다"))
}

/// **항목 TTL 이 참조하는 S3 객체의 Lifecycle 보다 길면 안 된다** (04 §2.3 불변식).
///
/// 깨지면: DynamoDB 항목은 아직 `plan_s3_key` 를 가리키는데 S3 객체가 이미 삭제돼
/// 플랜 화면이 404 를 낸다. 항목이 만료될 때까지 그 상태가 유지되고, **에러가 아니라
/// 빈 화면**으로 나타나므로 알아채기 어렵다.
#[test]
fn item_ttl_does_not_outlive_the_s3_object_it_references() {
    let tf = read("infra/layers/10-foundation/main.tf");
    let ttl35 = lifecycle_days(&tf, "ttl35");
    let ttl400 = lifecycle_days(&tf, "ttl400");

    let item_ttl = dbmon_core::HOT_TTL_DAYS as u32;
    assert!(
        ttl35 > item_ttl,
        "S3 ttl35/ Lifecycle {ttl35}일이 항목 TTL {item_ttl}일보다 길어야 한다. \
         HOT_TTL_DAYS 를 올렸으면 infra/layers/10-foundation/main.tf 의 ttl35 규칙도 올린다"
    );
    // `SLOWEST` 고정 샘플은 400일 티어를 참조한다.
    assert!(
        ttl400 > 400,
        "S3 ttl400/ Lifecycle 이 400일보다 길어야 한다 (실제 {ttl400}일)"
    );
    // 여유가 너무 크면 지워야 할 객체에 계속 과금한다.
    assert!(
        ttl35 - item_ttl <= 10,
        "여유가 {}일이다. 5일 정도면 충분하다 — 그 이상은 삭제되지 않는 객체에 과금이다",
        ttl35 - item_ttl
    );
}

/// 핫 티어 경계 < 항목 TTL < S3 Lifecycle 이 한 줄로 성립해야 한다.
#[test]
fn retention_tiers_are_strictly_increasing() {
    let tf = read("infra/layers/10-foundation/main.tf");
    let tier = dbmon_core::HOT_TIER_DAYS;
    let ttl = dbmon_core::HOT_TTL_DAYS;
    let s3 = lifecycle_days(&tf, "ttl35") as i64;
    assert!(
        tier < ttl && ttl < s3,
        "보관 단계가 단조 증가해야 한다: 핫 경계 {tier}일 < 항목 TTL {ttl}일 < S3 {s3}일"
    );
}

/// 샤드 수는 코드와 문서 양쪽에 있다. 다르면 샤드 하나가 아무도 소유하지 않거나
/// 두 워커가 같은 샤드를 소유한다 (F1 불변식 `ShardsOwned 합계 = 64 또는 0`).
#[test]
fn shard_count_matches_the_design_document() {
    // **`doc.contains("64")` 는 무의미하다.** `64배 개선`·`u64`·`86400초` 에 걸린다.
    // 주입 실험으로 확인: 4·8·16·32·256·1024 로 바꿔도 통과했다.
    // 샤드를 명시하는 문맥을 찾아야 한다.
    let doc = read("docs/05-collector.md");
    let n = dbmon_core::ports::stores::SHARD_COUNT;
    let patterns = [
        format!("SHARD_COUNT = {n}"),
        format!("SHARD_COUNT({n})"),
        format!("샤드 수 = SHARD_COUNT({n})"),
    ];
    assert!(
        patterns.iter().any(|p| doc.contains(p)),
        "05-collector.md 에 샤드 수 {n} 을 명시하는 문맥이 없다. 찾은 패턴: {patterns:?}"
    );
    // 2의 거듭제곱이어야 리샤딩이 단순하다.
    assert!(
        n.is_power_of_two(),
        "샤드 수 {n} 은 2의 거듭제곱이어야 한다"
    );
}

/// **Terraform 의 컨테이너 `stopTimeout` 기본값이 앱의 셧다운 예산보다 커야 한다.**
///
/// 깨지면 SIGKILL 이 먼저 도착해 다이제스트 누산기와 쓰기 버퍼가 유실된다.
/// Terraform 쪽에도 변수 검증이 있지만, 그 검증은 **plan 을 돌려야** 발동한다.
/// 이 테스트는 기본값끼리의 관계를 커밋 시점에 잡는다.
#[test]
fn stop_timeout_default_exceeds_shutdown_grace_default() {
    let tf = read("infra/layers/40-compute/variables.tf");

    let default_of = |var: &str| -> u64 {
        let start = tf
            .find(&format!("variable \"{var}\""))
            .unwrap_or_else(|| panic!("변수 {var} 를 찾을 수 없다"));
        let tail = &tf[start..];
        let d = tail
            .find("default")
            .unwrap_or_else(|| panic!("{var} 에 default 가 없다"));
        tail[d..]
            .split('=')
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("{var} 의 default 를 파싱할 수 없다"))
    };

    let stop = default_of("stop_timeout_secs");
    let grace = default_of("shutdown_grace_secs");
    assert!(
        stop > grace,
        "stop_timeout_secs({stop}s) 가 shutdown_grace_secs({grace}s) 보다 커야 한다"
    );
    // Fargate 상한.
    assert!(
        stop <= 120,
        "Fargate 의 stopTimeout 상한은 120초다 (현재 {stop})"
    );
}

/// 컨테이너 헬스체크는 `/healthz`, 대상 그룹 헬스체크는 `/readyz` 여야 한다.
///
/// **바꾸면 standby 태스크가 무한 재시작한다** — standby 는 정상적으로 `/readyz` 503 을
/// 낸다. 그걸 컨테이너 헬스체크로 쓰면 ECS 가 건강한 태스크를 계속 교체한다.
#[test]
fn container_and_target_group_healthchecks_are_not_swapped() {
    // **주석을 걷어낸다.** ecs.tf 의 주석은 "왜 /readyz 를 쓰지 않는가" 를 설명하므로
    // 파일 전체를 grep 하면 그 설명에 걸린다.
    let strip_comments = |src: String| -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let ecs = strip_comments(read("infra/layers/40-compute/ecs.tf"));
    let alb = strip_comments(read("infra/layers/40-compute/alb.tf"));

    // 컨테이너 헬스체크는 바이너리 서브커맨드를 쓴다(이미지에 curl 이 없다).
    assert!(
        ecs.contains("healthcheck"),
        "ecs.tf 의 컨테이너 healthCheck 가 dbmon healthcheck 를 쓰지 않는다"
    );
    assert!(
        !ecs.contains("/readyz"),
        "컨테이너 헬스체크에 /readyz 를 쓰면 standby 가 무한 재시작한다"
    );
    assert!(
        alb.contains("/readyz"),
        "대상 그룹 헬스체크는 /readyz 여야 한다 (standby 를 대상에서 빼기 위해)"
    );
}
