CREATE SCHEMA IF NOT EXISTS history;

CREATE TABLE IF NOT EXISTS history.cases (
    case_id            TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL,
    case_type          TEXT NOT NULL CHECK (case_type IN ('exploit', 'market_stress', 'anomaly')),
    classification     TEXT NOT NULL,
    severity_peak      TEXT NOT NULL DEFAULT 'medium',
    status             TEXT NOT NULL DEFAULT 'open',
    chain_slug         TEXT,
    protocol           TEXT,
    title              TEXT NOT NULL,
    summary            TEXT,
    incident_start_at  TIMESTAMPTZ,
    incident_end_at    TIMESTAMPTZ,
    loss_usd_estimate  TEXT,
    source_confidence  DOUBLE PRECISION,
    source_payload     JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_cases_tenant_start
    ON history.cases (tenant_id, incident_start_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_cases_tenant_class_start
    ON history.cases (tenant_id, classification, incident_start_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_cases_chain_protocol_start
    ON history.cases (chain_slug, protocol, incident_start_at DESC);

CREATE TABLE IF NOT EXISTS history.case_events (
    event_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    case_id        TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    tenant_id      TEXT NOT NULL,
    event_type     TEXT NOT NULL,
    event_ts       TIMESTAMPTZ NOT NULL,
    source_table   TEXT NOT NULL,
    source_pk      TEXT NOT NULL,
    payload_json   JSONB NOT NULL DEFAULT '{}'::jsonb,
    ingested_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (source_table, source_pk)
);

CREATE INDEX IF NOT EXISTS idx_history_case_events_case_ts
    ON history.case_events (case_id, event_ts);
CREATE INDEX IF NOT EXISTS idx_history_case_events_tenant_ts
    ON history.case_events (tenant_id, event_ts DESC);

CREATE TABLE IF NOT EXISTS history.case_alert_links (
    id              BIGSERIAL PRIMARY KEY,
    case_id         TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    tenant_id       TEXT NOT NULL,
    alert_id        TEXT NOT NULL,
    incident_id     TEXT,
    pattern_id      TEXT,
    severity        TEXT,
    delivery_status TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (case_id, alert_id)
);

CREATE INDEX IF NOT EXISTS idx_history_case_alert_links_case_created
    ON history.case_alert_links (case_id, created_at);
CREATE INDEX IF NOT EXISTS idx_history_case_alert_links_tenant_pattern_created
    ON history.case_alert_links (tenant_id, pattern_id, created_at DESC);

CREATE TABLE IF NOT EXISTS history.replay_catalog (
    scenario_id                  TEXT PRIMARY KEY,
    tenant_id                    TEXT NOT NULL,
    case_id                      TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    slug                         TEXT NOT NULL,
    title                        TEXT NOT NULL,
    category                     TEXT NOT NULL CHECK (category IN ('market_stress', 'exploit', 'anomaly')),
    tags                         TEXT[] NOT NULL DEFAULT '{}',
    incident_class               TEXT,
    chain                        TEXT,
    protocol                     TEXT,
    protocol_category            TEXT,
    description                  TEXT,
    impact_summary               TEXT,
    losses_usd_estimate          TEXT,
    attack_vector                TEXT,
    detection_focus              TEXT,
    default_time_window_start    TIMESTAMPTZ NOT NULL,
    default_time_window_end      TIMESTAMPTZ NOT NULL,
    default_speed                INTEGER NOT NULL DEFAULT 10,
    supported_patterns           TEXT[] NOT NULL DEFAULT '{}',
    supported_override_keys      TEXT[] NOT NULL DEFAULT '{}',
    baseline_expected_alerts     INTEGER NOT NULL DEFAULT 0,
    expected_alerts_json         JSONB NOT NULL DEFAULT '[]'::jsonb,
    timeline_json                JSONB NOT NULL DEFAULT '[]'::jsonb,
    references_json              JSONB NOT NULL DEFAULT '[]'::jsonb,
    source_feeds_json            JSONB NOT NULL DEFAULT '[]'::jsonb,
    runbook_notes                TEXT[] NOT NULL DEFAULT '{}',
    dataset_version              TEXT NOT NULL DEFAULT 'v1',
    object_prefix                TEXT NOT NULL,
    checksum                     TEXT NOT NULL,
    simlab_scenario_id           TEXT,
    is_active                    BOOLEAN NOT NULL DEFAULT TRUE,
    created_at                   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (tenant_id, slug, dataset_version)
);

CREATE INDEX IF NOT EXISTS idx_history_replay_catalog_tenant_active
    ON history.replay_catalog (tenant_id, is_active, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_replay_catalog_case
    ON history.replay_catalog (case_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS history.ml_feature_registry (
    feature_slice_id      TEXT PRIMARY KEY,
    tenant_id             TEXT NOT NULL,
    case_id               TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    feature_set_version   TEXT NOT NULL,
    label_set_version     TEXT NOT NULL,
    s3_uri                TEXT NOT NULL,
    row_count             BIGINT NOT NULL DEFAULT 0,
    metadata_json         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_ml_feature_registry_case
    ON history.ml_feature_registry (case_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_ml_feature_registry_tenant_feature
    ON history.ml_feature_registry (tenant_id, feature_set_version, created_at DESC);

CREATE TABLE IF NOT EXISTS history.dataset_manifests (
    manifest_id        TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL,
    case_id            TEXT REFERENCES history.cases(case_id) ON DELETE SET NULL,
    dataset_version    TEXT NOT NULL,
    checksum           TEXT NOT NULL,
    source_export_refs JSONB NOT NULL DEFAULT '[]'::jsonb,
    payload            JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_dataset_manifests_tenant_version
    ON history.dataset_manifests (tenant_id, dataset_version, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_dataset_manifests_case
    ON history.dataset_manifests (case_id, created_at DESC);

CREATE TABLE IF NOT EXISTS history.case_data_provenance (
    provenance_id   TEXT PRIMARY KEY,
    case_id         TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    tenant_id       TEXT NOT NULL,
    provider        TEXT NOT NULL,
    query_window    JSONB NOT NULL DEFAULT '{}'::jsonb,
    hash_chain      JSONB NOT NULL DEFAULT '[]'::jsonb,
    source_manifest TEXT,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_case_data_provenance_case
    ON history.case_data_provenance (case_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_case_data_provenance_tenant_provider
    ON history.case_data_provenance (tenant_id, provider, created_at DESC);

CREATE TABLE IF NOT EXISTS history.ingest_offsets (
    source_name   TEXT PRIMARY KEY,
    last_seen_ts  TIMESTAMPTZ NOT NULL DEFAULT to_timestamp(0),
    last_seen_id  TEXT NOT NULL DEFAULT '',
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

