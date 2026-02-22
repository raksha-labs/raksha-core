use common::DataSourceConfig;
use anyhow::Result;
use chrono::{DateTime, Utc};
use event_schema::{AlertEvent, DetectionResult, UnifiedEvent};
use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::{error::SqlState, Client, NoTls};
use tracing::{info, warn};

const DEFAULT_ALERT_FALLBACK_TENANT_ID: &str = "glider";

#[derive(Debug, Clone)]
pub struct EffectiveStreamConfig {
    pub stream_config_id: String,
    pub source_id: String,
    pub source_type: String,
    pub source_name: String,
    pub connection_config: Value,
    pub connector_mode: String,
    pub stream_name: String,
    pub subscription_key: Option<String>,
    pub event_type: String,
    pub parser_name: String,
    pub market_key: Option<String>,
    pub asset_pair: Option<String>,
    pub filter_config: Value,
    pub auth_secret_ref: Option<String>,
    pub auth_config: Value,
    pub payload_ts_path: Option<String>,
    pub payload_ts_unit: String,
}

#[derive(Debug, Clone)]
pub struct StreamTenantTarget {
    pub tenant_id: String,
}

#[derive(Debug, Clone)]
pub struct SourceFeedEventRecord {
    pub stream_config_id: Option<String>,
    pub source_id: String,
    pub source_type: String,
    pub event_type: String,
    pub event_id: Option<String>,
    pub market_key: Option<String>,
    pub asset_pair: Option<String>,
    pub chain_id: Option<i64>,
    pub block_number: Option<i64>,
    pub tx_hash: Option<String>,
    pub log_index: Option<i64>,
    pub topic0: Option<String>,
    pub price: Option<f64>,
    pub payload_event_ts: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub parse_status: String,
    pub parse_error: Option<String>,
    pub payload: Value,
    pub normalized_fields: Value,
    pub dedup_key: Option<String>,
}

#[derive(Clone)]
pub struct PostgresRepository {
    client: Arc<Client>,
}

impl PostgresRepository {
    pub async fn from_database_url(database_url: &str) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;

        tokio::spawn(async move {
            if let Err(err) = connection.await {
                tracing::error!(error = ?err, "postgres background connection error");
            }
        });

