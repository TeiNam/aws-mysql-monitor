# 20. 2way 리뷰 기록

리뷰마다 **무엇을 주장했고, 실측으로 확인됐는지, 어떻게 처리했는지**를 남긴다.
"고쳤다"만 적으면 다음 리뷰가 같은 것을 다시 찾고, 기각한 항목은 다시 올라온다.

**기각도 기록한다.** 근거 없이 고치는 것과 근거를 대고 고치지 않는 것은 다르다.

| 상태 | 뜻 |
|---|---|
| ✅ 고침 | 실측/코드로 확인하고 수정 + 회귀 테스트 |
| 📝 문서 | 코드는 맞았고 문서가 낡았다 |
| ❌ 기각 | 확인해보니 사실이 아니다 (근거 명시) |
| ⏳ 남김 | 실재하지만 지금 고치지 않는다 (이유 명시) |

---

## 1차 (2026-08-19) — 리뷰어 4명

`rust-reviewer`, `security-reviewer`, `devops`(Terraform·ECS), `design-consistency`.
**두 리뷰어가 독립적으로 같은 결함 2건에 도달했다** (`/*!` 우회, `ranges` 유출) — 신뢰도 신호다.

### 1.1 무성 데이터 손상 (전부 ✅)

| # | 결함 | 확인 방법 | 처리 |
|---|---|---|---|
| C1 | **62초 쿼리가 2.1초로 저장된다.** 선행 저장은 실행 *도중* `TIMER_WAIT`(하한)로 `Timer` 를, 확정은 `stmt` 재조회 불가로 완결된 `Polled` 를 붙인다. `Timer > Polled` 순위 때문에 미완결값이 이긴다 | 코드 경로 추적 + 회귀 테스트 | ✅ `merge_duration` — `slowlog` 는 권위값이라 크기 무관 우선, 그 외는 **최대값**. 정확도 순위에 "관측 완결성" 이 없던 것이 근본 원인 |
| C1b | 같은 원인으로 `merge(a,b) != merge(b,a)`. 동률이면 `existing` 승 → 통계·시각이 **도착 순서에 따라 달라진다** | 회귀 테스트 | ✅ "더 나은 레코드 하나를 고른다" 를 버리고 속성별 규칙. 카운터는 필드별 최대값(같은 실행의 하한이므로 큰 쪽이 참), 불리언은 OR |
| C1c | `sql_text_truncated` 가 채택한 텍스트와 무관하게 계산돼 **절단된 SQL 에 플래그가 false** | 회귀 테스트 | ✅ `merge_sql_text` 가 텍스트와 플래그를 함께 반환 |
| H5 | detect 가 `LIMIT 500` 에 잘린 tick 에서, 컷 아래로 밀린 **실행 중** 쿼리를 `Disappeared` 로 확정하고 거짓 `ended_at_ms` 를 박는다. 다음 tick 에 다시 보이면 한 실행이 여러 레코드로 쪼개진다 | 코드 추적 + 회귀 테스트 | ✅ `tick(…, list_truncated)` — 잘린 tick 에서는 사라짐 판정을 건너뛴다. `too_long` 상한은 유지 |
| H4 | 심층 조회 실패를 `unwrap_or_default()` 로 삼켜 **SQL·지표가 전부 빈 레코드**를 저장했다. tick 은 `Ok` 를 반환해 헬스·서킷도 정상으로 봤다 | 코드 추적 | ✅ 실패를 세고(`deep_probe_failed`) 로그로 남긴 뒤 그 tick 의 선행 저장을 건너뛴다 |
| A2 | `information_schema.PROCESSLIST.INFO` 는 **`utf8mb3`** 라 4바이트 문자를 `?` 로 잃는다 | **실측** (§ 아래) | ✅ 무손실 `utf8mb4` 인 `events_statements_current.SQL_TEXT` 를 함께 읽고 문자 수로 온전한 쪽을 고른다 |
| H6 | `Notify::notify_waiters()` 는 permit 을 저장하지 않는다. `select!` 루프는 매 iteration 재등록하므로 **DB 조회 중에 온 셧다운 신호가 영구히 사라진다** — 리스 반납·버퍼 플러시가 실행되지 않는다 | 코드 추적 | ✅ `AtomicBool` 을 함께 두고 등록 전후로 확인 |
| H7 | 셧다운 예산을 **전체** 단계 수로 나눠 마지막 단계(버퍼 플러시)가 가장 적게 받는다. 5단계·45초면 3.7초만 받고 14.7초가 미사용 | 산술 | ✅ **남은** 단계 수로 나눈다 |

#### A2 실측 — 두 소스는 상보적이다

```sql
SELECT '📊emoji' AS marker, SLEEP(15);
```

| 소스 | HEX | 상한 |
|---|---|---|
| `information_schema.PROCESSLIST.INFO` | `27 **3F** 656D6F6A6927` (`'?emoji'`) | 65,535바이트 |
| `performance_schema.processlist.INFO` | `27 **F09F938A** 656D6F6A6927` | 1,024바이트 |
| `events_statements_current.SQL_TEXT` | `27 **F09F938A** 656D6F6A6927` | 1,024바이트 |

```
information_schema.PROCESSLIST.INFO  = varchar(21845) octets=65535 cs=utf8mb3
performance_schema.processlist.INFO  = longtext       octets=4294967295 cs=utf8mb4
```

손실은 **서버가 IS 테이블을 채울 때** 일어나므로 `CONVERT(… USING utf8mb4)` 로 복구할 수 없다.
통제 재측정(ASCII 20KB, 같은 스레드): `IS bytes=18958 / PS bytes=1024` — 절단 방향은 기존
결론과 일치한다.

`DIGEST_TEXT` 는 리터럴을 `?` 로 마스킹하므로 **`app_digest` 는 영향을 받지 않는다.**
피해는 `full`/`full_restricted` 정책에서 저장·표시되는 전문 SQL 이다 — 운영자가 복사해
재현하면 **다른 쿼리**가 된다.

판정에 `performance_schema_max_sql_text_length` 를 읽지 않는다. 인스턴스마다 캐시해야 하고
설정 변경을 놓친다. 대신 **문자 수**를 비교한다 — `?` 치환은 1문자를 1문자로 바꾸므로
두 소스의 문자 수는 절단이 없을 때만 같다.

### 1.2 보안 (전부 ✅)

| # | 결함 | 확인 방법 | 처리 |
|---|---|---|---|
| S1 | **`/*!NNNNN … */` 가 게이트를 우회한다.** 렉서는 버리지만 서버 버전이 NNNNN 이상이면 MySQL 이 실행한다. SELECT 경로는 검증한 토큰이 아니라 **원문 부분문자열**을 `EXPLAIN FORMAT=JSON ` 뒤에 붙인다 → 렉서가 못 본 코드가 실행된다 | **실측** (§ 아래) | ✅ `plan_query` 가 `/*!` 포함 SQL 을 거부. `/*+ …*/`(힌트)는 통과 |
| L1 | 플랜 마스킹 allowlist 에 **`ranges`** 가 있어 범위 리터럴이 그대로 저장됐다. `SAFE_KEYS` 는 `mask_str` 이 **먼저** 반환하므로 마스킹을 시도조차 않고 `redactions` 도 0 이라 후조건이 안 걸린다 — 무성 유출 | **실측** (§ 아래) | ✅ `EXPR_KEYS` 로 이동 |
| L1b | 같은 이유로 **`operation`** — v2 의 주 조건식 캐리어다 | **실측** | ✅ `EXPR_KEYS` 로 이동. v2 의 `condition` 키도 명시 추가 |
| L2 | `scrub()` 의 인용부호 짝맞추기가 **반드시 어긋난다.** MySQL 에러는 `near '<조각>'` 이고 조각 안에 또 인용부호가 있다 | 손 추적 + 회귀 테스트 | ✅ 짝맞추기를 버리고 첫 인용부호~마지막까지 통째로 버린다. 인용부호가 하나뿐이면 뒤를 전부 버린다 |
| I1 | T-37 게이트가 `== Env::Dev` 만 막았다. 기본값은 `unknown` 이고 `Env::Unknown.treat_as_production() == true` — **프로덕션 취급을 받는 배포가 이 게이트만 통과했다** | 코드 추적 | ✅ `!= Env::Prd` 로 넓혔다. 계정 전체 수집은 `deployment_env=prd` 를 명시해야 한다 |
| H8 | `WITH c AS (…) UPDATE t …` 가 exact SELECT 로 통과 → **관측 도구가 EXPLAIN 으로 DML 을 보낸다.** `classify` 는 이미 `Update` 로 정확히 판정하는데 `plan_query` 가 그 결과를 쓰지 않았다 | 회귀 테스트 | ✅ `WITH` 는 `classify` 결과를 확인한다 |

#### S1 실측 — 절 주입이 동작한다

```sql
-- 서버 버전 80400 > 11111 이므로 주석 내용이 실행된다
SELECT COUNT(*) FROM orders WHERE id=1                     → 1행
SELECT COUNT(*) FROM orders WHERE id=1 /*!11111 OR 1=1 */   → 60,000행
```

우리 정규화 결과는 **두 입력 모두** `SELECT count ( * ) FROM orders WHERE id = ?` 다.
게이트가 `SELECT 1 /*!11111 ;DROP TABLE x */` 를 그대로 통과시키는 것도 확인했다.

스택드 쿼리(`;` 로 문장 분리)는 이 경로에서 구문 오류가 됐다. 그래도 HIGH 로 다루는 이유:
(a) 절 주입은 실측으로 동작하고, (b) SQL 은 `information_schema.PROCESSLIST.INFO` 에서 오는
**신뢰할 수 없는 입력**이며, (c) `mysql_async` 0.36 은 `CLIENT_MULTI_STATEMENTS` 를 무조건
켜고 끌 수 없어 게이트가 유일한 방어선이다.

#### L1 실측 — v2 에서 리터럴이 나온다

```sql
SET explain_json_format_version=2;   -- 8.4 기본값은 1, MySQL 9.x 는 2 가 기본
EXPLAIN FORMAT=JSON SELECT * FROM orders
  WHERE memo = 'kim@example.com' AND id BETWEEN 100 AND 200;
```

```json
"operation": "Filter: ((orders.memo = 'kim@example.com') and (orders.id between 100 and 200))"
"ranges":    ["(100 <= id <= 200)"]
"condition": "((orders.memo = 'kim@example.com') and ...)"
```

`condition` 은 어느 목록에도 없었지만 `may_contain_literal` 휴리스틱이 잡아 마스킹했다 —
**fail-closed 설계가 의도대로 동작한 사례**다. 문제는 명시적 allowlist 로 그 휴리스틱을
건너뛴 두 키였다.

### 1.3 Terraform / ECS

| # | 결함 | 확인 방법 | 처리 |
|---|---|---|---|
| B2 | CMK 로 암호화한 로그 그룹인데 **키 정책에 CloudWatch Logs 서비스 주체가 없다.** 기본 키 정책은 계정 root 위임뿐이고 Logs 는 IAM 위임이 아니라 서비스 주체로 키를 쓴다 → `CreateLogGroup(kmsKeyId=…)` 거부 → **40-compute 첫 apply 실패** | AWS 문서 확인 ("Step 2: Set permissions on the KMS key" 가 필수 단계) | ✅ `aws_kms_key_policy` 추가. root 위임 statement 를 유지하고 `kms:EncryptionContext:aws:logs:arn` 으로 좁혔다 |
| B3 | **S3 게이트웨이 엔드포인트가 이미 있다** (`vpce-03eb8b8a5dbfad810`, [18 §2](18-dev-environment.md)). 한 라우트 테이블에 같은 서비스의 prefix-list 라우트를 두 번 넣을 수 없다 → `RouteAlreadyExists` → **10-foundation 첫 apply 실패** | 기록된 실측 인벤토리 | ✅ `create_s3_gateway_endpoint`(기본 `false`) + `data` 참조 |
| R3 | 엔드포인트가 `data.aws_route_tables` 로 VPC 의 **모든** 라우트 테이블(메인 RT, 남의 서브넷)에 라우트를 넣었다. destroy 가 그걸 제거하는데, priv 서브넷은 NAT 가 없어 게이트웨이 엔드포인트가 **다른 워크로드의 유일한 S3 경로**일 수 있다 | 코드 + 인벤토리 | ✅ `endpoint_route_table_ids` 를 **필수 변수**로. 기본값을 두지 않는다 |
| B4 | backend 가 전 레이어에서 주석 상태인데 `40-compute` 는 `terraform_remote_state` 로 S3 객체를 직접 읽는다. data source 는 plan 시점에 읽히므로 **`plan` 자체가 죽는다** | 코드 | ✅ 전 레이어 partial configuration + `backends/dev.hcl`. apply 순서 6단계를 `infra/README.md` 에 문서화 |
| B5 | 서비스와 `aws_ecs_cluster_capacity_providers` 가 형제 노드라 병렬 생성된다. 기본 전략이 없는 순간 ECS 가 `launchType=EC2` 로 해석해 실패 — **재실행하면 통과하는 종류라 CI 에서 간헐적으로 터진다** | 코드 | ✅ 서비스에 `capacity_provider_strategy` 명시 + `depends_on`. `base` 를 1→0 (혼합 전략으로 갈 때 "prd 는 On-Demand" 와 충돌한다) |
| P1 | `readonlyRootFilesystem = true` 와 ECS Exec 은 양립하지 않는다(SSM 에이전트가 써야 한다). `enable_alb=false` 인 dev 는 인바운드 규칙도 0개 → **dev 배포에 도달 수단이 전혀 없다** | AWS 문서 | ✅ `readonlyRootFilesystem = !var.enable_ecs_exec`. 앱 자체는 디스크를 쓰지 않는다(확인) |
| P2 | ADR-022 는 "`shutdown_grace_secs < stopTimeout` 을 설정 검증으로 강제한다" 고 적었지만 **아무 데도 없었다.** 앱은 `stopTimeout` 을 알 수 없다 | 코드 | ✅ `stop_timeout_secs > shutdown_grace_secs` 변수 검증 |
| P3 | 시드 RDS 가 `dev-rds-subnet-group`(db-subnet-a/b, **라우트 없음**)에 퍼블릭으로 뜬다. RDS 는 생성 시 라우트를 검증하지 않으므로 apply 는 성공하고 퍼블릭 DNS 도 나오지만 **3306 이 타임아웃**한다 | 기록된 인벤토리 | ✅ 퍼블릭이면 IGW 라우트가 있는 pub 서브넷 위에 우리 서브넷 그룹을 만든다 |
| P4 | t4g.micro(1 GiB)에 `performance_schema` + 다이제스트 20,000×4,096 을 강제하면 mysqld 기동 실패 → **`incompatible-parameters`**(재부팅으로 안 풀린다) | 메모리 산술 | ✅ `innodb_buffer_pool_size = {DBInstanceClassMemory*1/2}` 명시 |
| P5 | `engine_version = "8.4"` 는 **플로팅**이다(prefix 매치). `auto_minor_version_upgrade = true` + `ignore_changes = [engine_version]` 이 겹쳐 버전이 올라가도 드리프트가 보고되지 않는다 — 이 인스턴스의 존재 이유가 측정인데 언제 무엇으로 측정했는지 남지 않는다 | 코드 | ✅ `8.4.6` 로 핀, `auto_minor_version_upgrade = false`, `ignore_changes` 제거 |
| P8 | 고정 `name` + `create_before_destroy` = 교체 시 `DuplicateTargetGroupName` / `InvalidGroup.Duplicate` 로 **apply 가 막힌다** | 코드 | ✅ `name_prefix` 로 전환 (SG·대상그룹·파라미터그룹) |
| R1 | `db_auth_resource_ids` 가 비면 `dbuser:*/dbmon` 으로 폴백 → **아무 값도 주지 않은 apply 가 계정 내 모든 RDS(prd 포함)에 IAM DB 인증을 허용**했다. fail-open | 코드 | ✅ 비-prd 는 열거 강제 |
| R2 | `logs:FilterLogEvents` 가 `/aws/rds/*` 전체. 슬로우로그에는 SQL 리터럴이 들어가고 이 계정에는 prd 워크로드가 함께 있다 | 코드 | ✅ `slowlog_log_group_arns` 를 비-prd 에서 열거 강제 |
| B6/P7 | `allowed_vpc_ids` 가 비면 태스크가 crash-loop 하는데 `terraform apply` 는 steady state 를 기다리지 않아 **성공으로 표시된다.** ALB 를 켤 때 인증서·서브넷이 없으면 부분 적용으로 실패 | 코드 | ✅ plan 시점 변수 검증 4개 |
| C2 | RDS 가 만든 로그 그룹은 **Never Expire** 다. `long_query_time=1` + 부하 생성기면 계속 쌓인다 | AWS 동작 | ✅ 로그 그룹을 먼저 선언해 보존기간을 박는다 |
| C3 | 변수 이름이 `enable_container_insights`(bool) 인데 `true` 가 옛 `enabled` 가 아니라 **Enhanced observability**(과금이 훨씬 크다)로 간다 | 코드 | ✅ `container_insights_mode` 로 값 그대로 받는다 |
| R6 | `.terraform.lock.hcl` 이 gitignore 대상. provider 6.x 의 GSI `key_schema` 처럼 버전에 민감한 코드다 | 코드 | ✅ 커밋 대상으로 전환 |
| — | `.gitignore` 의 `/local` 이 시드 SQL·설정·부하생성기를 제외해 **클론에 없었다** — "AWS 없이 전부 돌아간다" 가 성립하지 않았다 | `git check-ignore` | ✅ 데이터·비밀만 무시 |

