use anyhow::Result;
use common_types::{AlertEvent, DetectionResult};
use tokio_postgres::{Client, NoTls};
use tracing::info;

pub struct PostgresRepository {
    client: Client,
}

impl PostgresRepository {
    pub async fn from_database_url(database_url: &str) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;

        tokio::spawn(async move {
            if let Err(err) = connection.await {
                tracing::error!(error = ?err, "postgres background connection error");
            }
        });

        let repo = Self { client };
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
                    lifecycle_state TEXT NOT NULL,
                    severity TEXT NOT NULL,
                    risk_score DOUBLE PRECISION NOT NULL,
                    payload JSONB NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

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
                INSERT INTO detections (id, tx_hash, chain, protocol, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                ON CONFLICT (id) DO NOTHING
                "#,
                &[
                    &detection.detection_id.to_string(),
                    &detection.tx_hash,
                    &format!("{:?}", detection.chain).to_lowercase(),
                    &detection.protocol,
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
                INSERT INTO alerts (id, tx_hash, chain, chain_slug, protocol, lifecycle_state, severity, risk_score, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                ON CONFLICT (id) DO UPDATE
                SET lifecycle_state = EXCLUDED.lifecycle_state,
                    severity = EXCLUDED.severity,
                    risk_score = EXCLUDED.risk_score,
                    payload = EXCLUDED.payload
                "#,
                &[
                    &alert.alert_id.to_string(),
                    &alert.tx_hash,
                    &format!("{:?}", alert.chain).to_lowercase(),
                    &alert.chain_slug,
                    &alert.protocol,
                    &format!("{:?}", alert.lifecycle_state).to_lowercase(),
                    &format!("{:?}", alert.severity).to_lowercase(),
                    &alert.risk_score,
                    &payload,
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
}
