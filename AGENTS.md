# AGENTS.md (raksha-core)

AI tool guidance for the **core data-plane** repository.

## Scope

Work only in this repo for:
- Rust runtimes in `apps/*`
- Shared runtime crates in `crates/*`
- Rule assets in `rules/*`
- Core IaC in `infra/service-catalog.yaml` and `infra/terraform/*`
- Core CI/CD workflows in `.github/workflows/*`

Do not change platform control-plane code from this repo.

## Primary Runtime Model

Unified Pipeline:
`indexer -> detector (pattern registry: dpeg, flash_loan) -> orchestrator -> finality -> alerts`

Patterns are registered within detector and configured via tenant_pattern_configs table.
Redis streams and PostgreSQL schema are core runtime contracts.

## Standard Validation

Run after code changes:

```bash
cargo check
cargo test --workspace
```

Run after Terraform/IaC changes:

```bash
./scripts/local-terraform-check.sh
```

## Local Development

Dependencies only:

```bash
docker compose -f infra/local/docker-compose.yml up -d
```

Core stack (from workspace root):

```bash
docker compose up -d --build
docker compose logs -f indexer detector
```

Typical env vars:
- `DATABASE_URL`
- `REDIS_URL`
- `RULES_REPO_PATH`
- `ETH_WS_URL` / `BASE_WS_URL`
- `RUST_LOG`

## IaC and Deploy

- Region baseline: `eu-west-1`
- Terraform environments: `infra/terraform/environments/{test,stage,prod}`
- Service definitions source of truth: `infra/service-catalog.yaml`

If service runtime shape changes (image/port/cpu/memory/exposure/deploy strategy), update service catalog and Terraform consistently.

## CI/CD Files

- `.github/workflows/ci.yml`
- `.github/workflows/images.yml`
- `.github/workflows/infra-plan.yml`
- `.github/workflows/deploy-test.yml`
- `.github/workflows/deploy-stage-prod.yml`

Keep immutable tags and rollback-safe deployment behavior.

## Documentation

For behavior or operational changes, update:
- `docs/ARCHITECTURE.md`
- `docs/LOCAL_TESTING.md`
- scenario docs in `docs/`

## Definition of Done

- Change stays within core scope.
- Rust checks/tests pass (or failures are reported clearly).
- IaC checks run if infra changed.
- Docs updated when behavior/ops changed.
- No unrelated files modified.
