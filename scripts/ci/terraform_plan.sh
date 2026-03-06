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
log "terraform phase: init complete; starting plan (${ENVIRONMENT})"

TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")
PLAN_FILE="${TF_DIR}/tfplan"
PLAN_TXT="${TF_DIR}/plan.txt"

log "terraform plan (${ENVIRONMENT}) image_tag=${IMAGE_TAG_INPUT}"
set -o pipefail
terraform -chdir="${TF_DIR}" plan \
  -input=false \
  -lock=true \
  -refresh=true \
  -no-color \
  -var="image_tag=${IMAGE_TAG_INPUT}" \
  -out="${PLAN_FILE}" | tee "${PLAN_TXT}"
log "terraform phase: plan complete (${ENVIRONMENT}) plan_file=${PLAN_FILE}"
