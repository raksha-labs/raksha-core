# DeFi Surveillance - Terraform Infrastructure

This directory contains the complete AWS infrastructure configuration for the DeFi Surveillance platform.

## Architecture Overview

The Terraform configuration deploys:

- **VPC**: Multi-AZ VPC with public and private subnets
- **ECS Cluster**: Fargate-based container orchestration
- **RDS PostgreSQL**: Multi-AZ database with automatic backups
- **ElastiCache Redis**: Redis cluster for event streams and caching
- **Application Load Balancer**: HTTPS-enabled load balancer with WAF
- **Security Groups**: Least-privilege network access controls
- **IAM Roles**: Task execution and application roles
- **ECR Repositories**: Container image storage
- **Secrets Manager**: Secure credential storage
- **CloudWatch**: Logging and monitoring

## Prerequisites

1. **AWS CLI** configured with appropriate credentials
2. **Terraform** >= 1.6.0 installed
3. **AWS Account** with permissions to create resources
4. **Domain name** (optional, for HTTPS certificate)

## Quick Start

### 1. Initialize Terraform

```bash
cd defi-surv-core/infra/terraform
terraform init
```

### 2. Configure Variables

Create a `terraform.tfvars` file:

```hcl
aws_region        = "eu-west-1"
environment       = "production"
db_instance_class = "db.t4g.medium"
cache_node_type   = "cache.t4g.medium"
```

### 3. Plan Deployment

```bash
terraform plan -out=tfplan
```

Review the plan carefully before applying.

### 4. Apply Infrastructure

```bash
terraform apply tfplan
```

This will take ~15-20 minutes to provision all resources.

### 5. Retrieve Outputs

```bash
# Get all outputs
terraform output

# Get specific outputs
terraform output alb_dns_name
terraform output rds_endpoint
terraform output redis_primary_endpoint
```

## Configuration

### Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `aws_region` | AWS region | `eu-west-1` |
| `environment` | Environment name | `production` |
| `vpc_cidr` | VPC CIDR block | `10.0.0.0/16` |
| `db_instance_class` | RDS instance type | `db.t4g.medium` |
| `db_name` | Database name | `defi_surv` |
| `cache_node_type` | ElastiCache node type | `cache.t4g.medium` |
| `cache_num_nodes` | Number of cache nodes | `2` |
| `ecs_task_cpu` | CPU units for tasks | `512` |
| `ecs_task_memory` | Memory for tasks (MB) | `1024` |

### Secrets Management

Database and Redis credentials are automatically generated and stored in AWS Secrets Manager:

- `defi-surv-database-url-<env>`: Full PostgreSQL connection string
- `defi-surv-redis-url-<env>`: Redis connection endpoint
- `defi-surv-db-password-<env>`: Database master password

Retrieve secrets:

```bash
aws secretsmanager get-secret-value \
  --secret-id defi-surv-database-url-production \
  --query SecretString \
  --output text | jq -r .DATABASE_URL
```

### SSL/TLS Certificate

Update the ACM certificate configuration in `alb.tf`:

```hcl
resource "aws_acm_certificate" "main" {
  domain_name       = "api.your-domain.com"  # Update this
  validation_method = "DNS"
}
```

After applying, add the DNS validation records to your domain's DNS.

## Cost Estimation

**Monthly costs** (approximate, eu-west-1):

| Resource | Configuration | Monthly Cost |
|----------|--------------|--------------|
| RDS PostgreSQL | db.t4g.medium, Multi-AZ | ~$140 |
| ElastiCache Redis | 2x cache.t4g.medium | ~$90 |
| ECS Fargate | 10 tasks @ 0.5 vCPU, 1GB | ~$35 |
| ALB | Application Load Balancer | ~$20 |
| NAT Gateways | 2x NAT Gateways | ~$65 |
| Data Transfer | ~100GB egress | ~$9 |
| **Total** | | **~$360/month** |

## State Management

### Recommended: S3 Backend

Create S3 bucket and DynamoDB table:

