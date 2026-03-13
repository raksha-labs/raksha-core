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

run_sql_file() {
  database_url="$1"
  sql_file="$2"

  [ -f "$sql_file" ] || fail "sql file not found: ${sql_file}"
  log "applying $(basename "$sql_file")"
  psql "$database_url" -v ON_ERROR_STOP=1 -f "$sql_file"
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
