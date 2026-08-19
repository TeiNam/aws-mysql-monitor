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
| `60-seed` | 시드 MySQL 8.4 + 파라미터 그룹 | **M1** (관측 대상 확보) | ✗ |

**레이어별 독립 state.** `terraform_remote_state` 로 앞 레이어의 출력을 읽는다.
한 레이어의 `destroy` 가 다른 레이어를 건드리지 않는다.

## apply 순서 (처음 1회)

```bash
export AWS_PROFILE=teinam-primary-123456789012

cd layers/00-bootstrap && terraform init && terraform apply   # 로컬 state → S3 로 이관
cd ../10-foundation    && terraform init && terraform apply
cd ../60-seed          && terraform init && terraform apply   # 관측 대상
# 여기까지면 로컬 `cargo run` 으로 실제 DynamoDB + 실제 MySQL 에 붙는다
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
