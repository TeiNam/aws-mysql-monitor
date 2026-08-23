# 설치

**AWS Aurora & RDS MySQL Slow Query Monitor** — 돌리기까지 필요한 것 전부, 순서대로.

이건 운영자 문서다. 설계 근거(수집기가 왜 그렇게 도는지, ADR, 데이터 모델, 비용 산정)는
**공개하지 않는다** — 저장소 밖에 있고, 소스의 `.claude/docs/…` 주석 링크가 그걸 가리킨다.
설치하고 돌리는 데 그건 필요하지 않다.

English: [install.md](install.md).

---

## 0. 설치 경로는 둘이다

§1~8 은 **손으로 만드는** 경로다 — 콘솔·CLI 로 리소스를 하나씩 만든다. 이게 기준이다:
앱이 무엇을 필요로 하고 각 권한을 왜 좁혔는지가 그대로 보인다.

**Terraform** 경로도 있다. 같은 리소스를 만들지만 레이어별로 state 를 나눠 두고, 관리자가
실제로 쓰는 것은 이쪽이다.

| | 손으로 (§1~8) | Terraform (`infra/`) |
|---|---|---|
| 언제 | 평가용, 환경 하나, 또는 조직에 Terraform 이 없을 때 | 반복 가능하게, 환경 여러 개 |
| 수고 | 리소스 20여 개를 손으로 | 레이어별 `terraform apply` |
| 드리프트 | 사람이 관리한다 | state 가 관리한다 |
| 레이어 | — | `00-bootstrap` → `10-foundation` → `(20/30/50/60)` → `40-compute` |

레이어별 명령과 필수 변수는 [`infra/README.md`](../infra/README.md) 에 있다. 그 경로에서 물리는
두 가지만 여기 적는다:

- **레이어 순서는 권고가 아니다.** `40-compute` 는 `10-foundation` 의 state 객체를 직접 읽으므로,
  먼저 돌리면 `apply` 가 아니라 **`plan` 이 죽는다.**
- **apply 입력값을 파일로 둔다.** Terraform state 는 루트 모듈 입력 변수를 저장하지 않는다.
  한 번 잃으면 살아 있는 리소스에서 역구성해야 하고, `db_auth_resource_ids` 를 빠뜨린 apply 는
  `dbuser:*/dbmon` 으로 폴백해 **계정의 모든 인스턴스에 DB 인증을 조용히 허용한다.**
  `infra/layers/40-compute/dev.tfvars.example` 를 채워 두고 항상 `-var-file` 로 apply 한다.

어느 경로든 §3(IAM)·§4(네트워크)를 꼼꼼히 읽는다 — **대상 DB 의 보안 그룹은 두 경로 모두
이 프로젝트가 관리하지 않는다.**

## 1. 이미지

`.github/workflows/release.yml` 이 `main` 푸시마다 발행한다.

```
ghcr.io/<owner>/<repo>:latest          # 멀티 아키텍처 (arm64 + amd64)
ghcr.io/<owner>/<repo>:sha-<commit>    # 불변 — 태스크 정의에는 이걸 쓴다
```

워크플로는 여기서 끝난다. AWS 자격증명을 들지 않는다 — 이미지를 발행하는 일과 배포하는
일은 폭발 반경이 다르고, 계정에 쓸 수 있는 빌드 러너는 레지스트리에만 쓸 수 있는 러너보다
훨씬 큰 표적이다.

### 이미지를 ECS 로 가져가는 방법

GHCR 패키지가 **공개**면 ECS 가 설정 없이 끌어간다.

**비공개**면(private 저장소의 기본값) 태스크의 **실행 롤**에 레지스트리 자격증명이 필요하다.
`read:packages` 를 가진 GitHub 개인 액세스 토큰을 넣는다:

```bash
aws secretsmanager create-secret --name dbmon/ghcr \
  --secret-string '{"username":"<github-user>","password":"<read:packages PAT>"}'
```

컨테이너 정의에서 참조하고, 그 ARN 에 대한 `secretsmanager:GetSecretValue` 를 **실행 롤**에
준다(태스크 롤이 아니다 — 앱이 이미지를 끌 수 있어서는 안 된다):

```jsonc
"repositoryCredentials": {
  "credentialsParameter": "arn:aws:secretsmanager:ap-northeast-2:123456789012:secret:dbmon/ghcr-AbCdEf"
}
```

또는 릴리스마다 ECR 로 한 번 복사한다. 재빌드가 아니라 `imagetools create` 를 쓴다 —
같은 다이제스트를 옮기므로 검증한 바이트와 도는 바이트가 같다:

```bash
docker buildx imagetools create \
  -t 123456789012.dkr.ecr.ap-northeast-2.amazonaws.com/dbmon:sha-<commit> \
  ghcr.io/<owner>/<repo>:sha-<commit>
```

ECS 에는 보통 ECR 이 맞다 — 같은 계정 IAM, 이그레스 요금 없음, VPC 엔드포인트 —
그리고 풀 자격증명 문제가 아예 없어진다.

이미지는 **arm64 우선**이다(Graviton: 같은 성능에 약 20% 싸다). UID 10001 로 돌고,
컴파일러나 셸 유틸리티가 들어 있지 않고, 바이너리 자신의 `healthcheck` 서브커맨드를
부르는 `HEALTHCHECK` 가 붙어 있다 — 런타임 레이어에 `curl` 을 넣지 않기 위해서다.

로컬 빌드:

```bash
docker build -t dbmon:dev .
docker run --rm dbmon:dev --version
```

## 2. 저장소 (DynamoDB)

테이블 두 개. **태스크가 뜨기 전에 있어야 한다** — 앱이 만들지 않는다(만들려면
`dynamodb:CreateTable` 이 필요하고, 모니터링 태스크가 그 권한을 들고 있어서는 안 된다).

| 테이블 | 키 | 비고 |
|---|---|---|
| `dbmon-data` | PK `S`, SK `S` | 슬로우 쿼리, 다이제스트, 롤업, 인스턴스, 체크포인트, 튜닝 권고. TTL 속성 `ttl`(35일). |
| `dbmon-config` | PK `S`, SK `S` | 설정(`CFG/GLOBAL`), 정지 스코프, 리스. TTL 없음. |

`dbmon-data` 에는 GSI 두 개가 필요하다.

| 인덱스 | PK | SK | 용도 |
|---|---|---|---|
| `GSI1` | `GSI1PK` | `GSI1SK` | 다이제스트→최근 샘플, 진행 중 레코드 희소 스윕 |
| `GSI2` | `GSI2PK` | `GSI2SK` | 소요시간 버킷 조회 |

둘 다 PITR 을 권한다. `terraform -chdir=infra/layers/10-storage apply` 가 올바른 키
스키마로 만든다(프로바이더 6.x 가 GSI 안의 `hash_key` 를 개명했다 — 락 파일을 커밋해 둔
이유다).

**S3 오프로드는 없다.** 계획은 레코드에 인라인으로 저장된다. 마스킹된 JSON 이 150KB 를
넘으면 `too_large:<n>KB` 사유와 함께 계획만 버리고 레코드는 남긴다 — 계획이 크다는 이유로
슬로우 쿼리 자체를 잃는 쪽이 훨씬 나쁘다. (`storage.plan_bucket` 설정이 있었지만 읽는
코드가 없어 지웠다. 오프로드는 로드맵에 있다.)

### CLI 로 만들기

환경을 먼저 잡는다. 아래 절의 명령이 전부 이 변수를 쓴다.

```bash
export AWS_REGION=ap-northeast-2
export ENV=dev
export ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
```

**KMS 키** — 두 테이블을 이 키로 암호화한다.

```bash
KEY_ID=$(aws kms create-key --description "dbmon data encryption ($ENV)" \
  --query KeyMetadata.KeyId --output text)
aws kms create-alias --alias-name "alias/dbmon-data-$ENV" --target-key-id "$KEY_ID"
KEY_ARN=$(aws kms describe-key --key-id "$KEY_ID" --query KeyMetadata.Arn --output text)
echo "$KEY_ARN"
```

**`dbmon-data`** — GSI 두 개와 TTL 까지 한 번에.

