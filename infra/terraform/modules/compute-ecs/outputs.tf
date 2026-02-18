output "cluster_name" {
  value       = aws_ecs_cluster.this.name
  description = "ECS cluster name"
}

output "cluster_arn" {
  value       = aws_ecs_cluster.this.arn
  description = "ECS cluster ARN"
}

output "service_discovery_namespace_id" {
  value       = aws_service_discovery_private_dns_namespace.this.id
  description = "Cloud Map namespace ID"
}

output "service_discovery_namespace_name" {
  value       = aws_service_discovery_private_dns_namespace.this.name
  description = "Cloud Map namespace name"
}

output "repository_urls" {
  value = {
    for name, repo in aws_ecr_repository.services :
    name => repo.repository_url
  }
  description = "ECR repository URLs by service"
}

output "ecs_task_execution_role_arn" {
  value       = aws_iam_role.ecs_task_execution.arn
  description = "Task execution role ARN"
}

output "ecs_task_role_arn" {
  value       = aws_iam_role.ecs_task.arn
  description = "Task role ARN"
}

output "service_names" {
  value       = [for name, _ in aws_ecs_service.service : name]
  description = "ECS service names"
}

output "public_alb_dns_name" {
  value       = length(aws_lb.public) > 0 ? aws_lb.public[0].dns_name : null
  description = "Public ALB DNS"
}

output "public_alb_arn" {
  value       = length(aws_lb.public) > 0 ? aws_lb.public[0].arn : null
  description = "Public ALB ARN"
}

output "admin_alb_dns_name" {
  value       = length(aws_lb.admin_internal) > 0 ? aws_lb.admin_internal[0].dns_name : null
  description = "Internal admin ALB DNS"
}

output "admin_alb_arn" {
  value       = length(aws_lb.admin_internal) > 0 ? aws_lb.admin_internal[0].arn : null
  description = "Internal admin ALB ARN"
}

output "public_target_group_arns" {
  value = {
    for name, tg in aws_lb_target_group.public :
    name => tg.arn
  }
  description = "Public target groups by service"
}

output "admin_target_group_arns" {
  value = {
    for name, tg in aws_lb_target_group.admin :
    name => tg.arn
  }
  description = "Admin target groups by service"
}

output "blue_green_candidate_services" {
  value = [
    for name, cfg in local.services : name
    if try(cfg.deploy_strategy, "rolling") == "blue_green"
  ]
  description = "Services tagged for blue/green deployment strategy"
}

output "rolling_candidate_services" {
  value = [
    for name, cfg in local.services : name
    if try(cfg.deploy_strategy, "rolling") != "blue_green"
  ]
  description = "Services tagged for rolling deployment strategy"
}