        let repo = Self {
            client: Arc::new(client),
        };
        repo.init_schema().await?;
        Ok(repo)
    }

    pub fn from_env() -> Option<String> {
        std::env::var("DATABASE_URL").ok()
    }

    async fn init_schema(&self) -> Result<()> {
        self.client
            .batch_execute(
                r#"
                CREATE EXTENSION IF NOT EXISTS pgcrypto;

                CREATE TABLE IF NOT EXISTS detections (
                    id TEXT PRIMARY KEY,
                    tx_hash TEXT NOT NULL,
                    chain TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    subject_type TEXT,
                    subject_key TEXT,
                    severity TEXT NOT NULL,
                    risk_score DOUBLE PRECISION NOT NULL,
                    payload JSONB NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                CREATE TABLE IF NOT EXISTS alerts (
                    id TEXT PRIMARY KEY,
                    tx_hash TEXT NOT NULL,
                    chain TEXT NOT NULL,
                    chain_slug TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    block_number BIGINT,
                    subject_type TEXT,
                    subject_key TEXT,
                    lifecycle_state TEXT NOT NULL,
                    severity TEXT NOT NULL,
                    risk_score DOUBLE PRECISION NOT NULL,
                    payload JSONB NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                ALTER TABLE detections
                    ADD COLUMN IF NOT EXISTS subject_type TEXT;
                ALTER TABLE detections
                    ADD COLUMN IF NOT EXISTS subject_key TEXT;
                ALTER TABLE detections
                    ADD COLUMN IF NOT EXISTS payload JSONB;
                ALTER TABLE detections
                    ADD COLUMN IF NOT EXISTS tenant_id TEXT;
                ALTER TABLE detections
                    ADD COLUMN IF NOT EXISTS pattern_id TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS subject_type TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS subject_key TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS payload JSONB;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS lifecycle_state TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS tenant_id TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS pattern_id TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS chain_slug TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS block_number BIGINT;

                UPDATE detections
                SET payload = '{}'::jsonb
                WHERE payload IS NULL;
                ALTER TABLE detections
                    ALTER COLUMN payload SET NOT NULL;

                UPDATE alerts
                SET payload = '{}'::jsonb
                WHERE payload IS NULL;
                ALTER TABLE alerts
                    ALTER COLUMN payload SET NOT NULL;

                UPDATE detections
                SET tenant_id = COALESCE(NULLIF(payload->>'tenant_id', ''), 'glider')
                WHERE tenant_id IS NULL;

                UPDATE alerts
                SET tenant_id = COALESCE(NULLIF(payload->>'tenant_id', ''), 'glider')
                WHERE tenant_id IS NULL;

                UPDATE alerts
                SET lifecycle_state = COALESCE(NULLIF(payload->>'lifecycle_state', ''), 'confirmed')
                WHERE lifecycle_state IS NULL;

                UPDATE detections
                SET pattern_id = COALESCE(NULLIF(payload->>'pattern_id', ''), pattern_id)
                WHERE pattern_id IS NULL;

                UPDATE alerts
                SET pattern_id = COALESCE(NULLIF(payload->>'pattern_id', ''), pattern_id)
                WHERE pattern_id IS NULL;

                UPDATE alerts
                SET chain_slug = COALESCE(
                    NULLIF(payload->>'chain_slug', ''),
                    NULLIF(LOWER(chain), ''),
                    'unknown'
                )
                WHERE chain_slug IS NULL OR chain_slug = '';
                UPDATE alerts
                SET block_number = CASE
                    WHEN payload->>'block_number' ~ '^-?[0-9]+$' THEN (payload->>'block_number')::bigint
                    ELSE NULL
                END
                WHERE block_number IS NULL;

                ALTER TABLE alerts
                    ALTER COLUMN lifecycle_state SET NOT NULL;
                ALTER TABLE alerts
                    ALTER COLUMN chain_slug SET NOT NULL;

                CREATE INDEX IF NOT EXISTS idx_detections_subject_created
                    ON detections (subject_type, subject_key, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_subject_created
                    ON alerts (subject_type, subject_key, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_detections_tenant_created
                    ON detections (tenant_id, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_detections_tenant_pattern_created
                    ON detections (tenant_id, pattern_id, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_tenant_created
                    ON alerts (tenant_id, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_tenant_pattern_created
                    ON alerts (tenant_id, pattern_id, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_tenant_lifecycle
                    ON alerts (tenant_id, lifecycle_state, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_tenant_severity
                    ON alerts (tenant_id, severity, created_at DESC);

                CREATE TABLE IF NOT EXISTS alert_lifecycle_events (
                    id BIGSERIAL PRIMARY KEY,
                    alert_id TEXT NOT NULL,
                    event_key TEXT,
                    tx_hash TEXT NOT NULL,
                    block_number BIGINT NOT NULL,
                    lifecycle_state TEXT NOT NULL,
                    payload JSONB NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                ALTER TABLE alert_lifecycle_events
                    ADD COLUMN IF NOT EXISTS lifecycle_state TEXT;
                ALTER TABLE alert_lifecycle_events
                    ADD COLUMN IF NOT EXISTS payload JSONB;
                ALTER TABLE alert_lifecycle_events
                    ADD COLUMN IF NOT EXISTS tx_hash TEXT;
                ALTER TABLE alert_lifecycle_events
                    ADD COLUMN IF NOT EXISTS block_number BIGINT;
                UPDATE alert_lifecycle_events
                SET payload = '{}'::jsonb
                WHERE payload IS NULL;
                ALTER TABLE alert_lifecycle_events
                    ALTER COLUMN payload SET NOT NULL;
                UPDATE alert_lifecycle_events
                SET lifecycle_state = COALESCE(NULLIF(payload->>'lifecycle_state', ''), 'confirmed')
                WHERE lifecycle_state IS NULL;
                UPDATE alert_lifecycle_events
                SET tx_hash = COALESCE(NULLIF(payload->>'tx_hash', ''), '')
                WHERE tx_hash IS NULL;
                UPDATE alert_lifecycle_events
                SET block_number = CASE
                    WHEN payload->>'block_number' ~ '^-?[0-9]+$' THEN (payload->>'block_number')::bigint
                    ELSE 0
                END
                WHERE block_number IS NULL;
                ALTER TABLE alert_lifecycle_events
                    ALTER COLUMN lifecycle_state SET NOT NULL;
                ALTER TABLE alert_lifecycle_events
                    ALTER COLUMN tx_hash SET NOT NULL;
                ALTER TABLE alert_lifecycle_events
                    ALTER COLUMN block_number SET NOT NULL;

                CREATE INDEX IF NOT EXISTS idx_alert_lifecycle_events_event_key
                    ON alert_lifecycle_events (event_key);

                CREATE TABLE IF NOT EXISTS alert_delivery_attempts (
                    id BIGSERIAL PRIMARY KEY,
                    alert_id TEXT NOT NULL,
                    tenant_id TEXT NOT NULL,
                    channel TEXT NOT NULL,
                    delivered BOOLEAN NOT NULL,
                    reason TEXT,
                    status_code INTEGER,
                    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                CREATE INDEX IF NOT EXISTS idx_alert_delivery_attempts_alert
                    ON alert_delivery_attempts (alert_id, attempted_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alert_delivery_attempts_tenant
                    ON alert_delivery_attempts (tenant_id, attempted_at DESC);

                CREATE TABLE IF NOT EXISTS usage_events (
                    id BIGSERIAL PRIMARY KEY,
                    tenant_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    alert_type TEXT NOT NULL,
                    chain_id BIGINT,
                    quantity INTEGER NOT NULL DEFAULT 1,
                    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                CREATE INDEX IF NOT EXISTS idx_usage_events_tenant_recorded
                    ON usage_events (tenant_id, recorded_at DESC);

                CREATE TABLE IF NOT EXISTS data_sources (
                    source_id TEXT NOT NULL,
                    source_type TEXT NOT NULL,
                    source_name TEXT NOT NULL,
                    connection_config JSONB NOT NULL DEFAULT '{}',
                    filters JSONB,
                    scope TEXT NOT NULL DEFAULT 'global',
                    owner_tenant_id TEXT,
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (source_id)
                );

                ALTER TABLE data_sources
                    ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT 'global';
                ALTER TABLE data_sources
                    ADD COLUMN IF NOT EXISTS owner_tenant_id TEXT;

                CREATE TABLE IF NOT EXISTS tenant_data_sources (
                    tenant_id TEXT NOT NULL,
                    source_id TEXT NOT NULL REFERENCES data_sources(source_id),
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    override_config JSONB,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (tenant_id, source_id)
                );

                CREATE TABLE IF NOT EXISTS source_stream_configs (
                    stream_config_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                    source_id TEXT NOT NULL REFERENCES data_sources(source_id) ON DELETE CASCADE,
                    connector_mode TEXT NOT NULL CHECK (connector_mode IN ('websocket', 'rpc_logs', 'http_poll')),
                    stream_name TEXT NOT NULL,
                    subscription_key TEXT,
                    event_type TEXT NOT NULL,
                    parser_name TEXT NOT NULL,
                    market_key TEXT,
                    asset_pair TEXT,
                    filter_config JSONB NOT NULL DEFAULT '{}'::jsonb,
                    auth_secret_ref TEXT,
                    auth_config JSONB NOT NULL DEFAULT '{}'::jsonb,
                    payload_ts_path TEXT,
                    payload_ts_unit TEXT NOT NULL DEFAULT 'ms' CHECK (payload_ts_unit IN ('ms', 's', 'iso8601')),
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    created_by TEXT NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_by TEXT,
                    updated_at TIMESTAMPTZ
                );

                CREATE INDEX IF NOT EXISTS idx_source_stream_configs_source_enabled
                    ON source_stream_configs (source_id, enabled);
                CREATE INDEX IF NOT EXISTS idx_source_stream_configs_event_type
                    ON source_stream_configs (event_type);
                CREATE UNIQUE INDEX IF NOT EXISTS uq_source_stream_configs_natural
                    ON source_stream_configs (source_id, stream_name, COALESCE(asset_pair, ''), COALESCE(subscription_key, ''));

                CREATE TABLE IF NOT EXISTS source_stream_tenant_targets (
                    stream_config_id UUID NOT NULL REFERENCES source_stream_configs(stream_config_id) ON DELETE CASCADE,
                    tenant_id TEXT NOT NULL,
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    created_by TEXT NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_by TEXT,
                    updated_at TIMESTAMPTZ,
                    PRIMARY KEY (stream_config_id, tenant_id)
                );

                CREATE INDEX IF NOT EXISTS idx_source_stream_tenant_targets_tenant_enabled
                    ON source_stream_tenant_targets (tenant_id, enabled);

                CREATE TABLE IF NOT EXISTS patterns (
                    pattern_id TEXT NOT NULL,
                    pattern_name TEXT NOT NULL,
                    description TEXT,
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (pattern_id)
                );

                CREATE TABLE IF NOT EXISTS pattern_configs (
                    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id),
                    config JSONB NOT NULL DEFAULT '{}',
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (pattern_id)
                );

                CREATE TABLE IF NOT EXISTS tenant_pattern_configs (
                    tenant_id TEXT NOT NULL,
                    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id),
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    config JSONB NOT NULL DEFAULT '{}',
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (tenant_id, pattern_id)
                );

                CREATE TABLE IF NOT EXISTS tenant_pattern_source_bindings (
                    tenant_id TEXT NOT NULL,
                    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
                    source_id TEXT NOT NULL,
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    binding_config JSONB NOT NULL DEFAULT '{}'::jsonb,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_at TIMESTAMPTZ,
                    PRIMARY KEY (tenant_id, pattern_id, source_id),
                    FOREIGN KEY (tenant_id, source_id)
                        REFERENCES tenant_data_sources(tenant_id, source_id)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS idx_tenant_pattern_source_bindings_tenant_pattern
                    ON tenant_pattern_source_bindings (tenant_id, pattern_id);

                CREATE TABLE IF NOT EXISTS tenant_pattern_required_assets (
                    tenant_id TEXT NOT NULL,
                    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
                    market_key TEXT NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_at TIMESTAMPTZ,
                    PRIMARY KEY (tenant_id, pattern_id, market_key)
                );

                CREATE INDEX IF NOT EXISTS idx_tenant_pattern_required_assets_tenant_pattern
                    ON tenant_pattern_required_assets (tenant_id, pattern_id);

                CREATE TABLE IF NOT EXISTS source_required_pairs (
                    source_id TEXT NOT NULL,
                    market_key TEXT NOT NULL,
                    source_symbol TEXT NOT NULL,
                    required_tenant_count INTEGER NOT NULL DEFAULT 0,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (source_id, market_key, source_symbol)
                );

                CREATE INDEX IF NOT EXISTS idx_source_required_pairs_source_market
                    ON source_required_pairs (source_id, market_key);

                CREATE TABLE IF NOT EXISTS tenant_pattern_alert_policies (
                    tenant_id TEXT NOT NULL,
                    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
                    severity_threshold TEXT NOT NULL DEFAULT 'medium',
                    cooldown_sec INTEGER NOT NULL DEFAULT 300,
                    default_channels TEXT[] NOT NULL DEFAULT '{webhook}',
                    route_overrides JSONB NOT NULL DEFAULT '{}'::jsonb,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_at TIMESTAMPTZ,
                    PRIMARY KEY (tenant_id, pattern_id)
                );

                CREATE TABLE IF NOT EXISTS tenant_pattern_notification_channels (
                    tenant_id TEXT NOT NULL,
                    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
                    channel TEXT NOT NULL,
                    enabled BOOLEAN NOT NULL DEFAULT FALSE,
                    config_json JSONB NOT NULL DEFAULT '{}'::jsonb,
                    use_tenant_default BOOLEAN NOT NULL DEFAULT TRUE,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    updated_at TIMESTAMPTZ,
                    PRIMARY KEY (tenant_id, pattern_id, channel),
                    CHECK (channel IN ('webhook', 'slack', 'telegram', 'discord'))
                );

                CREATE TABLE IF NOT EXISTS source_feed_events (
                    raw_event_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                    stream_config_id UUID,
                    source_id TEXT NOT NULL,
                    source_type TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    event_id TEXT,
                    market_key TEXT,
                    asset_pair TEXT,
                    chain_id BIGINT,
                    block_number BIGINT,
                    tx_hash TEXT,
                    log_index BIGINT,
                    topic0 TEXT,
                    price DOUBLE PRECISION,
                    payload_event_ts TIMESTAMPTZ,
                    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    observed_at TIMESTAMPTZ NOT NULL,
                    parse_status TEXT NOT NULL CHECK (parse_status IN ('parsed', 'partial', 'raw_only', 'error')),
                    parse_error TEXT,
                    payload JSONB NOT NULL,
                    normalized_fields JSONB NOT NULL DEFAULT '{}'::jsonb,
                    dedup_key TEXT,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                CREATE INDEX IF NOT EXISTS idx_source_feed_events_source_observed
                    ON source_feed_events (source_id, observed_at DESC);
                CREATE INDEX IF NOT EXISTS idx_source_feed_events_source_market_observed
                    ON source_feed_events (source_id, market_key, observed_at DESC);
                CREATE INDEX IF NOT EXISTS idx_source_feed_events_stream_observed
                    ON source_feed_events (stream_config_id, observed_at DESC);
                CREATE INDEX IF NOT EXISTS idx_source_feed_events_event_type_observed
                    ON source_feed_events (event_type, observed_at DESC);
                CREATE INDEX IF NOT EXISTS idx_source_feed_events_market_observed
                    ON source_feed_events (market_key, observed_at DESC)
                    WHERE market_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_source_feed_events_tx
                    ON source_feed_events (tx_hash, log_index)
                    WHERE tx_hash IS NOT NULL;
                CREATE UNIQUE INDEX IF NOT EXISTS uq_source_feed_events_event_id
                    ON source_feed_events (event_id)
                    WHERE event_id IS NOT NULL;
                CREATE UNIQUE INDEX IF NOT EXISTS uq_source_feed_events_dedup
                    ON source_feed_events (dedup_key)
                    WHERE dedup_key IS NOT NULL;

                CREATE TABLE IF NOT EXISTS pattern_state (
                    tenant_id TEXT NOT NULL,
                    pattern_id TEXT NOT NULL,
                    state_key TEXT NOT NULL,
                    data JSONB NOT NULL DEFAULT '{}',
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (tenant_id, pattern_id, state_key)
                );

                CREATE TABLE IF NOT EXISTS pattern_snapshots (
                    id BIGSERIAL PRIMARY KEY,
                    tenant_id TEXT NOT NULL,
                    pattern_id TEXT NOT NULL,
                    snapshot_key TEXT NOT NULL,
                    data JSONB NOT NULL,
                    score DOUBLE PRECISION,
                    severity TEXT,
                    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                CREATE INDEX IF NOT EXISTS idx_pattern_snapshots_tenant_pattern_observed
                    ON pattern_snapshots (tenant_id, pattern_id, observed_at DESC);

                CREATE TABLE IF NOT EXISTS data_source_health (
                    tenant_id TEXT NOT NULL,
                    source_id TEXT NOT NULL,
                    healthy BOOLEAN NOT NULL,
                    last_message_at TIMESTAMPTZ,
                    last_error TEXT,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (tenant_id, source_id)
                );
                "#,
            )
            .await?;

        info!("postgres schema initialized");
        Ok(())
    }

    pub async fn save_detection(&self, detection: &DetectionResult) -> Result<()> {
        let payload = serde_json::to_value(detection)?;
        let tenant_id = resolve_tenant_id(detection.tenant_id.as_deref());
        self.client
            .execute(
                r#"
                INSERT INTO detections (id, tx_hash, chain, protocol, subject_type, subject_key, tenant_id, pattern_id, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (id) DO NOTHING
                "#,
                &[
                    &detection.detection_id.to_string(),
                    &detection.tx_hash,
                    &format!("{:?}", detection.chain).to_lowercase(),
                    &detection.protocol,
                    &detection.subject_type,
                    &detection.subject_key,
                    &tenant_id,
                    &detection.pattern_id,
                    &format!("{:?}", detection.severity).to_lowercase(),
                    &detection.risk_score.score,
                    &payload,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn save_alert(&self, alert: &AlertEvent) -> Result<()> {
        let payload = serde_json::to_value(alert)?;
        let tenant_id = resolve_tenant_id(alert.tenant_id.as_deref());
        self.client
            .execute(
                r#"
                INSERT INTO alerts (id, tx_hash, chain, chain_slug, protocol, block_number, subject_type, subject_key, tenant_id, pattern_id, lifecycle_state, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                ON CONFLICT (id) DO UPDATE
                SET lifecycle_state = EXCLUDED.lifecycle_state,
                    severity = EXCLUDED.severity,
                    risk_score = EXCLUDED.risk_score,
                    block_number = EXCLUDED.block_number,
                    subject_type = EXCLUDED.subject_type,
                    subject_key = EXCLUDED.subject_key,
                    tenant_id = EXCLUDED.tenant_id,
                    pattern_id = EXCLUDED.pattern_id,
                    payload = EXCLUDED.payload
                "#,
                &[
                    &alert.alert_id.to_string(),
                    &alert.tx_hash,
                    &format!("{:?}", alert.chain).to_lowercase(),
                    &alert.chain_slug,
                    &alert.protocol,
                    &(alert.block_number as i64),
                    &alert.subject_type,
                    &alert.subject_key,
                    &tenant_id,
                    &alert.pattern_id,
                    &format!("{:?}", alert.lifecycle_state).to_lowercase(),
                    &format!("{:?}", alert.severity).to_lowercase(),
                    &alert.risk_score,
                    &payload,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn update_source_health(
        &self,
        tenant_id: &str,
        source_id: &str,
        healthy: bool,
        error: Option<String>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO data_source_health (
                    tenant_id, source_id, healthy, last_error, updated_at
                )
                VALUES ($1, $2, $3, $4, NOW())
                ON CONFLICT (tenant_id, source_id) DO UPDATE
                SET healthy = EXCLUDED.healthy,
                    last_error = EXCLUDED.last_error,
                    updated_at = NOW()
                "#,
                &[&tenant_id, &source_id, &healthy, &error],
            )
            .await?;
        Ok(())
    }

    pub async fn load_tenant_data_sources(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<DataSourceConfig>>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT tds.tenant_id, ds.source_id, ds.source_type, ds.source_name,
                       COALESCE(tds.override_config, ds.connection_config) AS connection_config,
                       ds.filters, ds.enabled AND tds.enabled AS enabled
                FROM tenant_data_sources tds
                JOIN data_sources ds ON ds.source_id = tds.source_id
                WHERE ds.enabled = TRUE AND tds.enabled = TRUE
                ORDER BY tds.tenant_id, ds.source_id
                "#,
                &[],
            )
            .await?;

        let mut map: std::collections::HashMap<String, Vec<DataSourceConfig>> =
            std::collections::HashMap::new();
        for row in rows {
            let tenant_id: String = row.get(0);
            let cfg = DataSourceConfig {
                tenant_id: tenant_id.clone(),
                source_id: row.get(1),
                source_type: row.get(2),
                source_name: row.get(3),
                connection_config: row.get(4),
                filters: row.get(5),
                enabled: row.get(6),
            };
            map.entry(tenant_id).or_default().push(cfg);
        }
        Ok(map)
    }

    pub async fn load_tenant_pattern_configs(
        &self,
    ) -> Result<std::collections::HashMap<(String, String), serde_json::Value>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT tpc.tenant_id, tpc.pattern_id, tpc.config
                FROM tenant_pattern_configs tpc
                JOIN patterns p ON p.pattern_id = tpc.pattern_id
                WHERE p.enabled = TRUE AND tpc.enabled = TRUE
                ORDER BY tpc.tenant_id, tpc.pattern_id
                "#,
                &[],
            )
            .await?;

        let mut map = std::collections::HashMap::new();
        for row in rows {
            let tenant_id: String = row.get(0);
            let pattern_id: String = row.get(1);
            let config: serde_json::Value = row.get(2);
            map.insert((tenant_id, pattern_id), config);
        }
        Ok(map)
    }

    pub async fn list_effective_stream_configs(&self) -> Result<Vec<EffectiveStreamConfig>> {
        let rows = match self
            .client
            .query(
                r#"
                SELECT
                    ssc.stream_config_id::text,
                    ssc.source_id,
                    ds.source_type,
                    ds.source_name,
                    ds.connection_config,
                    ssc.connector_mode,
                    ssc.stream_name,
                    ssc.subscription_key,
                    ssc.event_type,
                    ssc.parser_name,
                    ssc.market_key,
                    ssc.asset_pair,
                    ssc.filter_config,
                    ssc.auth_secret_ref,
                    ssc.auth_config,
                    ssc.payload_ts_path,
                    ssc.payload_ts_unit
                FROM source_stream_configs ssc
                JOIN data_sources ds
                  ON ds.source_id = ssc.source_id
                WHERE ds.enabled = TRUE
                  AND ssc.enabled = TRUE
                ORDER BY ssc.source_id, ssc.stream_name, ssc.asset_pair NULLS FIRST
                "#,
                &[],
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!("stream config tables not found while listing effective stream configs");
                    return Ok(Vec::new());
                }
                return Err(error.into());
            }
        };

        let mut configs = Vec::with_capacity(rows.len());
        for row in rows {
            configs.push(EffectiveStreamConfig {
                stream_config_id: row.get(0),
                source_id: row.get(1),
                source_type: row.get(2),
                source_name: row.get(3),
                connection_config: row.get(4),
                connector_mode: row.get(5),
                stream_name: row.get(6),
                subscription_key: row.get(7),
                event_type: row.get(8),
                parser_name: row.get(9),
                market_key: row.get(10),
                asset_pair: row.get(11),
                filter_config: row.get(12),
                auth_secret_ref: row.get(13),
                auth_config: row.get(14),
                payload_ts_path: row.get(15),
                payload_ts_unit: row.get(16),
            });
        }
        Ok(configs)
    }

    pub async fn list_stream_tenant_targets(
        &self,
        stream_config_id: &str,
    ) -> Result<Vec<StreamTenantTarget>> {
        let rows = match self
            .client
            .query(
                r#"
                SELECT tenant_id
                FROM source_stream_tenant_targets
                WHERE stream_config_id::text = $1
                  AND enabled = TRUE
                ORDER BY tenant_id
                "#,
                &[&stream_config_id],
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!("source_stream_tenant_targets table not found while listing targets");
                    return Ok(Vec::new());
                }
                return Err(error.into());
            }
        };

        let mut targets = Vec::with_capacity(rows.len());
        for row in rows {
            targets.push(StreamTenantTarget {
                tenant_id: row.get(0),
            });
        }
        Ok(targets)
    }

    pub async fn insert_source_feed_event_record(
        &self,
        record: &SourceFeedEventRecord,
    ) -> Result<bool> {
        let inserted = self
            .client
            .execute(
                r#"
                INSERT INTO source_feed_events (
                    stream_config_id,
                    source_id,
                    source_type,
                    event_type,
                    event_id,
                    market_key,
                    asset_pair,
                    chain_id,
                    block_number,
                    tx_hash,
                    log_index,
                    topic0,
                    price,
                    payload_event_ts,
                    observed_at,
                    parse_status,
                    parse_error,
                    payload,
                    normalized_fields,
                    dedup_key
                )
                VALUES (
                    ($1)::text::uuid,
                    $2,
                    $3,
                    $4,
                    $5,
                    $6,
                    $7,
                    $8,
                    $9,
                    $10,
                    $11,
                    $12,
                    $13,
                    $14,
                    $15,
                    $16,
                    $17,
                    $18,
                    $19,
                    $20
                )
                ON CONFLICT DO NOTHING
                "#,
                &[
                    &record.stream_config_id.as_deref(),
                    &record.source_id,
                    &record.source_type,
                    &record.event_type,
                    &record.event_id,
                    &record.market_key,
                    &record.asset_pair,
                    &record.chain_id,
                    &record.block_number,
                    &record.tx_hash,
                    &record.log_index,
                    &record.topic0,
                    &record.price,
                    &record.payload_event_ts,
                    &record.observed_at,
                    &record.parse_status,
                    &record.parse_error,
                    &record.payload,
                    &record.normalized_fields,
                    &record.dedup_key,
                ],
            )
            .await?;

        Ok(inserted > 0)
    }

    pub async fn purge_old_tick_events(&self, retention_seconds: i64) -> Result<u64> {
        let seconds = retention_seconds.max(0);
        let event_types = vec!["quote", "trade"];
        let deleted = self
            .client
            .execute(
                r#"
                DELETE FROM source_feed_events
                WHERE event_type = ANY($1)
                  AND observed_at < NOW() - ($2::BIGINT * INTERVAL '1 second')
                "#,
                &[&event_types, &seconds],
            )
            .await?;
        Ok(deleted)
    }

    pub async fn latest_market_price(
        &self,
        market_key: &str,
        max_age_seconds: i64,
    ) -> Result<Option<f64>> {
        let freshness_seconds = max_age_seconds.max(1);
        let row = match self
            .client
            .query_opt(
                r#"
                SELECT price
                FROM source_feed_events
                WHERE market_key = $1
                  AND price IS NOT NULL
                  AND parse_status IN ('parsed', 'partial')
                  AND observed_at >= NOW() - ($2::BIGINT * INTERVAL '1 second')
                ORDER BY observed_at DESC
                LIMIT 1
                "#,
                &[&market_key, &freshness_seconds],
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!("source_feed_events table not found while reading market FX rate");
                    return Ok(None);
                }
                return Err(error.into());
            }
        };

        Ok(row.map(|record| record.get::<usize, f64>(0)))
    }

    pub async fn insert_raw_event(&self, event: &UnifiedEvent) -> Result<()> {
        self.insert_source_feed_event(event).await
    }

    pub async fn insert_source_feed_event(&self, event: &UnifiedEvent) -> Result<()> {
        if !self.is_source_stream_ingest_enabled(&event.source_id).await? {
            return Ok(());
        }
        if let Some(market_key) = event.market_key.as_deref() {
            if !market_key.trim().is_empty()
                && !self
                    .is_required_source_market_pair(&event.source_id, market_key)
                    .await?
            {
                return Ok(());
            }
        }

        let payload = serde_json::to_value(event)?;
        let asset_pair = event
            .payload
            .get("s")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let topic0 = event
            .payload
            .get("topics")
            .and_then(|value| value.as_array())
            .and_then(|topics| topics.first())
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let log_index = parse_json_i64(event.payload.get("logIndex"));
        let normalized_fields = serde_json::json!({
            "market_key": event.market_key,
            "price": event.price,
            "asset_pair": asset_pair,
            "topic0": topic0,
            "log_index": log_index,
        });

        self.client
            .execute(
                r#"
                INSERT INTO source_feed_events (
                    stream_config_id,
                    source_id,
                    source_type,
                    event_type,
                    event_id,
                    market_key,
                    asset_pair,
                    chain_id,
                    block_number,
                    tx_hash,
                    log_index,
                    topic0,
                    price,
                    payload_event_ts,
                    observed_at,
                    parse_status,
                    parse_error,
                    payload,
                    normalized_fields,
                    dedup_key
                )
                VALUES (
                    NULL,
                    $1,
                    $2,
                    $3,
                    $4,
                    $5,
                    $6,
                    $7,
                    $8,
                    $9,
                    $10,
                    $11,
                    $12,
                    $13,
                    $13,
                    'parsed',
                    NULL,
                    $14,
                    $15,
                    $16
                )
                ON CONFLICT DO NOTHING
                "#,
                &[
                    &event.source_id,
                    &format!("{:?}", event.source_type).to_lowercase(),
                    &event.event_type,
                    &event.event_id,
                    &event.market_key,
                    &asset_pair,
                    &event.chain_id,
                    &event.block_number,
                    &event.tx_hash,
                    &log_index,
                    &topic0,
                    &event.price,
                    &event.timestamp,
                    &payload,
                    &normalized_fields,
                    &event.event_id,
                ],
            )
            .await?;
        Ok(())
    }

    async fn is_required_source_market_pair(&self, source_id: &str, market_key: &str) -> Result<bool> {
        let row = match self
            .client
            .query_opt(
                r#"
                SELECT 1
                FROM source_required_pairs
                WHERE source_id = $1
                  AND market_key = $2
                LIMIT 1
                "#,
                &[&source_id, &market_key],
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!("source_required_pairs table not found; skipping market-key filtering");
                    return Ok(true);
                }
                return Err(error.into());
            }
        };

        Ok(row.is_some())
    }

    async fn is_source_stream_ingest_enabled(&self, source_id: &str) -> Result<bool> {
        let row = match self
            .client
            .query_opt(
                r#"
                SELECT 1
                FROM data_sources ds
                JOIN source_stream_configs ssc
                  ON ssc.source_id = ds.source_id
                 AND ssc.enabled = TRUE
                JOIN source_stream_tenant_targets stt
                  ON stt.stream_config_id = ssc.stream_config_id
                 AND stt.enabled = TRUE
                WHERE ds.source_id = $1
                  AND ds.enabled = TRUE
                LIMIT 1
                "#,
                &[&source_id],
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!(
                        "source stream config tables not found; skipping source-level activation gate"
                    );
                    return Ok(true);
                }
                return Err(error.into());
            }
        };

        Ok(row.is_some())
    }

    pub async fn load_pattern_state(
        &self,
        tenant_id: &str,
        pattern_id: &str,
        state_key: &str,
    ) -> Result<Option<serde_json::Value>> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT data FROM pattern_state
                WHERE tenant_id = $1 AND pattern_id = $2 AND state_key = $3
                "#,
                &[&tenant_id, &pattern_id, &state_key],
            )
            .await?;

        Ok(row.map(|r| r.get(0)))
    }

    pub async fn upsert_pattern_state(
        &self,
        tenant_id: &str,
        pattern_id: &str,
        state_key: &str,
        data: serde_json::Value,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO pattern_state (tenant_id, pattern_id, state_key, data, updated_at)
                VALUES ($1, $2, $3, $4, NOW())
                ON CONFLICT (tenant_id, pattern_id, state_key) DO UPDATE
                SET data = EXCLUDED.data, updated_at = NOW()
                "#,
                &[&tenant_id, &pattern_id, &state_key, &data],
            )
            .await?;
        Ok(())
    }

    pub async fn insert_pattern_snapshot(
        &self,
        tenant_id: &str,
        pattern_id: &str,
        snapshot_key: &str,
        data: serde_json::Value,
        score: Option<f64>,
        severity: Option<&str>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO pattern_snapshots
                    (tenant_id, pattern_id, snapshot_key, data, score, severity)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
                &[
                    &tenant_id,
                    &pattern_id,
                    &snapshot_key,
                    &data,
                    &score,
                    &severity,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn save_alert_lifecycle(&self, alert: &AlertEvent) -> Result<()> {
        let payload = serde_json::to_value(alert)?;
        self.client
            .execute(
                r#"
                INSERT INTO alert_lifecycle_events (alert_id, event_key, tx_hash, block_number, lifecycle_state, payload)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
                &[
                    &alert.alert_id.to_string(),
                    &alert.event_key,
                    &alert.tx_hash,
                    &(alert.block_number as i64),
                    &format!("{:?}", alert.lifecycle_state).to_lowercase(),
                    &payload,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn save_alert_delivery_attempt(
        &self,
        alert_id: &str,
        tenant_id: &str,
        channel: &str,
        delivered: bool,
        reason: Option<&str>,
        status_code: Option<u16>,
    ) -> Result<()> {
        let status_code_i32 = status_code.map(i32::from);
        self.client
            .execute(
                r#"
                INSERT INTO alert_delivery_attempts
                    (alert_id, tenant_id, channel, delivered, reason, status_code)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
                &[
                    &alert_id,
                    &tenant_id,
                    &channel,
                    &delivered,
                    &reason,
                    &status_code_i32,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn find_latest_alert_by_event_key(
        &self,
        event_key: &str,
    ) -> Result<Option<AlertEvent>> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT payload
                FROM alert_lifecycle_events
                WHERE event_key = $1
                ORDER BY id DESC
                LIMIT 1
                "#,
                &[&event_key],
            )
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let payload: serde_json::Value = row.get(0);
        let alert = serde_json::from_value(payload)?;
        Ok(Some(alert))
    }

    pub async fn record_usage_event(
        &self,
        tenant_id: &str,
        event_type: &str,
        alert_type: &str,
        chain_id: Option<i64>,
        quantity: i32,
    ) -> Result<()> {
        let normalized_quantity = quantity.max(1);
        self.client
            .execute(
                r#"
                INSERT INTO usage_events
                    (tenant_id, event_type, alert_type, chain_id, quantity, recorded_at)
                VALUES ($1, $2, $3, $4, $5, NOW())
                "#,
                &[
                    &tenant_id,
                    &event_type,
                    &alert_type,
                    &chain_id,
                    &normalized_quantity,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn count_usage_event_quantity_for_current_month(
        &self,
        tenant_id: &str,
        event_type: &str,
    ) -> Result<i64> {
        let row = self
            .client
            .query_one(
                r#"
                SELECT COALESCE(SUM(quantity), 0)::bigint AS total
                FROM usage_events
                WHERE tenant_id = $1
                  AND event_type = $2
                  AND recorded_at >= date_trunc('month', NOW())
                "#,
                &[&tenant_id, &event_type],
            )
            .await?;
        Ok(row.get::<_, i64>(0))
    }

    pub async fn load_tenant_monthly_alert_quota(&self, tenant_id: &str) -> Result<Option<i64>> {
        let row = match self
            .client
            .query_opt(
                r#"
                SELECT max_alerts_per_month
                FROM tenants
                WHERE tenant_id = $1
                "#,
                &[&tenant_id],
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!("tenants table not found while loading monthly alert quota");
                    return Ok(None);
                }
                return Err(error.into());
            }
        };

        let Some(row) = row else {
            return Ok(None);
        };

        let quota: Option<i32> = row.get(0);
        Ok(quota.map(i64::from))
    }

    // Finality state persistence methods
    pub async fn save_finality_state(
        &self,
        chain: &str,
        confirmation_depth: i32,
        head_block: i64,
        blocks_json: serde_json::Value,
        states_json: serde_json::Value,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO finality_state (chain, head_block, confirmation_depth, blocks, states, updated_at)
                VALUES ($1, $2, $3, $4, $5, NOW())
                ON CONFLICT (chain) DO UPDATE
                SET head_block = $2, confirmation_depth = $3, blocks = $4, states = $5, updated_at = NOW()
                "#,
                &[&chain, &head_block, &confirmation_depth, &blocks_json, &states_json],
            )
            .await?;
        Ok(())
    }

    pub async fn load_finality_state(&self, chain: &str) -> Result<Option<FinalityStateRow>> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT chain, head_block, confirmation_depth, blocks, states, updated_at
                FROM finality_state
                WHERE chain = $1
                "#,
                &[&chain],
            )
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(FinalityStateRow {
            chain: row.get(0),
            head_block: row.get(1),
            confirmation_depth: row.get(2),
            blocks: row.get(3),
            states: row.get(4),
            updated_at: row.get(5),
        }))
    }
}

fn resolve_tenant_id(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::env::var("ALERT_FALLBACK_TENANT_ID")
                .unwrap_or_else(|_| DEFAULT_ALERT_FALLBACK_TENANT_ID.to_string())
        })
}

fn parse_json_i64(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    let text = value.as_str()?;
    if let Some(hex) = text.strip_prefix("0x") {
        return i64::from_str_radix(hex, 16).ok();
    }
    text.parse::<i64>().ok()
}

#[derive(Debug)]
pub struct FinalityStateRow {
    pub chain: String,
    pub head_block: i64,
    pub confirmation_depth: i32,
    pub blocks: serde_json::Value,
    pub states: serde_json::Value,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
