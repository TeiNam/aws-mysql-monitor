# 18. 개발계(dev) 배포 설계

> **⚠ 두 가지가 바뀌었다** (2026-08-20)
> 1. **배포가 ECS Fargate 다** ([ADR-022](03-decisions.md)). 이 문서의 EC2 + SSM 포트
>    포워딩 서술은 컨테이너 기준으로 읽는다 — dev 접근은 `aws ecs execute-command`
>    (ECS Exec) 로 하고, 포트 포워딩이 필요하면 SSM 의 ECS 타깃을 쓴다.
>    **네트워크 비용 $0 는 그대로다**: 퍼블릭 서브넷 + `assign_public_ip=true` 로
>    IGW 를 통해 나가므로 NAT 가 필요 없고, `enable_alb=false` 로 ALB 도 만들지 않는다.
> 2. **M0~M5 는 배포조차 필요 없다.** 로컬 `cargo run` 이 `10-foundation` 의 실제
>    DynamoDB 와 `60-seed` 의 실제 MySQL 에 붙는다. `40-compute` 는 상시 가동이
>    필요해지는 M6 부터 올린다.
>
> Terraform 실제 구성은 [`infra/`](../infra/README.md) 가 정본이다.

이 문서는 **실제 계정을 조사해 작성했다.** 추정이 아니라 측정값이다.
조사 시점: 2026-08-19 / 프로파일 `teinam-primary-123456789012`

## 1. 계정 실측 현황

```
Account   123456789012
Region    ap-northeast-2 (서울)  ← 16-cost.md 의 단가 기준과 일치
Role      AWSReservedSSO_AdministratorAccess (SSO)
Alias     없음
```

### 1.1 ⚠ 이 계정은 순수 개발 계정이 아니다

| VPC | CIDR | 용도 | 상태 |
|---|---|---|---|
| `vpc-0123456789abcdef0` | 10.1.0.0/16 | **`dev-vpc-01`** | 서브넷 6개, NAT 삭제됨 |
| `vpc-0d2b86f3fd1ea4650` | 10.3.0.0/16 | **`prd-lla-vpc`** | NAT GW 2개 가동, **EC2 `c8g.large` 1대 실행 중** |
| `vpc-0b732a68c644eba3c` | 172.31.0.0/16 | default | 미사용 |

**프로덕션 워크로드가 같은 계정에서 돌고 있다.** 이 사실이 설계에 세 가지를 강제한다.

1. **계정 전역 설정을 건드리지 않는다** — S3 계정 레벨 퍼블릭 차단, GuardDuty, Config 규칙,
   default VPC, 계정 레벨 CloudTrail, SCP는 Terraform 관리 대상에서 제외한다.
   dev 배포가 prd에 영향을 줄 수 있는 유일한 경로다.
2. **모든 리소스에 `dev` 접두어와 태그를 붙인다** — 계정에 `dev`/`prod` 표식이 없으므로
   리소스 이름만으로 구분해야 한다.
