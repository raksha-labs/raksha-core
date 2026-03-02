use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::{start_health_check_server, ShutdownSignal};
use dotenvy::dotenv;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_postgres::{Client, NoTls, Row};
use tracing::{info, warn};

const DEFAULT_INTERVAL_SECS: u64 = 30;
const DEFAULT_BATCH_SIZE: i64 = 500;
const DEFAULT_TENANT_ID: &str = "glider";
const DEFAULT_HISTORY_ARCHIVE_DIR: &str = "history-archive";

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

    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL is required for history-worker")?;
    let interval_secs = std::env::var("HISTORY_WORKER_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    let run_once = std::env::var("HISTORY_WORKER_RUN_ONCE")
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    let batch_size = std::env::var("HISTORY_WORKER_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let history_bucket = std::env::var("HISTORY_BUCKET").ok();
    let history_prefix = std::env::var("HISTORY_PREFIX").unwrap_or_else(|_| "history".to_string());
    let simlab_tenant_id =
        std::env::var("SIMLAB_TENANT_ID").unwrap_or_else(|_| DEFAULT_TENANT_ID.to_string());
    let simlab_scenarios_dir = resolve_simlab_dir(
        "SIMLAB_SCENARIOS_DIR",
        &[
            "scenarios/historical",
            "../raksha-simlab/scenarios/historical",
            "../../raksha-simlab/scenarios/historical",
        ],
    );
    let simlab_curated_dir = resolve_simlab_dir(
        "SIMLAB_CURATED_DATASETS_DIR",
        &[
            "datasets/curated",
            "../raksha-simlab/datasets/curated",
            "../../raksha-simlab/datasets/curated",
        ],
    );
    let history_archive_dir = std::env::var("HISTORY_ARCHIVE_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(|| Some(PathBuf::from(DEFAULT_HISTORY_ARCHIVE_DIR)));

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
            format!("run_once={run_once}"),
            format!(
                "simlab_scenarios_dir={}",
                simlab_scenarios_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not-found".to_string())
            ),
            format!("simlab_tenant_id={simlab_tenant_id}"),
        ];
        health.is_ready = true;
    }

    info!(
        simlab_scenarios_dir = simlab_scenarios_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not-found".to_string()),
        "history-worker started"
    );

    while !shutdown.is_shutdown_requested() {
        let mut total_processed = 0usize;

        total_processed += sync_incidents(&client, batch_size).await?;
        total_processed += sync_alerts(&client, batch_size).await?;
        total_processed += sync_detections(&client, batch_size).await?;
        total_processed +=
            sync_feature_registry(&client, batch_size, history_bucket.clone(), &history_prefix)
                .await?;
        total_processed += sync_simlab_catalog(
            &client,
            simlab_scenarios_dir.as_deref(),
            simlab_curated_dir.as_deref(),
            history_archive_dir.as_deref(),
            history_bucket.as_deref(),
            &history_prefix,
            &simlab_tenant_id,
        )
        .await?;

        info!(
            processed_rows = total_processed,
            "history-worker iteration complete"
        );
        if run_once {
            break;
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }

    info!("history-worker shutdown complete");
    Ok(())
}

fn resolve_simlab_dir(env_key: &str, candidates: &[&str]) -> Option<PathBuf> {
    if let Ok(path) = std::env::var(env_key) {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
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
        ("history", "replay_catalog"),
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
        last_seen_ts: DateTime::<Utc>::from_timestamp(0, 0).expect("unix epoch should be valid"),
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
    let lowered = classification.to_ascii_lowercase();
    if lowered.contains("flash")
        || lowered.contains("oracle")
        || lowered.contains("exploit")
        || lowered.contains("loan")
    {
        return "exploit";
    }
    if lowered.contains("depeg")
        || lowered.contains("market")
        || lowered.contains("stress")
        || lowered.contains("liquidity")
    {
        return "market_stress";
    }
    "anomaly"
}

fn tenant_or_default(value: Option<String>) -> String {
    value.unwrap_or_else(|| DEFAULT_TENANT_ID.to_string())
}

fn parse_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc)),
        Some(Value::Number(n)) => {
            if let Some(ms) = n.as_i64() {
                if ms > 9_999_999_999 {
                    DateTime::<Utc>::from_timestamp_millis(ms)
                } else {
                    DateTime::<Utc>::from_timestamp(ms, 0)
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn discover_files_recursive(base_dir: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![base_dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(path = %current.display(), error = ?err, "failed to read directory during discovery");
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    warn!(error = ?err, "failed to read directory entry during discovery");
                    continue;
                }
            };
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if extensions
                    .iter()
                    .any(|candidate| ext.eq_ignore_ascii_case(candidate))
                {
                    files.push(path);
                }
            }
        }
    }

    files.sort();
    Ok(files)
}

