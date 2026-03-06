# Manual Destroy Sequence

Use this runbook when you want a full manual teardown of a shared Raksha
environment.

## Shared Ownership Model

- `raksha-core` owns the shared cluster, shared data, shared ALBs, and shared EC2 capacity.
- `raksha-platform` and `raksha-simlab` attach to shared infra in shared mode.

Because of that, always destroy in this order:

1. `raksha-platform`
2. `raksha-simlab`
3. `raksha-core`

Destroying `raksha-core` first can strand dependent services or leave the other
repos trying to operate against missing shared resources.

## Prerequisites

Set the backend values first:

```bash
export AWS_REGION=eu-west-1
export TF_BACKEND_BUCKET=<terraform-state-bucket>
export TF_BACKEND_DYNAMODB_TABLE=<terraform-lock-table>
export TF_BACKEND_KMS_KEY_ID=<optional-kms-key-id>
```

Optional image tag override:

```bash
export IMAGE_TAG=latest
```

## Test Environment

From the workspace root:

```bash
cd raksha-platform
scripts/ci/terraform_destroy.sh test "${IMAGE_TAG:-latest}"
```

```bash
cd raksha-simlab
scripts/ci/terraform_destroy.sh test "${IMAGE_TAG:-latest}"
```

```bash
cd raksha-core
scripts/ci/terraform_destroy.sh test "${IMAGE_TAG:-latest}"
```

## Stage / Prod

Use the same ordering:

```bash
cd raksha-platform
scripts/ci/terraform_destroy.sh stage "${IMAGE_TAG:-latest}"
cd ../raksha-simlab
scripts/ci/terraform_destroy.sh stage "${IMAGE_TAG:-latest}"
cd ../raksha-core
scripts/ci/terraform_destroy.sh stage "${IMAGE_TAG:-latest}"
```

```bash
cd raksha-platform
scripts/ci/terraform_destroy.sh prod "${IMAGE_TAG:-latest}"
cd ../raksha-simlab
scripts/ci/terraform_destroy.sh prod "${IMAGE_TAG:-latest}"
cd ../raksha-core
scripts/ci/terraform_destroy.sh prod "${IMAGE_TAG:-latest}"
```

## Optional Backend Cleanup

Terraform destroy removes resources tracked in state. If you also want to reset
the remote backend objects after a full teardown, clean the state objects only
after all three destroys succeed.

Example `test` state keys:

```bash
aws s3api delete-object --bucket "${TF_BACKEND_BUCKET}" --key "raksha/raksha-platform/test/terraform.tfstate" --region "${AWS_REGION}"
aws s3api delete-object --bucket "${TF_BACKEND_BUCKET}" --key "raksha/raksha-simlab/test/terraform.tfstate" --region "${AWS_REGION}"
aws s3api delete-object --bucket "${TF_BACKEND_BUCKET}" --key "raksha/raksha-core/test/terraform.tfstate" --region "${AWS_REGION}"
```

If stale locks remain, clear them with `terraform force-unlock` from the target
repo/env after `terraform init`.

## Important Notes

- These commands only destroy resources Terraform still knows about.
- If you manually deleted or changed AWS resources earlier, you can still have
  leftovers that need manual cleanup afterward.
- For a clean rebuild, recreate in the reverse order:
  1. `raksha-core`
  2. `raksha-platform`
  3. `raksha-simlab`
