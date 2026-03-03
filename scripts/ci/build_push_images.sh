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

BUNDLE_IMAGE="raksha-core-bundle:${IMAGE_TAG}"
log "building shared core runtime image from Dockerfile"
docker build -f "${REPO_ROOT}/Dockerfile" -t "${BUNDLE_IMAGE}" "${REPO_ROOT}"

while IFS= read -r service; do
  [[ -n "${service}" ]] || continue

  if ! is_selected_service "${service}"; then
    continue
  fi

  repo="raksha-${service}"
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
    if aws ecr describe-images \
      --repository-name "${repo}" \
      --image-ids imageTag=latest \
      --region "${AWS_REGION}" >/dev/null 2>&1; then
      log "skipping ${latest_image}: immutable tag 'latest' already exists"
    else
      log "pushing ${latest_image}"
      docker tag "${BUNDLE_IMAGE}" "${latest_image}"
      # Push latest but treat immutable-tag errors as soft failures.
      # Two concurrent master runs can both pass the describe-images check above
      # (race window), then the second push fails with "tag invalid / immutable".
      # The sha-tagged image is already pushed successfully, so this is safe to skip.
      _push_log=$(mktemp)
      if ! docker push "${latest_image}" 2>&1 | tee "${_push_log}"; then
        if grep -qi "immutable\|tag invalid" "${_push_log}"; then
          log "::warning::skipping ${latest_image}: immutable tag conflict (concurrent push race)"
        else
          rm -f "${_push_log}"
          exit 1
        fi
      fi
      rm -f "${_push_log}"
    fi
  fi
done < <(catalog_services)
