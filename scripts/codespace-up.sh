#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_DIR="${WORKSPACE_DIR:-$(cd "${REPO_DIR}/.." && pwd)}"
PLATFORM_DIR="${WORKSPACE_DIR}/raksha-platform"
SIMLAB_DIR="${WORKSPACE_DIR}/raksha-simlab"
NO_BUILD=false

if [[ "${1:-}" == "--no-build" ]]; then
  NO_BUILD=true
fi

bash "${SCRIPT_DIR}/codespace-bootstrap.sh"
source "${SCRIPT_DIR}/codespace-env.sh"

if [[ ! -f "${PLATFORM_DIR}/docker-compose.yml" || ! -f "${PLATFORM_DIR}/docker-compose.core.yml" ]]; then
  echo "[codespace-up] missing platform compose files under ${PLATFORM_DIR}" >&2
  exit 1
fi

if [[ ! -f "${SIMLAB_DIR}/web/docker-compose.yml" ]]; then
  echo "[codespace-up] missing simlab compose file under ${SIMLAB_DIR}/web" >&2
  exit 1
fi

platform_cmd=(docker compose -f "${PLATFORM_DIR}/docker-compose.yml" -f "${PLATFORM_DIR}/docker-compose.core.yml" up -d)
simlab_cmd=(docker compose -f "${SIMLAB_DIR}/web/docker-compose.yml" up -d)

if [[ "${NO_BUILD}" == "false" ]]; then
  platform_cmd+=(--build)
  simlab_cmd+=(--build)
fi

"${platform_cmd[@]}"
"${simlab_cmd[@]}"

echo
echo "Codespace sandbox is up:"
echo "  Landing   : ${NEXT_PUBLIC_LANDING_PAGE_URL:-http://localhost:3000}"
echo "  Web       : ${NEXT_PUBLIC_LANDING_PAGE_URL:-http://localhost:3000}"
echo "  Admin     : ${NEXT_PUBLIC_ADMIN_PORTAL_URL:-http://localhost:3003}"
echo "  Simlab    : ${NEXT_PUBLIC_SIMLAB_URL:-http://localhost:3010/simulation}"
echo "  Simlab API: ${SIMLAB_API_PUBLIC_URL:-http://localhost:8010}/docs"

