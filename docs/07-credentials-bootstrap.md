# 07. 자격증명 부트스트랩

## 0. 전체 그림

```
[1회성 · 부트스트랩]                        [상시 · 수집]
                                          
마스터 자격증명 확보                        IAM DB Auth 토큰 (15분)
  ├ RDS 관리형 마스터 시크릿                     │
  ├ 지정된 Secrets Manager ARN                  ▼
  └ UI 1회 수동 입력 (메모리만)          TLS 연결 → 읽기 전용 조회
        │
        ▼
CREATE USER dbmon IDENTIFIED WITH AWSAuthenticationPlugin
GRANT ... (읽기 전용)
        │
        ▼
마스터 자격증명 폐기 (zeroize)
```

**핵심 원칙 3개**
1. 마스터 자격증명은 **부트스트랩 시점에만** 존재한다. 저장하지 않는다.
2. 상시 수집에는 **비밀번호가 없다**. IAM 토큰을 매 연결마다 발급한다.
3. 부트스트랩은 **멱등**하다. 몇 번 돌려도 결과가 같다.

## 1. 마스터 자격증명 소스

### 1.1 (a) RDS 관리형 마스터 시크릿 — 우선

RDS가 마스터 비밀번호를 Secrets Manager에서 관리하고 자동 로테이션하는 기능이다.
활성화되어 있으면 `DescribeDBInstances` 응답에 시크릿 ARN이 들어온다.

```
DescribeDBInstances → DBInstances[].MasterUserSecret {
    SecretArn, SecretStatus, KmsKeyId
}
Aurora: DescribeDBClusters → DBClusters[].MasterUserSecret
```

```
if MasterUserSecret 존재 && SecretStatus == "active":
    GetSecretValue(SecretArn) → {"username": "...", "password": "..."}
    → 사람이 비밀번호를 만질 일이 전혀 없다
```

필요 권한:
```json
{
  "Effect": "Allow",
  "Action": ["secretsmanager:GetSecretValue", "secretsmanager:DescribeSecret"],
  "Resource": "arn:aws:secretsmanager:*:<account>:secret:rds!*"
}
```
관리형 마스터 시크릿의 이름은 `rds!db-<resource-id>` 패턴이므로 `rds!*`로 좁힐 수 있다.
KMS 키가 고객 관리 키면 `kms:Decrypt` 권한도 필요하다.

`SecretStatus`가 `rotating`이면 로테이션 중이다 → 부트스트랩을 미루고 재시도한다.

### 1.2 (b) 사용자 지정 Secrets Manager 시크릿

관리형 마스터 시크릿을 쓰지 않는 인스턴스를 위한 경로.
UI에서 인스턴스별(또는 환경별 공통) 시크릿 ARN을 등록한다.

기대 형식:
```json
{"username": "admin", "password": "...", "host": "(optional)", "port": 3306}
```

- `username`/`password` 키가 없으면 `masterUsername`/`masterPassword`,
  그다음 `user`/`pass`를 시도하고, 모두 없으면 명확한 에러를 낸다.
- ARN은 화이트리스트 검증한다(정규식 + `DescribeSecret` 성공 여부). 임의 ARN을 넣어
  다른 시크릿을 읽는 걸 막기 위해, IAM 정책의 리소스를 태그 조건(`secretsmanager:ResourceTag/dbmon=true`)
  으로 제한한다.

### 1.3 (c) UI 1회 수동 입력

레거시·수동 관리 환경을 위한 최후 경로.

**절대 규칙**
- 값은 HTTPS POST 본문으로만 받는다(쿼리스트링 금지 — ALB 액세스 로그에 남는다).
- 서버는 **메모리에만** 보관하고, 부트스트랩 종료 시 `zeroize`로 덮어쓴다.
- 전용 타입으로 감싼다:

```rust
/// Debug/Display/Serialize 를 의도적으로 구현하지 않는다.
/// 실수로 로그·에러·패닉 백트레이스에 노출되는 경로를 타입으로 차단한다.
pub struct MasterSecret(secrecy::SecretString);

impl MasterSecret {
    pub fn expose(&self) -> &str { self.0.expose_secret() }
}
// Drop 시 zeroize (secrecy 크레이트가 보장)
```
- 부트스트랩 요청은 **동기적으로 처리한다**. 큐·잡 테이블에 넣지 않는다(직렬화 = 유출 경로).
  여러 인스턴스 일괄 처리(FR-CRD-08)도 한 요청 안에서 순차 처리하고, 진행 상황만
  WebSocket으로 보고한다.