```bash
aws dynamodb create-table --table-name "dbmon-data-$ENV" \
  --billing-mode PAY_PER_REQUEST \
  --sse-specification "Enabled=true,SSEType=KMS,KMSMasterKeyId=$KEY_ARN" \
  --attribute-definitions \
    AttributeName=PK,AttributeType=S AttributeName=SK,AttributeType=S \
    AttributeName=GSI1PK,AttributeType=S AttributeName=GSI1SK,AttributeType=S \
    AttributeName=GSI2PK,AttributeType=S AttributeName=GSI2SK,AttributeType=S \
  --key-schema AttributeName=PK,KeyType=HASH AttributeName=SK,KeyType=RANGE \
  --global-secondary-indexes '[
    {"IndexName":"GSI1",
     "KeySchema":[{"AttributeName":"GSI1PK","KeyType":"HASH"},
                  {"AttributeName":"GSI1SK","KeyType":"RANGE"}],
     "Projection":{"ProjectionType":"INCLUDE","NonKeyAttributes":[
       "record_id","instance_id","env","started_at_ms","duration_ms","app_digest",
       "statement_type","state","last_seen_at_ms","owner_worker","exec_count",
       "total_time_ms","severity","rule_id","owner_epoch","thread_id","abandoned_reason"]}},
    {"IndexName":"GSI2",
     "KeySchema":[{"AttributeName":"GSI2PK","KeyType":"HASH"},
                  {"AttributeName":"GSI2SK","KeyType":"RANGE"}],
     "Projection":{"ProjectionType":"INCLUDE","NonKeyAttributes":[
       "record_id","instance_id","env","started_at_ms","duration_ms","app_digest",
       "statement_type","schema_name","db_user","kind"]}}]'

aws dynamodb update-time-to-live --table-name "dbmon-data-$ENV" \
  --time-to-live-specification "Enabled=true,AttributeName=ttl"
```

> ⚠ **GSI 사영을 `ALL` 로 만들지 않는다.** `sql_text`·`plan_json` 이 인덱스에도 복제되어 쓰기
> 비용과 크기가 배가 된다. 그리고 **프로덕션에서만 나는 결함을 가린다** — 고아 정리는
> 인덱스에서 키만 읽고 본문을 기본 테이블에서 가져오는데, `ALL` 이면 그 경로를 타지 않아
> 테스트가 통과한다(실제로 그렇게 겪었다).

**`dbmon-config`** — GSI 없음.

```bash
aws dynamodb create-table --table-name "dbmon-config-$ENV" \
  --billing-mode PAY_PER_REQUEST \
  --sse-specification "Enabled=true,SSEType=KMS,KMSMasterKeyId=$KEY_ARN" \
  --attribute-definitions AttributeName=PK,AttributeType=S AttributeName=SK,AttributeType=S \
  --key-schema AttributeName=PK,KeyType=HASH AttributeName=SK,KeyType=RANGE

aws dynamodb update-time-to-live --table-name "dbmon-config-$ENV" \
  --time-to-live-specification "Enabled=true,AttributeName=ttl"
```

**PITR 과 삭제 방지** — 지우면 데이터가 사라진다.

```bash
for t in "dbmon-data-$ENV" "dbmon-config-$ENV"; do
  aws dynamodb update-continuous-backups --table-name "$t" \
    --point-in-time-recovery-specification PointInTimeRecoveryEnabled=true
  aws dynamodb update-table --table-name "$t" --deletion-protection-enabled
done
```

**만들어졌는지 확인** — 키 스키마가 틀리면 앱이 조용히 0건을 읽는다.

```bash
aws dynamodb describe-table --table-name "dbmon-data-$ENV" \
  --query 'Table.[TableStatus,KeySchema,GlobalSecondaryIndexes[].IndexName]' --output json
```


## 3. IAM — 태스크 롤

앱이 쓰는 유일한 신원이다. 문장 9개를 각각 좁혀 놓았고, 정본은
`infra/layers/40-compute/iam.tf` 다.

### 3.1 저장소

```json
{ "Sid": "DynamoDbData", "Effect": "Allow",
  "Action": ["dynamodb:GetItem","dynamodb:PutItem","dynamodb:UpdateItem","dynamodb:DeleteItem",
             "dynamodb:Query","dynamodb:BatchWriteItem","dynamodb:BatchGetItem",
             "dynamodb:DescribeTable"],
  "Resource": ["arn:aws:dynamodb:ap-northeast-2:123456789012:table/dbmon-data",
               "arn:aws:dynamodb:ap-northeast-2:123456789012:table/dbmon-data/index/*",
               "arn:aws:dynamodb:ap-northeast-2:123456789012:table/dbmon-config"] }
```

```json
{ "Sid": "DenyScan", "Effect": "Deny", "Action": ["dynamodb:Scan"], "Resource": "*" }
```

`DescribeTable` 이 있는 이유는 준비 프로브가 그걸 부르기 때문이다 — 없으면 저장소
프로브가 `AccessDenied` 로 실패하고 서비스가 준비 상태에 도달하지 못한다.

`Scan` 명시 거부는 의도적이다. 500대짜리 테이블을 한 번 잘못 스캔하면 비용 사건이면서
지연 사건이다. 코드는 스캔하지 않는데, 이 문장이 그걸 **구조로** 만든다.

### 3.2 탐색 (RDS)

```json
{ "Sid": "DiscoveryReadOnly", "Effect": "Allow",
  "Action": ["rds:DescribeDBInstances","rds:DescribeDBClusters",
             "rds:DescribeDBParameters","rds:DescribeDBParameterGroups",
             "rds:DescribeDBClusterParameters","rds:DescribeEvents",
             "rds:DescribePendingMaintenanceActions","rds:ListTagsForResource"],
  "Resource": "*" }
```

`ListMetrics` 는 **주지 않는다.** 지표 카탈로그는 코드에 있으므로
(`dbmon_core::cw_metrics`) 열거할 이유가 없다.

`Resource: "*"` 는 불가피하다 — `rds:Describe*` 는 리소스 수준 권한을 지원하지 않는다.
범위는 대신 설정에서 강제한다(`discovery.allowed_vpc_ids`, `denied_name_substrings`,
`reject_production_tags`).

### 3.3 DB 접속 (IAM 인증 — 비밀번호 없음)

```json
{ "Effect": "Allow", "Action": ["rds-db:connect"],
  "Resource": ["arn:aws:rds-db:ap-northeast-2:123456789012:dbuser:db-ABC123…/dbmon",
               "arn:aws:rds-db:ap-northeast-2:123456789012:dbuser:cluster-XYZ789…/dbmon"] }
```

**리소스 ID 를 열거한다.** `dbuser:*/dbmon` 은 "이 계정 **모든** 인스턴스의 `dbmon` 계정"
이고 프로덕션을 포함한다. Terraform 변수 `db_auth_resource_ids` 는 비워 두면 그 와일드카드로
폴백하므로, 그 값을 빠뜨린 apply 가 조용히 범위를 넓힌다.

> 여기 들어가는 것은 인스턴스 **이름이 아니다.**
>
> ```bash
> # RDS 인스턴스 → DbiResourceId (db-…)
> aws rds describe-db-instances --query 'DBInstances[].[DBInstanceIdentifier,DbiResourceId]' --output text
> # Aurora → DbClusterResourceId (cluster-…)
> aws rds describe-db-clusters  --query 'DBClusters[].[DBClusterIdentifier,DbClusterResourceId]' --output text
> ```
>
> **Aurora 멤버는 *클러스터* 리소스 ID 로 인증한다.** 멤버의 `DbiResourceId` 를 쓰면 생성한
> 토큰이 무효가 되고 `1045 Access denied` 로 실패한다 — 비밀번호가 틀렸을 때와 **같은
> 오류**라서 원인을 찾는 데 몇 시간이 든다.

### 3.4 메트릭·슬로우로그

```json
{ "Sid": "PutOwnMetrics", "Effect": "Allow",
  "Action": ["cloudwatch:PutMetricData"], "Resource": "*",
  "Condition": { "StringEquals": { "cloudwatch:namespace": "dbmon" } } }
```

```json
{ "Sid": "MetricsRead", "Effect": "Allow",
  "Action": ["cloudwatch:GetMetricData"], "Resource": "*" }
```

