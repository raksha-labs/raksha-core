use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::{Client, NoTls};
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEnvelopeV1 {
    pub envelope_id: String,
    pub source_id: String,
    pub source_type: String,
    pub stream_id: String,
    pub schema_version: String,
    pub event_type: String,
    pub event_ts: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub partition_key: String,
    pub idempotency_key: String,
    pub payload: Value,
    pub chain_id: Option<i64>,
    pub block_number: Option<i64>,
    pub tx_hash: Option<String>,
    pub log_index: Option<i64>,
    pub topic0: Option<String>,
    pub market_key: Option<String>,
    pub price: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRecordPointer {
    pub raw_ref_type: String,
    pub raw_ref_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestBatchMetrics {
    pub batch_id: String,
    pub stream_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub rows_ingested: i64,
    pub rows_deduped: i64,
    pub status: String,
    pub error_summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IngestFailureRecord {
    pub stream_id: Option<String>,
    pub source_id: String,
    pub source_type: String,
    pub event_type: Option<String>,
    pub payload_excerpt: Value,
    pub error_kind: String,
    pub error_message: String,
    pub retryable: bool,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct PostgresRawRepository {
    client: Arc<Client>,
}

impl PostgresRawRepository {
    pub async fn from_database_url(database_url: &str) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(err) = connection.await {
                tracing::error!(error = ?err, "raw postgres background connection error");
            }
        });

        Ok(Self {
            client: Arc::new(client),
        })
    }

    pub async fn from_env() -> Option<Result<Self>> {
        let database_url = std::env::var("RAW_DATABASE_URL").ok()?;
        Some(Self::from_database_url(&database_url).await)
    }

    pub async fn write_source_envelope(
        &self,
        envelope: &SourceEnvelopeV1,
    ) -> Result<Option<RawRecordPointer>> {
        let pointer = match envelope.source_type.as_str() {
            "evm_chain" => self.insert_chain_event(envelope).await?,
            "cex_websocket" => self.insert_cex_tick(envelope).await?,
            _ => self.insert_dex_event(envelope).await?,
        };
        Ok(pointer)
    }

    pub async fn save_ingest_offset(
        &self,
        stream_id: &str,
        partition_key: &str,
        last_cursor: Option<&str>,
        last_block_number: Option<i64>,
        last_seen_ts: DateTime<Utc>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO raw_ingest.ingest_offsets (
                    stream_id,
                    partition_key,
                    last_cursor,
                    last_block_number,
                    last_seen_ts,
                    updated_at
                )
                VALUES ($1, $2, $3, $4, $5, NOW())
                ON CONFLICT (stream_id, partition_key) DO UPDATE
                SET last_cursor = EXCLUDED.last_cursor,
                    last_block_number = EXCLUDED.last_block_number,
                    last_seen_ts = EXCLUDED.last_seen_ts,
                    updated_at = NOW()
                "#,
                &[
                    &stream_id,
                    &partition_key,
                    &last_cursor,
                    &last_block_number,
                    &last_seen_ts,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn record_ingest_batch(&self, metrics: &IngestBatchMetrics) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO raw_ingest.ingest_batches (
                    batch_id,
                    stream_id,
                    started_at,
                    ended_at,
                    rows_ingested,
                    rows_deduped,
                    status,
                    error_summary
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (batch_id) DO UPDATE
                SET ended_at = EXCLUDED.ended_at,
                    rows_ingested = EXCLUDED.rows_ingested,
                    rows_deduped = EXCLUDED.rows_deduped,
                    status = EXCLUDED.status,
                    error_summary = EXCLUDED.error_summary
                "#,
                &[
                    &metrics.batch_id,
                    &metrics.stream_id,
                    &metrics.started_at,
                    &metrics.ended_at,
                    &metrics.rows_ingested,
                    &metrics.rows_deduped,
                    &metrics.status,
                    &metrics.error_summary,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn record_ingest_failure(&self, failure: &IngestFailureRecord) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO raw_ingest.ingest_failures (
                    stream_id,
                    source_id,
                    source_type,
                    event_type,
                    payload_excerpt,
                    error_kind,
                    error_message,
                    retryable,
                    observed_at,
                    created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
                "#,
                &[
                    &failure.stream_id,
                    &failure.source_id,
                    &failure.source_type,
                    &failure.event_type,
                    &failure.payload_excerpt,
                    &failure.error_kind,
                    &failure.error_message,
                    &failure.retryable,
                    &failure.observed_at,
                ],
            )
            .await?;
        Ok(())
    }

    async fn insert_chain_event(
        &self,
        envelope: &SourceEnvelopeV1,
    ) -> Result<Option<RawRecordPointer>> {
        let row = self
            .client
            .query_opt(
                r#"
                INSERT INTO raw_ingest.chain_events (
                    source_id,
                    stream_id,
                    event_type,
                    event_ts,
                    observed_at,
                    chain_id,
                    block_number,
                    tx_hash,
                    log_index,
                    topic0,
                    decoded_json,
                    raw_payload,
                    provider,
                    request_window,
                    schema_version,
                    idempotency_key
                )
                VALUES (
                    $1, $2, $3, $4, $5,
                    $6, $7, $8, $9, $10,
                    $11, $12, $13, $14, $15, $16
                )
                ON CONFLICT (idempotency_key) DO UPDATE
                SET observed_at = EXCLUDED.observed_at
                RETURNING event_id::text
                "#,
                &[
                    &envelope.source_id,
                    &envelope.stream_id,
                    &envelope.event_type,
                    &envelope.event_ts,
                    &envelope.observed_at,
                    &envelope.chain_id,
                    &envelope.block_number,
                    &envelope.tx_hash,
                    &envelope.log_index,
                    &envelope.topic0,
                    &envelope.payload,
                    &envelope.payload,
                    &Some(envelope.source_id.clone()),
                    &Some(envelope.partition_key.clone()),
                    &envelope.schema_version,
                    &envelope.idempotency_key,
                ],
            )
            .await;

        match row {
            Ok(Some(row)) => Ok(Some(RawRecordPointer {
                raw_ref_type: "rawdb:raw_ingest.chain_events".to_string(),
                raw_ref_id: row.get::<usize, String>(0),
            })),
            Ok(None) => Ok(None),
            Err(error) => {
                warn!(error = ?error, "raw chain_events insert failed");
                Ok(None)
            }
        }
    }

    async fn insert_dex_event(
        &self,
        envelope: &SourceEnvelopeV1,
    ) -> Result<Option<RawRecordPointer>> {
        let row = self
            .client
            .query_opt(
                r#"
                INSERT INTO raw_ingest.dex_events (
                    source_id,
                    stream_id,
                    event_type,
                    event_ts,
                    observed_at,
                    market_key,
                    chain_id,
                    tx_hash,
                    decoded_json,
                    raw_payload,
                    schema_version,
                    idempotency_key
                )
                VALUES (
                    $1, $2, $3, $4, $5,
                    $6, $7, $8, $9, $10, $11, $12
                )
                ON CONFLICT (idempotency_key) DO UPDATE
                SET observed_at = EXCLUDED.observed_at
                RETURNING event_id::text
                "#,
                &[
                    &envelope.source_id,
                    &envelope.stream_id,
                    &envelope.event_type,
                    &envelope.event_ts,
                    &envelope.observed_at,
                    &envelope.market_key,
                    &envelope.chain_id,
                    &envelope.tx_hash,
                    &envelope.payload,
                    &envelope.payload,
                    &envelope.schema_version,
                    &envelope.idempotency_key,
                ],
            )
            .await;

        match row {
            Ok(Some(row)) => Ok(Some(RawRecordPointer {
                raw_ref_type: "rawdb:raw_ingest.dex_events".to_string(),
                raw_ref_id: row.get::<usize, String>(0),
            })),
            Ok(None) => Ok(None),
            Err(error) => {
                warn!(error = ?error, "raw dex_events insert failed");
                Ok(None)
            }
        }
    }

    async fn insert_cex_tick(
        &self,
        envelope: &SourceEnvelopeV1,
    ) -> Result<Option<RawRecordPointer>> {
        let row = self
            .client
            .query_opt(
                r#"
                INSERT INTO raw_ingest.cex_ticks (
                    source_id,
                    stream_id,
                    event_type,
                    event_ts,
                    observed_at,
                    market_key,
                    price_last,
                    sequence,
                    payload,
                    schema_version,
                    idempotency_key
                )
                VALUES (
                    $1, $2, $3, $4, $5,
                    $6, $7, $8, $9, $10, $11
                )
                ON CONFLICT (idempotency_key) DO UPDATE
                SET observed_at = EXCLUDED.observed_at
                RETURNING tick_id::text
                "#,
                &[
                    &envelope.source_id,
                    &envelope.stream_id,
                    &envelope.event_type,
                    &envelope.event_ts,
                    &envelope.observed_at,
                    &envelope.market_key,
                    &envelope.price,
                    &Some(envelope.envelope_id.clone()),
                    &envelope.payload,
                    &envelope.schema_version,
                    &envelope.idempotency_key,
                ],
            )
            .await;

        match row {
            Ok(Some(row)) => Ok(Some(RawRecordPointer {
                raw_ref_type: "rawdb:raw_ingest.cex_ticks".to_string(),
                raw_ref_id: row.get::<usize, String>(0),
            })),
            Ok(None) => Ok(None),
            Err(error) => {
                warn!(error = ?error, "raw cex_ticks insert failed");
                Ok(None)
            }
        }
    }
}
