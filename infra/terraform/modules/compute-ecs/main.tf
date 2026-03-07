locals {
  services = var.service_catalog

  public_services = {
    for name, cfg in local.services : name => cfg
    if try(cfg.exposure, "internal") == "public"
  }

  admin_services = {
    for name, cfg in local.services : name => cfg
    if try(cfg.exposure, "internal") == "private-admin"
  }

  public_service_names = sort(keys(local.public_services))
  admin_service_names  = sort(keys(local.admin_services))

  public_default_service = contains(local.public_service_names, var.public_default_service) ? var.public_default_service : (
    length(local.public_service_names) > 0 ? local.public_service_names[0] : null
  )

  admin_default_service = contains(local.admin_service_names, var.admin_default_service) ? var.admin_default_service : (
    length(local.admin_service_names) > 0 ? local.admin_service_names[0] : null
  )

  desired_counts_base = {
    for name, cfg in local.services :
    name => try(cfg.desired_count[var.environment], 1)
  }

  desired_counts = merge(local.desired_counts_base, var.service_desired_counts)

  cpu_memory = {
    for name, cfg in local.services :
    name => {
      cpu    = try(var.service_cpu_memory[name].cpu, try(cfg.cpu, 256))
      memory = try(var.service_cpu_memory[name].memory, try(cfg.memory, 512))
    }
  }

  # Fargate accepts only specific CPU/memory pairs. When lower-cost EC2-sized
  # overrides are reused in Fargate mode, round them up to the nearest
  # supported pair instead of failing task registration.
  fargate_cpu_options = [256, 512, 1024, 2048, 4096]
  fargate_memory_options_by_cpu = {
    "256"  = [512, 1024, 2048]
    "512"  = [1024, 2048, 3072, 4096]
    "1024" = [2048, 3072, 4096, 5120, 6144, 7168, 8192]
    "2048" = [4096, 5120, 6144, 7168, 8192, 9216, 10240, 11264, 12288, 13312, 14336, 15360, 16384]
    "4096" = [8192, 9216, 10240, 11264, 12288, 13312, 14336, 15360, 16384, 17408, 18432, 19456, 20480, 21504, 22528, 23552, 24576, 25600, 26624, 27648, 28672, 29696, 30720]
  }
  effective_cpu_memory = var.compute_mode == "ec2" ? local.cpu_memory : {
    for name, cfg in local.cpu_memory :
    name => {
      cpu = try([for option in local.fargate_cpu_options : option if option >= cfg.cpu][0], local.fargate_cpu_options[length(local.fargate_cpu_options) - 1])
      memory = try([
        for option in local.fargate_memory_options_by_cpu[tostring(try([for cpu_option in local.fargate_cpu_options : cpu_option if cpu_option >= cfg.cpu][0], local.fargate_cpu_options[length(local.fargate_cpu_options) - 1]))] :
        option
        if option >= cfg.memory
      ][0], local.fargate_memory_options_by_cpu[tostring(try([for cpu_option in local.fargate_cpu_options : cpu_option if cpu_option >= cfg.cpu][0], local.fargate_cpu_options[length(local.fargate_cpu_options) - 1]))][length(local.fargate_memory_options_by_cpu[tostring(try([for cpu_option in local.fargate_cpu_options : cpu_option if cpu_option >= cfg.cpu][0], local.fargate_cpu_options[length(local.fargate_cpu_options) - 1]))]) - 1])
    }
  }

  spot_eligible = {
    for name, cfg in local.services :
    name => contains(var.fargate_spot_scaling_classes, try(cfg.scaling_class, ""))
  }

  test_data_services = var.enable_test_data_services ? {
    postgres = {
      image          = "postgres:15-alpine"
      container_port = 5432
      health_path    = "/"
      command        = []
      environment = [
        { name = "POSTGRES_DB", value = "raksha" },
        { name = "POSTGRES_USER", value = "defi_admin" },
        { name = "POSTGRES_PASSWORD", value = "defi_test_password" }
      ]
    }
    redis = {
      image          = "redis:7-alpine"
      container_port = 6379
      health_path    = "/"
      command        = ["redis-server", "--appendonly", "yes"]
      environment    = []
    }
  } : {}

  discovery_services = merge(local.services, local.test_data_services)

  service_static_env = {
    for name, _ in local.services :
    name => concat(
      [
        {
          name  = "ENVIRONMENT"
          value = var.environment
        },
        {
          name  = "AWS_REGION"
          value = var.aws_region
        }
      ],
      [
        for key in sort(keys(lookup(var.service_static_env, name, {}))) : {
          name  = key
          value = lookup(var.service_static_env, name, {})[key]
        }
      ]
    )
  }

  service_secret_env = {
    for name, _ in local.services :
    name => [
      for key in sort(keys(lookup(var.service_secret_env, name, {}))) : {
        name      = key
        valueFrom = lookup(var.service_secret_env, name, {})[key]
      }
    ]
  }
}

