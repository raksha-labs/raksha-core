use std::{collections::HashMap, time::Duration};

use notifier::NotifierGatewayClient;
use anyhow::Result;
use chrono::Utc;
use event_schema::{
    AlertEvent, Chain, DetectionResult, FinalityStatus, IncidentTransition, LifecycleState, Severity,
};
use common::{start_health_check_server, ShutdownSignal};
use dotenvy::dotenv;
use state_manager::{EntityExposureRecord, IncidentKey, IncidentRecord, PostgresRepository, RedisStreamPublisher};
use tracing::{info, warn};
use uuid::Uuid;

const DEFAULT_BATCH_SIZE: usize = 100;
const DEFAULT_BLOCK_MS: usize = 1000;
const DEFAULT_ALERT_FALLBACK_TENANT_ID: &str = "glider";

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
    let health_status = start_health_check_server("orchestrator");

    let Some(stream) = init_stream_publisher().await else {
        warn!("REDIS_URL not set or unavailable; state-manager requires Redis Streams");
        return Ok(());
    };

    // Install graceful shutdown handler
    let shutdown = ShutdownSignal::install();

    let repository = init_repository().await;

    let notifier_gateway = NotifierGatewayClient::from_env();

    let batch_size = std::env::var("STATE_MANAGER_STREAM_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let block_ms = std::env::var("STATE_MANAGER_STREAM_BLOCK_MS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BLOCK_MS);
    let run_once = std::env::var("STATE_MANAGER_RUN_ONCE")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let use_consumer_group = std::env::var("STATE_MANAGER_USE_CONSUMER_GROUP")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(true); // Enable consumer groups by default for horizontal scaling

    let mut detections_last_id =
        std::env::var("STATE_MANAGER_DETECTIONS_START_ID").unwrap_or_else(|_| "0-0".to_string());
    let mut finality_last_id =
        std::env::var("STATE_MANAGER_FINALITY_START_ID").unwrap_or_else(|_| "0-0".to_string());
    let stream_group = std::env::var("STATE_MANAGER_STREAM_GROUP")
        .unwrap_or_else(|_| "state-manager-workers".to_string());
    let stream_consumer = std::env::var("STATE_MANAGER_STREAM_CONSUMER")
        .unwrap_or_else(|_| default_consumer_name("state-manager"));

    if use_consumer_group {
        stream.ensure_detections_group(&stream_group).await?;
        stream.ensure_finality_updates_group(&stream_group).await?;
        info!(
            group = %stream_group,
            consumer = %stream_consumer,
            "state-manager stream consumer-group mode enabled"
        );
    }

    if let Some(status) = health_status.as_ref() {
        let mut health = status.write().await;
        health.redis_connected = true;
        health.postgres_connected = repository.is_some();
        health.details = vec![
            format!("consumer_group_enabled={use_consumer_group}"),
            format!("stream_group={stream_group}"),
        ];
        health.is_ready = true;
    }

    let mut alerts_by_event_key: HashMap<String, AlertEvent> = HashMap::new();
    info!("state-manager started");

    loop {
        let mut processed = 0usize;

        let detections = if use_consumer_group {
            stream
                .read_detections_group(&stream_group, &stream_consumer, batch_size, block_ms)
                .await?
        } else {
            stream
                .read_detections(&detections_last_id, batch_size, block_ms)
                .await?
        };
        for (entry_id, detection) in detections {
            if !use_consumer_group {
                detections_last_id = entry_id.clone();
            }

            let Some(event_key) = detection.event_key.as_deref() else {
                if use_consumer_group {
                    stream.ack_detection(&stream_group, &entry_id).await?;
                }
                continue;
            };
            if detection.triggered_rule_ids.is_empty() {
                if use_consumer_group {
                    stream.ack_detection(&stream_group, &entry_id).await?;
                }
                continue;
            }

            let alert = alert_from_detection(&detection);
            let alert = attach_incident_context(alert, &detection, repository.as_ref()).await;
            let dispatched = dispatch_alert(&alert, &notifier_gateway, repository.as_ref(), &stream).await;
            alerts_by_event_key.insert(event_key.to_string(), dispatched);
            processed += 1;

            if use_consumer_group {
                stream.ack_detection(&stream_group, &entry_id).await?;
            }
        }

        let finality_updates = if use_consumer_group {
            stream
                .read_finality_updates_group(&stream_group, &stream_consumer, batch_size, block_ms)
                .await?
        } else {
            stream
                .read_finality_updates(&finality_last_id, batch_size, block_ms)
                .await?
        };
        for (entry_id, update) in finality_updates {
            if !use_consumer_group {
                finality_last_id = entry_id.clone();
            }

            if !matches!(
                update.lifecycle_state,
                LifecycleState::Confirmed | LifecycleState::Retracted
            ) {
                if use_consumer_group {
                    stream.ack_finality_update(&stream_group, &entry_id).await?;
                }
                continue;
            }

            let mut existing = alerts_by_event_key.get(&update.event_key).cloned();
            if existing.is_none() {
                if let Some(repo) = repository.as_ref() {
                    match repo.find_latest_alert_by_event_key(&update.event_key).await {
                        Ok(found) => existing = found,
                        Err(err) => warn!(
                            event_key = %update.event_key,
                            error = ?err,
                            "failed to load alert context from postgres for finality update"
                        ),
                    }
                }
            }

            let Some(existing) = existing else {
                if use_consumer_group {
                    stream.ack_finality_update(&stream_group, &entry_id).await?;
                }
                continue;
            };
            if existing.lifecycle_state == update.lifecycle_state {
                if use_consumer_group {
                    stream.ack_finality_update(&stream_group, &entry_id).await?;
                }
                continue;
            }

            let mut updated_alert = existing;
            updated_alert.lifecycle_state = update.lifecycle_state.clone();
            updated_alert.finality_status = match update.lifecycle_state {
                LifecycleState::Confirmed => FinalityStatus::Finalized,
                LifecycleState::Retracted => FinalityStatus::Tentative,
                _ => updated_alert.finality_status,
            };
            updated_alert.created_at = Utc::now();

            if let Some(repo) = repository.as_ref() {
                if let Some(incident_id) = updated_alert.incident_id.as_deref() {
                    let severity_text = format!("{:?}", updated_alert.severity).to_lowercase();
                    let (transition, status, closes_incident, reason) = match update.lifecycle_state {
                        LifecycleState::Retracted => ("retract", "retracted", true, "finality_reorg_retraction"),
                        LifecycleState::Confirmed => ("update", "active", false, "finality_confirmation"),
                        _ => ("update", "active", false, "finality_update"),
                    };
                    if let Err(error) = repo
                        .update_incident_state(
                            incident_id,
                            status,
                            &severity_text,
                            updated_alert.created_at,
                            closes_incident,
                        )
                        .await
                    {
                        warn!(
                            error = ?error,
                            incident_id = incident_id,
                            "failed to update incident state from finality update"
                        );
                    }
                    if let Err(error) = repo
                        .append_incident_event(
                            incident_id,
                            transition,
                            None,
                            Some(status),
                            Some(reason),
                            serde_json::json!({
                                "event_key": update.event_key,
                                "lifecycle_state": format!("{:?}", update.lifecycle_state).to_lowercase(),
                                "block_number": update.block_number,
                            }),
                            updated_alert.created_at,
                        )
                        .await
                    {
                        warn!(
                            error = ?error,
                            incident_id = incident_id,
                            "failed to append incident event from finality update"
                        );
                    }
                }
            }
            let dispatched =
                dispatch_alert(&updated_alert, &notifier_gateway, repository.as_ref(), &stream).await;
            alerts_by_event_key.insert(update.event_key.clone(), dispatched);
            processed += 1;

            if use_consumer_group {
                stream.ack_finality_update(&stream_group, &entry_id).await?;
            }
        }

        // Check for graceful shutdown
        if shutdown.is_shutdown_requested() {
            info!("shutdown signal received; stopping gracefully");
            break;
        }

        if run_once {
            info!(
                processed,
                "STATE_MANAGER_RUN_ONCE=true; stopping after one loop"
            );
            break;
        }

        if processed == 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    Ok(())
}

