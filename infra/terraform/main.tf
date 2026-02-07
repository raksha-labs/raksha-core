terraform {
  required_version = ">= 1.6.0"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

resource "aws_ecs_cluster" "defi_surv" {
  name = "defi-surv-mvp"
}

resource "aws_cloudwatch_log_group" "detector" {
  name              = "/ecs/defi-surv/detector-worker"
  retention_in_days = 14
}