> ⚠ **`GetMetricData` 에 네임스페이스 조건을 걸지 않는다.** `cloudwatch:namespace` 조건 키는
> `PutMetricData` 요청에는 실려 오지만 `GetMetricData` 에는 **실려 오지 않는다.** 그래서 그
> 조건이 붙은 문은 절대 매치되지 않고 결과가 `implicitDeny` 다. 이 README 의 예전 판은 여기에
> `StringEquals { "cloudwatch:namespace": "AWS/RDS" }` 를 싣고 있었다 — 그걸 따라 설치하면
> CloudWatch 열이 영구히 비고 화면은 권한이 없다고 표시한다. `aws iam
> simulate-principal-policy` 로 확인한다(로컬 개발은 관리자로 돌기 때문에 이 함정이 보이지 않는다).
>
> 노출 범위를 정직하게 적는다: 이 롤은 계정의 **모든** CloudWatch 지표를 읽을 수 있다. IAM
> 정책으로는 좁힐 수 없고 권한 경계나 SCP 로 좁힌다. 코드가 실제로 조회하는 것은
> `dbmon_core::cw_metrics` 의 카탈로그가 전부이고 `AWS/RDS` 뿐이다.

```json
{ "Sid": "SlowLogRead", "Effect": "Allow", "Action": ["logs:FilterLogEvents"],
  "Resource": ["arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/instance/*/slowquery",
               "arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/instance/*/slowquery:*",
               "arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/cluster/*/slowquery",
               "arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/cluster/*/slowquery:*"] }
```

> ⚠ **`cluster` 형태가 Aurora 를 살린다.** Aurora 는 슬로우로그를 **클러스터 단위** 그룹
> (`/aws/rds/cluster/<클러스터>/slowquery`)에 쓰고 **멤버마다 스트림이 하나**다. RDS 는
> `/aws/rds/instance/<인스턴스>/slowquery` 다. `cluster` 형태를 빼면 Aurora 백필이 한 번도
> 돌지 않고, 그 실패는 조용하다 — 인스턴스는 수집되는데 `rows_examined` 만 계속 빈다(정확
> 지표는 슬로우로그에만 있다).

슬로우로그에는 **쿼리 리터럴이 들어 있다.** `/aws/rds/{instance,cluster}/*/slowquery` 는
`/aws/rds/*` 보다 훨씬 좁다 — 후자는 감사 로그(모든 문장이 남는다)까지 포함한다.

로그 그룹이 아직 없으면(슬로우로그가 한 번도 쓰이지 않았거나 내보내기가 꺼졌거나)
`슬로우로그 원천이 없다 … (장애가 아니다)` 를 라운드당 한 번 info 로 남기고 계속 간다 —
그룹이 없는 것은 환경의 사실이지 장애가 아니다.

### 3.5 멀티 리전

**추가 IAM 은 없다.** 같은 태스크 롤이 모든 리전에서 동작하고, 앱이 리전별 클라이언트를
만든다. "전체 리전" 옵션은 의도적으로 없다 — 리전마다 탐색 라운드당
`DescribeDBInstances` 가 한 번 나가고, 대부분의 계정은 RDS 가 한두 리전에만 있다.

**목록이 두 개이고, 서로 대체되지 않는다.**

| 목록 | 재시작 없이 반영? | 정하는 것 |
|---|---|---|
| **설정 → 탐색 범위** | 된다 (30초) | 어느 리전을 조회할지 |
| `aws.target_regions` (파일) | 안 된다 | 어느 리전에 IAM 인증·슬로우로그 클라이언트를 만들지 |

IAM 인증 토큰 공급자와 CloudWatch Logs 클라이언트는 `aws.target_regions` 로 **기동
시점에 한 번** 만들어진다. 그래서 설정에만 추가한 리전은 탐색은 되고(목록에 뜬다)
수집은 "이 인스턴스에 쓸 인증 공급자가 없다" 로 멈추며 슬로우로그는 클라이언트가 없다.
**수집할 리전은 두 곳에 모두 넣고**, 파일에 추가했으면 재시작한다. CloudWatch 지표
클라이언트만 예외다 — `(계정, 리전)` 별로 지연 생성되므로 설정에만 있는 리전도 플릿
메트릭은 나온다.

리전별로 읽는 것은 그 리전에 있어야 한다. 슬로우로그 그룹은 리전 자원이고, CloudWatch
지표는 인스턴스가 있는 리전에 있다(그래서 앱이 CloudWatch 클라이언트를 `(계정, 리전)`
으로 키잉한다).

### 3.6 멀티 계정

mgmt 계정 태스크 롤:

```json
{ "Sid": "AssumeDiscoveryRole", "Effect": "Allow", "Action": ["sts:AssumeRole"],
  "Resource": ["arn:aws:iam::111122223333:role/dbmon-discovery"] }
```

대상 계정의 `dbmon-discovery` 역할 — 신뢰 정책:

```json
{ "Version": "2012-10-17",
  "Statement": [{ "Effect": "Allow",
    "Principal": { "AWS": "arn:aws:iam::123456789012:role/dbmon-task" },
    "Action": "sts:AssumeRole",
    "Condition": { "StringEquals": { "sts:ExternalId": "dbmon" } } }] }
```

권한:

```json
{ "Version": "2012-10-17",
  "Statement": [{ "Effect": "Allow",
    "Action": ["rds:DescribeDBInstances","rds:DescribeDBClusters","rds:ListTagsForResource",
               "cloudwatch:GetMetricData"],
    "Resource": "*" }] }
```

**목록 조회는 API 다 — 네트워크 경로가 필요 없다.** VPC 피어링·TGW 는 그 **다음**
단계, 즉 DB 에 붙을 때 필요하다. 그래서 "목록에는 뜨는데 `unreachable`" 은 정상적으로
존재하는 상태이고, 화면이 그걸 구분해 보여준다.

**크로스 계정 수집은 아직 지원하지 않는다.** 탐색과 CloudWatch 메트릭은 대상 계정
역할을 맡지만, IAM DB 인증 토큰과 슬로우로그 클라이언트는 그렇지 않다 — mgmt 계정의
기본 자격증명으로 돈다. 두 경로는 이제 추측하지 않고 **거부한다**:

| 경로 | 크로스 계정 | 거부하지 않으면 |
|---|---|---|
| 탐색 (`rds:Describe*`) | **된다** (역할을 맡는다) | — |
| CloudWatch 메트릭 | **된다** (역할을 맡는다) | — |
| IAM DB 인증 (`rds-db:connect`) | 거부 | mgmt 로 서명한 토큰이 대상 DB 에 가서 "DB 계정이 없다" 로 실패한다 |
| 슬로우로그 (`logs:FilterLogEvents`) | 거부 | 그룹 이름에 계정이 없으므로 mgmt 계정의 같은 이름 인스턴스를 읽고 **그 SQL 을 대상 인스턴스의 것으로 저장한다** |

그래서 크로스 계정 인스턴스는 목록에 뜨고 메트릭도 나오지만 수집은 `unreachable` 로
남는다. 계정별 토큰 공급자·로그 클라이언트는 로드맵에 있다.

계정 목록은 **설정 → 탐색 범위**에 넣는데, `var.discovery_account_ids` 와 같아야 한다.
다르면 탐색이 `AccessDenied` 로 실패하고 그 인스턴스는 목록에 뜨지 않는다.

### 3.7 Bedrock (AI 튜닝)

```json
{ "Sid": "InvokeTuningModel", "Effect": "Allow", "Action": ["bedrock:InvokeModel"],
  "Resource": [
    "arn:aws:bedrock:ap-northeast-2:123456789012:inference-profile/global.anthropic.claude-sonnet-5",
    "arn:aws:bedrock:*::foundation-model/anthropic.claude-sonnet-5"
  ] }
```

크로스 리전 추론 프로파일에는 ARN 두 개가 필요하다 — **프로파일과 그것이 라우팅하는
기반 모델.** 프로파일만 허용하면 호출 시점에 `AccessDenied` 다.

`*` 로 열지 않고 열거하는 이유: 계정의 모델 목록에는 단가가 자릿수로 다른 이미지·비디오
모델도 들어 있다.

계정에서 쓸 수 있는 것 확인:

```bash
aws bedrock list-inference-profiles \
  --query 'inferenceProfileSummaries[?contains(inferenceProfileId,`claude`)].inferenceProfileId'
```

Bedrock 콘솔의 모델 액세스에서 해당 리전에 대해 켜져 있어야 한다.

