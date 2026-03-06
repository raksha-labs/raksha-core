#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd terraform
require_cmd aws
require_cmd python3

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
IMAGE_TAG_INPUT="${2:-${IMAGE_TAG:-latest}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment> [image_tag]"

"${SCRIPT_DIR}/terraform_init.sh" "${ENVIRONMENT}"
log "terraform phase: init complete; starting apply preparation (${ENVIRONMENT})"

TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")
AWS_REGION_EFFECTIVE="${AWS_REGION:-eu-west-1}"
TF_LOCK_TIMEOUT="${TF_LOCK_TIMEOUT:-10m}"

state_key_for_env() {
  local repo_name
  repo_name=$(basename "${REPO_ROOT}")
  echo "${TF_STATE_KEY:-${TF_BACKEND_KEY_PREFIX:-raksha}/${repo_name}/${ENVIRONMENT}/terraform.tfstate}"
}

state_lock_details_for_env() {
  local dynamo_table bucket
  dynamo_table=$(trim_whitespace "${TF_BACKEND_DYNAMODB_TABLE:-}")
  bucket=$(trim_whitespace "${TF_BACKEND_BUCKET:-}")
  [[ -n "${dynamo_table}" && -n "${bucket}" ]] || return 0

  local expected_state_key
  expected_state_key=$(state_key_for_env)

  local lock_info
  lock_info=$(aws dynamodb scan \
    --table-name "${dynamo_table}" \
    --region "${AWS_REGION_EFFECTIVE}" \
    --output json 2>/dev/null || true)
  [[ -n "${lock_info}" ]] || return 0

  echo "${lock_info}" | python3 -c "
import json, sys
doc=json.load(sys.stdin)
expected='${expected_state_key}'
bucket='${bucket}'

def matches(path: str) -> bool:
    if not path:
        return False
    if path == expected:
        return True
    if path == f'{bucket}/{expected}':
        return True
    if path == f's3://{bucket}/{expected}':
        return True
    return path.endswith('/' + expected) or expected in path

for item in doc.get('Items', []):
    info_raw=item.get('Info',{}).get('S')
    if not info_raw:
        continue
    try:
        info=json.loads(info_raw)
    except Exception:
        continue
    path=str(info.get('Path',''))
    if matches(path):
        print('\\t'.join([
            str(info.get('ID','')),
            str(info.get('Created','')),
            str(info.get('Who','')),
            str(info.get('Operation','')),
            path
        ]))
        break
" 2>/dev/null || true
}

