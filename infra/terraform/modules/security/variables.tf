variable "environment" {
  description = "Environment name"
  type        = string
}

variable "vpc_id" {
  description = "VPC ID"
  type        = string
}

variable "vpc_cidr" {
  description = "VPC CIDR"
  type        = string
}

variable "alb_ingress_cidrs" {
  description = "CIDR blocks allowed to public ALB"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "admin_ingress_cidrs" {
  description = "CIDR blocks allowed to internal admin ALB"
  type        = list(string)
  default     = []
}

variable "enable_internal_admin_alb" {
  description = "Whether to create an internal ALB security group for admin services"
  type        = bool
  default     = true
}

variable "tags" {
  description = "Common tags"
  type        = map(string)
  default     = {}
}
