#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd terraform

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment>"

TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")
[[ -d "${TF_DIR}" ]] || fail "terraform environment directory not found: ${TF_DIR}"

if [[ "${TERRAFORM_INIT_BACKEND_FALSE:-false}" == "true" ]]; then
  log "terraform init with backend disabled (${ENVIRONMENT})"
  terraform -chdir="${TF_DIR}" init -input=false -backend=false
  exit 0
fi

require_env TF_BACKEND_BUCKET
require_env TF_BACKEND_DYNAMODB_TABLE

REPO_NAME=$(basename "${REPO_ROOT}")
TF_BACKEND_REGION="${TF_BACKEND_REGION:-${AWS_REGION:-eu-west-1}}"
TF_STATE_KEY="${TF_STATE_KEY:-${TF_BACKEND_KEY_PREFIX:-raksha}/${REPO_NAME}/${ENVIRONMENT}/terraform.tfstate}"

init_args=(
  -chdir="${TF_DIR}"
  init
  -input=false
  -reconfigure
  -backend-config="bucket=${TF_BACKEND_BUCKET}"
  -backend-config="key=${TF_STATE_KEY}"
  -backend-config="region=${TF_BACKEND_REGION}"
  -backend-config="dynamodb_table=${TF_BACKEND_DYNAMODB_TABLE}"
  -backend-config="encrypt=true"
)

if [[ -n "${TF_BACKEND_KMS_KEY_ID:-}" ]]; then
  init_args+=("-backend-config=kms_key_id=${TF_BACKEND_KMS_KEY_ID}")
fi

if [[ -n "${TF_BACKEND_ROLE_ARN:-}" ]]; then
  init_args+=("-backend-config=role_arn=${TF_BACKEND_ROLE_ARN}")
fi

log "terraform init (${ENVIRONMENT}) using remote backend key ${TF_STATE_KEY}"
terraform "${init_args[@]}"
