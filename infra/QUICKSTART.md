# Local Testing Quick Start

This guide shows you how to run raksha-core locally with Docker Compose.

## Prerequisites

- Docker Desktop installed and running

## Step 1: Configure Environment

```bash
cd raksha-core/infra

# Copy the environment template
cp .env.example .env
```

Edit `.env` if you want to change detector toggles or logging:

```bash
# Optional: Logging
RUST_LOG=info
# For debug: RUST_LOG=debug,hyper=info,tokio=info
```

## Step 2: Start Services

```bash
# Start infrastructure services
docker compose up -d postgres redis

# Wait for health checks (about 10 seconds)
docker compose ps

# Start core services (will build first time)
docker compose up --build indexer detector orchestrator finality
```

## Step 3: Verify It's Working

### Check service logs:
```bash
# Watch all services
docker compose logs -f

# Watch specific service
docker compose logs -f indexer
```

### Check database is initialized:
```bash
docker compose exec postgres psql -U postgres -d raksha -c "\dt"
docker compose exec postgres psql -U postgres -d raksha -c "SELECT * FROM patterns;"
```

### Check Redis streams:
```bash
# Should see events flowing through
docker compose exec redis redis-cli XINFO STREAM raksha:unified-events
docker compose exec redis redis-cli XINFO STREAM raksha:detections
```

## What You Should See

If configured correctly, you'll see:

1. **Indexer**: Loading active stream configs from Postgres and starting workers
   ```
   INFO indexer: db-driven stream supervisor started
   INFO indexer: stream worker started by reconcile
   ```

2. **Detector**: Processing events, running pattern detection
   ```
   INFO detector: Consumed event from unified-events stream
   INFO detector: Pattern check: depeg for USDC/USD
   ```

3. **Orchestrator**: Enriching detections
   ```
   INFO orchestrator: Processing detection
   ```

4. **Finality**: Tracking block confirmations
   ```
   INFO finality: Finalized block 19234550
   ```

## Getting Data

### Option 1: Simulated Data (Recommended for Local Testing)

Use simlab to generate test scenarios:

```bash
# Start with simlab profile
docker compose --profile simlab up -d

# Run specific scenario
docker compose run --rm simlab run batch --scenario flash_loan_attack
docker compose run --rm simlab run batch --scenario usdc_depeg
```

### Option 2: DB-Managed Live Streams

Create or enable sources and stream configs in the catalog tables, then point the relevant connectors at their real upstream endpoints through per-source connection config. The indexer will pick them up automatically through `LISTEN source_stream_config_changed` plus periodic reconcile.

## Common Issues & Solutions

### No events appearing
- Check `RUST_LOG=debug` to see worker startup and stream activity
- Verify patterns are enabled: `SELECT * FROM patterns WHERE enabled=true;`
- Check data sources and streams: `SELECT * FROM catalog.data_sources;` and `SELECT * FROM catalog.source_stream_configs;`

### Database not initialized
- First startup auto-loads schema from `/docker-entrypoint-initdb.d/`
- If database already exists, drop it: `docker compose down -v`
- Then restart: `docker compose up -d postgres redis`

## Useful Commands

### Restart single service:
```bash
docker compose restart detector
```

### Rebuild after code changes:
```bash
docker compose build indexer detector orchestrator finality
docker compose up -d
```

### View logs with timestamps:
```bash
docker compose logs -f --timestamps indexer
```

### Stop all services:
```bash
docker compose down
```

### Clean everything (including data):
```bash
docker compose down -v
```

## Optional: Debug Tools

Start with GUI tools for Redis and PostgreSQL:

```bash
docker compose --profile debug up -d
```

Access:
- **Redis Commander**: http://localhost:8081
- **pgAdmin**: http://localhost:8080 (email: admin@raksha.local, password: admin)

## Next Steps

1. ✅ Verify services are running
2. ✅ Confirm data is flowing through Redis streams
3. ✅ Check detections are being stored in database
4. 📊 Query detections: `SELECT * FROM detections ORDER BY created_at DESC LIMIT 10;`
5. 🚨 Query alerts: `SELECT * FROM alerts ORDER BY created_at DESC LIMIT 10;`

## Production Deployment

For production deployment to AWS ECS:
- See `terraform/` directory for infrastructure-as-code
- Use `service-catalog.yaml` for service definitions
- Database bootstrap files are in `sql/bootstrap/` (`core_schema.sql`, `history_schema.sql`, `seed_sources.sql`, `seed_patterns.sql`, `seed_history_replay.sql`, and `raw_schema.sql` for `raksha_raw`)
