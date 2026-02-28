variable "environment" {
  description = "Environment name"
  type        = string
}

variable "secret_prefix" {
  description = "Secret prefix, for example raksha/test"
  type        = string
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

variable "db_password" {
  description = "Database password"
  type        = string
  sensitive   = true
}

variable "postgres_host" {
  description = "Internal Postgres host name"
  type        = string
  default     = "postgres"
}

variable "postgres_port" {
  description = "Internal Postgres port"
  type        = number
  default     = 5432
}

variable "redis_host" {
  description = "Internal Redis host name"
  type        = string
  default     = "redis"
}

variable "redis_port" {
  description = "Internal Redis port"
  type        = number
  default     = 6379
}

variable "tags" {
  description = "Common tags"
  type        = map(string)
  default     = {}
}
