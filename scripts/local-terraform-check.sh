#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if command -v terraform >/dev/null 2>&1; then
  TF=(terraform)
else
  TF=(docker run --rm -v "$ROOT_DIR":/work -w /work hashicorp/terraform:1.6.6)
fi

"${TF[@]}" fmt -recursive infra/terraform

for env in test stage prod; do
  echo "Validating $env"
  (
    cd "infra/terraform/environments/$env"
    "${TF[@]}" init -backend=false >/dev/null
    "${TF[@]}" validate
    rm -rf .terraform
  )
done

echo "Terraform checks passed for all environments."
