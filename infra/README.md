# Infrastructure Overview

This directory contains all infrastructure configuration for **both local development and AWS cluster deployment**.

## 🚀 Quick Start

**For Local Testing:** See [QUICKSTART.md](./QUICKSTART.md)

**For AWS Deployment:** See [terraform/README.md](./terraform/README.md)

## Directory Structure

```
infra/
├── QUICKSTART.md              # 👈 START HERE for local testing
├── README.md                  # This file - infrastructure overview
├── docker-compose.yml         # Local development orchestration
├── service-catalog.yaml       # Service definitions (used by ECS)
├── .env.example              # Environment variable template for local
├── sql/                      # Database schema (shared by local & cluster)
│   ├── schema.sql           # Complete database schema
│   ├── seed_data.sql        # Initial patterns and data sources
│   └── README.md            # Schema documentation
└── terraform/               # AWS ECS cluster infrastructure
    ├── main.tf              # Root Terraform configuration
    ├── environments/        # Environment-specific configs
    │   ├── test/           # Low-cost testing environment
    │   ├── stage/          # Pre-production environment
    │   └── prod/           # Production environment
    └── modules/            # Reusable Terraform modules
```

## Service Architecture

### Core Services (4)
- **indexer** - Ingests events from EVM chains → unified-events stream
- **detector** - Pattern-based detection (dpeg, flash_loan) → detections stream
- **orchestrator** - Enrichment and alert routing
- **finality** - Block finality tracking and state management

### Infrastructure
- **PostgreSQL** - Persistent data store (14 tables - see sql/README.md)
- **Redis** - Event streams: unified-events, detections, alerts

## Local Development

**👉 See [QUICKSTART.md](./QUICKSTART.md) for complete local testing guide.**

Quick commands:

```bash
cd infra

# 1. Configure RPC endpoints
cp .env.example .env
# Edit .env: Add your Alchemy/Infura WebSocket URL

# 2. Start services
docker compose up -d postgres redis
docker compose up --build indexer detector orchestrator finality

# 3. Verify
docker compose logs -f indexer
```
**Automatic initialization** on first postgres container start:
- `sql/schema.sql` - All table definitions
- `sql/seed_data.sql` - Default patterns (dpeg, flash_loan) and data sources

**Manual application** (if needed):

```bash
docker compose exec -i postgres psql -U postgres -d raksha < sql/schema.sql
docker compose exec -i postgres psql -U postgres -d raksha < sql/seed_data.sql
```

See [sql/README.md](./sql/README.md) for complete schema documentation
docker compose exec -i postgres psql -U postgres -d raksha < sql/schema.sql
docker compose exec -i postgres psql -U postgres -d raksha < sql/seed_data.sql
```

See [sql/README.md](./sql/README.md) for schema details.

## AWS Deployment

Deploy to AWS ECS using Terraform:

```bash
cd terraform/environments/test
terraform init
terraform plan -out=tfplan
terraform apply tfplan
```

Service definitions in `service-catalog.yaml` are automatically used by Terraform to create ECS tasks.

## Configuration Files

### service-catalog.yaml
Defines the 4 core services with:
- Container specifications (CPU, memory)
- Scaling policies (fixed, worker_cpu_and_lag)
- Desired counts per environment
- Health check paths
- Command overrides
for ECS deployment:
- Container specs (CPU: 256-1024, Memory: 512-2048)
- Scaling policies (fixed, worker_cpu_and_lag)
- Desired counts per environment (test: 1, stage: 1-3, prod: 2-5)
- Used by both local docker-compose and AWS Terraform

### .env (local only)
Environment variables for local docker-compose:
- `ETH_WS_URL` - Primary Ethereum RPC WebSocket endpoint (**required**)
- `ETH_WS_URL_BACKUP` - Backup RPC endpoint
- `RUST_LOG` - Logging level (info/debug)

See `.env.example` for template.
- Single consolidated schema (sql/schema.sql)
- Seed data for patterns and sources (sql/seed_data.sql)
- Docker Compose for local testing
- Service catalog for 4 core services
- Terraform modules for AWS deployment

❌ **Removed:**
- 7 migration SQL files (001-007)
- Legacy component references (market-indexer, market-detector, dpeg-engine, detection-engine, scorer)
- UWhat Configs to Modify for Actual Data

### For Local Testing:

**1. RPC Endpoints** (Required):
```bash
# Edit infra/.env
ETH_WS_URL=wss://eth-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY
ETH_WS_URL_BACKUP=wss://mainnet.infura.io/ws/v3/YOUR_INFURA_KEY
```

**2. Data Sources** (Optional - defaults provided):
```sql
-- Edit sql/seed_data.sql to customize data sources
-- Or update via database after startup
UPDATE data_sources SET connection_config = '{"rpc_url": "wss://..."}' WHERE source_id = 'ethereum-mainnet';
```

### For AWS Cluster:

**1. Terraform Variables**:
```bash
# Edit terraform/environments/prod/terraform.tfvars
rpc_endpoints = {
  ethereum = "wss://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"
  arbitrum = "wss://arb-mainnet.g.alchemy.com/v2/YOUR_KEY"
}
```

**2. Secrets Manager**:
- Store RPC API keys in AWS Secrets Manager
- Reference in ECS task definitions

## Notes

- Schema auto-initializes on first postgres container start
- All SQL files use `IF NOT EXISTS` for safe re-running
- Seed data uses `ON CONFLICT DO NOTHING` for idempotency
- Replace placeholder API keys in seed_data.sql before production use
- For production: Use AWS Secrets Manager for RPC endpoints and database credentials

