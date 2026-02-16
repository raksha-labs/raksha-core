use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use common::ShutdownSignal;
use dotenv::dotenv;
use dpeg_engine::{
    evaluate_policy, ConsensusSnapshot, DpegAlertState, DpegPolicy, DpegSeverityBands,
    DpegSourceOverride, QuoteInput,
};
use event_schema::{
    AttackFamily, Chain, DetectionResult, DetectionSignal, LifecycleState, MarketConsensusSnapshot,
    RiskScore, Severity, SignalType,
};
use state_manager::{
    DpegPolicyConfigRow, DpegPolicySourceConfigRow, PostgresRepository, RedisStreamPublisher,
};
use tracing::{info, warn};
use uuid::Uuid;

const DEFAULT_BATCH_SIZE: usize = 100;
const DEFAULT_BLOCK_MS: usize = 1000;

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

    if !env_bool("MARKET_DPEG_ENABLED", true) {
        info!("market-detector disabled by MARKET_DPEG_ENABLED flag");
        return Ok(());
    }

    let emit_alerts = env_bool("DPEG_ALERTS_EMIT_ENABLED", true);

    let Some(stream) = init_stream_publisher().await else {
        warn!("market-detector requires REDIS_URL");
        return Ok(());
    };
    let Some(repo) = init_repository().await else {
        warn!("market-detector requires DATABASE_URL");
        return Ok(());
    };

    let batch_size = std::env::var("MARKET_DETECTOR_STREAM_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let block_ms = std::env::var("MARKET_DETECTOR_STREAM_BLOCK_MS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BLOCK_MS);
    let group = std::env::var("MARKET_DETECTOR_STREAM_GROUP")
        .unwrap_or_else(|_| "market-detector-workers".to_string());
    let consumer = std::env::var("MARKET_DETECTOR_STREAM_CONSUMER")
        .unwrap_or_else(|_| default_consumer_name("market-detector"));

    stream.ensure_market_quotes_group(&group).await?;

    let shutdown = ShutdownSignal::install();

    let mut policies = load_policies(&repo).await?;
    let mut last_policy_reload = Utc::now();

    let mut quote_cache: HashMap<(String, String), HashMap<String, QuoteInput>> = HashMap::new();
    let mut state_cache: HashMap<(String, String), DpegAlertState> = HashMap::new();

    info!(policy_count = policies.len(), emit_alerts, "market-detector started");

    while !shutdown.is_shutdown_requested() {
        if Utc::now().signed_duration_since(last_policy_reload).num_seconds() >= 30 {
            match load_policies(&repo).await {
                Ok(reloaded) => {
                    policies = reloaded;
                    last_policy_reload = Utc::now();
                    info!(policy_count = policies.len(), "reloaded dpeg policies");
                }
                Err(err) => {
                    warn!(error = ?err, "failed to reload dpeg policies");
                }
            }
        }

        let entries = stream
            .read_market_quotes_group(&group, &consumer, batch_size, block_ms)
            .await?;

        if entries.is_empty() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        for (entry_id, quote_event) in entries {
            let policy_key = (quote_event.tenant_id.clone(), quote_event.market_key.clone());
            let Some(policy) = policies.get(&policy_key).cloned() else {
                stream.ack_market_quote(&group, &entry_id).await?;
                continue;
            };

            let market_quotes = quote_cache.entry(policy_key.clone()).or_default();
            market_quotes.insert(
                quote_event.source_id.clone(),
                QuoteInput {
                    source_id: quote_event.source_id.clone(),
                    price: quote_event.price,
                    observed_at: quote_event.observed_at,
                },
            );

            if !state_cache.contains_key(&policy_key) {
                let loaded = repo
                    .load_dpeg_alert_state(&policy_key.0, &policy_key.1)
                    .await?
                    .map(dpeg_state_from_row)
                    .unwrap_or_default();
                state_cache.insert(policy_key.clone(), loaded);
            }

            let current_state = state_cache
                .get(&policy_key)
                .cloned()
                .unwrap_or_default();

            let quotes: Vec<QuoteInput> = market_quotes.values().cloned().collect();
            let now = Utc::now();
            let outcome = evaluate_policy(&policy, &quotes, &current_state, now)?;

            let snapshot = snapshot_event_from_outcome(&policy, &outcome.snapshot, now);
            if let Err(err) = repo.save_market_snapshot(&snapshot).await {
                warn!(error = ?err, "failed to persist market snapshot");
            }
            if let Err(err) = stream.publish_market_snapshot(&snapshot).await {
                warn!(error = ?err, "failed to publish market snapshot");
            }

            state_cache.insert(policy_key.clone(), outcome.next_state.clone());
            let severity_raw = outcome
                .next_state
                .last_severity
                .as_ref()
                .map(|severity| format!("{:?}", severity).to_lowercase());
            if let Err(err) = repo
                .upsert_dpeg_alert_state(
                    &policy_key.0,
                    &policy_key.1,
                    outcome.next_state.breach_started_at,
                    outcome.next_state.cooldown_until,
                    outcome.next_state.last_alerted_at,
                    outcome.next_state.last_divergence_pct,
                    severity_raw.as_deref(),
                )
                .await
            {
                warn!(error = ?err, "failed to persist dpeg alert state");
            }

            if emit_alerts && outcome.should_emit_alert {
                if let Some(severity) = outcome.snapshot.severity.clone() {
                    let detection = detection_from_snapshot(&policy, &outcome.snapshot, severity, now);
                    if let Err(err) = repo.save_detection(&detection).await {
                        warn!(error = ?err, "failed to persist dpeg detection");
                    }
                    if let Err(err) = stream.publish_detection(&detection).await {
                        warn!(error = ?err, "failed to publish dpeg detection");
                    }
                }
            }

            stream.ack_market_quote(&group, &entry_id).await?;
        }
    }

    info!("market-detector stopping");
    Ok(())
}

