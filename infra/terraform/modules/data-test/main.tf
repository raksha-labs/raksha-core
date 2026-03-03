locals {
  database_url     = "postgresql://${var.db_username}:${var.db_password}@${var.postgres_host}:${var.postgres_port}/${var.db_name}"
  raw_database_url = local.database_url
  redis_url        = "redis://${var.redis_host}:${var.redis_port}"
}

resource "aws_s3_bucket" "test_backup" {
  bucket_prefix = "raksha-${var.environment}-backup-"

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-backup"
  })
}

resource "aws_s3_bucket_versioning" "test_backup" {
  bucket = aws_s3_bucket.test_backup.id

  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "test_backup" {
  bucket = aws_s3_bucket.test_backup.id

  rule {
    id     = "expire-old-backups"
    status = "Enabled"

    filter {}

    noncurrent_version_expiration {
      noncurrent_days = 14
    }

    expiration {
      days = 30
    }
  }
}

resource "aws_secretsmanager_secret" "database" {
  name        = "${var.secret_prefix}/shared/DATABASE_URL"
  description = "Test environment PostgreSQL connection string"

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "database" {
  secret_id = aws_secretsmanager_secret.database.id
  secret_string = jsonencode({
    DATABASE_URL = local.database_url
    DB_HOST      = var.postgres_host
    DB_PORT      = var.postgres_port
    DB_NAME      = var.db_name
    DB_USERNAME  = var.db_username
    DB_PASSWORD  = var.db_password
  })
}

resource "aws_secretsmanager_secret" "raw_database" {
  name        = "${var.secret_prefix}/shared/RAW_DATABASE_URL"
  description = "Test environment raw ingestion PostgreSQL connection string"

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "raw_database" {
  secret_id = aws_secretsmanager_secret.raw_database.id
  secret_string = jsonencode({
    RAW_DATABASE_URL = local.raw_database_url
    DB_HOST          = var.postgres_host
    DB_PORT          = var.postgres_port
    DB_NAME          = var.db_name
    DB_USERNAME      = var.db_username
    DB_PASSWORD      = var.db_password
  })
}

resource "aws_secretsmanager_secret" "redis" {
  name        = "${var.secret_prefix}/shared/REDIS_URL"
  description = "Test environment Redis connection string"

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "redis" {
  secret_id = aws_secretsmanager_secret.redis.id
  secret_string = jsonencode({
    REDIS_URL  = local.redis_url
    REDIS_HOST = var.redis_host
    REDIS_PORT = var.redis_port
  })
}
