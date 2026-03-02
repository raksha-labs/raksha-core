variable "environment" {
  description = "Environment name"
  type        = string
}

variable "github_org" {
  description = "GitHub organization or owner"
  type        = string
}

variable "github_repo" {
  description = "GitHub repository name"
  type        = string
}

variable "github_allowed_branches" {
  description = "Allowed branch names for OIDC role assumption"
  type        = list(string)
  default     = ["master"]
}

variable "allow_pull_request" {
  description = "Allow pull_request subject to assume plan role"
  type        = bool
  default     = true
}

variable "allow_tag_refs" {
  description = "Allow tag refs to assume roles"
  type        = bool
  default     = true
}

variable "github_allowed_environments" {
  description = "GitHub environments allowed to assume OIDC roles"
  type        = list(string)
  default     = ["test", "stage", "prod"]
}

variable "create_oidc_provider" {
  description = "Create GitHub OIDC provider in account"
  type        = bool
  default     = false
}

variable "oidc_provider_arn" {
  description = "Existing GitHub OIDC provider ARN"
  type        = string
  default     = null
}

variable "tags" {
  description = "Common tags"
  type        = map(string)
  default     = {}
}
