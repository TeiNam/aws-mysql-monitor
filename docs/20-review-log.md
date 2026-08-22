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

---

# 8차와 9차 사이 — 구현 중 발견 (2026-08-20)

리뷰가 아니라 **조회 API·WS 를 만들면서** 나온 것들. `21-resume.md` 가 임시로
들고 있던 기록을 여기로 옮긴다.

| # | 결함 | 발견 | 처리 |
|---|---|---|---|
| M6-1 | `Cursor` 페이로드를 `sub\|filters\|position\|exp` 로 만들고 주석에 "`\|` 는 나타날 수 없다" 고 적었다. **DynamoDB 키 직렬화가 `PK\|SK` 라서 항상 나타났다** — 정당한 커서 전부가 `Malformed` 로 거부돼 페이지네이션이 첫 페이지에서 멈춘다 | 테스트 | ✅ `position` 을 맨 뒤로 옮기고 `splitn(4, '\|')` + 회귀 테스트 |
| M6-2 | `caching_sha2_password` 전체 인증은 비밀을 서버 공개키로 RSA 암호화한다. IAM DB Auth 토큰은 ~1000바이트라 RSA-2048 블록에 안 들어가고 `mysql_common` 이 `assert!` 로 **패닉**한다(tokio 워커가 죽는다) | 실행 | ✅ `plaintext_secret_is_sendable(len)` 가드로 패닉을 오류로 바꿨다. 메시지가 `DBMON_TARGET_PASSWORD` 를 알려 준다 |
| M6-3 | dev 폴백 코드는 있는데 `DBMON_TARGET_PASSWORD` 가 justfile·`local/dbmon.toml`·docker-compose **어디에도 배선돼 있지 않았다** — 문서대로 따라 하면 M6-2 의 패닉을 만난다 | 실행 | ✅ 세 곳에 넣고 이유를 주석으로 |
| M6-4 | `MetricsSampler.last_sampled_at_ms: EpochMs = 0` 이 "아직 샘플링 안 함" 을 의미했는데 **`0` 은 유효한 `EpochMs`** 다 | 테스트(`now_ms = 0`) | ✅ `Option<EpochMs>` |
| M6-5 | 임베드 화면이 prd 에서도 서빙됐다. 정적 HTML 이라 데이터는 안 새지만 T-01 의 인증 예외가 하나 늘고, prd 에서는 인증을 통과할 수 없으니 **깨진 화면**이다 | 코드 | ✅ `policy.serves_local_ui()` 일 때만 라우트를 붙인다 (prd 404) |

## 진단을 두 번 틀리게 만든 하네스 문제 (결함 아님, 기록용)

제품 결함이 아니라 **테스트 방법**이 틀려서 없는 결함을 쫓았다.

- **로그 필터 환경변수가 `DBMON_LOG` 다**(`telemetry.rs`). `RUST_LOG` 로 주면
  조용히 무시된다 → "수집 tick 로그가 없다 → 수집기가 안 돈다" 로 오진했다.
  실제로는 1초마다 정상 tick 중이었다.
- **`docker exec` 에 `-d` 가 없으면** 셸이 쿼리 완료를 기다려 WS 관측 창과
  겹치지 않는다 → "느린 쿼리를 쐈는데 방송이 안 온다" 의 진짜 이유였다.

앞서 `kill` 이 래퍼 서브셸을 때린 일, 포트를 쥔 좀비 프로세스, 열려 있는
슬로우로그 파일을 `rm` 한 일과 같은 계열이다. **"관측되지 않음" 을 "동작하지
않음" 으로 읽기 전에 관측 경로를 먼저 의심한다.**

---

# 9차 라운드 — React SPA·조회 API·WS (2026-08-20)

리뷰어는 **Claude(이 세션)와 OpenAI Codex** 두 축. 대상은 M0-12 프론트(`web/`)와
그것이 의존하는 M6 경로(조회 API·WS·지표 방송), 그리고 정적 서빙·이미지 빌드.

네 라운드를 돌렸다. **양쪽이 독립적으로 같은 결함 5건에 도달했고**(1라운드),
Codex 는 **내 수정이 만든 새 결함 2건**을 잡았다(3·5라운드). 결함 2건은 실측으로
반박했다.

## CRITICAL 없음 / HIGH 2건

### R9-1 — WS 유휴 종료가 동작하지 않았다 (실측) ✅

`select!` 안에 `timeout(IDLE_TIMEOUT, socket.recv())` 을 두면 **다른 분기가 이길
때마다 타임아웃 future 가 새로 만들어진다.** 5초마다 오는 `status` 방송이 (구독하지
않은 인스턴스의 것까지) 그 분기를 깨우므로 기한이 영원히 밀렸다.

```text
수정 전: 인증만 하고 75초 침묵 → readyState=1 (열려 있음)
수정 후: 서버가 닫았다: 60.0초
```

인증된 소켓을 무기한 붙잡을 수 있으므로 자원 고갈 경로다. `sleep_until(절대 기한)`
으로 바꿨고, 기한은 **클라이언트에서 무엇이든 받을 때만** 밀린다. 회귀 방어는 이
프로젝트의 관용대로 소스 스캔 테스트(`the_idle_timeout_uses_an_absolute_deadline`).

> 이 결함은 **프론트가 없었으면 발견되지 않았다.** 화면이 5초 주기 방송을 받기
> 시작하면서 "왜 유휴 종료가 안 걸리나" 를 실측할 이유가 생겼다.

### R9-2 — 인증이 거부된 뒤에도 표에 SQL 이 남아 있었다 (실측) ✅

1라운드에서 `queryClient.clear()` 를 넣었는데, 그건 **캐시에서만 지운다** — 이미
마운트된 화면은 마지막 결과를 계속 그린다. 브라우저에서 토큰을 무효화해 확인:

```text
clear():        배지 "인증 실패" + 34행의 SQL 이 그대로 남아 있다   ← 결함
resetQueries(): 0행, DOM 에 SELECT 없음, 토큰 안내로 전환
```

백엔드는 T-33 으로 5분마다 재인증하고 실패하면 fail closed 인데, **화면만 열려
있으면 강등된 사용자가 계속 본다.** `resetQueries()` + 스냅샷 비우기로 고쳤고,
DOM 통합 테스트를 추가했다(`web/src/app.test.tsx`) — 단위 테스트로는 잡히지
않는다(스냅샷과 캐시는 이미 비어 있었다).

## 양쪽이 독립적으로 지적한 것 (전부 ✅)

| # | 결함 | 처리 |
|---|---|---|
| R9-3 | WS `errorCode` 를 저장만 하고 **아무 데도 보여 주지 않았다.** `malformed`·`auth_timeout` 은 프론트·백엔드 불일치 신호인데 화면은 "데이터가 안 온다" 로만 보였다 | 헤더 배너 |
| R9-4 | 플릿 합계 "실행 중 스레드" 가 표본이 없을 때 **`0`** 이었다 — 옆 칸은 `—` 인데 합계는 0 인 모순 | 표본 없으면 `null` → `—` |
| R9-5 | 유실 신호를 `laggedAt: number \| null` 로 뒀다 — 같은 밀리초에 두 번 밀리면 재조회가 한 번 빠진다 | 카운터 |
| R9-6 | API 응답에 캐시 정책이 없다 | `cache: "no-store"` |
| R9-7 | `GET /api` 가 `/api/{*rest}` 에 안 걸려 **index.html 을 200 으로** 줬다 | 명시 라우트 |

## Codex 만 지적 (전부 확인 후 ✅)

| # | 결함 | 처리 |
|---|---|---|
| R9-8 | 재접속 백오프를 `open` 에서 초기화했다. 업그레이드 직후 서버가 끊는 경우 **최소 지연으로 무한 재접속**한다 | `ready` 에서 초기화 + 회귀 테스트 |
| R9-9 | 끊긴 구간을 QPS 이력에 남기지 않아 10분 뒤 첫 표본이 옛 표본과 이어졌다 — **관측이 없던 구간에 선을 그린다** | `markStreamGap()`, 브라우저에서 구간 2개 확인 |
| R9-10 | `ready` 프레임에 `env_scope` 가 없으면 헤더가 `undefined.join()` 으로 죽어 **화면 전체가 하얘진다** | 화면이 읽는 필드 전수 검증 + `ErrorBoundary` |
| R9-11 | 지표·방송 프레임의 숫자가 아닌 값이 `NaN` 으로 차트에 들어갔다. `started_at_ms` 가 없으면 `Intl` 이 **조용히 현재 시각**을 그린다 | `normalizeMetrics`·`normalizeBroadcast` |
| R9-12 | 못 읽은 프레임을 조용히 버려서 `ready` 실패 시 **"연결 중" 에서 영구히 멈춘다**(하트비트가 나가므로 서버도 안 끊는다) | 배너 + 인증 전이면 소켓 종료 |
| R9-13 | 끊긴 동안의 **슬로우 쿼리 방송은 영구히 유실**되는데 아무 표시가 없었다 | `missedCount` → 목록 재조회 + 실시간 화면 구멍 표시 |
| R9-14 | `denied` 를 매 응답으로 덮어써 **뒤에 온 성공 응답이 거부 경고를 지웠다** | 연결 동안 누적, `ready` 에서 초기화 |
| R9-15 | 200 인데 본문이 JSON 이 아니면 화면이 "서버에 닿지 못했다" 고 말했다 — 닿았는데 | `malformed_response` |
| R9-16 | SPA 셸에 캐시 정책이 없었다. 낡은 셸이 캐시되면 **삭제된 자산 해시를 가리켜 빈 화면**이 된다 | 셸 `no-cache`, 해시 자산 `immutable`(성공 응답에만) |
| R9-17 | 목록이 24시간 구간인데 시각만 찍어 **자정을 넘는 행이 오늘 것으로 읽혔다** | 오늘이 아니면 날짜까지 |
| R9-18 | 스파크라인이 평평한 계열을 **바닥에** 그렸다(주석엔 "가운데"). `[1, null, 2]` 는 빈 `<svg>` 를 냈다 | 가운데 + 값 없음 표시 |
| R9-19 | `status` 밀림을 서버가 조용히 버리므로 사라진 표본이 이력에 흔적을 남기지 않았다 | 클라이언트가 `at_ms` 간격으로 판정 |
| R9-20 | 놓침 복구가 **선택된 필터만** 다시 읽었다 | 목록 키 전체 무효화 |
| R9-21 | 구독을 거둔 토픽이 거부 배너에 재접속까지 남았다 | `forgetDenied()` |

## Claude 만 지적 (전부 ✅)

- 헤더가 실시간 스냅샷을 구독해 **5초마다 화면 전체가 리렌더**됐다 → 잎
  컴포넌트로 내리고, 목록은 원시값 하나만 본다
- "다시 연결" 버튼이 소켓이 닫히는 중이면 **아무 일도 하지 않았다**
- `sticky` 헤더가 동작하지 않았다 — `overflow-x-auto` 는 세로축을 `auto` 로
  계산하므로 높이 제한 없이는 붙을 데가 없다