```bash
aws s3 mb s3://defi-surv-terraform-state --region eu-west-1
aws s3api put-bucket-versioning \
  --bucket defi-surv-terraform-state \
  --versioning-configuration Status=Enabled

aws dynamodb create-table \
  --table-name terraform-locks \
  --attribute-definitions AttributeName=LockID,AttributeType=S \
  --key-schema AttributeName=LockID,KeyType=HASH \
  --billing-mode PAY_PER_REQUEST \
  --region eu-west-1
```

Uncomment the backend configuration in `main.tf`:

```hcl
terraform {
  backend "s3" {
    bucket         = "defi-surv-terraform-state"
    key            = "production/terraform.tfstate"
    region         = "eu-west-1"
    encrypt        = true
    dynamodb_table = "terraform-locks"
  }
}
```

Then re-initialize:

```bash
terraform init -migrate-state
```

## Deployment Workflow

### 1. Deploy Infrastructure

```bash
terraform apply
```

### 2. Build and Push Docker Images

```bash
# Login to ECR
aws ecr get-login-password --region eu-west-1 | \
  docker login --username AWS --password-stdin $(terraform output -raw ecr_repository_urls | jq -r .ingestion | cut -d/ -f1)

# Build and push (example for ingestion worker)
cd ../../
docker build -f Dockerfile --build-arg WORKER=ingestion -t defi-surv-ingestion .
docker tag defi-surv-ingestion:latest $(terraform output -raw ecr_repository_urls | jq -r .ingestion)
docker push $(terraform output -raw ecr_repository_urls | jq -r .ingestion)
```

### 3. Deploy ECS Services

ECS task definitions and services are deployed via GitHub Actions (see `.github/workflows/build.yml`).

Manual deployment:

```bash
# Create task definition (example)
aws ecs register-task-definition --cli-input-json file://task-definition.json

# Create/update service
aws ecs create-service \
  --cluster defi-surv-production \
  --service-name ingestion \
  --task-definition defi-surv-ingestion:1 \
  --desired-count 1 \
  --launch-type FARGATE \
  --network-configuration "awsvpcConfiguration={subnets=[subnet-xxx],securityGroups=[sg-xxx],assignPublicIp=DISABLED}"
```

## Monitoring

### CloudWatch Dashboards

Access logs:

```bash
aws logs tail /ecs/defi-surv/detection-engine --follow
```

### Metrics

Key metrics to monitor:
- ECS CPU/Memory utilization
- RDS connections, read/write latency
- Redis cache hit rate, CPU
- ALB request count, latency, errors
- WAF blocked requests

## Disaster Recovery

### Database Backups

RDS automatically creates daily backups (7-day retention). Manual snapshot:

```bash
aws rds create-db-snapshot \
  --db-instance-identifier defi-surv-db \
  --db-snapshot-identifier defi-surv-manual-snapshot-$(date +%Y%m%d)
```

### Redis Backups

ElastiCache creates daily snapshots (5-day retention).

### Restore Process

1. Restore RDS from snapshot
2. Update Secrets Manager with new endpoint
3. Restart ECS services to pick up new credentials

## Cleanup

**WARNING**: This will destroy all resources and data.

```bash
# Disable deletion protection first
terraform apply -var="db_deletion_protection=false"

# Destroy all resources
terraform destroy
```

## Troubleshooting

### ECS Tasks Not Starting

Check task logs:
```bash
aws ecs describe-tasks --cluster defi-surv-production --tasks <task-id>
```

Common issues:
- Missing IAM permissions for Secrets Manager
- ECR image pull failures
- Insufficient memory/CPU limits

### RDS Connection Issues

Verify security group rules:
```bash
aws ec2 describe-security-groups --group-ids <sg-id>
```

Test connectivity from ECS task:
```bash
# Exec into running container
aws ecs execute-command --cluster defi-surv-production --task <task-id> --interactive --command "/bin/sh"

# Test PostgreSQL connection
pg_isready -h <rds-endpoint> -p 5432
```

### High Costs

- Enable RDS instance auto-pause for non-prod environments
- Use Fargate Spot for non-critical workloads
- Review CloudWatch log retention periods
- Consider Reserved Instances for predictable workloads

## Support

For issues or questions:
- Check Terraform documentation: https://registry.terraform.io/providers/hashicorp/aws/latest/docs
- AWS Support: https://console.aws.amazon.com/support/home
- Internal wiki: [Add your wiki URL]

## License

[Add your license information]
