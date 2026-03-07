#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd aws
require_cmd terraform

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment>"
AWS_REGION="${AWS_REGION:-eu-west-1}"

TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")
CLUSTER_NAME="${CLUSTER_NAME:-}"

if [[ -z "${CLUSTER_NAME}" ]]; then
  log "resolving ECS cluster name from Terraform outputs (${ENVIRONMENT})"
  "${SCRIPT_DIR}/terraform_init.sh" "${ENVIRONMENT}"
  CLUSTER_NAME=$(terraform -chdir="${TF_DIR}" output -raw cluster_name)
fi

[[ -n "${CLUSTER_NAME}" ]] || fail "unable to resolve ECS cluster name"

terraform_bool_var() {
  local var_name="$1"
  printf 'try(var.%s, false)\n' "${var_name}" | terraform -chdir="${TF_DIR}" console -no-color 2>/dev/null | tr -d '"[:space:]'
}

test_data_services_enabled() {
  [[ "${ENVIRONMENT}" == "test" ]] || return 1
  [[ "$(terraform_bool_var enable_managed_data)" == "false" ]]
}

log "rolling ECS services in cluster ${CLUSTER_NAME} (region=${AWS_REGION}, service_filter=${SERVICE_FILTER:-<all>})"
services_to_roll() {
  catalog_services
  if test_data_services_enabled; then
    printf '%s\n' postgres redis
  fi
}

rolled_count=0
skipped_count=0

while IFS= read -r service; do
  [[ -n "${service}" ]] || continue

  if ! is_selected_service "${service}"; then
    log "skipping ECS service ${service}; not selected by service_filter"
    ((skipped_count+=1))
    continue
  fi

  ecs_service="raksha-${ENVIRONMENT}-${service}"
  log "forcing deployment ${ecs_service}"
  aws ecs update-service \
    --cluster "${CLUSTER_NAME}" \
    --service "${ecs_service}" \
    --force-new-deployment \
    --region "${AWS_REGION}" >/dev/null
  log "deployment request submitted for ${ecs_service}"
  ((rolled_count+=1))
done < <(services_to_roll)

log "ECS rollout requests complete (submitted=${rolled_count}, skipped=${skipped_count})"
