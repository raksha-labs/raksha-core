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

module "network" {
  source = "../../modules/network"

  environment        = var.environment
  vpc_cidr           = var.vpc_cidr
  single_az          = false
  az_count           = var.az_count
  create_nat_gateway = var.create_nat_gateway
  nat_gateway_per_az = var.nat_gateway_per_az
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

module "data_prod" {
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
  database_url_secret_arn = module.data_prod.database_url_secret_arn
  redis_url_secret_arn    = module.data_prod.redis_url_secret_arn

  service_static_env = {
    orchestrator = {
      ALERT_FALLBACK_TENANT_ID = "glider"
      NOTIFIER_GATEWAY_URL     = "http://notifier-gateway:3002"
    }
    finality = {
      ALERT_FALLBACK_TENANT_ID = "glider"
    }
  }

  service_secret_env = {
    for service_name in keys(local.service_catalog_map) :
    service_name => {
      DATABASE_URL = "${local.database_url_secret_arn}:DATABASE_URL::"
      REDIS_URL    = "${local.redis_url_secret_arn}:REDIS_URL::"
    }
  }
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
  task_subnet_ids              = module.network.private_subnet_ids
  ec2_subnet_ids               = module.network.private_subnet_ids
  public_alb_subnet_ids        = module.network.public_subnet_ids
  admin_alb_subnet_ids         = module.network.private_subnet_ids
  ecs_tasks_sg_id              = module.security.ecs_tasks_sg_id
  ecs_instances_sg_id          = module.security.ecs_instances_sg_id
  alb_public_sg_id             = module.security.alb_public_sg_id
  alb_admin_internal_sg_id     = module.security.alb_admin_internal_sg_id
  secret_prefix                = local.secret_prefix
  image_tag                    = var.image_tag
  assign_public_ip             = false
  public_default_service       = var.public_default_service
  admin_default_service        = var.admin_default_service
  admin_access_mode            = var.admin_access_mode
  enable_public_https          = var.enable_public_https
  public_certificate_arn       = var.public_certificate_arn
  enable_test_data_services    = false
  fargate_spot_scaling_classes = var.fargate_spot_scaling_classes
  service_static_env           = local.service_static_env
  service_secret_env           = local.service_secret_env
  tags                         = var.tags
}

locals {
  core_contract_version = sha256(join("|", [
    var.environment,
    var.image_tag,
    local.database_url_secret_arn,
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

resource "aws_wafv2_web_acl" "public" {
  count = var.enable_waf && module.compute.public_alb_arn != null ? 1 : 0

  name  = "raksha-${var.environment}-waf"
  scope = "REGIONAL"

  default_action {
    allow {}
  }

  rule {
    name     = "RateLimit"
    priority = 1

    action {
      block {}
    }

    statement {
      rate_based_statement {
        limit              = var.waf_rate_limit
        aggregate_key_type = "IP"
      }
    }

    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "raksha-${var.environment}-rate-limit"
      sampled_requests_enabled   = true
    }
  }

  rule {
    name     = "AWSManagedRulesCommonRuleSet"
    priority = 2

    override_action {
      none {}
    }

    statement {
      managed_rule_group_statement {
        name        = "AWSManagedRulesCommonRuleSet"
        vendor_name = "AWS"
      }
    }

    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "raksha-${var.environment}-common-rules"
      sampled_requests_enabled   = true
    }
  }

  visibility_config {
    cloudwatch_metrics_enabled = true
    metric_name                = "raksha-${var.environment}-waf"
    sampled_requests_enabled   = true
  }

  tags = var.tags
}

resource "aws_wafv2_web_acl_association" "public" {
  count = var.enable_waf && module.compute.public_alb_arn != null ? 1 : 0

  resource_arn = module.compute.public_alb_arn
  web_acl_arn  = aws_wafv2_web_acl.public[0].arn
}

module "cost_controls" {
  source = "../../modules/cost-controls"

  environment           = var.environment
  budget_limit_usd      = var.budget_limit_usd
  alert_email_addresses = var.alarm_emails
  name_prefix           = "raksha"
  tags                  = var.tags
}

module "observability" {
  source = "../../modules/observability"

  environment         = var.environment
  cluster_name        = module.compute.cluster_name
  service_names       = keys(local.service_catalog_map)
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
