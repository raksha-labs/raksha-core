-- ============================================================================
-- Raksha Raw Ingestion Schema (separate database: raksha_raw)
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE SCHEMA IF NOT EXISTS raw_ingest;

CREATE TABLE IF NOT EXISTS raw_ingest.connectors (
    connector_id    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id       TEXT NOT NULL,
    source_type     TEXT NOT NULL,
    parser_name     TEXT NOT NULL,
    schema_version  TEXT NOT NULL DEFAULT 'v1',
    owner_tenant_id TEXT,
    visibility      TEXT NOT NULL DEFAULT 'platform'
        CHECK (visibility IN ('platform', 'tenant_private')),
    promoted_at     TIMESTAMPTZ,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_raw_connectors_source_parser
    ON raw_ingest.connectors (source_id, parser_name);
CREATE INDEX IF NOT EXISTS idx_raw_connectors_source_visibility_owner
    ON raw_ingest.connectors (source_id, visibility, owner_tenant_id);

CREATE TABLE IF NOT EXISTS raw_ingest.streams (
    stream_id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    connector_id       UUID REFERENCES raw_ingest.connectors(connector_id) ON DELETE CASCADE,
    source_id          TEXT NOT NULL,
    stream_name        TEXT NOT NULL,
    mode               TEXT NOT NULL,
    filter_config      JSONB NOT NULL DEFAULT '{}'::jsonb,
    auth_ref           TEXT,
    poll_interval_ms   INTEGER,
    enabled            BOOLEAN NOT NULL DEFAULT TRUE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_raw_streams_source_name
    ON raw_ingest.streams (source_id, stream_name);

CREATE TABLE IF NOT EXISTS raw_ingest.ingest_offsets (
    stream_id           TEXT NOT NULL,
    partition_key       TEXT NOT NULL,
    last_cursor         TEXT,
    last_block_number   BIGINT,
    last_seen_ts        TIMESTAMPTZ NOT NULL DEFAULT to_timestamp(0),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (stream_id, partition_key)
);

CREATE TABLE IF NOT EXISTS raw_ingest.ingest_batches (
    batch_id         TEXT PRIMARY KEY,
    stream_id        TEXT NOT NULL,
    started_at       TIMESTAMPTZ NOT NULL,
    ended_at         TIMESTAMPTZ NOT NULL,
    rows_ingested    BIGINT NOT NULL DEFAULT 0,
    rows_deduped     BIGINT NOT NULL DEFAULT 0,
    status           TEXT NOT NULL,
    error_summary    TEXT
);

CREATE INDEX IF NOT EXISTS idx_raw_ingest_batches_stream_started
    ON raw_ingest.ingest_batches (stream_id, started_at DESC);

CREATE TABLE IF NOT EXISTS raw_ingest.chain_events (
    event_id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id          TEXT NOT NULL,
    stream_id          TEXT NOT NULL,
    event_type         TEXT NOT NULL,
    event_ts           TIMESTAMPTZ NOT NULL,
    observed_at        TIMESTAMPTZ NOT NULL,
    chain_id           BIGINT,
    block_number       BIGINT,
    block_hash         TEXT,
    tx_hash            TEXT,
    log_index          BIGINT,
    topic0             TEXT,
    topic1             TEXT,
    topic2             TEXT,
    topic3             TEXT,
    event_sig          TEXT,
    contract_address   TEXT,
    decoded_json       JSONB NOT NULL DEFAULT '{}'::jsonb,
    raw_payload        JSONB NOT NULL,
    provider           TEXT,
    request_window     TEXT,
    schema_version     TEXT NOT NULL DEFAULT 'v1',
    idempotency_key    TEXT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_raw_chain_events_idempotency
    ON raw_ingest.chain_events (idempotency_key);
CREATE INDEX IF NOT EXISTS idx_raw_chain_events_source_observed
    ON raw_ingest.chain_events (source_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_chain_events_chain_block
    ON raw_ingest.chain_events (chain_id, block_number DESC);
CREATE INDEX IF NOT EXISTS idx_raw_chain_events_tx_log
    ON raw_ingest.chain_events (tx_hash, log_index);

CREATE TABLE IF NOT EXISTS raw_ingest.dex_events (
    event_id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id          TEXT NOT NULL,
    stream_id          TEXT NOT NULL,
    event_type         TEXT NOT NULL,
    event_ts           TIMESTAMPTZ NOT NULL,
    observed_at        TIMESTAMPTZ NOT NULL,
    market_key         TEXT,
    chain_id           BIGINT,
    tx_hash            TEXT,
    protocol           TEXT,
    pool_address       TEXT,
    decoded_json       JSONB NOT NULL DEFAULT '{}'::jsonb,
    raw_payload        JSONB NOT NULL,
    schema_version     TEXT NOT NULL DEFAULT 'v1',
    idempotency_key    TEXT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_raw_dex_events_idempotency
    ON raw_ingest.dex_events (idempotency_key);
CREATE INDEX IF NOT EXISTS idx_raw_dex_events_source_observed
    ON raw_ingest.dex_events (source_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_dex_events_market_observed
    ON raw_ingest.dex_events (market_key, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_dex_events_tx
    ON raw_ingest.dex_events (tx_hash);

CREATE TABLE IF NOT EXISTS raw_ingest.cex_ticks (
    tick_id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id          TEXT NOT NULL,
    stream_id          TEXT NOT NULL,
    event_type         TEXT NOT NULL,
    event_ts           TIMESTAMPTZ NOT NULL,
    observed_at        TIMESTAMPTZ NOT NULL,
    source_symbol      TEXT,
    market_key         TEXT,
    price_bid          DOUBLE PRECISION,
    price_ask          DOUBLE PRECISION,
    price_last         DOUBLE PRECISION,
    volume             DOUBLE PRECISION,
    sequence           TEXT,
    payload            JSONB NOT NULL,
    schema_version     TEXT NOT NULL DEFAULT 'v1',
    idempotency_key    TEXT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_raw_cex_ticks_idempotency
    ON raw_ingest.cex_ticks (idempotency_key);
CREATE INDEX IF NOT EXISTS idx_raw_cex_ticks_source_observed
    ON raw_ingest.cex_ticks (source_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_cex_ticks_market_observed
    ON raw_ingest.cex_ticks (market_key, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_cex_ticks_event_ts
    ON raw_ingest.cex_ticks (event_ts DESC);

CREATE TABLE IF NOT EXISTS raw_ingest.reorg_compensations (
    compensation_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id            TEXT NOT NULL,
    chain_id             BIGINT,
    orphaned_from_block  BIGINT NOT NULL,
    common_ancestor      BIGINT,
    affected_refs        JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_raw_reorg_compensations_chain_block
    ON raw_ingest.reorg_compensations (chain_id, orphaned_from_block DESC);

CREATE TABLE IF NOT EXISTS raw_ingest.ingest_failures (
    failure_id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id          TEXT,
    source_id          TEXT NOT NULL,
    source_type        TEXT NOT NULL,
    event_type         TEXT,
    payload_excerpt    JSONB NOT NULL DEFAULT '{}'::jsonb,
    error_kind         TEXT NOT NULL,
    error_message      TEXT NOT NULL,
    retryable          BOOLEAN NOT NULL DEFAULT FALSE,
    observed_at        TIMESTAMPTZ NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_raw_ingest_failures_source_created
    ON raw_ingest.ingest_failures (source_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_ingest_failures_stream_created
    ON raw_ingest.ingest_failures (stream_id, created_at DESC);

CREATE TABLE IF NOT EXISTS raw_ingest.export_manifest (
    snapshot_id      TEXT PRIMARY KEY,
    entity           TEXT NOT NULL,
    partition        TEXT NOT NULL,
    row_count        BIGINT NOT NULL DEFAULT 0,
    checksum         TEXT NOT NULL,
    s3_uri           TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_raw_export_manifest_entity_partition
    ON raw_ingest.export_manifest (entity, partition, created_at DESC);
