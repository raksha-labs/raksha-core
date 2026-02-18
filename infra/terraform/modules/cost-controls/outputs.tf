output "budget_alert_topic_arn" {
  value       = aws_sns_topic.budget_alerts.arn
  description = "SNS topic ARN for budget and anomaly alerts"
}

output "budget_name" {
  value       = aws_budgets_budget.monthly.name
  description = "Budget name"
}

output "anomaly_monitor_arn" {
  value       = aws_ce_anomaly_monitor.main.arn
  description = "Cost anomaly monitor ARN"
}
