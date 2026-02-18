output "database_endpoint" {
  value       = aws_db_instance.main.endpoint
  description = "RDS endpoint"
}

output "redis_primary_endpoint" {
  value       = aws_elasticache_replication_group.main.primary_endpoint_address
  description = "Redis primary endpoint"
}

output "database_url_secret_arn" {
  value       = aws_secretsmanager_secret.database.arn
  description = "Secret ARN for DATABASE_URL"
}

output "redis_url_secret_arn" {
  value       = aws_secretsmanager_secret.redis.arn
  description = "Secret ARN for REDIS_URL"
}

output "kms_key_arn" {
  value       = aws_kms_key.data.arn
  description = "KMS key ARN for data tier encryption"
}
