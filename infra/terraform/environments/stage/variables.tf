variable "environment" {
  description = "Environment name"
  type        = string
  default     = "stage"
}

variable "aws_region" {
  description = "AWS region"
  type        = string
  default     = "eu-west-1"
}

variable "vpc_cidr" {
  description = "VPC CIDR"
  type        = string
  default     = "10.60.0.0/16"
}

variable "az_count" {
  description = "Number of AZs"
  type        = number
  default     = 2
}

variable "compute_mode" {
  description = "Compute mode for stage"
  type        = string
  default     = "fargate_mix"
}

variable "enable_managed_data" {
  description = "Use managed RDS/ElastiCache"
  type        = bool
  default     = true
}

variable "create_nat_gateway" {
  description = "Create NAT gateway"
  type        = bool
  default     = true
}

variable "nat_gateway_per_az" {
  description = "Create one NAT gateway per AZ"
  type        = bool
  default     = false
}

variable "service_desired_counts" {
  description = "Optional desired count overrides by service"
  type        = map(number)
  default     = {}
}

variable "service_cpu_memory" {
  description = "Optional CPU/memory overrides by service"
  type = map(object({
    cpu    = number
    memory = number
  }))
  default = {}
}

variable "log_retention_days" {
  description = "CloudWatch log retention"
  type        = number
  default     = 14
}

variable "budget_limit_usd" {
  description = "Monthly budget cap in USD"
  type        = number
  default     = 500
}

variable "alarm_emails" {
  description = "Budget/alarm notification emails"
  type        = list(string)
  default     = []
}

variable "admin_access_mode" {
  description = "Admin access mode"
  type        = string
  default     = "private-only"
}

variable "public_default_service" {
  description = "Default public ALB service"
  type        = string
  default     = "raksha-web"
}

variable "admin_default_service" {
  description = "Default admin ALB service"
  type        = string
  default     = "raksha-admin"
}

variable "enable_public_https" {
  description = "Enable HTTPS on public ALB"
  type        = bool
  default     = true
}

variable "public_certificate_arn" {
  description = "ACM certificate ARN for public HTTPS"
  type        = string
  default     = null
}

variable "enable_waf" {
  description = "Enable WAF on public ALB"
  type        = bool
  default     = true
}

variable "waf_rate_limit" {
  description = "WAF IP rate limit"
  type        = number
  default     = 2000
}

variable "image_tag" {
  description = "Container image tag"
  type        = string
  default     = "latest"
}

variable "fargate_spot_scaling_classes" {
  description = "Scaling classes eligible for spot on fargate"
  type        = list(string)
  default     = ["worker_cpu_and_lag"]
}

variable "db_instance_class" {
  description = "RDS instance class"
  type        = string
  default     = "db.t4g.medium"
}

variable "db_name" {
  description = "Database name"
  type        = string
  default     = "defi_surv"
}

variable "db_username" {
  description = "Database admin username"
  type        = string
  default     = "defi_admin"
}

variable "cache_node_type" {
  description = "Redis node type"
  type        = string
  default     = "cache.t4g.medium"
}

variable "cache_num_nodes" {
  description = "Redis node count"
  type        = number
  default     = 2
}

variable "alb_ingress_cidrs" {
  description = "Allowed ingress CIDRs to public ALB"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "admin_ingress_cidrs" {
  description = "Allowed ingress CIDRs to internal admin ALB"
  type        = list(string)
  default     = []
}

variable "github_org" {
  description = "GitHub organization"
  type        = string
  default     = "your-org"
}

variable "github_repo" {
  description = "GitHub repository"
  type        = string
  default     = "defi-surv"
}

variable "github_allowed_branches" {
  description = "Branches allowed to assume OIDC roles"
  type        = list(string)
  default     = ["main", "release/*"]
}

variable "create_oidc_provider" {
  description = "Create GitHub OIDC provider"
  type        = bool
  default     = false
}

variable "oidc_provider_arn" {
  description = "Existing GitHub OIDC provider ARN"
  type        = string
  default     = null
}

variable "tags" {
  description = "Additional tags"
  type        = map(string)
  default     = {}
}
