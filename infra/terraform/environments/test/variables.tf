variable "environment" {
  description = "Environment name"
  type        = string
  default     = "test"
}

variable "aws_region" {
  description = "AWS region"
  type        = string
  default     = "eu-west-1"
}

variable "vpc_cidr" {
  description = "VPC CIDR"
  type        = string
  default     = "10.50.0.0/16"
}

variable "compute_mode" {
  description = "Compute mode for test"
  type        = string
  default     = "fargate_mix"
}

variable "enable_managed_data" {
  description = "Whether to use managed RDS/ElastiCache in test"
  type        = bool
  default     = true
}

variable "create_nat_gateway" {
  description = "Whether to create NAT in test"
  type        = bool
  default     = true
}

variable "service_desired_counts" {
  description = "Optional desired count overrides by service"
  type        = map(number)
  default     = {}
}

variable "streams_enabled" {
  description = "Enable stream-processing services (indexer, detector, orchestrator, finality, history-worker). Off by default in test — flip to true when testing the ingestion/detection pipeline."
  type        = bool
  default     = false
}

variable "service_cpu_memory" {
  description = "Optional CPU/memory overrides by service"
  type = map(object({
    cpu    = number
    memory = number
  }))
  # Reduced sizes for the single test EC2 instance. Memory is the hard ECS
  # limit per task; CPU is a soft reservation share only.
  default = {
    indexer          = { cpu = 128, memory = 256 }
    detector         = { cpu = 128, memory = 256 }
    orchestrator     = { cpu = 128, memory = 256 }
    finality         = { cpu = 128, memory = 256 }
    "history-worker" = { cpu = 128, memory = 256 }
  }
}

variable "log_retention_days" {
  description = "CloudWatch log retention"
  type        = number
  default     = 7
}

variable "budget_limit_usd" {
  description = "Monthly budget cap in USD"
  type        = number
  default     = 100
}

variable "anomaly_total_impact_absolute_usd" {
  description = "Absolute USD threshold for cost anomaly alerts"
  type        = number
  default     = 20
}

variable "enable_billing_estimated_charges_alarm" {
  description = "Enable CloudWatch Billing EstimatedCharges alarm (only created in us-east-1)"
  type        = bool
  default     = true
}

variable "billing_estimated_charges_alarm_usd" {
  description = "Absolute USD threshold for Billing EstimatedCharges alarm; null uses budget_limit_usd"
  type        = number
  default     = null
  nullable    = true
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
  default     = false
}

variable "public_certificate_arn" {
  description = "ACM certificate ARN for public HTTPS"
  type        = string
  default     = null
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

variable "ec2_instance_type" {
  description = "ECS instance type in EC2 mode"
  type        = string
  # t4g.medium (4 GB ARM) fits all platform + core services at reduced test
  # sizes. t4g.micro (1 GB) is too small once more than 3 services are live.
  default = "t4g.medium"
}

variable "ec2_desired_capacity" {
  description = "Desired ECS EC2 instance count"
  type        = number
  default     = 1
}

variable "ec2_min_capacity" {
  description = "Minimum ECS EC2 instance count"
  type        = number
  default     = 1
}

variable "ec2_max_capacity" {
  description = "Maximum ECS EC2 instance count"
  type        = number
  default     = 2
}

variable "db_instance_class" {
  description = "RDS instance class when managed data is enabled"
  type        = string
  default     = "db.t4g.small"
}

variable "db_name" {
  description = "Database name"
  type        = string
  default     = "raksha"
}

variable "db_username" {
  description = "Database admin username"
  type        = string
  default     = "defi_admin"
}

variable "cache_node_type" {
  description = "Redis node type when managed data is enabled"
  type        = string
  default     = "cache.t4g.small"
}

variable "cache_num_nodes" {
  description = "Redis node count when managed data is enabled"
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
  default     = "raksha-labs"
}

variable "github_repo" {
  description = "GitHub repository"
  type        = string
  default     = "raksha-core"
}

variable "github_additional_repositories" {
  description = "Additional repositories allowed to assume the shared GitHub Actions roles"
  type        = list(string)
  default     = ["raksha-platform", "raksha-simlab"]
}

variable "github_allowed_branches" {
  description = "Branches allowed to assume OIDC roles"
  type        = list(string)
  default     = ["master"]
}

variable "github_allowed_environments" {
  description = "GitHub environments allowed to assume OIDC roles"
  type        = list(string)
  default     = ["test", "stage", "prod"]
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

variable "rpc_ws_url_secret_arns" {
  description = "Map of RPC WebSocket URL env var names to Secrets Manager valueFrom strings for the indexer. Each value must be a full ECS secrets valueFrom reference (e.g. \"arn:aws:secretsmanager:eu-west-1:123456789012:secret:raksha/test/rpc-AbCdEf:ETH_WS_URL::\"). Without this the indexer falls back to mock/synthetic data."
  type        = map(string)
  default     = {}
}

variable "tags" {
  description = "Additional tags"
  type        = map(string)
  default     = {}
}
