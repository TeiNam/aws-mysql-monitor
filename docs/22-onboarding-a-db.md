# 22. DB 를 모니터링에 편입하기 (온보딩 런북)

기존 RDS·Aurora 를 이 도구의 감시 대상으로 만드는 절차. **모니터링 계정은 비밀번호를
쓰지 않는다** — IAM DB Auth 하나뿐이다([ADR-007](03-decisions.md), FR-CRD-04).

이 문서의 명령은 2026-08-21 에 `123456789012` 개발 계정의 시드 DB(`dbmon-seed-dev-mysql`,
MySQL 8.4.11)에 **실제로 실행해 확인한 것**이다. 확인하지 않은 단계는 그렇다고 적었다.

---

## 0. 5분 요약

```bash
export AWS_PROFILE=teinam-primary-123456789012
export AWS_REGION=ap-northeast-2          # ⚠ §9-①
aws sso login --profile "$AWS_PROFILE"
# Client VPN 연결 (또는 VPC 안에서 실행)

scripts/seed-aws-db.sh --list             # 대상 확인
scripts/seed-aws-db.sh dev-mysql          # 스키마·데이터·dbmon 계정 (시드 전용)
scripts/seed-aws-db.sh dev-aurora --user-only   # 기존 DB 면 계정만

# 검증 — 토큰으로 붙어 본다
EP=<writer 엔드포인트>
TOKEN=$(aws rds generate-db-auth-token --hostname "$EP" --port 3306 --username dbmon)
MYSQL_PWD="$TOKEN" mysql -h "$EP" -u dbmon --enable-cleartext-plugin \
  --ssl-mode=VERIFY_CA --ssl-ca=<rds-bundle.pem> -e "SELECT CURRENT_USER();"
```

`scripts/seed-aws-db.sh` 는 시드 fleet 용이다(엔드포인트·시크릿 ARN 을 `60-seed` 의
terraform output 에서 읽는다). 그 밖의 DB 는 §4 의 SQL 을 직접 실행한다.

---

## 1. 어디까지 자동이고 어디부터 사람 손인가

| 단계 | 지금 |
|---|---|
| 새 인스턴스 **탐지** (목록·태그·버전·엔드포인트) | **자동** — 5분 주기 `DescribeDBInstances` |
| 수집 대상 **판정** (VPC·태그·이름 필터) | **자동** — `[discovery]` 설정 |
| 모니터링 **계정 생성** | **사람 손** — 실행 코드가 없다(§8-②) |
| `rds-db:connect` IAM 부여 | terraform (`40-compute`) — 목록이 비면 `dbuser:*/dbmon` 와일드카드라 새 인스턴스가 자동 포함된다 |
| 수집 **시작** | **막혀 있다** — `Pending → Collecting` 승격 주체가 없다(§8-①) |

즉 "태그만 달면 알아서 계정 만들고 수집" 은 **아직 아니다.** 필요한 조각은 §8 에 적어 뒀다.

---

## 2. 전제 확인 (읽기 전용)

> 앱 쪽 전제(DynamoDB 테이블 등)는 [14 §0 설치 사전준비](14-infrastructure.md)를 본다.
> 여기서는 **감시 대상 DB** 쪽만 다룬다.

### 2.1 IAM DB 인증이 켜져 있는가

```bash
aws rds describe-db-instances --query \
  'DBInstances[].{id:DBInstanceIdentifier,iam:IAMDatabaseAuthenticationEnabled}' --output table
# Aurora 는 클러스터 단위다
aws rds describe-db-clusters --query \
  'DBClusters[].{id:DBClusterIdentifier,iam:IAMDatabaseAuthenticationEnabled}' --output table
```

`false` 면 켜는 것은 **RDS 변경 작업**이다. FR-CRD-09 에 따라 prd 대상이면 2차 확인·감사
로그가 필요하고, **우리가 재부팅을 실행하지는 않는다**:

