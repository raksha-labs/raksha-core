# RDS Subnet Group
resource "aws_db_subnet_group" "main" {
  name       = "raksha-db-subnet-group"
  subnet_ids = aws_subnet.private[*].id

  tags = merge(var.tags, {
    Name = "raksha-db-subnet-group"
  })
}

# Random password for RDS
resource "random_password" "db_password" {
  length  = 32
  special = true
}

# Store DB password in Secrets Manager
resource "aws_secretsmanager_secret" "db_password" {
  name        = "raksha-db-password-${var.environment}"
  description = "PostgreSQL master password for Raksha"

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "db_password" {
  secret_id     = aws_secretsmanager_secret.db_password.id
  secret_string = random_password.db_password.result
}

# RDS PostgreSQL Instance
resource "aws_db_instance" "main" {
  identifier     = "raksha-db"
  engine         = "postgres"
  engine_version = "15.5"
  instance_class = var.db_instance_class

  allocated_storage     = 100
  max_allocated_storage = 500
  storage_type          = "gp3"
  storage_encrypted     = true

  db_name  = var.db_name
  username = var.db_username
  password = random_password.db_password.result

  db_subnet_group_name   = aws_db_subnet_group.main.name
  vpc_security_group_ids = [aws_security_group.rds.id]

  multi_az               = true
  publicly_accessible    = false
  backup_retention_period = 7
  backup_window          = "03:00-04:00"
  maintenance_window     = "mon:04:00-mon:05:00"

  enabled_cloudwatch_logs_exports = ["postgresql", "upgrade"]
  
  skip_final_snapshot       = false
  final_snapshot_identifier = "raksha-db-final-snapshot-${formatdate("YYYY-MM-DD-hhmm", timestamp())}"
  
  deletion_protection = true

  tags = merge(var.tags, {
    Name = "raksha-postgres"
  })
}

# Store full DATABASE_URL in Secrets Manager
resource "aws_secretsmanager_secret" "database_url" {
  name        = "raksha-database-url-${var.environment}"
  description = "Full PostgreSQL connection string"

  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "database_url" {
  secret_id = aws_secretsmanager_secret.database_url.id
  secret_string = jsonencode({
    DATABASE_URL = "postgres://${var.db_username}:${random_password.db_password.result}@${aws_db_instance.main.endpoint}/${var.db_name}"
    DB_HOST      = aws_db_instance.main.address
    DB_PORT      = aws_db_instance.main.port
    DB_NAME      = var.db_name
    DB_USERNAME  = var.db_username
    DB_PASSWORD  = random_password.db_password.result
  })
}
