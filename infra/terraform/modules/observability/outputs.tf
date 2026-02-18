output "log_group_names" {
  value       = [for group in aws_cloudwatch_log_group.service : group.name]
  description = "CloudWatch log groups created for services"
}

output "dashboard_name" {
  value       = var.enable_dashboard ? aws_cloudwatch_dashboard.main[0].dashboard_name : null
  description = "CloudWatch dashboard name"
}