실측으로 알아 둘 것:

- **`temperature` 를 Claude 5 계열이 거부한다**(`ValidationException: temperature is
  deprecated for this model`). 그래서 `maxTokens` 만 보낸다.
- **`SLEEP()` 이 든 쿼리는 모델이 차단할 수 있다**(`stop_reason=content_filtered`, 빈
  응답) — 시간지연 SQL 인젝션 시그니처로 읽힌다. opus-5 는 막고 sonnet-5 는 통과한다.
  화면이 그 사유를 그대로 보여주므로 모델을 바꾸면 된다.
- **Opus 계열은 출력이 길다.** 잘리면 **출력 토큰 상한**을 8000 으로 올린다.

### 3.8 알림 채널 비밀

```json
{ "Sid": "ReadChannelSecret", "Effect": "Allow", "Action": ["secretsmanager:GetSecretValue"],
  "Resource": ["arn:aws:secretsmanager:ap-northeast-2:123456789012:secret:dbmon/channel/*"] }
```

접두어로 좁힌다. `*` 면 모니터링 태스크가 계정의 모든 비밀을 읽을 수 있게 되고, 거기엔
RDS 마스터 비밀번호가 들어 있다.

### 3.9 실행 롤 (태스크 롤과 별도)

ECS **실행** 롤은 이미지를 끌고 컨테이너 로그를 쓴다:
`AmazonECSTaskExecutionRolePolicy`, ECR·로그 그룹이 CMK 를 쓰면 `kms:Decrypt` 추가.
태스크 롤과 **분리**한다 — 앱이 이미지를 끌거나 밀 수 있어서는 안 된다.

### 3.10 CLI 로 역할·정책 만들기

신뢰 정책은 두 역할이 같다.

```bash
cat > /tmp/trust-ecs-tasks.json <<'JSON'
{ "Version": "2012-10-17",
  "Statement": [{ "Effect": "Allow",
    "Principal": { "Service": "ecs-tasks.amazonaws.com" },
    "Action": "sts:AssumeRole" }] }
JSON

aws iam create-role --role-name "dbmon-$ENV-ecs-task" \
  --assume-role-policy-document file:///tmp/trust-ecs-tasks.json
aws iam create-role --role-name "dbmon-$ENV-ecs-execution" \
  --assume-role-policy-document file:///tmp/trust-ecs-tasks.json
```

**실행 역할** — 관리형 정책 + KMS·시크릿 인라인. 관리형 정책만으로는 CMK 로 암호화한 ECR 과
시크릿을 못 읽는다.

```bash
aws iam attach-role-policy --role-name "dbmon-$ENV-ecs-execution" \
  --policy-arn arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy

SECRET_ARN=$(aws secretsmanager describe-secret --secret-id "dbmon/$ENV/auth-token" \
  --query ARN --output text)

cat > /tmp/exec-inline.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "EcrKmsDecrypt", "Effect": "Allow",
    "Action": ["kms:Decrypt"], "Resource": "$KEY_ARN" },
  { "Sid": "AuthTokenSecret", "Effect": "Allow",
    "Action": ["secretsmanager:GetSecretValue"], "Resource": "$SECRET_ARN" }
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-execution" \
  --policy-name dbmon-exec-extras --policy-document file:///tmp/exec-inline.json
```

**태스크 역할 — 저장소**(§3.1). 테이블 ARN 을 열거한다.

```bash
DATA_ARN="arn:aws:dynamodb:$AWS_REGION:$ACCOUNT:table/dbmon-data-$ENV"
CFG_ARN="arn:aws:dynamodb:$AWS_REGION:$ACCOUNT:table/dbmon-config-$ENV"

cat > /tmp/storage.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "DynamoDbData", "Effect": "Allow",
    "Action": ["dynamodb:GetItem","dynamodb:BatchGetItem","dynamodb:Query",
               "dynamodb:PutItem","dynamodb:UpdateItem","dynamodb:BatchWriteItem",
               "dynamodb:DeleteItem","dynamodb:DescribeTable"],
    "Resource": ["$DATA_ARN","$DATA_ARN/index/*","$CFG_ARN","$CFG_ARN/index/*"] },
  { "Sid": "DenyScan", "Effect": "Deny", "Action": ["dynamodb:Scan"], "Resource": "*" },
  { "Sid": "KmsUse", "Effect": "Allow",
    "Action": ["kms:Decrypt","kms:GenerateDataKey","kms:DescribeKey"],
    "Resource": "$KEY_ARN",
    "Condition": { "StringLike": { "kms:ViaService": [
      "dynamodb.$AWS_REGION.amazonaws.com" ]}}}
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-storage --policy-document file:///tmp/storage.json
```

**태스크 역할 — 탐색·지표·슬로우로그**(§3.2·§3.4). `cluster` 형태를 빼면 Aurora 가 조용히
안 돈다.

```bash
cat > /tmp/discovery.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "DiscoveryReadOnly", "Effect": "Allow",
    "Action": ["rds:DescribeDBInstances","rds:DescribeDBClusters",
               "rds:DescribeDBParameters","rds:DescribeDBParameterGroups",
               "rds:DescribeDBClusterParameters","rds:DescribeEvents",
               "rds:DescribePendingMaintenanceActions","rds:ListTagsForResource"],
    "Resource": "*" },
  { "Sid": "PutOwnMetrics", "Effect": "Allow",
    "Action": ["cloudwatch:PutMetricData"], "Resource": "*",
    "Condition": { "StringEquals": { "cloudwatch:namespace": "dbmon" }}},
  { "Sid": "MetricsRead", "Effect": "Allow",
    "Action": ["cloudwatch:GetMetricData"], "Resource": "*" },
  { "Sid": "SlowLogRead", "Effect": "Allow", "Action": ["logs:FilterLogEvents"],
    "Resource": [
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/instance/*/slowquery",
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/instance/*/slowquery:*",
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/cluster/*/slowquery",
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/cluster/*/slowquery:*"] }
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-discovery --policy-document file:///tmp/discovery.json
```

**태스크 역할 — IAM DB 인증**(§3.3). 리소스 id 를 조회해서 넣는다.

```bash
# RDS 인스턴스와 Aurora 클러스터의 리소스 id 를 모은다 (필요한 것만 골라 쓴다)
aws rds describe-db-instances \
  --query 'DBInstances[].[DBInstanceIdentifier,DbiResourceId,DBClusterIdentifier]' --output table
aws rds describe-db-clusters \
  --query 'DBClusters[].[DBClusterIdentifier,DbClusterResourceId]' --output table

# 위에서 고른 id 들을 넣는다. **Aurora 는 cluster- 쪽이다.**
IDS=(db-ABC123EXAMPLE cluster-XYZ789EXAMPLE)
RES=$(printf '"arn:aws:rds-db:%s:%s:dbuser:%s/dbmon",' "$AWS_REGION" "$ACCOUNT" "${IDS[@]}")
cat > /tmp/dbauth.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Effect": "Allow", "Action": ["rds-db:connect"], "Resource": [${RES%,}] }]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-db-auth --policy-document file:///tmp/dbauth.json
```

**권장 — 자기 권한 상승 차단.** `ForAllValues` 를 쓰면 우회된다(§3.1 의 경고와 같은 부류).

```bash
cat > /tmp/deny-authz.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "DenyUserRecordWrites", "Effect": "Deny",
    "Action": ["dynamodb:PutItem","dynamodb:UpdateItem",
               "dynamodb:DeleteItem","dynamodb:BatchWriteItem"],
    "Resource": ["$CFG_ARN"],
    "Condition": { "ForAnyValue:StringLike": { "dynamodb:LeadingKeys": ["USER#*"] }}},
  { "Sid": "DenyAuditMutation", "Effect": "Deny",
    "Action": ["dynamodb:UpdateItem","dynamodb:DeleteItem","dynamodb:BatchWriteItem"],
    "Resource": ["$CFG_ARN"],
    "Condition": { "ForAnyValue:StringLike": { "dynamodb:LeadingKeys": ["AUDIT#*"] }}}
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-deny-authz-writes --policy-document file:///tmp/deny-authz.json
```

**만든 뒤 반드시 시뮬레이터로 확인한다.** 로컬 개발은 관리자로 돌기 때문에 조건 키 실수가
보이지 않는다 — §3.4 의 `GetMetricData` 함정이 그렇게 배포까지 갔다.

