use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use common::{start_health_check_server, ShutdownSignal};
use dotenvy::dotenv;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_postgres::{Client, NoTls, Row};
use tracing::info;

const DEFAULT_INTERVAL_SECS: u64 = 30;
const DEFAULT_BATCH_SIZE: i64 = 500;
const DEFAULT_TENANT_ID: &str = "glider";

#[derive(Clone, Debug)]
struct Offset {
    last_seen_ts: DateTime<Utc>,
    last_seen_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .compact()
        .init();

    let health_status = start_health_check_server("history-worker");
    let shutdown = ShutdownSignal::install();

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is required for history-worker")?;
    let interval_secs = std::env::var("HISTORY_WORKER_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    let batch_size = std::env::var("HISTORY_WORKER_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let history_bucket = std::env::var("HISTORY_BUCKET").ok();
    let history_prefix = std::env::var("HISTORY_PREFIX").unwrap_or_else(|_| "history".to_string());

    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            tracing::error!(error = ?err, "history-worker postgres background connection error");
        }
    });

    validate_schema(&client).await?;

    if let Some(status) = health_status.as_ref() {
        let mut health = status.write().await;
        health.postgres_connected = true;
        health.redis_connected = false;
        health.details = vec![
            format!("interval_secs={interval_secs}"),
            format!("batch_size={batch_size}"),
            format!("history_prefix={history_prefix}"),
        ];
        health.is_ready = true;
    }

    info!("history-worker started");

    while !shutdown.is_shutdown_requested() {
        let mut total_processed = 0usize;

        total_processed += sync_incidents(&client, batch_size).await?;
        total_processed += sync_alerts(&client, batch_size).await?;
        total_processed += sync_detections(&client, batch_size).await?;
        total_processed += sync_feature_registry(&client, batch_size, history_bucket.clone(), &history_prefix).await?;

        info!(processed_rows = total_processed, "history-worker iteration complete");
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }

    info!("history-worker shutdown complete");
    Ok(())
}

async fn validate_schema(client: &Client) -> Result<()> {
    let required = [
        ("public", "incidents"),
        ("public", "alerts"),
        ("public", "detections"),
        ("public", "feature_vectors"),
        ("history", "cases"),
        ("history", "case_events"),
        ("history", "case_alert_links"),
        ("history", "ml_feature_registry"),
        ("history", "ingest_offsets"),
    ];

    for (schema, table) in required {
        let exists = client
            .query_opt(
                r#"
                SELECT 1
                FROM information_schema.tables
                WHERE table_schema = $1
                  AND table_name = $2
                "#,
                &[&schema, &table],
            )
            .await?;
        if exists.is_none() {
            anyhow::bail!(
                "missing required table {}.{}. Apply schema.sql / upgrade_history_intelligence.sql first",
                schema,
                table
            );
        }
    }

    Ok(())
}

async fn load_offset(client: &Client, source_name: &str) -> Result<Offset> {
    let row = client
        .query_opt(
            r#"
            SELECT last_seen_ts, last_seen_id
            FROM history.ingest_offsets
            WHERE source_name = $1
            "#,
            &[&source_name],
        )
        .await?;

    if let Some(row) = row {
        let ts: DateTime<Utc> = row.get(0);
        let id: String = row.get(1);
        return Ok(Offset {
            last_seen_ts: ts,
            last_seen_id: id,
        });
    }

    Ok(Offset {
        last_seen_ts: DateTime::<Utc>::from_timestamp(0, 0)
            .expect("unix epoch should be valid"),
        last_seen_id: String::new(),
    })
}

async fn save_offset(client: &Client, source_name: &str, offset: &Offset) -> Result<()> {
    client
        .execute(
            r#"
            INSERT INTO history.ingest_offsets (source_name, last_seen_ts, last_seen_id, updated_at)
            VALUES ($1, $2, $3, NOW())
            ON CONFLICT (source_name) DO UPDATE
            SET last_seen_ts = EXCLUDED.last_seen_ts,
                last_seen_id = EXCLUDED.last_seen_id,
                updated_at = NOW()
            "#,
            &[&source_name, &offset.last_seen_ts, &offset.last_seen_id],
        )
        .await?;
    Ok(())
}

fn normalize_case_type(classification: &str) -> &'static str {
    if classification.contains("flash_loan") || classification.contains("oracle") {
        return "exploit";
    }
    if classification.contains("depeg") || classification.contains("liquidity") {
        return "market_stress";
    }
    "anomaly"
}

