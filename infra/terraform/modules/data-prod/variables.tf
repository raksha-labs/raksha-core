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
  default     = "db.t4g.micro"
}

variable "db_allocated_storage" {
  description = "Initial RDS allocated storage in GB"
  type        = number
  default     = 20
}

variable "db_max_allocated_storage" {
  description = "Maximum autoscaling storage in GB"
  type        = number
  default     = 100
}

variable "db_multi_az" {
  description = "Enable Multi-AZ for RDS (doubles cost, enables automatic failover)"
  type        = bool
  default     = false
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
  default     = "cache.t4g.micro"
}

variable "cache_num_nodes" {
  description = "ElastiCache node count (1 = no failover, 2+ = automatic failover + Multi-AZ)"
  type        = number
  default     = 1
}

variable "tags" {
  description = "Common tags"
  type        = map(string)
  default     = {}
}