resource "aws_ecs_cluster" "this" {
  name = "raksha-${var.environment}"

  setting {
    name  = "containerInsights"
    value = "enabled"
  }

  tags = var.tags
}

resource "aws_ecr_repository" "services" {
  for_each = local.services

  name                 = each.value.image_repo
  image_tag_mutability = "IMMUTABLE"

  image_scanning_configuration {
    scan_on_push = true
  }

  encryption_configuration {
    encryption_type = "AES256"
  }

  tags = merge(var.tags, {
    Service = each.key
  })
}

resource "aws_ecr_lifecycle_policy" "services" {
  for_each = local.services

  repository = aws_ecr_repository.services[each.key].name
  policy = jsonencode({
    rules = [
      {
        rulePriority = 1
        description  = "Keep recent images only"
        selection = {
          tagStatus   = "any"
          countType   = "imageCountMoreThan"
          countNumber = var.ecr_image_retention_count
        }
        action = {
          type = "expire"
        }
      }
    ]
  })
}

resource "aws_iam_role" "ecs_task_execution" {
  name = "raksha-${var.environment}-ecs-task-execution-role"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Action = "sts:AssumeRole"
        Effect = "Allow"
        Principal = {
          Service = "ecs-tasks.amazonaws.com"
        }
      }
    ]
  })

  tags = var.tags
}

resource "aws_iam_role_policy_attachment" "ecs_task_execution_managed" {
  role       = aws_iam_role.ecs_task_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role_policy" "ecs_task_execution_custom" {
  name = "raksha-${var.environment}-ecs-task-execution-custom"
  role = aws_iam_role.ecs_task_execution.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "secretsmanager:GetSecretValue",
          "kms:Decrypt"
        ]
        Resource = [
          "arn:aws:secretsmanager:${var.aws_region}:*:secret:${var.secret_prefix}/*"
        ]
      },
      {
        Effect = "Allow"
        Action = [
          "logs:CreateLogGroup",
          "logs:CreateLogStream",
          "logs:PutLogEvents"
        ]
        Resource = "arn:aws:logs:${var.aws_region}:*:log-group:/ecs/raksha/*"
      }
    ]
  })
}

resource "aws_iam_role" "ecs_task" {
  name = "raksha-${var.environment}-ecs-task-role"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Action = "sts:AssumeRole"
        Effect = "Allow"
        Principal = {
          Service = "ecs-tasks.amazonaws.com"
        }
      }
    ]
  })

  tags = var.tags
}

resource "aws_iam_role_policy" "ecs_task" {
  name = "raksha-${var.environment}-ecs-task-policy"
  role = aws_iam_role.ecs_task.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "secretsmanager:GetSecretValue"
        ]
        Resource = "arn:aws:secretsmanager:${var.aws_region}:*:secret:${var.secret_prefix}/*"
      }
    ]
  })
}

resource "aws_service_discovery_private_dns_namespace" "this" {
  name        = "raksha-${var.environment}.local"
  description = "Private DNS namespace for raksha internal service communication"
  vpc         = var.vpc_id

  tags = var.tags
}

resource "aws_service_discovery_service" "main" {
  for_each = local.discovery_services

  name = each.key

  dns_config {
    namespace_id = aws_service_discovery_private_dns_namespace.this.id

    dns_records {
      ttl  = 10
      type = "A"
    }

    routing_policy = "WEIGHTED"
  }

  health_check_custom_config {
    failure_threshold = 1
  }
}

resource "aws_lb" "public" {
  count = length(local.public_services) > 0 ? 1 : 0

  name               = "raksha-${var.environment}-public"
  internal           = false
  load_balancer_type = "application"
  security_groups    = [var.alb_public_sg_id]
  subnets            = var.public_alb_subnet_ids

  enable_cross_zone_load_balancing = true
  idle_timeout                     = 60

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-public-alb"
  })
}

resource "aws_lb_target_group" "public" {
  for_each = local.public_services

  name        = substr("ds-${var.environment}-${replace(each.key, "-", "")}", 0, 32)
  port        = each.value.container_port
  protocol    = "HTTP"
  vpc_id      = var.vpc_id
  target_type = "ip"

  health_check {
    enabled             = true
    healthy_threshold   = 2
    unhealthy_threshold = 3
    timeout             = 5
    interval            = 30
    path                = try(each.value.health_path, "/health")
    matcher             = "200-399"
  }

  deregistration_delay = 30

  tags = var.tags
}

