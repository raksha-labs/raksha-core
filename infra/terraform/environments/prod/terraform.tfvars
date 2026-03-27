aws_region             = "eu-west-1"
environment            = "prod"
compute_mode           = "fargate_mix"
budget_limit_usd       = 1000
alarm_emails           = ["dumindu@rakshalabs.io"]
admin_access_mode      = "private-only"
enable_public_https    = true
public_certificate_arn = "arn:aws:acm:eu-west-1:988508076735:certificate/a26e820d-967c-4081-801b-a9965af9a3b8"
enable_waf             = true
image_tag              = "latest"

github_org              = "raksha-labs"
github_repo             = "raksha-core"
github_allowed_branches = ["master", "release/*"]

create_oidc_provider = false
# oidc_provider_arn  = "arn:aws:iam::<account-id>:oidc-provider/token.actions.githubusercontent.com"
