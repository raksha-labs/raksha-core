provider "aws" {
  region = var.aws_region

  default_tags {
    tags = merge({
      Project     = "defi-surv"
      Environment = var.environment
      ManagedBy   = "terraform"
    }, var.tags)
  }
}

locals {
  service_catalog_raw = yamldecode(file("${path.module}/../../../service-catalog.yaml"))
  service_catalog_map = {
    for svc in local.service_catalog_raw.services :
    svc.service_name => svc
  }
  secret_prefix = "defi-surv/${var.environment}"
}

resource "random_password" "test_db" {
  count   = var.enable_managed_data ? 0 : 1
  length  = 24
  special = false
}

module "network" {
  source = "../../modules/network"

  environment        = var.environment
  vpc_cidr           = var.vpc_cidr
  single_az          = true
  az_count           = 1
  create_nat_gateway = var.create_nat_gateway
  nat_gateway_per_az = false
  tags               = var.tags
}

module "security" {
  source = "../../modules/security"

  environment               = var.environment
  vpc_id                    = module.network.vpc_id
  vpc_cidr                  = module.network.vpc_cidr
  alb_ingress_cidrs         = var.alb_ingress_cidrs
  admin_ingress_cidrs       = var.admin_ingress_cidrs
  enable_internal_admin_alb = var.admin_access_mode == "private-only"
  tags                      = var.tags
}

module "data_test" {
  count  = var.enable_managed_data ? 0 : 1
  source = "../../modules/data-test"

  environment   = var.environment
  secret_prefix = local.secret_prefix
  db_name       = var.db_name
  db_username   = var.db_username
  db_password   = random_password.test_db[0].result
  postgres_host = "postgres"
  redis_host    = "redis"
  tags          = var.tags
}

module "data_prod" {
  count  = var.enable_managed_data ? 1 : 0
  source = "../../modules/data-prod"

  environment        = var.environment
  secret_prefix      = local.secret_prefix
  private_subnet_ids = module.network.private_subnet_ids
  database_sg_id     = module.security.database_sg_id
  redis_sg_id        = module.security.redis_sg_id
  db_instance_class  = var.db_instance_class
  db_name            = var.db_name
  db_username        = var.db_username
  cache_node_type    = var.cache_node_type
  cache_num_nodes    = var.cache_num_nodes
  tags               = var.tags
}

module "compute" {
  source = "../../modules/compute-ecs"

  environment                  = var.environment
  aws_region                   = var.aws_region
  service_catalog              = local.service_catalog_map
  service_desired_counts       = var.service_desired_counts
  service_cpu_memory           = var.service_cpu_memory
  compute_mode                 = var.compute_mode
  vpc_id                       = module.network.vpc_id
  task_subnet_ids              = var.compute_mode == "ec2" ? module.network.public_subnet_ids : module.network.private_subnet_ids
  ec2_subnet_ids               = module.network.public_subnet_ids
  public_alb_subnet_ids        = module.network.public_subnet_ids
  admin_alb_subnet_ids         = module.network.private_subnet_ids
  ecs_tasks_sg_id              = module.security.ecs_tasks_sg_id
  ecs_instances_sg_id          = module.security.ecs_instances_sg_id
  alb_public_sg_id             = module.security.alb_public_sg_id
  alb_admin_internal_sg_id     = module.security.alb_admin_internal_sg_id
  secret_prefix                = local.secret_prefix
  image_tag                    = var.image_tag
  assign_public_ip             = var.compute_mode == "ec2"
  public_default_service       = var.public_default_service
  admin_default_service        = var.admin_default_service
  admin_access_mode            = var.admin_access_mode
  enable_public_https          = var.enable_public_https
  public_certificate_arn       = var.public_certificate_arn
  enable_test_data_services    = !var.enable_managed_data
  fargate_spot_scaling_classes = var.fargate_spot_scaling_classes
  ec2_instance_type            = var.ec2_instance_type
  ec2_desired_capacity         = var.ec2_desired_capacity
  ec2_min_capacity             = var.ec2_min_capacity
  ec2_max_capacity             = var.ec2_max_capacity
  tags                         = var.tags
}

module "cost_controls" {
  source = "../../modules/cost-controls"

  environment           = var.environment
  budget_limit_usd      = var.budget_limit_usd
  alert_email_addresses = var.alarm_emails
  name_prefix           = "defi-surv"
  tags                  = var.tags
}

module "observability" {
  source = "../../modules/observability"

  environment         = var.environment
  cluster_name        = module.compute.cluster_name
  service_names       = concat(keys(local.service_catalog_map), var.enable_managed_data ? [] : ["postgres", "redis"])
  log_retention_days  = var.log_retention_days
  alarm_sns_topic_arn = module.cost_controls.budget_alert_topic_arn
  tags                = var.tags
}

module "cicd_iam" {
  source = "../../modules/cicd-iam"

  environment             = var.environment
  github_org              = var.github_org
  github_repo             = var.github_repo
  github_allowed_branches = var.github_allowed_branches
  create_oidc_provider    = var.create_oidc_provider
  oidc_provider_arn       = var.oidc_provider_arn
  tags                    = var.tags
}