- `focus:outline-none` 만 두고 테두리 색만 바꿨다 → 키보드 초점이 사라진다
- `text-zinc-500` 은 `zinc-950` 에서 대비 4.1:1 (기준 미달)
- `max-w truncate` 를 `td` 에 직접 줬다 — 표 자동 레이아웃이 무시할 수 있다
- 404 에 `immutable` 이 붙었다 → 배포 중 404 를 1년 캐시하면 영구히 깨진다
- 토큰 안내가 인증 방식을 **추측했다**(`/api/auth/config` 를 읽는 곳이 없어
  `fetchAuthConfig` 가 죽은 코드였다). 로그의 접속 URL 이 **컨테이너 안 포트**를
  쓴다는 사실도 안내에 없었다
- `stream_lagged` 를 slowq 구독자가 아닌 클라이언트에게도 보냈다(서버)

## 4·5라운드 (수렴)

라운드를 돌릴수록 새 지적이 줄었다: **HIGH 2 → HIGH 1 → HIGH 0**. 4라운드에서
Codex 가 지적한 6건 중 2건은 이미 내 HEAD 에서 고쳐져 있었다(Codex 도 그렇게 적었다).

| # | 결함 | 처리 |
|---|---|---|
| R9-22 | **놓침을 끊긴 시점에 셌다.** 목록이 그 즉시 HTTP 재조회를 하는데 그 조회는 서버가 죽은 동안 나가서 실패하고, 재접속에는 신호가 없어 목록이 낡은 채로 남는다 | ✅ `gapPending` → `ready` 에서 센다. 실측: 8초 정지 후 오류 배너 없이 갱신 |
| R9-23 | WS 가 `unauthorized` 를 받아도 **토큰을 버리지 않았다** — HTTP 조회가 없는 화면에서는 아무도 지우지 않아 새로고침 때 죽은 토큰을 또 보낸다 | ✅ |
| R9-24 | `forgetDenied()` 가 끊긴 동안 닿지 않았다. 또 구독 요청과 해제가 겹치면 **늦게 온 거부 응답이 지워진 경고를 되살렸다** | ✅ 해제 경로에서 즉시 + 응답 수신 시 요구 없는 거부를 정리 |
| R9-25 | `/api/` (트레일링 슬래시)가 `{*rest}` 에 안 걸려 **HTML 200** 이었다 | ✅ 실측 후 라우트 추가 |
| R9-26 | 플릿 합계가 지표 없는 인스턴스를 조용히 제외하면서 이름은 "합계" 였다(구독 상한 50 초과 플릿에서는 영구히 일부만) | ✅ `N/M 인스턴스 기준` 을 함께 적는다 |
| R9-27 | vitest `globals: false` 에서는 RTL 자동 정리가 붙지 않아 앞 테스트의 DOM 이 남는다 | ✅ 명시 `cleanup()` |
| R9-28 | 컨테이너가 계속 `unhealthy` 였다(이전 세션의 "원인 미확인"). `command:` 의 `--config` 는 엔트리포인트에만 붙고 **HEALTHCHECK 는 그 인자를 못 받는다** — 설정을 환경변수만으로 읽어 `aws.account_id` 검증에서 실패했다 | ✅ 컴포즈에 `DBMON_CONFIG` 추가. `Up (healthy)` 확인 |

## ❌ 기각

| 주장 | 근거 |
|---|---|
| "잠금 파일이 없어 이미지가 빌드되지 않는다"(HIGH) | 리뷰 신호를 높이려고 **diff 에서만** 뺐고 둘 다 커밋돼 있다. `docker compose --profile monitor build` 통과 + 컨테이너가 `spa=/app/web/dist` 로 SPA 서빙을 확인했다 |
| "`missedCount` 를 slowq 구독으로 한정해야 한다" | 실시간 화면을 떠난 동안 생긴 구멍도 **표에는 구멍으로 남는다.** 거짓 경고는 싸고, 조용한 구멍은 이 프로젝트가 아홉 번 당한 실패다 |

## ⏳ 남김

- `Lagged` 는 **과거의** 수신 지연인데 구독 판정은 현재 집합으로 한다. 밀리초 창에서
  거짓 경고나 누락이 가능하다. 정확히 하려면 허브가 토픽별 지연 정보를 실어야 하고,
  그건 이 배관보다 크다. 현재 동작은 "구독 중이면 알린다" 로 보수적인 쪽이다
- `status`/`qpsHistory` 는 구독 해제 후에도 남는다 — 상한이 50토픽 × 60표본이고
  `갱신` 열에 표본 시각이 보여 오해 소지가 없다
- HTTP 401 만 나고 WS 는 살아 있는 창(최대 5분)에는 캐시가 남는다 — WS 재인증이
  실패하는 순간 비워진다
- HTTP 응답의 필드별 검증은 하지 않는다(백엔드 뷰 타입이 계약이고, 렌더 예외는
  `ErrorBoundary` 가 받는다). WS 프레임은 방송이라 검증한다

## 이번 라운드의 교훈

**1. 리뷰가 만든 수정이 새 결함을 만든다.** R9-10(프레임 검증 강화)이 R9-12(영구
"연결 중")를, R9-9(이력 구멍)가 R9-13(유실 표시 없음)을 만들었다. **수정 뒤에
같은 diff 를 다시 리뷰에 넣는 것**이 이번에 두 번 값을 했다.

**2. 테스트가 통과하는데 동작이 틀릴 수 있다.** 처음 쓴 "ready 전에는 구독하지
않는다" 테스트는 **게이트를 지워도 통과했다** — 검사 지점이 실제 경로가 아니었다.
변이 테스트(수정을 되돌려 테스트가 깨지는지 확인)로 두 건을 잡았다. 앞으로 방어
코드에 테스트를 붙일 때는 **그 방어를 지워 보고** 깨지는지 확인한다.

**3. `null` 을 `0` 으로 만드는 실수는 화면에서 특히 싸게 일어난다.** `?? 0` 한 번,
`let mut sum = 0` 한 번으로 "관측 없음" 이 "값이 0" 이 됐다. 백엔드가 `Option` 으로
구분해 보낸 것을 화면이 뭉갠 것이고, 이 프로젝트가 백엔드에서 다섯 번 고친 것과
같은 실패다(4.5 `0` 센티널).

**4. 프론트가 백엔드 결함을 드러냈다.** R9-1(유휴 종료)은 화면이 실제로 5초 주기
방송을 받기 시작해서 실측할 이유가 생긴 것이고, R9-19(status 밀림)는 게이지가
**이력**을 갖게 되면서 "조용히 버린다" 는 기존 결정이 부분적으로 틀리게 된 경우다.
**소비자가 생기면 예전 결정을 다시 봐야 한다.**

---

# 참조 이식 (2026-08-21) — 부모 폴더의 두 구현을 Rust + React 로

## 왜 이 절이 있는가

한 세션이 **부모 폴더의 기존 구현을 보지 않고** 프론트를 만들었다. 결과는 동작했지만
사용자가 기대한 화면과 전혀 달랐다(다크 4화면 vs 라이트 5화면). 사용자의 지적:
"부모 폴더에 백엔드와 프론트가 가지고 있는것을 rust와 react로 만들어주길 원했는데.
하나도 안지켜졌어."

**교훈: 옮기는 작업인지 새로 만드는 작업인지 먼저 확인한다.** 설계 문서(09)가 24개
라우트를 규정하고 있어도, 이미 쓰이는 구현이 있으면 **그것이 기준**이다. 문서는
목표를 적지만 사용자의 기대는 현재 쓰는 화면이 만든다.

## 참조 API 표면과 대응

| 참조 (`my_slow_query_scraper`, FastAPI) | 여기 (Rust) | 비고 |
|---|---|---|
| `GET /aws/info` | `GET /api/aws/info` | ✅ |
| `GET /mysql/queries` | `GET /api/slow-queries` | ✅ 상한 500 + `total` |
| `GET /explain/plans` | `GET /api/plans` | ✅ 플랜 있는 레코드만 |
| (플랜 본문) | `GET /api/queries/{id}/plan` | ✅ |
| `GET /mysql/explain/{pid}/markdown` | `GET /api/queries/{id}/markdown` | ✅ |
| `GET /cw-slowquery/digest/stats` | `GET /api/digests?month=` | ✅ 조회 시점 집계 |
| `GET /sql/statistics/{ym}` | `GET /api/statistics?month=` | ✅ |
| `GET /sql/statistics/users/{ym}` | `GET /api/statistics/users?month=` | ✅ |
| `GET /rds-instances` | `GET /api/instances` | ✅ 태그·클래스·엔드포인트 추가 |
| `POST /mysql/start` / `stop` / `GET /mysql/status` | `POST /api/collector/resume` / `pause` / `GET /api/collector/status` | ✅ 리스는 놓지 않는다 |
| `POST /collectors/rds-instances` | `POST /api/discovery/run` | ✅ 즉시 탐색 |
| `POST /cloudwatch/run` + `WS /ws/collection/{id}` | `POST /api/backfill/run` | ⚠ **부분** — 즉시 실행만. 임의 월 재수집·진행률은 잡 큐가 필요하다 |
| `POST /sql/statistics/calculate/{ym}` | (없음) | ❌ **불필요** — 조회 시점에 계산하므로 미리 계산할 것이 없다 |
| `POST /aws/collect` | (없음) | ⏳ AWS 정보는 설정에서 읽는다 |

## 의도적으로 다르게 한 것

| 참조 | 여기 | 이유 |
|---|---|---|
| MongoDB 에 월간 다이제스트를 **미리 쌓는다** | 저장된 실행 레코드를 **조회 시점에 접는다** | 수집 경로에 새 쓰기를 추가하지 않는다. 천장(2만 건)에 걸리면 `truncated=true` 로 말한다 |
| 수집 루프를 **켜고 끈다** | 리스는 쥔 채 **태스크만 멈춘다** | 리스를 놓으면 다른 워커가 즉시 리더가 되어 계속 수집한다 — 멈춘 게 아니다 |
| canvas 에 좌표를 **라벨 문자열로** 정해 그린다 | EXPLAIN JSON 을 재귀로 걷는 순수 함수 + SVG | 참조 판은 테이블 3개·`grouping`·`union` 에서 노드가 겹치거나 사라졌다 |
| 화면 전체를 폴링 | 폴링 + **방송이 오면 즉시 재조회** | 주기를 기다리지 않는다 |

## 브라우저 검증이 잡은 것 (전부 ✅)

| # | 결함 | 처리 |
|---|---|---|
| P-1 | **처리목록 캡처의 `rows_examined=0` 이 평균을 깎았다.** 실행 *중* 문장의 행 카운터는 얻을 수 없어 0 이 들어오는데([19 §G2]) 그걸 평균에 넣으면 "인덱스가 잘 탄다" 로 오독된다 | `capture_source == processlist` 를 분모에서 뺀다. 슬로우로그가 붙은 0 은 **진짜 0** 이므로 센다(실측: 83건 중 7건) |
| P-2 | 자리표시자 epoch(`1000`)을 `1970. 01. 01.` 로 "마지막 관측" 이라고 그렸다 | 2000년 이전은 값 없음(`—`) |
| P-3 | 월 구간 표시가 시각만 찍어 `00:00:00 ~ 00:00:00` 이었다 | 날짜까지 |
| P-4 | 화면 재구성 중 **인증 거부 시 캐시 초기화를 빠뜨렸다** — 표에 SQL 이 남았다 | 통합 테스트가 잡았다(그래서 그 테스트가 있다) |
| P-5 | `useMutation` 을 헬퍼 함수로 감싸 훅 규칙을 위반했다 | 세 번 명시 호출 |
| P-6 | `ko-KR` 날짜 문자열을 앞 10자로 잘라 "같은 날" 을 판정해 **일(日)이 빠졌다** — 어제와 오늘이 같은 날로 보였다 | 날짜 전용 포맷터 |

