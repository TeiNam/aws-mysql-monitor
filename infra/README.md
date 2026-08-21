# 인프라 (Terraform)

**앱 코드와 독립적으로 apply 할 수 있다.** 바이너리가 없어도 `00`~`30`·`50`·`60` 을
올릴 수 있고, 컨테이너 이미지가 준비된 뒤에 `40-compute` 를 올린다.
이게 "개발하다 필요하면 테라폼으로 올리는" 방식을 가능하게 한다.

## 레이어

| 레이어 | 내용 | 언제 필요한가 | 앱 코드 필요? |
|---|---|---|---|
| `00-bootstrap` | state 백엔드 (S3 + DynamoDB 락) | 맨 처음 1회 | ✗ |
| `10-foundation` | DynamoDB 2개, S3, KMS, VPC 게이트웨이 엔드포인트 | **M0** — 로컬 앱이 실제 DynamoDB 에 쓴다 | ✗ |
| `20-data` | S3 Tables(Iceberg), Athena 워크그룹, Glue | M4 (아카이브 조회) | ✗ |
| `30-identity` | Cognito User Pool·App Client·Groups | M5 (인증) | ✗ |
| `40-compute` | **ECR + ECS Fargate** + (선택) ALB | M6 (상시 가동) | **✓ 이미지 필요** |
| `50-observability` | CloudWatch 알람·대시보드·로그 그룹 | M2 이후 아무 때나 | ✗ |
| `60-seed` | 시드 MySQL 플릿: env(prd/dev) × {Aurora W+R, RDS P+R} + SSM 베스천 | **M1** (관측 대상 확보) | ✗ |

**레이어별 독립 state.** `terraform_remote_state` 로 앞 레이어의 출력을 읽는다.
한 레이어의 `destroy` 가 다른 레이어를 건드리지 않는다.

## apply 순서 (처음 1회)

```bash
export AWS_PROFILE=teinam-primary-123456789012

cd layers/00-bootstrap && terraform init && terraform apply   # 로컬 state → S3 로 이관
cd ../10-foundation    && terraform init && terraform apply

# 관측 대상 플릿: prd/dev × (Aurora writer+reader, RDS primary+replica)
#              + prd Aurora 8.4 writer + 베스천
cd ../60-seed
terraform init -backend-config=../../backends/dev.hcl
# RDS MySQL 마스터 비밀번호는 write-only 로 주입한다 (state 에 안 남는다.
# RDS MySQL 은 관리형 시크릿 + 리드 리플리카가 양립 불가라서다 — variables.tf 참조).
# **최초 1회만 필요하다** — 이후 apply 는 생략 (시크릿에서 ephemeral 로 읽는다).
export TF_VAR_mysql_master_password=$(openssl rand -hex 16)
terraform apply \
  -var vpc_id=vpc-0123456789abcdef0 \
  -var 'db_subnet_ids=["subnet-04669c82c04e56f49","subnet-063033553c53f6983"]' \
  -var bastion_subnet_id=subnet-0f0805f3b0c916463
# 여기까지면 로컬 `cargo run` 으로 실제 DynamoDB + 실제 MySQL 에 붙는다
```

**시드 DB 는 전부 프라이빗이다.** 접근은 베스천 SSM 포트 포워딩으로만 한다
(SSH 키·퍼블릭 IP·인바운드 규칙 없음, 기존 SSM 인터페이스 엔드포인트 재사용이라
NAT 비용 $0):

```bash
scripts/db-tunnel.sh --list               # 타깃과 기본 로컬 포트 (prd 1431x, dev 1432x)
scripts/db-tunnel.sh prd-aurora-writer    # 터널 열기
mysql -h127.0.0.1 -P14310 -udbmonadmin -p # 다른 셸에서 접속
```

## 기존 리소스는 `data` 로만 참조한다

`dev-vpc-01`·서브넷·라우트 테이블은 **우리가 만들지 않았다.** `resource` 로 관리하면
`terraform destroy` 가 다른 워크로드를 지운다. 반드시 `data "aws_vpc"` 로 참조한다.

```hcl
# 올바름
data "aws_vpc" "target" { id = var.vpc_id }

# 금지 — destroy 가 남의 VPC 를 지운다
resource "aws_vpc" "target" { ... }
```