```bash
aws rds modify-db-instance --db-instance-identifier <id> \
  --enable-iam-database-authentication --apply-immediately
```

켤 수 없는 DB 는 §8-③.

### 2.2 수집 전제조건 (FR-DSC-10 점검 항목)

```sql
SELECT @@performance_schema, @@max_digest_length, @@performance_schema_max_digest_length,
       @@performance_schema_max_sql_text_length, @@slow_query_log, @@long_query_time;
SELECT name, enabled FROM performance_schema.setup_consumers
 WHERE name IN ('events_statements_current','statements_digest',
                'global_instrumentation','thread_instrumentation');
```

시드 DB 실측값(전부 충족, 재부팅 불필요): `performance_schema=1`, digest 4096/4096,
sql_text 4096, `slow_query_log=1`, `long_query_time=1`, consumer 4개 전부 `YES`.

⚠ 파라미터 그룹의 **정적** 항목(`performance_schema`, `max_digest_length` 등)은 재부팅
후에 적용된다. apply 직후 확인하면 기본값이 보인다.

### 2.3 네트워크

```bash
dig +short <endpoint>          # 사설 IP(10.x)로 풀려야 한다
nc -z -w 5 <endpoint> 3306
```

---

## 3. 호스트 패턴 — DB 가 보는 주소로 정한다

MySQL 에서 `user@host` 는 **각각 별개 계정**이다. 그래서 붙는 주체의 주소를 알아야 한다.

⚠ **Client VPN 은 소스 NAT 을 한다.** 클라이언트 CIDR 이 아니라 **association 서브넷의
ENI 주소**로 DB 에 도착한다. 실측: 클라이언트 주소가 `10.99.0.130` 인데 MySQL 은
`10.1.24.139`(priv-subnet-a, `10.1.16.0/20`)로 봤다.

```sql
-- 지금 내가 어느 주소로 보이는지
SELECT CURRENT_USER(), SUBSTRING_INDEX(host,':',1)
  FROM information_schema.processlist WHERE id=CONNECTION_ID();
```

결과: ECS 태스크(프라이빗 서브넷)와 VPN 을 탄 개발 노트북이 **같은 패턴 하나**로 덮인다.

| 주체 | DB 가 보는 주소 | 호스트 패턴 |
|---|---|---|
| ECS 태스크 | 프라이빗 서브넷 `10.1.16.0/20`·`10.1.32.0/20` | `10.1.%` 또는 netmask 표기(아래) |
| Client VPN 개발 노트북 | association 서브넷 ENI (`10.1.24.x`) | 같은 패턴으로 덮인다 |
| SSM 베스천 경유 | 베스천의 서브넷 주소 | 그 서브넷 CIDR |

`@'%'` 로 두지 않는다 — `rds-db:connect` 가 다른 주체로 새면(정책 복붙, 개발자 Role)
어디서든 `dbmon` 으로 붙을 수 있다. 호스트 패턴이 방어선 하나를 더 만든다(T-04).

### 3.1 CIDR 을 정확히 쓰려면 netmask 표기 (실측 확인)

MySQL 은 `호스트/넷마스크` 형식을 받는다. `10.1.%` 는 `/16` 전체를 여는 것이지만,
서브넷 하나만 열려면 이렇게 쓴다:

```sql
CREATE USER 'dbmon'@'10.1.16.0/255.255.240.0' ...   -- 10.1.16.0/20 (priv-subnet-a)
```

실측: 위 계정으로 `10.1.24.139` 에서 접속이 성공했다.

