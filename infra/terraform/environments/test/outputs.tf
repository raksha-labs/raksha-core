output "cluster_name" {
  value       = module.compute.cluster_name
  description = "ECS cluster name"
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
  value       = var.enable_managed_data ? module.data_prod[0].database_url_secret_arn : module.data_test[0].database_url_secret_arn
  description = "DATABASE_URL secret ARN"
  sensitive   = true
}

output "redis_url_secret_arn" {
  value       = var.enable_managed_data ? module.data_prod[0].redis_url_secret_arn : module.data_test[0].redis_url_secret_arn
  description = "REDIS_URL secret ARN"
  sensitive   = true
}

output "raw_database_url_secret_arn" {
  value       = var.enable_managed_data ? module.data_prod[0].raw_database_url_secret_arn : module.data_test[0].raw_database_url_secret_arn
  description = "RAW_DATABASE_URL secret ARN"
  sensitive   = true
}

output "budget_alert_topic_arn" {
  value       = module.cost_controls.budget_alert_topic_arn
  description = "Budget alarm SNS topic ARN"
}

output "github_images_role_arn" {
  value       = module.cicd_iam.images_role_arn
  description = "GitHub Actions images role ARN"
}

output "github_infra_role_arn" {
  value       = module.cicd_iam.infra_role_arn
  description = "GitHub Actions infra role ARN"
}

output "github_deploy_role_arn" {
  value       = module.cicd_iam.deploy_role_arn
  description = "GitHub Actions deploy role ARN"
}

output "core_contract_ssm_prefix" {
  value       = "/raksha/${var.environment}/core"
  description = "SSM prefix for core deployment contract"
}
