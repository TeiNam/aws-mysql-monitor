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
    // **설계 문서는 공개하지 않는다** — `.claude/docs/` 는 gitignore 된다. 클론·CI 에는
    // 없으므로 부재를 견딘다. 있으면 검사하고, 없으면 아래 2의 거듭제곱 불변식만 본다.
    //
    // 문서가 없을 때 조용히 통과하는 것이 마음에 걸리지만, 대안은 둘 다 나쁘다:
    // 설계 문서를 공개하거나(사용자가 원하지 않는다), CI 를 항상 실패시키거나.
    let n = dbmon_core::ports::stores::SHARD_COUNT;
    let doc_path = repo_root().join(".claude/docs/05-collector.md");
    if let Ok(doc) = std::fs::read_to_string(&doc_path) {
        let patterns = [
            format!("SHARD_COUNT = {n}"),
            format!("SHARD_COUNT({n})"),
            format!("샤드 수 = SHARD_COUNT({n})"),
        ];
        assert!(
            patterns.iter().any(|p| doc.contains(p)),
            "05-collector.md 에 샤드 수 {n} 을 명시하는 문맥이 없다. 찾은 패턴: {patterns:?}"
        );
    }
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

/// **슬로우로그 IAM 범위가 코드가 실제로 쓰는 것과 일치해야 한다.**
///
/// 슬로우로그에는 SQL 리터럴 — 즉 개인정보가 실릴 수 있는 텍스트 — 가 들어간다.
/// `/aws/rds/*` 는 audit 로그까지 포함하고, audit 로그는 **모든 문장**을 담으므로
/// 노출 범위가 전혀 다르다.
#[test]
fn slowlog_iam_scope_matches_what_the_code_reads() {
    let iam = std::fs::read_to_string("../../infra/layers/40-compute/iam.tf").expect("iam.tf");

    // 코드는 `FilterLogEvents` 만 부른다.
    let uses_describe_streams = std::fs::read_to_string("src/slowlog/fetch.rs")
        .expect("fetch.rs")
        .contains("describe_log_streams");
    assert!(
        !uses_describe_streams,
        "코드가 DescribeLogStreams 를 쓴다 — IAM 에 다시 추가해야 한다"
    );
    assert!(
        !iam.contains("logs:DescribeLogStreams"),
        "IAM 에 쓰지 않는 액션이 남아 있다 — 권한이 근거 없이 넓다"
    );

    // 폴백 범위가 `/aws/rds/*` 로 넓어지면 안 된다.
    assert!(
        !iam.contains(r#"log-group:/aws/rds/*""#),
        "슬로우로그 IAM 이 /aws/rds/* 로 열려 있다 — audit 로그(모든 문장)까지 읽힌다"
    );
    assert!(
        iam.contains("/aws/rds/instance/*/slowquery"),
        "슬로우로그 그룹으로 좁혀지지 않았다"
    );

    // 코드가 만드는 로그 그룹 이름 규칙과 IAM 패턴이 맞아야 한다.
    let fetch = std::fs::read_to_string("src/slowlog/fetch.rs").expect("fetch.rs");
    assert!(
        fetch.contains("/aws/rds/instance/{}/slowquery"),
        "로그 그룹 이름 규칙이 바뀌었다 — IAM 패턴도 함께 고쳐야 한다"
    );

    // **Aurora 는 클러스터 그룹이다.** 코드가 그 형태를 만드는데 IAM 폴백에 없으면
    // 권한이 막는다 — 반대로 IAM 에만 있고 코드가 안 만들면 백필이 조용히 0건이다.
    // 실제로 후자였다: Aurora 1,961건이 merged 0 · slowlog 0 이었다.
    assert!(
        fetch.contains("/aws/rds/cluster/{}/slowquery"),
        "Aurora 클러스터 그룹 이름을 만들지 않는다 — 그러면 Aurora 는 백필이 돌지 않는다"
    );
    assert!(
        iam.contains("/aws/rds/cluster/*/slowquery"),
        "IAM 폴백에 Aurora 클러스터 그룹이 없다 — 코드가 맞아도 권한이 막는다"
    );

    // 스트림을 좁히는 코드가 있어야 한다. 클러스터 그룹에는 멤버 스트림이 여럿이므로
    // 필터가 없으면 한 멤버가 다른 멤버의 슬로우 쿼리를 자기 것으로 저장한다.
    assert!(
        fetch.contains("set_log_stream_names"),
        "클러스터 그룹을 읽으면서 스트림을 좁히지 않는다 — 멤버 간 오귀속이 된다"
    );
}

/// **화면이 제시하는 모델을 IAM 이 전부 허용해야 한다.**
///
/// 설정 화면의 모델 바로가기 칩은 한 번 누르면 그 값이 저장되고, 그 다음 Tuning 버튼이
/// `bedrock:InvokeModel` 을 부른다. IAM 목록에 없는 모델을 제시하면 **눌리는데 403 인
/// 선택지**가 되고, 사용자는 자기가 무엇을 잘못했는지 알 수 없다.
///
/// 실제로 그렇게 됐다: 화면은 `claude-opus-5` 를 제시했고 IAM 기본값은 `claude-sonnet-5`
/// 하나였다. 배포는 `enable_ai_tuning=false` 라 정책 자체가 없어서 `AccessDenied` 였다.
///
/// 두 목록이 다른 언어·다른 레포 영역에 있으므로 컴파일러가 검사하지 않는다 — 여기서 본다.
#[test]
fn bedrock_iam_covers_the_models_the_ui_offers() {
    let ui = std::fs::read_to_string("../../web/src/components/settings/AiSection.tsx")
        .expect("AiSection.tsx");
    let vars = std::fs::read_to_string("../../infra/layers/40-compute/variables.tf")
        .expect("variables.tf");

    // `KNOWN_MODELS = [ … ]` 안의 문자열 리터럴을 뽑는다.
    let block = ui
        .split_once("const KNOWN_MODELS")
        .expect("KNOWN_MODELS 가 없다 — 화면의 모델 목록 이름이 바뀌었다")
        .1
        .split_once(']')
        .expect("KNOWN_MODELS 배열이 닫히지 않았다")
        .0;
    let offered: Vec<&str> = block
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| s.contains('.'))
        .collect();
    assert!(
        offered.len() >= 2,
        "화면 모델 목록을 못 읽었다 (파싱 결과 {offered:?}) — 이 검사가 무력해졌다"
    );

    let default_block = vars
        .split_once("variable \"bedrock_model_ids\"")
        .expect("bedrock_model_ids 변수가 없다")
        .1;
    for model in &offered {
        assert!(
            default_block.contains(model),
            "화면은 `{model}` 을 제시하는데 `bedrock_model_ids` 기본값에 없다 — \
             그 칩을 누르면 Tuning 이 403 이다"
        );
    }
}