`dev-vpc-01` 프라이빗 서브넷의 `0.0.0.0/0` 은 **blackhole** 이다(NAT 가 삭제되고 라우트만
남았다). 우리가 만든 것이 아니므로 건드리지 않는다 → [18 §1](../docs/18-dev-environment.md).

## 계정 전역 설정은 관리하지 않는다

이 계정에는 **프로덕션 워크로드가 함께 있다**(`prd-lla-vpc`, 실행 중 EC2).
다음은 Terraform 관리 대상에서 제외한다 — dev 배포가 prd 에 영향을 줄 수 있는 유일한 경로다.

- S3 계정 수준 퍼블릭 액세스 차단
- GuardDuty · Config · CloudTrail · Security Hub
- default VPC · default 보안 그룹
- SCP · 조직 정책
- 계정 수준 EBS 암호화 기본값

→ [18 §6](../docs/18-dev-environment.md)

## 규약

| 항목 | 규칙 |
|---|---|
| 태그 | `default_tags` 로 `Project=dbmon`, `Environment`, `ManagedBy=terraform` 를 전 리소스에 |
| 삭제 방지 | KMS 키·DynamoDB 테이블·S3 테이블버킷에 `prevent_destroy` |
| 이름 | `dbmon-<용도>-<환경>`. 전역 유일해야 하는 것(S3)은 계정 ID 접미 |
| 변수 | 필수값에 기본값을 두지 않는다. `terraform plan` 이 물어보게 한다 |

## apply 순서 (이 순서를 어기면 plan 이 안 된다)

`40-compute` 는 `terraform_remote_state` 로 `10-foundation` 의 state **객체를 직접
읽는다.** data source 는 plan 시점에 읽히므로, 객체가 없으면 `plan` 자체가
`Unable to find remote state` 로 죽는다. apply 실패가 아니라 **plan 불가**다.

```bash
# 0. 자격증명. `terraform_remote_state` 는 provider 블록이 아니라 환경 체인을 쓴다.
export AWS_PROFILE=teinam-primary-123456789012
aws sts get-caller-identity --query Account --output text   # backends/dev.hcl 의 버킷과 맞는지

# 1. state 버킷과 락을 만든다 (로컬 state)
cd infra/layers/00-bootstrap && terraform init && terraform apply

# 2. 부트스트랩 state 를 S3 로 이관한다 (backend.tf 의 주석을 해제한 뒤)
terraform init -backend-config=../../backends/dev.hcl -migrate-state

# 3. 기반 레이어. 라우트 테이블을 **명시**해야 한다 (endpoint_route_table_ids)
cd ../10-foundation
terraform init -backend-config=../../backends/dev.hcl
terraform apply -var environment=dev -var vpc_id=vpc-... \
  -var 'endpoint_route_table_ids=["rtb-priv","rtb-pub"]'

# 4. state 객체가 실제로 생겼는지 확인한다. 이 확인을 건너뛰면 5번이 죽는다.
aws s3api head-object --bucket dbmon-tfstate-<account> --key 10-foundation/terraform.tfstate

# 5. 컴퓨트
cd ../40-compute
terraform init -backend-config=../../backends/dev.hcl
terraform plan -var environment=dev -var vpc_id=vpc-... \
  -var state_bucket=dbmon-tfstate-<account> \
  -var 'allowed_vpc_ids=["vpc-..."]' -var 'db_auth_resource_ids=["db-XXXX"]' \
  -var 'slowlog_log_group_arns=["arn:aws:logs:...:log-group:/aws/rds/instance/dbmon-seed-dev/slowquery"]'
```

`terraform workspace` 는 쓰지 않는다 — 이유는 각 레이어의 `backend.tf` 주석에 있다.

## destroy 로는 되돌아가지 않는다

"레이어 destroy 로 되돌린다"는 서술은 사실이 아니다. 다음이 **의도적으로** 막는다:

| 레이어 | 막는 것 | 증상 |
|---|---|---|
| `10-foundation` | `prevent_destroy` (KMS 키, DynamoDB 2개) | plan 단계에서 실패 |
| `10-foundation` | `aws_s3_bucket.plans` 에 `force_destroy` 없음 | `BucketNotEmpty` |
| `40-compute` | `aws_ecr_repository` 에 `force_delete` 없음 | `RepositoryNotEmptyException` |

안전한 쪽으로 실패하는 것이니 그대로 둔다. 정말 지워야 하면 객체·이미지를 먼저 비우고
`prevent_destroy` 를 **의도적 커밋**으로 제거한다.
