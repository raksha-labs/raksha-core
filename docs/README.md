# defi-surv-core Documentation

This folder documents the **core data plane** repository (Rust workers, unified event pipeline, pattern-based detection, and core AWS IaC overlays).

## Scope

Owned by `defi-surv-core`:
- Multi-source event ingestion (EVM, CEX, DEX, Oracle)
- Pattern-based detection engine with extensible DetectionPattern trait
- Alert orchestration and finality tracking
- Core runtime streams and core PostgreSQL schema
- Core ECS/Terraform service catalog and deployment modules

Not owned by this repo:
- Tenant-facing APIs and UIs (`defi-surv-platform`)
- Admin APIs and control-plane policy/config lifecycle (`defi-surv-platform`)

## Documents

- `ARCHITECTURE.md`: core runtime architecture, data flow, streams, and deployment model.
- `LOCAL_TESTING.md`: local setup, migrations, worker startup, smoke checks, and debug commands.
- `SCENARIO_DOLLAR_DEPEG_ALERT.md`: DPEG alert flow from market quote ingestion to detection emission.
- `SCENARIO_FLASH_LOAN_ATTACK.md`: flash-loan attack detection flow from chain events to alert lifecycle.

## Related Repositories

- `defi-surv-platform`: control plane, API facade, policy/config services, tenant/admin UI.
- `defi-surv-simlab`: scenario generation and deterministic stress/simulation tooling.
