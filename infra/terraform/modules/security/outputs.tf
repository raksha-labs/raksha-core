output "alb_public_sg_id" {
  value       = aws_security_group.alb_public.id
  description = "Security group ID for public ALB"
}

output "alb_admin_internal_sg_id" {
  value       = var.enable_internal_admin_alb ? aws_security_group.alb_admin_internal[0].id : null
  description = "Security group ID for internal admin ALB"
}

output "ecs_tasks_sg_id" {
  value       = aws_security_group.ecs_tasks.id
  description = "Security group ID for ECS tasks"
}

output "ecs_instances_sg_id" {
  value       = aws_security_group.ecs_instances.id
  description = "Security group ID for ECS instances"
}

output "database_sg_id" {
  value       = aws_security_group.database.id
  description = "Security group ID for database access"
}

output "redis_sg_id" {
  value       = aws_security_group.redis.id
  description = "Security group ID for Redis access"
}