- 세션 타임아웃: 입력 후 10분 안에 실행하지 않으면 메모리에서 파기.
- 감사 로그에는 "수동 자격증명으로 부트스트랩 실행"만 남긴다(값은 절대 안 남긴다).

**로그 유출 방지 계층** — 애플리케이션 전역에 로그 마스킹 레이어를 둔다(NFR-S-10).
`tracing` subscriber에서 다음 패턴을 마스킹한다:
`password=`, `IDENTIFIED BY`, `token=`, `Authorization:`, `X-Amz-Security-Token`,
AWS 액세스 키 패턴(`(AKIA|ASIA)[A-Z0-9]{16}`).
방어의 주 수단은 타입이고, 마스킹은 2차 방어선이다.

## 2. 모니터링 계정 생성

### 2.1 IAM DB Auth 방식 (기본)

**전제** — 인스턴스에서 IAM DB 인증이 활성화되어 있어야 한다.

```
DescribeDBInstances → IAMDatabaseAuthenticationEnabled: true/false
Aurora: DescribeDBClusters → IAMDatabaseAuthenticationEnabled
```

`false`면 활성화가 필요하다. **이것은 프로덕션 RDS 변경 작업이다.**

```
ModifyDBInstance(DBInstanceIdentifier, EnableIAMDatabaseAuthentication=true,
                 ApplyImmediately=true)
Aurora: ModifyDBCluster(...)
```

FR-CRD-09에 따라:
- prd 환경 대상이면 **별도 2차 확인 대화상자**를 띄운다. 인스턴스 식별자를 사용자가
  직접 타이핑해야 진행된다.
- 변경 전 `DescribeDBInstances`로 현재 상태를 다시 확인해 화면에 표시한다.
- 감사 로그에 사용자·시각·대상·before/after를 남긴다.
- **재부팅이 필요한지는 인스턴스·엔진에 따라 다를 수 있다** → [OPEN-Q-10](OPEN-QUESTIONS.md)에서
  실측 확인한다. 확인 전까지 UI 문구는 "적용에 시간이 걸릴 수 있으며, 일부 구성에서는 재부팅이
  필요할 수 있습니다"로 보수적으로 표기한다. **우리가 재부팅을 실행하지는 않는다.**
- 앱이 `ModifyDB*`를 호출할 권한을 갖는 게 부담스러우면, 이 단계만 UI가 CLI 명령을 생성해
  보여주고 사람이 실행하는 모드를 제공한다(설정 `bootstrap.allow_rds_modify = false`).
  → **기본값은 `false`.** 최소 권한 원칙(NFR-S-09)에 따라, 앱이 프로덕션 RDS를 변경할 수
  있는 권한을 기본으로 갖지 않는다.

### 2.2 실행 SQL

**호스트 패턴을 `%`로 두지 않는다 (T-04 완화)** — 초기 설계는 모든 GRANT에 `@'%'`를 썼고
"`rds-db:connect` 부여를 통제하는 것이 유일한 방어선"이라고 했다. 그건 사실이 아니다.
MySQL 계정 자체를 **앱 서브넷으로 좁힐 수 있다.**

`rds-db:connect`가 다른 주체로 새면(정책 복붙, 개발자 Role, 침해된 Lambda) 어디서든
`dbmon`으로 붙어 권한 모드 A/B의 `SELECT`를 쓸 수 있다. 폴백 비밀번호 유출도 같다.
호스트 패턴을 좁히면 방어선이 하나 더 생긴다.

```
monitor_host 기본값 = 앱 서브넷 CIDR      예: '10.0.16.%'
워커가 여러 서브넷에 있으면 CIDR 축약 또는 계정을 서브넷별로 생성
부트스트랩 UI에 호스트 패턴을 노출하고, 기본값을 `%`가 아니게 한다
```
SG 인바운드([14 §3.4](14-infrastructure.md))와 이중 방어가 된다.
아래 예시의 `10.0.16.%`는 `monitor_host` 설정값이다.