resource "aws_lb_listener" "public_http" {
  count = length(local.public_services) > 0 ? 1 : 0

  load_balancer_arn = aws_lb.public[0].arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.public[local.public_default_service].arn
  }
}

resource "aws_lb_listener" "public_https" {
  count = length(local.public_services) > 0 && var.enable_public_https && var.public_certificate_arn != null ? 1 : 0

  load_balancer_arn = aws_lb.public[0].arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = var.public_certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.public[local.public_default_service].arn
  }
}

resource "aws_lb_listener_rule" "public_api_paths" {
  count = length(local.public_services) > 0 && contains(local.public_service_names, "api-service") && local.public_default_service != "api-service" ? 1 : 0

  listener_arn = var.enable_public_https && var.public_certificate_arn != null ? aws_lb_listener.public_https[0].arn : aws_lb_listener.public_http[0].arn
  priority     = 10

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.public["api-service"].arn
  }

  condition {
    path_pattern {
      values = ["/v1/*", "/health", "/api/*"]
    }
  }
}

resource "aws_lb" "admin_internal" {
  count = var.admin_access_mode == "private-only" && length(local.admin_services) > 0 ? 1 : 0

  name               = "raksha-${var.environment}-admin"
  internal           = true
  load_balancer_type = "application"
  security_groups    = [var.alb_admin_internal_sg_id]
  subnets            = length(var.admin_alb_subnet_ids) > 0 ? var.admin_alb_subnet_ids : var.task_subnet_ids

  enable_cross_zone_load_balancing = true

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-admin-alb"
  })
}

resource "aws_lb_target_group" "admin" {
  for_each = var.admin_access_mode == "private-only" ? local.admin_services : {}

  name        = substr("adm-${var.environment}-${replace(each.key, "-", "")}", 0, 32)
  port        = each.value.container_port
  protocol    = "HTTP"
  vpc_id      = var.vpc_id
  target_type = "ip"

  health_check {
    enabled             = true
    healthy_threshold   = 2
    unhealthy_threshold = 3
    timeout             = 5
    interval            = 30
    path                = try(each.value.health_path, "/health")
    matcher             = "200-399"
  }

  tags = var.tags
}

resource "aws_lb_listener" "admin_http" {
  count = var.admin_access_mode == "private-only" && length(local.admin_services) > 0 ? 1 : 0

  load_balancer_arn = aws_lb.admin_internal[0].arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.admin[local.admin_default_service].arn
  }
}

resource "aws_lb_listener_rule" "admin_api_paths" {
  count = var.admin_access_mode == "private-only" && length(local.admin_services) > 0 && contains(local.admin_service_names, "admin-api-service") && local.admin_default_service != "admin-api-service" ? 1 : 0

  listener_arn = aws_lb_listener.admin_http[0].arn
  priority     = 10

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.admin["admin-api-service"].arn
  }

  condition {
    path_pattern {
      values = ["/api/*", "/v1/*", "/health"]
    }
  }
}

resource "aws_ecs_task_definition" "service" {
  for_each = local.services

  family                   = "raksha-${var.environment}-${each.key}"
  requires_compatibilities = var.compute_mode == "ec2" ? ["EC2"] : ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = tostring(local.effective_cpu_memory[each.key].cpu)
  memory                   = tostring(local.effective_cpu_memory[each.key].memory)
  execution_role_arn       = aws_iam_role.ecs_task_execution.arn
  task_role_arn            = aws_iam_role.ecs_task.arn

  container_definitions = jsonencode([
    {
      name      = each.key
      image     = "${aws_ecr_repository.services[each.key].repository_url}:${var.image_tag}"
      essential = true
      command   = try(each.value.command, null)
      portMappings = try(each.value.container_port, 0) > 0 ? [
        {
          containerPort = each.value.container_port
          protocol      = "tcp"
        }
      ] : []
      environment = local.service_static_env[each.key]
      secrets     = local.service_secret_env[each.key]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          awslogs-group         = "/ecs/raksha/${var.environment}/${each.key}"
          awslogs-region        = var.aws_region
          awslogs-stream-prefix = "ecs"
        }
      }
    }
  ])

  tags = var.tags
}

resource "terraform_data" "ecs_service_launch_mode" {
  triggers_replace = var.compute_mode
}

