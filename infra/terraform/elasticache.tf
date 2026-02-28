# ElastiCache Subnet Group
resource "aws_elasticache_subnet_group" "main" {
  name       = "raksha-cache-subnet-group"
  subnet_ids = aws_subnet.private[*].id

  tags = var.tags
}

# ElastiCache Redis Replication Group
resource "aws_elasticache_replication_group" "main" {
  replication_group_id = "raksha-redis"
  description         = "Redis cluster for Raksha event streams"

  engine               = "redis"
  engine_version       = "7.1"
  node_type            = var.cache_node_type
  num_cache_clusters   = var.cache_num_nodes
  parameter_group_name = "default.redis7"
  port                 = 6379

  subnet_group_name    = aws_elasticache_subnet_group.main.name
  security_group_ids   = [aws_security_group.elasticache.id]

  automatic_failover_enabled = true
  multi_az_enabled           = true

  at_rest_encryption_enabled = true
  transit_encryption_enabled = true
  auth_token_enabled         = false

  snapshot_retention_limit = 5
  snapshot_window          = "03:00-05:00"
  maintenance_window       = "mon:05:00-mon:07:00"

  auto_minor_version_upgrade = true

  tags = merge(var.tags, {
    Name = "raksha-redis"
  })
}

# Store Redis URL in Secrets Manager
resource "aws_secretsmanager_secret" "redis_url" {
  name        = "raksha-redis-url-${var.environment}"
  description = "Redis connection string"

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "redis_url" {
  secret_id = aws_secretsmanager_secret.redis_url.id
  secret_string = jsonencode({
    REDIS_URL           = "redis://${aws_elasticache_replication_group.main.primary_endpoint_address}:6379"
    REDIS_HOST          = aws_elasticache_replication_group.main.primary_endpoint_address
    REDIS_PORT          = 6379
    REDIS_READER_ENDPOINT = aws_elasticache_replication_group.main.reader_endpoint_address
  })
}
