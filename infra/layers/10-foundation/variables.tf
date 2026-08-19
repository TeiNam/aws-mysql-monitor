variable "region" {
  type    = string
  default = "ap-northeast-2"
}

variable "environment" {
  description = "dev | stg | prd"
  type        = string

  validation {
    condition     = contains(["dev", "stg", "prd"], var.environment)
    error_message = "environment 는 dev, stg, prd 중 하나여야 한다."
  }
}

variable "vpc_id" {
  description = <<-EOT
    기존 VPC 의 ID. **data source 로만 참조한다** — 우리가 만들지 않았다.
    dev 계정에는 prd-lla-vpc 도 있으므로 반드시 명시해야 한다 (T-37).
  EOT
  type        = string

  validation {
    condition     = can(regex("^vpc-[0-9a-f]{8,}$", var.vpc_id))
    error_message = "vpc_id 는 vpc-xxxxxxxx 형태여야 한다."
  }
}

variable "endpoint_route_table_ids" {
  description = <<-EOT
    게이트웨이 엔드포인트를 연결할 라우트 테이블 ID **목록을 명시한다.**

    기본값을 두지 않는 이유: 이전 구현은 `data.aws_route_tables` 로 VPC 의 **모든**
    라우트 테이블(메인 RT 와 남의 서브넷 것까지)에 prefix-list 라우트를 넣었다.
    `terraform destroy` 는 그 라우트를 제거하므로, priv 서브넷에 NAT 가 없는 이 VPC 에서는
    게이트웨이 엔드포인트가 다른 워크로드의 **유일한 S3/DynamoDB 경로**일 수 있고
    우리 레이어 destroy 가 그 워크로드를 끊는다. `infra/README.md` 가 금지한 결합이다.

    확인: `aws ec2 describe-route-tables --filters Name=vpc-id,Values=<vpc> \
            --query 'RouteTables[].{id:RouteTableId,tags:Tags}'`
  EOT
  type        = list(string)

  validation {
    condition     = length(var.endpoint_route_table_ids) > 0
    error_message = "연결할 라우트 테이블을 최소 하나 지정해야 한다."
  }
}

variable "create_s3_gateway_endpoint" {
  description = <<-EOT
    S3 게이트웨이 엔드포인트를 **새로 만들 것인가.**

    dev VPC 에는 이미 있다(`vpce-03eb8b8a5dbfad810`, [18 §2](../../../docs/18-dev-environment.md)).
    한 라우트 테이블에 같은 서비스의 prefix-list 라우트를 두 번 넣을 수 없으므로
    `true` 로 두면 `RouteAlreadyExists` 로 **첫 apply 가 깨진다.**
  EOT
  type        = bool
  default     = false
}