**그래서 태스크 IP 를 자동 탐지할 이유가 없다.** 서브넷 CIDR 은 terraform 이 이미 아는
정적 값이고, netmask 로 한 번 만들어 두면 **배포마다 태스크 IP 가 바뀌어도 그대로 맞는다.**
반대로 자동 탐지는 실패 모드를 만든다 — `ECS_CONTAINER_METADATA_URI_V4` 로 얻는 것은
*그 태스크 하나의 IP* 라서 서브넷을 역추적해야 하고, 서브넷이 추가되면 그 대역에서 뜬
태스크가 조용히 접속 실패한다. 값은 설정으로 주입한다(`collector.monitor_host`, 07 §2.2).

서브넷이 여럿이면 **서브넷마다 계정**을 만들거나 상위 CIDR 하나로 합친다. 계정이 늘어나는
쪽이 좁지만, GRANT 를 N번 해야 하므로 부트스트랩이 자동화돼 있어야 실용적이다(§8-②).

### 3.2 보안 그룹과 호스트 패턴은 **다른 층**이다

둘을 같은 것으로 보면 한쪽을 빼먹는다.

```
SG (패킷 층)       : 누가 3306 에 도달할 수 있나   → **SG 참조**로 통제
MySQL 호스트 패턴   : 그 주소에서 온 접속을 어느 계정으로 받나 → CIDR/netmask 로 통제
```

MySQL 은 보안 그룹을 볼 수 없으므로 계정 층은 주소로 써야 한다. 두 층이 막는 것이 다르다 —
SG 는 침해된 **다른 워크로드의 패킷**을, 호스트 패턴은 `rds-db:connect` 가 다른 주체로
샜을 때의 **계정 사용**을 막는다(T-04 + [14 §3.4](14-infrastructure.md)).

**SG 는 CIDR 이 아니라 SG 참조로 연다.** `60-seed` 가 그렇게 한다:

```hcl
resource "aws_vpc_security_group_ingress_rule" "db_from_tasks" {
  count = var.task_security_group_id == "" ? 0 : 1   # 40-compute 를 올린 뒤 켜진다
  referenced_security_group_id = var.task_security_group_id
  from_port = 3306
}
```

| | CIDR 로 열기 | SG 참조로 열기 |
|---|---|---|
| 허용 범위 | 그 대역의 **모든 것**(같은 서브넷의 다른 태스크·EC2·Lambda 포함) | 그 SG 를 붙인 리소스만 |
| 서브넷·AZ 추가 | 규칙을 고쳐야 한다 | **안 고친다** |

⚠ 2026-08-21 현재 라이브 DB SG 의 ingress 는 `SSM bastion`·`Client VPN` 둘뿐이다 —
`task_security_group_id` 가 비어 있어 ECS 규칙이 아직 생성되지 않았다. 40-compute 를
apply 한 뒤 그 SG id 를 `60-seed` 에 넘겨야 태스크가 붙을 수 있다.

---

## 4. 모니터링 계정 만들기

마스터 자격증명으로 **한 번** 실행한다. 앱은 이 작업을 하지 않는다.

```sql
-- 1) 계정 — 비밀번호가 없다
CREATE USER IF NOT EXISTS 'dbmon'@'10.1.%'
  IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL;

-- 2) ⚠ IF NOT EXISTS 는 **잘못된 기존 상태를 고치지 않는다.**
--    예전에 비밀번호 계정으로 만들어 뒀으면 조용히 통과하고 IAM 인증만 계속 실패한다.
ALTER USER 'dbmon'@'10.1.%'
  IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL;

-- 3) 전역 권한
--    PROCESS  — `information_schema.PROCESSLIST` 에서 남의 세션을 본다(없으면 탐지 불가)
--    SHOW VIEW — 뷰에 `SHOW CREATE TABLE` 을 하려면 필요하다(계획이 뷰를 참조한다)
GRANT PROCESS, SHOW VIEW ON *.* TO 'dbmon'@'10.1.%';

-- 4) 관측 스키마
GRANT SELECT ON `performance_schema`.* TO 'dbmon'@'10.1.%';

-- 5) 데이터 스키마 — 권한 모드 (07 §2.3)
--    모드 B(권장): 관측 대상 스키마만 화이트리스트
GRANT SELECT ON `shop`.* TO 'dbmon'@'10.1.%';
--    모드 A(broad): GRANT SELECT ON *.* — 카디널리티·DDL 을 전부 얻지만
--    이 계정이 운영 데이터를 읽을 수 있게 된다
```