async fn load_policies(
    repo: &PostgresRepository,
) -> Result<HashMap<(String, String), DpegPolicy>> {
    let policy_rows = repo.load_dpeg_policies().await?;
    let source_rows = repo.load_dpeg_policy_sources().await?;

    let mut source_map: HashMap<(String, String), Vec<DpegPolicySourceConfigRow>> = HashMap::new();
    for source in source_rows {
        source_map
            .entry((source.tenant_id.clone(), source.market_key.clone()))
            .or_default()
            .push(source);
    }

    let mut policies = HashMap::new();
    for row in policy_rows {
        let key = (row.tenant_id.clone(), row.market_key.clone());
        let overrides = source_map.get(&key).cloned().unwrap_or_default();
        let policy = policy_from_rows(row, overrides)?;
        policies.insert((policy.tenant_id.clone(), policy.market_key.clone()), policy);
    }

    Ok(policies)
}

fn policy_from_rows(
    row: DpegPolicyConfigRow,
    overrides: Vec<DpegPolicySourceConfigRow>,
) -> Result<DpegPolicy> {
    let bands: DpegSeverityBands = serde_json::from_value(row.severity_bands.clone())?;
    let mut source_overrides = HashMap::new();
    for override_row in overrides {
        source_overrides.insert(
            override_row.source_id.clone(),
            DpegSourceOverride {
                source_id: override_row.source_id,
                weight: override_row.weight,
                enabled: override_row.enabled,
                stale_timeout_ms: override_row.stale_timeout_ms,
            },
        );
    }

    let policy = DpegPolicy {
        tenant_id: row.tenant_id,
        market_key: row.market_key,
        peg_target: row.peg_target,
        min_sources: row.min_sources,
        quorum_pct: row.quorum_pct,
        sustained_window_ms: row.sustained_window_ms,
        cooldown_sec: row.cooldown_sec,
        stale_timeout_ms: row.stale_timeout_ms,
        severity_bands: bands,
        source_overrides,
    };
    policy.validate()?;
    Ok(policy)
}

fn snapshot_event_from_outcome(
    policy: &DpegPolicy,
    snapshot: &ConsensusSnapshot,
    now: chrono::DateTime<chrono::Utc>,
) -> MarketConsensusSnapshot {
    let mut metadata = HashMap::new();
    metadata.insert(
        "quorum_pct".to_string(),
        serde_json::json!(policy.quorum_pct),
    );
    metadata.insert(
        "min_sources".to_string(),
        serde_json::json!(policy.min_sources),
    );

    MarketConsensusSnapshot {
        snapshot_id: Uuid::new_v4(),
        tenant_id: policy.tenant_id.clone(),
        market_key: policy.market_key.clone(),
        peg_target: policy.peg_target,
        weighted_median_price: snapshot.weighted_median_price,
        divergence_pct: snapshot.divergence_pct,
        source_count: snapshot.source_count as u32,
        quorum_met: snapshot.quorum_met,
        breach_active: snapshot.breach_active,
        severity: snapshot.severity.clone(),
        observed_at: now,
        metadata,
    }
}

