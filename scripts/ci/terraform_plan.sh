#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd terraform
install_cancel_trap

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
IMAGE_TAG_INPUT="${2:-${IMAGE_TAG:-latest}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment> [image_tag]"

"${SCRIPT_DIR}/terraform_init.sh" "${ENVIRONMENT}"
log "terraform phase: init complete; starting plan (${ENVIRONMENT})"

TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")
PLAN_FILE="${TF_DIR}/tfplan"
PLAN_TXT="${TF_DIR}/plan.txt"

tfvars_image_tag() {
  local tfvars_path="${TF_DIR}/terraform.tfvars"
  [[ -f "${tfvars_path}" ]] || return 0
  awk '
    $0 ~ /^[[:space:]]*image_tag[[:space:]]*=/ {
      line=$0
      sub(/^[^=]*=[[:space:]]*/, "", line)
      sub(/[[:space:]]*(#.*)?$/, "", line)
      gsub(/^"/, "", line)
      gsub(/"$/, "", line)
      print line
      exit
    }
  ' "${tfvars_path}"
}

resolve_image_tag_input() {
  local requested_tag="$1"
  local configured_tag
  configured_tag=$(trim_whitespace "$(tfvars_image_tag)")

  if [[ "${requested_tag}" == "latest" && -n "${configured_tag}" && "${configured_tag}" != "latest" ]]; then
    log "terraform plan: replacing image_tag=latest with terraform.tfvars image_tag=${configured_tag}" >&2
    printf '%s\n' "${configured_tag}"
    return 0
  fi

  printf '%s\n' "${requested_tag}"
}

IMAGE_TAG_INPUT=$(resolve_image_tag_input "${IMAGE_TAG_INPUT}")

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