```bash
ROLE="arn:aws:iam::$ACCOUNT:role/dbmon-$ENV-ecs-task"

# 있어야 하는 것
aws iam simulate-principal-policy --policy-source-arn "$ROLE" \
  --action-names cloudwatch:GetMetricData rds:DescribeDBInstances logs:FilterLogEvents \
  --query 'EvaluationResults[].[EvalActionName,EvalDecision]' --output text

# 없어야 하는 것 — explicitDeny 여야 한다
aws iam simulate-principal-policy --policy-source-arn "$ROLE" \
  --action-names dynamodb:Scan --resource-arns "$DATA_ARN" \
  --query 'EvaluationResults[0].EvalDecision' --output text

# 자기 권한 상승이 막혔는가 — 섞인 배치도 거부돼야 한다
aws iam simulate-principal-policy --policy-source-arn "$ROLE" \
  --action-names dynamodb:BatchWriteItem --resource-arns "$CFG_ARN" \
  --context-entries 'ContextKeyName=dynamodb:LeadingKeys,ContextKeyType=stringList,ContextKeyValues=USER#me,CFG' \
  --query 'EvaluationResults[0].EvalDecision' --output text
```


## 4. 네트워크

| 방향 | 규칙 |
|---|---|
| 태스크 → RDS | DB 보안그룹에 **태스크 보안그룹 출처로 TCP 3306** 허용(CIDR 아님). 보안그룹 참조는 IP 가 바뀌어도 살아 있다. |
| 태스크 → AWS API | NAT 게이트웨이, 또는 인터페이스 VPC 엔드포인트: `dynamodb`(게이트웨이), `rds`, `monitoring`, `logs`, `secretsmanager`, `bedrock-runtime`, `sts`, `ecr.api`, `ecr.dkr`, 그리고 **S3 게이트웨이 엔드포인트**(ECR 이 이미지 레이어를 S3 에 두기 때문이다 — 앱 자체는 S3 를 쓰지 않는다). 엔드포인트는 NAT 데이터 요금을 없애고 트래픽을 인터넷에서 뺀다. |
| ALB → 태스크 | 타깃 그룹 8080, 헬스체크 경로 **`/readyz`**. |
| 태스크 인바운드 | ALB 보안그룹, **또는** ALB 없이 쓸 때는 관리자·VPN 보안그룹. 그 밖은 없다. |

**ALB 없이 쓰는 것이 지원되고, 그게 더 싼 기본값이다.** ALB 는 시간당 과금이다. VPN 으로
닿는 내부 도구라면 컨테이너 포트를 VPN 보안그룹에 열면 된다:

```bash
aws ec2 authorize-security-group-ingress --group-id <태스크-SG> \
  --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,\
UserIdGroupPairs=[{GroupId=<VPN-SG>,Description='from VPN to container port'}]"

# 태스크의 사설 IP
TASK=$(aws ecs list-tasks --cluster dbmon --service-name dbmon --query 'taskArns[0]' --output text)
aws ecs describe-tasks --cluster dbmon --tasks "$TASK" \
  --query 'tasks[0].attachments[0].details[?name==`privateIPv4Address`].value' --output text
```

> ⚠ **두 헬스체크를 바꿔 쓰면 안 된다.** **컨테이너** 헬스체크는 `/healthz`, **타깃 그룹**은
> `/readyz` 다. standby 워커는 살아 있으면서 `/readyz` 에 503 을 준다(F1) — 컨테이너 체크를
> `/readyz` 로 두면 ECS 가 standby 를 영원히 죽이고 다시 띄운다. 타깃 그룹을 `/healthz` 로
> 두면 준비되지 않은 워커로 트래픽이 간다.

**ALB 유휴 타임아웃은 300초 이상이어야 한다.** WebSocket 이 실시간 메트릭을 나르고, AI
튜닝 요청이 30~100초 걸린다. 기본값 60초면 UI 가 계속 재접속하고 튜닝 버튼이 504 를
받는다 — 504 는 원인을 하나도 말해 주지 않는다.

Aurora 는 발견한 인스턴스의 **라이터 엔드포인트**(클러스터 엔드포인트가 아니라 인스턴스
엔드포인트)로 붙는다. `performance_schema` 조회가 어느 노드의 것인지 정확해야 한다.

### CLI 로 보안 그룹 만들기

```bash
VPC=vpc-0123456789abcdef0

SG=$(aws ec2 create-security-group --group-name "dbmon-$ENV-task" \
  --description "dbmon ECS tasks" --vpc-id "$VPC" --query GroupId --output text)
echo "$SG"
```

**아웃바운드는 기본으로 전부 열려 있다** — 대상 RDS, AWS API, 알림 채널로 나가야 하므로 그대로
둔다. AWS 는 보안 그룹 description 에 ASCII 만 허용한다(그래서 영어다).

**인바운드 — 관리자 경로만.** ALB 를 쓰지 않을 때:

```bash
# VPN 보안 그룹에서
aws ec2 authorize-security-group-ingress --group-id "$SG" \
  --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,\
UserIdGroupPairs=[{GroupId=sg-vpn0123456789,Description='from VPN to container port'}]"

# 또는 관리자 CIDR 에서
aws ec2 authorize-security-group-ingress --group-id "$SG" \
  --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,\
IpRanges=[{CidrIp=10.99.0.0/16,Description='from admin CIDR'}]"
```

**대상 DB 인바운드 — 이건 우리 리소스가 아니다.** 대상 팀과 협의한다. CIDR 이 아니라 **SG
참조**를 쓴다 — 태스크 IP 는 배포마다 바뀌므로 CIDR 로 열면 넓게 열거나 매번 깨진다.

```bash
# 대상 인스턴스의 보안 그룹을 찾는다
aws rds describe-db-instances --db-instance-identifier <대상-인스턴스> \
  --query 'DBInstances[0].VpcSecurityGroups[].VpcSecurityGroupId' --output text

aws ec2 authorize-security-group-ingress --group-id <대상-DB-SG> \
  --ip-permissions "IpProtocol=tcp,FromPort=3306,ToPort=3306,\
UserIdGroupPairs=[{GroupId=$SG,Description='from dbmon collector'}]"
```

**확인** — 규칙이 실제로 붙었는지 본다.

```bash
aws ec2 describe-security-groups --group-ids "$SG" \
  --query 'SecurityGroups[0].IpPermissions' --output json
```


## 5. 태스크 정의

```jsonc
{
  "family": "dbmon",
  "cpu": "512", "memory": "1024",
  "requiresCompatibilities": ["FARGATE"],
  "networkMode": "awsvpc",
  "runtimePlatform": { "cpuArchitecture": "ARM64", "operatingSystemFamily": "LINUX" },
  "taskRoleArn": "arn:aws:iam::123456789012:role/dbmon-task",
  "executionRoleArn": "arn:aws:iam::123456789012:role/dbmon-exec",
  "containerDefinitions": [{
    "name": "dbmon",
    "image": "123456789012.dkr.ecr.ap-northeast-2.amazonaws.com/dbmon:sha-<commit>",
    "portMappings": [{ "containerPort": 8080, "protocol": "tcp" }],
    "environment": [
      { "name": "DBMON__DEPLOYMENT_ENV",           "value": "prd" },
      { "name": "DBMON__AWS__REGION",              "value": "ap-northeast-2" },
      { "name": "DBMON__AWS__ACCOUNT_ID",          "value": "123456789012" },
      { "name": "DBMON__STORAGE__DATA_TABLE",      "value": "dbmon-data" },
      { "name": "DBMON__STORAGE__CONFIG_TABLE",    "value": "dbmon-config" },
      { "name": "DBMON__COLLECTOR__MONITOR_DB_USER", "value": "dbmon" },
      { "name": "DBMON__COLLECTOR__LITERAL_POLICY",  "value": "masked" },
      { "name": "DBMON__DISCOVERY__ALLOWED_VPC_IDS", "value": "vpc-0123456789abcdef0" }
    ],
    "secrets": [
      { "name": "DBMON__HTTP__AUTH_TOKEN",
        "valueFrom": "arn:aws:secretsmanager:ap-northeast-2:123456789012:secret:dbmon/api-token" }
    ],
    "stopTimeout": 60,
    "logConfiguration": {
      "logDriver": "awslogs",
      "options": { "awslogs-group": "/ecs/dbmon", "awslogs-region": "ap-northeast-2",
                   "awslogs-stream-prefix": "dbmon" }
    }
  }]
}
```

