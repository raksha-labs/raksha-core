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

variable "anomaly_total_impact_absolute_usd" {
  description = "Absolute USD threshold for anomaly alerts"
  type        = number
  default     = 20
}

variable "create_anomaly_monitor" {
  description = "Whether to create Cost Explorer anomaly monitor/subscription"
  type        = bool
  default     = true
}

variable "enable_billing_estimated_charges_alarm" {
  description = "Create CloudWatch billing alarm on AWS/Billing EstimatedCharges (requires us-east-1)"
  type        = bool
  default     = true
}

variable "billing_estimated_charges_alarm_usd" {
  description = "Absolute USD threshold for Billing EstimatedCharges alarm; defaults to budget_limit_usd when null"
  type        = number
  default     = null
  nullable    = true
}

variable "billing_estimated_charges_currency" {
  description = "Currency dimension for AWS/Billing EstimatedCharges metric"
  type        = string
  default     = "USD"
}

variable "billing_estimated_charges_period_seconds" {
  description = "Period in seconds for AWS/Billing EstimatedCharges metric"
  type        = number
  default     = 21600
}

variable "billing_estimated_charges_evaluation_periods" {
  description = "Evaluation periods for AWS/Billing EstimatedCharges alarm"
  type        = number
  default     = 1
}
