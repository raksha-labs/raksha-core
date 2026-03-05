# Local Testing Quick Start

This guide shows you how to run raksha-core locally with Docker Compose.

## Prerequisites

- Docker Desktop installed and running
- An Ethereum RPC endpoint (Alchemy, Infura, or Ankr)

## Step 1: Configure Environment

```bash
cd raksha-core/infra

# Copy the environment template
cp .env.example .env
```

Edit `.env` and add your **actual RPC endpoints**:

```bash
# Required: Ethereum mainnet WebSocket endpoints
ETH_WS_URL=wss://eth-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY
ETH_WS_URL_BACKUP=wss://mainnet.infura.io/ws/v3/YOUR_INFURA_KEY

# Optional: Adjust ingestion settings
INGESTION_POLL_INTERVAL_SECS=5
INGESTION_MAX_RETRIES=20
INGESTION_RETRY_BACKOFF_MS=2000

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

If configured correctly with real RPC endpoints, you'll see:

1. **Indexer**: Connecting to Ethereum, processing blocks
   ```
   INFO indexer: Connected to Ethereum mainnet
   INFO indexer: Processing block 19234567
   ```

2. **Detector**: Processing events, running pattern detection
   ```
   INFO detector: Consumed event from unified-events stream
   INFO detector: Pattern check: dpeg for USDC/USD
   ```

3. **Orchestrator**: Enriching detections
   ```
   INFO orchestrator: Processing detection
   ```

4. **Finality**: Tracking block confirmations
   ```
   INFO finality: Finalized block 19234550
   ```

## Getting Actual Data

### Option 1: Real Ethereum Data (Recommended for Testing)

Configure `.env` with your RPC endpoints as shown above. The indexer will:
- Connect to Ethereum mainnet
- Monitor for flash loan events
- Process transactions in real-time

### Option 2: Simulated Data (For Development)

Use simlab to generate test scenarios:

```bash
# Start with simlab profile
docker compose --profile simlab up -d

# Run specific scenario
docker compose run --rm simlab run batch --scenario flash_loan_attack
docker compose run --rm simlab run batch --scenario usdc_depeg
```

### Option 3: Historical Data Replay

Point to a local archive node:
```bash
ETH_WS_URL=wss://your-local-archive-node:8546
```

## Common Issues & Solutions

### "Connection refused" to RPC endpoint
- Check your API key is valid
- Verify the endpoint URL format (must start with `wss://`)
- Test the endpoint: `wscat -c "wss://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"`

### No events appearing
- Check `RUST_LOG=debug` to see connection attempts
- Verify patterns are enabled: `SELECT * FROM patterns WHERE enabled=true;`
- Check data sources: `SELECT * FROM tenant_data_sources WHERE tenant_id='glider';`

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
