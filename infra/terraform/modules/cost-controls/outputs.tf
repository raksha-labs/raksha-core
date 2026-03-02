output "budget_alert_topic_arn" {
  value       = aws_sns_topic.budget_alerts.arn
  description = "SNS topic ARN for budget and anomaly alerts"
}

output "budget_name" {
  value       = aws_budgets_budget.monthly.name
  description = "Budget name"
}

output "anomaly_monitor_arn" {
  value       = try(aws_ce_anomaly_monitor.main[0].arn, null)
  description = "Cost anomaly monitor ARN"
}

output "billing_estimated_charges_alarm_arn" {
  value       = try(aws_cloudwatch_metric_alarm.billing_estimated_charges[0].arn, null)
  description = "CloudWatch Billing EstimatedCharges alarm ARN (null when not created outside us-east-1)"
}