## 2way 리뷰 (참조 이식 diff)

Codex 가 9건. 전부 실코드로 확인했고 8건 수정, 1건은 이미 고쳐진 것을 재지적했다.

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R-1 | HIGH | **저장소가 상한에 걸리면 가장 오래된 날을 돌려줬다.** `date_parts()` 는 오래된 순인데 그 순서로 파티션을 훑었다 — 목록·플랜·집계는 전부 최근을 보는 화면이라 조용히 틀린 화면이 된다. 게다가 `LastEvaluatedKey` 를 버려서 1MB 페이지 때문에 적게 온 것을 "전부다" 로 오판했다 | ✅ 최신 파티션부터 + 페이지 추적. **변이 테스트**로 검증(`.rev()` 를 되돌리면 새 통합 테스트가 깨진다) |
| R-2 | HIGH | **목록·집계가 인스턴스의 *현재* 환경으로만 인가했다.** 환경이 바뀐 인스턴스의 과거 레코드가 스코프 밖 사용자에게 나갔다 — 그 레코드에는 당시 환경의 SQL 이 있다. 상세 조회는 이미 레코드별로 검사하고 있었다 | ✅ 두 경로 모두 레코드별 검사 |
| R-3 | HIGH | **전역 수집 스위치에 스코프 검사가 없었다.** `dev` 스코프 `operator` 가 누르면 prd 관측이 멈춘다 | ✅ 역할 + 전 환경 스코프 |
| R-4 | HIGH | **`role=api` 워커가 제어에 성공을 돌려줬다.** 플래그가 프로세스 원자값이라 수집 워커는 모르는데 화면은 "멈췄다" 고 말했다 | ✅ 409 `not_a_collector` (실측). 화면이 "권한 없음" 과 "수집기 아님" 을 구분 |
| R-5 | MEDIUM | **UTF-8 경계 패닉.** `&sql.trim_start()[..6]` 이 `/*가*/COMMIT` 에서 패닉 — 그런 레코드 하나가 다이제스트·통계 전체를 500 으로 만든다 | ✅ 문자 단위 비교 + 전수 테스트 |
| R-6 | MEDIUM | MySQL Monitor 가 `status:` 만 구독해 **"즉시 재조회" 가 거짓말**이었다 | ✅ `slowq:env=…` 도 구독(실측: 구독 거부 없음, 새 쿼리 즉시 반영) |
| R-7 | MEDIUM | 실행계획 **v2(`query_plan` 루트)** 를 못 읽어 노드 하나짜리 그래프가 그럴싸하게 비었다. 백엔드는 v2 를 지원한다 | ✅ `operation`·`inputs`·`estimated_*` 파싱 + 테스트 |
| R-8 | LOW | `paused` 와 시각이 **따로 있는 두 원자값**이라 모순 상태가 가능했다 | ✅ 하나로 합쳐 표현 불가능하게(0 = 안 멈춤) |
| R-9 | LOW | 월 기본값이 브라우저 시간대라 **서울 밖에서 다른 달이 열렸다**(백엔드는 KST 로 자른다) | ✅ KST 고정 + 테스트 |

## 2way 리뷰 2라운드

Codex 가 11건. 10건 수정, 1건은 문서화된 M6 격차(커서 미발급) 재지적.

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R2-1 | HIGH | **인스턴스 하나가 상한을 다 먹었다.** 등록부 순서로 읽고 상한에서 중단하면 뒤 인스턴스의 더 새로운 행이 통째로 빠진다 — 정렬은 그 뒤에 하므로 화면은 모른다 | ✅ 읽기 경로를 하나로 합치고(`collect_views`) 인스턴스마다 같은 몫 + 전역 최신순 |
| R2-2 | HIGH | **환경 판정이 요청 필터를 무시했다.** 레코드를 사용자 스코프 전체로 검사해 `?env=dev` 에 prd 행이 섞였다 | ✅ `사용자 스코프 ∩ 요청 env` |
| R2-3 | HIGH | **`from_ms=0` 이 인스턴스마다 1만 번 질의**하고 1970년부터 채워 최근 데이터가 빠졌다 — 인증된 사용자의 비용 공격 | ✅ 구간 상한 400일, 넘으면 `range_too_long`(실측 400) |
| R2-4 | HIGH | 집계 천장도 같은 이유로 편향됐다 | ✅ R2-1 과 같은 수정 |
| R2-5 | HIGH | **일시정지가 유령 레코드를 남겼다.** `abort_all` 후 남은 `in_flight` 는 이 워커 epoch 소유라 고아 스윕이 `Mine` 으로 건너뛴다 — TTL 까지 영구히 진행 중(F4 상태) | ✅ 3초 드레인 + 남은 것은 `collector_paused` 사유로 확정. 드레인 예산을 작게 쓰는 이유는 리스 유지(실측 `closed=1`) |
| R2-6 | HIGH | 플랜 화면이 없는 `record` 를 **다른 쿼리의 계획으로 갈아치웠다** | ✅ 그 사실을 말하고 대체하지 않는다 |
| R2-7 | MEDIUM | 플랜 목록이 500에서 잘려도 말하지 않았다 | ✅ `has_more` + 화면 표시 |
| R2-8 | MEDIUM | 지표 토픽이 구독 상한(50)을 다 먹어 **인스턴스 50개부터 방송 구독이 전부 거부** | ✅ slowq 자리를 먼저 확보 |
| R2-9 | MEDIUM | **월 경계가 겹쳤다** — `BETWEEN` 은 양끝 포함이라 다음 달 0시 레코드가 두 달에 잡혔다 | ✅ 끝을 1ms 당김 + 인접 월 맞물림 테스트 |
| R2-10 | MEDIUM | **로컬 개발에서 아무 웹페이지가 수집을 멈출 수 있었다**(CSRF). 루프백은 토큰 없이 통과하고, 응답은 못 읽어도 부작용은 일어난다 | ✅ `x-dbmon-control` 헤더 요구(실측 403) |
| R2-11 | LOW | 결과가 줄면 빈 페이지가 나왔다 | ✅ 페이지 보정 |

추가로 자체 발견: **표에 상태 열이 없어 진행 중·추적 끊김·확정이 같아 보였다.**
진행 중의 실행시간은 지금까지의 값이고 추적 끊김은 하한인데 확정값으로 읽힌다 →
`StateBadge` + 사유 표시(`⚠ collector_paused`).

## 2way 리뷰 3라운드

Codex 가 5건. 5건 다 실재했고 전부 수정했다. **이 라운드에서 처음으로 "주석은
맞는데 코드가 그 일을 하지 않는" 결함이 나왔다.**

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R3-1 | CRITICAL | **일시정지가 태스크를 멈추지 않았다.** `timeout_at(deadline, h)` 는 만료 시 `JoinHandle` 을 드롭하는데 드롭은 **취소가 아니라 분리(detach)** 다. 주석에는 "명시적으로 abort 해야 한다" 고 적어 놓고 abort 할 대상을 이미 넘겨버렸다 — 멈춤을 눌러도 수집이 계속되고 상태는 `collecting: 0` 이며, 재개하면 같은 인스턴스에 태스크가 둘 생긴다 | ✅ `&mut h` 로 기다리고 만료 시 `h.abort()`. 돌연변이 검증(abort 제거 → 실패) + 실제 부하로 "정지 구간 신규 기록 0건, 재개 후 다시 기록됨" 측정 |
| R3-2 | HIGH | **균등 분배가 표본을 왜곡했다.** 2라운드에서 굶주림을 없앴지만 500대 × 천장 501이면 몫이 **1** 이다 — 바쁜 1대의 최근 50건 중 1건만 표에 오르고 나머지 자리는 다른 인스턴스의 오래된 최신값이 채운다. 표는 "최신순" 이라고 말하면서 가운데가 빈다 | ✅ 1차에서 몫을 꽉 채운 인스턴스에만 남은 예산을 재분배(`redistributed`). 활동이 몇 대에 몰린 실제 상황에서 정확해진다 |
| R3-3 | HIGH | **일수 상한이 인스턴스 수를 막지 못했다.** 400일 × 500대 = 20만 회 질의 — 2라운드에 넣은 `MAX_RANGE_DAYS` 는 일수만 봤다 | ✅ 파티션 예산 4,000(`인스턴스 × 일수`). 넘으면 **최신 쪽만** 남기고 `truncated=true`. 근거: 현재 규모(100대)에서 월 통계가 온전히 돌아야 한다 |
| R3-4 | HIGH | 순차 조회라 **왕복 지연이 그대로 곱해졌다** (500대 × 5ms = 2.5초) | ✅ 동시 16개(`buffered` — 순서를 유지해야 결과가 다른 인스턴스에 붙지 않는다) |
| R3-5 | MEDIUM | 일시정지 확정이 **전역 200건**만 보고 자기 것을 걸렀다. 다른 워커의 오래된 진행 중 레코드가 그 앞자리를 채우면 내 것은 영원히 안 보인다 | ✅ 상한을 스윕과 같게(500) + 꽉 차면 경고. 더불어 `list_in_flight` 가 **1MB 페이지에서 끊기던 것**을 페이지 추적으로 고쳤다 — 고아 정리도 같은 함수 위에 있었다 |

자체 발견 3건:

- **0건인데 상한에 걸린 경우** 표가 "기록이 없다" 고 말했다. 상한 표시는
  `Pagination` 이 하는데 그건 0건에서 렌더되지 않는다 — 조사하던 사람이 "문제
  없음" 으로 결론 낸다. 표 안에서 구분해 말하고 DOM 테스트로 고정했다.
- 2차 재분배가 **1차 결과와 2차 결과를 동시에** 들고 있었다. 집계 천장(2만 건)에서
  그건 메모리 두 배다 → 버릴 슬롯을 미리 비운다.
- `clamp(1, per_instance)` 는 `per_instance = 0` 이면 **패닉**한다. 지금 호출부는
  안전하지만 조회 하나가 프로세스를 죽일 수 있는 형태는 남기지 않는다.

운영 쪽 하나: compose 에 `stop_grace_period: 60s` 를 넣었다. 기본 10초로는
셧다운의 리스 반납 단계가 돌지 못해 **다음 프로세스가 1분간 리더가 되지 못했다** —
이 라운드 검증 중에 직접 겪었고, 원인을 찾는 데 5분을 썼다. 운영(ECS)에서는
`stopTimeout` 이 같은 일을 하고 그 값은 Terraform 이 검증한다.

## 2way 리뷰 4라운드

