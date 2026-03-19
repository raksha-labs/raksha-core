#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd aws

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
IMAGE_TAG="${2:-${IMAGE_TAG:-}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment> <image_tag>"
[[ -n "${IMAGE_TAG}" ]] || fail "usage: $0 <environment> <image_tag>"

AWS_REGION="${AWS_REGION:-eu-west-1}"
APPLY_INFRA="${APPLY_INFRA:-false}"
SERVICE_FILTER_NORMALIZED=$(normalize_csv_filter "${SERVICE_FILTER:-}")
AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)

required_repositories() {
  local service
  while IFS= read -r service; do
    [[ -n "${service}" ]] || continue
    if [[ "${APPLY_INFRA}" != "true" ]] && [[ -n "${SERVICE_FILTER_NORMALIZED}" ]] && ! is_selected_service "${service}"; then
      continue
    fi
    printf 'raksha-%s\n' "${service}"
  done < <(catalog_services)
}

image_exists_in_repo() {
  local repository="$1"
  aws ecr describe-images \
    --repository-name "${repository}" \
    --image-ids "imageTag=${IMAGE_TAG}" \
    --region "${AWS_REGION}" \
    --query 'imageDetails[0].imageDigest' \
    --output text >/dev/null 2>&1
}

missing_repositories=()
checked_count=0
while IFS= read -r repository; do
  [[ -n "${repository}" ]] || continue
  checked_count=$((checked_count + 1))
  if ! image_exists_in_repo "${repository}"; then
    missing_repositories+=("${repository}")
  fi
done < <(required_repositories)

(( checked_count > 0 )) || fail "no repositories selected for image tag validation"

if (( ${#missing_repositories[@]} > 0 )); then
  fail "image tag ${IMAGE_TAG} is missing from ${#missing_repositories[@]} required repositories in ${AWS_REGION} for account ${AWS_ACCOUNT_ID}: ${missing_repositories[*]}. Build images for this tag in the same account before rollout or rerun with build_images=true."
fi

log "validated image tag ${IMAGE_TAG} across ${checked_count} required repositories in account ${AWS_ACCOUNT_ID}"
