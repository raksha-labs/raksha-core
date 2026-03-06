provider "aws" {
  region = var.aws_region

  default_tags {
    tags = merge({
      Project     = "raksha"
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
  secret_prefix = "raksha/${var.environment}"
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
  postgres_host = "postgres.raksha-${var.environment}.local"
  redis_host    = "redis.raksha-${var.environment}.local"
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

locals {
  database_url_secret_arn     = var.enable_managed_data ? module.data_prod[0].database_url_secret_arn : module.data_test[0].database_url_secret_arn
  raw_database_url_secret_arn = var.enable_managed_data ? module.data_prod[0].raw_database_url_secret_arn : module.data_test[0].raw_database_url_secret_arn
  redis_url_secret_arn        = var.enable_managed_data ? module.data_prod[0].redis_url_secret_arn : module.data_test[0].redis_url_secret_arn

  service_static_env = {
    orchestrator = {
      ALERT_FALLBACK_TENANT_ID = "glider"
      NOTIFIER_GATEWAY_URL     = "http://notifier-gateway:3002"
    }
    finality = {
      ALERT_FALLBACK_TENANT_ID = "glider"
    }
    "history-worker" = {
      HISTORY_WORKER_INTERVAL_SECS = "30"
      HISTORY_WORKER_BATCH_SIZE    = "500"
      HISTORY_PREFIX               = "history"
      HEALTH_CHECK_PORT            = "8080"
    }
  }

  service_secret_env = {
    for service_name in keys(local.service_catalog_map) :
    service_name => merge(
      {
        DATABASE_URL     = "${local.database_url_secret_arn}:DATABASE_URL::"
        RAW_DATABASE_URL = "${local.raw_database_url_secret_arn}:RAW_DATABASE_URL::"
        REDIS_URL        = "${local.redis_url_secret_arn}:REDIS_URL::"
      },
      service_name == "indexer" ? var.rpc_ws_url_secret_arns : {}
    )
  }
}

locals {
  # streams_enabled = true sets desired_count = 1 for the full data pipeline.
  # service_desired_counts can override individual services on top of this.
  streams_desired = var.streams_enabled ? {
    indexer          = 1
    detector         = 1
    orchestrator     = 1
    finality         = 1
    "history-worker" = 1
  } : {}

  effective_desired_counts = merge(local.streams_desired, var.service_desired_counts)
}

module "compute" {
  source = "../../modules/compute-ecs"

  environment                  = var.environment
  aws_region                   = var.aws_region
  service_catalog              = local.service_catalog_map
  service_desired_counts       = local.effective_desired_counts
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
  service_static_env           = local.service_static_env
  service_secret_env           = local.service_secret_env
  tags                         = var.tags
}

locals {
  core_contract_version = sha256(join("|", [
    var.environment,
    var.image_tag,
    local.database_url_secret_arn,
    local.raw_database_url_secret_arn,
    local.redis_url_secret_arn,
    module.compute.service_discovery_namespace_name
  ]))
}

resource "aws_ssm_parameter" "core_database_url_secret_arn" {
  name      = "/raksha/${var.environment}/core/database_url_secret_arn"
  type      = "String"
  value     = local.database_url_secret_arn
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_redis_url_secret_arn" {
  name      = "/raksha/${var.environment}/core/redis_url_secret_arn"
  type      = "String"
  value     = local.redis_url_secret_arn
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_raw_database_url_secret_arn" {
  name      = "/raksha/${var.environment}/core/raw_database_url_secret_arn"
  type      = "String"
  value     = local.raw_database_url_secret_arn
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_service_discovery_namespace" {
  name      = "/raksha/${var.environment}/core/service_discovery_namespace"
  type      = "String"
  value     = module.compute.service_discovery_namespace_name
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_cluster_name" {
  name      = "/raksha/${var.environment}/core/cluster_name"
  type      = "String"
  value     = module.compute.cluster_name
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_contract_version" {
  name      = "/raksha/${var.environment}/core/contract_version"
  type      = "String"
  value     = local.core_contract_version
  overwrite = true
  tags      = var.tags
}

module "cost_controls" {
  source = "../../modules/cost-controls"

  environment                            = var.environment
  budget_limit_usd                       = var.budget_limit_usd
  alert_email_addresses                  = var.alarm_emails
  anomaly_total_impact_absolute_usd      = var.anomaly_total_impact_absolute_usd
  enable_billing_estimated_charges_alarm = var.enable_billing_estimated_charges_alarm
  billing_estimated_charges_alarm_usd    = var.billing_estimated_charges_alarm_usd
  create_anomaly_monitor                 = false
  name_prefix                            = "raksha-core"
  tags                                   = var.tags
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

  environment                    = var.environment
  github_org                     = var.github_org
  github_repo                    = var.github_repo
  github_additional_repositories = var.github_additional_repositories
  github_allowed_branches        = var.github_allowed_branches
  github_allowed_environments    = var.github_allowed_environments
  create_oidc_provider           = var.create_oidc_provider
  oidc_provider_arn              = var.oidc_provider_arn
  tags                           = var.tags
}

resource "aws_ssm_parameter" "core_service_discovery_namespace_id" {
  name      = "/raksha/${var.environment}/core/service_discovery_namespace_id"
  type      = "String"
  value     = module.compute.service_discovery_namespace_id
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_vpc_id" {
  name      = "/raksha/${var.environment}/core/vpc_id"
  type      = "String"
  value     = module.network.vpc_id
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_public_subnet_ids" {
  name      = "/raksha/${var.environment}/core/public_subnet_ids"
  type      = "StringList"
  value     = join(",", module.network.public_subnet_ids)
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_private_subnet_ids" {
  name      = "/raksha/${var.environment}/core/private_subnet_ids"
  type      = "StringList"
  value     = join(",", module.network.private_subnet_ids)
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_ecs_tasks_sg_id" {
  name      = "/raksha/${var.environment}/core/ecs_tasks_sg_id"
  type      = "String"
  value     = module.security.ecs_tasks_sg_id
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_ecs_instances_sg_id" {
  name      = "/raksha/${var.environment}/core/ecs_instances_sg_id"
  type      = "String"
  value     = module.security.ecs_instances_sg_id
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_alb_public_sg_id" {
  name      = "/raksha/${var.environment}/core/alb_public_sg_id"
  type      = "String"
  value     = module.security.alb_public_sg_id
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_alb_admin_internal_sg_id" {
  name      = "/raksha/${var.environment}/core/alb_admin_internal_sg_id"
  type      = "String"
  value     = module.security.alb_admin_internal_sg_id
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_ecs_task_execution_role_arn" {
  name      = "/raksha/${var.environment}/core/ecs_task_execution_role_arn"
  type      = "String"
  value     = module.compute.ecs_task_execution_role_arn
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_ecs_task_role_arn" {
  name      = "/raksha/${var.environment}/core/ecs_task_role_arn"
  type      = "String"
  value     = module.compute.ecs_task_role_arn
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_github_images_role_arn" {
  name      = "/raksha/${var.environment}/core/github_images_role_arn"
  type      = "String"
  value     = module.cicd_iam.images_role_arn
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_github_infra_role_arn" {
  name      = "/raksha/${var.environment}/core/github_infra_role_arn"
  type      = "String"
  value     = module.cicd_iam.infra_role_arn
  overwrite = true
  tags      = var.tags
}

resource "aws_ssm_parameter" "core_github_deploy_role_arn" {
  name      = "/raksha/${var.environment}/core/github_deploy_role_arn"
  type      = "String"
  value     = module.cicd_iam.deploy_role_arn
  overwrite = true
  tags      = var.tags
}