fn tenant_or_default(value: Option<String>) -> String {
    value.unwrap_or_else(|| DEFAULT_TENANT_ID.to_string())
}

async fn sync_incidents(client: &Client, batch_size: i64) -> Result<usize> {
    let source = "incidents";
    let mut offset = load_offset(client, source).await?;

    let rows = client
        .query(
            r#"
            SELECT
              incident_id,
              tenant_id,
              pattern_id,
              chain_slug,
              status,
              current_severity,
              opened_at,
              updated_at
            FROM incidents
            WHERE (
              updated_at > $1
              OR (updated_at = $1 AND incident_id > $2)
            )
            ORDER BY updated_at ASC, incident_id ASC
            LIMIT $3
            "#,
            &[&offset.last_seen_ts, &offset.last_seen_id, &batch_size],
        )
        .await?;

    for row in &rows {
        let incident_id: String = row.get("incident_id");
        let tenant_id = tenant_or_default(row.get("tenant_id"));
        let pattern_id: String = row.get("pattern_id");
        let chain_slug: String = row.get("chain_slug");
        let status: String = row.get("status");
        let severity: String = row.get("current_severity");
        let opened_at: DateTime<Utc> = row.get("opened_at");
        let updated_at: DateTime<Utc> = row.get("updated_at");

        let case_type = normalize_case_type(&pattern_id);
        let title = format!("{} incident {}", pattern_id.replace('_', " "), incident_id);
        let summary = "Curated incident from operational lifecycle tables.";

        client
            .execute(
                r#"
                INSERT INTO history.cases (
                  case_id, tenant_id, case_type, classification, severity_peak, status,
                  chain_slug, protocol, title, summary, incident_start_at, incident_end_at,
                  source_payload, created_at, updated_at
                )
                VALUES (
                  $1, $2, $3, $4, $5, $6,
                  $7, NULL, $8, $9, $10, NULL,
                  $11, NOW(), NOW()
                )
                ON CONFLICT (case_id) DO UPDATE
                SET tenant_id = EXCLUDED.tenant_id,
                    case_type = EXCLUDED.case_type,
                    classification = EXCLUDED.classification,
                    severity_peak = EXCLUDED.severity_peak,
                    status = EXCLUDED.status,
                    chain_slug = EXCLUDED.chain_slug,
                    title = EXCLUDED.title,
                    summary = EXCLUDED.summary,
                    incident_start_at = EXCLUDED.incident_start_at,
                    updated_at = NOW()
                "#,
                &[
                    &incident_id,
                    &tenant_id,
                    &case_type,
                    &pattern_id,
                    &severity,
                    &status,
                    &chain_slug,
                    &title,
                    &summary,
                    &opened_at,
                    &json!({ "source_table": "incidents", "pattern_id": pattern_id }),
                ],
            )
            .await?;

        let payload = row_to_json(row);
        let source_pk = incident_id.clone();
        client
            .execute(
                r#"
                INSERT INTO history.case_events (
                  case_id, tenant_id, event_type, event_ts, source_table, source_pk, payload_json
                )
                VALUES ($1, $2, 'incident_snapshot', $3, 'incidents', $4, $5)
                ON CONFLICT (source_table, source_pk) DO NOTHING
                "#,
                &[&incident_id, &tenant_id, &updated_at, &source_pk, &payload],
            )
            .await?;

        offset.last_seen_ts = updated_at;
        offset.last_seen_id = incident_id;
    }

    if !rows.is_empty() {
        save_offset(client, source, &offset).await?;
    }

    Ok(rows.len())
}