resource "aws_ecs_service" "service" {
  for_each = local.services

  name                   = "raksha-${var.environment}-${each.key}"
  cluster                = aws_ecs_cluster.this.id
  task_definition        = aws_ecs_task_definition.service[each.key].arn
  desired_count          = local.desired_counts[each.key]
  force_new_deployment   = var.force_new_deployment
  enable_execute_command = var.enable_execute_command

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  deployment_minimum_healthy_percent = 50
  deployment_maximum_percent         = 200

  launch_type = var.compute_mode == "ec2" ? "EC2" : null

  dynamic "capacity_provider_strategy" {
    for_each = var.compute_mode == "fargate_mix" && local.spot_eligible[each.key] ? [
      { cp = "FARGATE", base = 0, weight = 1 },
      { cp = "FARGATE_SPOT", base = 0, weight = 3 }
      ] : var.compute_mode == "fargate_mix" ? [
      { cp = "FARGATE", base = 1, weight = 1 }
    ] : []

    content {
      capacity_provider = capacity_provider_strategy.value.cp
      base              = capacity_provider_strategy.value.base
      weight            = capacity_provider_strategy.value.weight
    }
  }

  dynamic "network_configuration" {
    for_each = var.compute_mode == "ec2" ? [1] : []
    content {
      subnets         = var.task_subnet_ids
      security_groups = [var.ecs_tasks_sg_id]
    }
  }

  dynamic "network_configuration" {
    for_each = var.compute_mode == "ec2" ? [] : [1]
    content {
      subnets          = var.task_subnet_ids
      security_groups  = [var.ecs_tasks_sg_id]
      assign_public_ip = var.assign_public_ip
    }
  }

  dynamic "service_registries" {
    for_each = [1]
    content {
      registry_arn = aws_service_discovery_service.main[each.key].arn
    }
  }

  dynamic "load_balancer" {
    for_each = try(each.value.exposure, "internal") == "public" ? [1] : []
    content {
      target_group_arn = aws_lb_target_group.public[each.key].arn
      container_name   = each.key
      container_port   = each.value.container_port
    }
  }

  dynamic "load_balancer" {
    for_each = try(each.value.exposure, "internal") == "private-admin" && var.admin_access_mode == "private-only" ? [1] : []
    content {
      target_group_arn = aws_lb_target_group.admin[each.key].arn
      container_name   = each.key
      container_port   = each.value.container_port
    }
  }

  tags = merge(var.tags, {
    Service        = each.key
    DeployStrategy = try(each.value.deploy_strategy, "rolling")
  })

  depends_on = [
    aws_lb_listener.public_http,
    aws_lb_listener.admin_http
  ]

  lifecycle {
    # ECS cannot update an existing EC2 service in place to a Fargate-style
    # network configuration (or the reverse). Force replacement when the module
    # launch mode changes so Terraform recreates the service cleanly.
    replace_triggered_by = [terraform_data.ecs_service_launch_mode]
  }
}

resource "aws_ecs_task_definition" "test_data" {
  for_each = local.test_data_services

  family                   = "raksha-${var.environment}-${each.key}"
  requires_compatibilities = var.compute_mode == "ec2" ? ["EC2"] : ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = "256"
  memory                   = "512"
  execution_role_arn       = aws_iam_role.ecs_task_execution.arn
  task_role_arn            = aws_iam_role.ecs_task.arn

  container_definitions = jsonencode([
    {
      name         = each.key
      image        = each.value.image
      essential    = true
      command      = each.value.command
      environment  = each.value.environment
      portMappings = [{ containerPort = each.value.container_port, protocol = "tcp" }]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          awslogs-group         = "/ecs/raksha/${var.environment}/${each.key}"
          awslogs-region        = var.aws_region
          awslogs-stream-prefix = "ecs"
        }
      }
    }
  ])

  tags = var.tags
}

resource "aws_ecs_service" "test_data" {
  for_each = local.test_data_services

  name                   = "raksha-${var.environment}-${each.key}"
  cluster                = aws_ecs_cluster.this.id
  task_definition        = aws_ecs_task_definition.test_data[each.key].arn
  desired_count          = 1
  force_new_deployment   = var.force_new_deployment
  enable_execute_command = var.enable_execute_command

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  launch_type = var.compute_mode == "ec2" ? "EC2" : null

  dynamic "capacity_provider_strategy" {
    for_each = var.compute_mode == "fargate_mix" ? [
      { cp = "FARGATE", base = 1, weight = 1 }
    ] : []

    content {
      capacity_provider = capacity_provider_strategy.value.cp
      base              = capacity_provider_strategy.value.base
      weight            = capacity_provider_strategy.value.weight
    }
  }

  dynamic "network_configuration" {
    for_each = var.compute_mode == "ec2" ? [1] : []
    content {
      subnets         = var.task_subnet_ids
      security_groups = [var.ecs_tasks_sg_id]
    }
  }

  dynamic "network_configuration" {
    for_each = var.compute_mode == "ec2" ? [] : [1]
    content {
      subnets          = var.task_subnet_ids
      security_groups  = [var.ecs_tasks_sg_id]
      assign_public_ip = var.assign_public_ip
    }
  }

  service_registries {
    registry_arn = aws_service_discovery_service.main[each.key].arn
  }

  tags = merge(var.tags, {
    Service = each.key
  })

  lifecycle {
    replace_triggered_by = [terraform_data.ecs_service_launch_mode]
  }
}