**전에는 `REPLICATION CLIENT`·`SHOW DATABASES`·`SELECT ON sys.*` 도 줬다.** 코드가 그것을
필요로 하는 쿼리를 하나도 던지지 않는다는 것이 교차 리뷰에서 확인됐다 — 읽는 것은
`performance_schema.{processlist, events_statements_current,
events_statements_summary_by_digest, global_status, global_variables}`,
`information_schema.{PROCESSLIST, STATISTICS, TABLES}`, `SHOW CREATE TABLE` 이다.
리플리카 지연을 지표로 넣게 되면 그때 `REPLICATION CLIENT` 를 다시 준다.

⚠ 로컬 픽스처(`local/seed/03-monitor-users.sql`)는 편의상 조금 더 넓게 준다. 실제
인스턴스에는 위 목록만 준다.

확인:

```sql
SELECT user, host, plugin, ssl_type FROM mysql.user WHERE user='dbmon';
SHOW GRANTS FOR 'dbmon'@'10.1.%';
```

`plugin=AWSAuthenticationPlugin`, `ssl_type=ANY`(= `REQUIRE SSL`) 이어야 한다.

### 4.1 Aurora·리플리카 규칙

| 대상 | 어디서 실행 |
|---|---|
| Aurora 클러스터 | **라이터 엔드포인트에서 한 번.** 리플리카로 복제된다 |
| RDS 리드 리플리카 (`-ro`) | **소스에서.** 리플리카는 read-only 라 `CREATE USER` 자체가 실패한다 |
| 단독 인스턴스 | 그 인스턴스에서 |

---

## 5. IAM 쪽 — `rds-db:connect`

토큰을 만드는 주체가 이 권한을 가져야 한다. **리소스가 이름이 아니라 resource id 다**:

```
인스턴스: arn:aws:rds-db:<region>:<acct>:dbuser:db-JJHQYOS2DZIBDHDOKWSYPPI5DQ/dbmon
Aurora:   arn:aws:rds-db:<region>:<acct>:dbuser:cluster-CB2MLLAWHPOSTXKPZSEZWMKPEY/dbmon
```

```bash
# 인스턴스의 DbiResourceId
aws rds describe-db-instances --db-instance-identifier <id> \
  --query 'DBInstances[0].DbiResourceId' --output text
# Aurora 클러스터
aws rds describe-db-clusters --db-cluster-identifier <id> \
  --query 'DBClusters[0].DbClusterResourceId' --output text
```

`40-compute` 가 태스크 롤에 이 정책을 쓴다. `var.db_auth_resource_ids` 가 비면
`dbuser:*/dbmon` 와일드카드이고, 열거하면 그 목록만 허용한다 —
**DB 를 재생성하면 resource id 가 바뀌므로** 열거 모드에서는 terraform 재적용이 필요하다.

권한 확인은 시뮬레이션으로 한다(실제 접속 없이):

```bash
aws iam simulate-principal-policy --policy-source-arn <role-arn> \
  --action-names rds-db:connect --resource-arns <위 ARN> \
  --query 'EvaluationResults[0].EvalDecision' --output text     # → allowed
```

---

## 6. 검증 — 토큰으로 실제로 붙어 본다

```bash
curl -s -o /tmp/rds-ca.pem https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem
TOKEN=$(aws rds generate-db-auth-token --hostname "$EP" --port 3306 --username dbmon)
MYSQL_PWD="$TOKEN" mysql -h "$EP" -u dbmon \
  --enable-cleartext-plugin --ssl-mode=VERIFY_CA --ssl-ca=/tmp/rds-ca.pem \
  -e "SELECT CURRENT_USER();
      SELECT COUNT(*) FROM performance_schema.events_statements_current;
      SELECT COUNT(*) FROM shop.orders;"
```