#### ❌ 기각 — B1 (ARM64 + FARGATE_SPOT 비호환)

> "Linux tasks with the ARM64 architecture don't support the Fargate Spot capacity provider."

**출처가 보관 종료된 GitHub 문서 미러다.** 현재 AWS 문서(`fargate-tasks-services.md`,
`ecs-arm64.md`)에는 그 제약이 없다. 문서화된 아키텍처 제약은 두 개뿐이다:
Windows 컨테이너는 X86_64 필수, ARM64 Fargate 는 `use1-az3` AZ 미지원.

dev 기본값 `use_spot = true` + ARM64 를 **유지한다.** 실제로 태스크 배치가 실패하면
그때 `use_spot = false` 로 한 줄 바꾸면 되고, 지금 바꾸면 근거 없이 dev 비용을 3배 올린다.

#### ⏳ 남김

| # | 항목 | 이유 |
|---|---|---|
| C1(devops) | dev 컴퓨트 $66/월 | B1 기각으로 Spot 이 유효하므로 전제가 사라졌다 |
| R4 | "레이어 destroy 로 되돌린다" 가 사실이 아니다 | 안전한 쪽으로 실패하므로 코드는 그대로. `infra/README.md` 에 사실을 적었다 📝 |
| R5 | 시드 DB 가 스냅샷 없이 사라진다 | 의도된 dev 정책 |
| P6 | `awslogs` non-blocking 이 넘치면 조용히 버린다 | non-blocking 유지가 맞다(로깅이 수집을 멈추면 안 된다). 감사·셧다운 이벤트를 DynamoDB 에 남기는 것은 M4-21 이후 |
| C1(sec) | 베이스 이미지 다이제스트 미고정 | `Cargo.lock` 은 `--locked` 로 고정된다. 다이제스트 고정은 갱신 절차가 필요해 별건 |
| C2(sec) | 런타임에 셸·apt 가 남아 있다 | `readonlyRootFilesystem` + prd exec off 로 완화. distroless 전환은 별건 |
| L3 | `tree_text` 가 마스킹되지 않는다 | 현재 모든 호출부가 `plan_tree: None` 이다. **배선 전에 마스킹을 넣어야 한다** — 아래 잔여 항목 |

### 1.4 설계-코드 불일치 (📝 문서 갱신)

측정이 무효화한 서술이 문서에 남아 있던 것들. **코드가 맞고 문서가 틀렸다.**

| 부류 | 규모 | 처리 |
|---|---|---|
| `EXPLAIN … FOR CONNECTION` 을 플랜 소스로 서술 (P0 요구 4건 포함) | 01·02·03·05·07·08·15·18 | FR-PLN-01/03/07/10, NFR-P-04, MVP 수용 기준을 실측 결과로 교체. **구조적으로 만족 불가능한 P0 가 4개 있었다** |
| `instance_id` 2성분 예시 | 10개 문서 17줄 | 3성분으로 통일. `InstanceId::parse` 가 2성분을 **거부**하므로 예시대로 쓰면 역직렬화가 실패한다 |
| EC2 ASG / systemd / `sd_notify` / Lifecycle Hook | 01·02·03·05·17 | ECS Fargate 로 교체. ADR-022 **안의** 서술은 "무엇을 대체했는가" 이므로 유지 |
| 정규화 규칙표 (05 §3.2) | 표 전체 | "이 규칙표는 가설이다" → M1-6 실측 규칙표로 교체. **규칙 8("식별자는 원본 대소문자 유지")이 가장 위험했다** — 그 규칙으로는 수렴하지 않는다 |
| `mysql_digests` 세 형태 공존 (`Set` / `{d,t}` 단일 / `SET` 덮어쓰기) | 04 §2.3·2.4·4.3.3 | 중첩 맵 하나로 통일 + **엔트리 추가** 연산 명시. 덮어쓰면 학습이 사라진다 |
| 04 §2.3 속성표 · Iceberg DDL | 04 | `duration_source` 에 `timer` 추가·`merged` 제거, `literal_policy` 에 `full_restricted`, `plan_json`→`plan_normalized`, `state`/`abandoned_reason`/`literal_policy_at_ms` 등 **재도출 불가 필드 9개** 추가 |
| 내부 모순 | 03·02·17·19 | 리스 60초 vs 80초 → 80초, 연결 풀 3 vs 6 → 6, Recharts 도입 vs uPlot 단독 → uPlot, M1 "6개" vs "17가지" → 17, `N:1` vs `1:N` → 방향 명시 |
| 해소된 미해결 질문이 "미해소" 로 남음 | OPEN-Q-05·06·07·16·20 | 해소 표시 + 결론 |
| `dur_bucket b0` 경계 (2~5s vs 5s 미만) | 04 | 코드가 맞다(임계값이 1초일 수 있다). 이유를 문서에 적었다 |
| 존재하지 않는 크레이트 이름 (`awsinfra`, `mysqlsrc`) | 03·17 | `dbmon::aws`, `dbmon::mysql` |
| M4-22 (`/* dbmon: */` 주석 기반 자기 계측) | 17 | **성립하지 않는다** — 다이제스트가 주석을 제거한다. 취소선 + M4-29 로 대체 |
| 로드맵 완료 상태 | 17 | M0 14건, M4 진행 상황 표시 |

---

### 1.5 리뷰가 놓친 것 — 직접 발견 (✅)

리뷰어 4명이 모두 놓쳤다. 교차 참조가 필요한 종류다.

**`SlowQuery` 에 `GSI1PK` 가 두 개 배정돼 있었다.** 한 항목은 `GSI1PK` 를 **하나만**
가질 수 있는데 GSI1 을 쓰는 접근 패턴이 둘이었다:

| 패턴 | 필요한 `GSI1PK` | 문서 위치 |
|---|---|---|
| AP-3 다이제스트 → 최근 실행 샘플 | `DG#<app_digest>` | 04 §2.2 · §2.3 |
| AP-18 진행 중 레코드 → 고아 정리 (F4) | `SQS#in_flight` (희소) | 04 §2.2 |

**놓치면 F4 가 조용히 죽는다.** §2.3 만 읽고 구현하면 `GSI1PK = DG#<app_digest>` 를 항상
쓰게 되고 AP-18 쿼리가 **항상 0건**을 반환한다. 고아 레코드는 TTL(35일)까지 화면에
"실행 중" 으로 남고, 실패가 에러로 나타나지 않는다.

해결: 두 패턴이 관심 있는 상태가 겹치지 않으므로 **상태 전이와 함께 키를 바꾼다.**
선행 저장은 `SQS#in_flight`(SK = `last_seen_at_ms`, 오래된 것부터 스캔 가능),
확정 시 `DG#<app_digest>`(SK = `Q#…`) 로 교체한다.

Terraform 주석은 "확정 시 `GSI1PK` **제거**" 라고 적혀 있었는데 그건 `Alert`(AP-9)의
방식이다. `SlowQuery` 는 제거가 아니라 교체다 — 제거하면 AP-3 가 0건이 된다.
GSI1 사영에 `owner_epoch`·`thread_id`·`abandoned_reason` 도 추가했다(고아 스윕이 필요하다).

**정렬 키에 들어가는 숫자가 고정 폭이 아니었다.** DynamoDB 의 `S` 정렬 키는 사전순이므로
`"1000" < "999"` 다. epoch 밀리초는 2286년까지 13자리라 **프로덕션 값끼리는 우연히
맞는다** — 그래서 테스트나 백필에서만 드러나고, `SK 범위 조회`(AP-1)와 고아 스윕
(`GSI1SK < 임계`, AP-18)이 조용히 잘못된 집합을 반환한다.

`sort_key_ms()` 를 `core::time` 에 추가하고 문서 키 정의에 `:013` 을 표기했다.
음수는 부호가 사전순을 완전히 깨뜨리므로 0 으로 클램프한다. 회귀 테스트는 작은 값과
프로덕션 값을 **섞어서** 사전순 = 숫자순임을 확인한다 — 프로덕션 값만 쓰면 통과해 버린다.

---

---

## 2차 (2026-08-19) — 리뷰어 2명

`rust-reviewer`, `security-reviewer`. 대상은 **1차가 바꾼 코드**다 — 새 코드에 새 결함이 있다.

**1차 수정 중 4건이 새 결함을 만들었다.** 그게 2차의 성과다.

### 2.1 CRITICAL — 실제로 실행되는 SQL 주입 (✅)

| # | 결함 | 확인 방법 |
|---|---|---|
| C-1 | **`sql_mode` 를 고정하지 않아 검증기와 실행기가 문자열 경계를 다르게 본다** | 실측 |

