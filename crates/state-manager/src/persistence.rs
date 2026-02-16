use anyhow::Result;
use event_schema::{AlertEvent, DetectionResult, MarketConsensusSnapshot, MarketQuoteEvent};
use std::sync::Arc;
use tokio_postgres::{Client, NoTls};
use tracing::info;

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
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS subject_type TEXT;
                ALTER TABLE alerts
                    ADD COLUMN IF NOT EXISTS subject_key TEXT;

                CREATE INDEX IF NOT EXISTS idx_detections_subject_created
                    ON detections (subject_type, subject_key, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_alerts_subject_created
                    ON alerts (subject_type, subject_key, created_at DESC);

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

                CREATE TABLE IF NOT EXISTS market_quote_ticks (
                    quote_id UUID PRIMARY KEY,
                    tenant_id TEXT NOT NULL,
                    source_id TEXT NOT NULL,
                    source_kind TEXT NOT NULL,
                    source_name TEXT NOT NULL,
                    market_key TEXT NOT NULL,
                    source_symbol TEXT NOT NULL,
                    price DOUBLE PRECISION NOT NULL,
                    peg_target DOUBLE PRECISION NOT NULL,
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
                "#,
            )
            .await?;

        info!("postgres schema initialized");
        Ok(())
    }

    pub async fn save_detection(&self, detection: &DetectionResult) -> Result<()> {
        let payload = serde_json::to_value(detection)?;
        self.client
            .execute(
                r#"
                INSERT INTO detections (id, tx_hash, chain, protocol, subject_type, subject_key, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                ON CONFLICT (id) DO NOTHING
                "#,
                &[
                    &detection.detection_id.to_string(),
                    &detection.tx_hash,
                    &format!("{:?}", detection.chain).to_lowercase(),
                    &detection.protocol,
                    &detection.subject_type,
                    &detection.subject_key,
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
        self.client
            .execute(
                r#"
                INSERT INTO alerts (id, tx_hash, chain, chain_slug, protocol, subject_type, subject_key, lifecycle_state, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (id) DO UPDATE
                SET lifecycle_state = EXCLUDED.lifecycle_state,
                    severity = EXCLUDED.severity,
                    risk_score = EXCLUDED.risk_score,
                    subject_type = EXCLUDED.subject_type,
                    subject_key = EXCLUDED.subject_key,
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
                    &format!("{:?}", alert.lifecycle_state).to_lowercase(),
                    &format!("{:?}", alert.severity).to_lowercase(),
                    &alert.risk_score,
                    &payload,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn save_market_quote(&self, quote: &MarketQuoteEvent) -> Result<()> {
        let payload = serde_json::to_value(quote)?;
        self.client
            .execute(
                r#"
                INSERT INTO market_quote_ticks (
                    quote_id, tenant_id, source_id, source_kind, source_name,
                    market_key, source_symbol, price, peg_target, observed_at, payload
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (quote_id) DO NOTHING
                "#,
                &[
                    &quote.quote_id,
                    &quote.tenant_id,
                    &quote.source_id,
                    &quote.source_kind,
                    &quote.source_name,
                    &quote.market_key,
                    &quote.source_symbol,
                    &quote.price,
                    &quote.peg_target,
                    &quote.observed_at,
                    &payload,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn save_market_snapshot(&self, snapshot: &MarketConsensusSnapshot) -> Result<()> {
        let payload = serde_json::to_value(snapshot)?;
        let severity = snapshot
            .severity
            .as_ref()
            .map(|value| format!("{:?}", value).to_lowercase());
        let source_count = snapshot.source_count as i32;
        self.client
            .execute(
                r#"
                INSERT INTO market_consensus_snapshots (
                    snapshot_id, tenant_id, market_key, peg_target, weighted_median_price,
                    divergence_pct, source_count, quorum_met, breach_active, severity,
                    observed_at, payload
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                ON CONFLICT (snapshot_id) DO NOTHING
                "#,
                &[
                    &snapshot.snapshot_id,
                    &snapshot.tenant_id,
                    &snapshot.market_key,
                    &snapshot.peg_target,
                    &snapshot.weighted_median_price,
                    &snapshot.divergence_pct,
                    &source_count,
                    &snapshot.quorum_met,
                    &snapshot.breach_active,
                    &severity,
                    &snapshot.observed_at,
                    &payload,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn upsert_dpeg_alert_state(
        &self,
        tenant_id: &str,
        market_key: &str,
        breach_started_at: Option<chrono::DateTime<chrono::Utc>>,
        cooldown_until: Option<chrono::DateTime<chrono::Utc>>,
        last_alerted_at: Option<chrono::DateTime<chrono::Utc>>,
        last_divergence_pct: Option<f64>,
        last_severity: Option<&str>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO dpeg_alert_state (
                    tenant_id, market_key, breach_started_at, cooldown_until,
                    last_alerted_at, last_divergence_pct, last_severity, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
                ON CONFLICT (tenant_id, market_key) DO UPDATE
                SET breach_started_at = EXCLUDED.breach_started_at,
                    cooldown_until = EXCLUDED.cooldown_until,
                    last_alerted_at = EXCLUDED.last_alerted_at,
                    last_divergence_pct = EXCLUDED.last_divergence_pct,
                    last_severity = EXCLUDED.last_severity,
                    updated_at = NOW()
                "#,
                &[
                    &tenant_id,
                    &market_key,
                    &breach_started_at,
                    &cooldown_until,
                    &last_alerted_at,
                    &last_divergence_pct,
                    &last_severity,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn load_dpeg_alert_state(
        &self,
        tenant_id: &str,
        market_key: &str,
    ) -> Result<Option<DpegAlertStateRow>> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT tenant_id, market_key, breach_started_at, cooldown_until,
                       last_alerted_at, last_divergence_pct, last_severity, updated_at
                FROM dpeg_alert_state
                WHERE tenant_id = $1 AND market_key = $2
                "#,
                &[&tenant_id, &market_key],
            )
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(DpegAlertStateRow {
            tenant_id: row.get(0),
            market_key: row.get(1),
            breach_started_at: row.get(2),
            cooldown_until: row.get(3),
            last_alerted_at: row.get(4),
            last_divergence_pct: row.get(5),
            last_severity: row.get(6),
            updated_at: row.get(7),
        }))
    }

    pub async fn upsert_connector_health(
        &self,
        tenant_id: &str,
        source_id: &str,
        healthy: bool,
        last_message_at: Option<chrono::DateTime<chrono::Utc>>,
        last_error: Option<&str>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO connector_health_state (
                    tenant_id, source_id, healthy, last_message_at, last_error, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, NOW())
                ON CONFLICT (tenant_id, source_id) DO UPDATE
                SET healthy = EXCLUDED.healthy,
                    last_message_at = EXCLUDED.last_message_at,
                    last_error = EXCLUDED.last_error,
                    updated_at = NOW()
                "#,
                &[&tenant_id, &source_id, &healthy, &last_message_at, &last_error],
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

    pub async fn load_enabled_market_sources(&self) -> Result<Vec<MarketSourceConfigRow>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT tenant_id, source_id, source_kind, source_name, ws_endpoint, metadata
                FROM tenant_market_sources
                WHERE enabled = TRUE
                ORDER BY tenant_id, source_id
                "#,
                &[],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| MarketSourceConfigRow {
                tenant_id: row.get(0),
                source_id: row.get(1),
                source_kind: row.get(2),
                source_name: row.get(3),
                ws_endpoint: row.get(4),
                metadata: row.get(5),
            })
            .collect())
    }

    pub async fn load_enabled_market_pairs(&self) -> Result<Vec<MarketPairConfigRow>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT tenant_id, source_id, market_key, source_symbol, peg_target
                FROM tenant_market_pairs
                WHERE enabled = TRUE
                ORDER BY tenant_id, source_id, market_key
                "#,
                &[],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| MarketPairConfigRow {
                tenant_id: row.get(0),
                source_id: row.get(1),
                market_key: row.get(2),
                source_symbol: row.get(3),
                peg_target: row.get(4),
            })
            .collect())
    }

    pub async fn load_dpeg_policies(&self) -> Result<Vec<DpegPolicyConfigRow>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT tenant_id, market_key, peg_target, min_sources, quorum_pct,
                       sustained_window_ms, cooldown_sec, stale_timeout_ms, severity_bands
                FROM tenant_dpeg_policies
                ORDER BY tenant_id, market_key
                "#,
                &[],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| DpegPolicyConfigRow {
                tenant_id: row.get(0),
                market_key: row.get(1),
                peg_target: row.get(2),
                min_sources: row.get::<_, i32>(3) as usize,
                quorum_pct: row.get(4),
                sustained_window_ms: row.get::<_, i32>(5) as i64,
                cooldown_sec: row.get::<_, i32>(6) as i64,
                stale_timeout_ms: row.get::<_, i32>(7) as i64,
                severity_bands: row.get(8),
            })
            .collect())
    }

    pub async fn load_dpeg_policy_sources(&self) -> Result<Vec<DpegPolicySourceConfigRow>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT tenant_id, market_key, source_id, weight, enabled, stale_timeout_ms
                FROM tenant_dpeg_policy_sources
                ORDER BY tenant_id, market_key, source_id
                "#,
                &[],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| DpegPolicySourceConfigRow {
                tenant_id: row.get(0),
                market_key: row.get(1),
                source_id: row.get(2),
                weight: row.get(3),
                enabled: row.get(4),
                stale_timeout_ms: row.get::<_, Option<i32>>(5).map(i64::from),
            })
            .collect())
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

#[derive(Debug)]
pub struct FinalityStateRow {
    pub chain: String,
    pub head_block: i64,
    pub confirmation_depth: i32,
    pub blocks: serde_json::Value,
    pub states: serde_json::Value,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct DpegAlertStateRow {
    pub tenant_id: String,
    pub market_key: String,
    pub breach_started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub cooldown_until: Option<chrono::DateTime<chrono::Utc>>,
    pub last_alerted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_divergence_pct: Option<f64>,
    pub last_severity: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct MarketSourceConfigRow {
    pub tenant_id: String,
    pub source_id: String,
    pub source_kind: String,
    pub source_name: String,
    pub ws_endpoint: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct MarketPairConfigRow {
    pub tenant_id: String,
    pub source_id: String,
    pub market_key: String,
    pub source_symbol: String,
    pub peg_target: f64,
}

#[derive(Debug, Clone)]
pub struct DpegPolicyConfigRow {
    pub tenant_id: String,
    pub market_key: String,
    pub peg_target: f64,
    pub min_sources: usize,
    pub quorum_pct: f64,
    pub sustained_window_ms: i64,
    pub cooldown_sec: i64,
    pub stale_timeout_ms: i64,
    pub severity_bands: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct DpegPolicySourceConfigRow {
    pub tenant_id: String,
    pub market_key: String,
    pub source_id: String,
    pub weight: f64,
    pub enabled: bool,
    pub stale_timeout_ms: Option<i64>,
}
