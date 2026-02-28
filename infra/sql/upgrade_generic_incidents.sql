-- One-time upgrade script: generic incident lifecycle persistence.
-- Safe to run multiple times.

ALTER TABLE alerts
  ADD COLUMN IF NOT EXISTS incident_id TEXT;

CREATE INDEX IF NOT EXISTS idx_alerts_incident_id
  ON alerts (incident_id);

ALTER TABLE alert_lifecycle_events
  ADD COLUMN IF NOT EXISTS incident_id TEXT;

CREATE INDEX IF NOT EXISTS idx_alert_lifecycle_incident_id
  ON alert_lifecycle_events (incident_id);

CREATE TABLE IF NOT EXISTS incidents (
    incident_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    pattern_id TEXT NOT NULL,
    subject_type TEXT,
    subject_key TEXT,
    chain_slug TEXT NOT NULL,
    status TEXT NOT NULL,
    current_severity TEXT NOT NULL,
    opened_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_incidents_tenant_pattern_subject
ON incidents (tenant_id, pattern_id, subject_type, subject_key, chain_slug);
CREATE INDEX IF NOT EXISTS idx_incidents_tenant_status_updated
ON incidents (tenant_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_incidents_updated_at
ON incidents (updated_at DESC);

CREATE TABLE IF NOT EXISTS incident_events (
    id BIGSERIAL PRIMARY KEY,
    incident_id TEXT NOT NULL,
    transition_type TEXT NOT NULL,
    from_state TEXT,
    to_state TEXT,
    reason TEXT,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_incident_events_incident_created
ON incident_events (incident_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_incident_events_transition_created
ON incident_events (transition_type, created_at DESC);

CREATE TABLE IF NOT EXISTS incident_context_snapshots (
    id BIGSERIAL PRIMARY KEY,
    incident_id TEXT NOT NULL,
    classification TEXT,
    score DOUBLE PRECISION,
    confidence DOUBLE PRECISION,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_incident_context_incident_observed
ON incident_context_snapshots (incident_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_incident_context_classification_observed
ON incident_context_snapshots (classification, observed_at DESC);
