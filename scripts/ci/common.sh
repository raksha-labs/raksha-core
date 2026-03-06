#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "${SCRIPT_DIR}/../.." && pwd)

log() {
  printf '[ci] %s\n' "$*"
}

fail() {
  printf '[ci] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

require_env() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    fail "required environment variable is not set: ${name}"
  fi
}

trim_whitespace() {
  printf '%s' "${1:-}" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//'
}

normalize_csv_filter() {
  echo "${1:-}" | tr -d '[:space:]'
}

is_selected_service() {
  local service="$1"
  local filter
  filter=$(normalize_csv_filter "${SERVICE_FILTER:-}")

  if [[ -z "${filter}" ]]; then
    return 0
  fi

  [[ ",${filter}," == *",${service},"* ]]
}

catalog_services() {
  awk -F': ' '/service_name:/{gsub(/"/,"",$2); print $2}' "${REPO_ROOT}/infra/service-catalog.yaml"
}

terraform_dir_for_env() {
  local environment="$1"
  echo "${REPO_ROOT}/infra/terraform/environments/${environment}"
}
