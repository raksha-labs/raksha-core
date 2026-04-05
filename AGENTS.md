# AGENTS.md (raksha-core)

AI tool guidance for the `raksha-core` data-plane repository.

## Scope

Work in this repo for:
- Rust runtimes in `apps/*`
- Shared crates in `crates/*`
- Detection rules in `rules/*`
- Core SQL/bootstrap assets in `infra/sql/*`
- Core Docker/runtime packaging such as `Dockerfile`

Do not implement platform UI or control-plane service behavior here unless the change is a core contract the platform consumes.

## Repo Flow

Primary alert pipeline:

1. `indexer`
   Reads raw chain/off-chain inputs, normalizes them, and writes ingest/state data used by pattern evaluation.
2. `detector`
   Loads enabled pattern configs and evaluates registered patterns such as `depeg`, `tvl_drop`, `utilization_high`, and `flash_loan`.
3. `DetectionResult`
   A pattern emits a normalized detection payload with risk, confidence, subject, evidence context, and `detected_at`.
4. `orchestrator`
   Converts detections into canonical alert events, assigns lifecycle metadata, and persists the alert contract the platform later reads.
5. `finality`
   Advances alert lifecycle/finality state as more information arrives.
6. Core outputs
   Alerts, lifecycle updates, and supporting evidence become the source of truth for platform investigation and notification flows.

## Pattern Authoring Flow

When changing a pattern:

1. Update the detector implementation in `apps/detector/src/patterns/*`.
2. Update shared event contracts in `crates/event-schema` if the alert/detection shape changes.
3. Update orchestrator mapping if new detection fields must survive into alerts.
4. Keep emitted `oracle_context`, `confidence_breakdown`, subject metadata, and timestamps aligned with what the platform investigation UI needs.
5. Validate the scenario end to end, not just the Rust unit.

## Contracts Owned By Core

Core is the source of truth for:
- `DetectionResult`
- `AlertEvent`
- Pattern-specific `oracle_context`
- Lifecycle/finality status
- Evidence/event timestamps such as `detected_at`, `created_at`, `block_number`, and `tx_hash`

If a field is missing in the platform investigation experience, first verify whether core emitted it at all before patching platform rendering.

## Standard Validation

Run after code changes:

```bash
cargo check
cargo test --workspace
```

When the change affects emitted alerts, also validate against the local stack from the workspace root:

```bash
./raksha-scripts/stack.sh restart core
python3 .codex/skills/simlab-scenario-e2e/scripts/run_verify.py --scenario <scenario-id> --mode keep --json
```

## Required Validation Gates

Before closing any core change, validate the relevant gates below and report any failures explicitly:

- `core · cargo fmt check`
- `core · cargo clippy`
- `core · cargo audit`
- `core · cargo test`
- `core · terraform fmt`
- `core · terraform validate (test)`
- `core · terraform validate (stage)`
- `core · terraform validate (prod)`
- `core · tflint (test)`
- `core · tflint (stage)`
- `core · tflint (prod)`
- `core · tfsec (soft-fail)`

Minimum expectation by change type:

- Rust/runtime change: run the Rust gates.
- SQL/schema/runtime packaging change: run Rust gates plus representative scenario validation.
- Terraform/IaC change: run Terraform, `tflint`, and `tfsec`.
- Cross-cutting change: run both the Rust and IaC gates that apply.

Do not mark work complete if any required gate was skipped. If a gate fails, include the exact failing gate in the handoff.

## Local Development

Useful local commands:

```bash
cargo check
cargo test --workspace
docker compose up -d --build
docker compose logs -f indexer detector orchestrator finality
```

Common env vars:
- `DATABASE_URL`
- `REDIS_URL`
- `RUST_LOG`
- `ETH_WS_URL`
- `BASE_WS_URL`

## Definition of Done

- The change stays within core ownership.
- Rust checks pass, or failures are reported clearly.
- Required validation gates were run for the affected scope, or explicitly called out as not run.
- New alert fields are carried through detector -> orchestrator -> stored alert payload.
- Behavior is validated against at least one representative local scenario when alert outputs changed.
- No unrelated files are modified.
