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

config_bool_var() {
  local var_name="$1"
  local tfvars_path="${TF_DIR}/terraform.tfvars"
  local variables_path="${TF_DIR}/variables.tf"
  local value=""

  if [[ -f "${tfvars_path}" ]]; then
    value=$(awk -v var_name="${var_name}" '
      $0 ~ "^[[:space:]]*" var_name "[[:space:]]*=" {
        line=$0
        sub(/^[^=]*=[[:space:]]*/, "", line)
        sub(/[[:space:]]*(#.*)?$/, "", line)
        gsub(/"/, "", line)
        print tolower(line)
        exit
      }
    ' "${tfvars_path}")
  fi

  if [[ -n "${value}" ]]; then
    printf '%s\n' "${value}"
    return 0
  fi

  if [[ -f "${variables_path}" ]]; then
    value=$(awk -v var_name="${var_name}" '
      $0 ~ "^[[:space:]]*variable[[:space:]]+\"" var_name "\"" {
        in_var=1
        next
      }
      in_var && $0 ~ /^[[:space:]]*default[[:space:]]*=/ {
        line=$0
        sub(/^[^=]*=[[:space:]]*/, "", line)
        sub(/[[:space:]]*(#.*)?$/, "", line)
        gsub(/"/, "", line)
        print tolower(line)
        exit
      }
      in_var && $0 ~ /^[[:space:]]*}/ {
        exit
      }
    ' "${variables_path}")
  fi

  printf '%s\n' "${value:-false}"
}

test_data_services_enabled() {
  [[ "${ENVIRONMENT}" == "test" ]] || return 1
  [[ "$(config_bool_var enable_managed_data)" == "false" ]]
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