Codex 가 5건. 4건 수정, 1건은 **의도된 방향**으로 남겼다.

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R4-1 | HIGH | **8일치 표본을 "이번 달" 이라고 이름 붙였다.** 파티션 예산으로 구간을 좁히면서 응답의 `from_ms`/`to_ms` 는 **요청한 구간**을 그대로 돌려줬다 | ✅ `Collected` 에 실제로 읽은 구간을 담아 보고. 실측: 41대 × 399일 → 이전 `399.0일`(거짓) → `96.7일` |
| R4-2 | HIGH | 재분배가 **한 번뿐**이라 재분배받은 쪽이 덜 채우면 그 몫이 또 놀았다 | ✅ 최대 2라운드 반복. 라운드마다 `asked[i]` 로 판정한다(몫이 인스턴스별로 달라진다) |
| R4-3 | HIGH | **재분배도 같은 파티션을 다시 읽는다** — 예산이 실제로는 3배였다 | ✅ 예산을 파도 수로 나눠 잡고 총량을 고정(12,000 ÷ 3). 100대 월 조회는 통과(실측 41대 `truncated=false`) |
| R4-4 | HIGH | **일시정지 중에는 고아 스윕이 멈췄다.** 며칠 멈춰 두면 다른 워커·이전 epoch 의 고아가 그동안 "진행 중" 으로 남는다 — F4 가 없애려던 상태 | ✅ `sweep_orphans` 로 빼서 두 경로가 같은 코드를 부른다 |
| R4-5 | LOW | `found.len() >= want` 는 "정확히 그 수만큼 있었다" 를 절단으로 오판한다 | 📝 남긴다. **과하게 말하는 방향**이고 화면 문구도 "이보다 더 있을 수 있다" 다 |

`ORPHAN_SWEEP_LIMIT` 의 천장도 문서화했다. GSI1 은 워커별로 나뉘지 않으므로 전역
진행 중이 500건을 넘으면 그 뒤는 보이지 않는다 — 조용히 넘기지 않고 로그로 말한다.

**이 라운드는 CRITICAL 이 없었다.** 4라운드 연속 지적 수: 9 → 11 → 5 → 5, 심각도는
CRITICAL 1 → 0.

## 2way 리뷰 5라운드 — 그리고 관찰이 잡은 것

Codex 가 2건. 둘 다 실재했고 수정했다. **그런데 이 라운드의 큰 것은 리뷰가 아니라
로컬 스택을 들여다보다 나왔다.**

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R5-1 | HIGH | 일시정지 정리에 **총 예산이 없었다.** 조회 20초 + 쓰기 20초 + 스윕 20초를 각각 재면 합이 리스 TTL(60초)을 넘을 수 있다 — 멈추려다 리더를 잃고, `paused` 플래그가 없는 다른 프로세스가 수집을 이어간다(화면은 "멈춤" 인데 기록은 쌓인다) | ✅ 정리 전체를 **하나의 예산**으로. 넘기면 다음 tick 이 이어서 한다 |
| R5-2 | HIGH | 화면이 응답의 `from_ms`/`to_ms` 를 **무시하고** 요청한 달을 카드 제목에 적었다 — 백엔드가 정직해진 것이 화면에서 다시 거짓이 됐다 | ✅ `monthRangeLabel` 이 좁혀진 구간을 제목에 적는다(`2026-08 중 08-24~08-31`) |

### 관찰이 잡은 것

로그를 보다가 **고아 스윕이 30초마다 같은 레코드를 "확정했다" 고 보고하면서 1시간
반 동안 아무것도 바꾸지 않는 것**을 발견했다. 그 옆에는 `낙관적 잠금 재시도 5회 초과`
가 같은 주기로 찍히고 있었다. 파고들어 보니 **한 실행에 항목이 둘** 있었다:

```text
SK 1787241482413#10912  state=finalized  capture=merged       rows_examined=21633
SK 1787241482416#10912  state=in_flight  capture=processlist  rows_examined=0     ← 유령
```

이 상태에서 유령(416)을 확정하려 하면 초 버킷 조회(`begins_with`)가 **작은 SK**(413)를
돌려주므로 병합 결과가 413에 써진다. 유령은 남고 스윕은 **성공을 보고한다.**

✅ 갱신 경로가 **자기 키를 먼저 본다**(`find_exact`, `GetItem`). 진행 중 갱신처럼 같은
키를 매 tick 쓰는 경로에서는 `Query` 대신 `GetItem` 이라 **더 싸다.** 실측: 1시간 반
동안 안 닫히던 유령 3건이 **한 번의 스윕에 닫혔다**(`abandoned=3 errors=0`).

✅ 조회도 그 쌍둥이를 두 줄로 냈다(200행 중 3건). `dedupe_executions` 로 접는다 —
끝을 관측한 쪽 > 정확 지표가 있는 쪽 > 더 길게 관측한 쪽. **버리는 행이 유일한
출처인 값은 옮긴다**(실행계획이 진행 중 행에만 있으면 계획이 화면에서 사라진다).
단 **처리목록의 `rows=0` 은 채우지 않는다** — 측정값이 아니어서 평균을 깎는다.
실측: 같은 테이블을 새 코드로 읽으면 중복 0건, 유령 줄 0건.

## 2way 리뷰 6라운드 — 뿌리를 찾았다

Codex 가 6건. **그중 하나가 5라운드에서 내가 "경합" 으로 넘긴 것의 진짜 원인이었다.**

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R6-1 | **CRITICAL** | `to_item_keyed` 가 **읽어온 레코드의 `started_at_ms` 필드로 키를 다시 만들었다.** 그런데 이 설계는 "항목은 처음 저장된 자리에 머문다" 이므로 **첫 병합 이후 필드와 물리 키가 어긋난다**(항목은 416, 필드는 413). 그 다음 갱신은 413을 겨냥해 ① 413이 비면 조건 실패 → **재시도 5회 초과로 죽고**(그 레코드는 그 시점부터 **영구히 갱신 불가**), ② 413에 쌍둥이가 있으면 그쪽을 고치고 416의 유령은 남는다. 즉 쌍둥이·유령·잠금 초과가 **한 뿌리**였다 | ✅ 읽을 때 **물리 키를 함께 들고 온다**(`Stored { record, pk, sk }`). 돌연변이 검증에서 예전 방식이 **로그에서 본 그 오류 문구 그대로** 실패하는 것을 확인했다 |
| R6-2 | HIGH | 접기가 **버리는 행의 정보를 함께 버렸다** — 실행계획이 진행 중 행에만 있으면 계획이 사라진다 | ✅ `absorb` (5라운드 커밋 직후 이미 반영) |
| R6-3 | HIGH | 접기가 `record_id` 만 봤다. **초 경계를 걸친 쌍둥이는 키가 다르다** | ✅ 저장소와 **같은 규칙**(인스턴스·스레드·다이제스트·±2초)으로 한 번 더 접는다. 임계값이 창보다 크면 오접기가 불가능하다는 근거를 주석에 남겼다 |
| R6-4 | HIGH | 리스 예산이 **드레인(3초)과 `sleep(interval)` 을 빼고** 계산됐다. `detect_interval_ms` 는 설정 상한이 60초라 잠든 사이에 리스가 만료된다 | ✅ 루프 수면을 **리스 갱신 주기(20초)로 상한**. 수집은 인스턴스별 태스크가 하므로 자주 깨워도 비용은 tick 게시뿐이다 |
| R6-5 | MEDIUM | `last_sweep_ms` 를 **일하기 전에** 찍어, 앞 작업이 예산을 다 쓰면 스윕이 "돌았다" 로 기록됐다 — 초과가 반복되면 **영구히 굶는다** | ✅ 실제로 돈 뒤에만 찍는다 |
| R6-6 | MEDIUM | 빈 상태가 여전히 "이 달에 없다" 라고 말했다 — 달의 일부만 읽었을 때 조사가 거기서 끝난다 | ✅ 좁혀졌으면 읽은 구간을 말한다 |

**교훈: "그 뒤에 뭐가 남았나" 를 두 번 물어야 했다.** 5라운드에서 나는 쌍둥이를
"동시 첫 쓰기 경합" 으로 결론 내고 조회·정리만 고쳤다. 경합은 실재하지만 **관측한
쌍둥이의 원인은 아니었다** — 원인은 병합 후 키가 표류하는 것이었고, 그건 결정론적으로
재현된다. 로그에 같이 찍혀 있던 `낙관적 잠금 재시도 5회 초과` 가 그 증거였는데
"다른 워커와 경합" 으로 넘겨 읽었다. **한 화면에 같이 나온 두 이상 징후는 같은
원인일 가능성이 높다.**

## 2way 리뷰 7라운드 — 남은 잔여를 쓸어냈다

Codex 가 8건. 6건 수정, 1건은 이미 반영돼 있었고, 1건은 **전제가 이 코드에 없었다**.

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R7-1 | HIGH | 6라운드 수정에도 **닫는 쓰기가 쌍둥이로 갈 수 있었다.** 병합이 시각을 앞당긴 레코드는 필드로 계산한 키가 쌍둥이를 가리키므로, `find_exact` 가 **그쪽**을 찾아 고친다 — 유령은 남는다 | ✅ 후보를 다 모아 `pick_target` 이 고른다. **"닫는 쓰기는 열려 있는 항목으로"** 규칙이 물리 키 일치보다 앞선다(테스트가 이 순서를 고정한다) |
| R7-2 | HIGH | `SK = <읽은 SK>` 는 `PutItem` 이 이미 그 키를 겨냥하므로 **항진명제**다. 동시 쓰기가 둘 다 성공하고 나중 것이 앞의 것을 덮는다 — 실행계획·정확 지표가 조용히 사라진다. 재시도 경로가 아예 돌지 않았다 | ✅ `rev` 카운터 조건. 옛 항목은 `attribute_not_exists(rev)` 로 받는다. 통합 테스트(`join!` 동시 쓰기)에서 **돌연변이 3/3 실패** 확인 |
| R7-3 | HIGH | 접기가 **연속한 두 실행**을 합칠 수 있었다 — `long_query_time` 이 2초보다 작으면 같은 스레드·다이제스트가 창 안에 두 번 시작한다 | ✅ 창에 더해 **구간 겹침**을 요구한다. 커넥션은 한 번에 한 문장만 실행하므로 겹치면 같은 실행이다 — 임계값 설정과 무관하게 성립한다 |
| R7-4 | HIGH | 접기 기준을 **남긴 행**으로 잡아 이긴 행이 뒤쪽이면 창이 사슬처럼 늘어났다 | ✅ 묶음의 **첫 행**을 기준으로. (Codex 지적 전에 자체 발견해 이미 반영) |
| R7-5 | MEDIUM | `has_plan` 을 진 행에서 **옮겼다.** 계획은 항목에 붙어 있고 화면은 남긴 행의 `record_id` 로 가져오므로, 눌렀을 때 **빈 상세**로 간다 | ✅ 옮기지 않는다. 대신 동순위면 **계획이 붙은 행을 남긴다** — 계획을 잃지도, 없는 계획을 약속하지도 않는다 |
| R7-6 | MEDIUM | 빈 상태가 **상한에 걸렸을 때도** "이 달에 없다" 라고 말했다(구간은 그대로여도 일부만 읽은 것이다) | ✅ `truncated` 도 함께 본다 |
| R7-7 | HIGH | 범위 조회가 **물리 시각**을 쓰므로 병합으로 시각이 앞당겨진 레코드가 월·일 경계에서 잘못 분류될 수 있다 | 📝 **의도된 대가**로 남긴다. "항목은 처음 자리에 머문다" 의 비용이고 최대 1초다. 대안(키 이동)은 조건부 쓰기를 깨뜨려 5라운드의 그 유령을 다시 만든다 |
| R7-8 | HIGH | 처리목록 행의 다이제스트가 `unknown-<thread>` 면 접기가 실패한다 | ⚠ **내가 틀렸다.** 처음에 "그런 자리표시자가 없다" 로 기각했는데, `grep` 이 조용히 실패한 결과였다(zsh 가 `--include` 를 거부). 실제로 `collector/build.rs:192` 가 `unknown-<thread_id>` 를 넣는다. 8라운드에서 수정 → R8-2 |