fn default_consumer_name(prefix: &str) -> String {
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string());
    format!("{prefix}-{hostname}")
}

fn alert_from_detection(detection: &DetectionResult) -> AlertEvent {
    AlertEvent {
        alert_id: Uuid::new_v4(),
        incident_id: None,
        event_key: detection.event_key.clone(),
        subject_type: detection.subject_type.clone(),
        subject_key: detection.subject_key.clone(),
        tenant_id: Some(resolve_alert_tenant_id(detection.tenant_id.clone())),
        pattern_id: detection.pattern_id.clone(),
        chain: detection.chain.clone(),
        chain_slug: detection.chain_slug.clone(),
        protocol: detection.protocol.clone(),
        lifecycle_state: detection.lifecycle_state.clone(),
        finality_status: FinalityStatus::Tentative,
        severity: detection.severity.clone(),
        risk_score: detection.risk_score.score,
        confidence: detection.risk_score.confidence,
        confidence_breakdown: detection.confidence_breakdown.clone(),
        rule_ids: detection.triggered_rule_ids.clone(),
        channel_routes: vec![
            "webhook".to_string(),
            "slack".to_string(),
            "telegram".to_string(),
            "discord".to_string(),
        ],
        dedup_key: detection.event_key.clone(),
        attribution: detection.risk_score.attribution.clone(),
        blast_radius: Vec::new(),
        exposure_summary: std::collections::HashMap::new(),
        tx_hash: detection.tx_hash.clone(),
        block_number: detection.block_number as u64,
        oracle_context: detection.oracle_context.clone(),
        actions_recommended: detection.actions_recommended.clone(),
        created_at: Utc::now(),
    }
}