- `--enable-cleartext-plugin` 이 **필수다.** IAM 토큰은 `mysql_clear_password` 로 전달되고,
  그래서 TLS 위에서만 쓴다.
- 토큰은 약 1500바이트, **유효 15분**이다.
- ⚠ **계정을 막 만든 직후 몇 초~1분은 `Access denied` 가 날 수 있다**(실측 1회). 같은
  명령이 잠시 뒤 통과한다 — 계정 상태·정책을 의심하기 전에 재시도한다.

---

## 7. 모니터링에 등록

계정이 준비되면 **설정이 그 인스턴스를 대상으로 보게** 만든다. 등록부는 탐색이 채우므로
사람이 행을 넣지 않는다.

```toml
[aws]
region     = "ap-northeast-2"
account_id = "123456789012"        # instance_id 의 첫 성분 — 나중에 못 바꾼다
# 여러 리전이면
# target_regions = ["ap-northeast-2", "us-east-1"]

[collector]
monitor_db_user = "dbmon"

[discovery]
allowed_vpc_ids = ["vpc-0123456789abcdef0"]   # AND 조건
# opt-in 으로 운영하려면 (태그 있는 것만 수집)
# required_tags = ["dbmon:enabled=true"]

# 환경을 **태그로만** 분류할 때. 기본값(dev 배포)은 이름에 prd/prod/production 이
# 들어가면 제외하고 태그가 prd 인 것도 거부한다(T-37). prd 를 흉내낸 시드를 관측하는
# 계정에서는 그 게이트가 대상을 지운다.
collect_production_targets = true
```

| 태그 | 뜻 |
|---|---|
| `env` / `environment` / `stage` / `tier` | 환경 분류. 값 매핑은 `EnvMapping` (기본: prd/prod/production/live/real → prd 등) |
| `dbmon:enabled=false` 또는 `0` | 이 인스턴스를 **제외**한다 → `Disabled` 로 등록 (opt-out) |

반영:

```bash
# 다음 주기(기본 5분)를 기다리지 않으려면 화면의 "인스턴스 수집" 을 누른다
curl -X POST http://<host>/api/discovery/run -H 'x-dbmon-control: 1'
curl -s http://<host>/api/instances | python3 -m json.tool | head -40
```

화면 **RDS Instances** 탭에서 `STATE`·`REAL-TIME`·태그를 확인한다.

---

## 8. 알려진 한계 (2026-08-21)

### ① `Pending → Collecting` 승격이 없다 — 수집이 시작되지 않는다

탐색은 `Pending` 으로만 등록하고(`aws/discovery.rs` `initial_state`),
`Instance::should_collect()` 는 `Collecting|Degraded|Unreachable` 만 참이다. 그 사이를
잇는 FR-DSC-10 자가진단이 [05 §9](05-collector.md) 에 문서화만 돼 있고 구현이 없다.

도커 로컬에서 이게 안 드러난 이유는 `local/instance.json` 이 `state: "collecting"` 을
손으로 박아 넣기 때문이다.

### ② 계정 부트스트랩 실행기가 없다

`CREATE USER` 를 실행하는 코드가 코드베이스에 **0곳**이고, `SecretSource` 포트는 선언만
있다. 태스크 롤에 `secretsmanager:*` 도 없다. 자동 온보딩을 만들려면:

1. `SecretSource` 어댑터 + `secretsmanager:GetSecretValue` (RDS 관리형은
   `secret:rds!*` 로, 자체 시크릿은 태그 조건으로 좁힌다)
2. 멱등 실행기 — `SHOW CREATE USER` 로 기존 상태를 보고 `CREATE`/`ALTER`, 리플리카는
   소스로 우회, Aurora 는 라이터로, 실패·감사 로그
