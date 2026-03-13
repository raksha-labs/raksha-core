#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/ci/common.sh
source "${SCRIPT_DIR}/common.sh"

require_cmd aws
require_cmd terraform
require_cmd python3

ENVIRONMENT="${1:-${ENVIRONMENT:-}}"
[[ -n "${ENVIRONMENT}" ]] || fail "usage: $0 <environment>"

AWS_REGION="${AWS_REGION:-eu-west-1}"
TF_DIR=$(terraform_dir_for_env "${ENVIRONMENT}")
CLUSTER_NAME="${CLUSTER_NAME:-}"
BOOTSTRAP_SERVICE_NAME="${BOOTSTRAP_SERVICE_NAME:-history-worker}"

if [[ -z "${CLUSTER_NAME}" ]]; then
  log "resolving ECS cluster name from Terraform outputs (${ENVIRONMENT})"
  "${SCRIPT_DIR}/terraform_init.sh" "${ENVIRONMENT}"
  CLUSTER_NAME=$(terraform -chdir="${TF_DIR}" output -raw cluster_name)
fi

[[ -n "${CLUSTER_NAME}" ]] || fail "unable to resolve ECS cluster name"

SERVICE_NAME="raksha-${ENVIRONMENT}-${BOOTSTRAP_SERVICE_NAME}"
TASK_COMMAND="${BOOTSTRAP_TASK_COMMAND:-/bin/sh /app/scripts/bootstrap_databases.sh}"

log "preparing database bootstrap task from ECS service ${SERVICE_NAME}"

service_json=$(aws ecs describe-services \
  --cluster "${CLUSTER_NAME}" \
  --services "${SERVICE_NAME}" \
  --region "${AWS_REGION}" \
  --query 'services[0]' \
  --output json)

if [[ -z "${service_json}" || "${service_json}" == "null" ]]; then
  fail "unable to describe ECS service ${SERVICE_NAME}"
fi

tmp_input=$(mktemp)
trap 'rm -f "${tmp_input}"' EXIT

python3 - "${service_json}" "${CLUSTER_NAME}" "${TASK_COMMAND}" "${BOOTSTRAP_SERVICE_NAME}" > "${tmp_input}" <<'PY'
import json
import shlex
import sys

service = json.loads(sys.argv[1])
cluster = sys.argv[2]
command = sys.argv[3]
container_name = sys.argv[4]

task_definition = service.get("taskDefinition")
if not task_definition:
    raise SystemExit("service task definition is missing")

network = service.get("networkConfiguration", {}).get("awsvpcConfiguration")
if not network:
    raise SystemExit("service awsvpc network configuration is missing")

payload = {
    "cluster": cluster,
    "taskDefinition": task_definition,
    "count": 1,
    "startedBy": "codex-db-bootstrap",
    "networkConfiguration": {
        "awsvpcConfiguration": {
            "subnets": network.get("subnets", []),
            "securityGroups": network.get("securityGroups", []),
            "assignPublicIp": network.get("assignPublicIp", "DISABLED"),
        }
    },
    "overrides": {
        "containerOverrides": [
            {
                "name": container_name,
                "command": ["/bin/sh", "-lc", command],
            }
        ]
    },
}

launch_type = service.get("launchType")
capacity_provider_strategy = service.get("capacityProviderStrategy") or []

if capacity_provider_strategy:
    payload["capacityProviderStrategy"] = [
        {
            "capacityProvider": entry["capacityProvider"],
            "weight": entry.get("weight", 0),
            "base": entry.get("base", 0),
        }
        for entry in capacity_provider_strategy
    ]
elif launch_type and launch_type != "None":
    payload["launchType"] = launch_type

print(json.dumps(payload))
PY

run_task_json=$(aws ecs run-task \
  --cli-input-json "file://${tmp_input}" \
  --region "${AWS_REGION}" \
  --output json)

task_arn=$(python3 - "${run_task_json}" <<'PY'
import json
import sys

doc = json.loads(sys.argv[1])
tasks = doc.get("tasks", [])
if not tasks:
    failures = doc.get("failures", [])
    raise SystemExit(json.dumps(failures))
print(tasks[0]["taskArn"])
PY
)

[[ -n "${task_arn}" ]] || fail "failed to start bootstrap ECS task"

log "started database bootstrap task ${task_arn}"
aws ecs wait tasks-stopped \
  --cluster "${CLUSTER_NAME}" \
  --tasks "${task_arn}" \
  --region "${AWS_REGION}"

task_json=$(aws ecs describe-tasks \
  --cluster "${CLUSTER_NAME}" \
  --tasks "${task_arn}" \
  --region "${AWS_REGION}" \
  --query 'tasks[0]' \
  --output json)

python3 - "${task_json}" <<'PY'
import json
import sys

task = json.loads(sys.argv[1])
containers = task.get("containers") or []
container = containers[0] if containers else {}
exit_code = container.get("exitCode")
reason = container.get("reason") or task.get("stoppedReason") or "unknown"
if exit_code not in (0, None):
    print(f"[ci] ERROR: database bootstrap task failed exit_code={exit_code} reason={reason}", file=sys.stderr)
    raise SystemExit(1)
if exit_code is None:
    print(f"[ci] ERROR: database bootstrap task stopped without exit code reason={reason}", file=sys.stderr)
    raise SystemExit(1)
print(f"[ci] database bootstrap task completed exit_code={exit_code} reason={reason}")
PY