fn detection_from_snapshot(
    policy: &DpegPolicy,
    snapshot: &ConsensusSnapshot,
    severity: Severity,
    now: chrono::DateTime<chrono::Utc>,
) -> DetectionResult {
    let subject_key = format!("{}:{}", policy.tenant_id, policy.market_key);

    DetectionResult {
        detection_id: Uuid::new_v4(),
        event_key: Some(format!("dpeg:{}:{}", policy.tenant_id, policy.market_key)),
        subject_type: Some("market".to_string()),
        subject_key: Some(subject_key.clone()),
        tenant_id: Some(policy.tenant_id.clone()),
        chain: Chain::Offchain,
        chain_slug: "offchain".to_string(),
        protocol: format!("market:{}", policy.market_key),
        lifecycle_state: LifecycleState::Confirmed,
        requires_confirmation: false,
        attack_family: AttackFamily::PegDeviation,
        near_miss: false,
        confidence: 0.95,
        severity: severity.clone(),
        triggered_rule_ids: vec![format!("dpeg-consensus:{}", policy.market_key)],
        tx_hash: subject_key,
        block_number: 0,
        signals: vec![DetectionSignal {
            signal_type: SignalType::PegDeviation,
            triggered: true,
            value: Some(snapshot.divergence_pct),
            detail: format!(
                "market_key={} weighted_median={:.6} peg_target={:.6} divergence_pct={:.4}",
                policy.market_key,
                snapshot.weighted_median_price,
                policy.peg_target,
                snapshot.divergence_pct
            ),
        }],
        risk_score: RiskScore {
            score: risk_score_for_severity(&severity),
            confidence: 0.9,
            rationale: vec![format!(
                "DPEG consensus breach for {} at {:.4}% divergence",
                policy.market_key, snapshot.divergence_pct
            )],
            attribution: Vec::new(),
        },
        attribution: Vec::new(),
        oracle_context: HashMap::from([
            (
                "market_key".to_string(),
                serde_json::json!(policy.market_key),
            ),
            (
                "peg_target".to_string(),
                serde_json::json!(policy.peg_target),
            ),
            (
                "weighted_median_price".to_string(),
                serde_json::json!(snapshot.weighted_median_price),
            ),
            (
                "divergence_pct".to_string(),
                serde_json::json!(snapshot.divergence_pct),
            ),
            (
                "source_count".to_string(),
                serde_json::json!(snapshot.source_count),
            ),
        ]),
        actions_recommended: vec![
            "Review stablecoin liquidity and peg health immediately".to_string(),
            "Consider pausing sensitive risk paths if divergence persists".to_string(),
        ],
        created_at: now,
    }
}

fn risk_score_for_severity(severity: &Severity) -> f64 {
    match severity {
        Severity::Info => 10.0,
        Severity::Low => 30.0,
        Severity::Medium => 55.0,
        Severity::High => 75.0,
        Severity::Critical => 90.0,
    }
}

fn dpeg_state_from_row(row: state_manager::DpegAlertStateRow) -> DpegAlertState {
    DpegAlertState {
        breach_started_at: row.breach_started_at,
        cooldown_until: row.cooldown_until,
        last_alerted_at: row.last_alerted_at,
        last_divergence_pct: row.last_divergence_pct,
        last_severity: row
            .last_severity
            .as_deref()
            .and_then(parse_severity_from_str),
    }
}

fn parse_severity_from_str(raw: &str) -> Option<Severity> {
    match raw.to_ascii_lowercase().as_str() {
        "info" => Some(Severity::Info),
        "low" => Some(Severity::Low),
        "medium" => Some(Severity::Medium),
        "high" => Some(Severity::High),
        "critical" => Some(Severity::Critical),
        _ => None,
    }
}

fn default_consumer_name(prefix: &str) -> String {
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string());
    format!("{prefix}-{hostname}")
}

async fn init_stream_publisher() -> Option<RedisStreamPublisher> {
    let Some(publisher_result) = RedisStreamPublisher::from_env() else {
        return None;
    };

    let publisher = match publisher_result {
        Ok(publisher) => publisher,
        Err(err) => {
            warn!(error = ?err, "invalid REDIS_URL; redis streams disabled");
            return None;
        }
    };

    if let Err(err) = publisher.healthcheck().await {
        warn!(error = ?err, "redis healthcheck failed");
        None
    } else {
        Some(publisher)
    }
}

async fn init_repository() -> Option<PostgresRepository> {
    let Some(database_url) = PostgresRepository::from_env() else {
        return None;
    };

    match PostgresRepository::from_database_url(&database_url).await {
        Ok(repo) => Some(repo),
        Err(err) => {
            warn!(error = ?err, "failed to connect to postgres");
            None
        }
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(default)
}