### 남은 것 (다음 세션의 결정 사항)

- **동시 첫 쓰기 경합**으로 쌍둥이가 생기는 것 자체는 그대로다. 조회는 접고, 정리는
  열린 쪽을 닫으므로 화면은 옳지만, 근본 수정은 `SK` 를 `record_id` 의 초 버킷으로
  **결정론화**하는 스키마 변경이다.
- 쌍둥이가 있는 동안 **계획이 버려진 항목에 붙어 있으면 볼 수 없다.** 위 결정과 함께 사라진다.

## 2way 리뷰 8라운드 — 6건 전부 실재, 하나는 내 오판을 되돌렸다

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R8-1 | HIGH | 저장소의 ±2초 병합이 **겹침을 보지 않았다.** 조회 쪽 접기는 7라운드에서 겹침을 요구하게 했는데, 저장소가 **먼저** 두 실행을 하나로 합쳐 버리면 조회는 볼 기회조차 없다 — 실행 하나가 영구히 사라진다 | ✅ `same_execution` 을 두 곳이 공유한다(겹침 + 자리표 처리) |
| R8-2 | HIGH | **자리표 다이제스트(`unknown-<thread>`)를 값으로 취급했다.** 심층 조회가 상한에 걸리면 실시간 캡처는 SQL 이 없어 다이제스트를 만들 수 없다. 그러면 슬로우로그의 진짜 다이제스트와 달라 **같은 실행이 두 줄로 남고 통계가 두 배**가 된다. 7라운드에 이미 지적됐는데 **내가 "그런 값은 없다" 로 잘못 기각**했다(grep 이 조용히 실패했다) | ✅ 병합 규칙(`merge_digest`)과 같게 "없음" 으로 본다. 조회 접기의 정렬 키에서도 다이제스트를 뺐다 — 자리표와 진짜 값은 문자열이 달라 정렬하면 서로 떨어지고, 그러면 인접 비교가 그 쌍을 영원히 못 만난다 |
| R8-3 | HIGH | 낙관적 잠금 재시도가 **결과적 일관성 읽기**로 다시 읽었다. 직전에 성공한 쓰기를 못 볼 수 있으므로 5회를 그대로 소진한다 | ✅ 읽고-병합하고-쓰는 경로의 두 조회를 `consistent_read(true)` 로 |
| R8-4 | MEDIUM | 닫는 쓰기가 **열린 후보 중 첫 번째**를 골랐다. 열린 쌍둥이가 둘이면 남의 것을 고치고 자기 것은 열린 채로 남는다 | ✅ 열린 부분집합에도 같은 우선순위(물리 키 → 필드 → 최근접)를 적용한다 |
| R8-5 | MEDIUM | 후보 조회가 **1MB 첫 페이지**만 봤다 — "모든 후보" 가 아니었다. 바쁜 1초에서 대상이 빠지면 쌍둥이를 새로 만든다 | ✅ 두 조회 모두 페이지를 따라간다 |
| R8-6 | MEDIUM | 저장소의 조회 우선순위에 **플랜 유무가 빠져** 목록은 플랜이 붙은 쌍둥이를, 상세는 다른 쪽을 고를 수 있었다 — 누르면 빈 상세 | ✅ 두 우선순위를 같은 축으로 맞췄다 |

**실측(전 경로 배선 후):** 실시간 캡처 → 슬로우로그 병합이 한 항목에서 일어났다 —
`항목 수 1`, `rev 5`(조건부 쓰기 5회 성공), `SK == started_at_ms`(키 표류 없음),
조회 5경로 중복 0건.

**교훈: 도구가 조용히 실패하면 그 결과를 사실로 적게 된다.** 7라운드에서 `grep -rn
"unknown-" --include=*.rs crates/` 를 zsh 가 거부해 **빈 결과**를 냈고, 나는 그걸
"없다" 로 읽어 리뷰 지적을 기각하고 문서에 적었다. 빈 결과는 "없다" 가 아니라
"못 찾았다" 다 — 특히 지적이 **파일과 줄 번호를 대고 있을 때** 그렇다.

## 2way 리뷰 9라운드 — 규칙을 도메인으로 올리고 수렴

Codex 가 4건. 그중 하나(R9-2)는 **자체 리뷰에서 먼저 찾아 이미 고쳐 둔 것**이다 —
두 축이 같은 결함을 독립적으로 짚었다.

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R9-1 | HIGH | **겹침은 완벽한 증거가 아니다.** 종료를 관측하지 못한 레코드(`Disappeared`)의 끝은 "사라진 것을 알아챈 폴링" 까지 늘어나므로, 같은 커넥션이 곧바로 같은 쿼리를 다시 돌리면 겹쳐 보인다 — 그 둘을 접으면 실행 하나가 사라진다 | ✅ 조회 접기는 겹침에 **시작 시각 차이 상한(1초)**을 더한다. 실시간 추정은 정수 초 절단이라 쌍둥이의 시작 차이가 1초를 넘을 수 없고, 시계 편차가 더 큰 경우는 저장소가 더 넓은 창으로 이미 합쳐 준다 |
| R9-2 | MEDIUM | 접기가 **바로 앞 한 줄만** 봤다 — 중첩 문장처럼 다이제스트가 다른 행이 쌍둥이 사이에 끼면 그 쌍은 영원히 못 만난다 | ✅ 창 안의 후보를 **모두** 본다(자체 발견 후 반영) |
| R9-3 | MEDIUM | **페이크 저장소가 다른 규칙을 썼다** — 다이제스트 동일 + ±2초, 겹침·자리표 없음. 단위 테스트가 통과하면서 실제만 틀리는 구조다(이 프로젝트가 이미 한 번 겪은 유형) | ✅ 판정을 도메인으로 올렸다(`ExecutionSpan::is_same_execution`). 실제 어댑터·페이크·조회가 **같은 함수**를 부른다 |
| R9-4 | LOW | "행을 누르면 **전체** SQL 을 본다" 는 과한 약속이다 — 마스킹·절단·권한 제한·대표 SQL 이 모두 가능하다 | ✅ "저장된 SQL", 다이제스트 표는 "대표 SQL" 로. 모달 주석도 정정 |

**수렴 신호.** 라운드별 지적 수와 최고 심각도:

| 라운드 | 지적 | 최고 심각도 | 성격 |
|---|---|---|---|
| 1 | 9 | HIGH | 조회 경로 정확성 |
| 2 | 11 | HIGH | 굶주림·비용·CSRF |
| 3 | 5 | CRITICAL | 일시정지가 안 멈췄다 |
| 4 | 5 | HIGH | 구간 보고·재분배 |
| 5 | 2 (+관찰 2) | CRITICAL | 유령이 안 닫혔다 |
| 6 | 6 | CRITICAL | **키 표류 — 뿌리** |
| 7 | 8 | HIGH | 잠금이 항진명제 |
| 8 | 6 | HIGH | 자리표·일관성 |
| 9 | 4 | HIGH | 겹침의 한계·페이크 정합 |

9라운드에서 처음으로 **새 결함이 아니라 기존 규칙의 경계**만 나왔다(겹침의 증거력,
페이크 정합, 문구). 저장 경로의 결함은 6~8라운드에서 뿌리째 정리됐다.

## 2way 리뷰 10라운드 — 저장 경로에도 같은 잣대를

Codex 가 2건, 그리고 **"수렴하지 않았다"** 는 판정을 냈다. 둘 다 맞았다.

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R10-1 | HIGH | 9라운드에서 **조회에만** 시작 시각 상한을 넣고 저장소는 겹침만 봤다. 종료를 관측하지 못한 레코드의 늘어난 끝이 다음 실행과 겹치고 창(±2초) 안이면 **두 실행이 한 레코드로 합쳐진다 — 되돌릴 수 없다.** 조회의 방어는 그때 이미 늦다 | ✅ 판정 폭을 도메인 상수로 뽑아(`LIVE_ESTIMATE_SPREAD_MS`) 저장소·페이크·조회가 **같은 값**을 쓴다. 통합 테스트 추가(돌연변이: 창으로 판정하면 `1 != 2` 로 실패) |
| R10-2 | MEDIUM | "리터럴은 마스킹돼 있다" 는 `Full`·`FullRestricted` 환경에서 **거짓**이다 — 원문이 저장되고 권한자에게 그대로 보인다 | ✅ "환경의 저장 정책에 따라 남아 있을 수도, 마스킹돼 있을 수도 있다" |

**남은 위험을 정확히 적는다.** `long_query_time` 이 1초 미만이면 추정 오차 폭(1초)
안에 연속한 두 실행이 들어올 수 있다. 근본 해결은 실행마다 고유한 식별자
(`performance_schema` 의 `EVENT_ID`)를 레코드에 담는 것이고 **지금 레코드에는 없다** —
[21 §3.2](21-resume.md) 에 남긴다. 창을 넓히거나 좁히는 것으로는 해결되지 않는다.

## 이 라운드의 교훈

**1. "옮기기" 는 참조를 읽는 것에서 시작한다.** 스크린샷 한 장이 설계 문서 열
페이지보다 기대를 정확히 말한다. 이 세션에서 잃은 시간은 전부 그걸 안 본 대가다.

**2. 셸 인용 실수로 소스를 128줄 오염시켰다.** `python3 -c "..."` 안에 백틱을
넣어 `env` 가 실행됐다. 이후 **모든 편집은 heredoc(`<<'PY'`)** 으로 한다.

**3. 브라우저로 보면 데이터의 거짓말이 보인다.** P-1~P-3 은 타입도 테스트도 통과한
값이었다. 표에 `1970. 01. 01.` 과 `avg_rows 0` 이 찍힌 것을 눈으로 보고서야 잡혔다.

## 2way 리뷰 11~12라운드 — 공개 배포 직전, "프로덕션에서만 죽는 것"

저장소를 GitHub 로 옮기고 CI 가 **처음** 실행되면서, 로컬에서 한 번도 드러나지 않은
부류가 한꺼번에 나왔다. 11라운드는 인증·발행 경로, 12라운드는 저장 경로·데이터 손실을
봤다.