```sql
-- 1) 계정 생성 (없을 때만)
CREATE USER IF NOT EXISTS 'dbmon'@'10.0.16.%'
  IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS'
  REQUIRE SSL;

-- 2) 전역 권한
GRANT PROCESS,
      REPLICATION CLIENT,
      SHOW DATABASES,
      SHOW VIEW
  ON *.* TO 'dbmon'@'<monitor_host>';

-- 3) performance_schema / sys 읽기
GRANT SELECT ON `performance_schema`.* TO 'dbmon'@'<monitor_host>';
GRANT SELECT ON `sys`.*                TO 'dbmon'@'<monitor_host>';

-- 4) 데이터 스키마 읽기 (권한 모드에 따라 4-A 또는 4-B)
```

### 2.3 권한 모드 — 두 가지 중 선택

`SELECT` 권한 범위가 이 설계에서 가장 민감한 결정이다.
`information_schema`는 별도 GRANT가 필요 없지만(모든 사용자가 접근 가능, 단 보이는 행은
권한에 따라 필터링됨), **`information_schema.TABLES`/`STATISTICS`/`COLUMN_STATISTICS`에서
어떤 테이블이 보이는지는 그 테이블에 대한 권한에 달려 있다.** 즉 카디널리티 수집에는
대상 테이블의 권한이 필요하다.

#### 모드 A: `broad` — 전체 읽기

```sql
GRANT SELECT ON *.* TO 'dbmon'@'<monitor_host>';
```

- 얻는 것: 모든 스키마의 카디널리티·DDL 수집, 사후 `EXPLAIN` 폴백 가능,
  `mysql.innodb_table_stats` 로 통계 신선도 확인 가능([ADR-016](03-decisions.md)).
- 잃는 것: 이 계정이 **운영 데이터를 읽을 수 있다.** IAM `rds-db:connect` 권한을 가진 주체는
  누구나 이 계정으로 붙어 데이터를 조회할 수 있다.
- 완화: `rds-db:connect` 권한을 앱의 Instance Profile에만 부여하고, 사람에게는 주지 않는다.
  CloudTrail로 `GenerateDbAuthToken`을... **은 로깅되지 않는다**(로컬 서명 연산). 따라서
  IAM 정책 부여 자체를 통제하는 것이 유일한 방어선이다. SCP/권한 경계로 고정한다.

#### 모드 B: `least` — 최소 권한 (권장 기본값)

```sql
-- 관측 대상 스키마만 명시적으로 허용
GRANT SELECT ON `shop`.*    TO 'dbmon'@'<monitor_host>';
GRANT SELECT ON `orders`.*  TO 'dbmon'@'<monitor_host>';
```

- 스키마 화이트리스트를 UI에서 관리한다. 화이트리스트에 없는 스키마는 카디널리티·DDL을
  수집하지 않고, 어드바이저는 "스키마 정보 없음"으로 동작한다(플랜 기반 분석만).
- **컬럼 수준으로 더 줄일 수 있는가?** `SELECT(col1, col2)` 형태의 컬럼 GRANT로는
  `SHOW CREATE TABLE`이 되지만 `EXPLAIN` 재실행이 깨진다. 실용성이 없어 채택하지 않는다.

#### 모드 C: `minimal` — 데이터 읽기 권한 없음 (검증 후 결정)

~~`EXPLAIN FOR CONNECTION` 은 이미 만들어진 플랜을 읽는 것이므로 대상 테이블 `SELECT` 권한이~~ → **기각** ([19 §B](19-m1-findings.md)): RDS 에서 실행 자체가 불가하다. 재실행 경로는 우리 계정으로 테이블을 읽으므로 `SELECT` 권한이
불필요할 가능성이 있다([OPEN-Q-06](OPEN-QUESTIONS.md)). 사실이면:

```sql
GRANT PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO 'dbmon'@'<monitor_host>';
GRANT SELECT ON `performance_schema`.* TO 'dbmon'@'<monitor_host>';
GRANT SELECT ON `sys`.*                TO 'dbmon'@'<monitor_host>';
-- 데이터 스키마 SELECT 없음
```

