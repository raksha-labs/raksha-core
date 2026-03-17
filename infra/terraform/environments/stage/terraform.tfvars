aws_region             = "eu-west-1"
environment            = "stage"
compute_mode           = "fargate_mix"
enable_managed_data    = true
budget_limit_usd       = 500
alarm_emails           = []
admin_access_mode      = "private-only"
enable_public_https    = true
public_certificate_arn = "replace-with-acm-certificate-arn"
enable_waf             = true
image_tag              = "master-latest"

github_org              = "raksha-labs"
github_repo             = "raksha-core"
github_allowed_branches = ["master", "release/*"]

create_oidc_provider = false
# oidc_provider_arn     = "arn:aws:iam::<account-id>:oidc-provider/token.actions.githubusercontent.com"

# Required for the production-like indexer in stage. Create a secret such as:
#   raksha/stage/rpc
# with JSON keys ETH_WS_URL and BASE_WS_URL, then reference the ECS valueFrom
# strings below.
rpc_ws_url_secret_arns = {
  ETH_WS_URL  = "replace-with-stage-rpc-secret-arn:ETH_WS_URL::"
  BASE_WS_URL = "replace-with-stage-rpc-secret-arn:BASE_WS_URL::"
}