async fn attach_incident_context(
    mut alert: AlertEvent,
    detection: &DetectionResult,
    repository: Option<&PostgresRepository>,
) -> AlertEvent {
    let Some(repo) = repository else {
        return alert;
    };

    let tenant_id = resolve_alert_tenant_id(alert.tenant_id.clone());
    let transition = detection
        .incident_transition
        .clone()
        .unwrap_or(IncidentTransition::Trigger);
    let transition_str = incident_transition_str(&transition);
    let incident_status = incident_status_for_transition(&transition);
    let closes_incident = matches!(transition, IncidentTransition::Resolve | IncidentTransition::Retract);
    let now = Utc::now();
    let key = IncidentKey {
        tenant_id: &tenant_id,
        pattern_id: &detection.pattern_id,
        subject_type: detection.subject_type.as_deref(),
        subject_key: detection.subject_key.as_deref(),
        chain_slug: &detection.chain_slug,
    };

    let incident = match repo.find_active_incident(key).await {
        Ok(Some(existing)) => {
            existing
        }
        Ok(None) => {
            let created = IncidentRecord {
                incident_id: Uuid::new_v4().to_string(),
                tenant_id: tenant_id.clone(),
                pattern_id: detection.pattern_id.clone(),
                subject_type: detection.subject_type.clone(),
                subject_key: detection.subject_key.clone(),
                chain_slug: detection.chain_slug.clone(),
                status: incident_status.to_string(),
                current_severity: format!("{:?}", detection.severity).to_lowercase(),
            };
            if let Err(error) = repo.create_incident(&created, now).await {
                warn!(
                    error = ?error,
                    tenant_id = %tenant_id,
                    pattern_id = %detection.pattern_id,
                    "failed to create incident"
                );
            }
            created
        }
        Err(error) => {
            warn!(
                error = ?error,
                tenant_id = %tenant_id,
                pattern_id = %detection.pattern_id,
                "failed to find active incident"
            );
            return alert;
        }
    };

    alert.incident_id = Some(incident.incident_id.clone());

    // --- Blast radius computation -------------------------------------------------
    // Identify monitored entities exposed to the depegged asset and compute how
    // much capital is at risk.  Results are persisted to incident_entity_exposures
    // and surfaced in the alert payload via blast_radius / exposure_summary.
    if let Some(symbol) = extract_asset_symbol(detection.subject_key.as_deref()) {
        match repo
            .find_monitored_entities_for_alert(&tenant_id, &symbol)
            .await
        {
            Ok(entities) if !entities.is_empty() => {
                let current_price = detection
                    .oracle_context
                    .get("weighted_median_price")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let divergence_pct = detection.risk_score.score;
                let is_critical = matches!(detection.severity, Severity::Critical);

                let exposures: Vec<EntityExposureRecord> = entities
                    .iter()
                    .map(|entity| {
                        let capital = if current_price > 0.0 {
                            entity.quantity * current_price
                        } else {
                            entity.valuation_usd
                        };
                        let loss_5pct = capital * 0.05;
                        let loss_10pct = capital * 0.10;
                        // Estimated slippage scales with the depeg magnitude (capped at 100 %)
                        let slippage = divergence_pct.min(100.0);
                        let liquidity = if is_critical {
                            "at_risk"
                        } else {
                            "degraded"
                        }
                        .to_string();
                        // Estimated savings = capital that could have been protected if exit
                        // was triggered at the initial peg floor rather than current price.
                        let savings = capital * (divergence_pct / 100.0).max(0.0);
                        EntityExposureRecord {
                            entity_id: entity.entity_id.clone(),
                            capital_at_risk_usd: capital,
                            liquidity_status: liquidity,
                            estimated_slippage_pct: slippage,
                            loss_scenario_5pct_usd: loss_5pct,
                            loss_scenario_10pct_usd: loss_10pct,
                            estimated_savings_usd: savings,
                            payload: serde_json::json!({
                                "entity_type": entity.entity_type,
                                "display_name": entity.display_name,
                                "chain_slug": entity.chain_slug,
                                "asset_symbol": entity.asset_symbol,
                                "quantity": entity.quantity,
                                "current_price": current_price,
                                "divergence_pct": divergence_pct,
                            }),
                        }
                    })
                    .collect();

                if let Err(err) = repo
                    .save_incident_entity_exposures(
                        &incident.incident_id,
                        &tenant_id,
                        &exposures,
                    )
                    .await
                {
                    warn!(
                        error = ?err,
                        incident_id = %incident.incident_id,
                        "failed to save blast radius exposures"
                    );
                }

                alert.blast_radius = exposures.iter().map(|e| e.entity_id.clone()).collect();
                let total_capital: f64 =
                    exposures.iter().map(|e| e.capital_at_risk_usd).sum();
                let total_loss_5pct: f64 =
                    exposures.iter().map(|e| e.loss_scenario_5pct_usd).sum();
                let total_loss_10pct: f64 =
                    exposures.iter().map(|e| e.loss_scenario_10pct_usd).sum();
                alert.exposure_summary.insert(
                    "total_capital_at_risk_usd".to_string(),
                    serde_json::json!(total_capital),
                );
                alert.exposure_summary.insert(
                    "entity_count".to_string(),
                    serde_json::json!(exposures.len()),
                );
                alert.exposure_summary.insert(
                    "total_loss_scenario_5pct_usd".to_string(),
                    serde_json::json!(total_loss_5pct),
                );
                alert.exposure_summary.insert(
                    "total_loss_scenario_10pct_usd".to_string(),
                    serde_json::json!(total_loss_10pct),
                );
                alert.exposure_summary.insert(
                    "asset_symbol".to_string(),
                    serde_json::json!(symbol),
                );
            }
            Ok(_) => {
                // No monitored entities for this asset — blast_radius stays empty.
            }
            Err(err) => {
                warn!(error = ?err, "failed to query monitored entities for blast radius");
            }
        }
    }
    // ------------------------------------------------------------------------------

    let severity_text = format!("{:?}", detection.severity).to_lowercase();
    if let Err(error) = repo
        .update_incident_state(
            &incident.incident_id,
            incident_status,
            &severity_text,
            now,
            closes_incident,
        )
        .await
    {
        warn!(
            error = ?error,
            incident_id = %incident.incident_id,
            "failed to update incident state"
        );
    }

    let event_payload = serde_json::json!({
        "pattern_id": &detection.pattern_id,
        "severity": severity_text,
        "event_key": &detection.event_key,
        "context_classification": &detection.context_classification,
        "confidence_breakdown": &detection.confidence_breakdown,
    });
    if let Err(error) = repo
        .append_incident_event(
            &incident.incident_id,
            transition_str,
            Some(incident.status.as_str()),
            Some(incident_status),
            detection.description.as_deref(),
            event_payload,
            now,
        )
        .await
    {
        warn!(
            error = ?error,
            incident_id = %incident.incident_id,
            "failed to append incident event"
        );
    }

    let classification = detection
        .context_classification
        .as_ref()
        .map(|value| match value {
            event_schema::ContextClassification::Isolated => "isolated",
            event_schema::ContextClassification::Systemic => "systemic",
            event_schema::ContextClassification::None => "none",
        });
    if let Err(error) = repo
        .append_incident_context_snapshot(
            &incident.incident_id,
            classification,
            Some(detection.risk_score.score),
            Some(detection.risk_score.confidence),
            serde_json::json!({
                "confidence_breakdown": &detection.confidence_breakdown,
                "signals": &detection.signals,
            }),
            now,
        )
        .await
    {
        warn!(
            error = ?error,
            incident_id = %incident.incident_id,
            "failed to append incident context snapshot"
        );
    }

    alert
}