- 얻는 것: 모니터링 계정이 **운영 데이터를 한 행도 읽을 수 없다.** 보안상 가장 깔끔하다.
- 잃는 것: 카디널리티·DDL 수집 불가 → AI 어드바이저의 품질이 크게 떨어진다.
  사후 `EXPLAIN` 폴백도 불가.
  (히스토그램은 `ANALYZE TABLE ... UPDATE HISTOGRAM` 을 누가 실행해 둔 컬럼에만 존재하므로
  대부분의 인스턴스에서 어차피 비어 있다 — 권한 모드 간 차별 요소가 아니다.
  [05 §2.10](05-collector.md))
- 절충: 평상시 `minimal`로 두고, 어드바이저를 돌릴 때만 별도 계정(`dbmon_advisor`, 모드 B)을
  쓰는 2계정 구성. 어드바이저는 온디맨드이므로 사용 빈도가 낮다.

**기본값 결정** — 모드 B(`least`) + 스키마 화이트리스트.
OPEN-Q-06 검증 결과에 따라 `minimal` + 어드바이저 전용 계정 구성을 M7에서 도입할 수 있다.

### 2.4 권한별 필요 이유 (감사 대응용)

| 권한 | 필요한 기능 | 없으면 |
|---|---|---|
| `PROCESS` | 다른 계정의 스레드를 `processlist`에서 보기, `EXPLAIN FOR CONNECTION`, `SHOW ENGINE INNODB STATUS` | 자기 연결만 보임 → 수집 불가 |
| `REPLICATION CLIENT` | `SHOW REPLICA STATUS`, `SHOW BINARY LOG STATUS` | 복제 모니터링 불가 |
| `SHOW DATABASES` | 스키마 목록 열거 | 자가진단·화이트리스트 UI 불완전 |
| `SHOW VIEW` | 뷰가 포함된 쿼리의 `SHOW CREATE TABLE`/`EXPLAIN` | 뷰 참조 쿼리 분석 불가 |
| `SELECT ON performance_schema.*` | 다이제스트, `events_statements_current`, 락, 상태 카운터 | 핵심 기능 전부 불가 |
| `SELECT ON sys.*` | 인덱스 위생, 락 대기 뷰 | 해당 기능만 불가 |
| `SELECT ON <data>.*` | 카디널리티, DDL, `EXPLAIN` 재실행 | 어드바이저 품질 저하 |

**주지 않는 권한** — `SUPER`, `RELOAD`, `SHUTDOWN`, `FILE`, `CREATE`, `DROP`, `ALTER`,
`INSERT`, `UPDATE`, `DELETE`, `EXECUTE`, `REPLICATION SLAVE`, `SET_USER_ID`,
`SYSTEM_VARIABLES_ADMIN`, `CONNECTION_ADMIN`.
RDS는 `SUPER`를 마스터에게도 주지 않으므로 애초에 불가하지만, 명시적으로 요청하지 않는다.

### 2.5 비밀번호 폴백 (FR-CRD-05)

IAM DB Auth를 쓸 수 없을 때만.

```sql
CREATE USER IF NOT EXISTS 'dbmon'@'<monitor_host>' IDENTIFIED BY '<생성된 32자 랜덤>' REQUIRE SSL;
-- 이후 GRANT 는 동일
```

