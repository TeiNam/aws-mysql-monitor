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

배선까지 테스트로 고정했다 — `needs_warm` 표시를 지우면 실패한다.
2차의 M2("`connect_with_limit` 호출부가 없어서 수정이 무효였다")와 같은 실수를 막는다.

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