fn incident_transition_str(value: &IncidentTransition) -> &'static str {
    match value {
        IncidentTransition::Trigger => "trigger",
        IncidentTransition::Escalate => "escalate",
        IncidentTransition::Deescalate => "deescalate",
        IncidentTransition::Resolve => "resolve",
        IncidentTransition::Retract => "retract",
        IncidentTransition::Update => "update",
    }
}

fn incident_status_for_transition(value: &IncidentTransition) -> &'static str {
    match value {
        IncidentTransition::Trigger => "triggered",
        IncidentTransition::Escalate => "active",
        IncidentTransition::Deescalate => "active",
        IncidentTransition::Update => "active",
        IncidentTransition::Resolve => "resolved",
        IncidentTransition::Retract => "retracted",
    }
}

async fn dispatch_alert(
    alert: &AlertEvent,
    notifier_gateway: &NotifierGatewayClient,
    repository: Option<&PostgresRepository>,
    stream: &RedisStreamPublisher,
) -> AlertEvent {
    let mut normalized_alert = alert.clone();
    let tenant_id = resolve_alert_tenant_id(normalized_alert.tenant_id.clone());
    normalized_alert.tenant_id = Some(tenant_id.clone());

    if let Some(repo) = repository {
        if should_enforce_monthly_quota(&normalized_alert) {
            match check_quota_exceeded(repo, &tenant_id).await {
                Ok(Some((limit, consumed))) => {
                    let mut suppressed = normalized_alert.clone();
                    suppressed.lifecycle_state = LifecycleState::Suppressed;
                    suppressed.created_at = Utc::now();
                    suppressed.oracle_context.insert(
                        "suppression_reason".to_string(),
                        serde_json::json!("monthly_alert_quota_exceeded"),
                    );
                    suppressed.oracle_context.insert(
                        "suppression_quota_limit".to_string(),
                        serde_json::json!(limit),
                    );
                    suppressed.oracle_context.insert(
                        "suppression_quota_used".to_string(),
                        serde_json::json!(consumed),
                    );

                    persist_and_publish_alert(&suppressed, repository, stream).await;
                    record_usage_event(
                        repo,
                        &tenant_id,
                        "alert_suppressed_quota",
                        &suppressed,
                    )
                    .await;
                    return suppressed;
                }
                Ok(None) => {}
                Err(err) => {
                    warn!(
                        error = ?err,
                        tenant_id = %tenant_id,
                        "failed to evaluate monthly alert quota"
                    );
                }
            }
        }
    }

    match notifier_gateway.dispatch_alert(&normalized_alert).await {
        Ok(dispatch_result) => {
            if let Some(repo) = repository {
                for result in &dispatch_result.results {
                    if let Err(err) = repo
                        .save_alert_delivery_attempt(
                            &normalized_alert.alert_id.to_string(),
                            &dispatch_result.tenant_id,
                            &result.channel,
                            result.delivered,
                            result.reason.as_deref(),
                            result.status_code,
                        )
                        .await
                    {
                        warn!(
                            error = ?err,
                            channel = %result.channel,
                            "failed to persist alert delivery attempt"
                        );
                    }
                }
            }
            if !dispatch_result.delivered {
                warn!(
                    tenant_id = %dispatch_result.tenant_id,
                    reason = ?dispatch_result.reason,
                    "notifier-gateway dispatch did not deliver alert"
                );
            }
        }
        Err(err) => {
            warn!(error = ?err, "failed to dispatch alert to notifier-gateway");
        }
    }

    persist_and_publish_alert(&normalized_alert, repository, stream).await;

    if let Some(repo) = repository {
        record_usage_event(repo, &tenant_id, "alert_fired", &normalized_alert).await;
    }

    normalized_alert
}