### 11라운드 — 설정 화면이 약속한 것을 코드가 하지 않았다

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R11-1 | CRITICAL | 설정 화면이 로그인 방식으로 `token` 을 제공하는데 **그 모드로 들어올 수단이 코드에 없었다.** `authenticate` 는 `dev_token`(dev ∧ 비루프백 ∧ 비ECS)만 비교하므로 **ECS 배포는 `off` 를 켜지 않는 한 모든 요청이 401** | ✅ `http.auth_token` 배선. 32자 미만·공백은 기동 거부, `Debug`·`Serialize` 에서 가림, `subject=shared-token` 으로 감사 구분 |
| R11-2 | CRITICAL | `serves_local_ui()` 게이트 때문에 **배포에서 `/` 가 404** 였다. 근거("prd 에서는 인증을 통과할 수 없다")가 R11-1 로 사라졌는데 게이트가 남았다 — README 는 "브라우저로 들어와 설정을 마친다" 를 안내하고 있었다 | ✅ 게이트 제거. 정적 자산은 비밀을 담지 않고, 자격증명이 없으면 화면이 `mode=unconfigured` 로 사유를 말한다 |
| R11-3 | HIGH | 슬로우로그 오귀속이 **계정 차원에서 열려 있었다.** 리전 불일치는 fail-closed 였는데 계정은 아니었다 — 크로스 계정 인스턴스를 우리 계정 클라이언트로 조회하면 **같은 이름의 다른 계정 DB 로그를 그 인스턴스의 것으로 저장한다**(리터럴 포함) | ✅ 두 판정을 `check_scope` 순수 함수로 빼고 계정도 거부. IAM DB 인증도 `for_instance` 가 계정을 본다 |
| R11-4 | HIGH | 문서의 신뢰 정책이 `sts:ExternalId=dbmon` 을 요구하는데 **코드가 보내지 않았다** — 문서를 그대로 따르면 크로스 계정 탐색·메트릭이 전부 `AccessDenied` | ✅ 상수로 항상 보낸다(조건 없는 역할에는 무시되므로 안전) |
| R11-5 | HIGH | 관리자 토큰을 URL(`?token=`)로 안내했다. 주소창에서 지워지는 것은 **첫 요청이 나간 뒤**이고, 그 시점에 ALB 액세스 로그가 이미 기록했다 — 만료 없는 admin 자격증명이 로그에 남는다 | ✅ 붙여넣기 칸(요청을 만들지 않는다). URL 방식은 로그가 로컬에만 남는 dev 컨테이너용으로 축소 |
| R11-6 | MEDIUM | `iam.tf` 의 조건 없는 `Sid = "Metrics"` 문이 뒤의 네임스페이스 조건을 **무력화**했다. IAM 은 허용의 합집합이다 — 좁히려고 쓴 주석만 남아 있었다 | ✅ 넓은 문 제거 |
| R11-7 | MEDIUM | `storage.plan_bucket` 은 **선언만 있고 읽는 곳이 없었다**(`aws.endpoint_url` 과 같은 부류). 그리고 계획 크기에 상한이 없어 400KB 항목 상한을 넘기면 **레코드 전체가 저장되지 않는다** | ✅ 설정 제거 + 150KB 초과 시 **계획만** 버리고 `too_large` 사유를 남긴다 |
| R11-8 | MEDIUM | WebSocket 재인증이 `InstanceStatus` 구독을 무조건 유지했다("환경을 모르니 보수적으로"). 보안 판정에서 보수적인 쪽은 **거부**다 | ✅ 스코프가 바뀌면 버린다. 재구독 경로가 등록부를 읽어 제대로 검사한다 |
| R11-9 | MEDIUM | Cognito 교집합의 지연 결함 둘: 실제 그룹 이름은 `dbmon-admin` 인데 `parse` 는 접두어 없는 이름만 알아 **토큰이 역할을 좁히지 못했다**(배선 순간 fail-open). `claims_version` 을 `<` 로만 봐서 서버보다 높은 버전이 통과했다 | ✅ 접두어 처리 + 등호 비교 |

### 12라운드 — 로컬이 프로덕션과 다르면 테스트는 아무것도 보증하지 않는다

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R12-1 | CRITICAL | 프로덕션 GSI1 은 `INCLUDE`(17개) 사영인데 `list_in_flight` 이 인덱스 항목을 **완전한 `SlowQuery`** 로 역직렬화했다. 그 타입에는 컨테이너 `#[serde(default)]` 가 없어 사영에 없는 **필수 필드 21개** 때문에 첫 항목에서 실패한다 — **F4 고아 정리와 일시정지 확정이 프로덕션에서만 아무것도 하지 못하고** 진행 중 레코드가 영구히 쌓인다. 로컬·테스트 테이블이 `ALL` 사영이라 통합 테스트 33개가 통과했다 | ✅ 인덱스에서 **키만** 읽고 기본 테이블에서 `BatchGetItem`. 테스트·로컬 투영을 프로덕션과 일치시키고, **Terraform 을 읽어 대조하는 단위 테스트**를 만들었다 |
| R12-2 | HIGH | `mark_missing` 이 무조건 `ADD` 였고 임계값은 2다. 멈췄다 되살아난 옛 리더와 새 리더가 같은 미발견을 각각 올리면 **살아 있는 인스턴스에 삭제 도장이 찍힌다** | ✅ `missing_at_ms` + 240초 창으로 중복 거부. epoch 으로는 막을 수 없다 — 같은 리더의 정상 라운드도 같은 epoch 이라 임계값에 영원히 못 닿는다 |
| R12-3 | HIGH | 설정 저장이 **Slack 비밀 참조를 지울 수 있었다.** `refresh()` 는 읽기 실패 시 기본값을 돌려주고, 저장 경로가 그걸로 마스킹된 비밀을 되메꿨다. `expected_version` 은 맞으므로 조건부 저장이 성공한다 | ✅ `refresh_checked` — 저장 경로는 실패를 받고 저장하지 않는다(502) |
| R12-4 | HIGH | Terraform `40-compute` 에 **공유 토큰을 넣을 방법이 없었다.** 그대로 배포하면 인증 수단이 하나도 없다 | ✅ `auth_token_secret_arn`(평문 아님 — tfstate·plan 에 남는다) + `secrets:` + **실행 롤**에만 읽기 권한 |
| R12-5 | MEDIUM | `set_state` 조건이 `attribute_exists(PK)` 뿐이어서 수집 태스크의 첫 판정이 탐색이 쓴 `Disabled`(`dbmon:enabled=false`)·`Excluded` 를 **덮었다** — 태그로 끈 인스턴스가 계속 수집된다 | ✅ 그 네 상태를 덮지 않고 `Conflict`. 규칙을 코어로 올려 어댑터·페이크가 공유 |
| R12-6 | MEDIUM | 상세 메트릭이 조회 실패를 **200 + 빈 계열**로 돌려줬다 — IAM 거부·스로틀링이 "이 지표를 안 내보내는 인스턴스" 와 구분되지 않았다(플릿 경로는 이미 `failed_scopes` 로 구분) | ✅ 응답에 `failed` 를 넣고 화면이 표시한다. 실패는 캐시하지 않는다 |
| R12-7 | MEDIUM | 모델이 낸 **재작성 SQL 이 검증되지 않았다**("비어 있지 않다" 만). 우리가 실행하지는 않지만 복사해 실행하라고 붙여 주는 문장이다 | ✅ `is_safe_rewrite` — 단일 문장, 원본과 같은 선두 키워드, 스키마·권한 변경 키워드 거부. 버린 사실은 주의사항에 |
| R12-8 | MEDIUM | `10-foundation` 의 `app_env` 가 지운 `PLAN_BUCKET` 을 계속 내보냈다 — export 하면 `deny_unknown_fields` 로 **기동 실패** | ✅ 제거 + "앱에 없는 설정을 내보내지 않는다" 를 주석으로 |

### 이 두 라운드의 교훈

**1. 로컬 테이블이 프로덕션과 다르면 테스트는 무엇도 보증하지 않는다.** R12-1 은
통합 테스트 33개를 통과한 채 프로덕션에서만 죽어 있었다. 그리고 그걸 막아야 할 대조
테스트는 **주석으로만 존재했다**("`it_store` 가 두 정의를 대조한다"). 주석이 주장하는
검증은 테스트로 있어야 한다 — 없으면 그 주장 자체가 결함이다.

**2. CI 를 한 번도 돌리지 않은 CI 는 CI 가 아니다.** `justfile` 이 파싱조차 되지
않아(주석 안의 백틱과 이중 중괄호를 just 1.58 이 렉싱한다) `just check` 가 전부 죽어
있었고, 그래서 `cargo fmt` 위반이 밀렸다. GitHub 에서 처음 돌자 여섯 잡 중 셋이 깨졌다.
로컬 게이트가 CI 가 보는 것을 다 보게 맞췄다(`terraform fmt` 포함).

**3. "설정 화면에 있다" 와 "동작한다" 는 다르다.** R11-1·R11-2 는 둘 다 화면이 약속한
것을 코드가 하지 않은 경우다. 화면이 고를 수 있게 해 둔 것은 실제로 그 경로로 들어갈 수
있어야 하고, 아니면 화면이 그 사실을 말해야 한다(`mode=unconfigured`).

**4. 실패를 값으로 담지 않으면 실패가 사라진다.** R12-6 은 이 저장소가 이미 세 번
배운 규칙("실패는 크게 알린다")을 한 엔드포인트에서만 빠뜨린 경우다. 플릿에는
`failed_scopes` 가 있었고 상세에는 없었다 — 같은 원칙이 경로마다 다시 확인돼야 한다.

## 2way 리뷰 13~15라운드 — 경계 상태를 스칼라로 뭉갠 자리들

13라운드부터 **critical 이 나오지 않았다.** 대신 리뷰어가 되풀이되는 **부류**를 짚었고
(14라운드), 그 부류를 닫자 같은 부류가 **생애 주기 경계**로 옮겨간 것을 다시 짚었다
(15라운드). 개별 결함보다 그 진단이 이 세 라운드의 성과다.

### 13라운드 — 내가 만든 구멍 셋 + 슬로우로그 유실

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R13-1 | HIGH | 슬로우로그가 **한 페이지만 읽고** 체크포인트를 `last_ts + 1` 로 올렸다. `FilterLogEvents` 는 `limit`·1MB 에서 끊으므로 **같은 밀리초의 남은 이벤트가 영구히 유실된다** | ✅ `next_token` 페이지네이션(상한 20) |
| R13-2 | HIGH | **내가 만든 것**: `is_safe_rewrite` 가 선두만 봐서 `WITH c AS (…) DELETE FROM t` 가 통과했다. MySQL 8.0 이 받는 문장이다 | ✅ 다른 종류의 DML 키워드가 아예 없어야 한다로 좁혔다 |
| R13-3 | MEDIUM | **내가 만든 것**: GSI2 에 GSI1 의 사영 목록을 줬다(프로덕션은 17 / 10) | ✅ 상수를 분리하고 대조 테스트가 두 인덱스와 `local/table.json` 을 본다 |
| R13-4 | MEDIUM | **내가 만든 것**: 조회 실패 배너와 "이 지표를 안 내보낸다" 타일이 동시에 떴다 | ✅ 실패 시에는 "못 읽었다" |
| R13-5 | MEDIUM | 240초 **고정** 간격이 짧은 탐색 주기(하한 30초)를 깼다 — 정상 라운드가 전부 거부되고 삭제 판정이 8라운드 늦어진다 | ✅ 주기의 절반으로 유도한다 |
| R13-6 | MEDIUM | 등록부 조회 실패가 "삭제된 인스턴스" 로 표시됐다 (`data ?? []`) | ✅ 오류를 먼저 표시한다 |
| R13-7 | MEDIUM | **수집 정지가 아무것도 빨갛게 만들지 않았다.** `/healthz` 는 항상 200(의도적), `/readyz` 는 성공 시각을 본문에만 담고, 자체 지표 발행 코드가 없다 | ✅ `readyz.collect_stale` 을 드러낸다. **`ready` 는 내리지 않는다** — 내리면 로드밸런서가 태스크를 빼고 ECS 가 교체하는데 수집 실패는 보통 환경 문제라 **무한 교체**가 된다 |
| R13-8 | MEDIUM | 정지 확정이 실패를 조용히 버렸다 (이 함수는 정지 전이에서만 불리고 고아 스윕은 자기 소유를 건너뛴다) | ✅ 사유·건수를 남긴다 |
| R13-9 | MEDIUM | 읽지 않는 클라이언트가 `send().await` 를 블록해 **`select!` 의 유휴 종료·재인증 분기가 폴링되지 않았다** | ✅ 5초 상한 |

