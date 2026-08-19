variable "region" {
  type    = string
  default = "ap-northeast-2"
}

variable "environment" {
  type = string
  validation {
    condition     = contains(["dev", "stg", "prd"], var.environment)
    error_message = "environment 는 dev, stg, prd 중 하나여야 한다."
  }
}
