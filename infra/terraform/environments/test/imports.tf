# Test environment imports are handled imperatively in `scripts/ci/terraform_apply.sh`.
#
# We intentionally avoid declarative `import {}` blocks here because they fail hard
# during plan/apply when a previously imported AWS object has been deleted as part of
# a clean rebuild. The CI reconcile/apply scripts already check whether resources
# exist before importing them, which is safe for both drift recovery and full resets.
