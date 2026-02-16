# defi-surv-core

Rust data-plane workspace aligned to architecture domains.

## Workspace crates

- `event-schema`: canonical event and detection contracts.
- `common`: shared interfaces, config loading, errors, logging, and event-id helpers.
- `ingestion`: EVM ingestion and checkpoint/reorg runtime (`cargo run -p ingestion`).
- `feature-builder`: feature enrichment runtime + reference price provider.
- `detection-engine`: rule evaluation + risk scoring runtime.
- `risk-scorer`: deterministic 0-100 risk scoring.
- `state-manager`: Redis stream bus + persistence/finality/correlation + state runtime.
- `notifier`: notifier client and sink adapters to `notifier-gateway`.

## Environment

Copy `.env.example` and set relevant values.

```bash
cp .env.example .env
```

Important runtime variables:
- `RULES_REPO_PATH`: optional path to `defi-surv-rules`.
- `ETH_WS_URL` / `BASE_WS_URL`: enable live adapters.
- `DATABASE_URL`: Postgres backing for state manager.
- `REDIS_URL`: Redis Streams transport.
- `NOTIFIER_GATEWAY_URL`: endpoint used by notifier sink adapters (`/dispatch`).

## Local Docker stack (recommended)

From repository root (`defi-surv/`):

```bash
# Optional for live Ethereum ingestion; omit to run in mock mode
export ETH_WS_URL=wss://eth-mainnet.g.alchemy.com/v2/<your-key>

docker compose up -d --build
docker compose logs -f indexer detector
```

This starts Postgres, Redis, indexer, and detector together.

## Run binaries

```bash
/usr/local/cargo/bin/cargo run -p ingestion
/usr/local/cargo/bin/cargo run -p feature-builder
/usr/local/cargo/bin/cargo run -p detection-engine
/usr/local/cargo/bin/cargo run -p state-manager
```

## Run checks

```bash
/usr/local/cargo/bin/cargo check
/usr/local/cargo/bin/cargo test --workspace
```
