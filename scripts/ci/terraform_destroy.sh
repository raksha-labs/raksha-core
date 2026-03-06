#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd terraform

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
IMAGE_TAG_INPUT="${2:-${IMAGE_TAG:-latest}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment> [image_tag]"

"${SCRIPT_DIR}/terraform_init.sh" "${ENVIRONMENT}"
log "terraform phase: init complete; starting destroy (${ENVIRONMENT})"

TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")

log "terraform destroy (${ENVIRONMENT}) image_tag=${IMAGE_TAG_INPUT}"
terraform -chdir="${TF_DIR}" destroy \
  -input=false \
  -auto-approve \
  -lock=true \
  -var="image_tag=${IMAGE_TAG_INPUT}"
log "terraform phase: destroy complete (${ENVIRONMENT})"
