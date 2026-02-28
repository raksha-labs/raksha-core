# Local Testing (Core)

## Prerequisites

- Rust toolchain (stable)
- Docker + Docker Compose
- `psql` optional (we use `docker exec` commands below)

Repository root for commands: `raksha-core/`.

## 1. Start local dependencies

```bash
docker compose -f infra/local/docker-compose.yml up -d
```

This starts:
- Postgres (`localhost:5432`, db `raksha`, user `postgres`, password `postgres`)
- Redis (`localhost:6379`)

## 2. Apply schema migrations

```bash
for f in infra/sql/001_init.sql infra/sql/002_lifecycle_tenant.sql; do
  docker exec -i raksha-postgres psql -U postgres -d raksha < "$f"
done
```

## 3. Export runtime env

```bash
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/raksha
export REDIS_URL=redis://localhost:6379
export RULES_REPO_PATH=$(pwd)/rules
export RUST_LOG=info
export STREAM_SUPERVISOR_ENABLED=true
export STREAM_PURGE_ENABLED=true
```

For chain ingestion, also set `ETH_WS_URL` / `BASE_WS_URL` if you want live adapter mode.
For DB-managed stream ingestion, configure rows in:
- `data_sources`
- `source_stream_configs`
- `source_stream_tenant_targets`

If a stream endpoint template contains placeholders (for example `wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}`),
set the value in `source_stream_configs.auth_config` (for example `{"alchemy_api_key":"<key>"}`) via admin UI/API.

Indexer will automatically start/stop stream workers from DB config changes (no process restart required).

## 4. Run core workers

Run in separate terminals as needed:

```bash
cargo run -p indexer
cargo run -p detector
cargo run -p orchestrator
cargo run -p finality
```

## 5. Smoke checks

Redis stream activity:

```bash
docker exec -it raksha-redis redis-cli XINFO STREAM raksha:unified-events
docker exec -it raksha-redis redis-cli XINFO STREAM raksha:detections
```

Database rows:

```bash
docker exec -it raksha-postgres psql -U postgres -d raksha -c "SELECT count(*) FROM detections;"
docker exec -it raksha-postgres psql -U postgres -d raksha -c "SELECT count(*) FROM alerts;"
docker exec -it raksha-postgres psql -U postgres -d raksha -c "SELECT count(*) FROM pattern_snapshots;"
docker exec -it raksha-postgres psql -U postgres -d raksha -c "SELECT stream_config_id, source_id, event_type, observed_at FROM raw_events ORDER BY observed_at DESC LIMIT 20;"
docker exec -it raksha-postgres psql -U postgres -d raksha -c "SELECT count(*) FROM raw_events WHERE event_type IN ('quote','trade') AND observed_at < NOW() - INTERVAL '5 minutes';"
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

# Targeted pattern checks
cargo test -p detector dpeg
cargo test -p detector flash_loan
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

When `raksha-core` and `raksha-platform` run independently, keep database ports distinct if both stacks run on the same machine. If you wire platform control APIs to core data, point `CONTROL_CORE_DATABASE_URL` (platform) to this core Postgres instance.