3. **권한 분리**: 상시 수집 태스크에 마스터 시크릿 읽기를 주지 않고, 부트스트랩만 별도
   태스크/Lambda 롤로 단발 실행한다. 그 컨테이너가 침해되면 모든 감시 대상 DB 의
   마스터가 되기 때문이다(NFR-S-09, `bootstrap.allow_rds_modify=false` 가 기본인 이유).
   이 앱은 이미 `role` 개념(`all`·`api`·`collector`·`control`)이 있으므로
   **`role = "bootstrap"`** 을 더하고 그 태스크 정의에만 시크릿 권한을 붙이는 것이
   구조적으로 가장 싸다 — 같은 이미지, 다른 롤
4. 마스터 시크릿은 자동 로테이션된다 — **캐시하지 말고 사용 시점에 읽는다**

### ③ IAM 인증이 꺼진 DB 를 편입할 수 없다

탐색은 `iam_auth_enabled` 를 등록부에 넣고 화면에도 내보내지만, **인증 경로가 그 값을
읽지 않는다.** `build_auth()` 는 프로세스 전체를 "환경변수 비밀번호 하나"(dev 전용) 또는
"리전별 IAM" 으로 정하고 인스턴스별 분기가 없다. 그래서 IAM 이 꺼진 인스턴스는 IAM
토큰으로 붙으려다 `Access denied` 를 반복한다.

대응은 셋 중 하나: (a) §2.1 로 IAM 을 켠다, (b) 인스턴스별 인증 선택을 구현한다
(`SecretSource` + 등록부에 시크릿 ARN + `TargetAuth` 인스턴스별), (c) 태그
`dbmon:enabled=false` 로 제외한다.

---

## 9. 트러블슈팅

### ① `인스턴스 0대` · `시크릿을 찾을 수 없다`

환경변수 `AWS_REGION` 이 **프로파일 리전을 덮는다.** 쉘 프로파일이
`AWS_REGION=us-west-2` 를 내보내고 있으면 프로파일이 `ap-northeast-2` 여도 그쪽으로
간다. 증상이 권한 문제처럼 보인다 — 실측으로 두 번 걸렸다.

```bash
echo "$AWS_REGION"        # 확인
export AWS_REGION=ap-northeast-2
```

### ② `Access denied for user 'dbmon'@'10.1.24.139'`

원인이 셋인데 메시지가 같다. 이 순서로 가른다:

| 확인 | 방법 | 아니라면 |
|---|---|---|
| 호스트 패턴이 맞는가 | 임시 비밀번호 계정을 같은 패턴으로 만들어 붙어 본다 | §3 |
| IAM 이 허용하는가 | `simulate-principal-policy` (§5) | 정책·resource id |
| 계정이 IAM 플러그인인가 | `SELECT plugin FROM mysql.user WHERE user='dbmon'` | §4 의 `ALTER USER` |
| 방금 만들었나 | 1분 뒤 재시도 | §6 의 전파 지연 |

### ③ 그 밖

| 증상 | 원인 |
|---|---|
| `ERROR 1045` 인데 마스터도 안 붙는다 | Aurora 는 **RDS 관리형 시크릿**(`rds!cluster-…`)이고 RDS MySQL 은 별도 시크릿이다 — 시크릿을 섞어 쓴 것이다 |
| `CREATE USER` 가 실패한다 | `-ro` 리드 리플리카다. 소스에서 실행한다(§4.1) |
| 재귀 CTE 가 1001회에서 끊긴다 | `SET SESSION cte_max_recursion_depth` (시드 SQL 은 이미 올린다) |
| TLS 오류 | RDS CA 번들을 쓴다. 터널로 `127.0.0.1` 에 붙으면 인증서 이름이 맞지 않는다 — VPN 이면 실제 이름으로 붙으므로 이 문제가 없다 |
