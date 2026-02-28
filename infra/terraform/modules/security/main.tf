resource "aws_security_group" "alb_public" {
  name        = "raksha-${var.environment}-alb-public-sg"
  description = "Public ALB security group"
  vpc_id      = var.vpc_id

  ingress {
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = var.alb_ingress_cidrs
    description = "HTTP ingress"
  }

  ingress {
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = var.alb_ingress_cidrs
    description = "HTTPS ingress"
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
    description = "All outbound"
  }

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-alb-public-sg"
  })
}

resource "aws_security_group" "alb_admin_internal" {
  count       = var.enable_internal_admin_alb ? 1 : 0
  name        = "raksha-${var.environment}-alb-admin-internal-sg"
  description = "Internal admin ALB security group"
  vpc_id      = var.vpc_id

  ingress {
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = length(var.admin_ingress_cidrs) > 0 ? var.admin_ingress_cidrs : [var.vpc_cidr]
    description = "Admin HTTPS ingress"
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
    description = "All outbound"
  }

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-alb-admin-internal-sg"
  })
}

resource "aws_security_group" "ecs_tasks" {
  name        = "raksha-${var.environment}-ecs-tasks-sg"
  description = "ECS task ENI security group"
  vpc_id      = var.vpc_id

  ingress {
    from_port       = 0
    to_port         = 65535
    protocol        = "tcp"
    security_groups = [aws_security_group.alb_public.id]
    description     = "Traffic from public ALB"
  }

  dynamic "ingress" {
    for_each = var.enable_internal_admin_alb ? [1] : []
    content {
      from_port       = 0
      to_port         = 65535
      protocol        = "tcp"
      security_groups = [aws_security_group.alb_admin_internal[0].id]
      description     = "Traffic from internal admin ALB"
    }
  }

  ingress {
    from_port   = 0
    to_port     = 65535
    protocol    = "tcp"
    self        = true
    description = "Inter-service communication"
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
    description = "All outbound"
  }

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-ecs-tasks-sg"
  })
}

resource "aws_security_group" "ecs_instances" {
  name        = "raksha-${var.environment}-ecs-instances-sg"
  description = "ECS EC2 container instance security group"
  vpc_id      = var.vpc_id

  ingress {
    from_port   = 0
    to_port     = 65535
    protocol    = "tcp"
    self        = true
    description = "Intra-cluster node communication"
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
    description = "All outbound"
  }

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-ecs-instances-sg"
  })
}

resource "aws_security_group" "database" {
  name        = "raksha-${var.environment}-db-sg"
  description = "Database access security group"
  vpc_id      = var.vpc_id

  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [aws_security_group.ecs_tasks.id]
    description     = "PostgreSQL from ECS tasks"
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
    description = "All outbound"
  }

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-db-sg"
  })
}

resource "aws_security_group" "redis" {
  name        = "raksha-${var.environment}-redis-sg"
  description = "Redis access security group"
  vpc_id      = var.vpc_id

  ingress {
    from_port       = 6379
    to_port         = 6379
    protocol        = "tcp"
    security_groups = [aws_security_group.ecs_tasks.id]
    description     = "Redis from ECS tasks"
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
    description = "All outbound"
  }

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-redis-sg"
  })
}