fn load_scenario_file(path: &Path) -> Result<Value> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read simlab scenario file {}", path.display()))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let mut value = if ext == "yaml" || ext == "yml" {
        let yaml: serde_yaml::Value = serde_yaml::from_str(&content)
            .with_context(|| format!("invalid YAML scenario file {}", path.display()))?;
        serde_json::to_value(yaml).with_context(|| {
            format!("failed to convert YAML scenario to JSON {}", path.display())
        })?
    } else {
        serde_json::from_str::<Value>(&content)
            .with_context(|| format!("invalid JSON scenario file {}", path.display()))?
    };

    if !value.is_object() {
        anyhow::bail!("scenario file {} is not a JSON object", path.display());
    }

    // Ensure required keys are present to keep behavior aligned with simlab validation.
    let object = value.as_object().expect("checked object shape above");
    for required in ["scenario_id", "chain", "incident_class", "steps"] {
        if !object.contains_key(required) {
            anyhow::bail!(
                "scenario file {} is missing required key {}",
                path.display(),
                required
            );
        }
    }

    // Normalize missing optional arrays to avoid null handling everywhere.
    if let Some(obj) = value.as_object_mut() {
        obj.entry("source_refs")
            .or_insert_with(|| Value::Array(Vec::new()));
        obj.entry("tags")
            .or_insert_with(|| Value::Array(Vec::new()));
        obj.entry("steps")
            .or_insert_with(|| Value::Array(Vec::new()));
    }

    Ok(value)
}

fn load_curated_records(curated_dir: Option<&Path>) -> Result<Vec<Value>> {
    let Some(curated_dir) = curated_dir else {
        return Ok(Vec::new());
    };
    if !curated_dir.exists() {
        return Ok(Vec::new());
    }

    let files = discover_files_recursive(curated_dir, &["json"])?;
    let mut rows = Vec::new();
    for path in files {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                warn!(path = %path.display(), error = ?err, "failed to read curated dataset file");
                continue;
            }
        };
        let parsed: Value = match serde_json::from_str(&content) {
            Ok(parsed) => parsed,
            Err(err) => {
                warn!(path = %path.display(), error = ?err, "failed to parse curated dataset file");
                continue;
            }
        };
        match parsed {
            Value::Array(items) => {
                for item in items {
                    if item.is_object() {
                        rows.push(item);
                    }
                }
            }
            Value::Object(_) => rows.push(parsed),
            _ => {}
        }
    }

    Ok(rows)
}

fn extract_string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn scenario_tokens(
    scenario_id: &str,
    name: &str,
    protocol: &str,
    incident_class: &str,
) -> Vec<String> {
    let mut tokens = BTreeSet::new();
    for part in [scenario_id, name, protocol, incident_class] {
        let lowered = part.to_ascii_lowercase();
        if !lowered.is_empty() {
            tokens.insert(lowered.clone());
            for segment in lowered
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .filter(|segment| !segment.is_empty() && segment.len() > 2)
            {
                tokens.insert(segment.to_string());
            }
        }
    }
    tokens.into_iter().collect()
}

