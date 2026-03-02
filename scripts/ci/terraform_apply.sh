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
TF_LOCK_TIMEOUT="${TF_LOCK_TIMEOUT:-10m}"

tf_import() {
  local address="$1"
  local id="$2"
  terraform -chdir="${TF_DIR}" import -input=false -lock-timeout="${TF_LOCK_TIMEOUT}" "${address}" "${id}"
}

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
      tf_import "${addresses[$i]}" "${names[$i]}"
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
      tf_import "${address}" "${repo}"
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
      tf_import "module.data_test[0].aws_secretsmanager_secret.database" "${db_secret}"
    fi
  fi

  if ! resource_in_state "module.data_test[0].aws_secretsmanager_secret.redis"; then
    if aws secretsmanager describe-secret --secret-id "${redis_secret}" --region "${AWS_REGION_EFFECTIVE}" >/dev/null 2>&1; then
      log "import existing Secrets Manager secret into state: ${redis_secret}"
      tf_import "module.data_test[0].aws_secretsmanager_secret.redis" "${redis_secret}"
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
      tf_import "${address}" "${log_group}"
    fi
  done
}

force_unlock_stale_if_needed() {
  local dynamo_table="${TF_BACKEND_DYNAMODB_TABLE:-}"
  local bucket="${TF_BACKEND_BUCKET:-}"
  [[ -n "${dynamo_table}" && -n "${bucket}" ]] || return 0

  local stale_threshold_minutes="${TF_STALE_LOCK_MINUTES:-15}"
  local state_path_suffix="/${ENVIRONMENT}/terraform.tfstate"

  # Query DynamoDB for a lock on this environment's state file
  local lock_info
  lock_info=$(aws dynamodb scan \
    --table-name "${dynamo_table}" \
    --filter-expression "contains(LockID, :suffix)" \
    --expression-attribute-values "{\":suffix\":{\"S\":\"${state_path_suffix}\"}}" \
    --region "${AWS_REGION_EFFECTIVE}" \
    --output json 2>/dev/null || true)

  [[ -n "${lock_info}" ]] || return 0

  # Extract Created timestamp and lock ID from the Info JSON blob
  local lock_created_ts lock_id
  lock_created_ts=$(echo "${lock_info}" | python3 -c "
import json,sys
d=json.load(sys.stdin)
items=d.get('Items',[])
if not items: sys.exit(0)
info=json.loads(items[0].get('Info',{}).get('S','{}'))
print(info.get('Created',''))
" 2>/dev/null || true)

  lock_id=$(echo "${lock_info}" | python3 -c "
import json,sys
d=json.load(sys.stdin)
items=d.get('Items',[])
if not items: sys.exit(0)
info=json.loads(items[0].get('Info',{}).get('S','{}'))
print(info.get('ID',''))
" 2>/dev/null || true)

  [[ -n "${lock_created_ts}" && -n "${lock_id}" ]] || return 0

  local lock_created_epoch now_epoch age_minutes
  lock_created_epoch=$(python3 -c "
from datetime import datetime, timezone
s='${lock_created_ts}'.split('.')[0].strip()
dt=datetime.strptime(s,'%Y-%m-%d %H:%M:%S').replace(tzinfo=timezone.utc)
print(int(dt.timestamp()))
" 2>/dev/null || echo "0")

  now_epoch=$(date +%s)
  age_minutes=$(( (now_epoch - lock_created_epoch) / 60 ))

  if [[ "${lock_created_epoch}" -gt 0 && "${age_minutes}" -ge "${stale_threshold_minutes}" ]]; then
    log "WARNING: stale state lock detected (${age_minutes}m old, threshold ${stale_threshold_minutes}m) — force-unlocking: ${lock_id}"
    terraform -chdir="${TF_DIR}" force-unlock -force "${lock_id}" || true
  else
    log "Active state lock found (${age_minutes}m old) — waiting up to ${TF_LOCK_TIMEOUT}"
  fi
}

log "terraform apply (${ENVIRONMENT}) image_tag=${IMAGE_TAG_INPUT}"
force_unlock_stale_if_needed
import_cicd_roles_if_needed
import_ecr_repositories_if_needed
import_shared_secrets_if_needed
import_log_groups_if_needed
terraform -chdir="${TF_DIR}" apply \
  -input=false \
  -auto-approve \
  -lock=true \
  -lock-timeout="${TF_LOCK_TIMEOUT}" \
  -var="image_tag=${IMAGE_TAG_INPUT}"