### 14라운드 — "경계 상태를 스칼라나 로그로 뭉갠다"

리뷰어가 이 문장으로 부류를 정의했다: **시각 대신 커서, `send() -> ()` 대신 전달 결과,
0 이 "기동" 과 "정지" 를 동시에 뜻하는 것, 실패에 재시도 상태가 없는 것.**

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R14-1 | HIGH | 체크포인트가 **시각 하나**여서 "페이지 중간" 을 표현할 수 없었다. `+1` 은 건너뛰고 그대로 두면 진행이 0 이다 — R13-1 의 두 선택이 모두 증상이었다 | ✅ `Cursor { position_ms, next_token }`. 토큰이 있는 동안 위치는 움직이지 않는다(CloudWatch 가 같은 `start_time` 을 요구한다) |
| R14-2 | MEDIUM | **내가 만든 것**: `" DELETE "` 를 찾는 방식이 `)DELETE` 를 0건으로 봤다 | ✅ 식별자 문자가 아닌 것을 구분자로 보고 **토큰 단위**로 판정 |
| R14-3 | MEDIUM | **내가 만든 것**: 5초 상한을 `ready`·`error` 에도 적용했다. `ready` 를 잃으면 브라우저는 인증되지 않은 채 핑만 보내고 서버 연결은 무기한 살아 있다 | ✅ `send`(버린다) / `send_or_close`(닫는다) 로 분리 |
| R14-4 | MEDIUM | 정지 확정 실패에 **재시도 상태가 없었다** | ✅ `pending_pause_close` |
| R14-5 | LOW | **내가 만든 것**: `collect_stale` 이 리더 취임 직후 참이었다(`last_ok == 0`) | ✅ 취임 시각부터 유예 |
| R14-6 | LOW | 페이크가 파생 `Default` 로 간격 0(중복 방지 꺼짐), 고정 300초, 경계에서 어댑터와 반대 판정 | ✅ 손으로 쓴 `Default`, `with_discovery_interval`, `<=` |
| R14-7 | LOW | 투영 테스트가 상수와 파일만 대조해 **호출부의 실수를 놓쳤다**(R13-3 이 그렇게 살아남았다) | ✅ `gsi_definition` 을 함수로 빼서 **만들어진 요청**을 검증. 돌연변이로 확인 |