/// **설치 문서의 IAM 정책이 `iam.tf` 와 갈리지 않아야 한다.**
///
/// `docs/install.md`·`docs/install.ko.md` 는 손으로 옮겨 쓰는 문서다.
/// 거기 적힌 정책이 실제와 갈리면 문서를 따른 설치가 조용히 다르게 동작한다 — 권한이 좁으면
/// 기능이 죽고, 넓으면 그 사실을 아무도 모른다.
///
/// 실제로 갈려 있었다. 이 안내가 다음 정책을 싣고 있었다:
///
/// ```json
/// { "Action": ["cloudwatch:GetMetricData"],
///   "Condition": { "StringEquals": { "cloudwatch:namespace": "AWS/RDS" } } }
/// ```
///
/// 그 조건 키는 `GetMetricData` 요청에 실려 오지 않으므로 문이 절대 매치되지 않는다. 코드는
/// 그걸 고쳤는데(`b50deb7`) README 는 따라오지 않았고, **문서를 따른 설치는 CloudWatch 열이
/// 영구히 빈다.** 그 부류를 여기서 막는다.
///
/// 정책 JSON 전체를 비교하지는 않는다(형식이 달라 의미 없는 실패가 난다). **의도를 담은
/// 이름과 함정 경고**가 양쪽에 있는지 본다 — 그게 갈릴 때가 위험한 순간이다.
#[test]
fn the_install_guide_matches_the_iam_policies() {
    let iam = std::fs::read_to_string("../../infra/layers/40-compute/iam.tf").expect("iam.tf");
    let main_tf =
        std::fs::read_to_string("../../infra/layers/40-compute/main.tf").expect("main.tf");
    // 설치 안내는 `docs/install*.md` 로 옮겼다 — README 는 링크만 갖는다. 설계 문서와 달리
    // 이 둘은 **공개 대상**이므로 CI 에도 있고, 없으면 실패하는 것이 맞다.
    let readmes = [
        (
            "docs/install.md",
            std::fs::read_to_string("../../docs/install.md").expect("docs/install.md"),
        ),
        (
            "docs/install.ko.md",
            std::fs::read_to_string("../../docs/install.ko.md").expect("docs/install.ko.md"),
        ),
    ];

    for (name, doc) in &readmes {
        // 문서가 옮겨 적은 `Sid` 는 실제 정책에도 있어야 한다.
        for sid in [
            "DynamoDbData",
            "DenyScan",
            "DiscoveryReadOnly",
            "PutOwnMetrics",
            "MetricsRead",
            "SlowLogRead",
        ] {
            assert!(iam.contains(sid), "{name} 의 `{sid}` 가 iam.tf 에 없다");
            assert!(doc.contains(sid), "iam.tf 의 `{sid}` 를 {name} 이 빠뜨렸다");
        }

        // **기능을 끄는 정책이 다시 들어오지 않게 한다.**
        //
        // 본문 어디에 그 문자열이 있는지가 아니라 **정책 블록 안에** 있는지를 본다 —
        // 함정을 설명하는 경고 문구에도 같은 문자열이 나오고, 그건 있어야 하는 것이다.
        // **코드 블록 안에서, 문장 단위로 본다.** 두 범위를 겹쳐야 오탐이 없다:
        //
        // - 블록만 보면 → 같은 정책의 `PutMetricData`(조건이 **필요한** 액션)에 걸린다
        // - 문장만 보면 → 청크가 블록을 넘어 **함정을 설명하는 경고 산문**까지 삼킨다
        //
        // 우리 예시는 문장마다 `Sid` 가 있으므로 블록 안에서 그걸 경계로 쓴다.
        for block in doc.split("```").skip(1).step_by(2) {
            for stmt in block.split("\"Sid\"") {
                if stmt.contains("cloudwatch:GetMetricData") {
                    assert!(
                        !stmt.contains("cloudwatch:namespace"),
                        "{name} 의 정책 예시가 GetMetricData 에 네임스페이스 조건을 걸었다 — \
                         그 조건 키는 이 액션의 요청에 실려 오지 않아 문이 절대 매치되지 않는다"
                    );
                }
            }
        }

        // **쓸 수 없는 권한이 다시 들어오지 않게 한다.**
        //
        // `KmsUse` 의 `kms:ViaService` 에 S3 가 있었지만 이 롤에는 **S3 액션이 하나도
        // 없다**(플랜 오프로드를 지웠다). 쓸 수 없는 경로를 열어 두면 침해 시 열람 범위만
        // 넓어진다 — 같은 정책이 `Scan` 을 막는 것과 같은 이유다.
        assert!(
            !iam.contains("s3.${var.region}.amazonaws.com"),
            "iam.tf 의 KmsUse 에 S3 경로가 다시 들어왔다 — 이 롤에는 S3 액션이 없다"
        );
        for stmt in doc.split("```").skip(1).step_by(2) {
            if stmt.contains("kms:ViaService") {
                assert!(
                    !stmt.contains("s3."),
                    "{name} 의 KmsUse 예시에 S3 경로가 있다 — 앱은 S3 를 쓰지 않는다"
                );
            }
        }

        // Aurora 를 살리는 두 가지.
        assert!(
            doc.contains("/aws/rds/cluster/*/slowquery"),
            "{name} 에 Aurora 클러스터 로그 그룹이 없다 — Aurora 백필이 조용히 안 돈다"
        );
        assert!(
            doc.contains("dbuser:cluster-"),
            "{name} 에 Aurora 클러스터 리소스 id 가 없다 — 멤버 id 를 쓰면 1045 다"
        );

        // 배포에서 조용히 물리는 설정.
        for key in [
            "DBMON__DISCOVERY__ALLOWED_VPC_IDS",
            "DBMON__COLLECTOR__LITERAL_POLICY",
        ] {
            assert!(main_tf.contains(key), "`{key}` 가 app_env 에 없다");
            assert!(
                doc.contains(key),
                "{name} 의 태스크 정의 예시에 `{key}` 가 없다"
            );
        }

        // 헬스체크를 바꿔 쓰면 standby 가 영원히 죽는다 — 두 경로를 문서가 구분해야 한다.
        assert!(
            doc.contains("/readyz") && doc.contains("/healthz"),
            "{name} 이 컨테이너·타깃그룹 헬스체크를 구분하지 않는다"
        );
    }
}

