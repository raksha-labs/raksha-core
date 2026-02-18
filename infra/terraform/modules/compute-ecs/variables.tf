variable "environment" {
  description = "Environment name"
  type        = string
}

variable "aws_region" {
  description = "AWS region"
  type        = string
}

variable "service_catalog" {
  description = "Service catalog map keyed by service name"
  type        = map(any)
}

variable "service_desired_counts" {
  description = "Optional desired count overrides by service"
  type        = map(number)
  default     = {}
}

variable "service_cpu_memory" {
  description = "Optional cpu/memory overrides by service"
  type = map(object({
    cpu    = number
    memory = number
  }))
  default = {}
}

variable "compute_mode" {
  description = "Compute mode: ec2 or fargate_mix"
  type        = string
}

variable "vpc_id" {
  description = "VPC ID"
  type        = string
}

variable "task_subnet_ids" {
  description = "Subnets for ECS task ENIs"
  type        = list(string)
}

variable "ec2_subnet_ids" {
  description = "Subnets for ECS container instances when compute_mode is ec2"
  type        = list(string)
  default     = []
}

variable "public_alb_subnet_ids" {
  description = "Subnets for public ALB"
  type        = list(string)
  default     = []
}

variable "admin_alb_subnet_ids" {
  description = "Subnets for internal admin ALB"
  type        = list(string)
  default     = []
}

variable "ecs_tasks_sg_id" {
  description = "Security group for ECS tasks"
  type        = string
}

variable "ecs_instances_sg_id" {
  description = "Security group for ECS EC2 instances"
  type        = string
  default     = null
}

variable "alb_public_sg_id" {
  description = "Security group for public ALB"
  type        = string
  default     = null
}

variable "alb_admin_internal_sg_id" {
  description = "Security group for internal admin ALB"
  type        = string
  default     = null
}

variable "secret_prefix" {
  description = "Secret prefix, for example defi-surv/test"
  type        = string
}

variable "image_tag" {
  description = "Image tag to deploy"
  type        = string
  default     = "latest"
}

variable "assign_public_ip" {
  description = "Assign public IP to awsvpc task ENI"
  type        = bool
  default     = false
}

variable "public_default_service" {
  description = "Default public ALB target service"
  type        = string
  default     = "raksha-web"
}

variable "admin_default_service" {
  description = "Default internal admin ALB target service"
  type        = string
  default     = "raksha-admin"
}

variable "admin_access_mode" {
  description = "Admin access mode, for example private-only"
  type        = string
  default     = "private-only"
}

variable "enable_public_https" {
  description = "Enable HTTPS listener on public ALB"
  type        = bool
  default     = false
}

variable "public_certificate_arn" {
  description = "ACM certificate ARN for public ALB"
  type        = string
  default     = null
}

variable "enable_test_data_services" {
  description = "Create Postgres and Redis test services on ECS"
  type        = bool
  default     = false
}

variable "force_new_deployment" {
  description = "Force new deployment for ECS services"
  type        = bool
  default     = false
}

variable "enable_execute_command" {
  description = "Enable ECS Exec"
  type        = bool
  default     = true
}

variable "fargate_spot_scaling_classes" {
  description = "Scaling classes eligible for Fargate Spot"
  type        = list(string)
  default     = ["worker_cpu_and_lag"]
}

variable "ec2_instance_type" {
  description = "Instance type for ECS container instances in test mode"
  type        = string
  default     = "t4g.medium"
}

variable "ecs_ami_ssm_parameter_name" {
  description = "SSM parameter path for ECS-optimized AMI"
  type        = string
  default     = "/aws/service/ecs/optimized-ami/amazon-linux-2/arm64/recommended/image_id"
}

variable "ec2_desired_capacity" {
  description = "Desired ASG instance count for ECS EC2 cluster"
  type        = number
  default     = 1
}

variable "ec2_min_capacity" {
  description = "Minimum ASG instance count for ECS EC2 cluster"
  type        = number
  default     = 1
}

variable "ec2_max_capacity" {
  description = "Maximum ASG instance count for ECS EC2 cluster"
  type        = number
  default     = 2
}

variable "ecr_image_retention_count" {
  description = "How many images to keep in each ECR repository"
  type        = number
  default     = 25
}

variable "tags" {
  description = "Common tags"
  type        = map(string)
  default     = {}
}