async fn persist_and_publish_alert(
    alert: &AlertEvent,
    repository: Option<&PostgresRepository>,
    stream: &RedisStreamPublisher,
) {
    if let Some(repo) = repository {
        if let Err(err) = repo.save_alert(alert).await {
            warn!(error = ?err, "failed to persist alert");
        }
        if let Err(err) = repo.save_alert_lifecycle(alert).await {
            warn!(error = ?err, "failed to persist alert lifecycle event");
        }
    }

    if let Err(err) = stream.publish_alert(alert).await {
        warn!(error = ?err, "failed to publish alert stream event");
    }
    if let Err(err) = stream.publish_alert_lifecycle(alert).await {
        warn!(error = ?err, "failed to publish alert lifecycle stream event");
    }
}

async fn check_quota_exceeded(
    repository: &PostgresRepository,
    tenant_id: &str,
) -> Result<Option<(i64, i64)>> {
    let Some(limit) = repository.load_tenant_monthly_alert_quota(tenant_id).await? else {
        return Ok(None);
    };
    if limit < 0 {
        return Ok(None);
    }

    let consumed = repository
        .count_usage_event_quantity_for_current_month(tenant_id, "alert_fired")
        .await?;
    if consumed >= limit {
        return Ok(Some((limit, consumed)));
    }
    Ok(None)
}