`session_init` 이 `sql_mode` 를 건드리지 않아 **대상 인스턴스의 global 설정을 상속**한다.
거기에 `NO_BACKSLASH_ESCAPES` 가 있으면 서버는 `\` 를 이스케이프로 보지 않는데
우리 렉서는 항상 이스케이프로 처리한다. 8.4.11 실측:

```sql
-- PROCESSLIST.INFO 에서 온 신뢰 불가 텍스트
SELECT * FROM orders WHERE memo = '\'; SELECT 31337 AS pwned; SELECT 1 -- '
```

| 세션 `sql_mode` | `Com_select` 델타 | 결과 |
|---|---|---|
| `''` | **1** | 페이로드가 문자열 리터럴 안에 머문다 (`attached_condition` 에서 확인) |
| `NO_BACKSLASH_ESCAPES` | **3** | `SELECT 31337 AS pwned` 가 **실행된다** |

우리 렉서는 두 경우 모두 `;` 토큰을 0개로 본다 → 멀티문장 게이트 통과.
`mysql_async` 는 `CLIENT_MULTI_STATEMENTS` 를 무조건 켜고 끌 방법이 없으므로
`plan_query` 가 유일한 차단점이었고, 그게 뚫렸다.

**`docs/05-collector.md:756` 이 이미 `sql_mode=''` 를 요구하고 있었다** — 코드가 하지
않았을 뿐이다. 1차의 P2(문서에만 있는 통제)와 같은 부류다.

`LOAD DATA LOCAL INFILE` 파일 탈취는 **이중 차단으로 불가**하다: `mysql_async` 는
핸들러가 없으면 `NoHandler` 를 반환하고 서버 `local_infile` 도 기본 OFF 다.

회귀 테스트는 `crates/dbmon/tests/it_injection.rs` 다. **통제를 끈 대조 테스트를 함께
둔다** — 그게 없으면 "고정이 효과가 있어서" 통과하는지 "애초에 뚫리지 않아서" 통과하는지
구분할 수 없다.

### 2.2 1차 수정이 만든 결함 (전부 ✅)

| # | 무엇이 나빠졌나 | 처리 |
|---|---|---|
| H1 | **1차 수정이 수정 전보다 나빴다.** "실패를 삼키지 않는다" 를 `return Ok(stats)` 로 구현했는데, `tracker.tick()` 은 이미 캐시에서 엔트리를 제거해 `tick.finalized` 로 넘긴 상태였다 → 조기 반환이 그 Vec 을 버려 **확정이 영구히 사라졌다.** 확정 경로는 그 조회를 쓰지도 않는다 | `prefetch_save` 를 별도 메서드로 분리. 실패는 선행 저장에만 국한된다. `it_probe_failure.rs` 가 결함을 재주입해 잡히는지 확인 |
| H4→H4' | `duration`(최대) + `ended_at`(최소) 를 따로 골라 **모순을 다른 필드로 옮겼다** (duration 62,000ms 인데 구간 4,000ms) | `merge_duration` 이 둘을 함께 반환한다 |
| H3 | `merge_digest` doc 이 "`pick_str` 과 같은 규칙(사전순)" 이라고 적었지만 `pick_str` 은 **`a` 우선**이다 → 교환법칙 미성립. `app_digest` 는 GSI1PK 이므로 도착 순서가 집계 파티션을 바꾼다 | 실제 사전순(`min`)으로 |
| H6 | `pick_sql_text` 가 PS 텍스트를 고를 수 있게 됐는데 절단 플래그는 IS 상한(65,535)만 봤다. PS 는 1,024 다 → **잘린 SQL 이 "잘리지 않았다" 로 저장된다.** 1차 C1c 와 같은 실패를 소스 추가로 재도입했다 | `PickedSql { text, maybe_truncated, lossy }`. IS 가 없으면 판정 근거가 없으므로 **보수적으로 true** |
| M1' | `literal_policy` 동률에서 `min()` 을 썼는데 **선언 순서가 `Full` 먼저**라 `min` = 가장 느슨한 값이었다 — 내 fail-open 수정이 방향을 반대로 했다 | `restrictiveness()` 를 명시하고 `more_restrictive()` 로. **선언 순서에 보안 결정을 맡기지 않는다** |

### 2.3 나머지 (전부 ✅)

| # | 결함 | 처리 |
|---|---|---|
| H2 | 잘린 tick 이 지속되면 사라짐 판정을 건너뛰므로 엔트리가 1시간 동안 한 건도 제거되지 않는다 — 인스턴스당 약 **225MB**, 이후 250건/tick 이 `TooLong` 으로 쏟아진다. 절단은 정의상 "장애 중" 이라 가장 나쁠 때 터진다 | `DEFAULT_MAX_ENTRIES = 50_000` + `FinalizeReason::Evicted`(=`long_running` 오탐 방지) + `evicted_total` 카운터 |
| H5 | `scrub` 이 **`Can't` 의 어포스트로피를 여는 인용부호로 봐서** 메시지가 `Can'?'` 로 붕괴했다. MySQL 에러 절반이 축약형으로 시작한다 → 운영자가 "연결 거부" 와 "접근 거부" 를 구분할 수 없다 | `'` 양옆이 모두 ASCII 알파벳이면 축약형으로 판정해 건너뛴다 |
| M-2 | 같은 맥락: `mask_digit_runs` 가 3자리 이상을 지워 **에러 코드까지 `?`** 였다 | 코드를 `[mysql 1045]` 로 **마스킹 밖에** 붙인다 |
| H-1 | `used_columns`·`used_key_parts` 가 `SAFE_KEYS` 였는데, **함수형 인덱스가 걸리면 표현식**이 들어온다. 실측: `concat(\`email\`,_utf8mb4'@internal-payroll.example.com')` | `EXPR_KEYS` 로 이동. `query`·`heading`·`lookup_condition`·`sort_fields` 도 명시 추가 |
| H-2 | **JSON 숫자는 마스킹 대상이 아니었다.** v2 는 `LIMIT`/`OFFSET` 을 숫자로 낸다(실측 `"limit_offset": 4242`) | 숫자도 마스킹. `is_estimate_key` allowlist(비용·행수·순번)만 보존 — **새 숫자 키는 기본 마스킹**(fail-closed) |
| M6 | `operation` 마스킹은 옳았지만 SQL 정규화기를 산문 라벨에 적용해 `Limit: 10 row(s)` → `LIMIT : ? ROW ( s )` 로 망가졌다 | `mask_label()` — 인용 구간과 **독립** 숫자만 지운다. `t1` 의 `1` 은 식별자라 남는다 |
| M5 | 절단 가드가 폴백을 `digest_text` 로 돌렸는데 거기엔 `?`·`(...)` 자리표가 남아 **항상 1064** 다 → 실패가 보장된 EXPLAIN 을 최대 3회 보낸다 | `plan_query` 가 `Tok::Param`·`Tok::Ellipsis` 를 거부. 렉서가 이미 "입력의 `?`" 와 "마스킹한 리터럴" 을 구분하고 있었다 |
| M8 | `unknown-<tid>` 자리표가 뒤늦게 도착한 진짜 다이제스트를 이겼다 | `UNKNOWN_DIGEST_PREFIX` 를 "없음" 으로 취급 |
| M9 | 4바이트 문자 손실이 레코드에 남지 않았다 | `sql_text_lossy` 필드 추가 |
| M3 | `detect_total` 이 하드코딩 1,000ms 라 `detect_timeout_ms` 설정을 조용히 덮었다 | `Timeouts::derive` 로 파생 |
| M4 | **`detect_total` 초과 시 진행 중인 연결 수립이 취소된다** (`GetConn::drop` → `cancel_connection`). "다음 tick 은 warm" 은 사실이 아니었고, 콜드 상태에서 매 tick 취소를 반복해 `probe` 가 영구 실패할 수 있었다 | `detect_total > connect` 를 `derive` 가 보장 |
| M2 | `connect_with_limit` 의 **호출부가 하나도 없었다** → M27 수정이 무효 | `from_config` 를 유일한 프로덕션 경로로 만들고 테스트로 고정 |
| M7 | v2 는 `estimated_total_cost` 를 쓰는데 코드가 `cost_info.query_cost` 만 봤다 → MySQL 9.x 에서 비용이 **항상 `None`** | 두 키 모두 읽는다 |
| M-5 | CI 에 `permissions` 가 없어 `GITHUB_TOKEN` 이 리포 기본 권한을 받는다 | `permissions: contents: read` |
| M-6 | `allowed_client_cidrs` 가 `0.0.0.0/0` **리터럴만** 막았다. `["0.0.0.0/1","128.0.0.0/1"]` 는 통과 = IPv4 전체 | 접두 길이 `>= 24` 로 fail-closed |

### 2.4 2차가 확인한 "1차가 맞았다" (건드리지 말 것)

- `Shutdown::wait()` 의 확인-등록-확인은 lost wakeup 이 없다 (tokio 의 `notified()` 가 생성 시점 카운터를 캡처한다). `AtomicBool` 조합이 완전하다.
- `scrub()` 은 UTF-8 안전하다 (인용부호가 ASCII 라 경계가 보장된다). 40만 케이스 퍼징 패닉 0.
- `merge_stats`(필드별 최대 / 불리언 OR) 는 교환·결합법칙을 만족한다.
- `list_truncated` 로 사라짐 판정을 건너뛰는 것 자체는 맞다 (H2 는 축출 부재의 문제).
- `ranges`/`operation`/`condition` 를 `EXPR_KEYS` 로 옮긴 판단은 옳다 (M6 은 마스킹 *방식*의 문제).
- `WITH` → `classify` 게이트, `checked_add`, T-37 `!= Env::Prd`, 루프 주기 검증 — 전부 정확.
- 조회 도중 취소는 풀을 오염시키지 않는다 (recycler `cleanup_for_pool`).
- T-37 게이트는 종단 검증 통과: `Env` 는 인식 못 하는 문자열을 `Unknown` 으로 떨어뜨리지 않고 **역직렬화 실패**로 기동을 거부한다. `join(",", [])` 의 빈 문자열도 빈 배열이 된다.

### 2.5 대칭성은 전수 검사로 바꿨다

손으로 고른 한 쌍으로는 대칭성을 확인할 수 없다 — **1차·2차가 모두 "고쳤다" 고 한 뒤에도
비대칭이 남아 있었다.** 이제 `duration × source × state × ended_at × digest` 를 조합한
**29,646 쌍 전부**에 대해 `merge(a,b) == merge(b,a)` 를 확인한다
(`merge_is_commutative_across_all_axes`). 동률 tie-break 를 제거하면 실제로 실패한다.

### 2.6 내가 만든 테스트 결함 3건

기록해 두는 이유: 같은 함정을 다시 밟는다.

1. **`exclusive_target` 은 프로세스를 넘지 않는다.** 새 테스트 바이너리에서 그걸 부르니
   `reset_targets` 가 `it_collector` 의 장기 실행 쿼리를 죽여 그쪽이 플래키해졌다.
   → 세션 범위 카운터만 쓰면 독점이 필요 없다.
2. **`performance_schema.session_status` 에는 `Com_*` 가 없다** (8.4 실측: 336행 중 부재).
   `SHOW SESSION STATUS` 만이 세션 단위 명령 카운터를 준다.
3. **`LIKE` 자기 매칭** — 검색 패턴이 쿼리 자신의 텍스트에 있으면 자신을 찾는다.
   그리고 `explain_rerun` 은 `/* dbmon:planrerun */ EXPLAIN …` 이라 `LIKE 'EXPLAIN%'` 로는
   우리 EXPLAIN 을 제외하지 못한다.

---

## 3차 진행 중 — 내가 먼저 찾은 회귀 (✅)

**`detect_total` 을 두고 M17 과 M4 사이를 한 바퀴 돌았다.** 기록해 두는 이유: 두 요구가
충돌하는데 그걸 인지하지 못하고 양쪽을 번갈아 만족시켰다.

| 시점 | 값 | 결과 |
|---|---|---|
| 원래 | (없음) | detect 경로 총 5.8초 → 1초 케이던스 붕괴 (**M17**) |
| 1차 수정 | 1,000ms 하드코딩 | 콜드 연결 수립이 매 tick 취소 → `probe` 영구 실패 (**M4**) |
| 2차 수정 | `connect + detect + 20%` = 6,960ms | 1초 주기의 **8.7배** — 아무것도 묶지 못한다 |
| 3차 수정 | `interval × 80%` = 800ms + **`warm()`** | 둘 다 만족 |

두 요구는 **연결 수립이 tick 안에서 일어나면** 화해할 수 없다:

| 요구 | 필요한 시간 |
|---|---|
| tick 케이던스 | 800ms |
| 콜드 연결 수립 (TLS + IAM 토큰) | 최대 5,000ms |

해법은 상한을 조정하는 게 아니라 **연결 수립을 tick 밖으로 빼는 것**이다.
`TargetDb::warm()`(기본 구현 = `ping`)을 기동 시와 실패 후에 부른다. 그러면 tick 은
항상 warm 커넥션만 집으므로 `detect_total` 은 tick 예산이면 된다.

`needs_warm` 표시를 지우면 테스트가 실패한다 — 표시와 처리의 **관계**는 고정됐다.
2차의 M2("`connect_with_limit` 호출부가 없어서 수정이 무효였다")와 같은 실수를 막는다.

⚠ **다만 `warm_if_needed` 도 `detect_tick` 도 프로덕션 호출부가 없다.** 수집 루프 자체가
`main.rs` 에 배선되지 않았다(M4-21 미구현). 즉 이 수정들은 **배선되는 시점에** 유효해진다.
아래 "가장 큰 남은 격차" 를 참조한다.

---

---

## 3차 (2026-08-19) — 리뷰어 2명

대상은 2차가 바꾼 코드. **2차 수정 중 2건이 CRITICAL 결함이었다.** 두 리뷰어가 독립적으로
같은 두 건에 도달했다.

### 3.1 CRITICAL — 2차의 `sql_mode` 고정이 **첫 쿼리에만** 적용됐다 (✅)

`PoolOpts::default()` 는 `reset_connection: true` 다. 커넥션이 풀로 돌아갈 때마다
`COM_RESET_CONNECTION` 이 나가 세션 변수가 **전역값으로 되돌아가고**, `mysql_async` 는
그 뒤 `setup` 명령만 재실행한다 — `init` 은 하지 않는다. 같은 풀 설정으로 4회 획득/반납한 실측:

```text
.init  #1: sql_mode=""                    max_exec=3000 iso=READ-COMMITTED
.init  #2: sql_mode="ONLY_FULL_GROUP_BY…" max_exec=0    iso=REPEATABLE-READ
.init  #3~4: (동일)
.setup #1~4: sql_mode="" max_exec=3000 iso=READ-COMMITTED
```

**세 가지를 동시에 잃었다:**

| 변수 | 잃으면 |
|---|---|
| `sql_mode` | `NO_BACKSLASH_ESCAPES` 상속 → **2차 C-1 주입이 2번째 쿼리부터 다시 열린다** |
| `max_execution_time` | **0 = 무제한.** 프로덕션 DB 에 서버측 상한이 사라진다 |
| `transaction_isolation` | `REPEATABLE-READ` — 긴 스냅샷이 undo 를 붙잡는다 |

수정: `.init(` → `.setup(` 한 단어. 회귀 테스트는 **획득 → 반납 → 재획득** 을 4회 반복한다.
2차 테스트는 `Conn::new` 로 단일 커넥션만 봤기 때문에 아무것도 검증하지 못했다.

### 3.2 CRITICAL — 2차의 `mask_label` 이 리터럴을 그대로 유출했다 (✅)

2차에서 `operation`/`heading` 의 가독성(M6)을 위해 인용부호를 직접 훑는 스캐너를 썼다.
**이스케이프를 몰랐다.** 실제 8.4.11 플랜 출력:

| 입력 | 2차 출력 | `redactions` |
|---|---|---|
| `Filter: (t.memo = 'It\'s a secret')` | `Filter: (t.memo = ?s a secret?` | **0** |
| `Filter: (t.blob = 0x536563726574)` | `Filter: (t.blob = ?x536563726574)` | **0** |
| `Filter: (t.a = 'p\'q' and t.b = 'topsecret')` | `Filter: (t.a = ?q?topsecret?` | **0** |

`0x536563726574` 는 `Secret` 이다. 랜덤 퍼징에서 **30.1%** 가 유출됐고, `redactions` 를
올리지 않으므로 1차 L1 과 **똑같이 무성**이었다. `LABEL_KEYS` 가 `EXPR_KEYS` 보다 먼저
검사되므로 fail-closed 휴리스틱도 우회했다.

수정: **직접 훑기를 버리고 렉서를 쓴다.** 렉서는 `\'`·`''`·`0x`·`b'`·지수 표기를 이미
정확히 처리한다. 토큰 스팬을 받아 리터럴 구간만 `?` 로 바꾸고 나머지는 원문을 복사하므로
가독성도 유지된다:

```text
Limit: 10 row(s)                     → Limit: ? row(s)
Table scan on t1  (cost=1.25 rows=5) → Table scan on t1  (cost=? rows=?)
Filter: (t.memo = 'It\'s a secret')  → Filter: (t.memo = ?)
```

닫히지 않은 인용부호는 경계를 알 수 없으므로 `REDACTED` + `redactions += 1` 로 **fail-closed**
한다 — 이전 구현에는 그 경로가 아예 없었다. `heading` 이 두 목록에 중복돼 `EXPR_KEYS` 쪽이
죽은 코드였던 것도 고쳤고, 중복을 막는 테스트를 넣었다.

### 3.3 HIGH — 우리가 앱과 SQL 을 다르게 파싱한 플랜을 "정확" 으로 저장했다 (✅)

`sql_mode=''` 고정은 주입 방어로는 옳지만, SQL 은 **앱 세션**에서 오고 그 모드는 통제하지
못한다. 8.4.11 실측:

```text
SET sql_mode='ANSI_QUOTES';
SELECT COUNT(*) FROM orders WHERE "status" = 'PAID';   → 15000   ("status" = 식별자)
SET sql_mode='';
SELECT COUNT(*) FROM orders WHERE "status" = 'PAID';   → 0       ("status" = 문자열)

EXPLAIN 결과: operation = "Zero rows (Impossible WHERE)"
```

15,000행을 스캔하는 쿼리를 조사하는 운영자가 "Zero rows" 플랜을 **정확한 플랜으로** 보게
된다. 수정: `TargetDb::target_sql_mode()` 를 `warm()` 시점(tick 밖)에 읽어 캐시하고,
`ANSI_QUOTES`·`NO_BACKSLASH_ESCAPES` 가 있으면 `PlanSource::RerunAsSelect` 로 강등한다
(UI 가 "근사" 배지를 붙인다). 읽기 실패도 위험으로 본다 — 모르는 상태에서 정확하다고
표시하는 것보다 근사가 안전하다.

### 3.4 HIGH — 2차의 `.or(other.ended_at_ms)` 가 모순을 만들고, 제거하면 사실을 잃었다 (✅)

2차 H4' 는 duration 과 종료 시각을 한 쌍으로 반환하게 했지만 `.or()` 폴백이 **패자의**
종료 시각을 되가져왔다. 1,296 쌍 중 810 쌍이 모순이었고, 그중에 **유일하게 배선된 경로**가 있다:

```text
선행 저장: TIMER_WAIT 4.4초(실행 중) → duration=4,400 Timer  ended=None
확정:      관측 TIME 5초             → duration=5,000 Polled ended=start+5,000
병합:      duration=4,400 + ended=start+5,000 → 구간 5,000 vs duration 4,400
```

`.or()` 를 그냥 지웠더니 **관측한 종료 시각을 잃었다** (`it_collector` 가 잡았다).
둘 다 틀렸다 — **종료를 아는 순간 duration 은 추정이 아니라 구간이다.** 최종 규칙:

| 상황 | duration | ended_at |
|---|---|---|
| 슬로우로그 있음 | 그 측정값 (권위) | `min_opt` |
| 종료 관측됨 | **`ended − started`** (사실) | `min_opt` |
| 종료 미관측 | 관측된 최대값 (하한) | `None` |

`min_opt` 를 **모든 분기에서** 쓰는 것이 결합법칙의 조건이다 — 슬로우로그 분기만
`.or()` 로 두면 `merge(merge(a,b),c) != merge(a,merge(b,c))` 다(실측).

### 3.5 HIGH — 2차의 축출이 O(n²) 이고 `record_id` 를 흔들었다 (✅)

| 한 tick 축출 건수 | 릴리스 측정 |
|---|---|
| 250 (문서의 시나리오) | 56.9 ms |
| 5,000 | 749 ms |
| 10,000 (`detect_limit` 최대) | **2.6 초** |

tick 예산은 800ms 다. 동기 코드라 같은 tokio 워커의 다른 인스턴스까지 굶는다.
게다가 **실행 중인** 스레드를 축출하면 다음 tick 에 새 엔트리로 생기고 `started_at_ms` 가
재계산돼 `record_id` 가 달라진다 → 한 실행이 여러 레코드로(1차 H5 재발, 지터 ±120ms 에서 11.1%).

수정: **이번 tick 에 보이지 않은 것만** 축출 대상으로 삼고, 한 번의 정렬로 필요한 만큼만
고른다. 관측 중인 것만으로 상한을 넘으면 상한을 일시적으로 넘기고 `over_cap_ticks` 로
관측 가능하게 남긴다 — 관측 중인 실행을 버리는 것보다 낫다. `seen` 은 `BTreeSet` 으로.

### 3.6 HIGH — 2차의 `scrub` 축약형 예외가 유출 경로였다 (✅)

"양옆이 알파벳이면 축약형" 규칙이 MySQL 의 **문자 접두 리터럴**을 통과시켰다:

```text
Cannot convert x'topsecret' to utf8mb4        → Cannot convert x'topsecret'?'
Incorrect string value for memo: _binary'S3CRET' → … _binary'S3CRET'?'
```

`x'…'`, `b'…'`, `N'…'`, `_binary'…'`, `_utf8mb4'…'` 가 실재한다. 10,976 형태 중 1,844 유출.

첫 수정(뒤쪽 알파벳 1~2자 제한)으로도 `_binary'S3CRET'` 이 통과했다 — 접미가 `S` + 숫자다.
**영문 축약형 접미는 닫힌 집합**(`s t d m re ll ve`)이므로 열거하고 **단어 경계**를 요구한다.
`x'topsecret'` 은 접미 `t` 뒤가 `o` 라 리터럴로 판정된다.

### 3.7 MEDIUM — 공격자가 우리 서버측 상한을 무력화한다 (✅)

`plan_query` 는 옵티마이저 힌트를 의도적으로 통과시킨다(플랜을 바꾸므로 보존해야 한다).
그런데 힌트 중 둘은 플랜이 아니라 **실행 자원**을 바꾼다. 8.4.11 실측:

```text
SET SESSION max_execution_time = 1000;
SELECT SLEEP(2)                                    → 1  (죽었다)
SELECT /*+ MAX_EXECUTION_TIME(600000) */ SLEEP(2)  → 0  (완료됐다)
SELECT /*+ SET_VAR(max_execution_time=0) */ …      → 상한 0
```

SQL 은 `PROCESSLIST.INFO` 에서 오므로 대상 DB 에 쿼리를 날릴 수 있는 누구나 힌트를
통제한다. `Timeouts::query` 는 클라이언트만 취소하므로 `max_execution_time` 이 유일한
서버측 바운드다. 수정: 자원 힌트만 스팬 단위로 제거하고 플랜 힌트는 보존한다.
`SET_VAR` 은 `optimizer_switch` 로 플랜도 바꿀 수 있어 `is_exact = false` 로 강등한다.

### 3.8 나머지 (전부 ✅)

| # | 결함 | 처리 |
|---|---|---|
| C3-5 | **2차의 `warm()` 이 자기 시나리오에서 발동하지 않았다.** 연결 문제는 `probe` 에서 드러나는데 `needs_warm` 은 `prefetch_save` 실패에서만 표시됐다. 내 테스트는 `fail_full_sql` 을 썼으므로 **이름이 약속한 보장을 전혀 건드리지 않았다** (vacuous) | `probe` 실패도 표시. 페이크에 `fail_probe` 추가하고 테스트를 실제 시나리오로 교체 |
| C3-8 | `statement_type`(`Other` 가 자리표), `mysql_digest`, `cluster_id`, `schema_name`, `db_user`, `db_host`, `owner_worker`, `abandoned_reason`, `engine_version` 이 순서 의존 | `pick_statement_type`·`pick_opt_str`·`pick_str` 을 결정론적으로. `StatementType`·`ClusterId` 에 `Ord` 추가 |
| C3-7 | 같은 텍스트인데 절단·손실 플래그가 도착 순서로 결정됐다 | 길이 동률이면 플래그를 `AND` 로 결합. 두 플래그는 "배제할 수 없었다" 는 뜻이므로 한쪽이 배제했으면 배제된 것이다 |
| C3-9 | 절단 폴백이 `digest_text` 라 `plan_query` 가 항상 거부 → **실패가 보장된 EXPLAIN** 이 `plan_attempts` 를 소모하고 3회 뒤 영구 포기 | 폴백을 무손실 `stmt.sql_text` 로 |
| C3-10 | 인프라 게이트 2개가 무의미했다: `doc.contains("64")` 는 `64배 개선`·`u64`·`86400` 에 걸려 4·8·16·256·1024 로도 통과했고, `find("days")` 는 부분 문자열이라 `noncurrent_days = 37` 을 `ttl35` 로 읽었다 | `SHARD_COUNT = 64` 문맥을 요구. `expiration` 블록을 이름 경계로 찾고 정확히 `days` 인자만 읽는다. **주입 실험 3종으로 확인** |
| C3-11 | `m1_capture` 가 락을 잡지 않아 `it_collector` 의 `reset_targets` 에 죽는다(잠재) | `ROOT` 로 실행 — 리셋이 `USER <> 'root'` 로 제외한다. 락이 바이너리를 넘지 않는다는 사실을 `support` 에 명시 |
| LOW-1 | `sql_text_lossy` 에 `serde(default)` 가 없어 이전 레코드 역직렬화가 실패한다 | 추가 |
| LOW-2 | `fakes` 가 게이트 없이 `pub` — `TargetDb` 페이크가 배선되면 "정상" 을 보고하며 아무것도 수집하지 않는다 | `#[cfg(any(test, feature = "testing"))]` + dev-dependency. 프로덕션 바이너리에서 컴파일되지 않는다 |
| LOW-3 | `aws_kms_key_policy` 에 `prevent_destroy` 가 없어 정책만 지워질 수 있다(키는 30일 대기) | 키와 같은 보호. `arn:aws:` 하드코딩을 `data.aws_partition` 으로 |
| MEDIUM-2 | **2차의 M2 가 재발했다** — `from_config` 의 호출부가 여전히 0개고 `connect` 가 `pub` 이라 다음 사람이 그걸 잡는다 | `connect` 를 `testing` 피처로, `connect_with_limit` 을 비공개로. `from_config` 가 유일한 공개 경로다 |
| C3-12 | `derive` 주석이 "`connect` 보다 크게 잡는다" 는 낡은 서술을 유지 | 갱신 |

### 3.9 대칭성 검사를 4.25M 쌍으로 확대

2차의 29,646 쌍은 **모든 변형이 같은 `sql_text` 를 공유해서** 길이 동률 경로와 플래그
짝짓기를 한 번도 지나지 않았다. 축을 추가해 **4,252,986 쌍**을 검사하고, 별도로
**결합법칙 46,656 삼중**을 검사한다(3소스 병합 F5 가 좌측 폴드다).

### 3.10 리뷰어가 확인한 "2차가 맞았다"

`Tok::Param` 거부는 과잉 거부가 아니다(합법적 `?` 9형태 전부 통과, 그리고 **서버측
프리페어드는 `PROCESSLIST.INFO` 에 `?` 를 보이지 않는다** — MySQL 이 전개한다).
`sql_mode=''` 가 `ONLY_FULL_GROUP_BY` 를 없애는 것은 이득이다(EXPLAIN 이 상위집합을 받고
우리는 쓰기를 하지 않는다). `NO_BACKSLASH_ESCAPES`·`ANSI_QUOTES` 가 **유일한 어휘 발산**이다.
숫자 마스킹의 `is_estimate_key` allowlist 는 8.4 가 내는 모든 숫자 키에 대해 정확하다.
`mask_label`·`scrub`·`next_quote` 는 패닉 없고 UTF-8 안전하다(합계 80만+ 퍼징).
`LOAD DATA LOCAL INFILE` 파일 탈취는 이중 차단으로 불가.

---

## 배선 진행 — **리더 게이트가 실제로 돈다** (2026-08-19)

`main.rs` 가 이제 저장소를 조립하고 리더 게이트 루프를 띄운다. 실제 프로세스 두 개로 검증:

```text
worker-a: 리더 0회   worker-b: 리더 1회   합계 1        ← F1: 정확히 하나
:8080 collect_leader=False ready=False                  ← standby 는 대상에서 빠진다
:8081 collect_leader=True  ready=True
리더에 SIGTERM → 리스 반납 1회 → 상대 인수 1초 (epoch 3)  ← TTL 60초를 기다리지 않는다
```

**배선하지 않으면 보이지 않았던 결함을 하나 잡았다.** 처음 배선했을 때 셧다운 단계
순서가 `lease → http` 였는데, `shutdown.trigger()` 가 `http` 단계에 있어서 `lease` 단계는
**아직 아무도 멈추라고 하지 않은** 루프를 10초 기다리다 타임아웃했고 리스는 반납되지
않았다. 단위 테스트로는 볼 수 없다 — 두 단계의 순서 문제다.

남은 것:

| # | 필요한 것 | 로드맵 | 상태 |
|---|---|---|---|
| 1 | ~~`SlowQueryStore` 구현 (DynamoDB)~~ | M2-4 | **완료 (2026-08-19).** `DynamoSlowQueryStore` + DynamoDB Local 통합 테스트 9건. AWS 자격증명 불필요 |
| 2 | 샤드 리스 + 수집 리더 게이트 (F1) | M4-21 | **어댑터 완료 (2026-08-19).** `DynamoLeaseStore` + `target_shard_count(is_leader)`. 불변식 `합계 = 64 또는 0` 을 코드가 강제하고 테스트가 워커 1~10대에서 확인한다. 남은 것: 수집 루프에 배선 |
| 3 | RDS 인스턴스 탐색 | M2-5 | **필터 완료.** T-37 판정을 순수 함수로 분리해 AWS 없이 전수 검증(9건) — prd VPC·미지 VPC·이름 거부·태그 AND. 남은 것: `DescribeDBInstances` 호출과 레지스트리 저장 |

### M2-4 가 리뷰로는 알 수 없던 것을 두 개 잡았다

배선이 리뷰보다 값이 크다고 판단한 근거가 바로 이것이다. 저장소를 실제로 붙이자마자
**리뷰 다섯 라운드가 볼 수 없던** 두 가지가 나왔다:

1. **`record_id` 로는 `SK` 를 복원할 수 없다.** `record_id` 는 `started_at_ms` 를 초 단위로
   절단해 담고(멱등 키가 ±1초 흔들림을 흡수하도록 의도된 설계다) `SK` 는 밀리초를 담는다.
   그래서 `GetItem` 이 불가능하고 `begins_with(SK, <초 접두>)` + `thread_id` 필터를 써야 한다.
   순수 로직 리뷰에서는 이 간극이 보이지 않는다 — 두 값이 다른 파일에 있다.

2. **테스트가 이전 실행 상태에 의존했다.** DynamoDB Local 은 `-dbPath` 로 영속이라
   이전 실행의 `finalized` 레코드가 남는다. 거기에 `in_flight` 를 병합하니
   `merge_state` 가 종료 상태 되돌리기를 (정확하게) 거부해 인덱스에 나타나지 않았다.
   **코드가 아니라 테스트가 틀렸다** — MySQL 쪽 `reset_targets` 와 같은 교훈이고, 이번에는
   `reset_table_for_local()` 로 매 실행 초기화한다.

또한 문서에만 있던 **GSI1 상태 전이**(진행 중 `SQS#in_flight` ↔ 확정 `DG#<digest>`)가
실제로 동작하는지 처음 확인했다. 그 키를 잘못 쓰면 AP-18 이 영구히 0건이고 고아가
TTL 35일까지 "실행 중" 으로 남는다 — 에러가 아니라 빈 결과라 운영에서만 드러난다.

**리뷰의 한계**: 배선되지 않은 코드에 대한 리뷰는 "이 로직이 맞는가" 까지만 답한다.
"이 로직이 실제로 불리는가", "루프가 예산 안에 도는가", "리스가 실제로 split-brain 을
막는가" 는 배선 후에만 검증된다. 3차의 C3-5(`warm()` 이 자기 시나리오에서 발동하지 않음)와
2차의 M2(호출부 없음)가 그 부류였고, **둘 다 배선이 없어서 리뷰로만 잡힌 것**이다.

---

---

## 4차 (2026-08-19) — 리뷰어 2명

대상은 3차가 바꾼 코드. **또 3차 자신의 수정에서 CRITICAL 1건 + HIGH 2건이 나왔다.**
"매 라운드 약 2건" 패턴이 네 라운드 연속 성립했다.

### 4.1 CRITICAL — `mask_label` 이 **세 번째로** 유출했다 (✅)

3차에서 렉서 기반으로 바꿨는데 근본 원인을 놓쳤다: **렉서는 주석을 토큰으로 만들지 않고
버린다.** 버려진 바이트는 스팬 **사이의 간격**에 남고, "리터럴 아닌 부분은 원문 복사"
방식이 그 간격을 그대로 복사한다.

MySQL 8.4.11 이 이걸 실제로 만든다 — v2 의 `operation` 은 테이블 별칭을 **백틱 없이** 낸다:

```sql
SELECT name FROM customers AS `c/*` WHERE email >= 'victim-ssn-900101-1234567@example.com'
```
```text
"operation": "Index range scan on c/* using uk_customers_email
              over ('victim-ssn-900101-1234567@example.com' <= email), ..."
```

`mask_plan` 출력 = **입력과 동일, redactions=0.** 닫히지 않은 `/*` 는 `unterminated` 를
세우지 않아 fail-closed 경로도 발동하지 않았다. 퍼징 **19.2% 유출**, 그중 87%가 주석 경로.
`y#`, `z-- ` 별칭도 같다. `Tok::Hint` 본문도 리터럴 집합에 없어 원문 복사됐다.

**대상 DB 에 쿼리를 날릴 수 있는 누구나 별칭을 고른다** — 의도적 exfiltration 수단이다.
1차 L1 · 3차 §3.2 와 **글자 그대로 같은 무성 유출**이 세 번째다.

수정: **스팬 사이의 간격은 공백뿐이어야 한다.** 아니면 렉서가 버린 내용이 있다는 뜻이므로
`REDACTED` + `redactions += 1`. 꼬리도 검사한다. `Tok::Hint` 도 리터럴로 취급한다.

**그리고 그 수정만으로는 부족했다.** 리뷰어의 유출 성질을 저장소 테스트로 옮기자
(마커를 항상 인용부호 안에 두고 바깥을 2,197 조합으로 흔든다) **네 번째 경로**가 나왔다:

```text
Filter: ` (t.c = 'ssn900101')  idx `
  → 짝 없는 백틱이 뒤 전체를 **하나의 식별자 토큰**으로 만든다
  → 원문 복사가 그 안의 리터럴을 내보낸다
```

간격 검사로는 못 잡는다 — 내용이 **토큰 안**에 있다. 그래서 리터럴이 아닌 토큰의
원문에 `'`·`"` 가 있으면 fail-closed 한다. 백틱 자체는 허용한다 — MySQL 이 라벨에서
식별자를 감쌀 때 쓰기 때문이다(`` (`c/*`.email >= ?) ``).

실서버 라벨은 여전히 `redactions=0` 으로 온전히 읽힌다:

```text
Filter: ((`c`.email >= ?) and (`c`.id < ?))
Index range scan on c using uk_customers_email over (? <= email)
Sort: c.name DESC  (cost=? rows=?)
```

**교훈**: 리뷰어의 퍼징 성질을 저장소 테스트로 옮긴 것이 즉시 값을 냈다. 외부 퍼징은
한 번 돌고 사라지지만 성질 테스트는 남아서 다음 회귀를 잡는다 — 실제로 같은 커밋 안에서
잡았다.

### 4.2 HIGH — 3차의 어휘 발산 통제가 **doc 주석 안에 들어갔다** (✅)

`target_sql_mode` 를 `impl TargetDb for TargetMysql` 에 넣으려 한 편집이 `session_init` 의
doc 주석 안에 삽입됐다. 결과: 트레이트 **기본 구현**(`Ok(String::new())`)이 반환되고,
빈 문자열은 "위험 없음" 으로 해석되므로 `lexical_divergence` 가 영구히 `false` 였다.
`ANSI_QUOTES` 대상의 플랜이 계속 "정확" 으로 저장된다. `GLOBAL_SQL_MODE` 는 호출부 0개였다.

**"미배선 수정" 의 세 번째 재발**이다 (2차 M2, 3차 MEDIUM-2, 4차 HIGH-1).
그래서 이번에는 **실제 어댑터가 서버에서 값을 가져오는지** 확인하는 테스트를 넣었다 —
기본 구현이 남아 있으면 빈 문자열이 오므로 실패한다.

### 4.3 HIGH — 한 주석의 결합 힌트를 통째로 잘라 플랜을 파괴하면서 "정확" 을 유지했다 (✅)

MySQL 은 한 주석에 힌트 여러 개를 허용한다. 3차의 `strip_resource_hints` 는 스팬 전체를
잘랐으므로 플랜 힌트까지 사라졌다. 8.4.11 실측:

| 문장 | access_type | key | rows |
|---|---|---|---|
| `/*+ NO_RANGE_OPTIMIZATION(orders PRIMARY) MAX_EXECUTION_TIME(600000) */` | `index` | `idx_orders_customer` | **60,023** |
| 주석 통째로 삭제 (3차가 보내던 것) | `range` | `PRIMARY` | **100** |

`is_exact = true` 로 저장했으므로 운영자는 60,023행 스캔 쿼리를 100행 플랜으로 본다 —
3차 §3.3 이 막으려던 것과 **같은 피해**다. 기존 테스트는 **분리된 두 주석**만 봤다.

수정: 힌트 본문을 파싱해 **자원 힌트 항목만** 제거하고 나머지를 재조립한다.
재구성한 힌트로 서버가 `rows=60023` 을 낸다는 것을 실측으로 확인했다.

### 4.4 MEDIUM — 힌트 절단이 `-` + `-` 를 `--` 로 접합해 문장을 잘라냈다 (✅)

스팬을 제거하고 아무것도 넣지 않아 양옆 문자가 붙었다. 8.4.11 실측 — 둘 다 유효한 문장이다:

```text
원문:   SELECT a FROM t WHERE x = 5 -/*+ MAX_EXECUTION_TIME(9) */- 3   → x = 2
접합후: SELECT a FROM t WHERE x = 5 -- 3                              → x = 5
```

라인 주석이 되어 뒤가 조용히 사라진다. `SELECT/*+h*/a` → `SELECTa` 도 같은 원인.
퍼징 540,752 입력 중 새 `--` 76건. 수정: 절단 자리에 공백 한 칸.
(좋은 소식: 새 `;` 0건, 진짜 `/*!` 0건 — **1차 S1 주입은 재개되지 않았다.**)

### 4.5 MEDIUM — 축약형 예외가 **세 번째로** 유출했다 (✅)

3차의 "접미 + 단어 경계" 규칙도 값이 축약형 접미로 시작하면 통과한다:

```text
scrub("Cannot convert x's,kim@example.com' to utf8mb4")
  → "Cannot convert x's,kim@example.com'?'"        ← 유출
```

`x's,` `x't.` `x'd)` `x'm ` `N've/` `_binary'd)` … 접미를 더 좁히는 방향은 **값의 내용에
의존해 끝이 없다.** 세 번 시도해서 세 번 실패했다.

그래서 **방향을 뒤집었다**: 인용부호 **앞**의 단어가 MySQL 리터럴 도입자(`x` `b` `n`,
`_` 로 시작하는 문자셋 도입자)면 그건 항상 리터럴이다. 도입자 집합은 **문법이 정한
닫힌 집합**이고 값과 무관하다 — 값의 내용에 의존하는 규칙을 값과 무관한 규칙으로 바꿨다.

⚠ 리뷰어 지적: 3차가 근거로 든 `Cannot convert x'topsecret' to utf8mb4` 는 **미검증 전제**다.
8.4.11 에서 그 형태의 메시지를 재현하지 못했다(실제 템플릿은 값을 공백으로 프레이밍한다).
클래스는 열려 있고 `Scrubbed` 는 AWS SDK 메시지에도 쓰이므로 수정은 유지한다.

### 4.6 나머지 (✅)

| # | 결함 | 처리 |
|---|---|---|
| LOW-1 | `RESOURCE_GROUP(name)` 이 자원 힌트인데 제거 집합에 없었다. 모니터링 롤에 `RESOURCE_GROUP_USER` 가 붙으면 공격자가 우리 EXPLAIN 스레드를 스로틀 그룹에 묶는다 | 제거 집합에 추가 |
| LOW-2 | `GLOBAL_SQL_MODE` 의 doc 이 `session_init` 의 doc 블록을 가로채 보안 통제 설명이 const 에 붙었다 | 분리 |
| LOW-3 | 3차 C3-12 가 "갱신했다" 고 했지만 `derive` 주석에 모순이 남았다 | 낡은 문단 제거 |
| LOW-4 | `data "aws_partition"` 이 "기존 VPC 는 data source 로만" 주석과 그 대상 사이에 삽입됐다 | 위치 정리 |
| — | **세 번째 크로스 바이너리 간섭.** `m1_capture` 의 `LOADGEN` 3곳이 여러 줄 형태라 3차 sed 가 놓쳤다 | 정규식으로 전부 `ROOT` 로. 병렬 2회 확인 |

### 4.7 리뷰어의 미해결 항목을 실측으로 닫았다

> "`max_execution_time` 이 **`EXPLAIN` 에 적용되는지** 판정하지 못했다. 적용되지 않는다면
> 3차 §3.7 이 지키는 서버측 바운드가 우리가 실행하는 문장에는 존재하지 않는다."

`optimizer_prune_level=0` + 12테이블 조인으로 충분히 느린 EXPLAIN 을 만들어 측정:

```text
max_execution_time = 50     → ERROR 3024 (ER_QUERY_TIMEOUT) — EXPLAIN 이 죽는다
max_execution_time = 60000  → 정상 완료
```

**적용된다.** 즉 §3.7 의 힌트 제거는 실재하는 바운드를 지킨다.

### 4.8 리뷰어가 확인한 "3차가 맞았다"

`.init` → `.setup` 은 sound — 프로덕션 풀 형태(min=2/max=4)와 1/1 양쪽에서 4회 재사용
동안 세 설정이 유지되고, `setup` 은 첫 커넥션에도 실행된다. 우회 경로 없음
(`hot`·`bulk` 모두 `pool()` 이 만들고 `Conn::new` 직접 호출이 없다).
`session_pins_survive_pool_reuse` 는 vacuous 하지 않고 recycler 를 실제로 지난다.
렉서 스팬 불변식은 500k 입력에서 역행 0·비문자경계 0이고, `end > len` 은 전부
`unterminated == true` 와 동시에 발생하므로 가드가 도달 불가.
피처 게이팅은 `cargo tree -e no-dev` 로 확인 — `testing` 은 dev-dependency 로만 들어오고
Dockerfile 은 `--all-features` 를 쓰지 않는다. `EXPR_KEYS` 경로는 진짜 fail-closed
(`mask_expression` 은 토큰에서 재렌더하므로 주석이 **삭제**된다).
`EXPLAIN … INTO OUTFILE` 은 파일을 쓰지 않고, `EXPLAIN` 은 스칼라 서브쿼리를 실행하지 않는다.
KMS `prevent_destroy` 는 새 락아웃을 만들지 않는다(`policy` 변경은 in-place).

---

### 4.9 4차 Rust 리뷰 — 4차 자신의 수정에서 다시 HIGH 2건 (✅)

리뷰어가 **자기 결론을 정정**한 것도 기록해 둔다: 플래키를 처음엔 `.setup` vs `.init` 에
귀속시켰다가, 부하를 통제한 재실행으로 그 귀속이 성립하지 않음을 확인하고 철회했다.
(부수 측정: `.setup` 은 풀 체크아웃당 +0.08ms — 왕복 1회 추가. 실재하지만 작다.)

| # | 결함 | 처리 |
|---|---|---|
| L2 | **`TargetMysql::warm()` 이 죽은 코드였다.** 고유 메서드로 정의해서 `InstanceCollector<D: TargetDb>` 의 `self.db.warm()` 이 **트레이트 기본 구현(`ping`)** 으로 해소됐다 — 제네릭 경계는 고유 메서드를 보지 못한다. **미배선 4번째 재발**이고 컴파일러가 잡지 못한다 | 트레이트 impl 로 옮겼다. **제네릭 경계를 통해** 부르는 테스트를 추가했다 |
| H1 | `merge_duration` 의 `span >= 0` 폴백이 **모순된 `ended_at_ms` 를 유지**했다. 157,464 삼중 중 5,916 이 결합법칙을 깼고 11,664 쌍 중 2,448 이 모순. 기존 전수 테스트는 모든 종료 시각을 `start + 양수` 로 만들어 이 축을 못 봤다 | **레코드별로** 종료 유효성을 판정한다(`ended >= 자기 시작`). 병합된 시작으로 판정하면 이전에 무효였던 값이 유효해져 결합법칙이 깨진다 |
| H2 | 구간을 **조밀** 시작(`started_at_ms`, 초 단위 `TIME` 유도)으로 계산해 `started_at_ms_precise`(`TIMER_WAIT` 보정)를 버렸다. 실측 400ms 짧게 저장 — `Timer` 를 도입한 이유가 사라졌다 | 정밀 시작을 우선한다. 일관성 불변식도 정밀 기준으로 바꿨다 |
| M1 | 구간에서 유도한 duration 을 `Timer` 로 표시해 `TIMER_WAIT` 정밀도를 가진 것처럼 보였다 | **`DurationSource::Span` 을 추가했다.** rank 는 `polled 0 < timer 1 < span 2 < slowlog 3` — 구간은 실행 전체를 덮으므로 도중 관측보다 낫다 |
| M2 | `lifecycle_days` 가 규칙 블록 경계를 안 봐서 **다음 규칙의 `expiration` 을 읽었다**(`ttl35` 를 물으면 `ttl400` 의 405). 조용히 통과 | 규칙 블록 안에서만 찾고, 주석·한 줄 형태도 처리한다. **주입 3종으로 확인** |
| M3 | `evicted_total`·`over_cap_ticks` 를 읽는 곳이 0개였다. 문서는 "관측 가능해야 한다" 고 적었지만 **읽히지 않는 카운터는 관측성이 아니다** | `TickStats.evicted`·`over_entry_cap` 으로 노출 + 경고 로그 |
| M5 | `RunningQuery` 에 `Drop` 이 없어 **단정 실패 시 `SLEEP(10)` 이 서버에 남았다** → 다음 테스트가 그걸 후보로 본다. 릴리스 플래키의 원인 중 하나 | `Drop` 에서 `KILL QUERY`. `handle` 을 `Option` 으로 바꿔 `Drop` 이 take 할 수 있게 했다 |
| M6 | 3차의 doc 주석 스플라이스 2곳이 살아 있었다 — 4차 HIGH 를 만든 것과 **같은 편집 실수** | 분리 |
| L1 | `cfg` 된 `fakes` 로의 rustdoc 링크가 깨졌다 | 링크 제거 |
| L3 | `needs_warm` 이 **모든** probe 오류에 표시돼, 권한 오류(1045/1142)에서도 매 tick 왕복을 낭비했다 | `is_retryable()` 일 때만 표시 |
| L5 | `support` 주석이 "`cargo test` 는 바이너리를 병렬로 돌린다" 고 단정했는데 리뷰어 측정으로는 동시에 하나다 | 규칙의 근거를 "보장이 아니라 현재 동작" 으로 정정 |

**M4(쓰기 증폭)는 남긴다.** 축출 500건이 tick 안에서 순차 `upsert_merged` 500회가 된다.
CPU 비용은 3차에서 고쳤지만 I/O 는 그대로다. 배치 쓰기가 필요하고 그건 DynamoDB 어댑터
설계와 함께 결정해야 한다 — 잔여 항목에 있다.

### 4.10 4차 Rust 리뷰가 확인한 "3차가 맞았다"

축출은 진짜 O(n log n) 이다 — 릴리스 실측 50,000 엔트리에서 10,001 축출 **1.39ms**
(3차 이전 판은 2.6초). 캐시는 관측 엔트리 면제에도 상한에 수렴한다(220 tick 모델링).
`!seen.insert(id)` 는 `ORDER BY TIME DESC` 아래에서 큰 `TIME` 을 남긴다.
`strip_resource_hints` 는 200,000 적대적 입력에서 패닉·토큰 접합·우회 0.
`mask_plan` 은 독립 퍼징 300,000 조합에서 유출 0. `testing` 피처는 릴리스 빌드에
`feature="testing"` rustc 호출이 **0개**이고 릴리스 바이너리에 `fake` 심볼이 없다.
`started_at_ms` 축은 교환·결합법칙을 깨지 않는다 — **`ended < started` 만** 깼다(H1).

### 4.11 5차 대기 중 — 내가 먼저 찾은 미검증 축 (✅)

4차의 전수 테스트에 **`started_at_ms_precise` 축이 빠져 있었다.** 그래서 조밀값 재시도
경로(`precise > ended` 일 때)를 한 번도 지나지 않았다. 축을 독립적으로 흔들자 784 쌍에서
`duration != ended - precise` 가 나왔다 (결합법칙은 0 위반 — 그쪽은 4차 수정이 옳았다).

원인은 **지역적으로 판정할 수 없는 조합**이다. 병합된 `precise` 는 두 레코드의 `min`,
병합된 `ended` 도 `min` 이다. 한쪽이 정밀값만 갖고 다른 쪽이 종료만 가지면 서로를 본 적
없는 두 값이 한 레코드에 모인다:

```text
a: precise = +2000, ended = None      ← a 기준으로는 유효하다
b: precise = None,  ended = +1000     ← b 기준으로도 유효하다
병합: precise = +2000, ended = +1000  ← precise > ended
```

"내 정밀값이 **남의** 종료보다 이른가" 는 레코드별로 판정할 수 없고, 병합 후에 판정하면
중간 결과가 달라져 **결합법칙이 깨진다** — 4차 H1 이 정확히 그 부류였다.

**그래서 수정 대상을 코드가 아니라 불변식으로 잡았다.** 실제로 참인 것으로 좁힌다:

> `duration_ms` 는 기록된 **두 시작 시각 중 하나**로부터의 구간이다.

제3의 숫자를 만들지 않으므로 검증 가능하고, 데이터를 버리지도 않으며, 결합법칙을
지킨다. 전수 테스트에 `precise` 축을 추가했고 통과한다.

**교훈**: 세 값(duration·종료·정밀시작)의 관계를 **두 개씩 짝지어** 정하면 남은 한 쌍이
어긋난다. 이게 "모순을 다른 필드로 옮김" 이 네 번 반복된 이유다. 이번에는 세 값을 한
문장으로 서술할 수 있는 불변식을 먼저 정하고 거기에 맞췄다.

---

## 네 라운드에서 배운 것

같은 결함이 세 번씩 재발한 이유를 적어 둔다.

| 재발 | 횟수 | 왜 |
|---|---|---|
| `mask_label` 유출 | 3 (1차 L1 → 3차 §3.2 → 4차 §4.1) | **문자열을 직접 훑었다.** 매번 새 이스케이프·새 토큰 종류를 놓쳤다. 4차에 와서야 "덮이지 않은 바이트가 있으면 포기" 라는 **완전성 검사**로 바꿨다 |
| 축약형 예외 유출 | 3 (2차 → 3차 §3.6 → 4차 §4.5) | **값의 내용에 의존하는 규칙**을 세 번 정교화했다. 값과 무관한 규칙(도입자 집합)으로 뒤집자 끝났다 |
| 미배선 수정 | 3 (2차 M2 → 3차 MEDIUM-2 → 4차 §4.2) | "고쳤다" 를 컴파일 성공으로 확인했다. 호출부가 없으면 컴파일은 통과한다 |
| 크로스 바이너리 간섭 | 3 | 락이 프로세스를 넘지 않는다는 사실을 매번 부분적으로만 적용했다 |
| 미배선 수정 (제네릭 경계) | 4번째 | `warm()` 을 고유 메서드로 둬서 트레이트 기본 구현이 쓰였다. **컴파일러가 잡지 못하는 형태** |
| 모순을 다른 필드로 옮김 | 3 (2차 H4′ → 3차 §3.4 → 4차 H1/H2) | duration·종료·정밀시작 세 값의 관계를 한 번에 정하지 않고 두 개씩 짝지었다 |

**교훈 세 줄.**
휴리스틱을 정교화하는 방향은 끝이 없다 — 판정 자체를 값과 무관한 것으로 바꿔야 끝난다.
직접 훑는 파서는 매번 새 입력에 진다 — 완전성을 검사하고 아니면 포기하는 편이 짧고 안전하다.
컴파일 성공은 배선의 증거가 아니다 — **통제가 무력화됐을 때 실패하는 테스트**가 증거다.

---

---

## 5차 (2026-08-19) — 수렴 확인 라운드

4차 리뷰어가 "H1/H2 를 고치고 빠진 축을 넣으면 다음 라운드는 깨끗할 것" 이라고 예측했다.
**merge.rs 에 대해서는 예측이 맞았다** — H1/H2 는 진짜로 고쳐졌고 지목한 축이 통과한다
(결합법칙 0/2,985,984 삼중, 교환법칙 0/20,736 쌍, `precise` 축 포함).

**그러나 전체로는 아니다. `mask_label` 이 다섯 번째로 유출했다.**

### 5.1 CRITICAL — 인용부호 **없는** 리터럴이 백틱 토큰을 통해 나갔다 (5번째)

4차의 검사는 리터럴이 아닌 토큰의 원문에서 `'`·`"` 만 걸렀다. 짝 없는 백틱이 렉서로
하여금 뒤 전체를 **식별자 하나**로 삼키게 만들면, 그 안의 숫자·16진수·실수는 인용부호가
없으므로 통과한다. **4차 자신의 성질 격자**를 마커만 인용 없이 바꿔 돌린 결과:

| 마커 형태 | 유출 |
|---|---|
| `'ssn900101'` (4차가 측정한 모양) | 0 / 2,197 |
| `9001011234567` | **21 / 2,197 (1.0%)** |
| `0x536563726574` | **21 / 2,197 (1.0%)** |
| `1.5e300` | **21 / 2,197 (1.0%)** |

```text
IN : Filter: ` (t.c = 0x536563726574)  idx `
OUT: 입력과 동일, redactions=0        ← 0x536563726574 = "Secret"
```

`0x536563726574` 는 **3차 §3.2 가 자기 유출을 시연할 때 쓴 그 값**이다.

**왜 또 뚫렸나**: 4차가 "완전성 검사로 바꿨다" 고 적었지만 완전하지 않았다. 검사한 것은
(a) 스팬 **사이**의 바이트와 (b) 리터럴 아닌 토큰 안의 **인용부호**뿐이고,
(c) **리터럴 아닌 토큰 안의 리터럴**은 남아 있었다. 마지막 원문 복사 경로가 그것이다.

수정: **원문 바이트가 그 토큰 종류와 일치하는지** 본다(`raw_matches_token_kind`).
백틱 식별자만 임의 내용을 담을 수 있으므로 — 다른 종류는 렉서의 토큰 경계가 모양을
이미 제한한다 — 백틱 안의 내용이 **식별자인지** 확인한다.

실서버 라벨은 `redactions=0` 으로 온전히 읽힌다(CJK 식별자 포함):

```text
Filter: ((`c`.email >= ?) and (`c`.id < ?))
Filter: (`한글컬럼` = ?)
Single-row index lookup on o using PRIMARY (id=?)
```

공백이 든 컬럼명(`` `my col` ``)은 `REDACTED` 가 된다. 드물고, 안전한 방향이다.

**테스트가 자기가 방어하는 모양만 측정하고 있었다.** 인용 없는 축을
`unquoted_literals_never_survive_either` 로 고정했다.

### 5.2 HIGH — 릴리스 플래키의 진짜 원인 (4차 귀속이 틀렸다)

4차는 릴리스 플래키를 `RunningQuery` 의 `Drop` 부재로 귀속했다. `Drop` 을 넣었는데도
14회 중 2회(~14%) 실패했다. 진짜 원인은 **테스트가 불변식이 아닌 것을 단정**한 것이다:

```rust
assert_eq!(s.deep_probe_skipped, s.candidates - 1);   // 불변식이 아니다
```

`deep_probe_skipped` 는 `needs_deep_probe` 초과분인데, 엔트리는
`plan_attempts >= max_plan_attempts` 가 되면 **후보로 남은 채** `needs_deep_probe` 에서
빠진다. tick 이 여러 번 도는 동안 하나가 시도를 소진하면 `skipped < candidates - 1` 이
된다 — 릴리스가 빨라 tick 이 많이 돌므로 **릴리스에서만** 나타났다.

확인할 것은 "상한이 지켜졌고 초과분이 보고된다" 다. 단정을 그렇게 바꾸고
**릴리스 5회 연속 통과**를 확인했다.

### 5.3 나머지 (전부 ✅)

| # | 결함 | 처리 |
|---|---|---|
| MEDIUM-1 | `started_at_ms_precise` 에 `<= started_at_ms` 불변식이 없었다. 두 값은 서로 다른 tick 의 EMA 를 쓰므로 역전 가능하고, `display_started_at_ms()` 가 정밀값을 반환하므로 **운영자가 "종료가 시작보다 이른" 레코드를 본다**. 사전 존재 결함 — 4차가 만든 것이 아니다 | **발생 지점에서 클램프**한다(`inflight.rs`, 한 줄). 병합에서 막으려면 "내 정밀값이 남의 종료보다 이른가" 를 물어야 하는데 그건 지역적으로 판정 불가고 결합법칙을 깬다(4차 H1) |
| MEDIUM-2 | 도입자 "닫힌 집합" 에 시간 리터럴(`date'…'`·`time'…'`·`timestamp'…'`)이 빠졌다. 208/312 유출. 도달성은 낮지만 그 논거가 **값의 내용에 의존한다** — 4차가 없애려던 종류의 논거다 | 6개로 확장 |
| MEDIUM-3 | 규칙 블록 경계가 리터럴 `"\n  rule {"` 매칭이라 4칸·탭 들여쓰기에서 **조용히 다음 규칙의 값을 읽었다**(`ttl35` → 405) | `rule` 을 **단어**로 찾고, `expiration` 이 정확히 하나인지 단정한다. **포맷 변형 5종 주입으로 확인** |
| LOW-1 | `Drop` 의 정리 스레드에 상한이 없어 컨테이너가 멈추면 무한 블록 | `tokio::time::timeout` 으로 감쌌다 (`OptsBuilder` 에 연결 타임아웃이 없다 — 프로덕션도 같은 이유로 감싼다). `kill_and_wait` 후 `mark_cleaned()` 로 중복 kill 제거 |
| LOW-3 | `filter_hint_body` 의 doc 이 `None` 의 의미를 반대로 적었다 | 정정 |

**LOW-2(`over_entry_cap` 이 프로덕션에서 도달 불가)는 남긴다.** `detect_limit ≤ 10,000 <
50,000` 이라 현재 검증 아래서는 참이 될 수 없다. 필드를 지우는 대신 남기는 이유: 상한을
설정으로 노출하는 시점(배선 커밋)에 도달 가능해진다. 그때까지 항상 `false` 다.

**M4(쓰기 증폭)도 남긴다** — 배치 쓰기는 DynamoDB 어댑터 설계와 함께 결정한다.

### 5.4 5차 리뷰어가 확인한 "4차가 맞았다"

`filter_hint_body`/`strip_resource_hints` — 390,625 적대적 입력에서 패닉 0, 새 `--` 0,
새 `;` 0, 새 `/*!` 0. `depth` 언더플로 없음(릴리스 wrapping 모드로도 확인).
MySQL 8.4.11 실측으로 "필터를 통과하는" 모든 변형이 서버에서도 **무시된다**.
`merge_duration` 은 `precise` 축을 독립적으로 흔들어도 교환·결합법칙을 만족한다 —
모든 항이 최종 입력의 `min` 이기 때문이다(5차 리뷰어의 반대 가설이 틀렸다).
`DurationSource::Span` 은 exhaustive match 파괴 없음, serde 왕복 정상.
`mask_label` 의 유니코드 공백 간격 검사는 NBSP·U+3000·NEL·VT·FF·ZWSP 에서 오탐 없음.
`Tok::Hint → ?` 는 실제 라벨 내용을 파괴하지 않는다.

---

## 다섯 라운드 결산

| 재발 클래스 | 횟수 | 끝낸 방법 |
|---|---|---|
| `mask_label` 유출 | **5** | 원문 복사의 **모든** 경로를 열거했다: 이스케이프(렉서 사용) → 스팬 사이 간격(공백만) → 토큰 안 인용부호 → **토큰 안 리터럴**(종류 일치 검사). 매번 "완전성 검사" 라고 적었지만 실제로 완전해진 것은 다섯 번째다 |
| 축약형/도입자 유출 | **4** | 값 내용 의존 규칙(접미 정교화) → 값 무관 규칙(도입자 집합) → 집합 확장. 방향 전환이 옳았고 집합이 덜 열거됐던 것이 남은 문제였다 |
| 미배선 수정 | **4** | 컴파일 성공 → 호출부 확인 → **통제 무력화 시 실패하는 테스트** → 제네릭 경계를 통한 호출 테스트 |
| 모순 이동 | **4** | 두 값씩 짝지어 정함 → 세 값을 한 문장으로 서술하는 불변식 + 발생 지점 클램프 |
| 테스트가 자기 모양만 측정 | **2** | 4차 격자(인용 마커) → 5차가 같은 격자를 인용 없이 돌려 1.0% 발견. **성질 테스트도 입력 모양을 열거해야 한다** |

**가장 값진 교훈**: 리뷰어의 퍼징 성질을 저장소 테스트로 옮긴 것이 두 번 즉시 값을 냈다
(4차에서 백틱 경로, 5차에서 인용 없는 경로). 외부 퍼징은 한 번 돌고 사라지지만 성질
테스트는 남아서 **다음 라운드의 결함을 그 라운드 안에** 잡는다.

**수렴 판단**: 5차의 CRITICAL 은 4차 수정이 만든 것이고, 나머지는 사전 존재 또는 4차가
만든 작은 것들이다. 그런데 `mask_label` 은 이제 원문 복사 경로가 열거로 닫혔고
(간격·인용부호·리터럴·힌트), 각 경로에 성질 테스트가 붙었다. 다음 라운드의 값은
**배선되지 않은 코드**를 더 보는 것보다 낮다 — 아래 "가장 큰 남은 격차" 를 먼저 닫는다.

---

## 잔여 항목 (다음 라운드)

> **1차 마무리 (2026-08-19)**: 아래 중 취소선이 그어진 10건은 1차에서 처리했다.
> 배선이 필요한 항목(M11~M16, M24~M26)만 남았다.


우선순위 순. 실재하지만 이번 라운드에서 처리하지 않았다.

| # | 항목 | 왜 지금 아닌가 |
|---|---|---|
| L3 | `tree_text` 마스킹 | 배선 전이라 dormant. **배선하는 커밋에서 함께** |
| M12 | `db_now_ms: EpochMs` 가 `Option` 이어야 한다. 0 이면 `LAST_SEEN >= FROM_UNIXTIME(-5)` → NULL 비교 → **영구히 0행**인 자기 유지 루프 | 다이제스트 스냅샷 배선(M4-14)과 같은 커밋 |
| M11 | `DIGEST_OVERFLOW` 조회 실패를 `unwrap_or_default()` 로 삼켜 "오버플로 없음" 이 된다 | 동일 |
| M13 | 리셋 감지가 `count_star` 감소만 본다. `first_seen_ms` 전진도 봐야 한다 | 동일 |
| M14 | `SnapshotCache` 에 축출이 없다 → 장기 실행 워커 메모리가 무한 증가 | 동일 |
| M15 | `per_digest` 가 `app_digest` 만 키로 써서 멀티테넌트 인스턴스에서 스키마가 오귀속된다 | 동일 |
| M16 | 심층 조회가 tick 안에서 **직렬**로 돈다. 문서는 "tick 밖의 병렬 태스크" 라고 명시 | M4-21(리스) 이후 구조 변경과 함께 |
| ~~M17~~ | 연결 획득 타임아웃(5초)이 detect 예산(800ms) **밖**에 있어 총 5.8초 가능 | ✅ **검증이 아니라 코드로 고쳤다.** `connect + detect <= tick_budget` 를 강제하면 콜드 경로(TLS+IAM)에 필요한 5초가 거부되고 정상 기본값이 무효가 된다 — 내가 처음 넣은 검증이 실제로 기본값을 깨뜨렸다. 대신 `probe` 가 **획득+조회 전체**를 `detect_total`(1 interval) 안으로 묶는다. 초과하면 그 tick 을 버리고, 그 사이 풀이 커넥션을 확보하므로 다음 tick 은 warm 이다 |
| M18 | `UNIX_TIMESTAMP(FIRST_SEEN)*1000` 은 DECIMAL 로 온다(`timestamp(6)`). `num::<i64>` 가 실패해 **조용히 0** | 실인스턴스 타입 확인 후. 같은 파일이 `db_now` 는 `f64` 로 읽는다 |
| ~~M20~~ | 절단된 SQL 로 EXPLAIN 을 재실행한다. 잘린 지점이 우연히 유효하면 **다른 쿼리의 플랜이 `is_exact` 로 저장된다** | ✅ `collect_plan` 이 `is_info_truncated` 를 확인한다. 기존 주석은 '잘린 SQL 은 거부된다' 고 했지만 `plan_query` 는 닫히지 않은 인용부호만 잡았다 |
| M21 | `DELETE … USING` / `INSERT … ON DUPLICATE KEY UPDATE` 가 깨진 문장을 만든다 | 데이터 변경 위험은 없다(EXPLAIN 은 실행하지 않는다). 플랜을 영구히 못 얻는 것이 비용 |
| ~~M22~~ | `HourBucket` 이 `serde(transparent)` 로 **검증 없이** 역직렬화된다. `&self.0[..7]` 가 손상된 값에 패닉 | ✅ `year_month`/`date_part` 를 `get(..n)` 으로. `is_valid()` 추가 |
| ~~M23~~ | `TimeRange::date_parts()` 가 구간 길이 제한 없이 커진다. API 파라미터 경로다 | ✅ `MAX_DATE_PARTS = 10_000` + `checked_add` |
| M24 | IAM 토큰 만료(1045)를 재시도 불가로 분류한다. RDS IAM 토큰은 15분마다 만료된다 | 토큰 갱신 배선(M3)과 함께 |
| M25 | `rbac` 의 빈 토큰 스코프가 "전체 허용" 이고, `Full`/`FullRestricted` 구분이 인가 경로에서 죽어 있다 | M5(인증) 에서 |
| M26 | `last_collect_ok_ms` 가 준비 판정에 안 들어간다 — "살아 있지만 아무것도 수집하지 않는" 상태가 200 이다 | M4-21 에서 리더 게이트와 함께 |
| ~~M27~~ | `digest/status/health_interval_ms` 범위 검증 없음(0 이면 핫 루프). `collector.detect_limit` 이 상수에 밀려 **조용히 무시**된다 | ✅ `digest`/`status`/`health_interval_ms` 범위 검증 추가(0 은 '비활성' 이 아니라 '즉시 반복' 이다). `detect_limit` 은 `TargetMysql::connect_with_limit` 으로 전달 — 상수가 항상 이겨서 `truncated` 판정까지 500 기준이었다 |
| ~~M28~~ | `FakeDigestStore::new()` 와 `default()` 가 **반대로** 동작한다(전부 수락 vs 전부 거부) | ✅ `Default` derive 를 제거하고 `Default = new()`. `AtomicUsize::default()` 가 0(=전부 거부)이라 두 생성자가 정반대였다 |
| ~~L30~~ | `drain()` 이 셧다운을 `TooLong` 으로 기록해 사후 분석이 원인을 구분할 수 없다 | ✅ `FinalizeReason::Shutdown` 추가. `long_running` 오탐과 사후 분석 혼동을 막는다 |
| ~~L31~~ | 같은 tick 에 동일 `thread_id` 가 두 번 오면 쓰레기 레코드가 생긴다 | ✅ 같은 tick 의 중복 `thread_id` 는 첫 관측만 쓴다 |
| ~~L32~~ | `app_digest` 는 existing 유지, `digest_algo_version` 은 `max()` → 버전이 다이제스트를 설명하지 않는다 | ✅ `merge_digest` 가 `app_digest` 와 `digest_algo_version` 을 짝으로 고른다 |
| ~~L39~~ | `find_query_cost` 의 doc 과 구현이 다르다(항상 트리 최대) | ✅ 최상위 `query_block.cost_info.query_cost` 우선, 없으면 트리 최대값(v2 경로) |

**결정 대기** (사용자):

| # | 질문 | 현재 기본값 |
|---|---|---|
| OPEN-Q-15 | prd 리터럴 저장 정책 | 코드는 `masked`. FR-CAP-07 은 `full_restricted` 를 명시하지만 **소급 취소가 불가능**하므로 보수적으로 두었다 |
| M0-13g | Cognito 콜백에 `http://localhost:8080/auth/callback` 등록 가능 여부 | 미확인 (AWS 필요) |
| OPEN-Q-21 | dev/prd 토폴로지 검증 격차 | 미해소 |
| C-26 | RDS 슬로우로그 `# Time:` 이 완료 시각인가 | **미검증인데 C-* 제약으로 단정돼 있다.** ±2s 병합 창이 여기 의존한다 |

---

# 6차 라운드 — 배선된 코드 (2026-08-19)

`565667e` 이후 처음 도는 리뷰다. 이전 5라운드가 "실행되지 않는 코드" 를 봤다면,
이번은 **실제로 도는 코드**를 봤다. 결과가 그 차이를 그대로 보여 준다:
CRITICAL 3건과 데이터 파괴 경로 2건이 나왔고, **전부 배선 이후에만 존재할 수 있는
결함**이었다.

## 판정 요약

| # | 결함 | 등급 | 상태 |
|---|---|---|---|
| R6-1 | `role=api` 워커가 수집 리더 리스를 가로챈다 | CRITICAL | ✅ |
| R6-2 | `role=api` 워커가 영원히 `/readyz` 503 (R6-1 수정이 드러냄) | CRITICAL | ✅ |
| R6-3 | 탐색이 20초 붙잡는데 셧다운은 10초 대기 → 리스 누출 | CRITICAL | ✅ |
| R6-4 | 필터 거부·매핑 실패를 "사라졌다" 로 판정 → 등록부 전멸 | CRITICAL | ✅ |
| R6-5 | `stamp_deleted` 무조건 쓰기 → 손상 항목 1개가 `list()` 를 영구히 죽인다 | HIGH | ✅ |
| R6-6 | 예산 타임아웃이 `mark_missing` 중간을 잘라 증가분을 남긴다 | HIGH | ✅ |
| R6-7 | 필터가 `Environment=production` 태그를 보지 않는다 | HIGH | ✅ |
| R6-8 | 이름 거부가 Aurora 클러스터명을 보지 않는다 | HIGH | ✅ |
| R6-9 | `endpoint_url` 게이트가 `== Prd` — 기본값 `unknown` 이 통과 | HIGH | ✅ |
| R6-10 | `reconcile_one` 이 죽은 코드 (규칙이 두 곳에 적혀 있다) | HIGH | ✅ |
| R6-11 | `/readyz` 가 네트워크를 안 보고 `storage_ok=true` | MEDIUM | ✅ |
| R6-12 | `required_tags` 키 조회만 대소문자 구분 | MEDIUM | ✅ |
| R6-13 | 명시 거부목록이 T-37 기본값을 **대체**(union 아님) | MEDIUM | ✅ |
| R6-14 | 페이크가 임계 규칙을 다시 적어 규칙 드리프트를 못 막는다 | MEDIUM | ✅ |
| R6-15 | `work_budget` 테스트가 항진식 (`x <= x`) | MEDIUM | ✅ |
| R6-16 | `RawDbInstance.status` 를 옮겨만 놓고 안 읽는다 | MEDIUM | ✅ |
| R6-17 | 500건 순차 쓰기 → 조절 시 20초, 예산 초과 | MEDIUM | ✅ |
| R6-18 | `errors` 를 세기만 하고 아무도 반응하지 않는다 | MEDIUM | ✅ |
| R6-19 | `map_sdk_err` 가 원시 SDK Debug 를 담아 로그로 유출 | MEDIUM | ✅ |
| R6-20 | 통합 테스트가 DynamoDB 없으면 조용히 `ok` 로 통과 | MEDIUM | ✅ |
| R6-21 | `aws.endpoint_url` 이 죽은 설정 (게이트 밖) | LOW | ✅ 삭제 |
| R6-22 | 역할 오버라이드가 검증 이후에 적용 | LOW | ✅ |
| R6-23 | 엔드포인트 URL 원문 로그 (자격증명 유출 여지) | LOW | ✅ |
| R6-24 | `is_collectible()` 호출부 없음 | — | ⏳ M2-6 게이트 |
| R6-25 | `account_id` 를 STS 로 대조하지 않음 | — | ⏳ |
| R6-26 | 태그 맵 인스턴스당 2회 복제 | — | ❌ 기각(5분당 1만 할당) |

## 이번 라운드에서 배운 것

### 1. 하나를 고치면 아래에서 두 번째가 나온다

R6-1(`api` 워커가 리스를 가로챔)을 고치자 R6-2 가 드러났다 — 준비 판정이
`serves_api && collect_leader` 였으므로, 리스를 잡지 않게 만든 `api` 워커는
**영원히 준비되지 않는다.** ALB 가 트래픽을 보내지 않아 API 가 전면 중단된다.

즉 첫 결함이 두 번째를 **가리고 있었다.** api 워커가 리스를 훔쳤기 때문에
`collect_leader=true` 가 되어 준비 판정을 통과했던 것이다. 두 결함이 서로를
상쇄해 역할 분리가 "동작하는 것처럼" 보였다.

고친 규칙: 리더 여부는 **API 와 수집을 겸하는 워커**만 따진다.
`role=all` 의 standby 는 여전히 ALB 에서 빠진다(FR-OPS-08).

### 2. F1 불변식이 성립하는데도 수집이 0건일 수 있다

`합계 ShardsOwned = 64 또는 0` 은 R6-1 상태에서도 **성립한다** —
`shards_owned()` 가 `is_leader()` 하나에서 파생되기 때문이다. 산술 불변식을
코드로 강제해도 "리더가 엉뚱한 워커" 는 잡지 못한다.

**불변식은 자기가 참조하는 값만 검증한다.** `is_leader()` 가 옳은 워커에서
참인지는 별개의 문제이고, 그건 두 프로세스를 실제로 띄워야 보인다.

### 3. "없다" 의 이유를 구분하지 않은 것이 가장 위험했다 (R6-4)

등록부에 있는데 탐색 결과에 없으면 미발견으로 처리했다. 그런데 없는 이유는
세 가지고 하나만 진짜 삭제다:

| 왜 없는가 | 예전 판정 | 지금 |
|---|---|---|
| AWS 가 주지 않았다 | 미발견 → 2회면 삭제 | 그대로 (맞다) |
| API 가 부분 실패했다 | 건너뜀 | 그대로 (맞다) |
| **필터·매핑이 제외했다** | **미발견 → 2회면 삭제** | `Excluded` 전환 (비파괴) |

세 번째의 방아쇠가 셋이나 있고 **API 는 완전히 정상이다**:
`DBSubnetGroup` 누락(전건 VPC 거부), AWS 의 새 버전 문자열(전건 매핑 실패),
태그 일괄 변경(전건 `MissingTag`). 어느 하나로 500대가 10분 뒤 `Deleted` 가 된다.

`InstanceState::Excluded` 를 새로 만들었다. `Disabled`(사용자가 끔)로 쓰면
"내가 끄지 않았는데" 가 되고, `Deleted` 로 쓰면 파괴적이다.

그리고 그 새 상태가 곧바로 다음 결함을 만들었다 — `pick_state` 의 "사유가
사라지면 재평가" 목록에 `Excluded` 를 넣지 않아 **한 번 제외된 인스턴스가
영구히 수집되지 않았다.** 테스트가 잡았다. 상태를 추가하면 그 상태를 보는
모든 분기를 세어야 한다.

### 4. 취소 가능한 구간과 그렇지 않은 구간을 나눠야 했다 (R6-3, R6-6)

`tokio::time::timeout` 으로 라운드 전체를 감쌌더니 두 가지가 깨졌다:

- 셧다운 중 최대 20초 갇혀 반납 단계(10초)가 먼저 타임아웃 → **리스 누출**
- `mark_missing`(유일한 비멱등 쓰기) 중간에 잘려 **증가분이 남은 채 라운드가
  실패로 보고** → "2회 **연속**" 성질이 깨진다

구조를 나눴다: **조회(읽기·순수 판정)는 취소 가능**, **재조정(쓰기)은 취소하지
않는다.** 예산 초과는 라운드를 버리는 대신 `truncated=true` 로 바꾼다 —
버리면 다음 라운드도 같은 이유로 초과해 등록부가 영구히 갱신되지 않는다.

### 5. 항진식 테스트를 다섯 라운드 동안 못 알아봤다 (R6-15)

`work_budget() <= LEASE_RENEW_INTERVAL_MS` 를 "불변식을 고정한다" 며 썼는데,
`work_budget()` 이 그 상수로 **정의돼** 있으니 `x <= x` 였다. 상수 값이 무엇이든,
아무도 그 함수를 부르지 않아도 통과한다.

진짜 성질은 `갱신 주기 + 작업 예산 + tick < TTL` 이다. 갱신 주기를 39초로 올려
확인했다 — 예전 테스트는 통과하고 새 테스트는 "최악 간격 79000ms 가 TTL
60000ms 를 넘는다" 로 실패한다.

**테스트가 검증하는 것이 정의인지 성질인지 구별해야 한다.** 파생값을 그 파생의
근거와 비교하면 아무것도 검증하지 않는다.

### 6. 상수를 공유해도 규칙은 갈라진다 (R6-14)

`MISSING_THRESHOLD` 를 공유했지만 페이크는 비교식을 **다시 적었다**
(`inst.missing_count >= MISSING_THRESHOLD`). 그래서 `should_mark_deleted` 를
`>` 로 바꿔도 페이크 기반 FR-DSC-07 테스트 4건이 전부 통과했다.

술어를 `dbmon_core::instance` 로 옮겨 양쪽이 같은 함수를 부르게 했다.
`>` 재주입으로 확인 — 이제 2건이 실패한다.

**값의 드리프트를 막는 것과 규칙의 드리프트를 막는 것은 다르다.**

### 7. 조용히 건너뛰는 테스트는 커버리지가 0이다 (R6-20)

`it_store` 23건이 DynamoDB Local 에 못 붙으면 `return` 으로 빠지면서
`ignored` 도 아니라 **`ok`** 로 보고됐다. 이 파일이 등록부·리스 어댑터를 덮는
유일한 경로인데, 서비스 컨테이너가 깨지면 조용히 초록이 된다.

`DBMON_REQUIRE_DYNAMO=1`(CI 가 준다)이면 건너뛰지 않고 실패한다.
컨테이너를 실제로 멈춰 확인했다 — 없을 때 `ok 2건` → `FAILED 2건`.

## 재발 추적 — "고쳤는데 호출부가 없다" 다섯 번째·여섯 번째

| 회차 | 대상 | 발견 방법 |
|---|---|---|
| 1 | `connect_with_limit` | 리뷰 |
| 2 | `from_config` | 리뷰 |
| 3 | `target_sql_mode` (편집이 doc 주석에 들어갔다) | 리뷰 |
| 4 | `warm()` (고유 메서드라 트레이트 기본값이 이겼다) | 실행 |
| 5 | `default_denied_for_dev` (테스트만 부름) | 자체 리뷰 |
| 6 | `reconcile_one` (규칙이 두 곳, 쓰는 쪽은 다른 곳) | 리뷰 |
| 7 | `RawDbInstance.status` (채우기만 하고 안 읽음) | 리뷰 |

7회다. 이번에 대응책을 하나 넣었다 — `Filter::from_config` 를 **설정에서
만드는 유일한 경로**로 두고 테스트도 그걸 지나게 했다. 테스트가 필드를 다시
옮기면 배선이 아니라 자기 자신을 검증한다.

## 검증 증거 (실제 프로세스)

```text
── 역할 분리 (role=api + role=collector) ────────────────────────────────
리스 소유자: collector-col-1          ← api 가 먼저 떴는데도 가로채지 않는다
:8090 api       ready=true  collect_leader=false   ← ALB 가 트래픽을 보낸다
:8091 collector ready=true  collect_leader=true
탐색: api=0회  collector=1회
반납: 1회

── role=all 2대 (FR-OPS-08 유지) ────────────────────────────────────────
리더 합계 1
:8090 leader=False ready=False reason=standby   ← standby 는 ALB 에서 빠진다
:8091 leader=True  ready=True

── 자격증명 전무(SSO 만료 재현) + 등록부에 시드 1건 ──────────────────────
"탐색이 부분 결과였다 — 미발견 판정을 건너뛴다" skipped=1   (2회)
시드 인스턴스: missing_count=0  state=collecting  deleted_at=NULL
```

## 결함 재주입 확인

| 주입 | 잡은 테스트 |
|---|---|
| `if_not_exists(deleted_at_ms, ...)` | `two_consecutive_misses_...`, `reappearing_...` |
| `apply_derived_defaults()` 호출 제거 | `dev_deployment_gets_the_default_deny_list` 외 1 |
| `should_mark_deleted` 를 `>` 로 | `two_missed_rounds_mark_deleted` 외 1 |
| `REMOVE #ttl` 제거 | `retention_ttl_is_set_only_when_deleted` |
| TTL 설정 제거 | 같은 테스트 (다른 단정) |
| `LEASE_RENEW_INTERVAL_MS` 39초 | `a_renewal_chance_always_arrives...` |
| DynamoDB Local 중지 + `REQUIRE=1` | `it_store` 전체 |

## 남은 격차

**수집 루프 자체는 아직 배선되지 않았다** (M2-6). 탐색이 등록부를 채우지만
`detect_tick()` 을 부르는 곳이 없다 — 대상 접속에 `AuthTokenProvider`(IAM DB
Auth)가 먼저 필요하다. R6-24(`is_collectible()` 호출부 없음)가 그 게이트다.

이번 라운드의 결과로 보아, **M2-6 을 배선한 뒤 7차 라운드가 필요하다.**
5라운드가 못 본 것을 배선 한 번이 4건 드러냈고, 이번에도 같은 일이 일어났다.

---

# 7차 라운드 — 수집 루프 (2026-08-19)

M2-6 배선 직후. **두 리뷰어가 독립적으로 같은 CRITICAL 두 개를 찾았다** — 그게
신호였다. 둘 다 로컬에서는 절대 드러나지 않는 결함이다.

## CRITICAL

| # | 결함 | 왜 로컬에서 안 보였나 | 상태 |
|---|---|---|---|
| R7-1 | `secure_auth(false)` 는 **틀린 손잡이** — IAM 인증이 한 커넥션도 성립 못 한다 | 로컬은 `caching_sha2_password` 라 이 분기를 안 탄다 | ✅ |
| R7-2 | presign 이 쿼리 값을 **퍼센트 인코딩하지 않는다** | 골든 벡터가 장기 자격증명(세션 토큰 없음)이었다 | ✅ |
| R7-3 | 셧다운에 `drain()` 이 **도달하지 못한다** — `in_flight` 유령이 남는다 | 배포마다 쌓이지만 에러가 아니다 | ✅ |

### R7-1 — 이름이 오해를 부르는 손잡이

`mysql_async` 의 `secure_auth` 는 **"`mysql_old_password` 를 막는가"** 이고 기본값이
`true` 다. `mysql_clear_password`(IAM 토큰이 요구하는 것)를 켜는 것은
`enable_cleartext_plugin` 이고 기본값이 `false` 다.

둘을 혼동해서 **두 가지를 동시에** 만들었다:

1. `enable_cleartext_plugin` 이 꺼진 채라 서버의 auth switch 에서
   `CleartextPluginDisabled` 로 즉시 끊긴다 — **프로덕션 인증 경로 전체가 죽어 있었다.**
2. `secure_auth(false)` 는 얻는 것 없이 pre-4.1 스크램블 **다운그레이드만** 열었다.

주석은 "IAM 토큰은 서버가 `mysql_clear_password` 를 요구한다" 고 정확히 적었는데
코드는 다른 일을 했다. **주석이 맞고 코드가 틀린 경우**다.

고칠 때 `enable_cleartext_plugin(true)` 를 `TlsMode::RdsCa` **팔 안에** 넣었다.
"평문 비밀번호는 검증된 TLS 위에서만" 을 주석이 아니라 **위치로** 강제한다 —
주석은 지켜지지 않고 위치는 지켜진다.

### R7-2 — 오라클이 좁아서 통과했다

`aws-sigv4` 의 `instructions.params()` 는 **디코딩된 원문**을 준다. 서명은 퍼센트
인코딩된 canonical query 위에서 계산됐다. 원문을 그대로 이어 붙이면 토큰이 자기
서명과 정합하지 않는다.

`X-Amz-Security-Token` 은 base64 라 `+`·`/`·`=` 를 담고, **ECS 태스크 롤은 항상
임시 자격증명**이다. 즉 프로덕션의 정상 경로가 전부 깨져 있었다.

**왜 "AWS CLI 와 바이트 단위로 일치" 라는 앞선 주장이 이걸 못 잡았나:**

- 골든 벡터와 스크립트가 **장기 자격증명**(세션 토큰 없음)을 썼다
- 스크립트가 `X-Amz-Signature` **16진수만** 비교했다

`X-Amz-Credential` 의 `/` 는 왕복해도 값이 같아 살아남았고, 서명 자체는 canonical
form 위에서 계산되므로 **양쪽 다 맞았다.** 다른 것은 방출 문자열이었다.

실측으로 확인했다:

```text
우리 X-Amz-Security-Token=FwoGZXIvYXdzEBYaDG+abc/def=ghi+jkl/mno=
CLI  X-Amz-Security-Token=FwoGZXIvYXdzEBYaDG%2Babc%2Fdef%3Dghi%2Bjkl%2Fmno%3D
```

오라클을 고쳤다: **토큰 문자열 전체**를 비교하고(순서는 정규화 — 계약이 아니다),
**세션 토큰 케이스를 반드시 포함**한다. 골든 벡터도 임시 자격증명용을 하나 더 뒀다.

교훈: **오라클의 범위가 곧 보증의 범위다.** "외부 구현과 대조했다" 는 말은
*무엇을* 대조했는지까지 말해야 참이다.

### R7-3 — 고친 경로에 abort 가 먼저 도달했다

각 태스크가 루프 top 에서 셧다운을 관측하면 `drain()` 으로 진행 중 레코드를
확정하도록 만들었다. 그런데 리더 루프가 **먼저 `abort_all()`** 을 불러서 그 경로에
도달하지 못했다. 실제 프로세스로 확인했다 — 종료 후에도 `in_flight` 가 1개 남았다.

`drain_all(budget)` 로 정리할 시간을 주고, 예산을 넘긴 태스크만 abort 한다.
예산은 셧다운 유예의 1/3 이다 — 크면 상위 단계가 먼저 타임아웃해 리스가 반납되지
않는다(6차에서 이미 한 번 만든 결함이다).

검증:

```text
"종료 — 진행 중 레코드를 정리했다" finalized=1
"수집 태스크 정리 완료" count=1 aborted=0
"수집 리더 리스 반납"
종료 후 in_flight: 0
남은 레코드: state=abandoned  reason=shutdown  duration_source=polled
```

마지막 줄이 옳다 — 끝나지 않은 쿼리에 **완료 시각을 만들어 붙이지 않고**
`abandoned` + 사유로 남긴다.

## HIGH — 태스크 수명

| # | 결함 | 결과 |
|---|---|---|
| R7-4 | 리더를 되찾아도 **최대 5분간 태스크 0개** | `/readyz` 는 리더라고 보고한다 |
| R7-5 | `replace_db` 가 풀을 `disconnect()` 없이 버린다 | **감시 대상 DB** 의 `Aborted_clients` 증가 |
| R7-6 | 토큰 갱신 실패에 백오프가 없다 | 500대면 초당 500회 STS/IMDS 호출 |
| R7-7 | 신선도가 전역 값 하나 | 정상인 1대가 **나머지 499대의 실패를 가린다** |
| R7-8 | 저장소 오류 중 만료된 리스로 리더라고 믿는다 | 새 리더와 중복 수집 |

R7-5 가 특히 아팠다. 정상 갱신 경로(10분마다)에서 발생하고, 피해가 **우리 쪽이
아니라 감시 대상 프로덕션 DB** 다 — "대상에 부하를 주지 않는다" 는 주장과 정면으로
부딪친다. `replace_db` 가 이전 연결을 `#[must_use]` 로 돌려주게 해서 호출부가
반드시 정리하게 만들었다.

R7-8 은 펜싱 토큰 없이 닫을 수 있는 부분이었다. `is_leader()` 가 **로컬 시계로**
만료를 확인한다 — 저장소에 못 물어봐도 시계는 안다. GC 정지 케이스는 여전히
ADR-018 의 몫이다.

## 그 외 고친 것

- CA 번들에 **다이제스트 핀**. 만료일·개수 하한은 **CA 를 덧붙이는 공격을
  통과시킨다**(삭제가 아니라 추가가 공격이다). 실제로 자기 서명 CA 를 한 장 붙여
  확인했다 — 다이제스트 테스트만 잡았고 만료 스크립트는 "인증서 109개" 라고
  적으면서 통과했다.
- `ca_bundle_health` 에 호출부가 없었다(이 부류 9번째). 기동 로그에 배선했다.
- **`Opts` 의 `Debug` 는 비밀번호를 평문으로 담는다.** 지금 유출은 없지만
  `derive(Debug)` 한 줄이면 IAM 토큰이 CloudWatch 로 간다. 그 사실을 테스트로
  고정하고(직관과 반대라서), `Pool` 을 품은 타입의 `Debug` 파생을 정적 검사로 막았다.
- **`scrub()` 은 자격증명을 마스킹하지 않는다.** SQL 리터럴 전용이다. 인증 오류를
  `Scrubbed` 로 감싸는 것이 토큰을 보호한다고 **오해하고 있었다** — 문서에 명시했다.
- `monitor_db_user` 에 예약문자 검증. IAM 토큰의 **서명 대상 URI 에 보간**되므로
  `#` 하나로 `DBUser` 가 서명에서 빠진다.
- 비-dev 배포에 `DBMON_TARGET_PASSWORD` 가 남아 있으면 **기동을 거부**한다.
  조용히 무시하면 아무도 로테이션하지 않는다.
- **엔드포인트 형태 검증** 2중. RDS CA 전용 신뢰는 "Amazon RDS CA 가 서명한 아무
  호스트" 까지만 좁힌다 — **다른 AWS 고객의 인스턴스도 그 CA 로 서명돼 있다.**
- 공허한 테스트 4건 교체. 재주입으로 전부 확인:
  `NEVER_MS = i64::MAX` 로 되돌리면 오버플로 패닉, TTL 의 `* 1000` 을 빼면 실패,
  인코딩을 되돌리면 3건 실패, `ca_bundle_health` 의 fail-closed 팔을 바꾸면 실패.
- `reap_finished` 가 패닉을 구분해 남긴다. 기본 패닉 훅은 stderr 라 JSON 로그
  스트림에 아예 나타나지 않았다.
- `check-ca-bundle.sh` 의 조용한 통과 두 곳(openssl 부재, 부분 파싱 실패)을 막았다.
- 문서/코드 환경변수 이름 불일치(`DBMON_TARGET_AUTH` vs `DBMON_TARGET_PASSWORD`).

## 이번 라운드의 교훈

**1. 두 리뷰어가 독립적으로 같은 것을 찾으면 그건 신호다.** R7-1, R7-2 를 둘 다
1·2순위로 올렸다. 서로 다른 프롬프트·서로 다른 관점에서 같은 결론에 도달한 것이
"이건 진짜다" 의 가장 값싼 증거였다.

**2. 오라클의 범위를 정확히 말해야 한다.** "AWS CLI 와 일치함을 확인했다" 는
문장이 참이면서 오해를 만들었다 — 일치를 확인한 것은 **서명 16진수**였고
**토큰 문자열**이 아니었다. 앞으로 이런 주장에는 무엇을 비교했는지 함께 적는다.

**3. 로컬에서 도는 것이 프로덕션 경로라는 보장은 없다.** dev 폴백(고정 비밀번호)이
IAM 경로를 완전히 우회하므로, 로컬 검증이 성공할수록 R7-1 이 가려졌다. 로컬 우선
개발의 대가이고, 그 대가는 "프로덕션만 타는 분기" 를 따로 표시하는 것으로 갚는다.

**4. 고친 경로에 다른 경로가 먼저 도달할 수 있다.** R7-3 은 코드가 맞았는데
호출 순서가 그것을 무력화했다. "호출부가 없다" 의 변종 — **호출부가 있지만
다른 것이 먼저 온다.**

---

# 8차 라운드 — 백필·스윕·펜싱 (2026-08-19)

두 리뷰어가 **또 독립적으로 같은 CRITICAL** 을 1순위로 올렸다(헤더 위조). 그리고
한쪽이 이 세션에서 가장 중요한 것을 찾았다 — **F5 병합이 절반의 확률로 일어나지
않는다**는 것.

## CRITICAL

### R8-1 — SQL 본문이 로그 헤더를 위조할 수 있다 (실증)

파서가 블록의 **모든 줄**에 헤더 접두를 다시 판정했고 매칭마다 덮어썼다. SQL 은
여러 줄일 수 있고 MySQL 은 문장을 개행 그대로 기록한다. 그래서 **대상 DB 에 쿼리를
던질 수 있는 아무 계정이** 헤더를 주입할 수 있었다. 실제로 돌려 확인했다:

```text
입력:  SELECT SLEEP(3) /*
       # User@Host: 4111-1111-1111-1111[x] @ evil []  Id: 999
       # Query_time: 120.0  Rows_examined: 999999999
       */
결과:  thread_id=999  db_user="4111-1111-1111-1111"  duration=120000ms
       rows_examined=999999999
```

세 가지가 동시에 깨졌다:

1. **리터럴 정책 우회.** `db_user`·`db_host`·`schema_name` 은 마스킹 대상이 아니다 —
   정책이 `masked` 여도 임의 문자열이 저장된다.
2. **남의 레코드 오염.** `thread_id` + 시작 초를 정할 수 있으니 임의 `record_id` 를
   겨냥할 수 있고, `merge` 는 지표를 필드별 최대값으로 취하며 슬로우로그 duration 을
   권위값으로 본다 → 999,999,999행이 영구히 박힌다.
3. **유령 엔트리.** 본문의 `# Time:` 이 블록을 쪼갠다.

**고침 — 측정에 기반한 상태 기계:**

| 규칙 | 근거 (실측) |
|---|---|
| `SET timestamp=` 뒤는 전부 본문 | 실제 로그 1,983 엔트리 **전부**가 이 줄을 갖고, 전부 그 직후가 SQL |
| `# Time:` 은 문장이 `;` 로 끝났을 때만 경계 | 1,983 엔트리 **전부**가 `;` 로 끝난다 (100%) |
| 헤더는 첫 등장만 채택 | 심층 방어 |
| 메타 필드 문자셋·길이 검증 | 정책이 `sql_text` 에만 걸리므로 |

두 규칙 모두 **통념이 아니라 측정**에서 나왔다. 부수 효과로 `#` 주석(MySQL 의 주석
문자)이 SQL 에 살아남는다 — 전에는 헤더로 먹혀 **훼손된 텍스트로 다이제스트가
계산되어 실시간 경로와 영구히 다른 유령 다이제스트**가 만들어졌다.

### R8-2 — F5 병합이 약 50% 확률로 일어나지 않았다

두 경로의 시작 시각 추정 정밀도가 다르다:

| 경로 | 시작 시각 | 정밀도 |
|---|---|---|
| 실시간 | `now − PROCESSLIST.TIME × 1000` | **정수 초** |
| 슬로우로그 | `# Time − Query_time` | 밀리초 |

`record_id` 는 시작 시각을 **초 버킷**으로 접는다. 두 추정이 초 경계를 사이에 두면
버킷이 갈리고 — 시작 시각의 밀리초가 균등분포면 **약 50%** — 한 실행이 레코드
2건이 된다. 한쪽은 정확 지표만, 다른 쪽은 플랜만. **F5 의 목적 자체가 달성되지
않는다.**

설계된 해법은 있었다: `find_merge_candidate`(±2초 보조 조회, 05 §8.2)에 doc 이
정확히 이 문제를 적어 뒀다. **프로덕션 호출부가 0개였다.**

그런데 **모든 테스트가 통과했다** — 이유가 둘이다:

1. **페이크와 실제의 계약이 갈려 있었다.** `FakeSlowQueryStore::upsert_merged` 에는
   ±2초 폴백이 있고 `DynamoSlowQueryStore` 에는 없었다.
2. 내가 쓴 테스트가 공허했다. "실시간" 키를 슬로우로그 값으로 만들어 비교해서
   `RecordId::new(x) == RecordId::new(x)` 를 확인했다.

그리고 **앞서 "F5 병합 확인" 이라고 보고한 e2e 관측은 버킷이 우연히 맞은 경우였다.**

고치는 과정에서 두 번째 결함이 드러났다: `merge` 는 `started_at_ms` 를 **더 이른
쪽**으로 정하므로 병합이 시작 시각을 앞당기면 `SK` 가 이동하고, 조건부 쓰기가
가리키는 자리에 항목이 없어 **낙관적 잠금 5회 초과로 죽었다.** 항목은 처음 저장된
자리에 머물게 했다(정확한 시작 시각은 속성으로 남는다).

### R8-3 — 재시작한 워커가 자기 유령을 영구히 건너뛴다

`worker_id` 는 `{role}-{HOSTNAME}` 이고 docker compose·k8s StatefulSet·로컬은
**재시작해도 같다.** `judge` 는 이름만 보고 `Mine` → 무조건 제외했다. 급사한
프로세스가 남긴 `in_flight` 를 같은 이름의 새 프로세스가 영구히 건너뛴다 —
**F4 가 없애려던 유령이 정확히 그 경로로 TTL(35일)까지 남는다.**

근거는 이미 레코드에 있었다: 리스 획득은 항상 `epoch + 1` 이므로 `owner_epoch` 가
현재보다 작으면 이전 생애의 것이다. `judge` 가 그 값을 **읽지 않고 로그에만
찍고 있었다.**

### R8-4 — 예산 없는 await 가 epoch 스냅샷 불변식을 깼다

`work_budget()` 의 doc 이 규칙을 이미 적어 뒀다 — "리더 루프와 같은 태스크에서 도는
작업이 길어지면 리스가 만료되고 두 리더가 생긴다." 탐색 블록은 지켰다. **새로 넣은
스윕·백필 블록은 지키지 않았다.** 백필은 인스턴스 N개를 순차로 돌고 각 호출에
타임아웃이 없다.

그러면 "태스크는 자기 epoch 보다 오래 살 수 없다" 가 성립하지 않는다 —
`abort_all()` 은 루프가 그 블록에서 **돌아온 뒤에야** 실행된다. `run_in_budget()`
으로 감쌌다.

## HIGH

| # | 결함 | 결과 |
|---|---|---|
| R8-5 | CloudWatch 클라이언트가 홈 리전 고정 | 타 리전 결측 + **같은 이름이면 다른 DB 로그를 엉뚱한 인스턴스로 저장** |
| R8-6 | 주기 필드 검증 없음 | `backfill_secs=0` → 창이 비어 아무것도 안 하면서 초당 500회 API |
| R8-7 | `has_more`·fetch 실패를 아무도 안 봄 | "못 읽었다" 와 "읽을 게 없었다" 가 구분되지 않는다 |
| R8-8 | `Full` 정책 길이 상한 없음 | 400KB 문장 하나가 **체크포인트를 영구 정지** |
| R8-9 | `FileFetcher` 가 FIFO 를 읽으면 영구 블록 | 리더 루프 전체(스윕·탐색·백필)가 멈춘다 |

R8-5 가 특히 나쁘다. 로그 그룹 이름에 리전이 없으므로(`/aws/rds/instance/<id>/slowquery`)
리전은 클라이언트가 정한다. DR·미러 네이밍에서 흔한 "같은 식별자" 조합이면
**원문 리터럴을 포함한 다른 DB 의 로그를 엉뚱한 `instance_id` 로 저장**하고,
prd↔dev 경계를 넘을 수 있다. 리전별 클라이언트 + 불일치 fail-closed 로 고쳤다.

## 그 외

- **마스킹 실패 시 엔트리를 버리는 것을 그만뒀다.** 렉서가 못 다루는 문장 = 가장
  조사할 필요가 큰 문장이고, 그 `rows_examined` 가 영구히 없어졌다.
  텍스트만 포기하고 레코드는 남긴다(`masking_degraded` 로 따로 센다 — `unnormalizable`
  과 섞으면 마스킹 회귀가 파서 문제로 오인된다).
- `sql_text_truncated: false` 하드코딩. `canonical` 은 8,192자에서 잘리므로 **잘린
  SQL 을 전문으로 표시**하고 있었다.
- `Rows_affected` 가 `Query_time` 과 같은 줄인 벤더 형식(Percona·Aurora)에서 누락.
- `SkipReason::BadTimestamp` 가 로그 원문을 담고 있었다 — R8-1 로 공격자 통제
  하에 있는 값이다. **타입이 들 수 없게** 바꿨다.
- prd IAM 이 `/aws/rds/*` 로 열려 있었다. **audit 로그는 모든 문장을 담으므로**
  노출 범위가 전혀 다르다. `/aws/rds/instance/*/slowquery` 로 좁히고, 쓰지 않는
  `DescribeLogStreams` 를 제거했다. 코드와 IAM 의 일치를 정적 검사로 고정했다.
- 공허한 테스트 4건 교체(전부 재주입 확인).

## 이번 라운드의 교훈

**1. 페이크가 실제보다 관대하면 테스트는 거짓을 증명한다.** R8-2 의 핵심은
"호출부가 없다" 가 아니라 **"페이크에는 있고 실제에는 없다"** 였다. 포트를 두 번
구현하면 계약이 갈리고, 갈린 쪽이 관대하면 그쪽만 초록이 된다. 앞으로 포트
구현이 둘이면 **같은 시나리오를 실제 저장소로도** 돌린다.

**2. 내 e2e 관측이 우연이었다.** `capture_source: merged` 를 보고 "F5 확인" 이라고
보고했는데, 그건 버킷이 맞은 한 사례였다. **한 번의 성공 관측은 확률적 결함의
반증이 아니다.** 경계를 의도적으로 만드는 테스트가 필요했고, 그게 없어서 놓쳤다.

**3. 통념 대신 측정이 규칙을 줬다.** R8-1 의 두 규칙(`SET timestamp` 표지, `;` 종료)은
"슬로우로그 형식은 이럴 것이다" 가 아니라 실제 로그 1,983 엔트리 전수 확인에서
나왔다. 파서를 손으로 쓸 때 이 프로젝트가 다섯 번 데인 이유가 통념이었다.

**4. 새 블록은 기존 불변식을 상속하지 않는다.** R8-4 는 `work_budget()` 의 doc 이
이미 규칙을 설명하고 있는데도 새로 넣은 두 블록이 그걸 지키지 않은 경우다.
불변식이 **문서에 있는 것과 모든 경로에 적용되는 것은 다르다.**