`stopTimeout` 은 `http.shutdown_grace_secs`(기본 45)보다 커야 한다. 아니면 SIGKILL 이
먼저 오고 처리 중인 기록을 잃는다.

### 설정 참조

환경변수가 TOML 파일을 덮고, `__` 가 계층을 나눈다
(`DBMON__COLLECTOR__SLOW_THRESHOLD_SECS`). 잘못된 설정에는 **기동에서 실패한다** —
수집은 하는데 저장을 못 하는 반쯤 동작하는 프로세스가 안 뜨는 것보다 나쁘다.

**필수**

| 키 | 뜻 |
|---|---|
| `deployment_env` | `dev`/`stg`/`prd`. `dev` 에서는 탐색 필터가 의무가 된다. |
| `aws.region` | 배포 리전. |
| `aws.account_id` | 모든 `instance_id` 에 들어간다 — **나중에 바꿀 수 없다.** |
| `storage.data_table`, `storage.config_table` | DynamoDB 테이블 이름. |

**자주 쓰는 값**

| 키 | 기본 | 뜻 |
|---|---|---|
| `role` | `all` | `all`/`api`/`collector`/`control`. 확장할 때 쪼갠다. |
| `http.bind`, `http.port` | `0.0.0.0`, `8080` | |
| `http.shutdown_grace_secs` | `45` | ECS `stopTimeout` 보다 작아야 한다. |
| `http.deregistration_wait_secs` | `20` | 로드밸런서가 없으면 `0`. |
| `http.allow_auth_disable` | `false` | "인증 없음" 설정을 허용한다. 두 곳의 동의가 필요하다. |
| `http.auth_token` | 없음 | **공유 접속 토큰.** 없고 `off` 도 아니면 모든 요청이 401 이다. 32자 이상·공백 없음, 아니면 기동 실패. |
| `aws.target_regions` | `[region]` | 설정에 리전 목록이 없을 때의 폴백. |
| `collector.slow_threshold_secs` | `2` | 무엇을 슬로우로 볼지. |
| `collector.monitor_db_user` | `dbmon` | 자기 제외 키이기도 하다. |
| `collector.literal_policy` | `masked` | `masked`/`full`/`full_restricted`/`off`. 아래 참고. |
| `collector.backfill_secs` | `60` | 슬로우로그 백필 주기. |
| `discovery.allowed_vpc_ids` | `[]` | **`prd` 가 아닌 모든 환경에서 필수**(`dev`·`stg`·미지정) — 비어 있으면 기동 검증이 거부한다. |
| `discovery.collect_production_targets` | `false` | `prd` 태그 인스턴스 게이트. |

**리터럴 정책**은 저장된 SQL 이 값을 유지하는지를 정한다.

| 값 | 저장되는 SQL | 복사해서 실행 가능? |
|---|---|---|
| `masked` (기본) | `WHERE id = ?` | 아니오 |
| `full_restricted` | 원문 | 예, `operator` 이상만 열람 |
| `full` | 원문 | 예, `can_see_literals` 에 따름 |
| `off` | 저장하지 않음 | 아니오 |

마스킹은 **단방향**이다. 나중에 `masked` 로 조여도 이미 저장된 원문은 남고, 반대로 풀어도
이미 마스킹된 것은 복구되지 않는다. **앞으로 저장되는 레코드에만** 적용된다. 실행계획
JSON 은 이 설정과 무관하게 항상 마스킹된다.

> ⚠ **런타임 이미지에는 설정 파일이 없다.** 바이너리와 `web/dist` 만 담으므로 `environment` 에
> 없는 값은 코드 기본값이 된다. `local/dbmon-aws.toml` 은 로컬 실행에만 적용된다 — 거기에만
> 적어 둔 값은 배포에서 조용히 무시된다.
>
> 특히 물리는 것이 `DBMON__COLLECTOR__LITERAL_POLICY` 다. 기본값 `masked` 는 SQL 리터럴을 `?`
> 로 바꿔 저장하고 **되돌릴 수 없다.** 원문이 필요하면 `full_restricted` 로 둔다(operator 이상
> + 열람 시 감사 로그).
>
> 앱이 모르는 키를 주지 않는다 — 설정이 `deny_unknown_fields` 라 남는 키 하나가 무시되는 게
> 아니라 **기동을 실패시킨다.**

### CLI 로 배포하기

**ECR·시크릿·로그 그룹·클러스터** — 한 번만 만든다.

```bash
aws ecr create-repository --repository-name "dbmon-$ENV" \
  --image-tag-mutability IMMUTABLE \
  --image-scanning-configuration scanOnPush=true \
  --encryption-configuration "encryptionType=KMS,kmsKey=$KEY_ARN"

# Cognito 를 쓰지 않으면 공유 토큰이 필요하다
aws secretsmanager create-secret --name "dbmon/$ENV/auth-token" \
  --secret-string "$(openssl rand -hex 32)"

aws logs create-log-group --log-group-name "/dbmon/$ENV/app"
aws logs put-retention-policy --log-group-name "/dbmon/$ENV/app" --retention-in-days 30

aws ecs create-cluster --cluster-name "dbmon-$ENV"
```

**이미지** — 태스크가 Graviton 이므로 `--platform linux/arm64` 가 필수다.

```bash
# 버전은 `Cargo.toml` 이 정본이다 — 손으로 적으면 `--version` 과 갈린다.
TAG="v$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version *=/{gsub(/[" ]/,"");sub(/version=/,"");print;exit}' Cargo.toml)-$(git rev-parse --short HEAD)"     # `latest` 는 쓰지 않는다
REPO="$ACCOUNT.dkr.ecr.$AWS_REGION.amazonaws.com/dbmon-$ENV"

aws ecr get-login-password --region "$AWS_REGION" \
  | docker login --username AWS --password-stdin "${REPO%/*}"
docker build --platform linux/arm64 -t "$REPO:$TAG" .
docker push "$REPO:$TAG"
```

**태스크 정의 등록** — 위의 `taskdef.json` 을 채운 뒤.

```bash
aws ecs register-task-definition --cli-input-json "file://taskdef.json"
```

**서비스 생성** (처음 1회). Spot 으로 두면 비용이 크게 줄고, 이 워크로드는 중단에 견딘다.

```bash
aws ecs create-service --cluster "dbmon-$ENV" --service-name "dbmon-$ENV" \
  --task-definition "dbmon-$ENV" --desired-count 1 \
  --capacity-provider-strategy capacityProvider=FARGATE_SPOT,weight=1 \
  --network-configuration "awsvpcConfiguration={subnets=[subnet-aaa,subnet-bbb],\
securityGroups=[$SG],assignPublicIp=ENABLED}" \
  --health-check-grace-period-seconds 60
```

`assignPublicIp=ENABLED` + 퍼블릭 서브넷이면 NAT 없이 IGW 로 나간다(NAT 요금 0). 프라이빗
서브넷에 둘 때는 NAT 나 인터페이스 엔드포인트가 필요하다.

**이후 배포** (이미지만 바꿀 때).

```bash
aws ecs register-task-definition --cli-input-json "file://taskdef.json"   # 새 이미지 태그로
aws ecs update-service --cluster "dbmon-$ENV" --service "dbmon-$ENV" \
  --task-definition "dbmon-$ENV" --force-new-deployment
```

**롤아웃 확인** — `aws ecs wait services-stable` 은 실패 사유를 말해 주지 않으므로 직접 본다.

```bash
aws ecs describe-services --cluster "dbmon-$ENV" --services "dbmon-$ENV" \
  --query 'services[0].{td:taskDefinition,running:runningCount,desired:desiredCount,
                        rollout:deployments[0].rolloutState}' --output json

TASK=$(aws ecs list-tasks --cluster "dbmon-$ENV" --service-name "dbmon-$ENV" \
  --query 'taskArns[0]' --output text)
aws ecs describe-tasks --cluster "dbmon-$ENV" --tasks "$TASK" \
  --query 'tasks[0].{status:lastStatus,health:healthStatus,image:containers[0].image,
                     stopped:stoppedReason}' --output json
```