**⚠ 이 문장 자체가 비밀이다 (T-19)** — 폴백 경로에서 `CREATE USER ... IDENTIFIED BY '<pw>'`의
문장 전문은 **살아있는 DB 비밀번호**다. 그런데 설계상 이 문장은
(a) `plan` 응답의 `statement` 필드([13 §2.7](13-api-spec.md)),
(b) 감사 레코드의 `actions[].statement`([§4](#4-감사-로그-fr-crd-10)),
(c) WS 진행 메시지에 그대로 들어간다. → HTTP 응답 → 브라우저 메모리·스크린샷 →
DynamoDB `AUDIT#` → Iceberg 3년 보관까지 남는다.
로그 마스킹 레이어는 `tracing`에만 걸려 있고 API 응답·감사 레코드에는 걸리지 않는다.

**대응**
- 표시·저장용 문장은 **항상 렌더 함수를 통과**한다:
  `CREATE USER ... IDENTIFIED BY '<redacted>'`.
  원문은 `Secret<String>`이 담긴 별도 필드에만 있고 실행 시점에만 조립한다.
- `plan`/`apply` 응답 DTO와 감사 레코드의 `statement` 필드 타입을 `RedactedSql`로 두고,
  `Display`/`Serialize`가 항상 마스킹된 형태를 내보내게 한다(타입으로 강제).
- 비밀번호는 CSPRNG로 32자 생성한다(영숫자 + 안전한 특수문자. MySQL 파싱을 깨는
  `'`, `\`, 백틱은 제외).
- 즉시 Secrets Manager에 저장한다: 이름 `dbmon/target/<instance_id>`,
  태그 `dbmon=true`. 30일 자동 로테이션 Lambda를 연결한다.
- **인스턴스별로 다른 비밀번호를 쓴다.** 공통 비밀번호는 1세대의 문제였다(유출 시 전체 노출).
- 앱은 연결 시 Secrets Manager에서 읽고 캐시한다(TTL 15분). 로테이션 후 인증 실패 시
  캐시를 무효화하고 1회 재시도한다.
- 로테이션 Lambda는 이 프로젝트가 제공한다(Terraform 모듈에 포함). `dbmon` 계정의
  비밀번호만 바꾸는 단일 목적 함수이며, 마스터 자격증명이 필요하다
  → **`ALTER USER` 를 자기 자신에게 실행**하므로 마스터가 필요 없다:
  ```sql
  ALTER USER USER() IDENTIFIED BY '<new>';
  ```
  자기 비밀번호 변경은 별도 권한이 필요 없다. 이 방식이면 로테이션에도 마스터가 불필요하다.

### 2.6 멱등성과 권한 diff (FR-CRD-07, FR-CRD-11)

```
1. SHOW CREATE USER '<u>'@'<h>'   → 인증 플러그인, REQUIRE SSL, 계정 잠금 상태
   SHOW GRANTS FOR '<u>'@'<h>'    → 권한 집합 (동적 권한·롤 포함)
   (계정 없으면 에러 → 생성 경로)
2. 파싱해 현재 상태 산출
3. 필요 상태와 차집합 계산
4. 부족한 GRANT만 추가. 초과분은 REVOKE 하지 않고 화면에 경고 표시
   (사람이 의도적으로 준 권한을 우리가 뺏지 않는다)
5. 인증 플러그인·SSL 요구가 기대와 다르면 → 진행 중단 (§2.6.1)
```

**1차 소스는 `SHOW CREATE USER` + `SHOW GRANTS`다.**
초기 설계는 `information_schema.USER_PRIVILEGES` / `SCHEMA_PRIVILEGES` /
`TABLE_PRIVILEGES`를 1차로 쓰려 했지만 이 뷰들은
(a) **인증 플러그인·`REQUIRE SSL`을 보여주지 않고**(§2.6 5단계에 필요한 정보다),
(b) 동적 권한·롤이 나오지 않고,
(c) MySQL 8.0에서 **deprecated**다.
`SHOW GRANTS` 파싱이 문자열 처리라 취약한 것은 사실이지만, 출력 형식이 안정적이고
골든 테스트로 고정할 수 있다. `mysql.user` 직접 조회도 권한 모드 A(`SELECT ON *.*`)에서는
실제로 가능하므로 보조로 쓴다.

#### 2.6.1 계정 선점 공격 방어 (T-28)

**`CREATE USER IF NOT EXISTS`는 잘못된 기존 상태를 교정하지 않는다.**
계정이 이미 있으면 인증 플러그인이 무엇이든, `REQUIRE SSL`이 없어도, 초과 권한이 있어도
조용히 통과한다. 이게 공격 통로다.

```
시나리오
  1. 관리자가 plan 을 생성한다 (current_state.user_exists = false)
  2. 대상 DB에 CREATE USER 권한이 있는 내부자(또는 이전에 침해된 계정)가
     'dbmon'@'<monitor_host>' IDENTIFIED BY '<자기가 아는 비밀번호>' 를 만들어 둔다
  3. 관리자가 apply 한다 → IF NOT EXISTS 로 조용히 통과
  4. GRANT 가 실행되어 공격자가 비밀번호를 아는 계정에
     prd 스키마 SELECT + PROCESS 권한이 부여된다
```

**방어**

```
apply 시:
  a) 저장된 plan 문장의 해시와, 현재 상태로 재도출한 문장의 해시를 비교
     → 불일치면 409 plan_stale (무조건)
     (이게 없으면 "사용자가 타이핑 확인한 SQL"과 "실제 실행 SQL"이 다를 수 있다)
  b) plan 이 user_exists=false 였는데 apply 시 존재하면 → 진행 거부
     IF NOT EXISTS 대신 명시적 존재 검사 + 상태 비교
  c) 계정이 존재하는 경우:
       auth_plugin 이 기대와 다르면 → GRANT 실행하지 않고 중단 (제안이 아니라 차단)
       REQUIRE SSL 이 없으면 → 중단
       초과 권한이 있으면 → prd 는 중단, 비prd 는 경고 후 진행
  d) 모든 판정 결과를 감사 로그에 남긴다
