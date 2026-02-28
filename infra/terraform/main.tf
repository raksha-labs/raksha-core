terraform {
  required_version = ">= 1.6.0"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }

  # Recommended: Use S3 backend for state management
  # backend "s3" {
  #   bucket         = "defi-surv-terraform-state"
  #   key            = "production/terraform.tfstate"
  #   region         = "eu-west-1"
  #   encrypt        = true
  #   dynamodb_table = "terraform-locks"
  # }
}

provider "aws" {
  region = var.aws_region

  default_tags {
    tags = var.tags
  }
}

# ECS Cluster with Container Insights
resource "aws_ecs_cluster" "defi_surv" {
  name = "defi-surv-${var.environment}"

  setting {
    name  = "containerInsights"
    value = "enabled"
  }

  tags = var.tags
}

# CloudWatch Log Groups for all services
resource "aws_cloudwatch_log_group" "services" {
  for_each = toset([
    "indexer",
    "detector",
    "orchestrator",
    "finality",
    "api-service",
    "auth-tenants",
    "config-service",
    "integration-manager",
    "notifier-gateway",
    "policy-manager"
  ])

  name              = "/ecs/defi-surv/${each.key}"
  retention_in_days = 14

  tags = merge(var.tags, {
    Service = each.key
  })
}
