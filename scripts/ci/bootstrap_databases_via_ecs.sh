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

config_string_var() {
  local var_name="$1"
  local tfvars_path="${TF_DIR}/terraform.tfvars"
  local variables_path="${TF_DIR}/variables.tf"
  local value=""

  if [[ -f "${tfvars_path}" ]]; then
    value=$(awk -v var_name="${var_name}" '
      $0 ~ "^[[:space:]]*" var_name "[[:space:]]*=" {
        line=$0
        sub(/^[^=]*=[[:space:]]*/, "", line)
        sub(/[[:space:]]*(#.*)?$/, "", line)
        gsub(/^"/, "", line)
        gsub(/"$/, "", line)
        print line
        exit
      }
    ' "${tfvars_path}")
  fi

  if [[ -n "${value}" ]]; then
    printf '%s\n' "${value}"
    return 0
  fi

  if [[ -f "${variables_path}" ]]; then
    value=$(awk -v var_name="${var_name}" '
      $0 ~ "^[[:space:]]*variable[[:space:]]+\"" var_name "\"" {
        in_var=1
        next
      }
      in_var && $0 ~ /^[[:space:]]*default[[:space:]]*=/ {
        line=$0
        sub(/^[^=]*=[[:space:]]*/, "", line)
        sub(/[[:space:]]*(#.*)?$/, "", line)
        gsub(/^"/, "", line)
        gsub(/"$/, "", line)
        print line
        exit
      }
      in_var && $0 ~ /^[[:space:]]*}/ {
        exit
      }
    ' "${variables_path}")
  fi

  printf '%s\n' "${value}"
}

desired_ecs_launch_mode() {
  local compute_mode
  compute_mode=$(trim_whitespace "$(config_string_var compute_mode)")
  if [[ "${compute_mode}" == "ec2" ]]; then
    printf 'EC2\n'
  else
    printf 'FARGATE\n'
  fi
}

SERVICE_NAME="raksha-${ENVIRONMENT}-${BOOTSTRAP_SERVICE_NAME}"
TASK_COMMAND="${BOOTSTRAP_TASK_COMMAND:-/bin/sh /app/scripts/bootstrap_databases.sh}"
DESIRED_LAUNCH_MODE="${DESIRED_LAUNCH_MODE:-$(desired_ecs_launch_mode)}"

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

task_definition_arn=$(python3 - "${service_json}" <<'PY'
import json
import sys

service = json.loads(sys.argv[1])
print(service.get("taskDefinition", ""))
PY
)

[[ -n "${task_definition_arn}" ]] || fail "unable to resolve task definition for ECS service ${SERVICE_NAME}"

task_definition_json=$(aws ecs describe-task-definition \
  --task-definition "${task_definition_arn}" \
  --region "${AWS_REGION}" \
  --query 'taskDefinition' \
  --output json)

cluster_capacity_providers=$(aws ecs describe-clusters \
  --clusters "${CLUSTER_NAME}" \
  --region "${AWS_REGION}" \
  --query 'clusters[0].capacityProviders' \
  --output json)

tmp_input=$(mktemp)
trap 'rm -f "${tmp_input}"' EXIT

python3 - "${service_json}" "${cluster_capacity_providers}" "${CLUSTER_NAME}" "${TASK_COMMAND}" "${BOOTSTRAP_SERVICE_NAME}" "${DESIRED_LAUNCH_MODE}" > "${tmp_input}" <<'PY'
import json
import sys

service = json.loads(sys.argv[1])
cluster_capacity_providers = set(json.loads(sys.argv[2]) or [])
cluster = sys.argv[3]
command = sys.argv[4]
container_name = sys.argv[5]
desired_launch_mode = sys.argv[6]

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
    valid_capacity_provider_strategy = [
        {
            "capacityProvider": entry["capacityProvider"],
            "weight": entry.get("weight", 0),
            "base": entry.get("base", 0),
        }
        for entry in capacity_provider_strategy
        if entry.get("capacityProvider") in cluster_capacity_providers
    ]
    if valid_capacity_provider_strategy:
        payload["capacityProviderStrategy"] = valid_capacity_provider_strategy
    elif desired_launch_mode in {"FARGATE", "EC2"}:
        payload["launchType"] = desired_launch_mode
    elif launch_type and launch_type != "None":
        payload["launchType"] = launch_type
    else:
        raise SystemExit(
            "service capacity provider strategy is not valid for the cluster and no launch type fallback is available"
        )
elif launch_type and launch_type != "None":
    payload["launchType"] = launch_type
elif desired_launch_mode in {"FARGATE", "EC2"}:
    payload["launchType"] = desired_launch_mode
else:
    raise SystemExit("unable to determine a valid ECS launch mode for the bootstrap task")

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

task_status=$(python3 - "${task_json}" "${task_definition_json}" "${BOOTSTRAP_SERVICE_NAME}" "${task_arn}" <<'PY'
import json
import sys

task = json.loads(sys.argv[1])
task_definition = json.loads(sys.argv[2])
container_name = sys.argv[3]
task_arn = sys.argv[4]
containers = task.get("containers") or []
container = next((entry for entry in containers if entry.get("name") == container_name), None)
if container is None:
    container = containers[0] if containers else {}

log_group_name = ""
log_stream_name = container.get("logStreamName") or ""

container_definitions = task_definition.get("containerDefinitions") or []
container_definition = next(
    (entry for entry in container_definitions if entry.get("name") == container_name),
    None,
)
if container_definition is None:
    container_definition = container_definitions[0] if container_definitions else {}

log_options = (container_definition.get("logConfiguration") or {}).get("options") or {}
log_group_name = log_options.get("awslogs-group", "")
if not log_stream_name:
    stream_prefix = log_options.get("awslogs-stream-prefix", "")
    task_id = task_arn.rsplit("/", 1)[-1] if "/" in task_arn else task_arn
    effective_container_name = container.get("name") or container_name
    if stream_prefix and effective_container_name and task_id:
        log_stream_name = f"{stream_prefix}/{effective_container_name}/{task_id}"

print(json.dumps({
    "exit_code": container.get("exitCode"),
    "reason": container.get("reason") or task.get("stoppedReason") or "unknown",
    "log_group_name": log_group_name,
    "log_stream_name": log_stream_name,
}))
PY
)

exit_code=$(python3 - "${task_status}" <<'PY'
import json
import sys
status = json.loads(sys.argv[1])
print("" if status.get("exit_code") is None else status["exit_code"])
PY
)

reason=$(python3 - "${task_status}" <<'PY'
import json
import sys
status = json.loads(sys.argv[1])
print(status.get("reason", "unknown"))
PY
)

log_stream_name=$(python3 - "${task_status}" <<'PY'
import json
import sys
status = json.loads(sys.argv[1])
print(status.get("log_stream_name", ""))
PY
)

log_group_name=$(python3 - "${task_status}" <<'PY'
import json
import sys
status = json.loads(sys.argv[1])
print(status.get("log_group_name", ""))
PY
)

if [[ -z "${exit_code}" ]]; then
  printf '[ci] ERROR: database bootstrap task stopped without exit code reason=%s\n' "${reason}" >&2
  if [[ -n "${log_group_name}" ]]; then
    printf '[ci] bootstrap task log group: %s\n' "${log_group_name}" >&2
  fi
  if [[ -n "${log_stream_name}" ]]; then
    printf '[ci] bootstrap task log stream: %s\n' "${log_stream_name}" >&2
  fi
  exit 1
fi

if [[ "${exit_code}" != "0" ]]; then
  printf '[ci] ERROR: database bootstrap task failed exit_code=%s reason=%s\n' "${exit_code}" "${reason}" >&2
  if [[ -n "${log_group_name}" ]]; then
    printf '[ci] bootstrap task log group: %s\n' "${log_group_name}" >&2
  fi
  if [[ -n "${log_group_name}" && -n "${log_stream_name}" ]]; then
    printf '[ci] last bootstrap task logs from %s / %s\n' "${log_group_name}" "${log_stream_name}" >&2
    aws logs get-log-events \
      --log-group-name "${log_group_name}" \
      --log-stream-name "${log_stream_name}" \
      --region "${AWS_REGION}" \
      --limit 200 \
      --query 'events[].message' \
      --output text >&2 || true
  fi
  exit 1
fi

printf '[ci] database bootstrap task completed exit_code=%s reason=%s\n' "${exit_code}" "${reason}"
