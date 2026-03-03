# Infrastructure Overview

This directory contains local runtime and AWS infrastructure for `raksha-core`.

## Layout

```text
infra/
├── QUICKSTART.md
├── README.md
├── docker-compose.yml
├── service-catalog.yaml
├── .env.example
├── sql/
│   ├── bootstrap/
│   │   ├── core_schema.sql
│   │   ├── history_schema.sql
│   │   ├── seed_sources.sql
│   │   ├── seed_patterns.sql
│   │   └── raw_schema.sql
│   └── README.md
└── terraform/
    ├── environments/
    └── modules/
```

## Local Runtime

From `raksha-core/infra`:

```bash
cp .env.example .env
# set ETH_WS_URL / ETH_WS_URL_BACKUP

docker compose up -d postgres postgres_raw redis
docker compose up --build indexer detector orchestrator finality
```

The compose stack initializes:

- `raksha` DB from `sql/bootstrap/core_schema.sql`, `history_schema.sql`, `seed_sources.sql`, `seed_patterns.sql`
- `raksha_raw` DB from `sql/bootstrap/raw_schema.sql`

## AWS/Terraform

Use environment stacks under `terraform/environments/{test,stage,prod}`.

```bash
cd terraform/environments/test
terraform init
terraform plan -out=tfplan
terraform apply tfplan
```

Service secrets now include both:

- `DATABASE_URL`
- `RAW_DATABASE_URL`

and environment stacks expose `raw_database_url_secret_arn`.

## Notes

- Primary ingestion landing for the rewrite is `raksha_raw.raw_ingest.*` plus `ingest_operational_events` in core DB.
- See `infra/sql/README.md` for bootstrap details.
