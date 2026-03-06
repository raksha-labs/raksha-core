output "backup_bucket_name" {
  value       = aws_s3_bucket.test_backup.bucket
  description = "S3 bucket used for low-cost test backup artifacts"
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

output "database_url" {
  value       = local.database_url
  description = "Resolved test DATABASE_URL"
  sensitive   = true
}

output "redis_url" {
  value       = local.redis_url
  description = "Resolved test REDIS_URL"
  sensitive   = true
}

output "raw_database_url" {
  value       = local.raw_database_url
  description = "Resolved test RAW_DATABASE_URL"
  sensitive   = true
}
