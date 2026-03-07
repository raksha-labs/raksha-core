#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd terraform
require_cmd aws
require_cmd python3
install_cancel_trap

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
IMAGE_TAG_INPUT="${2:-${IMAGE_TAG:-latest}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment> [image_tag]"

"${SCRIPT_DIR}/terraform_init.sh" "${ENVIRONMENT}"
log "terraform phase: init complete; starting destroy (${ENVIRONMENT})"

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
  log "state lock guard: lock detected id=${lock_id} who=${lock_who} op=${lock_operation}"
  log "state lock guard: lock path=${lock_path}"
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

force_unlock_stale_if_needed() {
  local stale_threshold_minutes="${TF_LOCK_STALE_MINUTES:-30}"
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

run_tf_with_lock_retry() {
  local max_attempts="${TF_LOCK_RETRY_MAX_ATTEMPTS:-6}"
  local sleep_seconds="${TF_LOCK_RETRY_SLEEP_SECONDS:-20}"
  local attempt=1
  local output rc output_file

  while (( attempt <= max_attempts )); do
    output_file=$(mktemp)
    set +e
    "$@" 2>&1 | tee "${output_file}"
    rc=${PIPESTATUS[0]}
    set -e
    output=$(<"${output_file}")
    rm -f "${output_file}"

    if [[ "${rc}" -eq 0 ]]; then
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

log "terraform destroy (${ENVIRONMENT}) image_tag=${IMAGE_TAG_INPUT}"
print_state_lock_guard
force_unlock_stale_if_needed
run_tf_with_lock_retry terraform -chdir="${TF_DIR}" destroy \
  -input=false \
  -auto-approve \
  -lock=true \
  -lock-timeout="${TF_LOCK_TIMEOUT}" \
  -var="image_tag=${IMAGE_TAG_INPUT}"
log "terraform phase: destroy complete (${ENVIRONMENT})"
