# Database Schema

This directory contains the SQL files for the Raksha database.

## Files

- **`schema.sql`** - Complete database schema for fresh deployments
  - Contains all table definitions, indexes, and constraints
  - Run this first on a new database

- **`seed_data.sql`** - Initial data for patterns and data sources
  - Seeds default patterns (dpeg, flash_loan)
  - Seeds example data sources (Binance, Coinbase, Uniswap, Chainlink, etc.)
  - Creates default "glider" tenant configuration
  - Run after schema.sql

- **`upgrade_poll_interval_ms.sql`** - One-time upgrade for existing databases
  - Adds `source_stream_configs.poll_interval_ms`
  - Backfills defaults for polling connectors (`rpc_logs=2000`, `http_poll=5000`)
  - Applies range constraint (`200..60000` ms, nullable for websocket)

- **`upgrade_generic_incidents.sql`** - One-time upgrade for existing databases
  - Adds `incident_id` columns/indexes to `alerts` and `alert_lifecycle_events`
  - Creates generic incident lifecycle tables:
    - `incidents`
    - `incident_events`
    - `incident_context_snapshots`
- **`upgrade_history_intelligence.sql`** - One-time upgrade for history intelligence layer
  - Creates `history` schema and replay/history tables:
    - `history.cases`
    - `history.case_events`
    - `history.case_alert_links`
    - `history.replay_catalog`
    - `history.ml_feature_registry`
    - `history.ingest_offsets`

## Quick Start

### For Fresh Installation:

```bash
# 1. Create database
psql -U postgres -c "CREATE DATABASE raksha;"

# 2. Apply schema
psql -U postgres -d raksha -f schema.sql

# 3. Load seed data
psql -U postgres -d raksha -f seed_data.sql
```

### For Existing Installation Upgrade:

```bash
psql -U postgres -d raksha -f upgrade_poll_interval_ms.sql
psql -U postgres -d raksha -f upgrade_generic_incidents.sql
psql -U postgres -d raksha -f upgrade_history_intelligence.sql
```

### With Docker:

```bash
# Copy SQL files into running postgres container
docker cp schema.sql raksha-postgres:/tmp/schema.sql
docker cp seed_data.sql raksha-postgres:/tmp/seed_data.sql

# Execute schema
docker exec -i raksha-postgres psql -U postgres -d raksha -f /tmp/schema.sql

# Execute seed data
docker exec -i raksha-postgres psql -U postgres -d raksha -f /tmp/seed_data.sql
```

### Alternative Docker Method (via stdin):

```bash
# Apply schema
docker exec -i raksha-postgres psql -U postgres -d raksha < schema.sql

# Apply seed data
docker exec -i raksha-postgres psql -U postgres -d raksha < seed_data.sql
```

## Schema Overview

### Core Tables
- `detections` - Detection results from pattern matching
- `alerts` - Alerts generated from detections
- `finality_state` - Block finality tracking state

### Pattern System
- `patterns` - Pattern definitions (dpeg, flash_loan, etc.)
- `pattern_configs` - Default pattern configurations
- `tenant_pattern_configs` - Per-tenant pattern policies
- `pattern_state` - Runtime pattern state persistence
- `pattern_snapshots` - Audit trail of pattern evaluations

### Data Sources
- `data_sources` - Available data source catalog
- `tenant_data_sources` - Per-tenant data source assignments
- `data_source_health` - Health monitoring for data sources

### Events & Lifecycle
- `raw_events` - Global tenant-agnostic raw feed events from all configured streams
- `alert_lifecycle_events` - Alert state transitions
- `tenant_policies` - Tenant-level notification policies

### Analytics
- `feature_vectors` - ML/analysis feature data
- `dependency_edges` - Protocol dependency graphs

## Notes

- All tables use `IF NOT EXISTS` for safe re-running
- Seed data uses `ON CONFLICT DO NOTHING` for idempotency
- Replace placeholder API keys in seed_data.sql before production use
- See schema.sql comments for production optimization recommendations
