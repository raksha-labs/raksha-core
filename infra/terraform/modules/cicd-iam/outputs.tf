output "oidc_provider_arn" {
  value       = local.effective_oidc_provider_arn
  description = "OIDC provider ARN used by GitHub Actions"
}

output "images_role_arn" {
  value       = aws_iam_role.images.arn
  description = "Role ARN for image build/push workflow"
}

output "infra_role_arn" {
  value       = aws_iam_role.infra.arn
  description = "Role ARN for Terraform plan/apply workflow"
}

output "deploy_role_arn" {
  value       = aws_iam_role.deploy.arn
  description = "Role ARN for deployment workflow"
}

output "trusted_github_subjects" {
  value       = local.subjects
  description = "GitHub OIDC subjects trusted by this module"
}