fn curated_record_matches(record: &Value, tokens: &[String]) -> bool {
    let Some(obj) = record.as_object() else {
        return false;
    };

    let mut haystack = String::new();
    for key in [
        "title",
        "name",
        "description",
        "line",
        "category",
        "chain",
        "url",
    ] {
        if let Some(value) = obj.get(key).and_then(Value::as_str) {
            haystack.push_str(&value.to_ascii_lowercase());
            haystack.push(' ');
        }
    }

    tokens.iter().any(|token| haystack.contains(token))
}

fn event_type_for_step(incident_class: &str, step: &Map<String, Value>) -> String {
    if let Some(event_type) = step.get("event_type").and_then(Value::as_str) {
        return event_type.to_string();
    }

    let normalized = incident_class.to_ascii_lowercase();
    if normalized.contains("flash") {
        return "flash_loan_candidate".to_string();
    }

    "oracle_update".to_string()
}

fn derive_supported_patterns(
    incident_class: &str,
    explicit_patterns: Option<&Value>,
) -> Vec<String> {
    let explicit = extract_string_array(explicit_patterns);
    if !explicit.is_empty() {
        return explicit;
    }

    let normalized = incident_class.to_ascii_lowercase();
    if normalized.contains("depeg") {
        return vec!["dpeg".to_string()];
    }
    if normalized.contains("oracle") {
        return vec!["oracle_manipulation".to_string(), "flash_loan".to_string()];
    }
    if normalized.contains("flash") || normalized.contains("exploit") {
        return vec!["flash_loan".to_string()];
    }
    vec!["anomaly".to_string()]
}

fn stable_checksum(value: &Value) -> String {
    let serialized = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    format!("{:x}", hasher.finalize())
}

fn checksum_short(value: &Value) -> String {
    stable_checksum(value).chars().take(16).collect()
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in input.chars() {
        let candidate = ch.to_ascii_lowercase();
        if candidate.is_ascii_alphanumeric() {
            out.push(candidate);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "scenario".to_string()
    } else {
        trimmed
    }
}

fn build_storage_prefix(
    history_prefix: &str,
    tenant_id: &str,
    slug: &str,
    dataset_version: &str,
) -> String {
    format!(
        "{history_prefix}/entity=simlab_case_events/tenant_id={tenant_id}/scenario_id={slug}/dataset_version={dataset_version}"
    )
}

fn persist_long_term_artifacts(
    archive_dir: Option<&Path>,
    storage_prefix: &str,
    scenario_payload: &Value,
    events: &[Value],
    manifest: &Value,
) -> Result<Option<String>> {
    let Some(archive_dir) = archive_dir else {
        return Ok(None);
    };

    let target_dir = archive_dir.join(storage_prefix);
    fs::create_dir_all(&target_dir).with_context(|| {
        format!(
            "failed to create history archive directory {}",
            target_dir.display()
        )
    })?;

    let scenario_path = target_dir.join("scenario.json");
    fs::write(
        &scenario_path,
        serde_json::to_vec_pretty(scenario_payload)
            .context("failed to serialize scenario payload for archive")?,
    )
    .with_context(|| format!("failed to write {}", scenario_path.display()))?;

    let events_path = target_dir.join("events.ndjson");
    let mut ndjson = String::new();
    for event in events {
        ndjson.push_str(&serde_json::to_string(event).context("failed to serialize event")?);
        ndjson.push('\n');
    }
    fs::write(&events_path, ndjson)
        .with_context(|| format!("failed to write {}", events_path.display()))?;

    let manifest_path = target_dir.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(manifest).context("failed to serialize archive manifest")?,
    )
    .with_context(|| format!("failed to write {}", manifest_path.display()))?;

    Ok(Some(target_dir.display().to_string()))
}

