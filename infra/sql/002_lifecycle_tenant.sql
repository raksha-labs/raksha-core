-- Lifecycle + multi-tenant + dependency graph extensions
-- Version: 0.2.0

CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant_id TEXT PRIMARY KEY,
    severity_threshold TEXT NOT NULL DEFAULT 'medium',
    cooldown_sec INTEGER NOT NULL DEFAULT 300,
    default_channels TEXT[] NOT NULL DEFAULT '{webhook}',
    protocol_watchlist TEXT[] NOT NULL DEFAULT '{}',
    route_overrides JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

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
