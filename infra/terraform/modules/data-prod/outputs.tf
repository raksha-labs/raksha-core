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

output "database_url_secret_version_id" {
  value       = aws_secretsmanager_secret_version.database.version_id
  description = "Current secret version ID for DATABASE_URL"
}

output "redis_url_secret_arn" {
  value       = aws_secretsmanager_secret.redis.arn
  description = "Secret ARN for REDIS_URL"
}

output "redis_url_secret_version_id" {
  value       = aws_secretsmanager_secret_version.redis.version_id
  description = "Current secret version ID for REDIS_URL"
}

output "raw_database_url_secret_arn" {
  value       = aws_secretsmanager_secret.raw_database.arn
  description = "Secret ARN for RAW_DATABASE_URL"
}

output "raw_database_url_secret_version_id" {
  value       = aws_secretsmanager_secret_version.raw_database.version_id
  description = "Current secret version ID for RAW_DATABASE_URL"
}

output "kms_key_arn" {
  value       = aws_kms_key.data.arn
  description = "KMS key ARN for data tier encryption"
}
