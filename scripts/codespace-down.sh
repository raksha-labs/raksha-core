#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_DIR="${WORKSPACE_DIR:-$(cd "${REPO_DIR}/.." && pwd)}"
PLATFORM_DIR="${WORKSPACE_DIR}/raksha-platform"
SIMLAB_DIR="${WORKSPACE_DIR}/raksha-simlab"
REMOVE_VOLUMES=false

if [[ "${1:-}" == "-v" || "${1:-}" == "--volumes" ]]; then
  REMOVE_VOLUMES=true
fi

platform_cmd=(docker compose -f "${PLATFORM_DIR}/docker-compose.yml" -f "${PLATFORM_DIR}/docker-compose.core.yml" down --remove-orphans)
simlab_cmd=(docker compose -f "${SIMLAB_DIR}/web/docker-compose.yml" down --remove-orphans)

if [[ "${REMOVE_VOLUMES}" == "true" ]]; then
  platform_cmd+=(-v)
  simlab_cmd+=(-v)
fi

if [[ -f "${SIMLAB_DIR}/web/docker-compose.yml" ]]; then
  "${simlab_cmd[@]}"
fi

if [[ -f "${PLATFORM_DIR}/docker-compose.yml" && -f "${PLATFORM_DIR}/docker-compose.core.yml" ]]; then
  "${platform_cmd[@]}"
fi

echo "Codespace sandbox stopped."