/// **버전의 정본이 하나여야 한다.**
///
/// `Cargo.toml` 의 `workspace.package.version` 이 정본이고 `clap` 이 `--version` 을 그
/// 값으로 만든다. 문서·예시가 다른 값을 적으면 운영자가 그걸 태그로 쓰고, 그러면 코드가
/// `1.0.0` 이라고 말하면서 이미지는 다른 번호로 발행된다 — 롤백 대상을 특정할 수 없다.
///
/// `release.yml` 의 `version-gate` 가 릴리스 태그를 막지만, 그건 태그를 붙인 **뒤**다.
/// 여기서는 저장소 안의 값들이 서로 맞는지 본다.
#[test]
fn the_version_has_a_single_source_of_truth() {
    let cargo = read("Cargo.toml");
    // `[workspace.package]` 블록의 첫 `version` — 의존성의 `version = "4"` 에 걸리면 안 된다.
    let block = cargo
        .split_once("[workspace.package]")
        .expect("[workspace.package] 가 없다")
        .1;
    let block = block.split("\n[").next().expect("블록");
    let version = block
        .lines()
        .find_map(|l| l.strip_prefix("version"))
        .and_then(|l| l.split('"').nth(1))
        .expect("workspace.package.version 을 못 읽었다");

    // 코드가 보고하는 값과 같아야 한다. `CARGO_PKG_VERSION` 은 이 테스트 크레이트의 것이고,
    // 워크스페이스가 버전을 상속하므로 같은 값이다 — 다르면 상속이 끊긴 것이다.
    assert_eq!(
        version,
        env!("CARGO_PKG_VERSION"),
        "워크스페이스 버전 상속이 끊겼다 — `version.workspace = true` 를 확인한다"
    );

    // 1.0.0 이후로는 0.x 로 되돌아가지 않는다. major 가 0 이면 위 정책표가 뜻을 잃는다.
    let major: u32 = version.split('.').next().unwrap().parse().expect("major");
    assert!(
        major >= 1,
        "버전이 0.x 로 돌아갔다 ({version}) — 정책은 1.0.0 부터다"
    );

    // 릴리스 게이트가 `release.yml` 에 살아 있어야 한다. 없으면 태그와 코드가 갈릴 수 있다.
    let release = read(".github/workflows/release.yml");
    assert!(
        release.contains("version-gate"),
        "release.yml 의 버전 게이트가 사라졌다 — 태그와 코드가 갈려도 발행된다"
    );

    // 문서의 이미지 태그 예시가 버전을 손으로 박지 않아야 한다.
    for doc in ["docs/install.md", "docs/install.ko.md"] {
        let t = read(doc);
        assert!(
            t.contains("workspace\\.package") || t.contains("workspace.package"),
            "{doc} 의 태그 예시가 `Cargo.toml` 에서 버전을 뽑지 않는다 — 손으로 적으면 갈린다"
        );
    }
}
