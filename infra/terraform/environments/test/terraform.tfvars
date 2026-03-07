aws_region          = "eu-west-1"
environment         = "test"
compute_mode        = "fargate_mix"
create_nat_gateway  = true
enable_managed_data = true
budget_limit_usd    = 100
alarm_emails        = []
admin_access_mode   = "private-only"
image_tag           = "master-latest"

github_org                  = "raksha-labs"
github_repo                 = "raksha-core"
github_allowed_branches     = ["master"]
github_allowed_environments = ["test", "stage", "prod"]
create_oidc_provider        = false

# Stream-processing pipeline — flip to true to also start indexer/detector/etc.
streams_enabled = true

# RPC WebSocket URLs for the indexer, sourced from Secrets Manager.
# Secret created: aws secretsmanager create-secret --name raksha/test/rpc ...
# ARN: arn:aws:secretsmanager:eu-west-1:988508076735:secret:raksha/test/rpc-1o8W7Y
rpc_ws_url_secret_arns = {
  ETH_WS_URL  = "arn:aws:secretsmanager:eu-west-1:988508076735:secret:raksha/test/rpc-1o8W7Y:ETH_WS_URL::"
  BASE_WS_URL = "arn:aws:secretsmanager:eu-west-1:988508076735:secret:raksha/test/rpc-1o8W7Y:BASE_WS_URL::"
}
