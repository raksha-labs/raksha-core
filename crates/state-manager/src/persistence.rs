use anyhow::Result;
use event_schema::{AlertEvent, DetectionResult, UnifiedEvent};
use common::DataSourceConfig;
use std::sync::Arc;
use tokio_postgres::{error::SqlState, Client, NoTls};
use tracing::{info, warn};

const DEFAULT_ALERT_FALLBACK_TENANT_ID: &str = "glider";

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
                    ADD COLUMN IF NOT EXISTS tenant_id TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS subject_type TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS subject_key TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS tenant_id TEXT;

                UPDATE detections
                SET tenant_id = COALESCE(NULLIF(payload->>'tenant_id', ''), 'glider')
                WHERE tenant_id IS NULL;

                UPDATE alerts
                SET tenant_id = COALESCE(NULLIF(payload->>'tenant_id', ''), 'glider')
                WHERE tenant_id IS NULL;

                CREATE INDEX IF NOT EXISTS idx_detections_subject_created
                    ON detections (subject_type, subject_key, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_subject_created
                    ON alerts (subject_type, subject_key, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_detections_tenant_created
                    ON detections (tenant_id, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_tenant_created
                    ON alerts (tenant_id, created_at DESC);
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
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (source_id)
                );

                CREATE TABLE IF NOT EXISTS tenant_data_sources (
                    tenant_id TEXT NOT NULL,
                    source_id TEXT NOT NULL REFERENCES data_sources(source_id),
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    override_config JSONB,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (tenant_id, source_id)
                );

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

                CREATE TABLE IF NOT EXISTS raw_events (
                    event_id TEXT NOT NULL,
                    tenant_id TEXT NOT NULL,
                    source_id TEXT NOT NULL,
                    source_type TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    payload JSONB NOT NULL,
                    chain_id BIGINT,
                    block_number BIGINT,
                    tx_hash TEXT,
                    market_key TEXT,
                    price DOUBLE PRECISION,
                    observed_at TIMESTAMPTZ NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (event_id)
                );

                CREATE INDEX IF NOT EXISTS idx_raw_events_tenant_source_observed
                    ON raw_events (tenant_id, source_id, observed_at DESC);
                CREATE INDEX IF NOT EXISTS idx_raw_events_tenant_event_type
                    ON raw_events (tenant_id, event_type, observed_at DESC);

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
                INSERT INTO detections (id, tx_hash, chain, protocol, subject_type, subject_key, tenant_id, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
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
                INSERT INTO alerts (id, tx_hash, chain, chain_slug, protocol, subject_type, subject_key, tenant_id, lifecycle_state, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                ON CONFLICT (id) DO UPDATE
                SET lifecycle_state = EXCLUDED.lifecycle_state,
                    severity = EXCLUDED.severity,
                    risk_score = EXCLUDED.risk_score,
                    subject_type = EXCLUDED.subject_type,
                    subject_key = EXCLUDED.subject_key,
                    tenant_id = EXCLUDED.tenant_id,
                    payload = EXCLUDED.payload
                "#,
                &[
                    &alert.alert_id.to_string(),
                    &alert.tx_hash,
                    &format!("{:?}", alert.chain).to_lowercase(),
                    &alert.chain_slug,
                    &alert.protocol,
                    &alert.subject_type,
                    &alert.subject_key,
                    &tenant_id,
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

    pub async fn insert_raw_event(&self, event: &UnifiedEvent) -> Result<()> {
        let payload = serde_json::to_value(event)?;
        self.client
            .execute(
                r#"
                INSERT INTO raw_events (
                    event_id, tenant_id, source_id, source_type, event_type,
                    payload, chain_id, block_number, tx_hash,
                    market_key, price, observed_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                ON CONFLICT (event_id) DO NOTHING
                "#,
                &[
                    &event.event_id,
                    &event.tenant_id,
                    &event.source_id,
                    &format!("{:?}", event.source_type).to_lowercase(),
                    &event.event_type,
                    &payload,
                    &event.chain_id,
                    &event.block_number,
                    &event.tx_hash,
                    &event.market_key,
                    &event.price,
                    &event.timestamp,
                ],
            )
            .await?;
        Ok(())
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

#[derive(Debug)]
pub struct FinalityStateRow {
    pub chain: String,
    pub head_block: i64,
    pub confirmation_depth: i32,
    pub blocks: serde_json::Value,
    pub states: serde_json::Value,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