```

초기 설계의 "인증 플러그인이 다르면 `ALTER USER`로 변경 **제안**"은 약하다.
제안은 무시될 수 있고, 무시된 상태로 GRANT가 실행되면 위 시나리오가 성립한다.
**차단이 맞다.**

### 2.7 Dry-run과 확인 (FR-CRD-06)

```
POST /api/bootstrap/plan   { instance_ids, credential_source, privilege_mode, schemas }
  → 200 {
      plan: [
        { instance_id, current_state: { user_exists, auth_plugin, grants },
          actions: [
            { kind: "modify_rds", detail: "EnableIAMDatabaseAuthentication=true",
              risk: "high", requires_confirmation: true },
            { kind: "sql", statement: "CREATE USER IF NOT EXISTS ..." },
            { kind: "sql", statement: "GRANT PROCESS, ... " }
          ],
          warnings: ["prd 환경입니다", "재부팅이 필요할 수 있습니다"]
        }
      ]
    }

POST /api/bootstrap/apply  { plan_id, confirmations: { "<instance_id>": "<typed name>" } }
```

- **실행될 SQL 전문을 그대로 보여준다.** 요약하지 않는다.
- prd 인스턴스는 `confirmations`에 식별자를 정확히 타이핑해야 통과한다.
- `plan_id`는 5분 후 만료된다(계획과 실행 사이에 상태가 바뀌었을 수 있으므로,
  apply 시 현재 상태를 재확인하고 계획과 다르면 거부한다).

## 3. IAM DB Auth 상시 연결

### 3.1 토큰 발급

```
region      = 인스턴스의 리전
hostname    = 인스턴스 엔드포인트 (라이터/리더 각각)
port        = 3306
username    = dbmon

token = SigV4 presigned "rds-db:connect" 요청  (유효 15분)
```

AWS SDK for Rust에 전용 헬퍼가 없으면 SigV4 서명을 직접 구성한다.
서명 대상은 `<host>:<port>/?Action=connect&DBUser=<user>`이고 리전·서비스명(`rds-db`)이
정확해야 한다. **크로스 리전 대상은 대상 인스턴스의 리전으로 서명한다.**

MySQL 연결 시 이 토큰을 비밀번호 위치에 넣고, TLS를 필수로 한다.
토큰이 200자를 넘으므로 클라이언트의 비밀번호 길이 제한을 확인해야 한다
(`mysql_async`는 문제없음. `mysql_clear_password` 플러그인 사용을 서버가 요구한다).

### 3.2 IAM 정책

```json
{
  "Effect": "Allow",
  "Action": "rds-db:connect",
  "Resource": "arn:aws:rds-db:*:<account>:dbuser:*/dbmon"
}
```

리소스의 두 번째 요소는 `DbiResourceId`(예: `db-ABCDEFGHIJKL`)이며 인스턴스 식별자가 아니다.
인스턴스가 교체되면 리소스 ID가 바뀐다. 500대를 개별 열거하는 것은 실용적이지 않고 정책 크기
한도에도 걸리므로 `*`를 쓴다. 사용자 이름을 `dbmon`으로 고정함으로써 범위를 제한한다.
**근거를 이 문서에 남기는 것이 NFR-S-09의 요구다.**

더 좁히고 싶으면: Terraform이 탐색된 인스턴스의 `DbiResourceId`를 모아 정책을 생성하는
방식이 가능하지만, 인스턴스 추가마다 apply가 필요해진다. 채택하지 않는다.

### 3.3 CA 인증서

RDS 글로벌 CA 번들을 앱 바이너리에 임베드한다(`include_bytes!`).
- 다운로드: `https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem`
- **번들 갱신이 필요하므로 만료 감시가 필요하다.** 번들에 포함된 인증서의 최소 만료일을
  기동 시 계산해 90일 미만이면 경고 로그 + 자체 알림.
- 인증서 검증을 비활성화하는 설정 항목을 **만들지 않는다**(NFR-S-03). 만들면 언젠가 쓰인다.

### 3.4 연결 실패 진단

| 증상 | 원인 | 안내 |
|---|---|---|
| `Access denied for user 'dbmon'` | IAM 정책에 `rds-db:connect` 없음 / 사용자명 불일치 | IAM 정책 확인 |
| `Access denied` + 토큰 정상 | 인스턴스에 IAM Auth 미활성 | 부트스트랩 재실행 |
| TLS handshake 실패 | CA 번들 만료 / SG에서 TLS 차단 | 번들 갱신 |
| `Too many connections` | 대상 DB 연결 포화 | `max_connections` 확인, 풀 크기 축소 |
| 토큰 만료 에러 | 시계 오차 | EC2 시각 동기(chrony) 확인 |
| 간헐적 `Access denied` | 초당 연결 제한 초과 | 풀 재생성 지터 확인 |

각 케이스를 자가진단 결과에 그대로 노출한다.

## 4. 감사 로그 (FR-CRD-10)

부트스트랩 관련 모든 동작을 `dbmon-config / AUDIT#<yyyy-mm>`에 남긴다.

