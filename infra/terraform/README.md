# DeFi Surv Terraform Infrastructure

This directory is the new IaC baseline for phased AWS deployment:

1. `test`: minimum-cost environment for rapid validation.
2. `stage` and `prod`: hardened environments with managed data, security controls, and controlled deployment posture.

## Layout

- `modules/network`: VPC, subnets, routes, and optional NAT.
- `modules/security`: security groups for ALB, ECS, database, and Redis.
- `modules/compute-ecs`: ECS cluster, ECR repos, ECS task/service definitions from `infra/service-catalog.yaml`, ALBs, and optional ECS EC2 capacity.
- `modules/data-test`: low-cost test data wiring (Secrets Manager + backup bucket).
- `modules/data-prod`: managed RDS and ElastiCache with encrypted secrets.
- `modules/observability`: CloudWatch log groups, alarms, dashboard.
- `modules/cost-controls`: AWS Budgets + Cost Anomaly Detection + SNS notifications.
- `modules/cicd-iam`: GitHub Actions OIDC roles for image, infra, and deploy workflows.
- `environments/test`: single-account, lowest-cost baseline.
- `environments/stage`: pre-production hardened baseline.
- `environments/prod`: production baseline.

## Service Catalog

`infra/service-catalog.yaml` is the source of truth for runtime services and includes:

- `service_name`
- `image_repo`
- `container_port`
- `health_path`
- `exposure`
- `deploy_strategy`
- `scaling_class`
- `cpu`
- `memory`

## Usage

```bash
cd infra/terraform/environments/test
terraform init
terraform plan -var-file=terraform.tfvars
terraform apply -var-file=terraform.tfvars
```

Repeat for `stage` and `prod` directories.

## Notes

- `test` defaults to `compute_mode = "ec2"` and `enable_managed_data = false` for cost minimization.
- `stage` and `prod` default to `compute_mode = "fargate_mix"` and managed data.
- `prod` requires `public_certificate_arn`.
- OIDC provider creation should happen once per account; set `create_oidc_provider = true` only in one environment per account.

## CloudFormation/Terraform Interoperability

This structure is intentionally modular and environment-scoped so migration paths remain open:

- Keep stable naming conventions and module boundaries.
- Avoid dual-management of the same resource in CloudFormation and Terraform at the same time.
- For migration, import existing resources into the target tool before cutting over ownership.
