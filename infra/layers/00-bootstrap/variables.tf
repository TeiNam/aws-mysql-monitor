variable "region" {
  description = "state 백엔드를 둘 리전. 앱 배포 리전과 같게 유지한다."
  type        = string
  default     = "ap-northeast-2"
}

variable "environment" {
  description = "태그에 들어갈 환경 이름. dev | stg | prd"
  type        = string
  default     = "dev"

  validation {
    condition     = contains(["dev", "stg", "prd"], var.environment)
    error_message = "environment 는 dev, stg, prd 중 하나여야 한다."
  }
}