async fn sync_simlab_catalog(
    client: &Client,
    scenarios_dir: Option<&Path>,
    curated_dir: Option<&Path>,
    archive_dir: Option<&Path>,
    history_bucket: Option<&str>,
    history_prefix: &str,
    simlab_tenant_id: &str,
) -> Result<usize> {
    let Some(scenarios_dir) = scenarios_dir else {
        return Ok(0);
    };
    if !scenarios_dir.exists() {
        return Ok(0);
    }

    let scenario_files = discover_files_recursive(scenarios_dir, &["yaml", "yml", "json"])?;
    if scenario_files.is_empty() {
        return Ok(0);
    }

    let curated_records = load_curated_records(curated_dir)?;
    let mut processed = 0usize;

    for file in scenario_files {
        let scenario_value = match load_scenario_file(&file) {
            Ok(value) => value,
            Err(err) => {
                warn!(path = %file.display(), error = ?err, "skipping invalid simlab scenario");
                continue;
            }
        };

        let scenario_obj = match scenario_value.as_object() {
            Some(obj) => obj,
            None => continue,
        };

        let scenario_id = scenario_obj
            .get("scenario_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown-scenario")
            .to_string();
        let dataset_version = scenario_obj
            .get("dataset_version")
            .and_then(Value::as_str)
            .unwrap_or("v1")
            .to_string();
        let case_id = format!("simlab:{scenario_id}");
        let catalog_scenario_id = format!("simlab:{scenario_id}:{dataset_version}");
        let name = scenario_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&scenario_id)
            .to_string();
        let chain_slug = scenario_obj
            .get("chain_slug")
            .or_else(|| scenario_obj.get("chain"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let protocol = scenario_obj
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let protocol_category = scenario_obj
            .get("protocol_category")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let incident_class = scenario_obj
            .get("incident_class")
            .and_then(Value::as_str)
            .unwrap_or("anomaly")
            .to_string();
        let category = normalize_case_type(&incident_class).to_string();
        let severity = scenario_obj
            .get("severity_peak")
            .and_then(Value::as_str)
            .unwrap_or("critical")
            .to_string();
        let default_speed = scenario_obj
            .get("default_speed")
            .and_then(Value::as_i64)
            .unwrap_or(10)
            .max(1);
        let summary = scenario_obj
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("Historical simulation scenario curated from SimLab inputs.")
            .to_string();
        let losses_estimate = scenario_obj
            .get("losses_usd_estimate")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                scenario_obj
                    .get("loss_usd_estimate")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            });
        let source_confidence = scenario_obj
            .get("source_confidence")
            .and_then(Value::as_f64)
            .unwrap_or(0.75_f64);

        let base_start = parse_datetime(
            scenario_obj
                .get("default_time_window_start")
                .or_else(|| scenario_obj.get("incident_start_at"))
                .or_else(|| {
                    scenario_obj
                        .get("fork")
                        .and_then(Value::as_object)
                        .and_then(|fork| fork.get("block_timestamp"))
                }),
        )
        .unwrap_or_else(Utc::now);

        let steps = scenario_obj
            .get("steps")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let default_end = parse_datetime(
            scenario_obj
                .get("default_time_window_end")
                .or_else(|| scenario_obj.get("incident_end_at")),
        )
        .unwrap_or_else(|| base_start + ChronoDuration::seconds((steps.len().max(1) * 60) as i64));

        let tags = {
            let mut tags = extract_string_array(scenario_obj.get("tags"));
            tags.push(chain_slug.clone());
            tags.push(protocol.clone());
            tags.push(incident_class.clone());
            let mut dedup = BTreeSet::new();
            for tag in tags {
                if !tag.trim().is_empty() {
                    dedup.insert(tag);
                }
            }
            dedup.into_iter().collect::<Vec<String>>()
        };

        let supported_patterns =
            derive_supported_patterns(&incident_class, scenario_obj.get("supported_patterns"));
        let supported_override_keys = {
            let explicit = extract_string_array(scenario_obj.get("supported_override_keys"));
            if explicit.is_empty() {
                vec![
                    "speed".to_string(),
                    "time_window".to_string(),
                    "pattern_overrides".to_string(),
                    "source_overrides".to_string(),
                ]
            } else {
                explicit
            }
        };

        let tokens = scenario_tokens(&scenario_id, &name, &protocol, &incident_class);
        let matched_sources: Vec<Value> = curated_records
            .iter()
            .filter(|record| curated_record_matches(record, &tokens))
            .cloned()
            .collect();

        let references = {
            let mut refs = Vec::new();
            for source_ref in extract_string_array(scenario_obj.get("source_refs")) {
                refs.push(json!({ "type": "source_ref", "url": source_ref }));
            }
            for record in &matched_sources {
                refs.push(json!({ "type": "curated_source", "payload": record }));
            }
            Value::Array(refs)
        };

        let mut source_ids = BTreeSet::new();
        let mut timeline_rows = Vec::new();
        let mut event_payloads = Vec::new();
        let mut expected_alerts = scenario_obj
            .get("expected_alerts")
            .filter(|value| value.is_array())
            .cloned()
            .unwrap_or_else(|| {
                Value::Array(vec![json!({
                    "pattern_id": supported_patterns.first().cloned().unwrap_or_else(|| "anomaly".to_string()),
                    "severity": severity,
                    "title": format!("{} replay expected alert", name),
                    "delivery_status": "blocked_simulation"
                })])
            });

        if !expected_alerts.is_array() {
            expected_alerts = Value::Array(Vec::new());
        }

        for (idx, step) in steps.iter().enumerate() {
            let Some(step_obj) = step.as_object() else {
                continue;
            };
            let step_type = step_obj
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(step_type, "replay_tx" | "simulate_call" | "mutate_param") {
                continue;
            }

            let source_id = step_obj
                .get("source_id")
                .and_then(Value::as_str)
                .unwrap_or("simlab")
                .to_string();
            source_ids.insert(source_id.clone());

            let event_type = event_type_for_step(&incident_class, step_obj);
            let event_ts = base_start + ChronoDuration::seconds(idx as i64);
            let tx_hash = step_obj
                .get("tx_hash")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let log_index = step_obj
                .get("log_index")
                .and_then(Value::as_i64)
                .unwrap_or(idx as i64);

            let mut payload = step.clone();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "scenario_id".to_string(),
                    Value::String(scenario_id.clone()),
                );
                obj.insert(
                    "dataset_version".to_string(),
                    Value::String(dataset_version.clone()),
                );
                obj.insert(
                    "scenario_step".to_string(),
                    Value::Number((idx as i64).into()),
                );
                obj.insert(
                    "event_type_resolved".to_string(),
                    Value::String(event_type.clone()),
                );
                obj.insert(
                    "source_file".to_string(),
                    Value::String(file.display().to_string()),
                );
            }

            timeline_rows.push(json!({
                "index": idx,
                "type": step_type,
                "event_type": event_type,
                "event_ts": event_ts,
                "source_id": source_id,
                "tx_hash": tx_hash,
                "log_index": log_index,
            }));

            event_payloads.push(json!({
                "event_ts": event_ts,
                "event_type": event_type,
                "source_pk": format!("{scenario_id}:{dataset_version}:{idx}"),
                "payload": payload,
            }));
        }

        // Persist curated source records as immutable timeline entries so research data is replay-traceable.
        for (idx, record) in matched_sources.iter().enumerate() {
            event_payloads.push(json!({
                "event_ts": base_start - ChronoDuration::seconds((idx + 1) as i64),
                "event_type": "source_reference",
                "source_pk": format!("{scenario_id}:{dataset_version}:src:{}", checksum_short(record)),
                "payload": json!({ "record": record, "source": "curated_dataset" }),
            }));
        }

        event_payloads.sort_by_key(|item| {
            item.get("event_ts")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        });

        let scenario_checksum = stable_checksum(&scenario_value);
        let slug = slugify(&scenario_id.replace('_', "-"));
        let storage_prefix =
            build_storage_prefix(history_prefix, simlab_tenant_id, &slug, &dataset_version);
        let object_prefix = history_bucket
            .map(|bucket| format!("s3://{bucket}/{storage_prefix}"))
            .unwrap_or_else(|| storage_prefix.clone());

        let source_payload = json!({
            "source": "simlab",
            "scenario_file": file.display().to_string(),
            "scenario": scenario_value,
            "matched_curated_records": matched_sources,
            "ingested_at": Utc::now(),
        });

        client
            .execute(
                r#"
                INSERT INTO history.cases (
                  case_id, tenant_id, case_type, classification, severity_peak, status,
                  chain_slug, protocol, title, summary, incident_start_at, incident_end_at,
                  loss_usd_estimate, source_confidence, source_payload, created_at, updated_at
                )
                VALUES (
                  $1, $2, $3, $4, $5, 'cataloged',
                  $6, $7, $8, $9, $10, $11,
                  $12, $13, $14, NOW(), NOW()
                )
                ON CONFLICT (case_id) DO UPDATE
                SET tenant_id = EXCLUDED.tenant_id,
                    case_type = EXCLUDED.case_type,
                    classification = EXCLUDED.classification,
                    severity_peak = EXCLUDED.severity_peak,
                    chain_slug = EXCLUDED.chain_slug,
                    protocol = EXCLUDED.protocol,
                    title = EXCLUDED.title,
                    summary = EXCLUDED.summary,
                    incident_start_at = EXCLUDED.incident_start_at,
                    incident_end_at = EXCLUDED.incident_end_at,
                    loss_usd_estimate = EXCLUDED.loss_usd_estimate,
                    source_confidence = EXCLUDED.source_confidence,
                    source_payload = EXCLUDED.source_payload,
                    updated_at = NOW()
                "#,
                &[
                    &case_id,
                    &simlab_tenant_id,
                    &category,
                    &incident_class,
                    &severity,
                    &chain_slug,
                    &protocol,
                    &name,
                    &summary,
                    &base_start,
                    &default_end,
                    &losses_estimate,
                    &source_confidence,
                    &source_payload,
                ],
            )
            .await?;

        for row in &event_payloads {
            let event_ts = parse_datetime(row.get("event_ts")).unwrap_or_else(Utc::now);
            let event_type = row
                .get("event_type")
                .and_then(Value::as_str)
                .unwrap_or("replay_event")
                .to_string();
            let source_pk = row
                .get("source_pk")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let payload = row.get("payload").cloned().unwrap_or_else(|| json!({}));

            client
                .execute(
                    r#"
                    INSERT INTO history.case_events (
                      case_id, tenant_id, event_type, event_ts, source_table, source_pk, payload_json
                    )
                    VALUES ($1, $2, $3, $4, 'simlab_scenario_step', $5, $6)
                    ON CONFLICT (source_table, source_pk) DO UPDATE
                    SET case_id = EXCLUDED.case_id,
                        tenant_id = EXCLUDED.tenant_id,
                        event_type = EXCLUDED.event_type,
                        event_ts = EXCLUDED.event_ts,
                        payload_json = EXCLUDED.payload_json,
                        ingested_at = NOW()
                    "#,
                    &[&case_id, &simlab_tenant_id, &event_type, &event_ts, &source_pk, &payload],
                )
                .await?;
        }

        let source_feeds = Value::Array(
            source_ids
                .iter()
                .map(|source_id| json!({ "source_id": source_id }))
                .collect(),
        );

        client
            .execute(
                r#"
                INSERT INTO history.replay_catalog (
                  scenario_id, tenant_id, case_id, slug, title, category,
                  tags, incident_class, chain, protocol, protocol_category,
                  description, impact_summary, losses_usd_estimate, attack_vector,
                  detection_focus, default_time_window_start, default_time_window_end,
                  default_speed, supported_patterns, supported_override_keys,
                  baseline_expected_alerts, expected_alerts_json, timeline_json,
                  references_json, source_feeds_json, runbook_notes,
                  dataset_version, object_prefix, checksum, simlab_scenario_id,
                  is_active, created_at, updated_at
                )
                VALUES (
                  $1, $2, $3, $4, $5, $6,
                  $7, $8, $9, $10, $11,
                  $12, $13, $14, $15,
                  $16, $17, $18,
                  $19, $20, $21,
                  $22, $23, $24,
                  $25, $26, $27,
                  $28, $29, $30, $31,
                  TRUE, NOW(), NOW()
                )
                ON CONFLICT (scenario_id) DO UPDATE
                SET tenant_id = EXCLUDED.tenant_id,
                    case_id = EXCLUDED.case_id,
                    slug = EXCLUDED.slug,
                    title = EXCLUDED.title,
                    category = EXCLUDED.category,
                    tags = EXCLUDED.tags,
                    incident_class = EXCLUDED.incident_class,
                    chain = EXCLUDED.chain,
                    protocol = EXCLUDED.protocol,
                    protocol_category = EXCLUDED.protocol_category,
                    description = EXCLUDED.description,
                    impact_summary = EXCLUDED.impact_summary,
                    losses_usd_estimate = EXCLUDED.losses_usd_estimate,
                    attack_vector = EXCLUDED.attack_vector,
                    detection_focus = EXCLUDED.detection_focus,
                    default_time_window_start = EXCLUDED.default_time_window_start,
                    default_time_window_end = EXCLUDED.default_time_window_end,
                    default_speed = EXCLUDED.default_speed,
                    supported_patterns = EXCLUDED.supported_patterns,
                    supported_override_keys = EXCLUDED.supported_override_keys,
                    baseline_expected_alerts = EXCLUDED.baseline_expected_alerts,
                    expected_alerts_json = EXCLUDED.expected_alerts_json,
                    timeline_json = EXCLUDED.timeline_json,
                    references_json = EXCLUDED.references_json,
                    source_feeds_json = EXCLUDED.source_feeds_json,
                    runbook_notes = EXCLUDED.runbook_notes,
                    dataset_version = EXCLUDED.dataset_version,
                    object_prefix = EXCLUDED.object_prefix,
                    checksum = EXCLUDED.checksum,
                    simlab_scenario_id = EXCLUDED.simlab_scenario_id,
                    is_active = TRUE,
                    updated_at = NOW()
                "#,
                &[
                    &catalog_scenario_id,
                    &simlab_tenant_id,
                    &case_id,
                    &slug,
                    &name,
                    &category,
                    &tags,
                    &incident_class,
                    &chain_slug,
                    &protocol,
                    &protocol_category,
                    &summary,
                    &scenario_obj
                        .get("impact_summary")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    &losses_estimate,
                    &scenario_obj
                        .get("attack_vector")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    &scenario_obj
                        .get("detection_focus")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    &base_start,
                    &default_end,
                    &default_speed,
                    &supported_patterns,
                    &supported_override_keys,
                    &(expected_alerts
                        .as_array()
                        .map(|rows| rows.len() as i32)
                        .unwrap_or(0)),
                    &expected_alerts,
                    &Value::Array(timeline_rows.clone()),
                    &references,
                    &source_feeds,
                    &extract_string_array(scenario_obj.get("runbook_notes")),
                    &dataset_version,
                    &object_prefix,
                    &scenario_checksum,
                    &scenario_id,
                ],
            )
            .await?;

        let export_manifest = json!({
            "scenario_id": scenario_id,
            "dataset_version": dataset_version,
            "case_id": case_id,
            "checksum": scenario_checksum,
            "events": event_payloads.len(),
            "expected_alerts": expected_alerts,
            "generated_at": Utc::now(),
            "storage_prefix": storage_prefix,
        });

        let _archive_path = persist_long_term_artifacts(
            archive_dir,
            &storage_prefix,
            &source_payload,
            &event_payloads,
            &export_manifest,
        )?;

        processed += 1;
    }

    Ok(processed)
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
