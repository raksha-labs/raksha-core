-- Market DPEG extension schema
-- Version: 0.3.0

ALTER TABLE detections
    ADD COLUMN IF NOT EXISTS subject_type TEXT;
ALTER TABLE detections
    ADD COLUMN IF NOT EXISTS subject_key TEXT;

ALTER TABLE alerts
    ADD COLUMN IF NOT EXISTS subject_type TEXT;
ALTER TABLE alerts
    ADD COLUMN IF NOT EXISTS subject_key TEXT;

CREATE INDEX IF NOT EXISTS idx_detections_subject_created
    ON detections (subject_type, subject_key, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alerts_subject_created
    ON alerts (subject_type, subject_key, created_at DESC);

CREATE TABLE IF NOT EXISTS market_quote_ticks (
    quote_id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_name TEXT NOT NULL,
    market_key TEXT NOT NULL,
    source_symbol TEXT NOT NULL,
    price DOUBLE PRECISION NOT NULL,
    peg_target DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    observed_at TIMESTAMPTZ NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_market_quote_ticks_tenant_market_observed
    ON market_quote_ticks (tenant_id, market_key, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_market_quote_ticks_source_observed
    ON market_quote_ticks (source_id, observed_at DESC);

CREATE TABLE IF NOT EXISTS market_consensus_snapshots (
    snapshot_id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    market_key TEXT NOT NULL,
    peg_target DOUBLE PRECISION NOT NULL,
    weighted_median_price DOUBLE PRECISION NOT NULL,
    divergence_pct DOUBLE PRECISION NOT NULL,
    source_count INTEGER NOT NULL,
    quorum_met BOOLEAN NOT NULL,
    breach_active BOOLEAN NOT NULL,
    severity TEXT,
    observed_at TIMESTAMPTZ NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_market_consensus_snapshots_tenant_market_observed
    ON market_consensus_snapshots (tenant_id, market_key, observed_at DESC);

CREATE TABLE IF NOT EXISTS dpeg_alert_state (
    tenant_id TEXT NOT NULL,
    market_key TEXT NOT NULL,
    breach_started_at TIMESTAMPTZ,
    cooldown_until TIMESTAMPTZ,
    last_alerted_at TIMESTAMPTZ,
    last_divergence_pct DOUBLE PRECISION,
    last_severity TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, market_key)
);

CREATE TABLE IF NOT EXISTS connector_health_state (
    tenant_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    healthy BOOLEAN NOT NULL,
    last_message_at TIMESTAMPTZ,
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, source_id)
);

-- Suggested retention strategy:
-- 1) Keep raw ticks for 7 days via scheduled DELETE on market_quote_ticks.created_at
-- 2) Keep snapshots for 180 days via scheduled DELETE on market_consensus_snapshots.created_at
