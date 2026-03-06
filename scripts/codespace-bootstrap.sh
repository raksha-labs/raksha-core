#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_DIR="${WORKSPACE_DIR:-$(cd "${REPO_DIR}/.." && pwd)}"
REQUIRED_REPOS=(raksha-core raksha-platform raksha-simlab)

missing_repos=()
for repo in "${REQUIRED_REPOS[@]}"; do
  if [[ ! -d "${WORKSPACE_DIR}/${repo}" ]]; then
    missing_repos+=("${repo}")
  fi
done

if [[ "${#missing_repos[@]}" -eq 0 ]]; then
  echo "[codespace-bootstrap] sibling repos already present in ${WORKSPACE_DIR}"
  exit 0
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "[codespace-bootstrap] gh CLI is required to clone missing sibling repos." >&2
  echo "[codespace-bootstrap] missing: ${missing_repos[*]}" >&2
  exit 1
fi

for repo in "${missing_repos[@]}"; do
  echo "[codespace-bootstrap] cloning raksha-labs/${repo} into ${WORKSPACE_DIR}/${repo}"
  gh repo clone "raksha-labs/${repo}" "${WORKSPACE_DIR}/${repo}"
done