**롤백** — 이전 리비전 번호로 되돌린다. 이미지 태그가 불변이므로 리비전이 곧 이미지다.

```bash
aws ecs update-service --cluster "dbmon-$ENV" --service "dbmon-$ENV" \
  --task-definition "dbmon-$ENV:<이전-리비전>"
```


## 6. DB 계정 (인스턴스별)

비밀번호는 어디에도 저장하지 않는다. 15분 유효한 IAM 토큰으로 인증하므로 DB 계정이
`AWSAuthenticationPlugin` 을 써야 한다.

인스턴스 쪽 전제:

1. **IAM DB 인증 활성화** —
   `aws rds modify-db-instance --db-instance-identifier X --enable-iam-database-authentication`
   (Aurora 는 클러스터에).
2. **슬로우 쿼리 로그 켜고 CloudWatch Logs 로 내보내기** — 파라미터 그룹에
   `slow_query_log=1`, `long_query_time=1`(원하는 임계값), `log_output=FILE`, 그리고
   `EnableCloudwatchLogsExports=["slowquery"]`.
3. `performance_schema=1` (`db.t3.medium` 이상은 기본 켜짐).

그 다음 마스터 사용자로 접속해서:

```sql
-- 호스트 패턴은 태스크가 접속해 오는 **출처**와 맞아야 한다.
-- 태스크 IP 가 아니라 ECS 서브넷 CIDR 을 쓴다 — 배포마다 IP 가 바뀐다.
CREATE USER IF NOT EXISTS 'dbmon'@'10.1.%'
  IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS'
  REQUIRE SSL;

-- PROCESS: `information_schema.PROCESSLIST` 에서 남의 세션을 본다 — 없으면 자기
--   스레드만 보이고 탐지가 아무것도 못 찾는다.
-- SHOW VIEW: 뷰에 `SHOW CREATE TABLE` 을 하려면 필요하다(계획이 뷰를 참조한다).
GRANT PROCESS, SHOW VIEW ON *.* TO 'dbmon'@'10.1.%';

-- 메트릭·다이제스트.
GRANT SELECT ON `performance_schema`.* TO 'dbmon'@'10.1.%';

-- 실행계획과 튜닝 컨텍스트는 대상 스키마의 SELECT 가 필요하다.
-- 최소 권한: 열거한다. 넓게: GRANT SELECT ON *.* (모든 테이블의 카디널리티를 얻는다).
GRANT SELECT ON `shop`.* TO 'dbmon'@'10.1.%';

SHOW GRANTS FOR 'dbmon'@'10.1.%';
```

`REPLICATION CLIENT`·`SHOW DATABASES`·`SELECT ON sys.*` 는 전에 이 목록에 있었다.
수집기가 그것을 필요로 하는 쿼리를 하나도 던지지 않는다 — 읽는 것은
`performance_schema.{processlist,events_statements_current,events_statements_summary_by_digest,
global_status,global_variables}`, `information_schema.{PROCESSLIST,STATISTICS,TABLES}`,
그리고 `SHOW CREATE TABLE` 이다. 모니터링 계정이 침해됐을 때 보이는 범위는 일에
필요한 만큼이어야 한다.

놓치면 시간을 잡아먹는 것들:

- **리드 리플리카(`-ro`)**: 계정을 **소스**에서 만든다. 리플리카는 read-only 라
  `CREATE USER` 자체가 실패하고, 계정은 복제로 온다.
- **Aurora**: 라이터에서 만들면 리더로 전파된다.
- **`REQUIRE SSL`** 은 IAM 인증에서 선택이 아니다 — 토큰이 비밀번호 자리로 간다.
- IAM 토큰은 **실제 엔드포인트**로 서명된다. 로컬 개발에서 SSM 터널을 쓰면 서명은
  엔드포인트로 하고 TCP 주소만 돌린다(`collector.target_endpoint_overrides`, dev +
  루프백만).
- Client VPN 은 출처 NAT 를 한다 — DB 는 VPN 클라이언트 CIDR 이 아니라 **서브넷 ENI**
  주소를 본다. 서브넷 패턴(`10.1.%`)으로 GRANT 하면 ECS 태스크와 노트북이 함께 덮인다.

전체 경로와 권한 모드·검증은 [`.claude/docs/22-onboarding-a-db.md`](../.claude/docs/22-onboarding-a-db.md) 에
있다.

## 7. 화면 설정

서비스가 정상이면 UI 를 열고 **설정**(오른쪽 위 톱니)에서 마무리한다.

| 절 | 넣을 것 |
|---|---|
| **탐색 범위** | 조회할 리전, 그리고 mgmt 모드면 계정(12자리 ID + 역할 **이름**, ARN 아님). |
| **로그인** | `token` / `cognito`(현재는 설정만) / `off`(`http.allow_auth_disable` 필요). **토큰 값은 여기서 넣지 않는다** — 아래 참고. |
| **AI 튜닝** | 활성화, 모델 ID, Bedrock 리전, 출력 토큰 상한. |
| **알림** | Slack 방식, 채널, Secrets Manager **참조**, 미리보기가 붙은 문구 템플릿. |

설정은 DynamoDB(`CFG/GLOBAL`)에 저장되고 30초 안에 모든 워커에 반영된다. 낙관적 잠금이
있어 두 관리자가 서로의 변경을 조용히 덮지 않는다. 자세한 실패 의미는
[`.claude/docs/23-settings.md`](../.claude/docs/23-settings.md).

그 다음 **RDS** 탭에서 인스턴스마다 **수집 시작**을 누른다. 등록은 자동이고 시작은
아니다 — 수집은 대상 DB 에 1초마다 쿼리를 던지는 일이고, 그건 사람이 결정해야 한다.

### 인증, 구체적으로

비밀번호로 로그인하는 화면은 없다. 들어오는 방법이 넷이고, 어느 것이 적용되는지는
배포에서 유도된다 — `GET /api/auth/config` 가 그걸 알려주고 화면의 안내 문구가 따라간다.

| 모드 | 자격증명 | 언제 |
|---|---|---|
| `local-dev` | 없음 | `dev` **이고** 루프백 바인드. 둘 다여야 하므로 `0.0.0.0` 으로 연 dev 배포는 공짜로 통과하지 않는다. |
| `local-token` | 기동 로그에 찍히는 무작위 토큰 | `dev`, 비루프백, **비ECS**. 루프백 바인드가 불가능한 `docker run` 용. |
| `shared-token` | 배포 설정의 `http.auth_token` | **ECS 경로가 이것이다.** |
| `off` | 없음 — 누구나 admin | `http.allow_auth_disable = true` **이고** 설정에서 `auth.mode = off`. |

실제 배포가 쓰는 것은 `shared-token` 이다:

```bash
openssl rand -hex 32
```

태스크 정의의 `secrets:` 로 `DBMON__HTTP__AUTH_TOKEN` 에 주입한다(§5). 그러면 정의에는
Secrets Manager ARN 만 남는다. 32자 미만이거나 공백이 섞이면 **기동에서 실패한다** —
약한 토큰은 인증이 있다는 착각을 만들어 없는 것보다 나쁘고, 공백은 헤더에서 잘려
"맞는 토큰인데 401" 이 된다.

화면을 열면 안내가 토큰을 묻는다 — **칸에 붙여넣는다.** `sessionStorage` 로 들어가고,
붙여넣기는 HTTP 요청을 만들지 않는다.

`https://<alb>/?token=<토큰>` 도 되지만 배포에서는 틀린 선택이다. 주소창에서 지워지는
것은 **첫 요청이 나간 뒤**이고, 그 시점에 ALB 액세스 로그는 이미 그 값을 기록했다.
그 로그가 만료 없는 admin 자격증명을 들고 있게 된다. URL 방식은 로그가 로컬에만 남는
dev 컨테이너용으로 남겨 둔다.

**이 토큰은 admin 이다.** 토큰 하나에는 주체가 없어 역할을 나눌 근거가 없다 — 가진
사람은 설정을 바꾸고 인증을 끌 수도 있다. 감사 로그에는 `subject=shared-token` 으로
남아 `local-dev`·`anonymous` 와 구분된다. 사람별 역할은 Cognito 가 할 일이다.

**토큰도 없고 `off` 도 아니면 모든 API 요청이 401 이다.** 기동 로그가 정확히 그 사실을
경고한다 — 맨 `401` 은 "설정 한 줄이 없다" 를 말해 주지 못한다.

