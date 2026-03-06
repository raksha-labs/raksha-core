output "cluster_name" {
  value       = module.compute.cluster_name
  description = "ECS cluster name"
  sensitive   = true
}

output "public_alb_dns_name" {
  value       = module.compute.public_alb_dns_name
  description = "Public ALB DNS"
}

output "admin_alb_dns_name" {
  value       = module.compute.admin_alb_dns_name
  description = "Internal admin ALB DNS"
}

output "repository_urls" {
  value       = module.compute.repository_urls
  description = "ECR repository URLs by service"
}

output "database_url_secret_arn" {
  value       = module.data_prod.database_url_secret_arn
  description = "DATABASE_URL secret ARN"
  sensitive   = true
}

output "redis_url_secret_arn" {
  value       = module.data_prod.redis_url_secret_arn
  description = "REDIS_URL secret ARN"
  sensitive   = true
}

output "raw_database_url_secret_arn" {
  value       = module.data_prod.raw_database_url_secret_arn
  description = "RAW_DATABASE_URL secret ARN"
  sensitive   = true
}

output "waf_arn" {
  value       = var.enable_waf && module.compute.public_alb_arn != null ? aws_wafv2_web_acl.public[0].arn : null
  description = "WAF ARN"
}

output "budget_alert_topic_arn" {
  value       = module.cost_controls.budget_alert_topic_arn
  description = "Budget alarm SNS topic ARN"
}

output "github_images_role_arn" {
  value       = module.cicd_iam.images_role_arn
  description = "GitHub Actions images role ARN"
  sensitive   = true
}

output "github_infra_role_arn" {
  value       = module.cicd_iam.infra_role_arn
  description = "GitHub Actions infra role ARN"
  sensitive   = true
}

output "github_deploy_role_arn" {
  value       = module.cicd_iam.deploy_role_arn
  description = "GitHub Actions deploy role ARN"
  sensitive   = true
}

output "core_contract_ssm_prefix" {
  value       = "/raksha/${var.environment}/core"
  description = "SSM prefix for core deployment contract"
}
