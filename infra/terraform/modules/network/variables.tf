variable "environment" {
  description = "Environment name (test/stage/prod)"
  type        = string
}

variable "vpc_cidr" {
  description = "CIDR block for the VPC"
  type        = string
}

variable "single_az" {
  description = "Whether to use a single availability zone"
  type        = bool
  default     = false
}

variable "az_count" {
  description = "Number of AZs to use when single_az is false"
  type        = number
  default     = 2
}

variable "create_nat_gateway" {
  description = "Whether to create NAT gateway egress from private subnets"
  type        = bool
  default     = true
}

variable "nat_gateway_per_az" {
  description = "Whether to create one NAT gateway per AZ"
  type        = bool
  default     = true
}

variable "tags" {
  description = "Common resource tags"
  type        = map(string)
  default     = {}
}
