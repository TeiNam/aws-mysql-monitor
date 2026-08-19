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
