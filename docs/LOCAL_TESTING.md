# Local Testing (Core)

## Prerequisites

- Rust toolchain (stable)
- Docker + Docker Compose
- `psql` optional (we use `docker exec` commands below)

Repository root for commands: `defi-surv-core/`.

## 1. Start local dependencies

```bash
docker compose -f infra/local/docker-compose.yml up -d
```

This starts:
- Postgres (`localhost:5432`, db `defi_surv`, user `postgres`, password `postgres`)
- Redis (`localhost:6379`)

## 2. Apply schema migrations

```bash
for f in infra/sql/001_init.sql infra/sql/002_lifecycle_tenant.sql infra/sql/003_market_dpeg.sql; do
  docker exec -i defi-surv-postgres psql -U postgres -d defi_surv < "$f"
done
```

## 3. Export runtime env

```bash
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/defi_surv
export REDIS_URL=redis://localhost:6379
export RULES_REPO_PATH=$(pwd)/rules
export RUST_LOG=info

# Optional market flow flags
export MARKET_DPEG_ENABLED=true
export DPEG_ALERTS_EMIT_ENABLED=true
```

For chain ingestion, also set `ETH_WS_URL` / `BASE_WS_URL` if you want live adapter mode.

## 4. Run core workers

Run in separate terminals as needed:

```bash
cargo run -p indexer
cargo run -p detector
cargo run -p orchestrator
cargo run -p finality
```

Market DPEG path:

```bash
cargo run -p market-indexer
cargo run -p market-detector
```

## 5. Smoke checks

Redis stream activity:

```bash
docker exec -it defi-surv-redis redis-cli XINFO STREAM defi-surv:normalized-events
docker exec -it defi-surv-redis redis-cli XINFO STREAM defi-surv:detections
docker exec -it defi-surv-redis redis-cli XINFO STREAM defi-surv:market-quotes
```

Database rows:

```bash
docker exec -it defi-surv-postgres psql -U postgres -d defi_surv -c "SELECT count(*) FROM detections;"
docker exec -it defi-surv-postgres psql -U postgres -d defi_surv -c "SELECT count(*) FROM alerts;"
docker exec -it defi-surv-postgres psql -U postgres -d defi_surv -c "SELECT count(*) FROM market_consensus_snapshots;"
```

Optional worker health endpoints (if enabled):

```bash
export HEALTH_CHECK_ENABLED=true
export HEALTH_CHECK_PORT=8082
cargo run -p detector
# in another shell
curl -s http://localhost:8082/health
curl -s http://localhost:8082/ready
```

## 6. Automated checks

```bash
cargo check
cargo test --workspace

# Targeted scenario checks
cargo test -p ingestion decode_flash_loan
cargo test -p dpeg-engine
```

## 7. Terraform checks (repo-local)

```bash
./scripts/local-terraform-check.sh
```

## 8. Cleanup

```bash
docker compose -f infra/local/docker-compose.yml down -v
```

## Notes for split-repo setup

When `defi-surv-core` and `defi-surv-platform` run independently, keep database ports distinct if both stacks run on the same machine. If you wire platform control APIs to core data, point `CONTROL_CORE_DATABASE_URL` (platform) to this core Postgres instance.
