#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd terraform
require_cmd aws

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
IMAGE_TAG_INPUT="${2:-${IMAGE_TAG:-latest}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment> [image_tag]"

"${SCRIPT_DIR}/terraform_init.sh" "${ENVIRONMENT}"

TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")
AWS_REGION_EFFECTIVE="${AWS_REGION:-eu-west-1}"

resource_in_state() {
  local address="$1"
  terraform -chdir="${TF_DIR}" state show "${address}" >/dev/null 2>&1
}

import_cicd_roles_if_needed() {
  local images_role="raksha-${ENVIRONMENT}-github-images-role"
  local infra_role="raksha-${ENVIRONMENT}-github-infra-role"
  local deploy_role="raksha-${ENVIRONMENT}-github-deploy-role"

  local names=("${images_role}" "${infra_role}" "${deploy_role}")
  local addresses=(
    "module.cicd_iam.aws_iam_role.images"
    "module.cicd_iam.aws_iam_role.infra"
    "module.cicd_iam.aws_iam_role.deploy"
  )

  local i
  for i in "${!names[@]}"; do
    if resource_in_state "${addresses[$i]}"; then
      continue
    fi
    if aws iam get-role --role-name "${names[$i]}" >/dev/null 2>&1; then
      log "import existing IAM role into state: ${names[$i]}"
      terraform -chdir="${TF_DIR}" import -input=false "${addresses[$i]}" "${names[$i]}" || true
    fi
  done
}

import_ecr_repositories_if_needed() {
  local svc
  local repo
  while IFS=$'\t' read -r svc repo; do
    [[ -n "${svc}" && -n "${repo}" ]] || continue
    local address="module.compute.aws_ecr_repository.services[\"${svc}\"]"
    if resource_in_state "${address}"; then
      continue
    fi
    if aws ecr describe-repositories --repository-names "${repo}" --region "${AWS_REGION_EFFECTIVE}" >/dev/null 2>&1; then
      log "import existing ECR repository into state: ${repo}"
      terraform -chdir="${TF_DIR}" import -input=false "${address}" "${repo}" || true
    fi
  done < <(awk '
    /^[[:space:]]*-[[:space:]]*service_name:[[:space:]]*/ {
      svc=$0
      sub(/.*service_name:[[:space:]]*/, "", svc)
      gsub(/"/, "", svc)
      next
    }
    /^[[:space:]]*image_repo:[[:space:]]*/ {
      repo=$0
      sub(/.*image_repo:[[:space:]]*/, "", repo)
      gsub(/"/, "", repo)
      if (svc != "") {
        printf "%s\t%s\n", svc, repo
      }
    }
  ' "${REPO_ROOT}/infra/service-catalog.yaml")
}

import_shared_secrets_if_needed() {
  local db_secret="raksha/${ENVIRONMENT}/shared/DATABASE_URL"
  local redis_secret="raksha/${ENVIRONMENT}/shared/REDIS_URL"

  if ! resource_in_state "module.data_test[0].aws_secretsmanager_secret.database"; then
    if aws secretsmanager describe-secret --secret-id "${db_secret}" --region "${AWS_REGION_EFFECTIVE}" >/dev/null 2>&1; then
      log "import existing Secrets Manager secret into state: ${db_secret}"
      terraform -chdir="${TF_DIR}" import -input=false "module.data_test[0].aws_secretsmanager_secret.database" "${db_secret}" || true
    fi
  fi

  if ! resource_in_state "module.data_test[0].aws_secretsmanager_secret.redis"; then
    if aws secretsmanager describe-secret --secret-id "${redis_secret}" --region "${AWS_REGION_EFFECTIVE}" >/dev/null 2>&1; then
      log "import existing Secrets Manager secret into state: ${redis_secret}"
      terraform -chdir="${TF_DIR}" import -input=false "module.data_test[0].aws_secretsmanager_secret.redis" "${redis_secret}" || true
    fi
  fi
}

import_log_groups_if_needed() {
  local services=()
  local svc
  while IFS= read -r svc; do
    [[ -n "${svc}" ]] || continue
    services+=("${svc}")
  done < <(catalog_services)

  if [[ "${ENVIRONMENT}" == "test" ]]; then
    services+=("postgres" "redis")
  fi

  local log_group
  local address
  local existing
  for svc in "${services[@]}"; do
    log_group="/ecs/raksha/${ENVIRONMENT}/${svc}"
    address="module.observability.aws_cloudwatch_log_group.service[\"${svc}\"]"
    if resource_in_state "${address}"; then
      continue
    fi
    existing=$(aws logs describe-log-groups \
      --region "${AWS_REGION_EFFECTIVE}" \
      --log-group-name-prefix "${log_group}" \
      --query "logGroups[?logGroupName=='${log_group}'].logGroupName | [0]" \
      --output text 2>/dev/null || true)
    if [[ -n "${existing}" && "${existing}" != "None" ]]; then
      log "import existing log group into state: ${log_group}"
      terraform -chdir="${TF_DIR}" import -input=false "${address}" "${log_group}" || true
    fi
  done
}

log "terraform apply (${ENVIRONMENT}) image_tag=${IMAGE_TAG_INPUT}"
import_cicd_roles_if_needed
import_ecr_repositories_if_needed
import_shared_secrets_if_needed
import_log_groups_if_needed
terraform -chdir="${TF_DIR}" apply \
  -input=false \
  -auto-approve \
  -lock=true \
  -var="image_tag=${IMAGE_TAG_INPUT}"
