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
