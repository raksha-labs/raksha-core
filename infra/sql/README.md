# SQL Bootstrap

This directory contains create-only bootstrap SQL for the ingestion/history rewrite.

## Preferred Files

- `bootstrap/core_schema.sql` - core operational schema
- `bootstrap/history_schema.sql` - history/replay schema
- `bootstrap/seed_sources.sql` - source + stream seed data
- `bootstrap/seed_patterns.sql` - pattern + policy seed data
- `bootstrap/raw_schema.sql` - raw ingestion schema (for `raksha_raw`)

## Fresh Install

```bash
psql -U postgres -c "CREATE DATABASE raksha;"
psql -U postgres -d raksha -f bootstrap/core_schema.sql
psql -U postgres -d raksha -f bootstrap/history_schema.sql
psql -U postgres -d raksha -f bootstrap/seed_sources.sql
psql -U postgres -d raksha -f bootstrap/seed_patterns.sql

psql -U postgres -c "CREATE DATABASE raksha_raw;"
psql -U postgres -d raksha_raw -f bootstrap/raw_schema.sql
```

## Docker Compose

`infra/docker-compose.yml` mounts and applies these files automatically on first container startup.