async fn sync_alerts(client: &Client, batch_size: i64) -> Result<usize> {
    let source = "alerts";
    let mut offset = load_offset(client, source).await?;

    let rows = client
        .query(
            r#"
            SELECT
              id,
              incident_id,
              tenant_id,
              pattern_id,
              severity,
              lifecycle_state,
              created_at,
              payload
            FROM alerts
            WHERE (
              created_at > $1
              OR (created_at = $1 AND id > $2)
            )
            ORDER BY created_at ASC, id ASC
            LIMIT $3
            "#,
            &[&offset.last_seen_ts, &offset.last_seen_id, &batch_size],
        )
        .await?;

    for row in &rows {
        let alert_id: String = row.get("id");
        let incident_id: Option<String> = row.get("incident_id");
        let tenant_id = tenant_or_default(row.get("tenant_id"));
        let pattern_id: Option<String> = row.get("pattern_id");
        let severity: String = row.get("severity");
        let lifecycle_state: String = row.get("lifecycle_state");
        let created_at: DateTime<Utc> = row.get("created_at");
        let payload: Value = row.get("payload");

        let case_id = incident_id
            .clone()
            .unwrap_or_else(|| format!("alert:{alert_id}"));
        let classification = pattern_id.clone().unwrap_or_else(|| "anomaly".to_string());
        let case_type = normalize_case_type(&classification);

        client
            .execute(
                r#"
                INSERT INTO history.cases (
                  case_id, tenant_id, case_type, classification, severity_peak, status,
                  title, summary, incident_start_at, source_payload, created_at, updated_at
                )
                VALUES (
                  $1, $2, $3, $4, $5, $6,
                  $7, $8, $9, $10, NOW(), NOW()
                )
                ON CONFLICT (case_id) DO UPDATE
                SET severity_peak = EXCLUDED.severity_peak,
                    status = EXCLUDED.status,
                    updated_at = NOW()
                "#,
                &[
                    &case_id,
                    &tenant_id,
                    &case_type,
                    &classification,
                    &severity,
                    &lifecycle_state,
                    &format!("Alert case {}", case_id),
                    &"Case generated from alerts stream".to_string(),
                    &created_at,
                    &json!({ "source_table": "alerts" }),
                ],
            )
            .await?;

        client
            .execute(
                r#"
                INSERT INTO history.case_alert_links (
                  case_id, tenant_id, alert_id, incident_id, pattern_id, severity, delivery_status, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (case_id, alert_id) DO UPDATE
                SET severity = EXCLUDED.severity,
                    delivery_status = EXCLUDED.delivery_status
                "#,
                &[
                    &case_id,
                    &tenant_id,
                    &alert_id,
                    &incident_id,
                    &pattern_id,
                    &severity,
                    &lifecycle_state,
                    &created_at,
                ],
            )
            .await?;

        let source_pk = alert_id.clone();
        client
            .execute(
                r#"
                INSERT INTO history.case_events (
                  case_id, tenant_id, event_type, event_ts, source_table, source_pk, payload_json
                )
                VALUES ($1, $2, 'alert_emitted', $3, 'alerts', $4, $5)
                ON CONFLICT (source_table, source_pk) DO NOTHING
                "#,
                &[&case_id, &tenant_id, &created_at, &source_pk, &payload],
            )
            .await?;

        offset.last_seen_ts = created_at;
        offset.last_seen_id = alert_id;
    }

    if !rows.is_empty() {
        save_offset(client, source, &offset).await?;
    }

    Ok(rows.len())
}

async fn sync_detections(client: &Client, batch_size: i64) -> Result<usize> {
    let source = "detections";
    let mut offset = load_offset(client, source).await?;

    let rows = client
        .query(
            r#"
            SELECT
              id,
              tenant_id,
              pattern_id,
              chain,
              protocol,
              severity,
              created_at,
              payload
            FROM detections
            WHERE (
              created_at > $1
              OR (created_at = $1 AND id > $2)
            )
            ORDER BY created_at ASC, id ASC
            LIMIT $3
            "#,
            &[&offset.last_seen_ts, &offset.last_seen_id, &batch_size],
        )
        .await?;

    for row in &rows {
        let detection_id: String = row.get("id");
        let tenant_id = tenant_or_default(row.get("tenant_id"));
        let pattern_id: Option<String> = row.get("pattern_id");
        let chain: String = row.get("chain");
        let protocol: String = row.get("protocol");
        let severity: String = row.get("severity");
        let created_at: DateTime<Utc> = row.get("created_at");
        let payload: Value = row.get("payload");

        let case_id = payload
            .get("incident_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("detection:{detection_id}"));
        let classification = pattern_id.clone().unwrap_or_else(|| "anomaly".to_string());
        let case_type = normalize_case_type(&classification);

        client
            .execute(
                r#"
                INSERT INTO history.cases (
                  case_id, tenant_id, case_type, classification, severity_peak, status,
                  chain_slug, protocol, title, summary, incident_start_at, source_payload, created_at, updated_at
                )
                VALUES (
                  $1, $2, $3, $4, $5, 'open',
                  $6, $7, $8, $9, $10, $11, NOW(), NOW()
                )
                ON CONFLICT (case_id) DO UPDATE
                SET severity_peak = EXCLUDED.severity_peak,
                    chain_slug = COALESCE(history.cases.chain_slug, EXCLUDED.chain_slug),
                    protocol = COALESCE(history.cases.protocol, EXCLUDED.protocol),
                    updated_at = NOW()
                "#,
                &[
                    &case_id,
                    &tenant_id,
                    &case_type,
                    &classification,
                    &severity,
                    &chain,
                    &protocol,
                    &format!("Detection case {}", case_id),
                    &"Case generated from detections stream".to_string(),
                    &created_at,
                    &json!({ "source_table": "detections" }),
                ],
            )
            .await?;

        let source_pk = detection_id.clone();
        client
            .execute(
                r#"
                INSERT INTO history.case_events (
                  case_id, tenant_id, event_type, event_ts, source_table, source_pk, payload_json
                )
                VALUES ($1, $2, 'detection_emitted', $3, 'detections', $4, $5)
                ON CONFLICT (source_table, source_pk) DO NOTHING
                "#,
                &[&case_id, &tenant_id, &created_at, &source_pk, &payload],
            )
            .await?;

        offset.last_seen_ts = created_at;
        offset.last_seen_id = detection_id;
    }

    if !rows.is_empty() {
        save_offset(client, source, &offset).await?;
    }

    Ok(rows.len())
}

async fn sync_feature_registry(
    client: &Client,
    batch_size: i64,
    history_bucket: Option<String>,
    history_prefix: &str,
) -> Result<usize> {
    let source = "feature_vectors";
    let mut offset = load_offset(client, source).await?;

    let rows = client
        .query(
            r#"
            SELECT
              id,
              detection_id,
              tenant_id,
              feature_set_version,
              labels,
              created_at
            FROM feature_vectors
            WHERE (
              created_at > $1
              OR (created_at = $1 AND id::text > $2)
            )
            ORDER BY created_at ASC, id ASC
            LIMIT $3
            "#,
            &[&offset.last_seen_ts, &offset.last_seen_id, &batch_size],
        )
        .await?;

    for row in &rows {
        let id: i64 = row.get("id");
        let detection_id: String = row.get("detection_id");
        let tenant_id = tenant_or_default(row.get("tenant_id"));
        let feature_set_version: String = row.get("feature_set_version");
        let labels: Value = row.get("labels");
        let created_at: DateTime<Utc> = row.get("created_at");
        let case_id = format!("detection:{detection_id}");
        let feature_slice_id = format!("fv:{id}");

        let s3_uri = if let Some(bucket) = history_bucket.as_ref() {
            format!(
                "s3://{bucket}/{history_prefix}/entity=features/feature_set_version={feature_set_version}/tenant_id={tenant_id}/case_type=anomaly/{feature_slice_id}.parquet"
            )
        } else {
            format!(
                "file://history/entity=features/feature_set_version={feature_set_version}/tenant_id={tenant_id}/{feature_slice_id}.parquet"
            )
        };

        client
            .execute(
                r#"
                INSERT INTO history.ml_feature_registry (
                  feature_slice_id, tenant_id, case_id, feature_set_version, label_set_version,
                  s3_uri, row_count, metadata_json, created_at
                )
                VALUES ($1, $2, $3, $4, 'v1', $5, 1, $6, $7)
                ON CONFLICT (feature_slice_id) DO NOTHING
                "#,
                &[
                    &feature_slice_id,
                    &tenant_id,
                    &case_id,
                    &feature_set_version,
                    &s3_uri,
                    &json!({ "labels": labels, "detection_id": detection_id }),
                    &created_at,
                ],
            )
            .await?;

        offset.last_seen_ts = created_at;
        offset.last_seen_id = id.to_string();
    }

    if !rows.is_empty() {
        save_offset(client, source, &offset).await?;
    }

    Ok(rows.len())
}

fn row_to_json(row: &Row) -> Value {
    json!({
        "incident_id": row.get::<_, String>("incident_id"),
        "tenant_id": row.get::<_, Option<String>>("tenant_id"),
        "pattern_id": row.get::<_, String>("pattern_id"),
        "chain_slug": row.get::<_, String>("chain_slug"),
        "status": row.get::<_, String>("status"),
        "current_severity": row.get::<_, String>("current_severity"),
        "opened_at": row.get::<_, DateTime<Utc>>("opened_at"),
        "updated_at": row.get::<_, DateTime<Utc>>("updated_at")
    })
}
