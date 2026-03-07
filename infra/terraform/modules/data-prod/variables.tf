variable "environment" {
  description = "Environment name"
  type        = string
}

variable "secret_prefix" {
  description = "Secret prefix, for example raksha/prod"
  type        = string
}

variable "private_subnet_ids" {
  description = "Private subnet IDs for data tier"
  type        = list(string)
}

variable "database_sg_id" {
  description = "Security group ID for database"
  type        = string
}

variable "redis_sg_id" {
  description = "Security group ID for Redis"
  type        = string
}

variable "db_instance_class" {
  description = "RDS instance class"
  type        = string
  default     = "db.t4g.medium"
}

variable "db_allocated_storage" {
  description = "Initial RDS allocated storage in GB"
  type        = number
  default     = 100
}

variable "db_max_allocated_storage" {
  description = "Maximum autoscaling storage in GB"
  type        = number
  default     = 500
}

variable "db_multi_az" {
  description = "Whether the RDS instance runs as Multi-AZ"
  type        = bool
  default     = true
}

variable "db_name" {
  description = "Database name"
  type        = string
  default     = "raksha"
}

variable "db_username" {
  description = "Database username"
  type        = string
  default     = "defi_admin"
}

variable "db_backup_retention_days" {
  description = "RDS backup retention"
  type        = number
  default     = 7
}

variable "db_deletion_protection" {
  description = "RDS deletion protection"
  type        = bool
  default     = true
}

variable "cache_node_type" {
  description = "ElastiCache node type"
  type        = string
  default     = "cache.t4g.medium"
}

variable "cache_num_nodes" {
  description = "ElastiCache node count"
  type        = number
  default     = 2
}

variable "tags" {
  description = "Common tags"
  type        = map(string)
  default     = {}
}