### 15라운드 — 같은 부류가 생애 주기 경계로 옮겨갔다

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R15-1 | MEDIUM | **빈 페이지로 끝나면** 위치가 `None` 이어서 호출부가 커서를 쓰지 않고 **낡은 토큰이 영구히 남았다.** CloudWatch 는 빈 페이지를 정상적으로 준다 | ✅ 항상 위치를 돌려준다 |
| R15-2 | MEDIUM | `NO_BACKSLASH_ESCAPES` 에서 `SELECT 'x\'; DELETE FROM orders` 는 **두 문장**인데 스캐너는 하나로 봤다. `"` 인용을 같은 이유로 이미 거부하고 있었다 | ✅ 인용 안의 `\` 와 닫히지 않은 인용도 판정 포기 사유로 |
| R15-3 | MEDIUM | **내가 만든 것**: 비예약어(`start`·`session`·`global`)를 문맥 없이 거부해 **정상 재작성이 버려졌다** | ✅ 문장 단위 키워드는 **선두에서만**. `FOR UPDATE` 는 인접 토큰으로 |
| R15-4 | MEDIUM | `Slowq`·`Subscribed` 가 아직 버려질 수 있었다. 방송을 잃으면 `stream_lagged` 도 안 뜨고, 구독 확인을 잃으면 집합이 영구히 어긋난다 | ✅ 둘 다 `send_or_close` |
| R15-5 | MEDIUM | **내가 만든 것**: `pending_pause_close` 가 bool 이라 재시도가 그 시점의 정지 집합을 다시 썼다 — 실패한 인스턴스를 재개하면 대상에서 사라져 **영구히 안 닫힌다** | ✅ 못 닫은 **집합**을 돌려주고 대상은 `paused ∪ pending` |
| R15-6 | LOW | **내가 만든 것**: 리더 재취임 시 이전 임기의 성공 시각이 새 유예를 덮었다 | ✅ 기준은 `max(성공, 취임)` |

### 이 세 라운드의 교훈

**1. 부류를 이름 붙이면 다음 결함이 보인다.** 14라운드의 "경계 상태를 스칼라로 뭉갠다"
는 한 문장이 R15-1·R15-5 를 미리 설명한다. 개별 결함 목록만으로는 그게 안 보였다.

**2. 안전 쪽으로 조이면 반드시 반대 방향을 열어 본다.** 이 세 라운드에서 내가 만든
결함이 8건이고 전부 그 패턴이다 — 선두 키워드 검사(→ CTE 우회), 5초 전송 상한(→ 프로토콜
프레임 유실), 240초 고정 간격(→ 짧은 주기 파괴), 페이지 토큰(→ 만료 시 영구 정지),
비예약어 금지(→ 정상 SQL 거부). **조인 뒤에 "이 조건이 정상 운영에서 유지되는가" 와
"이 값이 극단이면 무엇이 되는가" 를 묻는다.**

**3. 판정을 순수 함수로 빼면 그 자리에서 검증된다.** `check_scope`·`next_since`·
`is_bad_token`·`missing_min_gap_ms` 는 전부 AWS 없이 전수 검증된다. `is_bad_token` 은
처음 문자열 코드로 썼다가 테스트가 바로 실패했다 — `code()` 는 응답에서 채워지므로 손으로
만든 오류에는 없다. **타입 변형을 보는 편이 견고하고 테스트할 수 있다.**

**4. 로컬 게이트가 CI 가 보는 것을 다 봐야 한다.** `terraform fmt` 가 로컬에 없어서
7초 만에 죽는 왕복을 한 번 했다. 이제 `just tf-check` 가 fmt 를 먼저 본다.

## 2way 리뷰 16~22라운드 — 한 자리가 세 라운드 동안 방향을 번갈아 틀렸다

> **번호 대응.** 커밋 메시지의 `N회차 교차 리뷰` 는 이 문서의 **`N+10`라운드**다
> (`5회차` = 15라운드). 세션 중간에 카운터를 다시 시작해서 벌어진 일이라 여기 적어 둔다 —
> `git log --grep 회차` 로 각 라운드의 실제 변경을 찾을 수 있다.

critical 은 계속 나오지 않았다. 대신 **같은 자리(고아 판정)가 세 라운드 연속으로 최고
심각도**였고, 그 자리를 세 번 다르게 고친 끝에 22라운드에서 **판정을 없애는** 것으로
끝났다. 이 절의 값어치는 개별 결함이 아니라 그 궤적이다.

| 라운드 | 최고 심각도 항목 | 처리 |
|---|---|---|
| 16 | 재작성 검사를 문장 키워드 **어디서나 금지**로 조여 `SELECT start FROM t` 같은 정상 SQL 을 거부했다 | ✅ 선두에서만 + 인접 토큰 판정 |
| 17 | 정지 확정 상한이 "정지 전이 시점" 에 묶여 **재정지 구간의 레코드를 아무도 닫지 않았다** | ✅ 규칙을 "지금 멈춰 있는가" 로 (레벨 트리거) |
| 18 | 상한 절단이 **완성된 엔트리까지 버렸다**. 부수효과 함수가 결과를 안 돌려줘 호출부가 재시도할 수 없었다 | ✅ 경계를 자르지 않고, 전달 결과를 돌려준다 |
| 19 | 정지·`abort` 경로의 개별 구멍을 계속 막는 대신 **판정을 옳게** 만들었다 — 내 것이라도 도는 태스크가 없으면 걷는다 | ✅ `judge` 에 도는 태스크 집합을 넘긴다 |
| 20 | 같은 이름·같은 epoch 으로 뜬 **새 태스크가 옛 태스크의 레코드를 영구히 가렸다** | ✅ 태스크 세대를 시각으로 구분 |
| 21 | **HIGH — 그 수정의 반대 방향**: 갓 뜬 태스크는 아직 아무 레코드도 만지지 못해 **살아 있는 쿼리를 버렸다** | ✅ 유예를 준다 |
| 22 | **배포 차단 — 또 반대 방향**: 크래시 루프가 유예를 매번 되돌려 **아무도 확정하지 않을 레코드를 무기한 가렸다.** 동시에 플랜을 확보한 살아 있는 레코드는 저장된 갱신 시각이 굳어 여전히 버려질 수 있었다 | ✅ **판정에서 소유 추론을 삭제.** 수집기가 하트비트로 "아직 관측 중" 을 저장한다 |

### 22라운드 — 보정을 늘리는 대신 전제를 참으로 만들었다

리뷰어의 처방은 "명시적 생애 주기 상태(유예 시작 + 추적 중인 레코드 id)를 만들어라" 였다.
그렇게 하지 않았다. 세 라운드의 결함이 전부 **같은 전제 위반**의 증상이었기 때문이다:

> 설계([05 §4.5](05-collector.md))는 `last_seen_at_ms` 가 tick 마다 갱신된다고 적었는데,
> 선행 저장은 **심층 조회 대상일 때만** 일어나고 플랜을 확보하면 그 대상에서 빠진다.
> 그래서 **살아 있는 쿼리의 저장된 갱신 시각이 굳었다.**

침묵이 "아무도 관측하지 않는다" 를 뜻하지 못하니 스윕은 소유자 정보로 그걸 추론해야 했고,
추론은 벽시계 하나로는 옳을 수 없었다(태스크는 뜨자마자 아무것도 만지지 못하고, 크래시
루프는 그 시각을 계속 되돌린다). 그래서 **관측 중이라는 사실을 아는 유일한 곳** —
수집기 — 이 그것을 15초마다 저장하게 했다.

| | 이전 | 이후 |
|---|---|---|
| 판정 입력 | 침묵 + 이름 + epoch + 도는 태스크 맵 | **침묵만** |
| 살아 있는 레코드 보호 | 소유 추론(양방향으로 틀렸다) | 하트비트가 침묵을 만들지 않는다 |
| 확정 실패한 레코드 | `Mine` 이라 영구히 남았다 | 침묵이 쌓여 `abandoned` |
| 삭제된 코드 | — | `RunningTasks`, `Verdict::Mine`, `SweepStats.mine`, `CollectTasks.started_ms`, `running_since()` |

**저장소가 죽으면 하트비트도 실패하지만 확정 쓰기도 같은 저장소를 쓴다** — 그래서 한쪽으로
치우칠 여지가 없다. 이게 소유 추론에는 없던 성질이다.

하트비트 주기는 **임계의 최소값보다 작아야** 한다(`GRACE_MS / 2`). 스윕하는 리더와 수집하는
워커의 `detect_interval_ms` 가 다를 수 있어 자기 임계에서 유도하면 긴 주기를 쓰는 워커의
살아 있는 레코드를 짧은 주기의 리더가 버린다 — 그 불변식을 단정으로 박았다
(`the_heartbeat_interval_stays_below_every_threshold`).

### 이 일곱 라운드의 교훈

**1. 같은 자리가 세 번 최고 심각도면 고치는 방향이 틀린 것이다.** 20·21·22라운드는 전부
고아 판정이었고, 매번 "이 경우도 가려야 한다 / 이 경우는 가리면 안 된다" 를 조건으로
추가했다. 조건이 세 개가 되었을 때 문제는 조건이 아니라 **판정이 없는 정보를 추론하려
한다**는 것이었다.

**2. 설계 문서의 전제를 코드가 지키는지 확인한다.** 05 §4.5 는 "tick 마다 갱신" 이라고
적었고 아무도 그걸 검증하지 않았다. 문서가 참이라고 믿고 그 위에 판정을 쌓으면, 틀린
전제가 **판정의 결함으로 나타난다** — 그러면 판정을 계속 고치게 된다.

**3. 실행해서 확인한다.** 이 라운드에서 로컬 MySQL 로 35초 쿼리를 흘려
`last_seen_at_ms` 가 15초마다 오르는 것과 스윕이 그 레코드를 `alive` 로 보는 것을 직접
봤다. 그 전 라운드들은 코드만 읽고 넘어갔고, 그래서 "굳는다" 를 세 라운드 동안 못 봤다.

### 23라운드 — 하트비트가 세 가지를 깨뜨렸다 (내가 만든 것)

22라운드의 수정(`upsert_merged` 로 레코드 전체를 다시 쓰는 하트비트)이 **배포 차단 셋**을
새로 열었다. 셋 다 "생존 신호를 데이터 경로로 보냈다" 는 하나의 실수에서 나왔다.

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R23-1 | BLOCKER | 하트비트가 **없는 레코드를 만들었다.** 심층 조회 상한 밖 후보는 레코드가 없는 것이 설계인데, SQL 없이 만든 레코드는 정책이 `off` 로 강등되고 **병합은 먼저 기록된 정책을 고정**한다 — 그 뒤 도착하는 SQL 이 영구히 버려진다 | ✅ `touch_in_flight`(조건부 `UpdateItem`)로 내렸다. 만들지 않고 정책을 싣지 않는다. `needs_heartbeat` 도 **저장된 적 없는 항목을 제외**한다 |
| R23-2 | BLOCKER | 임계가 **리더의** `detect_interval_ms` 로 계산됐다. 하트비트는 tick 안에서만 쓸 수 있으므로 60초 tick 워커는 200ms tick 리더가 만든 30.6초 임계를 지킬 수 없다 — 살아 있는 레코드가 버려진다 | ✅ 임계를 **합법 설정 최악값**의 상수로(`3 × 60초 + 30초 = 210초`). `config.rs` 가 같은 상수를 쓰고 컴파일 시점 단정이 관계를 지킨다 |
| R23-3 | BLOCKER | 스윕이 읽고-판정하고-쓰는 사이에 하트비트가 도착하면 **살아 있는 레코드가 `abandoned`** 가 된다. `rev` 재시도는 그 결정을 그대로 다시 적용한다 | ⚠ 부분 — 병합이 `Finalized` 일 때 `abandoned_reason` 을 지우게 해 **영구 오표시**를 없앴다(확정이 도착하면 정정된다). 원자적 조건부 확정은 [21 의 잔여 위험 4](21-resume.md) |
| R23-4 | MAJOR | 모든 저장이 방송되므로 **브라우저가 15초마다 목록 전체를 무효화**했다 | ✅ 데코레이터가 `touch_in_flight` 를 방송하지 않는다 — 새 사실이 아니다 |
| R23-5 | MAJOR | 확정 저장이 실패하면 추적기에서 이미 빠져 **재시도 주체가 없다.** 스윕이 `owner_lost` 로 닫아 정상 종료 관측을 잃는다 | ⚠ [21 의 잔여 위험 5](21-resume.md). 슬로우로그가 대부분 정정한다 |
| R23-6 | MINOR | 내 통합 테스트가 **손으로 만든 하트비트 레코드**를 썼다 — 실제 `build()` 를 거치지 않아 R23-1 을 놓쳤다. 문서는 "모든 살아 있는 레코드가 15초 안에 갱신된다" 고 단정했다 | ✅ 테스트를 실제 포트로 다시 썼다(만들지 않음·되살리지 않음·되돌리지 않음·`GSI1SK` 순서). 문서는 tick 주기가 실질 상한임을 적는다 |
| R23-7 | MINOR | 삭제된 `Verdict::Mine` 을 근거로 코드를 정당화하는 주석 8개 | ✅ 실제 근거로(사유가 틀린다·유령이 보인다) 다시 썼다 |

### 22~23라운드의 교훈

**1. 생존 신호를 데이터 경로로 보내지 않는다.** `upsert_merged` 는 "새 사실을 병합한다" 는
연산이고 하트비트는 사실을 만들지 않는다. 같은 함수를 쓴 순간 생성·정책 고정·방송이 전부
따라왔다. **연산의 의미가 다르면 포트도 달라야 한다.**

**2. 내가 만든 테스트가 내 결함을 통과시켰다.** 22라운드에서 "하트비트가 SQL 을 지우지
않는가" 를 확인했는데, 그 테스트가 **프로덕션 경로 대신 손으로 만든 레코드**를 썼다. 정책
강등은 `build()` 안에서 일어나므로 보이지 않았다. **의심하는 코드가 실제로 만드는 값을
써야 테스트가 의미를 갖는다.**

**3. 설정 상한은 판정식의 입력이다.** 임계를 "내 주기의 3배" 로 두는 것은 **모든 워커가
같은 설정**이라는 암묵적 가정이었다. 분산 시스템에서 그 가정은 공짜가 아니다 — 합법 설정의
최악값으로 계산하면 가정이 사라지고, 그 대가(유령 210초)는 이미 다른 주기가 지배한다.

### 24라운드 — 인수인계가 하트비트를 무력화한다 (또 내가 만든 것)

23라운드의 수정이 두 방향을 새로 열었고, 하나는 **없앤 신호를 소비자가 둘 쓰고 있었다.**

| # | 심각도 | 결함 | 처리 |
|---|---|---|---|
| R24-1 | BLOCKER | 물리 키는 `started_at_ms` **추정치**로 만들어지는데 그 추정은 관측자마다 최대 1초 다르다. 리더가 바뀌면 `upsert_merged` 는 ±2초 후보 조회로 **이전 리더가 만든 행**에 병합하는데, 하트비트는 자기 추정으로 키를 다시 계산해 그 행을 영원히 못 찾는다 — 살아 있는 쿼리가 210초 뒤 고아로 확정된다 | ✅ 조건 실패 시 `upsert_merged` 와 **같은 범위 조회**로 그 행을 찾아 갱신한다. 빠른 경로는 여전히 쓰기 한 번 |
| R24-2 | BLOCKER | `saved_at_ms == None` 이 "저장소에 행이 없다" 와 "다른 수집기가 저장했다" 를 **한 값으로 뭉갰다.** 인수인계 뒤 새 추적기는 전부 미저장이므로, 심층 조회 상한 밖이거나 선행 저장이 계속 실패하면 그 행을 아무도 갱신하지 않는다 | ✅ 미저장 항목도 대상에 넣는다(조건부 갱신은 만들 수 없으므로 23라운드의 제외 이유가 사라졌다). **저장된 것을 먼저** 주어 상한에서 보호한다 |
| R24-3 | MAJOR | 23라운드에 `Finalized` 면 `abandoned_reason` 을 지우게 했는데, `StateBadge` 와 `aggregate::merge_rows` 가 그 값을 **정확 지표를 의심할 근거**로 쓰고 있었다. `merge(q, q)` 의 멱등성도 깨졌다 | ✅ 되돌렸다. 멱등성 테스트에 **사유를 든 확정 레코드**를 넣었다 — 그 조합이 없어서 회귀가 통과했다 |
| R24-4 | MAJOR | 하트비트가 `duration_ms` 를 안 쓰므로 진행 중 쿼리의 소요가 화면에서 **얼어붙는다.** 09 §3.4 는 "클라이언트가 1초마다 증가시킨다" 고 적었는데 구현이 없었다 | ✅ `lib/elapsed.ts` — 진행 중 행만, 저장값보다 짧게 보여주지 않는다(시계 차이 흡수). 진행 중 행이 보일 때만 타이머를 돈다 |
| R24-5 | MINOR | `orphan.rs` 가 삭제된 `stale_threshold_ms` 를 링크했다 | ✅ 상수로 |

### 24라운드의 교훈

**1. 신호를 지우기 전에 소비자를 grep 한다.** "확정인데 포기 사유가 있다" 를 모순으로 보고
지웠는데, 화면과 집계가 그걸 **의도적으로** 읽고 있었다. 코드에서 모순처럼 보이는 것이
도메인에서는 이력일 수 있다 — 값을 없애는 변경은 읽는 쪽을 먼저 세어 본다.

**2. 추정치로 만든 키는 관측자가 바뀌면 어긋난다.** `record_id`·`PK`/`SK` 가 `started_at_ms`
추정에서 나오고 그 추정은 `PROCESSLIST.TIME`(정수 초)에 묶여 있다. 저장 경로는 그 사실을
알고 ±2초 후보 조회를 갖고 있었는데, **새로 만든 경로가 그 지식을 다시 갖추지 않았다.**
같은 행을 겨냥하는 연산은 같은 방법으로 행을 찾아야 한다.

**3. "없다" 와 "내가 모른다" 를 한 값으로 뭉개지 않는다.** `saved_at_ms: Option<_>` 의
`None` 이 두 사실을 뭉갰고, 그중 하나(인수인계)에서 기능이 죽었다. 14라운드가 이름 붙인
**"경계 상태를 스칼라로 뭉갠다"** 가 이 세션에서 다섯 번째로 같은 모양으로 나왔다.

### 24라운드 수정의 실측 (코드 읽기로는 확인할 수 없는 부분)

리뷰어가 지적한 두 시나리오를 **실제로 재현**했다. 로컬 MySQL + DynamoDB Local 이다.

**① 인수인계 표류** — 수집기 1을 띄워 90초 쿼리의 진행 중 행을 만들고
(`SK=1787382001818#46604`), **드레인 없이 `kill -9`** 했다. 리스가 만료된 뒤(60초)
수집기 2가 인수했고, 그 뒤의 하트비트가 **같은 물리 행**을 15초 주기로 계속 갱신했다
(`last_seen` …164627 → …179664 → …194702). 스윕은 매 라운드 `alive=1` 로 판정했다.
수정 전이라면 수집기 2의 추정 키에는 행이 없으므로 조건이 계속 깨지고, 210초 뒤 살아
있는 쿼리가 `abandoned` 가 된다.

**② 화면의 경과 시간** — 같은 행을 브라우저에서 4초 간격으로 읽었다:

| | 진행 중 행 | 확정 행 |
|---|---|---|
| 처음 | `261.1s` | `2.2s` / `8.4s` |
| 4초 뒤 | `265.1s` | `2.2s` / `8.4s` |

진행 중만 흐르고 확정은 멈춰 있다 — 두 방향을 한 번에 확인했다. 저장된 값은
88,108ms 였다(플랜을 확보한 시점에 굳었다). **화면이 그 값을 그대로 보여줬다면 4분째
도는 쿼리를 88초로 표시하고 있었다.**