print_state_lock_guard() {
  local expected_state_key lock_details
  expected_state_key=$(state_key_for_env)
  log "state lock guard: backend key=${expected_state_key}"

  lock_details=$(state_lock_details_for_env || true)
  if [[ -z "${lock_details}" ]]; then
    log "state lock guard: no existing lock row found"
    return 0
  fi

  local lock_id lock_created_ts lock_who lock_operation lock_path
  IFS=$'\t' read -r lock_id lock_created_ts lock_who lock_operation lock_path <<< "${lock_details}"

  local lock_created_epoch now_epoch age_minutes
  lock_created_epoch=$(python3 -c "
from datetime import datetime, timezone
s='${lock_created_ts}'.split('.')[0].strip()
if not s:
  print(0)
  raise SystemExit(0)
dt=datetime.strptime(s,'%Y-%m-%d %H:%M:%S').replace(tzinfo=timezone.utc)
print(int(dt.timestamp()))
" 2>/dev/null || echo "0")
  now_epoch=$(date +%s)
  age_minutes=0
  if [[ "${lock_created_epoch}" -gt 0 ]]; then
    age_minutes=$(( (now_epoch - lock_created_epoch) / 60 ))
  fi

  log "state lock guard: lock detected id=${lock_id} who=${lock_who} op=${lock_operation} age=${age_minutes}m"
  log "state lock guard: lock path=${lock_path}"
}

run_tf_with_lock_retry() {
  local max_attempts="${TF_LOCK_RETRY_MAX_ATTEMPTS:-6}"
  local sleep_seconds="${TF_LOCK_RETRY_SLEEP_SECONDS:-20}"
  local attempt=1
  local output rc

  while (( attempt <= max_attempts )); do
    set +e
    output=$("$@" 2>&1)
    rc=$?
    set -e

    if [[ "${rc}" -eq 0 ]]; then
      [[ -n "${output}" ]] && printf '%s\n' "${output}"
      return 0
    fi

    if grep -q "Error acquiring the state lock" <<<"${output}" \
      || grep -q "ConditionalCheckFailedException" <<<"${output}"; then
      printf '%s\n' "${output}" >&2
      if ! force_unlock_from_output_if_needed "${output}"; then
        force_unlock_stale_if_needed || true
      fi

      if (( attempt < max_attempts )); then
        log "terraform state lock busy (attempt ${attempt}/${max_attempts}) — retrying in ${sleep_seconds}s"
        sleep "${sleep_seconds}"
        ((attempt++))
        continue
      fi
    fi

    printf '%s\n' "${output}" >&2
    return "${rc}"
  done

  return 1
}

force_unlock_from_output_if_needed() {
  local output="$1"
  local lock_id

  lock_id=$(printf '%s\n' "${output}" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -n 1 || true)

  if grep -q "OperationTypeInvalid" <<<"${output}" && [[ -n "${lock_id}" ]]; then
    log "WARNING: invalid Terraform state lock detected from command output — force-unlocking immediately: ${lock_id}"
    terraform -chdir="${TF_DIR}" force-unlock -force "${lock_id}" || true
    return 0
  fi

  return 1
}

tf_import() {
  local address="$1"
  local id="$2"
  run_tf_with_lock_retry \
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

import_compute_iam_if_needed() {
  local task_exec_role="raksha-${ENVIRONMENT}-ecs-task-execution-role"
  local task_role="raksha-${ENVIRONMENT}-ecs-task-role"
  local instance_role="raksha-${ENVIRONMENT}-ecs-instance-role"
  local instance_profile="raksha-${ENVIRONMENT}-ecs-instance-profile"

  local role_names=("${task_exec_role}" "${task_role}" "${instance_role}")
  local role_addresses=(
    "module.compute.aws_iam_role.ecs_task_execution"
    "module.compute.aws_iam_role.ecs_task"
    "module.compute.aws_iam_role.ecs_instance[0]"
  )

  local i
  for i in "${!role_names[@]}"; do
    if resource_in_state "${role_addresses[$i]}"; then
      continue
    fi
    if aws iam get-role --role-name "${role_names[$i]}" >/dev/null 2>&1; then
      log "import existing ECS IAM role into state: ${role_names[$i]}"
      tf_import "${role_addresses[$i]}" "${role_names[$i]}"
    fi
  done

  if ! resource_in_state "module.compute.aws_iam_instance_profile.ecs_instance[0]"; then
    if aws iam get-instance-profile --instance-profile-name "${instance_profile}" >/dev/null 2>&1; then
      log "import existing ECS instance profile into state: ${instance_profile}"
      tf_import "module.compute.aws_iam_instance_profile.ecs_instance[0]" "${instance_profile}"
    fi
  fi
}

import_compute_ec2_resources_if_needed() {
  local asg_name="raksha-${ENVIRONMENT}-ecs-asg"
  local cp_name="raksha-${ENVIRONMENT}-ec2-cp"

  if ! resource_in_state "module.compute.aws_autoscaling_group.ecs[0]"; then
    if aws autoscaling describe-auto-scaling-groups \
      --auto-scaling-group-names "${asg_name}" \
      --region "${AWS_REGION_EFFECTIVE}" \
      --query "length(AutoScalingGroups)" \
      --output text 2>/dev/null | grep -q "^1$"; then
      log "import existing ECS Auto Scaling Group into state: ${asg_name}"
      tf_import "module.compute.aws_autoscaling_group.ecs[0]" "${asg_name}"
    fi
  fi

  if ! resource_in_state "module.compute.aws_ecs_capacity_provider.ec2[0]"; then
    if aws ecs describe-capacity-providers \
      --capacity-providers "${cp_name}" \
      --region "${AWS_REGION_EFFECTIVE}" \
      --query "length(capacityProviders)" \
      --output text 2>/dev/null | grep -q "^1$"; then
      log "import existing ECS capacity provider into state: ${cp_name}"
      tf_import "module.compute.aws_ecs_capacity_provider.ec2[0]" "${cp_name}"
    fi
  fi

  if ! resource_in_state "module.compute.aws_launch_template.ecs[0]"; then
    local lt_id
    lt_id=$(aws ec2 describe-launch-templates \
      --region "${AWS_REGION_EFFECTIVE}" \
      --filters "Name=launch-template-name,Values=raksha-${ENVIRONMENT}-ecs-*" \
      --query "LaunchTemplates[0].LaunchTemplateId" \
      --output text 2>/dev/null || true)
    if [[ -n "${lt_id}" && "${lt_id}" != "None" ]]; then
      log "import existing ECS launch template into state: ${lt_id}"
      tf_import "module.compute.aws_launch_template.ecs[0]" "${lt_id}"
    fi
  fi
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


import_ecs_services_if_needed() {
  local cluster_name="raksha-${ENVIRONMENT}"
  local services=()
  local svc
  while IFS= read -r svc; do
    [[ -n "${svc}" ]] || continue
    services+=("${svc}")
  done < <(catalog_services)

  if [[ "${ENVIRONMENT}" == "test" ]]; then
    services+=("postgres" "redis")
  fi

  local service_name
  local address
  local existing_count
  for svc in "${services[@]}"; do
    if [[ "${svc}" == "postgres" || "${svc}" == "redis" ]]; then
      address="module.compute.aws_ecs_service.test_data[\"${svc}\"]"
    else
      address="module.compute.aws_ecs_service.service[\"${svc}\"]"
    fi

    if resource_in_state "${address}"; then
      continue
    fi

    service_name="raksha-${ENVIRONMENT}-${svc}"
    existing_count=$(aws ecs describe-services \
      --cluster "${cluster_name}" \
      --services "${service_name}" \
      --region "${AWS_REGION_EFFECTIVE}" \
      --query "length(services[?status!='INACTIVE'])" \
      --output text 2>/dev/null || true)

    if [[ "${existing_count}" == "1" ]]; then
      log "import existing ECS service into state: ${service_name}"
      tf_import "${address}" "${cluster_name}/${service_name}"
    fi
  done
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
  local dynamo_table bucket
  dynamo_table=$(trim_whitespace "${TF_BACKEND_DYNAMODB_TABLE:-}")
  bucket=$(trim_whitespace "${TF_BACKEND_BUCKET:-}")
  [[ -n "${dynamo_table}" && -n "${bucket}" ]] || return 0

  local stale_threshold_minutes="${TF_STALE_LOCK_MINUTES:-15}"
  local lock_details lock_id lock_created_ts lock_who lock_operation lock_path
  lock_details=$(state_lock_details_for_env || true)
  IFS=$'\t' read -r lock_id lock_created_ts lock_who lock_operation lock_path <<< "${lock_details}"

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

  if [[ "${lock_operation}" == "OperationTypeInvalid" ]]; then
    log "WARNING: invalid Terraform state lock detected — force-unlocking immediately: ${lock_id}"
    terraform -chdir="${TF_DIR}" force-unlock -force "${lock_id}" || true
  elif [[ "${lock_created_epoch}" -gt 0 && "${age_minutes}" -ge "${stale_threshold_minutes}" ]]; then
    log "WARNING: stale state lock detected (${age_minutes}m old, threshold ${stale_threshold_minutes}m) — force-unlocking: ${lock_id}"
    terraform -chdir="${TF_DIR}" force-unlock -force "${lock_id}" || true
  else
    log "Active state lock found (${age_minutes}m old) — waiting up to ${TF_LOCK_TIMEOUT}"
  fi
}

log "terraform apply (${ENVIRONMENT}) image_tag=${IMAGE_TAG_INPUT}"
print_state_lock_guard
force_unlock_stale_if_needed
import_cicd_roles_if_needed
import_compute_iam_if_needed
import_compute_ec2_resources_if_needed
import_ecr_repositories_if_needed
import_shared_secrets_if_needed
import_log_groups_if_needed
import_ecs_services_if_needed
log "terraform phase: apply preparation complete; starting terraform apply (${ENVIRONMENT})"
run_tf_with_lock_retry terraform -chdir="${TF_DIR}" apply \
  -input=false \
  -auto-approve \
  -lock=true \
  -lock-timeout="${TF_LOCK_TIMEOUT}" \
  -var="image_tag=${IMAGE_TAG_INPUT}"
log "terraform phase: apply complete (${ENVIRONMENT})"
