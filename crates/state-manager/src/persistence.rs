use anyhow::Result;
use chrono::{DateTime, Utc};
use common::{connect_postgres_client, DataSourceConfig};
use event_schema::{AlertEvent, DetectionResult, UnifiedEvent};
use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::{error::SqlState, Client};
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
    pub operating_mode_profile: String, // "live" | "test"
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
    pub poll_interval_ms: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct StreamTenantTarget {
    pub tenant_id: String,
}

#[derive(Debug, Clone)]
pub struct IncidentRecord {
    pub incident_id: String,
    pub tenant_id: String,
    pub pattern_id: String,
    pub subject_type: Option<String>,
    pub subject_key: Option<String>,
    pub chain_slug: String,
    pub status: String,
    pub current_severity: String,
}

/// A row from the `tenant_monitored_entities` table representing a user's tracked position.
#[derive(Debug, Clone)]
pub struct MonitoredEntityRow {
    pub entity_id: String,
    pub entity_type: String,
    pub display_name: Option<String>,
    pub chain_slug: Option<String>,
    pub asset_symbol: Option<String>,
    pub quantity: f64,
    pub valuation_usd: f64,
    pub metadata_json: Option<Value>,
}

/// A computed exposure record to write into `incident_entity_exposures`.
#[derive(Debug, Clone)]
pub struct EntityExposureRecord {
    pub entity_id: String,
    pub capital_at_risk_usd: f64,
    pub liquidity_status: String,
    pub estimated_slippage_pct: f64,
    pub loss_scenario_5pct_usd: f64,
    pub loss_scenario_10pct_usd: f64,
    pub estimated_savings_usd: f64,
    pub payload: Value,
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

#[derive(Debug, Clone)]
pub struct IngestOperationalEventRecord {
    pub stream_id: Option<String>,
    pub source_id: String,
    pub source_type: String,
    pub tenant_id: Option<String>,
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
    pub raw_ref_type: Option<String>,
    pub raw_ref_id: Option<String>,
    pub raw_s3_uri: Option<String>,
    pub is_simulated: bool,
    pub simulation_run_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OperationalSourcePrice {
    pub source_id: String,
    pub price: f64,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct AlertEvidenceOperationalRow {
    pub ingest_event_id: String,
    pub source_id: String,
    pub source_type: String,
    pub event_type: String,
    pub market_key: Option<String>,
    pub price: Option<f64>,
    pub observed_at: DateTime<Utc>,
    pub event_ts: Option<DateTime<Utc>>,
    pub tx_hash: Option<String>,
    pub block_number: Option<i64>,
    pub payload: Value,
    pub normalized_fields: Value,
}

#[derive(Debug, Clone)]
pub struct AlertEvidenceSimulationRow {
    pub run_event_id: String,
    pub source_id: String,
    pub source_table: String,
    pub event_type: String,
    pub observed_at: DateTime<Utc>,
    pub original_payload: Value,
    pub published_payload: Value,
}

pub struct AlertEvidenceOperationalQuery<'a> {
    pub tenant_id: &'a str,
    pub market_key: Option<&'a str>,
    pub source_ids: &'a [String],
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub is_simulated: bool,
    pub simulation_run_id: Option<&'a str>,
}

pub struct AlertEvidenceSimulationQuery<'a> {
    pub simulation_run_id: &'a str,
    pub source_ids: &'a [String],
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct AlertEvidenceSnapshotRecord {
    pub alert_id: String,
    pub incident_id: Option<String>,
    pub tenant_id: String,
    pub pattern_id: Option<String>,
    pub source_id: String,
    pub source_type: String,
    pub event_type: String,
    pub market_key: Option<String>,
    pub price: Option<f64>,
    pub observed_at: DateTime<Utc>,
    pub event_ts: Option<DateTime<Utc>>,
    pub tx_hash: Option<String>,
    pub block_number: Option<i64>,
    pub payload: Value,
    pub normalized_fields: Value,
    pub raw_ref_type: Option<String>,
    pub raw_ref_id: Option<String>,
}

fn simulation_metadata_from_payload(payload: &Value) -> (bool, Option<String>) {
    let simulation = payload.get("simulation").and_then(Value::as_object);
    let run_id = simulation
        .and_then(|meta| {
            meta.get("run_id")
                .or_else(|| meta.get("simulation_run_id"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let is_simulated = simulation
        .and_then(|meta| meta.get("is_simulated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || payload
            .get("is_simulated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || run_id.is_some();
    (is_simulated, run_id)
}

#[derive(Debug, Clone)]
pub struct PatternSnapshotInsert<'a> {
    pub tenant_id: &'a str,
    pub pattern_id: &'a str,
    pub snapshot_key: &'a str,
    pub data: Value,
    pub score: Option<f64>,
    pub severity: Option<&'a str>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct PostgresRepository {
    client: Arc<Client>,
}

pub struct IncidentKey<'a> {
    pub tenant_id: &'a str,
    pub pattern_id: &'a str,
    pub subject_type: Option<&'a str>,
    pub subject_key: Option<&'a str>,
    pub chain_slug: &'a str,
}

impl PostgresRepository {
    pub async fn from_database_url(database_url: &str) -> Result<Self> {
        let client =
            connect_postgres_client(database_url, "postgres background connection error").await?;

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
        let required_tables = [
            "detection.detections",
            "detection.alerts",
            "detection.alert_lifecycle_events",
            "detection.incidents",
            "detection.incident_events",
            "detection.incident_context_snapshots",
            "detection.alert_delivery_attempts",
            "detection.usage_events",
            "catalog.data_sources",
            "catalog.tenant_data_sources",
            "catalog.source_stream_configs",
            "catalog.source_stream_tenant_targets",
            "pattern.patterns",
            "pattern.pattern_configs",
            "pattern.tenant_pattern_configs",
            "pattern.tenant_pattern_source_bindings",
            "pattern.tenant_pattern_required_assets",
            "catalog.source_required_pairs",
            "pattern.tenant_pattern_alert_policies",
            "pattern.tenant_pattern_notification_channels",
            "pattern.pattern_state",
            "pattern.pattern_snapshots",
            "catalog.data_source_health",
            "catalog.ingest_operational_events",
        ];

        let mut missing_tables = Vec::new();
        for table in required_tables {
            let (table_schema, table_name) = table.split_once('.').unwrap_or(("public", table));
            let exists = self
                .client
                .query_opt(
                    r#"
                    SELECT 1
                    FROM information_schema.tables
                    WHERE table_schema = $1
                      AND table_name = $2
                    "#,
                    &[&table_schema, &table_name],
                )
                .await?;
            if exists.is_none() {
                missing_tables.push(table);
            }
        }

        if !missing_tables.is_empty() {
            anyhow::bail!(
                "missing core schema tables: {}. Run SQL bootstrap (bootstrap/core_schema.sql + bootstrap/seed_sources.sql + bootstrap/seed_patterns.sql)",
                missing_tables.join(", ")
            );
        }

        info!("postgres schema validated");
        Ok(())
    }

    pub async fn save_detection(&self, detection: &DetectionResult) -> Result<()> {
        let payload = serde_json::to_value(detection)?;
        let tenant_id = resolve_tenant_id(detection.tenant_id.as_deref());
        self.client
            .execute(
                r#"
                INSERT INTO detections (
                    id, tx_hash, chain, protocol, subject_type, subject_key, tenant_id, pattern_id,
                    severity, risk_score, payload, is_simulated, simulation_run_id
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
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
                    &detection.is_simulated,
                    &detection.simulation_run_id,
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
                INSERT INTO alerts (
                    id, incident_id, tx_hash, chain, chain_slug, protocol, block_number, subject_type,
                    subject_key, tenant_id, pattern_id, lifecycle_state, severity, risk_score, payload,
                    is_simulated, simulation_run_id
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
                ON CONFLICT (id) DO UPDATE
                SET incident_id = EXCLUDED.incident_id,
                    lifecycle_state = EXCLUDED.lifecycle_state,
                    severity = EXCLUDED.severity,
                    risk_score = EXCLUDED.risk_score,
                    block_number = EXCLUDED.block_number,
                    subject_type = EXCLUDED.subject_type,
                    subject_key = EXCLUDED.subject_key,
                    tenant_id = EXCLUDED.tenant_id,
                    pattern_id = EXCLUDED.pattern_id,
                    payload = EXCLUDED.payload,
                    is_simulated = EXCLUDED.is_simulated,
                    simulation_run_id = EXCLUDED.simulation_run_id
                "#,
                &[
                    &alert.alert_id.to_string(),
                    &alert.incident_id,
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
                    &alert.is_simulated,
                    &alert.simulation_run_id,
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
                    tenant_id, source_id, healthy, last_message_at, last_error, updated_at
                )
                VALUES (
                    $1,
                    $2,
                    $3,
                    CASE WHEN $3 THEN NOW() ELSE NULL END,
                    $4,
                    NOW()
                )
                ON CONFLICT (tenant_id, source_id) DO UPDATE
                SET healthy = EXCLUDED.healthy,
                    last_message_at = CASE
                        WHEN EXCLUDED.healthy THEN NOW()
                        ELSE data_source_health.last_message_at
                    END,
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
                FROM catalog.tenant_data_sources tds
                JOIN catalog.data_sources ds ON ds.source_id = tds.source_id
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
        // Composite query: joins all four per-tenant pattern config tables into one
        // JSONB blob per (tenant_id, pattern_id).  Each pattern's reload_config()
        // extracts "detection_config" for its thresholds; the outer wrapper
        // also carries source_bindings, alert_policy, and notification_channels
        // so the detector can gate events and alert delivery per-tenant.
        //
        // Backward compat: individual patterns do `config.get("detection_config").unwrap_or(config)`
        // so older code that stored flat configs still works.
        let rows = self
            .client
            .query(
                r#"
                SELECT
                    tpc.tenant_id,
                    tpc.pattern_id,
                    jsonb_build_object(
                        'enabled',          tpc.enabled,
                        'detection_config', tpc.config,
                        'source_bindings',  (
                            SELECT jsonb_agg(jsonb_build_object(
                                'source_id',      b.source_id,
                                'enabled',        b.enabled,
                                'binding_config', b.binding_config
                            ))
                            FROM pattern.tenant_pattern_source_bindings b
                            WHERE b.tenant_id   = tpc.tenant_id
                              AND b.pattern_id  = tpc.pattern_id
                              AND b.enabled     = TRUE
                        ),
                        'alert_policy',     (
                            SELECT row_to_json(ap)::jsonb
                            FROM pattern.tenant_pattern_alert_policies ap
                            WHERE ap.tenant_id  = tpc.tenant_id
                              AND ap.pattern_id = tpc.pattern_id
                        ),
                        'notification_channels', (
                            SELECT jsonb_agg(jsonb_build_object(
                                'channel',    nc.channel,
                                'enabled',    nc.enabled,
                                'config_json', nc.config_json
                            ))
                            FROM pattern.tenant_pattern_notification_channels nc
                            WHERE nc.tenant_id  = tpc.tenant_id
                              AND nc.pattern_id = tpc.pattern_id
                              AND nc.enabled    = TRUE
                        )
                    ) AS full_config
                FROM pattern.tenant_pattern_configs tpc
                JOIN pattern.patterns p ON p.pattern_id = tpc.pattern_id
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
            let full_config: serde_json::Value = row.get(2);
            map.insert((tenant_id, pattern_id), full_config);
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
                    COALESCE(ssc.connection_config_override, ds.connection_config) AS connection_config,
                    ssc.connector_mode,
                    ssc.operating_mode_profile,
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
                    ssc.payload_ts_unit,
                    ssc.poll_interval_ms
                FROM catalog.source_stream_configs ssc
                JOIN catalog.data_sources ds
                  ON ds.source_id = ssc.source_id
                WHERE ds.enabled = TRUE
                  AND ssc.enabled = TRUE
                  AND EXISTS (
                    SELECT 1
                    FROM catalog.source_stream_tenant_targets sstt
                    LEFT JOIN catalog.tenant_operating_mode tom
                      ON tom.tenant_id = sstt.tenant_id
                    WHERE sstt.stream_config_id = ssc.stream_config_id
                      AND sstt.enabled = TRUE
                      AND COALESCE(tom.mode, 'live') = ssc.operating_mode_profile
                  )
                ORDER BY ssc.source_id, ssc.operating_mode_profile, ssc.stream_name, ssc.asset_pair NULLS FIRST
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
                operating_mode_profile: row.get(6),
                stream_name: row.get(7),
                subscription_key: row.get(8),
                event_type: row.get(9),
                parser_name: row.get(10),
                market_key: row.get(11),
                asset_pair: row.get(12),
                filter_config: row.get(13),
                auth_secret_ref: row.get(14),
                auth_config: row.get(15),
                payload_ts_path: row.get(16),
                payload_ts_unit: row.get(17),
                poll_interval_ms: row.get(18),
            });
        }
        Ok(configs)
    }

    /// Returns tenant targets for a stream config, filtered by operating mode.
    ///
    /// Only tenants whose operating mode matches the stream's `operating_mode_profile`
    /// receive events from that stream.  Tenants with no row in
    /// `catalog.tenant_operating_mode` default to `'live'`.
    pub async fn list_stream_tenant_targets(
        &self,
        stream_config_id: &str,
        operating_mode_profile: &str,
    ) -> Result<Vec<StreamTenantTarget>> {
        let rows = match self
            .client
            .query(
                r#"
                SELECT sstt.tenant_id
                FROM catalog.source_stream_tenant_targets sstt
                LEFT JOIN catalog.tenant_operating_mode tom
                  ON tom.tenant_id = sstt.tenant_id
                WHERE sstt.stream_config_id::text = $1
                  AND sstt.enabled = TRUE
                  AND COALESCE(tom.mode, 'live') = $2
                ORDER BY sstt.tenant_id
                "#,
                &[&stream_config_id, &operating_mode_profile],
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
                INSERT INTO catalog.ingest_operational_events (
                    stream_id,
                    source_id,
                    source_type,
                    tenant_id,
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
                    dedup_key,
                    raw_ref_type,
                    raw_ref_id,
                    raw_s3_uri,
                    is_simulated,
                    simulation_run_id
                )
                VALUES (
                    ($1)::text::uuid,
                    $2,
                    $3,
                    NULL,
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
                    $20,
                    NULL,
                    NULL,
                    NULL,
                    FALSE,
                    NULL
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

    pub async fn insert_ingest_operational_event(
        &self,
        record: &IngestOperationalEventRecord,
    ) -> Result<bool> {
        let inserted = self
            .client
            .execute(
                r#"
                INSERT INTO catalog.ingest_operational_events (
                    stream_id,
                    source_id,
                    source_type,
                    tenant_id,
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
                    dedup_key,
                    raw_ref_type,
                    raw_ref_id,
                    raw_s3_uri,
                    is_simulated,
                    simulation_run_id
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
                    $20,
                    $21,
                    $22,
                    $23,
                    $24,
                    $25,
                    $26
                )
                ON CONFLICT DO NOTHING
                "#,
                &[
                    &record.stream_id.as_deref(),
                    &record.source_id,
                    &record.source_type,
                    &record.tenant_id,
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
                    &record.raw_ref_type,
                    &record.raw_ref_id,
                    &record.raw_s3_uri,
                    &record.is_simulated,
                    &record.simulation_run_id,
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
                DELETE FROM catalog.ingest_operational_events
                WHERE event_type = ANY($1)
                  AND created_at < NOW() - ($2::BIGINT * INTERVAL '1 second')
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
        self.latest_operational_market_price(market_key, max_age_seconds)
            .await
    }

    pub async fn latest_operational_market_price(
        &self,
        market_key: &str,
        max_age_seconds: i64,
    ) -> Result<Option<f64>> {
        let freshness_seconds = max_age_seconds.max(1);
        let row = self
            .client
            .query_opt(
                r#"
                SELECT price
                FROM catalog.ingest_operational_events
                WHERE market_key = $1
                  AND price IS NOT NULL
                  AND parse_status IN ('parsed', 'partial')
                  AND observed_at >= NOW() - ($2::BIGINT * INTERVAL '1 second')
                ORDER BY observed_at DESC
                LIMIT 1
                "#,
                &[&market_key, &freshness_seconds],
            )
            .await?;

        Ok(row.map(|record| record.get::<usize, f64>(0)))
    }

    pub async fn latest_operational_source_prices(
        &self,
        market_key: &str,
        max_age_seconds: i64,
    ) -> Result<Vec<OperationalSourcePrice>> {
        let freshness_seconds = max_age_seconds.max(1);
        let rows = self
            .client
            .query(
                r#"
                SELECT DISTINCT ON (source_id)
                    source_id,
                    price,
                    observed_at
                FROM catalog.ingest_operational_events
                WHERE market_key = $1
                  AND price IS NOT NULL
                  AND parse_status IN ('parsed', 'partial')
                  AND observed_at >= NOW() - ($2::BIGINT * INTERVAL '1 second')
                ORDER BY source_id, observed_at DESC
                "#,
                &[&market_key, &freshness_seconds],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| OperationalSourcePrice {
                source_id: row.get(0),
                price: row.get(1),
                observed_at: row.get(2),
            })
            .collect())
    }

    pub async fn insert_raw_event(&self, event: &UnifiedEvent) -> Result<()> {
        self.insert_source_feed_event(event).await
    }

    pub async fn insert_source_feed_event(&self, event: &UnifiedEvent) -> Result<()> {
        if !self
            .is_source_stream_ingest_enabled(&event.source_id)
            .await?
        {
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
        let (is_simulated, simulation_run_id) = simulation_metadata_from_payload(&payload);
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
                INSERT INTO catalog.ingest_operational_events (
                    stream_id,
                    source_id,
                    source_type,
                    tenant_id,
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
                    dedup_key,
                    raw_ref_type,
                    raw_ref_id,
                    raw_s3_uri,
                    is_simulated,
                    simulation_run_id
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
                    $14,
                    $14,
                    'parsed',
                    NULL,
                    $15,
                    $16,
                    $17,
                    NULL,
                    NULL,
                    NULL,
                    $18,
                    $19
                )
                ON CONFLICT DO NOTHING
                "#,
                &[
                    &event.source_id,
                    &format!("{:?}", event.source_type).to_lowercase(),
                    &Some(event.tenant_id.clone()),
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
                    &is_simulated,
                    &simulation_run_id,
                ],
            )
            .await?;
        Ok(())
    }

    async fn is_required_source_market_pair(
        &self,
        source_id: &str,
        market_key: &str,
    ) -> Result<bool> {
        let row = match self
            .client
            .query_opt(
                r#"
                SELECT 1
                FROM catalog.source_required_pairs
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
                    warn!(
                        "catalog.source_required_pairs table not found; skipping market-key filtering"
                    );
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
                FROM catalog.data_sources ds
                JOIN catalog.source_stream_configs ssc
                  ON ssc.source_id = ds.source_id
                 AND ssc.enabled = TRUE
                JOIN catalog.source_stream_tenant_targets stt
                  ON stt.stream_config_id = ssc.stream_config_id
                 AND stt.enabled = TRUE
                LEFT JOIN catalog.tenant_operating_mode tom
                  ON tom.tenant_id = stt.tenant_id
                WHERE ds.source_id = $1
                  AND ds.enabled = TRUE
                  AND COALESCE(tom.mode, 'live') = ssc.operating_mode_profile
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
                SELECT data FROM pattern.pattern_state
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
                INSERT INTO pattern.pattern_state (tenant_id, pattern_id, state_key, data, updated_at)
                VALUES ($1, $2, $3, $4, NOW())
                ON CONFLICT (tenant_id, pattern_id, state_key) DO UPDATE
                SET data = EXCLUDED.data, updated_at = NOW()
                "#,
                &[&tenant_id, &pattern_id, &state_key, &data],
            )
            .await?;
        Ok(())
    }

    pub async fn insert_pattern_snapshot(&self, snapshot: PatternSnapshotInsert<'_>) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO pattern.pattern_snapshots
                    (tenant_id, pattern_id, snapshot_key, data, score, severity, observed_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                "#,
                &[
                    &snapshot.tenant_id,
                    &snapshot.pattern_id,
                    &snapshot.snapshot_key,
                    &snapshot.data,
                    &snapshot.score,
                    &snapshot.severity,
                    &snapshot.observed_at,
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
                INSERT INTO alert_lifecycle_events (alert_id, incident_id, event_key, tx_hash, block_number, lifecycle_state, payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                "#,
                &[
                    &alert.alert_id.to_string(),
                    &alert.incident_id,
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

    pub async fn load_simulation_events_for_alert_evidence(
        &self,
        query: AlertEvidenceSimulationQuery<'_>,
    ) -> Result<Vec<AlertEvidenceSimulationRow>> {
        if query.source_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = match self
            .client
            .query(
                r#"
                SELECT
                    id::text,
                    NULLIF(
                        BTRIM(
                            COALESCE(
                                published_payload_json->>'source_id',
                                original_payload_json->>'source_id',
                                ''
                            )
                        ),
                        ''
                    ) AS source_id,
                    source_table,
                    event_type,
                    event_ts,
                    COALESCE(original_payload_json, '{}'::jsonb) AS original_payload_json,
                    COALESCE(published_payload_json, '{}'::jsonb) AS published_payload_json
                FROM workbench.simulation_run_events
                WHERE run_id = $1
                  AND event_ts >= $2
                  AND event_ts <= $3
                  AND NULLIF(
                        BTRIM(
                            COALESCE(
                                published_payload_json->>'source_id',
                                original_payload_json->>'source_id',
                                ''
                            )
                        ),
                        ''
                      ) = ANY($4)
                ORDER BY event_ts ASC, id ASC
                "#,
                &[
                    &query.simulation_run_id,
                    &query.window_start,
                    &query.window_end,
                    &query.source_ids,
                ],
            )
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!("simulation_run_events table not found; skipping simulation evidence lookup");
                    return Ok(vec![]);
                }
                return Err(error.into());
            }
        };

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let source_id: Option<String> = row.get(1);
                Some(AlertEvidenceSimulationRow {
                    run_event_id: row.get(0),
                    source_id: source_id?,
                    source_table: row.get(2),
                    event_type: row.get(3),
                    observed_at: row.get(4),
                    original_payload: row.get(5),
                    published_payload: row.get(6),
                })
            })
            .collect())
    }

    pub async fn load_operational_events_for_alert_evidence(
        &self,
        query: AlertEvidenceOperationalQuery<'_>,
    ) -> Result<Vec<AlertEvidenceOperationalRow>> {
        if query.source_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = self
            .client
            .query(
                r#"
                SELECT
                    ingest_event_id::text,
                    source_id,
                    source_type,
                    event_type,
                    market_key,
                    price,
                    observed_at,
                    payload_event_ts,
                    tx_hash,
                    block_number,
                    COALESCE(payload, '{}'::jsonb) AS payload,
                    COALESCE(normalized_fields, '{}'::jsonb) AS normalized_fields
                FROM catalog.ingest_operational_events
                WHERE tenant_id = $1
                  AND observed_at >= $2
                  AND observed_at <= $3
                  AND source_id = ANY($4)
                  AND ($5::text IS NULL OR market_key = $5)
                  AND is_simulated = $6
                  AND (
                    ($7::text IS NULL AND simulation_run_id IS NULL)
                    OR simulation_run_id = $7
                  )
                ORDER BY observed_at ASC, ingest_event_id ASC
                "#,
                &[
                    &query.tenant_id,
                    &query.window_start,
                    &query.window_end,
                    &query.source_ids,
                    &query.market_key,
                    &query.is_simulated,
                    &query.simulation_run_id,
                ],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| AlertEvidenceOperationalRow {
                ingest_event_id: row.get(0),
                source_id: row.get(1),
                source_type: row.get(2),
                event_type: row.get(3),
                market_key: row.get(4),
                price: row.get(5),
                observed_at: row.get(6),
                event_ts: row.get(7),
                tx_hash: row.get(8),
                block_number: row.get(9),
                payload: row.get(10),
                normalized_fields: row.get(11),
            })
            .collect())
    }

    pub async fn save_alert_evidence_snapshot_batch(
        &self,
        records: &[AlertEvidenceSnapshotRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        self.client
            .execute(
                r#"
                DELETE FROM detection.alert_evidence_snapshots
                WHERE tenant_id = $1
                  AND alert_id = $2
                "#,
                &[&records[0].tenant_id, &records[0].alert_id],
            )
            .await?;

        for record in records {
            self.client
                .execute(
                    r#"
                    INSERT INTO detection.alert_evidence_snapshots (
                        alert_id,
                        incident_id,
                        tenant_id,
                        pattern_id,
                        source_id,
                        source_type,
                        event_type,
                        market_key,
                        price,
                        observed_at,
                        event_ts,
                        tx_hash,
                        block_number,
                        payload,
                        normalized_fields,
                        raw_ref_type,
                        raw_ref_id
                    )
                    VALUES (
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
                        $14,
                        $15,
                        $16,
                        $17
                    )
                    "#,
                    &[
                        &record.alert_id,
                        &record.incident_id,
                        &record.tenant_id,
                        &record.pattern_id,
                        &record.source_id,
                        &record.source_type,
                        &record.event_type,
                        &record.market_key,
                        &record.price,
                        &record.observed_at,
                        &record.event_ts,
                        &record.tx_hash,
                        &record.block_number,
                        &record.payload,
                        &record.normalized_fields,
                        &record.raw_ref_type,
                        &record.raw_ref_id,
                    ],
                )
                .await?;
        }

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

    pub async fn find_active_incident(
        &self,
        key: IncidentKey<'_>,
    ) -> Result<Option<IncidentRecord>> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT incident_id, tenant_id, pattern_id, subject_type, subject_key, chain_slug, status, current_severity
                FROM incidents
                WHERE tenant_id = $1
                  AND pattern_id = $2
                  AND COALESCE(subject_type, '') = COALESCE($3, '')
                  AND COALESCE(subject_key, '') = COALESCE($4, '')
                  AND chain_slug = $5
                  AND status NOT IN ('resolved', 'retracted', 'closed', 'cancelled')
                ORDER BY updated_at DESC
                LIMIT 1
                "#,
                &[
                    &key.tenant_id,
                    &key.pattern_id,
                    &key.subject_type,
                    &key.subject_key,
                    &key.chain_slug,
                ],
            )
            .await?;

        Ok(row.map(|record| IncidentRecord {
            incident_id: record.get(0),
            tenant_id: record.get(1),
            pattern_id: record.get(2),
            subject_type: record.get(3),
            subject_key: record.get(4),
            chain_slug: record.get(5),
            status: record.get(6),
            current_severity: record.get(7),
        }))
    }

    pub async fn find_active_incident_for_simulation(
        &self,
        key: IncidentKey<'_>,
        simulation_run_id: &str,
    ) -> Result<Option<IncidentRecord>> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT i.incident_id, i.tenant_id, i.pattern_id, i.subject_type, i.subject_key, i.chain_slug, i.status, i.current_severity
                FROM incidents i
                WHERE i.tenant_id = $1
                  AND i.pattern_id = $2
                  AND COALESCE(i.subject_type, '') = COALESCE($3, '')
                  AND COALESCE(i.subject_key, '') = COALESCE($4, '')
                  AND i.chain_slug = $5
                  AND i.status NOT IN ('resolved', 'retracted', 'closed', 'cancelled')
                  AND EXISTS (
                      SELECT 1
                      FROM alerts a
                      WHERE a.incident_id = i.incident_id
                        AND a.is_simulated = TRUE
                        AND a.simulation_run_id = $6
                  )
                ORDER BY i.updated_at DESC
                LIMIT 1
                "#,
                &[
                    &key.tenant_id,
                    &key.pattern_id,
                    &key.subject_type,
                    &key.subject_key,
                    &key.chain_slug,
                    &simulation_run_id,
                ],
            )
            .await?;

        Ok(row.map(|record| IncidentRecord {
            incident_id: record.get(0),
            tenant_id: record.get(1),
            pattern_id: record.get(2),
            subject_type: record.get(3),
            subject_key: record.get(4),
            chain_slug: record.get(5),
            status: record.get(6),
            current_severity: record.get(7),
        }))
    }

    pub async fn create_incident(
        &self,
        incident: &IncidentRecord,
        opened_at: DateTime<Utc>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO incidents (
                    incident_id, tenant_id, pattern_id, subject_type, subject_key,
                    chain_slug, status, current_severity, opened_at, updated_at, closed_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, NULL)
                ON CONFLICT (incident_id) DO NOTHING
                "#,
                &[
                    &incident.incident_id,
                    &incident.tenant_id,
                    &incident.pattern_id,
                    &incident.subject_type,
                    &incident.subject_key,
                    &incident.chain_slug,
                    &incident.status,
                    &incident.current_severity,
                    &opened_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn update_incident_state(
        &self,
        incident_id: &str,
        status: &str,
        current_severity: &str,
        updated_at: DateTime<Utc>,
        closed: bool,
    ) -> Result<()> {
        let closed_at: Option<DateTime<Utc>> = if closed { Some(updated_at) } else { None };
        self.client
            .execute(
                r#"
                UPDATE incidents
                SET status = $2,
                    current_severity = $3,
                    updated_at = $4,
                    closed_at = CASE WHEN $5 THEN $6 ELSE closed_at END
                WHERE incident_id = $1
                "#,
                &[
                    &incident_id,
                    &status,
                    &current_severity,
                    &updated_at,
                    &closed,
                    &closed_at,
                ],
            )
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn append_incident_event(
        &self,
        incident_id: &str,
        transition_type: &str,
        from_state: Option<&str>,
        to_state: Option<&str>,
        reason: Option<&str>,
        payload: serde_json::Value,
        created_at: DateTime<Utc>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO incident_events
                    (incident_id, transition_type, from_state, to_state, reason, payload, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                "#,
                &[
                    &incident_id,
                    &transition_type,
                    &from_state,
                    &to_state,
                    &reason,
                    &payload,
                    &created_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn append_incident_context_snapshot(
        &self,
        incident_id: &str,
        classification: Option<&str>,
        score: Option<f64>,
        confidence: Option<f64>,
        payload: serde_json::Value,
        observed_at: DateTime<Utc>,
    ) -> Result<()> {
        self.client
            .execute(
                r#"
                INSERT INTO incident_context_snapshots
                    (incident_id, classification, score, confidence, payload, observed_at)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
                &[
                    &incident_id,
                    &classification,
                    &score,
                    &confidence,
                    &payload,
                    &observed_at,
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

    pub async fn count_usage_event_quantity_for_past_hour(
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
                  AND recorded_at >= NOW() - INTERVAL '1 hour'
                "#,
                &[&tenant_id, &event_type],
            )
            .await?;
        Ok(row.get::<_, i64>(0))
    }

    pub async fn load_tenant_hourly_alert_limit(&self, tenant_id: &str) -> Result<Option<i64>> {
        let row = match self
            .client
            .query_opt(
                r#"
                SELECT hourly_alert_limit
                FROM notify.tenant_delivery_controls
                WHERE tenant_id = $1
                "#,
                &[&tenant_id],
            )
            .await
        {
            Ok(row) => row,
            Err(_) => return Ok(None),
        };

        Ok(row.map(|row| row.get::<_, i32>(0) as i64))
    }

    pub async fn load_tenant_monthly_alert_quota(&self, tenant_id: &str) -> Result<Option<i64>> {
        let row = match self
            .client
            .query_opt(
                r#"
                SELECT max_alerts_per_month
                FROM iam.tenants
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

    /// Fetch all enabled monitored entities for a given tenant and asset symbol.
    /// Used to compute blast radius when a depeg alert fires.
    pub async fn find_monitored_entities_for_alert(
        &self,
        tenant_id: &str,
        asset_symbol: &str,
    ) -> Result<Vec<MonitoredEntityRow>> {
        let rows = match self
            .client
            .query(
                r#"
                SELECT entity_id, entity_type, display_name, chain_slug, asset_symbol,
                       quantity::float8, valuation_usd::float8, metadata_json
                FROM control.tenant_monitored_entities
                WHERE tenant_id = $1
                  AND asset_symbol = $2
                  AND enabled = TRUE
                "#,
                &[&tenant_id, &asset_symbol],
            )
            .await
        {
            Ok(v) => v,
            Err(error) => {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!("tenant_monitored_entities table not found; blast radius unavailable");
                    return Ok(vec![]);
                }
                return Err(error.into());
            }
        };

        Ok(rows
            .into_iter()
            .map(|row| MonitoredEntityRow {
                entity_id: row.get(0),
                entity_type: row.get(1),
                display_name: row.get(2),
                chain_slug: row.get(3),
                asset_symbol: row.get(4),
                quantity: row.get::<usize, Option<f64>>(5).unwrap_or(0.0),
                valuation_usd: row.get::<usize, Option<f64>>(6).unwrap_or(0.0),
                metadata_json: row.get(7),
            })
            .collect())
    }

    /// Persist computed blast radius exposure records for an incident.
    /// Uses ON CONFLICT to allow safe upserts when an incident escalates.
    pub async fn save_incident_entity_exposures(
        &self,
        incident_id: &str,
        tenant_id: &str,
        exposures: &[EntityExposureRecord],
    ) -> Result<()> {
        for exposure in exposures {
            if let Err(error) = self
                .client
                .execute(
                    r#"
                    INSERT INTO incident_entity_exposures
                        (incident_id, tenant_id, entity_id, capital_at_risk_usd,
                         liquidity_status, estimated_slippage_pct,
                         loss_scenario_5pct_usd, loss_scenario_10pct_usd,
                         estimated_savings_usd, payload)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                    ON CONFLICT (incident_id, entity_id) DO UPDATE
                    SET capital_at_risk_usd    = EXCLUDED.capital_at_risk_usd,
                        liquidity_status       = EXCLUDED.liquidity_status,
                        estimated_slippage_pct = EXCLUDED.estimated_slippage_pct,
                        loss_scenario_5pct_usd = EXCLUDED.loss_scenario_5pct_usd,
                        loss_scenario_10pct_usd = EXCLUDED.loss_scenario_10pct_usd,
                        estimated_savings_usd  = EXCLUDED.estimated_savings_usd,
                        payload                = EXCLUDED.payload
                    "#,
                    &[
                        &incident_id,
                        &tenant_id,
                        &exposure.entity_id,
                        &exposure.capital_at_risk_usd,
                        &exposure.liquidity_status,
                        &exposure.estimated_slippage_pct,
                        &exposure.loss_scenario_5pct_usd,
                        &exposure.loss_scenario_10pct_usd,
                        &exposure.estimated_savings_usd,
                        &exposure.payload,
                    ],
                )
                .await
            {
                if error.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    warn!(
                        "incident_entity_exposures table not found; skipping blast radius persist"
                    );
                    return Ok(());
                }
                common::log_error!(
                    warn,
                    error,
                    "failed to save incident entity exposure",
                    incident_id = %incident_id,
                    entity_id = %exposure.entity_id
                );
            }
        }
        Ok(())
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