3. **IAM 스코핑을 좁힌다** — 우리 설계의 `rds:DescribeDBInstances` `Resource: "*"` 와
   `rds-db:connect` on `dbuser:*/dbmon` 는 이 계정에서 **prd 인스턴스까지 커버한다.**
   → [§6](#6-prd-혼재-계정에서의-격리) 참조.

**네트워크는 완전 격리되어 있다** (실측): VPC 피어링 0개, Transit Gateway 0개.
`dev-vpc-01` ↔ `prd-lla-vpc` 사이에 경로가 없다. **이게 1차 방어선이다.**
피어링이나 TGW를 추가하면 그 방어선이 사라지므로, 추가하려면 IAM 스코핑을 먼저 강화한다.

### 1.2 `dev-vpc-01` 구성

| 서브넷 | CIDR | AZ | 아웃바운드 |
|---|---|---|---|
| `dev-vpc-priv-subnet-a` (`subnet-0f0805f3b0c916463`) | 10.1.16.0/20 | 2a | **blackhole** |
| `dev-vpc-priv-subnet-b` (`subnet-02eca20bcc7a4bff9`) | 10.1.32.0/20 | 2b | **blackhole** |
| `dev-vpc-pub-subnet-a` (`subnet-092f5ab6edf079a59`) | 10.1.48.0/20 | 2a | IGW ✓ |
| `dev-vpc-pub-subnet-b` (`subnet-0954a70fa1ebf1375`) | 10.1.64.0/20 | 2b | IGW ✓ |
| `dev-vpc-db-subnet-a` (`subnet-04669c82c04e56f49`) | 10.1.80.0/24 | 2a | 없음 (DB 전용) |
| `dev-vpc-db-subnet-b` (`subnet-063033553c53f6983`) | 10.1.81.0/24 | 2b | 없음 (DB 전용) |

**프라이빗 서브넷의 `0.0.0.0/0` 라우트가 `blackhole`이다.**
NAT Gateway가 삭제됐고 라우트만 남아 있다(`nat-0ba866d09af78700f`, `nat-0b33b2b36457c2a32` —
`describe-nat-gateways`에 존재하지 않는다). 즉 프라이빗 서브넷에서 아웃바운드 인터넷이 불가하다.

**이미 있는 것** (이걸 활용하면 NAT가 불필요하다)

| 리소스 | ID | 의미 |
|---|---|---|
| Internet Gateway | `igw-0db498c5d387a2b8d` | 퍼블릭 서브넷 정상 |
| S3 게이트웨이 엔드포인트 | `vpce-03eb8b8a5dbfad810` | 무료. priv 라우트 테이블에 연결됨 |
| SSM 인터페이스 엔드포인트 | `ssm`, `ssmmessages`, `ec2messages` | **NAT 없이 Session Manager 접속 가능** |
| RDS 서브넷 그룹 | `dev-rds-subnet-group` (db-subnet-a/b) | 시드 MySQL 바로 생성 가능 |

**없는 것**: DynamoDB 게이트웨이 엔드포인트(무료인데 없다 — 추가한다),
NAT, RDS/CloudWatch/Secrets/Athena/Glue/Bedrock/Cognito/STS 인터페이스 엔드포인트.

### 1.3 데이터·AI 계층 (설계 전제 검증 결과)

| 항목 | 실측 | 설계 영향 |
|---|---|---|
| **RDS 인스턴스** | **전 리전 0개** (ap-northeast-2 / us-east-1 / ap-northeast-1 / ap-southeast-1) | **모니터링 대상이 없다** → 시드 MySQL 필요 ([§5](#5-시드-mysql--모니터링-대상-만들기)) |
| **Aurora 클러스터** | **0개** | 동일 |
| **S3 Tables** | **사용 가능.** `dev-test-table-bucket-01` 존재 (2025-09-17 생성), 네임스페이스 `my_stock`·`test_skt_bss_nova_s3_table`, 테이블 `my_stock.news` | [OPEN-Q-03](OPEN-QUESTIONS.md) 리전 가용성 **해소** |
| **Glue 페더레이션** | `s3tablescatalog` 카탈로그 등록됨 | 동일 |
| **카탈로그 이름 형식** | **`<account>:s3tablescatalog/<table-bucket-name>`** 로 확정 | [04 §4.3.2](04-data-model.md)의 3파트 이름이 맞다 |
| **Athena** | engine version **3** (`primary` 워크그룹) | Iceberg DML(`MERGE INTO`/`DELETE`) 지원 |
| **Glue 데이터베이스** | 0개 | `dbmon_raw` 신규 생성 |
| **DynamoDB 테이블** | **0개** | 이름 충돌 없음 |
| **Cognito User Pool** | **0개** | 이름 충돌 없음 |
| **S3 버킷 `dbmon-*`** | 없음 | 이름 충돌 없음 |
| **Terraform state 버킷** | **없음** | `00-bootstrap` 레이어에서 생성 |
| **Route53** | `teinam.com`, `english-note.com` (public) | 도메인 사용 가능 |
| **ACM (ap-northeast-2)** | `cvpn.teinam.com` 등 5건. `dbmon*` 없음 | dev는 인증서 불필요 ([§4](#4-dev-접근-모델--alb-도-nat-도-쓰지-않는다)) |
| **Bedrock** | Claude 다수 가용 (`claude-opus-5`, `claude-sonnet-5`, `claude-haiku-4-5` …) | 모델 선택 가능 |
| **Bedrock 추론 프로파일** | `global.*` / `apac.*` 존재 | 크로스 리전 추론 사용 가능 |

**카탈로그 이름 검증 근거**
```
$ aws glue get-databases --catalog-id "123456789012:s3tablescatalog"
  → EntityNotFoundException: The specified bucket does not exist

$ aws glue get-databases --catalog-id "123456789012:s3tablescatalog/dev-test-table-bucket-01"
  → my_stock, test_skt_bss_nova_s3_table   ✓
```
즉 Athena 3파트 이름은 `"s3tablescatalog/<bucket>"."<namespace>"."<table>"` 이다.

## 2. Terraform 레이어 구조 — 앱 코드와 독립

**요구사항: 인프라 배포가 애플리케이션 코드와 독립적으로 동작해야 한다.**
그래서 레이어를 나누고 **각 레이어에 독립 state**를 둔다. 앱 바이너리가 존재하지 않아도
레이어 00~30·50·60을 apply할 수 있고, 그것만으로 의미가 있다.

```
infra/
  layers/
    00-bootstrap/       # TF state 백엔드 자체 (S3 + DynamoDB 락)
    10-foundation/      # 네트워크 조회, 엔드포인트, KMS, S3 버킷, DynamoDB 테이블
    20-data/            # S3 Tables 네임스페이스·테이블, Glue DB, Athena 워크그룹
    30-identity/        # Cognito, IAM Role·정책
    40-compute/         # ALB(prd만), ASG, Launch Template
    50-observability/   # 로그그룹, 메트릭 필터, 알람, SNS, Budgets
    60-seed/            # dev 전용: 시드 MySQL + 부하 생성기
  modules/              # 레이어들이 공유하는 모듈
  envs/
    dev.tfvars
    prd.tfvars
```

| 레이어 | 앱 코드 필요? | 단독 apply 시 얻는 것 |
|---|---|---|
| `00-bootstrap` | ✗ | state 백엔드. **local state로 1회만 apply** 후 이후 레이어가 사용 |
| `10-foundation` | ✗ | DynamoDB 테이블·S3 버킷·KMS 키·게이트웨이 엔드포인트. **여기까지 하면 로컬 개발이 실제 AWS 저장소를 쓸 수 있다** |
| `20-data` | ✗ | Iceberg 테이블·Athena 워크그룹. Athena 콘솔에서 직접 쿼리 가능 |
| `30-identity` | ✗ | Cognito User Pool·초기 admin·IAM Role. **로컬 개발에서 실제 로그인 플로우 테스트 가능** |
| `40-compute` | 있으면 좋음 | ASG·Launch Template. `desired_capacity=0`으로 apply하면 인프라만 생기고 인스턴스는 안 뜬다 |
| `50-observability` | ✗ | 로그 그룹·알람·예산. 앱이 로그를 쓰기 시작하면 바로 동작 |
| `60-seed` | ✗ | 시드 MySQL. **모니터링 대상이 생긴다** |

**닭·달걀 문제가 레이어 분리로 해소된다.** 이전 설계([14 §6.4](14-infrastructure.md))는
"ASG가 아티팩트를 못 찾아 부팅 루프"를 `asg_desired=0` 트릭으로 다뤘다.
레이어를 나누면 그 트릭이 불필요하다 — `40-compute`를 나중에 apply하면 된다.

### 2.1 레이어 간 참조

```hcl
# 10-foundation/backend.tf
terraform {
  backend "s3" {
    bucket         = "dbmon-tfstate-123456789012"
    key            = "dev/10-foundation.tfstate"
    region         = "ap-northeast-2"
    dynamodb_table = "dbmon-tflock"
    encrypt        = true
  }
}

# 20-data/main.tf — 앞 레이어 출력을 읽는다
data "terraform_remote_state" "foundation" {
  backend = "s3"
  config = {
    bucket = "dbmon-tfstate-123456789012"
    key    = "dev/10-foundation.tfstate"
    region = "ap-northeast-2"
  }
}
```

`terraform_remote_state`를 쓰는 이유: 레이어가 7개뿐이고 의존이 **단방향**이다.
SSM Parameter Store로 느슨하게 결합하는 것이 더 깔끔하지만 배선이 늘어난다.
→ 레이어가 늘거나 팀이 커지면 그때 전환한다.

**state 파일에는 민감 정보가 들어간다**(Cognito client_id, KMS 키 ARN 등).
state 버킷을 SSE-KMS + 버전 관리 + 퍼블릭 차단으로 만들고, 읽기 권한을 통제한다.

### 2.2 기존 네트워크는 조회만 한다 (관리하지 않는다)

`dev-vpc-01`은 이 프로젝트가 만든 것이 아니다. Terraform이 관리하면 `destroy` 시
다른 워크로드를 지울 수 있다.

```hcl
# 10-foundation/network.tf — data source 로만 참조
data "aws_vpc" "dev" {
  filter { name = "tag:Name"  values = ["dev-vpc-01"] }
}
data "aws_subnets" "public" {
  filter { name = "vpc-id" values = [data.aws_vpc.dev.id] }
  filter { name = "tag:Name" values = ["dev-vpc-pub-subnet-*"] }
}
data "aws_subnets" "db" {
  filter { name = "vpc-id" values = [data.aws_vpc.dev.id] }
  filter { name = "tag:Name" values = ["dev-vpc-db-subnet-*"] }
}
```

**이 프로젝트가 `dev-vpc-01`에 추가하는 것은 두 개뿐이다.**
1. DynamoDB 게이트웨이 엔드포인트 (무료, priv·pub 라우트 테이블에 연결)
2. 보안 그룹 (`dbmon-dev-app-sg`, `dbmon-dev-seed-rds-sg`)

**blackhole 라우트는 건드리지 않는다.** 우리가 만든 것이 아니고, 고치면 다른 워크로드의
동작이 바뀔 수 있다. dev 앱은 퍼블릭 서브넷을 쓴다([§4](#4-dev-접근-모델--alb-도-nat-도-쓰지-않는다)).

## 3. 점진적 적용 — "개발하다 필요하면 올린다"

레이어가 독립이므로 필요한 시점에 필요한 레이어만 apply한다.

| 개발 단계 | 필요한 레이어 | 얻는 것 |
|---|---|---|
| M0 (골격·로컬 개발) | `00` + `10` | 로컬 앱이 **실제 DynamoDB**에 쓴다. docker-compose의 DynamoDB Local을 대체할 수 있다 |
| M1 (검증 스파이크) | `+ 60-seed` | 실제 MySQL 8.4 인스턴스에 대고 `EXPLAIN FOR CONNECTION`·`information_schema.PROCESSLIST` 절단 여부를 측정 |
| M2 (RDS 탐색) | (추가 없음) | `10`의 IAM Role로 탐색. 시드 인스턴스가 목록에 나타난다 |
| M4 (수집기) | `+ 20-data` | 다이제스트 롤업이 쌓이고 Athena로 조회 |
| M5 (인증·웹) | `+ 30-identity` | Cognito 로그인. 로컬 SPA가 실제 User Pool로 인증 |
| M6 이후 (EC2 배포) | `+ 40-compute` `+ 50-observability` | 상시 가동 |

**로컬 개발이 실제 AWS 저장소를 쓰는 것이 이 구조의 핵심 이득이다.**
DynamoDB Local·모의 S3로는 잡히지 않는 문제(항목 크기 한도, GSI 사영, TTL 동작,
증분 내보내기 포맷)를 M0부터 만난다.

```bash
# 로컬 개발 (10-foundation 만 apply 한 상태)
export AWS_PROFILE=teinam-primary-123456789012
export DBMON_ENV=dev
export DBMON_TABLE_PREFIX=dbmon-dev
cargo run -- serve --role api
# → 실제 DynamoDB·S3·RDS API 를 쓰고, 웹은 localhost:8080
```

## 4. dev 접근 모델 — ALB도 NAT도 쓰지 않는다

**문제** — 프라이빗 서브넷은 blackhole이고, Cognito 콜백 URL은 HTTPS를 요구한다
(예외: `http://localhost`). 순진하게 하면 NAT($43/월) + ALB($16/월) + ACM이 필요하다.
dev 전체 예산이 $60 수준이어야 하는데 네트워크에만 $59를 쓴다.

**해법 — 퍼블릭 서브넷 배치 + SSM 포트 포워딩**

```
개발자 브라우저
   │  http://localhost:8080
   ▼
SSM 포트 포워딩 세션  (aws ssm start-session)
   │
   ▼
EC2 (dev-vpc-pub-subnet-a, 퍼블릭 IP, 인바운드 규칙 0개)
   │  아웃바운드만 IGW 경유
   ▼
AWS API (RDS / DynamoDB / CloudWatch / Athena / Bedrock / Cognito …)
   │  + dev-vpc-db-subnet 의 시드 MySQL (VPC local 라우트)
```

```bash
aws ssm start-session \
  --profile teinam-primary-123456789012 \
  --target i-xxxxxxxx \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["8080"],"localPortNumber":["8080"]}'
# → 브라우저에서 http://localhost:8080
```

| 항목 | 값 | 근거 |
|---|---|---|
| ALB | **없음** | SSM 포워딩으로 접근. $16/월 절감 |
| ACM 인증서 | **없음** | `http://localhost`는 Cognito가 허용하는 유일한 HTTP 콜백 |
| Route53 레코드 | **없음** | |
| WAF | **없음** | 인바운드가 0이므로 방어할 대상이 없다 |
| NAT | **없음** | 퍼블릭 IP + IGW로 아웃바운드. $43/월 절감 |
| 인터페이스 엔드포인트 | **없음** | 8종이면 $76/월. NAT보다 비싸다 |
| 게이트웨이 엔드포인트 | S3(기존) + **DynamoDB(추가)** | 무료 |
| 보안 그룹 인바운드 | **규칙 0개** | SSM은 아웃바운드로 동작한다 |
| SSH 키페어 | **없음** | SSM Session Manager |
| Cognito 콜백 URL | `http://localhost:8080/auth/callback` | **검증 필요** — 아래 |

**네트워크 비용 $0.**

**⚠ 이 설계는 "Cognito가 `http://localhost` 콜백을 허용한다"에 의존한다.**
Cognito App Client의 콜백 URL은 HTTPS를 요구하고 `http://localhost`만 예외인 것으로 알고 있으나,
이 프로젝트의 dev 접근 모델 전체가 여기에 걸려 있으므로 **`30-identity` apply 시 첫 확인
항목으로 둔다**(M0-13g). 포트 번호가 포함된 `http://localhost:8080`도 허용되는지 함께 본다.

**허용되지 않으면** 세 가지 폴백이 있다.
1. `dbmon-dev.teinam.com` + ALB + ACM (Route53 존을 이미 보유) → **+$16/월**
2. 로컬에서 자체 서명 인증서로 HTTPS 프록시(caddy/mkcert) → `https://localhost:8443` 사용.
   비용 0이지만 개발자 로컬 셋업이 늘어난다
3. dev에서 Cognito를 비활성하고 인증 우회 모드를 쓴다 → **채택하지 않는다.**
   [15 §1](15-testing.md)의 "테스트를 위해 프로덕션 스위치를 만들지 않는다" 원칙 위반이고,
   인증·인가는 dev에서 가장 많이 깨지는 부분이라 오히려 켜둬야 한다.

→ 1번이 안전하다. 폴백 시 dev 비용은 $59 → $75.

**⚠ prd와 토폴로지가 다르다** — prd는 프라이빗 서브넷 + ALB + NAT다.
dev에서 검증한 것이 prd에서 다르게 동작할 수 있는 지점:
- ALB 헬스체크·스티키·유휴 타임아웃·등록 해제 지연 → dev에서 검증 불가
- WAF 규칙 → dev에서 검증 불가
- VPC 엔드포인트 경유 시의 IAM 조건(`aws:SourceVpce`) → dev에서 검증 불가
- 프라이빗 서브넷에서의 DNS 해석·NAT 경유 지연 → dev에서 검증 불가

→ 이 4개는 **M12 릴리스 전 스테이징에서 확인**하거나, dev에 임시로 ALB 레이어를 apply해
검증하고 destroy한다(레이어가 독립이므로 가능하다). 로드맵에 태스크로 남긴다.

**`AssociatePublicIpAddress=true`를 Launch Template에 명시해야 한다** —
`dev-vpc-pub-subnet-*`는 `MapPublicIpOnLaunch=False`다(이름은 pub인데 자동 할당이 꺼져 있다).
명시하지 않으면 퍼블릭 IP가 없어 아웃바운드가 막힌다.

**IMDSv2는 dev에서도 강제한다.** 퍼블릭 IP가 붙으므로 오히려 더 중요하다.

## 5. 시드 MySQL — 모니터링 대상 만들기

**전 리전에 RDS가 0개다.** 관측할 대상이 없으면 이 프로젝트를 개발할 수 없다.
`60-seed` 레이어가 시드 인스턴스와 부하 생성기를 만든다.

```hcl
# 60-seed/rds.tf
resource "aws_db_instance" "seed" {
  identifier     = "dbmon-dev-seed-01"
  engine         = "mysql"
  engine_version = "8.4"                    # 하한 검증용
  instance_class = "db.t4g.micro"
  allocated_storage     = 20
  storage_type          = "gp3"
  db_subnet_group_name  = "dev-rds-subnet-group"   # 기존 것 재사용
  vpc_security_group_ids = [aws_security_group.seed_rds.id]

  iam_database_authentication_enabled = true       # ADR-007 검증
  performance_insights_enabled        = false      # 비용
  backup_retention_period             = 1          # 최소
  skip_final_snapshot                 = true       # dev
  deletion_protection                 = false      # dev
  publicly_accessible                 = false
  apply_immediately                   = true

  parameter_group_name = aws_db_parameter_group.seed.name

  tags = {
    Name        = "dbmon-dev-seed-01"
    env         = "dev"                # 우리 탐색이 읽는 태그 (FR-DSC-03)
    "dbmon:enabled" = "true"
    Project     = "dbmon"
  }
}

resource "aws_db_parameter_group" "seed" {
  name   = "dbmon-dev-seed-mysql84"
  family = "mysql8.4"

  # 자가진단(FR-DSC-10)이 요구하는 값들
  parameter { name = "performance_schema"                  value = "1"    apply_method = "pending-reboot" }
  parameter { name = "slow_query_log"                      value = "1" }
  parameter { name = "long_query_time"                     value = "1" }
  parameter { name = "log_output"                          value = "FILE" }
  # 선택적 최적화 (OPEN-Q-07 폴백 경로 검증용)
  parameter { name = "performance_schema_max_digest_length"   value = "4096" apply_method = "pending-reboot" }
  parameter { name = "performance_schema_max_sql_text_length" value = "8192" apply_method = "pending-reboot" }
}
```

**두 번째 인스턴스를 만들 이유** — Aurora 3.05를 하나 더 두면 [ADR-019](03-decisions.md)의
버전 분기와 [16 §2.3](16-cost.md)의 엔진별 메트릭 세트를 실제로 검증할 수 있다.
`aurora-mysql` `db.t4g.medium` 1대 = 월 약 $47. **상시 켜두지 않는다** —
M1 스파이크 기간에만 apply하고 destroy한다(레이어가 독립이므로 쉽다).

### 5.1 부하 생성기

시드 DB에 슬로우 쿼리가 없으면 아무것도 관측되지 않는다.

```hcl
# 60-seed/load.tf — EC2 t4g.nano 1대 (월 $3)
#   또는 개발자 로컬에서 실행 (비용 0, 권장)
```

`just seed`([14 §7](14-infrastructure.md))가 로컬에서 시드 DB에 붙어 부하를 만든다.
상시 부하가 필요하면(예: 다이제스트 롤업 24시간 관측) `t4g.nano`를 띄운다.

**부하 생성기가 만들어야 하는 것** — 설계 검증에 필요한 시나리오를 의도적으로 만든다.

| 시나리오 | 검증 대상 |
|---|---|
| 인덱스 없는 100만 행 테이블 풀스캔 (4초 이상) | in-flight 플랜 수집(ADR-006), `rows_examined` |
| **1024바이트 초과 SQL** (긴 `IN` 절) | `information_schema.PROCESSLIST` 절단 여부 ([OPEN-Q-07](OPEN-QUESTIONS.md)) |
| UPDATE / DELETE 장기 실행 | DML 플랜 수집 |
| prepared statement 경유 | `DIGEST` 가 텍스트 프로토콜과 같은지 |
| 락 경합 2세션 | `sys.innodb_lock_waits` 컬럼명·64자 절단 |
| 의도적 데드락 | `SHOW ENGINE INNODB STATUS` 파싱 |
| 다이제스트 1000종 이상 | 상위 N hard cap, `_other` 총량 보존 |
| sub-second 고빈도 쿼리 | 다이제스트 스냅샷이 폴링 사각지대를 덮는지 |
| 리터럴에 이메일·전화번호 | 마스킹 후조건(T-36), Bedrock 페이로드 검증 |

## 6. prd 혼재 계정에서의 격리

### 6.1 네트워크 (1차 방어선 — 이미 확보됨)

VPC 피어링 0개, TGW 0개. `dev-vpc-01`의 앱은 `prd-lla-vpc`의 어떤 것에도 **도달할 수 없다.**
prd RDS가 생겨도 dev 앱이 접속할 수 없다.

**이 방어선을 유지하는 것이 가장 중요하다.** 피어링·TGW를 추가하려면 아래 2차·3차 방어선을
먼저 구현한다.

### 6.2 탐색 필터 (2차 — 애플리케이션)

`rds:DescribeDBInstances`는 리소스 레벨 권한을 지원하지 않으므로 **prd 인스턴스도 목록에
나타난다.** 지금은 RDS가 0개라 무해하지만, 앱 설정으로 미리 막아둔다.

```
CFG/GLOBAL/discovery.allowed_vpc_ids   = ["vpc-0123456789abcdef0"]   # dev-vpc-01 만
CFG/GLOBAL/discovery.required_tags     = { "Project": "dbmon" }
CFG/GLOBAL/discovery.denied_name_regex = "^prd-"
```

- 세 조건을 **AND**로 적용한다. 하나라도 불통과면 레지스트리에 등록하지 않는다.
- dev 환경에서는 `discovery.allowed_vpc_ids`를 **필수 설정**으로 만든다(비어 있으면 기동 거부).
  prd 환경에서는 비워둘 수 있다.
- 목록에서 걸러진 인스턴스는 로그에 `filtered_out` 이벤트로만 남긴다(존재를 UI에 노출하지 않는다).

### 6.3 IAM (3차)

**`rds-db:connect`를 dev에서는 와일드카드로 주지 않는다.**
[08 §5.2](08-security-auth.md)는 500대 규모를 전제로 `dbuser:*/dbmon`을 썼다.
dev는 인스턴스가 1~2대이므로 **`DbiResourceId`를 열거**할 수 있다.

```hcl
# 30-identity/iam.tf
data "aws_db_instance" "seed" { db_instance_identifier = "dbmon-dev-seed-01" }

statement {
  actions   = ["rds-db:connect"]
  resources = [
    "arn:aws:rds-db:${var.region}:${var.account_id}:dbuser:${data.aws_db_instance.seed.resource_id}/dbmon"
  ]
}
```

인스턴스를 추가할 때마다 `terraform apply`가 필요하지만, dev에서는 그게 정상이다.
**prd RDS가 생겨도 dev 앱이 접속할 IAM 권한이 없다.**

추가로 dev Role의 `rds:Describe*`에 조건을 걸 수는 없지만, 다음은 걸 수 있다.

```hcl
# S3·DynamoDB·Secrets 는 dev 접두어로 제한
resources = ["arn:aws:dynamodb:*:${var.account_id}:table/dbmon-dev-*"]
condition { test = "StringEquals" variable = "aws:ResourceAccount" values = [var.account_id] }
```

### 6.4 태그·명명 규약

계정에 `dev`/`prod` 표식이 없으므로 **이름으로 구분한다.**

| 리소스 | dev 이름 | prd 이름 |
|---|---|---|
| DynamoDB | `dbmon-dev-data`, `dbmon-dev-config` | `dbmon-prd-data`, … |
| S3 | `dbmon-dev-raw-123456789012` | `dbmon-prd-raw-<acct>` |
| S3 Tables 버킷 | `dbmon-dev-tables-123456789012` | `dbmon-prd-tables-<acct>` |
| Iceberg 네임스페이스 | `dbmon_dev` | `dbmon_prd` |
| Glue DB | `dbmon_dev_raw` | `dbmon_prd_raw` |
| Athena 워크그룹 | `dbmon-dev` (1개) | `dbmon-prd-user`, `dbmon-prd-batch` |
| IAM Role | `dbmon-dev-instance-role` | `dbmon-prd-instance-role` |
| Cognito Pool | `dbmon-dev` | `dbmon-prd` |
| KMS 별칭 | `alias/dbmon-dev` | `alias/dbmon-prd` |
| 로그 그룹 | `/dbmon/dev/app` | `/dbmon/prd/app` |

**공통 태그** (Terraform `default_tags`로 전 리소스에 강제)
```hcl
default_tags {
  tags = {
    Project     = "dbmon"
    Environment = "dev"
    ManagedBy   = "terraform"
    TFLayer     = "10-foundation"      # 레이어별로 다르게
    Owner       = "teinam"
  }
}
```

`Project = "dbmon"` 태그로 [16 §5](16-cost.md)의 AWS Budgets 비용 할당이 가능해진다.
**이 계정에 prd 워크로드가 있으므로 태그 없이는 우리 비용을 분리할 수 없다.**

### 6.5 하지 않는 것

| 금지 | 이유 |
|---|---|
| 계정 레벨 S3 퍼블릭 액세스 차단 변경 | prd 버킷에 영향 |
| GuardDuty / Config / SecurityHub 활성·변경 | 계정 전역 |
| default VPC 삭제·수정 | 다른 용도가 있을 수 있다 |
| `dev-vpc-01`의 라우트 테이블·IGW·기존 엔드포인트 수정 | 우리가 만들지 않았다 |
| `prd-lla-vpc`의 모든 리소스 | 명백 |
| 계정 레벨 CloudTrail 변경 | 감사 추적 |
| SCP / Organizations | 권한 밖이자 범위 밖 |
| `terraform destroy`를 `10-foundation`에 실행 | 기존 네트워크를 data source로 참조하므로 안전하지만, KMS 키 삭제는 되돌릴 수 없다 → `prevent_destroy` |

**`prevent_destroy = true`를 붙일 리소스**: KMS 키, DynamoDB 테이블,
S3 Tables 테이블 버킷, 리포트·아티팩트 버킷, Cognito User Pool.
dev여도 실수로 지우면 재구축이 번거롭다.

## 7. dev 설정 차이

| 항목 | prd | **dev** | 근거 |
|---|---|---|---|
| EC2 | c7g.large × 2 (active+standby) | **c7g.medium × 1** | standby 불필요. 배포 중단 수용 |
| ALB / WAF / ACM | 있음 | **없음** | SSM 포워딩 ([§4](#4-dev-접근-모델--alb-도-nat-도-쓰지-않는다)) |
| NAT | 단일 AZ | **없음** | 퍼블릭 서브넷 |
| 서브넷 | 프라이빗 | **퍼블릭** (인바운드 0) | NAT 비용 회피 |
| 핫 보관 (`hot_retention_days`) | 31일 | **7일** | FR-STO-11 |
| 콜드 보관 | 1년 | **90일** | |
| DynamoDB TTL | 35일 | **10일** | 핫 7일 + 여유 3일 |
| 다이제스트 상위 N | 200 | **50** | 쓰기 비용 |
| 다이제스트 임계값 | 100ms | **50ms** | dev 워크로드가 작다 |
| 리터럴 정책 | `full_restricted` | **`full`** | 운영 데이터가 없다 |
| CloudWatch 플릿 폴링 | 3메트릭 × 15분 | **비활성** | 자체 수집만. 인스턴스 1~2대 |
| **EMF 커스텀 메트릭** | 228개 | **비활성 (로그만)** | 108개 × $0.30 = $32/월. dev 예산의 절반 |
| 알람 | 22개 | **5개** (수집 실패, 유실, 예산, 잡 침묵, 앱 에러율) | |
| PITR | 활성 | **활성** | 증분 내보내기 전제 |
| 아카이브 잡 | 일 1회 | **일 1회** | 경로 검증이 목적 |
| Athena 워크그룹 | user / batch 2개 | **1개** (`dbmon-dev`, 쿼리당 10GB) | |
| Cognito MFA | admin 필수 | **optional** | |
| Cognito 콜백 | `https://<도메인>/auth/callback` | **`http://localhost:8080/auth/callback`** | |
| AI 예산 | 환경별 | **월 100만 입력 토큰** | |
| AI 모델 | 설정 | **`global.anthropic.claude-sonnet-4-6`** 등 | 실측 가용 목록에서 선택 |
| 자동 어드바이저 실행 | 옵션 | **비활성** | 수동만 |
| 로그 보관 | 30일 | **7일** | |
| 로그 레벨 | `info` | **`debug`** (24시간 자동 해제 없음) | |
| 백업·삭제 방지 | 활성 | **비활성** (KMS·DynamoDB 제외) | |

## 8. dev 비용

| 항목 | 계산 | 월 |
|---|---|---|
| EC2 c7g.medium × 1 | 730h × $0.0406 (온디맨드, SP 없음) | $30 |
| EBS gp3 20GB | | $2 |
| **네트워크** (ALB·NAT·엔드포인트·WAF 없음) | | **$0** |
| DynamoDB 쓰기 | 시드 1~2대, 상위 N 50 → 일 약 3,000 WRU | $0.1 |
| DynamoDB 저장 + PITR | 1GB | $0.5 |
| S3 Tables (저장 + 모니터링 + 컴팩션) | 1GB | $0.6 |
| S3 (raw·results·plans) | 1GB | $0.1 |
| Athena | 월 20GB 스캔 | $0.1 |
| CloudWatch (로그 2GB + 알람 5개) | EMF 비활성 | $2 |
| Bedrock (수동 테스트 월 50회) | Sonnet급 | $3 |
| Secrets Manager (2개) | | $0.8 |
| KMS (키 1개 + 요청) | | $1.5 |
| DynamoDB 증분 내보내기 | 일 50MB | $0.2 |
| **시드 RDS** `db.t4g.micro` MySQL 8.4 (상시) | 730h × $0.021 + gp3 20GB | $18 |
| **합계 (상시 가동)** | | **약 $59** |

**절감 옵션**

| 조치 | 절감 |
|---|---|
| 시드 RDS를 업무 시간만 (일 8시간) | −$12 → $47 |
| EC2를 개발 중에만 (일 8시간) | −$20 → $27 |
| 둘 다 | **→ $27/월** |
| Aurora 시드 추가 (M1 기간만) | +$47 (해당 기간) |

**EC2·RDS 스케줄 정지**는 `60-seed`와 `40-compute`에 `enabled` 변수를 두어
`terraform apply -var=enabled=false`로 처리한다. Lambda 스케줄러를 만들지 않는다
(dev에서 그건 과잉이다).

**로컬 개발 중에는 `40-compute`가 필요 없다.** 앱을 로컬에서 돌리고 `10`~`30`만 apply하면
월 **약 $25**(시드 RDS 상시 $18 + 저장·기타 $7)다.

## 9. 이 조사로 해소된 미해결 이슈

| 항목 | 이전 상태 | 실측 결과 |
|---|---|---|
| [OPEN-Q-03](OPEN-QUESTIONS.md) S3 Tables 가용성 | 미해소 | **해소 (일부)** — ap-northeast-2 사용 가능, Glue 페더레이션 등록됨, 카탈로그 이름 형식 확정, Athena engine v3. **남은 것: `MERGE INTO` 실제 동작** |
| [OPEN-Q-12](OPEN-QUESTIONS.md) Aurora 버전 하한 | 부분 결정 | **해소** — 전 리전 RDS 0개. 새로 만들면 8.4로 만든다. **업그레이드 프로젝트가 선행 조건이 될 위험 없음** |
| [OPEN-Q-02](OPEN-QUESTIONS.md) 크로스 리전 | 미해소 | **현재 불필요** — 타 리전 RDS 0개. 멀티 리전은 미래 요구. 검증을 M12로 미룬다 |
| M1-12 플릿 능력 인벤토리 | 미실시 | **완료** — 대상 0개 |
| Bedrock 모델 가용성 | 미확인 | **확인** — Claude 다수 + `global`/`apac` 추론 프로파일 |

**새로 생긴 이슈**: [OPEN-Q-21](OPEN-QUESTIONS.md) — dev/prd 토폴로지 차이로 검증할 수 없는
4개 항목(ALB 동작, WAF, VPC 엔드포인트 IAM 조건, 프라이빗 서브넷 DNS·지연)을 어디서 확인할 것인가.

## 10. 적용 순서 (최초 1회)

```bash
export AWS_PROFILE=teinam-primary-123456789012
cd infra/layers

# 0) state 백엔드 — local state 로 1회
cd 00-bootstrap && terraform init && terraform apply -var-file=../../envs/dev.tfvars
#    → S3 dbmon-tfstate-123456789012 + DynamoDB dbmon-tflock 생성
#    → 생성된 backend 설정을 이후 레이어에 반영

# 1) 기반 — 여기까지만 해도 로컬 개발이 실제 AWS 를 쓴다
cd ../10-foundation && terraform init && terraform apply -var-file=../../envs/dev.tfvars

# 2) 시드 MySQL — 모니터링 대상 확보 (M1 스파이크 전제)
cd ../60-seed && terraform init && terraform apply -var-file=../../envs/dev.tfvars
#    → 마스터 비밀번호는 RDS 관리형 시크릿 (manage_master_user_password = true)
#    → ADR-008 (0) 수동 경로로 dbmon 계정 생성 SQL 을 실행하거나
#      부트스트랩 UI 의 rds_managed 소스를 쓴다

# 3) 데이터 계층 (M4 시점)
cd ../20-data && terraform init && terraform apply -var-file=../../envs/dev.tfvars

# 4) 인증 (M5 시점)
cd ../30-identity && terraform init && terraform apply -var-file=../../envs/dev.tfvars
#    → initial_admin_email 필수. 초대 메일로 첫 로그인

# 5) 관측성 (선택, 언제든)
cd ../50-observability && terraform init && terraform apply -var-file=../../envs/dev.tfvars

# 6) EC2 배포 (M6 이후, 아티팩트 업로드 후)
cd ../40-compute && terraform init && terraform apply -var-file=../../envs/dev.tfvars
```

**각 레이어는 독립이므로 순서를 지키지 않아도 `plan`이 실패로 알려준다**
(`terraform_remote_state`가 앞 레이어 출력을 못 찾는다). 조용히 잘못된 것을 만들지 않는다.

**`00-bootstrap`만 예외적으로 local state를 쓴다** — state 백엔드 자체를 만드는 레이어이므로.
생성 후 `terraform init -migrate-state`로 원격으로 옮길 수 있지만, dev에서는 local state를
그대로 두고 `.gitignore`에 넣는 편이 단순하다(재생성이 쉽다).
