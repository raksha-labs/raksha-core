# Auto-import resources that already exist in AWS but are missing from Terraform state.
# These import blocks run automatically during `tofu plan` / `tofu apply`.
# Remove an import block once the resource has been successfully imported.

import {
  to = module.data_test[0].aws_secretsmanager_secret.database
  id = "raksha/test/shared/DATABASE_URL"
}

import {
  to = module.data_test[0].aws_secretsmanager_secret.raw_database
  id = "raksha/test/shared/RAW_DATABASE_URL"
}

import {
  to = module.data_test[0].aws_secretsmanager_secret.redis
  id = "raksha/test/shared/REDIS_URL"
}

import {
  to = module.observability.aws_cloudwatch_log_group.service["indexer"]
  id = "/ecs/raksha/test/indexer"
}

import {
  to = module.observability.aws_cloudwatch_log_group.service["detector"]
  id = "/ecs/raksha/test/detector"
}

import {
  to = module.observability.aws_cloudwatch_log_group.service["orchestrator"]
  id = "/ecs/raksha/test/orchestrator"
}

import {
  to = module.observability.aws_cloudwatch_log_group.service["finality"]
  id = "/ecs/raksha/test/finality"
}

import {
  to = module.observability.aws_cloudwatch_log_group.service["history-worker"]
  id = "/ecs/raksha/test/history-worker"
}

import {
  to = module.observability.aws_cloudwatch_log_group.service["postgres"]
  id = "/ecs/raksha/test/postgres"
}

import {
  to = module.observability.aws_cloudwatch_log_group.service["redis"]
  id = "/ecs/raksha/test/redis"
}

import {
  to = module.compute.aws_iam_role.ecs_task_execution
  id = "raksha-test-ecs-task-execution-role"
}

import {
  to = module.compute.aws_iam_role.ecs_task
  id = "raksha-test-ecs-task-role"
}

import {
  to = module.compute.aws_iam_role.ecs_instance
  id = "raksha-test-ecs-instance-role"
}

import {
  to = module.compute.aws_iam_instance_profile.ecs_instance
  id = "raksha-test-ecs-instance-profile"
}

import {
  to = module.cicd_iam.aws_iam_role.images
  id = "raksha-test-github-images-role"
}

import {
  to = module.cicd_iam.aws_iam_role.infra
  id = "raksha-test-github-infra-role"
}

import {
  to = module.cicd_iam.aws_iam_role.deploy
  id = "raksha-test-github-deploy-role"
}

import {
  to = module.compute.aws_ecr_repository.services["indexer"]
  id = "raksha-indexer"
}

import {
  to = module.compute.aws_ecr_repository.services["detector"]
  id = "raksha-detector"
}

import {
  to = module.compute.aws_ecr_repository.services["orchestrator"]
  id = "raksha-orchestrator"
}

import {
  to = module.compute.aws_ecr_repository.services["finality"]
  id = "raksha-finality"
}

import {
  to = module.compute.aws_ecr_repository.services["history-worker"]
  id = "raksha-history-worker"
}
