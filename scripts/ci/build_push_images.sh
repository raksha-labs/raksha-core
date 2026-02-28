#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd aws
require_cmd docker

AWS_REGION="${AWS_REGION:-eu-west-1}"
IMAGE_TAG="${IMAGE_TAG:-sha-${GITHUB_SHA:-local}}"
PUSH_LATEST="${PUSH_LATEST:-false}"

ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
REGISTRY="${ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"

log "logging in to ECR ${REGISTRY}"
aws ecr get-login-password --region "${AWS_REGION}" | docker login --username AWS --password-stdin "${REGISTRY}"

BUNDLE_IMAGE="defi-surv-core-bundle:${IMAGE_TAG}"
log "building shared core runtime image from Dockerfile"
docker build -f "${REPO_ROOT}/Dockerfile" -t "${BUNDLE_IMAGE}" "${REPO_ROOT}"

while IFS= read -r service; do
  [[ -n "${service}" ]] || continue

  if ! is_selected_service "${service}"; then
    continue
  fi

  repo="defi-surv-${service}"
  if ! aws ecr describe-repositories --repository-names "${repo}" --region "${AWS_REGION}" >/dev/null 2>&1; then
    log "creating missing ECR repository ${repo}"
    aws ecr create-repository --repository-name "${repo}" --image-scanning-configuration scanOnPush=true --region "${AWS_REGION}" >/dev/null
  fi

  remote_image="${REGISTRY}/${repo}:${IMAGE_TAG}"
  log "pushing ${remote_image}"
  docker tag "${BUNDLE_IMAGE}" "${remote_image}"
  docker push "${remote_image}"

  if [[ "${PUSH_LATEST}" == "true" ]]; then
    latest_image="${REGISTRY}/${repo}:latest"
    log "pushing ${latest_image}"
    docker tag "${BUNDLE_IMAGE}" "${latest_image}"
    docker push "${latest_image}"
  fi
done < <(catalog_services)
