variable "environment" {
  description = "Environment name"
  type        = string
}

variable "budget_limit_usd" {
  description = "Monthly budget limit in USD"
  type        = number
}

variable "alert_email_addresses" {
  description = "Email addresses for budget and anomaly notifications"
  type        = list(string)
  default     = []
}

variable "name_prefix" {
  description = "Prefix for budget and anomaly monitor names"
  type        = string
  default     = "raksha"
}

variable "tags" {
  description = "Common tags"
  type        = map(string)
  default     = {}
}
