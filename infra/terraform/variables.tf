variable "aws_region" {
  type        = string
  description = "AWS region for MVP deployment"
  default     = "eu-west-1"
}

variable "environment" {
  type        = string
  description = "Environment name (dev, staging, production)"
  default     = "production"
}

variable "vpc_cidr" {
  type        = string
  description = "CIDR block for VPC"
  default     = "10.0.0.0/16"
}

variable "db_instance_class" {
  type        = string
  description = "RDS instance type"
  default     = "db.t4g.medium"
}

variable "db_name" {
  type        = string
  description = "PostgreSQL database name"
  default     = "raksha"
}

variable "db_username" {
  type        = string
  description = "PostgreSQL master username"
  default     = "defi_admin"
  sensitive   = true
}

variable "cache_node_type" {
  type        = string
  description = "ElastiCache node type"
  default     = "cache.t4g.medium"
}

variable "cache_num_nodes" {
  type        = number
  description = "Number of cache nodes"
  default     = 2
}

variable "ecs_task_cpu" {
  type        = string
  description = "CPU units for ECS tasks"
  default     = "512"
}

variable "ecs_task_memory" {
  type        = string
  description = "Memory for ECS tasks (MB)"
  default     = "1024"
}

variable "tags" {
  type        = map(string)
  description = "Common tags for all resources"
  default = {
    Project     = "raksha"
    ManagedBy   = "terraform"
    Environment = "production"
  }
}

variable "github_org" {
  type        = string
  description = "GitHub organization or username"
  default     = "raksha-labs"
}

variable "github_repo" {
  type        = string
  description = "GitHub repository name"
  default     = "raksha-core"
}

variable "enable_github_oidc" {
  type        = bool
  description = "Enable GitHub Actions OIDC integration"
  default     = true
}