async fn record_usage_event(
    repository: &PostgresRepository,
    tenant_id: &str,
    event_type: &str,
    alert: &AlertEvent,
) {
    if let Err(err) = repository
        .record_usage_event(
            tenant_id,
            event_type,
            alert_type(alert),
            alert_chain_id(alert),
            1,
        )
        .await
    {
        warn!(
            error = ?err,
            event_type = event_type,
            tenant_id = tenant_id,
            "failed to persist usage event"
        );
    }
}

fn should_enforce_monthly_quota(alert: &AlertEvent) -> bool {
    !matches!(alert.severity, Severity::Critical)
        && matches!(
            alert.lifecycle_state,
            LifecycleState::Provisional | LifecycleState::Confirmed
        )
}

fn alert_type(alert: &AlertEvent) -> &str {
    let event_key = alert.event_key.as_deref().unwrap_or_default();
    if event_key.starts_with("dpeg:") || alert.protocol.starts_with("market:") {
        "dpeg"
    } else {
        "generic"
    }
}

fn alert_chain_id(alert: &AlertEvent) -> Option<i64> {
    match alert.chain {
        Chain::Ethereum => Some(1),
        Chain::Arbitrum => Some(42161),
        Chain::Optimism => Some(10),
        Chain::Base => Some(8453),
        Chain::Polygon => Some(137),
        Chain::Avalanche => Some(43114),
        Chain::BSC => Some(56),
        Chain::Offchain | Chain::Unknown => None,
    }
}

fn resolve_alert_tenant_id(tenant_id: Option<String>) -> String {
    tenant_id.unwrap_or_else(|| {
        std::env::var("ALERT_FALLBACK_TENANT_ID")
            .unwrap_or_else(|_| DEFAULT_ALERT_FALLBACK_TENANT_ID.to_string())
    })
}

/// Extract the base asset symbol from a DPEG subject_key.
///
/// Subject keys are formatted as `"tenant_id:BASE/QUOTE"` (e.g. `"tenant-a:USDC/USD"`).
/// This function returns the `BASE` portion (`"USDC"`) which is the stablecoin being
/// monitored and used to look up tenant positions in `tenant_monitored_entities`.
fn extract_asset_symbol(subject_key: Option<&str>) -> Option<String> {
    let key = subject_key?;
    // The rightmost colon-separated segment is the market_key ("BASE/QUOTE").
    let market_part = key.rsplit(':').next().unwrap_or(key);
    let symbol = market_part.split('/').next()?;
    if symbol.is_empty() {
        None
    } else {
        Some(symbol.to_string())
    }
}

async fn init_stream_publisher() -> Option<RedisStreamPublisher> {
    let publisher_result = RedisStreamPublisher::from_env()?;

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
        info!("DATABASE_URL not set; postgres state_manager disabled");
        return None;
    };

    match PostgresRepository::from_database_url(&database_url).await {
        Ok(repo) => Some(repo),
        Err(err) => {
            warn!(error = ?err, "failed to initialize postgres state_manager; disabled");
            None
        }
    }
}