### Cognito — 있는 것과 없는 것

설정 화면은 풀 ID·클라이언트 ID·리전·호스팅 UI 도메인을 저장한다. 전부 로그인 전에
브라우저가 알아야 하는 공개 값이고, 그래서 `/api/auth/config` 가 인증 없이 답한다.
시크릿을 쓰는 앱 클라이언트는 넣지 않는다 — SPA 는 시크릿을 지킬 수 없다.

**검증기가 배선되지 않았다**(`crates/dbmon/src/api/auth.rs` 의 `COGNITO_READY = false`).
그래서 `cognito` 를 고르면 공유 토큰 경로로 떨어진다 — 토큰을 넣어 둔 배포는 계속
동작하고, 넣지 않은 배포는 401 이다. 스텁으로 통과시키지 않는다. 그건 인증이 있는
것처럼 보이면서 없는 상태다.

남은 일, 순서대로:
`https://cognito-idp.<리전>.amazonaws.com/<풀>/.well-known/jwks.json` 에서 JWKS 조회,
`kid` 키 캐시, RS256 서명 검증과 `iss`·`aud`·`exp`·`token_use` 확인, 그리고
`AuthContext::intersect` 로 클레임을 역할에 매핑(그룹 → 역할). `CognitoSettings` 는 그
단계들이 필요한 값을 이미 다 담고 있으므로 남은 것은 설정이 아니라 코드다.

## 8. 확인

```bash
ALB=http://<alb>
# `/api` 아래는 전부 토큰이 필요하다. `/healthz`·`/readyz` 는 아니다 — 인증 없이 답하는
# 세 경로 중 둘이다(나머지는 `/api/auth/config`).
AUTH="Authorization: Bearer $DBMON_TOKEN"

# 헬스
curl -s $ALB/healthz && curl -s $ALB/readyz

# 어느 인증 방식이 실제로 적용됐는가
curl -s $ALB/api/auth/config | jq '.mode'

# 탐색이 인스턴스를 찾았는가
curl -s -H "$AUTH" $ALB/api/instances | jq 'length, .[0].state'

# 플릿 메트릭 (failed_scopes 가 비어 있어야 한다)
curl -s -H "$AUTH" $ALB/api/metrics/fleet | jq '.failed_scopes, (.rows | length)'

# 수집기가 리더이고 돌고 있는가
curl -s -H "$AUTH" $ALB/api/collector/status | jq '{is_leader, collecting, last_tick_ms}'
```

`.mode` 가 `unconfigured` 면 여기서 멈춘다 — 자격증명이 없어서 `/api` 는 전부 401 이다.
`http.auth_token`(§5)을 넣고 다시 배포한다.

처음에 흔한 실패:

| 증상 | 원인 |
|---|---|
| 인스턴스가 계속 `pending` | **수집 시작**을 아무도 누르지 않았다(설계다). |
| `unreachable` | 보안그룹, 또는 그 인스턴스에 `dbmon` 계정·IAM 인증이 없다. |
| 메트릭 열이 전부 `—` 이고 `failed_scopes` 가 찬다 | CloudWatch 권한, 또는 크로스 계정 역할에 `cloudwatch:GetMetricData` 가 없다. |
| `collecting` 인데 슬로우 쿼리가 안 잡힌다 | 슬로우로그가 CloudWatch Logs 로 안 나가거나, `long_query_time` 이 트래픽보다 높다. |
| 튜닝 버튼이 `model_failed` | 사유가 버튼 옆에 찍힌다 — 모델 ID, 리전 모델 미활성, 콘텐츠 필터. |

### 헬스 신호가 덮는 것과 덮지 않는 것

| 신호 | 뜻 | 언제 실패하는가 |
|---|---|---|
| `GET /healthz` | 프로세스가 HTTP 에 답한다 | 없다 — 항상 200 이다 |
| `GET /readyz` | 이 워커가 트래픽을 받아야 하는가 | 종료 중, 설정 미로드, 저장소 불가, KMS 거부, 또는 (api+수집 겸임 워커라면) 수집 리더가 아님 |
| `readyz.collect_stale` | **수집이 멈췄다** | 수집해야 하는 워커에서 5분간 성공한 tick 이 없다 |
| `GET /api/collector/status` | 왜 그런가 | 인스턴스별 상세 |

ECS 컨테이너 헬스체크가 `/healthz` 를 쓰는 것은 **의도적이다.** standby 워커는 설계상
`/readyz` 에 503 을 준다(FR-OPS-08 active/standby) — 컨테이너 체크를 `/readyz` 로 두면
ECS 가 standby 를 영원히 죽이고 다시 띄운다. `/readyz` 를 쓰는 것은 ALB 대상 그룹이다.

**`collect_stale` 은 `ready` 를 내리지 않는다.** 내리면 로드밸런서가 태스크를 빼고 ECS 가
교체하는데, 수집 실패의 원인은 보통 환경(대상 DB 접속 불가·IAM)이라 교체해도 낫지 않고
**무한 교체**가 된다. 그건 반응하려던 장애보다 나쁘다. 그래서 값으로 드러내고 경보는
바깥에 맡긴다.

**자체 CloudWatch 지표를 발행하는 코드는 아직 없다.** IAM 정책에 `PutMetricData` 가 있지만
부르는 곳이 없어서 `collect_stale` 에 대한 CloudWatch 경보가 없다 — 이미 쓰는 신서틱
체크에서 `/readyz` 를 폴링한다. FR-OPS-09 지표 발행은 로드맵에 있다.

## 9. 안 될 때

아래 표의 모든 줄은 실제 배포에서 겪은 것이다.

| 증상 | 원인 | 볼 곳 |
|---|---|---|
| 태스크가 즉시 종료 | x86 이미지 (태스크는 Graviton/arm64) | 태스크 정지 사유 |
| 기동 즉시 설정 오류로 종료 | 앱이 모르는 env 키 (`deny_unknown_fields`) | 로그 첫 줄 |
| prd 아닌 환경에서 기동 거부 | `DBMON__DISCOVERY__ALLOWED_VPC_IDS` 가 비었다 (T-37) | 로그 |
| 모든 요청이 401 | 공유 토큰이 주입되지 않았다 | `GET /api/auth/config` 의 `mode` |
| SQL 이 전부 `?` | `LITERAL_POLICY` 를 안 줘서 코드 기본값 `masked` | 레코드의 `literal_policy` |
| CloudWatch 열이 전부 `—` | `GetMetricData` 에 네임스페이스 조건 (§3.4) | `aws iam simulate-principal-policy` |
| Aurora 만 `rows_examined` 가 빈다 | 슬로우로그 IAM 에 `cluster` 형태가 없다 (§3.4) | 백필 로그의 `no_source_instances` |
| Tuning 이 `AccessDenied` | `bedrock:InvokeModel` 이 없거나 그 모델이 열거되지 않았다 | 화면에 뜨는 오류 원문 |
| IAM DB 인증이 `1045` | Aurora 멤버의 `DbiResourceId` 를 클러스터 것 대신 썼다 (§3.3) | 정책의 `dbuser:` 값 |
| 대상 DB 접속 타임아웃 | 대상 DB 보안그룹에 태스크 보안그룹 인바운드가 없다 | §4 |
| standby 워커가 계속 재시작 | **컨테이너** 헬스체크가 `/readyz` 를 본다 | §4 |
| 커서 페이지네이션이 1페이지에서 멈춘다 | 예전 빌드 — 커서의 필터 해시에 해석된 시간 구간이 들어 있었다 | 응답의 `next_cursor` |

알아 둘 진단 두 개:

```bash
curl -s "$URL/readyz" | jq       # config_loaded / storage_ok / kms_denied / auth_mode_supported
aws iam simulate-principal-policy --policy-source-arn <태스크-롤-ARN> \
  --action-names cloudwatch:GetMetricData rds:DescribeDBInstances logs:FilterLogEvents \
  --query 'EvaluationResults[].[EvalActionName,EvalDecision]' --output text
```

**시뮬레이터가 권한 문제를 볼 수 있는 유일한 방법인 경우가 있다.** 로컬 개발은 관리자
자격증명으로 돌기 때문에 조건 키 실수가 보이지 않는다.
