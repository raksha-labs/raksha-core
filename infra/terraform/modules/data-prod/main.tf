resource "random_password" "db_password" {
  length  = 32
  special = true
}

resource "random_password" "redis_auth_token" {
  length  = 64
  special = false
}

resource "aws_kms_key" "data" {
  description             = "KMS key for defi-surv ${var.environment} data tier"
  deletion_window_in_days = 30
  enable_key_rotation     = true

  tags = merge(var.tags, {
    Name = "defi-surv-${var.environment}-data-kms"
  })
}

resource "aws_db_subnet_group" "main" {
  name       = "defi-surv-${var.environment}-db-subnet-group"
  subnet_ids = var.private_subnet_ids

  tags = merge(var.tags, {
    Name = "defi-surv-${var.environment}-db-subnet-group"
  })
}

resource "aws_db_instance" "main" {
  identifier     = "defi-surv-${var.environment}-db"
  engine         = "postgres"
  engine_version = "15.5"
  instance_class = var.db_instance_class

  allocated_storage     = var.db_allocated_storage
  max_allocated_storage = var.db_max_allocated_storage
  storage_type          = "gp3"
  storage_encrypted     = true
  kms_key_id            = aws_kms_key.data.arn

  db_name  = var.db_name
  username = var.db_username
  password = random_password.db_password.result

  db_subnet_group_name   = aws_db_subnet_group.main.name
  vpc_security_group_ids = [var.database_sg_id]

  multi_az                        = true
  publicly_accessible             = false
  backup_retention_period         = var.db_backup_retention_days
  enabled_cloudwatch_logs_exports = ["postgresql", "upgrade"]

  deletion_protection = var.db_deletion_protection
  skip_final_snapshot = !var.db_deletion_protection

  tags = merge(var.tags, {
    Name = "defi-surv-${var.environment}-postgres"
  })
}

resource "aws_elasticache_subnet_group" "main" {
  name       = "defi-surv-${var.environment}-cache-subnet-group"
  subnet_ids = var.private_subnet_ids

  tags = merge(var.tags, {
    Name = "defi-surv-${var.environment}-cache-subnet-group"
  })
}

resource "aws_elasticache_replication_group" "main" {
  replication_group_id       = "defi-surv-${var.environment}-redis"
  description                = "Redis replication group for defi-surv ${var.environment}"
  engine                     = "redis"
  engine_version             = "7.1"
  node_type                  = var.cache_node_type
  num_cache_clusters         = var.cache_num_nodes
  parameter_group_name       = "default.redis7"
  port                       = 6379
  subnet_group_name          = aws_elasticache_subnet_group.main.name
  security_group_ids         = [var.redis_sg_id]
  automatic_failover_enabled = var.cache_num_nodes > 1
  multi_az_enabled           = var.cache_num_nodes > 1

  at_rest_encryption_enabled = true
  transit_encryption_enabled = true
  auth_token                 = random_password.redis_auth_token.result

  snapshot_retention_limit = 7

  tags = merge(var.tags, {
    Name = "defi-surv-${var.environment}-redis"
  })
}

resource "aws_secretsmanager_secret" "database" {
  name        = "${var.secret_prefix}/shared/DATABASE_URL"
  description = "Managed PostgreSQL connection string"
  kms_key_id  = aws_kms_key.data.arn

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "database" {
  secret_id = aws_secretsmanager_secret.database.id
  secret_string = jsonencode({
    DATABASE_URL = "postgresql://${var.db_username}:${random_password.db_password.result}@${aws_db_instance.main.endpoint}/${var.db_name}"
    DB_HOST      = aws_db_instance.main.address
    DB_PORT      = aws_db_instance.main.port
    DB_NAME      = var.db_name
    DB_USERNAME  = var.db_username
    DB_PASSWORD  = random_password.db_password.result
  })
}

resource "aws_secretsmanager_secret" "redis" {
  name        = "${var.secret_prefix}/shared/REDIS_URL"
  description = "Managed Redis connection string"
  kms_key_id  = aws_kms_key.data.arn

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "redis" {
  secret_id = aws_secretsmanager_secret.redis.id
  secret_string = jsonencode({
    REDIS_URL        = "rediss://:${random_password.redis_auth_token.result}@${aws_elasticache_replication_group.main.primary_endpoint_address}:6379"
    REDIS_HOST       = aws_elasticache_replication_group.main.primary_endpoint_address
    REDIS_PORT       = 6379
    REDIS_AUTH_TOKEN = random_password.redis_auth_token.result
  })
}