data "aws_ssm_parameter" "ecs_optimized_ami" {
  count = var.compute_mode == "ec2" ? 1 : 0

  name = var.ecs_ami_ssm_parameter_name
}

resource "aws_iam_role" "ecs_instance" {
  count = var.compute_mode == "ec2" ? 1 : 0

  name = "raksha-${var.environment}-ecs-instance-role"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Principal = {
          Service = "ec2.amazonaws.com"
        }
        Action = "sts:AssumeRole"
      }
    ]
  })

  tags = var.tags
}

resource "aws_iam_role_policy_attachment" "ecs_instance_role_ecs" {
  count = var.compute_mode == "ec2" ? 1 : 0

  role       = aws_iam_role.ecs_instance[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonEC2ContainerServiceforEC2Role"
}

resource "aws_iam_role_policy_attachment" "ecs_instance_role_ssm" {
  count = var.compute_mode == "ec2" ? 1 : 0

  role       = aws_iam_role.ecs_instance[0].name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "ecs_instance" {
  count = var.compute_mode == "ec2" ? 1 : 0

  name = "raksha-${var.environment}-ecs-instance-profile"
  role = aws_iam_role.ecs_instance[0].name
}

resource "aws_launch_template" "ecs" {
  count = var.compute_mode == "ec2" ? 1 : 0

  name_prefix   = "raksha-${var.environment}-ecs-"
  image_id      = data.aws_ssm_parameter.ecs_optimized_ami[0].value
  instance_type = var.ec2_instance_type

  iam_instance_profile {
    name = aws_iam_instance_profile.ecs_instance[0].name
  }

  vpc_security_group_ids = compact([var.ecs_instances_sg_id])

  user_data = base64encode(<<-EOT
    #!/bin/bash
    echo ECS_CLUSTER=${aws_ecs_cluster.this.name} >> /etc/ecs/ecs.config
    echo ECS_ENABLE_CONTAINER_METADATA=true >> /etc/ecs/ecs.config
  EOT
  )

  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }

  tags = var.tags
}

resource "aws_autoscaling_group" "ecs" {
  count = var.compute_mode == "ec2" ? 1 : 0

  name                      = "raksha-${var.environment}-ecs-asg"
  min_size                  = var.ec2_min_capacity
  max_size                  = var.ec2_max_capacity
  desired_capacity          = var.ec2_desired_capacity
  health_check_type         = "EC2"
  health_check_grace_period = 120

  vpc_zone_identifier = length(var.ec2_subnet_ids) > 0 ? var.ec2_subnet_ids : var.task_subnet_ids

  launch_template {
    id      = aws_launch_template.ecs[0].id
    version = "$Latest"
  }

  tag {
    key                 = "Name"
    value               = "raksha-${var.environment}-ecs-instance"
    propagate_at_launch = true
  }

  tag {
    key                 = "AmazonECSManaged"
    value               = "true"
    propagate_at_launch = true
  }
}

resource "aws_ecs_capacity_provider" "ec2" {
  count = var.compute_mode == "ec2" ? 1 : 0

  name = "raksha-${var.environment}-ec2-cp"

  auto_scaling_group_provider {
    auto_scaling_group_arn = aws_autoscaling_group.ecs[0].arn

    managed_scaling {
      status                    = "ENABLED"
      target_capacity           = 100
      minimum_scaling_step_size = 1
      maximum_scaling_step_size = 1
    }

    managed_termination_protection = "DISABLED"
  }

  tags = var.tags
}

resource "aws_ecs_cluster_capacity_providers" "this" {
  count = var.compute_mode == "ec2" ? 1 : 0

  cluster_name = aws_ecs_cluster.this.name

  capacity_providers = [aws_ecs_capacity_provider.ec2[0].name]

  default_capacity_provider_strategy {
    capacity_provider = aws_ecs_capacity_provider.ec2[0].name
    weight            = 1
  }
}