```json
{
  "event": "bootstrap.apply",
  "actor": { "sub": "...", "email": "...", "groups": ["admin"] },
  "at_ms": 1755527391000,
  "instance_id": "123456789012/ap-northeast-2/orders-prd-01",
  "env": "prd",
  "credential_source": "rds_managed_secret",
  "privilege_mode": "least",
  "schemas": ["shop", "orders"],
  "actions": [
    { "kind": "sql", "statement": "CREATE USER IF NOT EXISTS 'dbmon'@'<monitor_host>' IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL", "result": "ok" },
    { "kind": "sql", "statement": "GRANT PROCESS, REPLICATION CLIENT, ... ", "result": "ok" }
  ],
  "before": { "user_exists": false },
  "after":  { "user_exists": true, "auth_plugin": "AWSAuthenticationPlugin" },
  "result": "success",
  "confirmation_typed": true
}
```

- 자격증명 값은 어떤 필드에도 넣지 않는다. `credential_source`만 남긴다.
- 감사 로그는 매월 Iceberg `audit_log` 테이블로 아카이브하고 3년 보관한다.

## 5. 수동 부트스트랩 (앱 권한 없이)

조직 정책상 앱에 `ModifyDBInstance`나 마스터 자격증명 접근을 주지 못할 수 있다.
그 경우 UI가 **실행할 SQL과 CLI 명령을 생성해 보여준다**. DBA가 직접 실행한다.

```bash
# 1) IAM DB 인증 활성화
aws rds modify-db-instance \
  --db-instance-identifier orders-prd-01 \
  --enable-iam-database-authentication \
  --apply-immediately \
  --region ap-northeast-2
```

```sql
-- 2) 모니터링 계정 생성 (마스터로 접속해 실행)
CREATE USER IF NOT EXISTS 'dbmon'@'<monitor_host>'
  IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL;
GRANT PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO 'dbmon'@'<monitor_host>';
GRANT SELECT ON `performance_schema`.* TO 'dbmon'@'<monitor_host>';
GRANT SELECT ON `sys`.*                TO 'dbmon'@'<monitor_host>';
GRANT SELECT ON `shop`.*               TO 'dbmon'@'10.0.16.%';   -- 화이트리스트 스키마마다
```

`FLUSH PRIVILEGES`는 **필요하지 않다.** `GRANT`는 인메모리 권한 캐시를 즉시 갱신한다.
이 문장은 `RELOAD` 권한을 요구하며 "필요 최소 문장만"이라는 원칙에도 어긋난다.
(grant 테이블을 직접 수정한 경우에만 필요하고, 우리는 그러지 않는다.)

실행 후 UI의 "권한 재점검" 버튼으로 검증한다. 이 경로가 **가장 안전한 기본값**이므로,
설치 가이드에서 이것을 먼저 제시하고 자동 부트스트랩은 편의 기능으로 소개한다.
