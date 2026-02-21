-- ============================================================================
-- DeFi Surveillance - Complete Database Schema
-- ============================================================================
-- This schema represents the final state of the database for a fresh deployment.
-- It consolidates all migrations into a single file for clean installations.
-- 
-- Run this file once on a new database to set up all tables and indexes.
-- ============================================================================

-- ─── Core Detection & Alert Tables ──────────────────────────────────────────

CREATE TABLE IF NOT EXISTS detections (
    id TEXT PRIMARY KEY,
    tx_hash TEXT NOT NULL,
    chain TEXT NOT NULL,
    protocol TEXT NOT NULL,
    severity TEXT NOT NULL,
    risk_score DOUBLE PRECISION NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_detections_chain ON detections(chain);
CREATE INDEX IF NOT EXISTS idx_detections_protocol ON detections(protocol);
CREATE INDEX IF NOT EXISTS idx_detections_tx_hash ON detections(tx_hash);
CREATE INDEX IF NOT EXISTS idx_detections_created_at ON detections(created_at);

CREATE TABLE IF NOT EXISTS alerts (
    id TEXT PRIMARY KEY,
    tx_hash TEXT NOT NULL,
    chain TEXT NOT NULL,
    protocol TEXT NOT NULL,
    severity TEXT NOT NULL,
    risk_score DOUBLE PRECISION NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_alerts_chain ON alerts(chain);
CREATE INDEX IF NOT EXISTS idx_alerts_protocol ON alerts(protocol);
CREATE INDEX IF NOT EXISTS idx_alerts_tx_hash ON alerts(tx_hash);
CREATE INDEX IF NOT EXISTS idx_alerts_created_at ON alerts(created_at);

-- ─── Finality State Persistence ─────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS finality_state (
    chain TEXT PRIMARY KEY,
    head_block BIGINT NOT NULL,
    confirmation_depth INT NOT NULL,
    blocks JSONB NOT NULL,  -- Serialized BTreeMap<u64, BlockEntry>
    states JSONB NOT NULL,  -- Serialized HashMap<String, LifecycleState>
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─── Tenant Configuration ───────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant_id TEXT PRIMARY KEY,
    severity_threshold TEXT NOT NULL DEFAULT 'medium',
    cooldown_sec INTEGER NOT NULL DEFAULT 300,
    default_channels TEXT[] NOT NULL DEFAULT '{webhook}',
    protocol_watchlist TEXT[] NOT NULL DEFAULT '{}',
    route_overrides JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─── Alert Lifecycle Tracking ───────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS alert_lifecycle_events (
    id BIGSERIAL PRIMARY KEY,
    alert_id TEXT NOT NULL,
    lifecycle_state TEXT NOT NULL,
    reason TEXT,
    event_key TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_alert_lifecycle_alert_id
    ON alert_lifecycle_events (alert_id, created_at DESC);

-- ─── Dependency Graph ───────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS dependency_edges (
    id BIGSERIAL PRIMARY KEY,
    chain TEXT NOT NULL,
    protocol TEXT NOT NULL,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    relation TEXT NOT NULL,
    weight DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_dependency_edges_protocol
    ON dependency_edges (chain, protocol);

-- ─── Feature Vectors for ML/Analysis ────────────────────────────────────────

CREATE TABLE IF NOT EXISTS feature_vectors (
    id BIGSERIAL PRIMARY KEY,
    detection_id TEXT NOT NULL,
    tenant_id TEXT,
    feature_set_version TEXT NOT NULL,
    values JSONB NOT NULL,
    labels JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_feature_vectors_detection
    ON feature_vectors (detection_id);

-- ─── Data Source Catalog ────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS data_sources (
    source_id         TEXT NOT NULL,
    source_type       TEXT NOT NULL,        -- evmchain | cex_websocket | dex_api | oracle_api | custom_api
    source_name       TEXT NOT NULL,        -- display name / connector name (e.g. "binance", "chainlink")
    connection_config JSONB NOT NULL DEFAULT '{}',
    filters           JSONB,                -- optional per-source filters (e.g. symbols list)
    enabled           BOOLEAN NOT NULL DEFAULT TRUE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (source_id)
);

CREATE TABLE IF NOT EXISTS tenant_data_sources (
    tenant_id       TEXT NOT NULL,
    source_id       TEXT NOT NULL REFERENCES data_sources(source_id) ON DELETE CASCADE,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    override_config JSONB,          -- optional per-tenant connection override
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, source_id)
);

CREATE INDEX IF NOT EXISTS idx_tenant_data_sources_tenant
    ON tenant_data_sources (tenant_id);

-- ─── Pattern Catalog ─────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS patterns (
    pattern_id   TEXT NOT NULL,
    pattern_name TEXT NOT NULL,
    description  TEXT,
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (pattern_id)
);

CREATE TABLE IF NOT EXISTS pattern_configs (
    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    config     JSONB NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (pattern_id)
);

CREATE TABLE IF NOT EXISTS tenant_pattern_configs (
    tenant_id  TEXT NOT NULL,
    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    config     JSONB NOT NULL DEFAULT '{}',   -- tenant-specific policy JSONB
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, pattern_id)
);

CREATE INDEX IF NOT EXISTS idx_tenant_pattern_configs_tenant
    ON tenant_pattern_configs (tenant_id);

-- ─── Raw Event Store ─────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS raw_events (
    event_id     TEXT        NOT NULL,
    tenant_id    TEXT        NOT NULL,
    source_id    TEXT        NOT NULL,
    source_type  TEXT        NOT NULL,
    event_type   TEXT        NOT NULL,
    payload      JSONB       NOT NULL,
    chain_id     BIGINT,
    block_number BIGINT,
    tx_hash      TEXT,
    market_key   TEXT,
    price        DOUBLE PRECISION,
    observed_at  TIMESTAMPTZ NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (event_id)
);

CREATE INDEX IF NOT EXISTS idx_raw_events_tenant_source_observed
    ON raw_events (tenant_id, source_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_events_tenant_event_type_observed
    ON raw_events (tenant_id, event_type, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_raw_events_market_key_observed
    ON raw_events (market_key, observed_at DESC)
    WHERE market_key IS NOT NULL;

-- ─── Pattern State Persistence ───────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS pattern_state (
    tenant_id  TEXT        NOT NULL,
    pattern_id TEXT        NOT NULL,
    state_key  TEXT        NOT NULL,          -- e.g. market_key, attacker_address
    data       JSONB       NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, pattern_id, state_key)
);

CREATE INDEX IF NOT EXISTS idx_pattern_state_tenant_pattern
    ON pattern_state (tenant_id, pattern_id);

-- ─── Pattern Snapshots (Audit Trail) ─────────────────────────────────────────

CREATE TABLE IF NOT EXISTS pattern_snapshots (
    id           BIGSERIAL   PRIMARY KEY,
    tenant_id    TEXT        NOT NULL,
    pattern_id   TEXT        NOT NULL,
    snapshot_key TEXT        NOT NULL,        -- e.g. market_key, tx_hash
    data         JSONB       NOT NULL,
    score        DOUBLE PRECISION,
    severity     TEXT,
    observed_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_pattern_snapshots_tenant_pattern_observed
    ON pattern_snapshots (tenant_id, pattern_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_pattern_snapshots_tenant_key_observed
    ON pattern_snapshots (tenant_id, snapshot_key, observed_at DESC);

-- ─── Data Source Health Monitoring ───────────────────────────────────────────

CREATE TABLE IF NOT EXISTS data_source_health (
    tenant_id       TEXT        NOT NULL,
    source_id       TEXT        NOT NULL,
    healthy         BOOLEAN     NOT NULL,
    last_message_at TIMESTAMPTZ,
    last_error      TEXT,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, source_id)
);

-- ============================================================================
-- Production Optimization Notes
-- ============================================================================
-- For high-volume production workloads, consider:
-- 1. Time-based partitioning for detections and alerts tables
--    ALTER TABLE detections PARTITION BY RANGE (created_at);
-- 2. Separate partitions for each chain
--    ALTER TABLE detections PARTITION BY LIST (chain);
-- 3. Retention policies to archive old data
--    pg_cron or application-level archival jobs
-- 4. Connection pooling (PgBouncer, pg_pool)
-- 5. Read replicas for analytics queries
-- ============================================================================
