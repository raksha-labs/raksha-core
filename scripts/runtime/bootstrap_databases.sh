#!/bin/sh
set -eu

log() {
  printf '[bootstrap] %s\n' "$*"
}

fail() {
  printf '[bootstrap] ERROR: %s\n' "$*" >&2
  exit 1
}

require_env() {
  var_name="$1"
  eval "value=\${$var_name:-}"
  [ -n "$value" ] || fail "required environment variable is not set: ${var_name}"
}

is_retryable_psql_error() {
  message="$1"
  case "$message" in
    *"remaining connection slots are reserved"*|*"too many clients already"*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

run_psql_retry() {
  retry_attempts="${DB_PSQL_RETRY_ATTEMPTS:-12}"
  retry_delay_sec="${DB_PSQL_RETRY_DELAY_SEC:-5}"
  attempt=1

  while :; do
    output_file="$(mktemp)"
    if "$@" >"$output_file" 2>&1; then
      cat "$output_file"
      rm -f "$output_file"
      return 0
    fi

    status=$?
    output="$(cat "$output_file")"
    rm -f "$output_file"

    if is_retryable_psql_error "$output" && [ "$attempt" -lt "$retry_attempts" ]; then
      log "psql hit transient connection pressure (attempt ${attempt}/${retry_attempts}); retrying in ${retry_delay_sec}s"
      printf '%s\n' "$output" >&2
      attempt=$((attempt + 1))
      sleep "$retry_delay_sec"
      continue
    fi

    printf '%s\n' "$output" >&2
    return "$status"
  done
}

run_sql_file() {
  database_url="$1"
  sql_file="$2"

  [ -f "$sql_file" ] || fail "sql file not found: ${sql_file}"
  log "applying $(basename "$sql_file")"
  run_psql_retry psql "$database_url" -v ON_ERROR_STOP=1 -f "$sql_file"
}

require_env DATABASE_URL

BOOTSTRAP_ROOT="/app/sql/bootstrap"
CORE_DB_URL="${DATABASE_URL}"
RAW_DB_URL="${RAW_DATABASE_URL:-$DATABASE_URL}"

run_sql_file "$CORE_DB_URL" "${BOOTSTRAP_ROOT}/core_schema.sql"
run_sql_file "$CORE_DB_URL" "${BOOTSTRAP_ROOT}/history_schema.sql"
run_sql_file "$CORE_DB_URL" "${BOOTSTRAP_ROOT}/seed_sources.sql"
run_sql_file "$CORE_DB_URL" "${BOOTSTRAP_ROOT}/seed_patterns.sql"
run_sql_file "$CORE_DB_URL" "${BOOTSTRAP_ROOT}/seed_history_replay.sql"
run_sql_file "$RAW_DB_URL" "${BOOTSTRAP_ROOT}/raw_schema.sql"

log "database bootstrap complete"
