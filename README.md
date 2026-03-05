# raksha-core

Rust data-plane workspace — the event-processing and detection runtime for the Raksha platform.

## Architecture overview

```
EVM chains / CEX / DEX / Oracles
          │
          ▼
      [indexer]          ← DB-driven multi-source supervisor
          │  publishes UnifiedEvent
          ▼
  raksha:unified-events  (Redis Stream)
          │
          ▼
      [detector]         ← pattern registry (DPEG, Flash-Loan, …)
          │  publishes DetectionResult
          ▼
  raksha:detections
          │
          ▼
   [orchestrator]        ← lifecycle, quota enforcement, notifier dispatch
          │  publishes AlertEvent
          ▼
  raksha:alerts
```

## Workspace crates

| Crate | Role |
|---|---|
| `event-schema` | Canonical event and detection contracts (`UnifiedEvent`, `DetectionResult`, `AlertEvent`, …). |
| `common` | Shared traits (`ChainAdapter`, `RuleEvaluator`, `RiskScorer`), config loading, logging, circuit-breaker, health. |
| `ingestion` | EVM chain adapter, `DataSourceConnector` trait, `EvmChainConnector`, `CexWebsocketConnector`. |
| `feature-builder` | Post-detection enrichment (trace analysis, oracle context injection). |
| `risk-scorer` | Deterministic 0–100 risk scoring from `DetectionResult` signals. |
| `state-manager` | Redis stream bus + PostgreSQL repository + finality/correlation state. |
| `notifier` | Notifier client and sink adapters to `notifier-gateway`. |

## Apps

| Binary | Role |
|---|---|
| `apps/indexer` | DB-driven supervisor: loads `tenant_data_sources`, spawns per-source connectors, publishes `UnifiedEvent`. |
| `apps/detector` | Pattern registry consumer: reads `unified-events` stream, runs every registered pattern, publishes `DetectionResult`. |
| `apps/orchestrator` | Detection lifecycle manager: quota enforcement, alert emission, notifier dispatch. |
| `apps/finality` | Confirmation-depth tracker and reorg handler. |

Detection patterns live in `apps/detector/src/patterns/`:
- `dpeg.rs` — dollar-depeg multi-source weighted median consensus
- `flash_loan.rs` — EVM flash-loan profit extraction

New patterns are added by implementing the `DetectionPattern` trait and registering in `PatternRegistry::new()`.

## Environment

```bash
cp .env.example .env
```

Key variables:

| Variable | Purpose |
|---|---|
| `DATABASE_URL` | PostgreSQL connection string |
| `REDIS_URL` | Redis connection string |
| `ETH_WS_URL` / `BASE_WS_URL` | Live EVM WebSocket RPC (optional; mock mode used if absent) |
| `NOTIFIER_GATEWAY_URL` | Notifier gateway endpoint (`/dispatch`) |
| `RUST_LOG` | Log level filter (e.g. `info`, `debug`) |
| `HEALTH_CHECK_ENABLED` | Enable `/health` + `/ready` endpoints |
| `HEALTH_CHECK_PORT` | Health port (default `8080`) |

## Local Docker stack (recommended)

From the repository root (`raksha-core/`):

```bash
# Optional — enables live EVM ingestion; omit to run in mock mode
export ETH_WS_URL=wss://eth-mainnet.g.alchemy.com/v2/<your-key>

docker compose up -d --build
docker compose logs -f indexer detector
```

This starts Postgres, Redis, indexer, and detector.

## SQL migrations

Apply bootstrap SQL in order:

```bash
docker exec -i raksha-postgres psql -U postgres -d raksha < infra/sql/bootstrap/core_schema.sql
docker exec -i raksha-postgres psql -U postgres -d raksha < infra/sql/bootstrap/history_schema.sql
docker exec -i raksha-postgres psql -U postgres -d raksha < infra/sql/bootstrap/seed_sources.sql
docker exec -i raksha-postgres psql -U postgres -d raksha < infra/sql/bootstrap/seed_patterns.sql
docker exec -i raksha-postgres psql -U postgres -d raksha < infra/sql/bootstrap/seed_history_replay.sql

docker exec -i raksha-postgres-raw psql -U postgres -d raksha_raw < infra/sql/bootstrap/raw_schema.sql
```

## Run checks

```bash
cargo check --workspace
cargo test --workspace
```

## Run individual workers

```bash
cargo run -p indexer
cargo run -p detector
cargo run -p orchestrator
cargo run -p finality
```

## AWS IaC

Core-specific IaC lives in:

- `infra/service-catalog.yaml` (core service definitions)
- `infra/terraform/environments/{test,stage,prod}`
- `infra/terraform/modules/*`

Local Terraform validation:

```bash
./scripts/local-terraform-check.sh
```
