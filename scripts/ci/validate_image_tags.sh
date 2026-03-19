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
AUTO_BUILD_MISSING_IMAGES="${AUTO_BUILD_MISSING_IMAGES:-false}"
IMAGE_VALIDATION_MAX_ATTEMPTS="${IMAGE_VALIDATION_MAX_ATTEMPTS:-12}"
IMAGE_VALIDATION_RETRY_SLEEP_SECONDS="${IMAGE_VALIDATION_RETRY_SLEEP_SECONDS:-5}"
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

collect_missing_repositories() {
  missing_repositories=()
  checked_count=0
  while IFS= read -r repository; do
    [[ -n "${repository}" ]] || continue
    checked_count=$((checked_count + 1))
    if ! image_exists_in_repo "${repository}"; then
      missing_repositories+=("${repository}")
    fi
  done < <(required_repositories)
}

missing_services_csv() {
  local repository service csv=""
  for repository in "${missing_repositories[@]}"; do
    service="${repository#raksha-}"
    if [[ -n "${csv}" ]]; then
      csv+=","
    fi
    csv+="${service}"
  done
  printf '%s\n' "${csv}"
}

auto_build_missing_images() {
  [[ "${AUTO_BUILD_MISSING_IMAGES}" == "true" ]] || return 0
  (( ${#missing_repositories[@]} > 0 )) || return 0

  require_cmd docker

  local missing_services
  missing_services=$(missing_services_csv)
  [[ -n "${missing_services}" ]] || return 0

  log "image tag ${IMAGE_TAG} missing in account ${AWS_ACCOUNT_ID}; rebuilding services inline: ${missing_services}"
  SERVICE_FILTER="${missing_services}" IMAGE_TAG="${IMAGE_TAG}" AWS_REGION="${AWS_REGION}" "${SCRIPT_DIR}/build_push_images.sh"
}

wait_for_images_to_become_visible() {
  local attempt=1

  while (( attempt <= IMAGE_VALIDATION_MAX_ATTEMPTS )); do
    collect_missing_repositories
    if (( ${#missing_repositories[@]} == 0 )); then
      return 0
    fi

    if (( attempt == IMAGE_VALIDATION_MAX_ATTEMPTS )); then
      return 1
    fi

    log "waiting for image tag ${IMAGE_TAG} to become visible in ECR (attempt ${attempt}/${IMAGE_VALIDATION_MAX_ATTEMPTS}); still missing: ${missing_repositories[*]}"
    sleep "${IMAGE_VALIDATION_RETRY_SLEEP_SECONDS}"
    attempt=$((attempt + 1))
  done

  return 1
}

collect_missing_repositories

(( checked_count > 0 )) || fail "no repositories selected for image tag validation"

if (( ${#missing_repositories[@]} > 0 )); then
  auto_build_missing_images
  wait_for_images_to_become_visible || true
fi

if (( ${#missing_repositories[@]} > 0 )); then
  fail "image tag ${IMAGE_TAG} is missing from ${#missing_repositories[@]} required repositories in ${AWS_REGION} for account ${AWS_ACCOUNT_ID}: ${missing_repositories[*]}. Build images for this tag in the same account before rollout or rerun with build_images=true."
fi

log "validated image tag ${IMAGE_TAG} across ${checked_count} required repositories in account ${AWS_ACCOUNT_ID}"
