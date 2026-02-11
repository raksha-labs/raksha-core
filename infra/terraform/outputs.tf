output "ecs_cluster_name" {
  value       = aws_ecs_cluster.defi_surv.name
  description = "ECS cluster used for DeFi surveillance services"
}

output "ecs_cluster_arn" {
  value       = aws_ecs_cluster.defi_surv.arn
  description = "ARN of the ECS cluster"
}

output "vpc_id" {
  value       = aws_vpc.main.id
  description = "VPC ID"
}

output "private_subnet_ids" {
  value       = aws_subnet.private[*].id
  description = "Private subnet IDs for ECS tasks"
}

output "public_subnet_ids" {
  value       = aws_subnet.public[*].id
  description = "Public subnet IDs for ALB"
}

output "alb_dns_name" {
  value       = aws_lb.main.dns_name
  description = "DNS name of the Application Load Balancer"
}

output "alb_arn" {
  value       = aws_lb.main.arn
  description = "ARN of the Application Load Balancer"
}

output "rds_endpoint" {
  value       = aws_db_instance.main.endpoint
  description = "RDS PostgreSQL endpoint"
}

output "rds_address" {
  value       = aws_db_instance.main.address
  description = "RDS PostgreSQL hostname"
}

output "redis_primary_endpoint" {
  value       = aws_elasticache_replication_group.main.primary_endpoint_address
  description = "ElastiCache Redis primary endpoint"
}

output "redis_reader_endpoint" {
  value       = aws_elasticache_replication_group.main.reader_endpoint_address
  description = "ElastiCache Redis reader endpoint"
}

output "database_url_secret_arn" {
  value       = aws_secretsmanager_secret.database_url.arn
  description = "ARN of Secrets Manager secret for DATABASE_URL"
  sensitive   = true
}

output "redis_url_secret_arn" {
  value       = aws_secretsmanager_secret.redis_url.arn
  description = "ARN of Secrets Manager secret for REDIS_URL"
  sensitive   = true
}

output "ecs_task_execution_role_arn" {
  value       = aws_iam_role.ecs_task_execution.arn
  description = "ARN of ECS task execution role"
}

output "ecs_task_role_arn" {
  value       = aws_iam_role.ecs_task.arn
  description = "ARN of ECS task role"
}

output "ecr_repository_urls" {
  value = {
    for k, v in aws_ecr_repository.services : k => v.repository_url
  }
  description = "ECR repository URLs for all services"
}

output "security_group_ecs_tasks" {
  value       = aws_security_group.ecs_tasks.id
  description = "Security group ID for ECS tasks"
}

output "waf_web_acl_arn" {
  value       = aws_wafv2_web_acl.main.arn
  description = "ARN of WAF Web ACL"
}
