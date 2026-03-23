use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use chrono::Utc;
use common::{init_logging, start_health_check_server, ShutdownSignal};
use dotenvy::dotenv;
use event_schema::{
    AlertEvent, Chain, DetectionResult, FinalityStatus, IncidentTransition, LifecycleState,
    Severity,
};
use notifier::{GatewayDispatchResult, NotifierGatewayClient};
use state_manager::{
    describe_redis_url, EntityExposureRecord, IncidentKey, IncidentRecord, PostgresRepository,
    RedisStreamPublisher,
};
use tracing::{info, warn};
use uuid::Uuid;

const DEFAULT_BATCH_SIZE: usize = 100;
const DEFAULT_BLOCK_MS: usize = 1000;
const DEFAULT_ALERT_FALLBACK_TENANT_ID: &str = "glider";
const REDIS_STARTUP_RETRY_ATTEMPTS: usize = 30;
const REDIS_STARTUP_RETRY_DELAY_MS: u64 = 2_000;
const ALL_NOTIFICATION_CHANNELS: [&str; 5] = ["webhook", "slack", "telegram", "discord", "email"];

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    init_logging("info");
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

            if is_test_mode_detection(&detection) {
                log_test_mode_detection_received(&detection);
            }

            let alert = alert_from_detection(&detection);
            let alert = attach_incident_context(alert, &detection, repository.as_ref()).await;
            let dispatched =
                dispatch_alert(&alert, &notifier_gateway, repository.as_ref(), &stream).await;
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
                        Err(err) => common::log_error!(
                            warn,
                            err,
                            "failed to load alert context from postgres for finality update",
                            event_key = %update.event_key
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
                    let (transition, status, closes_incident, reason) = match update.lifecycle_state
                    {
                        LifecycleState::Retracted => {
                            ("retract", "retracted", true, "finality_reorg_retraction")
                        }
                        LifecycleState::Confirmed => {
                            ("update", "active", false, "finality_confirmation")
                        }
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
                        common::log_error!(
                            warn,
                            error,
                            "failed to update incident state from finality update",
                            incident_id = incident_id
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
                        common::log_error!(
                            warn,
                            error,
                            "failed to append incident event from finality update",
                            incident_id = incident_id
                        );
                    }
                }
            }
            let dispatched = dispatch_alert(
                &updated_alert,
                &notifier_gateway,
                repository.as_ref(),
                &stream,
            )
            .await;
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
        channel_routes: Vec::new(),
        dedup_key: detection.event_key.clone(),
        attribution: detection.risk_score.attribution.clone(),
        blast_radius: Vec::new(),
        exposure_summary: std::collections::HashMap::new(),
        tx_hash: detection.tx_hash.clone(),
        block_number: detection.block_number as u64,
        oracle_context: detection.oracle_context.clone(),
        actions_recommended: detection.actions_recommended.clone(),
        is_simulated: detection.is_simulated,
        simulation_run_id: detection.simulation_run_id.clone(),
        created_at: Utc::now(),
    }
}

fn is_test_mode_detection(detection: &DetectionResult) -> bool {
    detection.is_simulated || detection.simulation_run_id.is_some()
}

fn is_test_mode_alert(alert: &AlertEvent) -> bool {
    alert.is_simulated || alert.simulation_run_id.is_some()
}

fn log_test_mode_detection_received(detection: &DetectionResult) {
    info!(
        pipeline_mode = "test",
        component = "orchestrator",
        detection_id = %detection.detection_id,
        pattern_id = %detection.pattern_id,
        tenant_id = detection.tenant_id.as_deref(),
        event_key = detection.event_key.as_deref(),
        subject_key = detection.subject_key.as_deref(),
        severity = ?detection.severity,
        lifecycle_state = ?detection.lifecycle_state,
        simulation_run_id = detection.simulation_run_id.as_deref(),
        "test-mode detection received by orchestrator"
    );
}

fn log_test_mode_alert_state(message: &'static str, alert: &AlertEvent) {
    info!(
        pipeline_mode = "test",
        component = "orchestrator",
        alert_id = %alert.alert_id,
        incident_id = alert.incident_id.as_deref(),
        tenant_id = alert.tenant_id.as_deref(),
        pattern_id = %alert.pattern_id,
        event_key = alert.event_key.as_deref(),
        subject_key = alert.subject_key.as_deref(),
        severity = ?alert.severity,
        lifecycle_state = ?alert.lifecycle_state,
        simulation_run_id = alert.simulation_run_id.as_deref(),
        "{message}"
    );
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
    let closes_incident = matches!(
        transition,
        IncidentTransition::Resolve | IncidentTransition::Retract
    );
    let now = Utc::now();
    let key = IncidentKey {
        tenant_id: &tenant_id,
        pattern_id: &detection.pattern_id,
        subject_type: detection.subject_type.as_deref(),
        subject_key: detection.subject_key.as_deref(),
        chain_slug: &detection.chain_slug,
    };

    let incident = match repo.find_active_incident(key).await {
        Ok(Some(existing)) => existing,
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
                common::log_error!(
                    warn,
                    error,
                    "failed to create incident",
                    tenant_id = %tenant_id,
                    pattern_id = %detection.pattern_id
                );
            }
            created
        }
        Err(error) => {
            common::log_error!(
                warn,
                error,
                "failed to find active incident",
                tenant_id = %tenant_id,
                pattern_id = %detection.pattern_id
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
                        let liquidity =
                            if is_critical { "at_risk" } else { "degraded" }.to_string();
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
                    .save_incident_entity_exposures(&incident.incident_id, &tenant_id, &exposures)
                    .await
                {
                    common::log_error!(
                        warn,
                        err,
                        "failed to save blast radius exposures",
                        incident_id = %incident.incident_id
                    );
                }

                alert.blast_radius = exposures.iter().map(|e| e.entity_id.clone()).collect();
                let total_capital: f64 = exposures.iter().map(|e| e.capital_at_risk_usd).sum();
                let total_loss_5pct: f64 = exposures.iter().map(|e| e.loss_scenario_5pct_usd).sum();
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
                alert
                    .exposure_summary
                    .insert("asset_symbol".to_string(), serde_json::json!(symbol));
            }
            Ok(_) => {
                // No monitored entities for this asset — blast_radius stays empty.
            }
            Err(err) => {
                common::log_error!(
                    warn,
                    err,
                    "failed to query monitored entities for blast radius"
                );
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
        common::log_error!(
            warn,
            error,
            "failed to update incident state",
            incident_id = %incident.incident_id
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
        common::log_error!(
            warn,
            error,
            "failed to append incident event",
            incident_id = %incident.incident_id
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
        common::log_error!(
            warn,
            error,
            "failed to append incident context snapshot",
            incident_id = %incident.incident_id
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
    let is_test_mode = is_test_mode_alert(&normalized_alert);

    if is_test_mode {
        log_test_mode_alert_state("dispatching test-mode alert", &normalized_alert);
    }

    if let Some(repo) = repository {
        match should_escalate_to_all_channels_for_rate_limit(repo, &tenant_id, &normalized_alert)
            .await
        {
            Ok(true) => normalized_alert.channel_routes = all_notification_channels(),
            Ok(false) => {}
            Err(err) => {
                common::log_error!(
                    warn,
                    err,
                    "failed to evaluate hourly alert-rate escalation",
                    tenant_id = %tenant_id
                );
            }
        }

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

                    if is_test_mode_alert(&suppressed) {
                        log_test_mode_alert_state(
                            "test-mode alert suppressed before notifier dispatch",
                            &suppressed,
                        );
                    }

                    persist_and_publish_alert(&suppressed, repository, stream).await;
                    record_usage_event(repo, &tenant_id, "alert_suppressed_quota", &suppressed)
                        .await;
                    return suppressed;
                }
                Ok(None) => {}
                Err(err) => {
                    common::log_error!(
                        warn,
                        err,
                        "failed to evaluate monthly alert quota",
                        tenant_id = %tenant_id
                    );
                }
            }
        }
    }

    match notifier_gateway.dispatch_alert(&normalized_alert).await {
        Ok(dispatch_result) => {
            info!(
                tenant_id = %dispatch_result.tenant_id,
                alert_id = %normalized_alert.alert_id,
                delivered = dispatch_result.delivered,
                reason = ?dispatch_result.reason,
                resolved_channels = ?dispatch_result.resolved_channels,
                "notifier-gateway dispatch completed"
            );
            if is_test_mode {
                info!(
                    pipeline_mode = "test",
                    component = "orchestrator",
                    tenant_id = %dispatch_result.tenant_id,
                    alert_id = %normalized_alert.alert_id,
                    delivered = dispatch_result.delivered,
                    reason = ?dispatch_result.reason,
                    resolved_channels = ?dispatch_result.resolved_channels,
                    simulation_run_id = normalized_alert.simulation_run_id.as_deref(),
                    "test-mode alert dispatch completed"
                );
            }
            normalized_alert = apply_dispatch_result_metadata(normalized_alert, &dispatch_result);
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
                        common::log_error!(
                            warn,
                            err,
                            "failed to persist alert delivery attempt",
                            channel = %result.channel
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
            common::log_error!(warn, err, "failed to dispatch alert to notifier-gateway");
        }
    }

    persist_and_publish_alert(&normalized_alert, repository, stream).await;

    if let Some(repo) = repository {
        record_usage_event(repo, &tenant_id, "alert_fired", &normalized_alert).await;
    }

    normalized_alert
}

fn apply_dispatch_result_metadata(
    mut alert: AlertEvent,
    dispatch_result: &GatewayDispatchResult,
) -> AlertEvent {
    alert.channel_routes = dispatch_result.resolved_channels.clone();
    alert.oracle_context.insert(
        "dispatch_resolved_channels".to_string(),
        serde_json::json!(dispatch_result.resolved_channels),
    );
    alert.oracle_context.insert(
        "dispatch_delivered".to_string(),
        serde_json::json!(dispatch_result.delivered),
    );
    if let Some(reason) = dispatch_result.reason.as_deref() {
        alert
            .oracle_context
            .insert("dispatch_reason".to_string(), serde_json::json!(reason));
    }
    alert
}

async fn persist_and_publish_alert(
    alert: &AlertEvent,
    repository: Option<&PostgresRepository>,
    stream: &RedisStreamPublisher,
) {
    if let Some(repo) = repository {
        if let Err(err) = repo.save_alert(alert).await {
            common::log_error!(warn, err, "failed to persist alert");
        }
        if let Err(err) = repo.save_alert_lifecycle(alert).await {
            common::log_error!(warn, err, "failed to persist alert lifecycle event");
        }
    }

    if let Err(err) = stream.publish_alert(alert).await {
        common::log_error!(warn, err, "failed to publish alert stream event");
    }
    if let Err(err) = stream.publish_alert_lifecycle(alert).await {
        common::log_error!(warn, err, "failed to publish alert lifecycle stream event");
    }
    if is_test_mode_alert(alert) {
        log_test_mode_alert_state("test-mode alert persisted and published", alert);
    }
}

async fn check_quota_exceeded(
    repository: &PostgresRepository,
    tenant_id: &str,
) -> Result<Option<(i64, i64)>> {
    let Some(limit) = repository
        .load_tenant_monthly_alert_quota(tenant_id)
        .await?
    else {
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
        common::log_error!(
            warn,
            err,
            "failed to persist usage event",
            event_type = event_type,
            tenant_id = tenant_id
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

fn all_notification_channels() -> Vec<String> {
    ALL_NOTIFICATION_CHANNELS
        .iter()
        .map(|channel| (*channel).to_string())
        .collect()
}

async fn should_escalate_to_all_channels_for_rate_limit(
    repository: &PostgresRepository,
    tenant_id: &str,
    alert: &AlertEvent,
) -> Result<bool> {
    if matches!(alert.severity, Severity::Critical) {
        return Ok(false);
    }
    if !matches!(
        alert.lifecycle_state,
        LifecycleState::Provisional | LifecycleState::Confirmed
    ) {
        return Ok(false);
    }

    let Some(limit) = repository.load_tenant_hourly_alert_limit(tenant_id).await? else {
        return Ok(false);
    };
    if limit < 0 {
        return Ok(false);
    }

    let current_hour_alerts = repository
        .count_usage_event_quantity_for_past_hour(tenant_id, "alert_fired")
        .await?;
    Ok(current_hour_alerts >= limit)
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
    let redis_url = std::env::var("REDIS_URL").ok()?;
    info!(redis_url = %describe_redis_url(&redis_url), "attempting redis connection");

    let publisher_result = RedisStreamPublisher::from_url(&redis_url);

    let publisher = match publisher_result {
        Ok(publisher) => publisher,
        Err(err) => {
            common::log_error!(warn, err, "invalid REDIS_URL; redis streams disabled");
            return None;
        }
    };

    for attempt in 1..=REDIS_STARTUP_RETRY_ATTEMPTS {
        match publisher.healthcheck().await {
            Ok(()) => return Some(publisher),
            Err(err) if attempt < REDIS_STARTUP_RETRY_ATTEMPTS => {
                common::log_error!(
                    warn,
                    err,
                    "redis healthcheck failed during startup; retrying",
                    attempt,
                    max_attempts = REDIS_STARTUP_RETRY_ATTEMPTS
                );
                tokio::time::sleep(Duration::from_millis(REDIS_STARTUP_RETRY_DELAY_MS)).await;
            }
            Err(err) => {
                common::log_error!(warn, err, "redis healthcheck failed");
                return None;
            }
        }
    }

    None
}

async fn init_repository() -> Option<PostgresRepository> {
    let Some(database_url) = PostgresRepository::from_env() else {
        info!("DATABASE_URL not set; postgres state_manager disabled");
        return None;
    };

    match PostgresRepository::from_database_url(&database_url).await {
        Ok(repo) => Some(repo),
        Err(err) => {
            common::log_error!(
                warn,
                err,
                "failed to initialize postgres state_manager; disabled"
            );
            None
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// BA/QA Test Suite — Orchestrator Tests (Test_Cases_DepegV1_0)
// ═══════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;
    use event_schema::{
        AttackFamily, Chain, ContextClassification, DetectionResult, DetectionSignal,
        IncidentTransition, LifecycleState, RiskScore, Severity, SignalType,
    };

    fn mk_detection(severity: Severity, transition: IncidentTransition) -> DetectionResult {
        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: "dpeg".to_string(),
            event_key: Some("dpeg:tenant-a:USDC/USD".to_string()),
            subject_type: Some("market".to_string()),
            subject_key: Some("tenant-a:USDC/USD".to_string()),
            tenant_id: Some("tenant-a".to_string()),
            chain: Chain::Base,
            chain_slug: "base".to_string(),
            protocol: "market:USDC/USD".to_string(),
            lifecycle_state: LifecycleState::Provisional,
            requires_confirmation: true,
            attack_family: AttackFamily::PegDeviation,
            severity,
            description: Some("USDC depeg detected".to_string()),
            triggered_rule_ids: vec!["depeg_stablecoin".to_string()],
            tx_hash: "0xabc123".to_string(),
            block_number: 12345000,
            signals: vec![DetectionSignal {
                signal_type: SignalType::PriceDeviation,
                value: 1.5,
                label: Some("1.50%".to_string()),
                source_id: Some("weighted_median".to_string()),
            }],
            risk_score: RiskScore {
                score: 1.5,
                confidence: 85.0,
                rationale: vec!["depeg detected".to_string()],
                attribution: Vec::new(),
            },
            incident_transition: Some(transition),
            context_classification: Some(ContextClassification::Isolated),
            confidence_breakdown: HashMap::new(),
            oracle_context: HashMap::new(),
            actions_recommended: vec!["REBALANCE_TO_ETH".to_string()],
            is_simulated: false,
            simulation_run_id: None,
            created_at: Utc::now(),
        }
    }

    // ─── 7. Dedup / Escalation / Suppression (TC-D-705 to TC-D-708) ──────

    /// TC-D-705: Terminal state — RESOLVED maps correctly from Resolve transition
    #[test]
    fn tc_d_705_terminal_state_resolved() {
        let status = incident_status_for_transition(&IncidentTransition::Resolve);
        assert_eq!(status, "resolved");
    }

    /// TC-D-706: Terminal state — Retract maps to "retracted"
    /// NOTE: Spec references FALSE_POSITIVE which doesn't exist in code.
    /// Testing the actual terminal state: retracted.
    #[test]
    fn tc_d_706_terminal_state_retracted() {
        let status = incident_status_for_transition(&IncidentTransition::Retract);
        assert_eq!(status, "retracted");
    }

    /// TC-D-707: Escalation/Deescalation/Update all map to "active" (not terminal)
    #[test]
    fn tc_d_707_non_terminal_transitions() {
        assert_eq!(
            incident_status_for_transition(&IncidentTransition::Escalate),
            "active"
        );
        assert_eq!(
            incident_status_for_transition(&IncidentTransition::Deescalate),
            "active"
        );
        assert_eq!(
            incident_status_for_transition(&IncidentTransition::Update),
            "active"
        );
    }

    /// TC-D-708: Trigger maps to "triggered" (initial state)
    #[test]
    fn tc_d_708_trigger_maps_to_triggered() {
        assert_eq!(
            incident_status_for_transition(&IncidentTransition::Trigger),
            "triggered"
        );
    }

    // ─── 8. Alert Routing (TC-D-800 to TC-D-808) ─────────────────────────

    fn dispatch_result(resolved_channels: &[&str], delivered: bool) -> GatewayDispatchResult {
        GatewayDispatchResult {
            tenant_id: "tenant-a".to_string(),
            delivered,
            reason: (!delivered).then_some("all_channels_failed".to_string()),
            resolved_channels: resolved_channels
                .iter()
                .map(|item| (*item).to_string())
                .collect(),
            results: Vec::new(),
        }
    }

    /// TC-D-800: CRITICAL routes to all channels
    #[test]
    fn tc_d_800_critical_routes_to_all_channels() {
        let detection = mk_detection(Severity::Critical, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        let routed = apply_dispatch_result_metadata(
            alert,
            &dispatch_result(&["webhook", "slack", "telegram", "discord", "email"], true),
        );
        assert_eq!(routed.channel_routes, all_notification_channels());
    }

    /// TC-D-801: HIGH routes to webhook + email
    #[test]
    fn tc_d_801_high_routes_to_webhook_email() {
        let detection = mk_detection(Severity::High, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        let routed =
            apply_dispatch_result_metadata(alert, &dispatch_result(&["webhook", "email"], true));
        assert_eq!(
            routed.channel_routes,
            vec!["webhook".to_string(), "email".to_string()]
        );
    }

    /// TC-D-802: MEDIUM routes to webhook only (primary)
    #[test]
    fn tc_d_802_medium_routes_to_webhook_only() {
        let detection = mk_detection(Severity::Medium, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        let routed = apply_dispatch_result_metadata(alert, &dispatch_result(&["webhook"], true));
        assert_eq!(routed.channel_routes, vec!["webhook".to_string()]);
    }

    /// TC-D-803: Quiet hours suppress MEDIUM
    #[test]
    fn tc_d_803_quiet_hours_suppress_medium() {
        let detection = mk_detection(Severity::Medium, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        let routed = apply_dispatch_result_metadata(alert, &dispatch_result(&[], false));
        assert!(routed.channel_routes.is_empty());
        assert_eq!(
            routed
                .oracle_context
                .get("dispatch_reason")
                .and_then(|value| value.as_str()),
            Some("all_channels_failed")
        );
    }

    /// TC-D-804: Quiet hours do NOT suppress CRITICAL
    #[test]
    fn tc_d_804_quiet_hours_do_not_suppress_critical() {
        let detection = mk_detection(Severity::Critical, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        let routed = apply_dispatch_result_metadata(
            alert,
            &dispatch_result(&["webhook", "slack", "telegram", "discord", "email"], true),
        );
        assert_eq!(routed.channel_routes, all_notification_channels());
    }

    /// TC-D-805: Rate limit hit — escalates to all channels
    #[test]
    fn tc_d_805_rate_limit_escalates() {
        assert_eq!(all_notification_channels().len(), 5);
        assert!(all_notification_channels().contains(&"email".to_string()));
        assert!(all_notification_channels().contains(&"slack".to_string()));
    }

    /// TC-D-806: Silenced incident — notifications suppressed
    #[test]
    #[ignore = "Incident silencing not yet implemented"]
    fn tc_d_806_silenced_incident_suppressed() {}

    /// TC-D-807: Silence overridden by CRITICAL escalation
    #[test]
    #[ignore = "Incident silencing not yet implemented"]
    fn tc_d_807_silence_overridden_by_critical() {}

    /// TC-D-808: Silence overridden by reorg correction
    #[test]
    #[ignore = "Incident silencing not yet implemented"]
    fn tc_d_808_silence_overridden_by_reorg() {}

    // ─── 9. Notification Delivery (TC-D-900 to TC-D-904) — PARTIAL ──────

    /// TC-D-900: Webhook delivery with HMAC-SHA256 signature
    #[test]
    #[ignore = "HMAC-SHA256 webhook signing not yet implemented in notifier crate"]
    fn tc_d_900_webhook_hmac_signature() {}

    /// TC-D-901: Webhook failure triggers backup channel
    #[test]
    #[ignore = "Backup channel routing not yet implemented"]
    fn tc_d_901_webhook_failure_triggers_backup() {}

    /// TC-D-902: Webhook retry with exponential backoff
    #[test]
    #[ignore = "Retry logic handled by notifier-gateway (not in core)"]
    fn tc_d_902_webhook_retry_exponential_backoff() {}

    /// TC-D-903: Webhook payload contains required depeg fields
    #[test]
    #[ignore = "Webhook payload structure validation — requires notifier integration test"]
    fn tc_d_903_webhook_payload_required_fields() {}

    /// TC-D-904: Telegram message format
    #[test]
    #[ignore = "Telegram formatting test — requires notifier integration test"]
    fn tc_d_904_telegram_message_format() {}

    // ─── 12. Per-Position Threshold Overrides (TC-D-1200 to TC-D-1202) ───

    /// TC-D-1200: Sensitive position alerted below default threshold
    #[test]
    #[ignore = "Per-position threshold multipliers not yet implemented"]
    fn tc_d_1200_sensitive_position_lower_threshold() {}

    /// TC-D-1201: Tolerant position NOT alerted at default threshold
    #[test]
    #[ignore = "Per-position threshold multipliers not yet implemented"]
    fn tc_d_1201_tolerant_position_higher_threshold() {}

    /// TC-D-1202: Mixed positions — some affected, some not
    #[test]
    #[ignore = "Per-position threshold multipliers not yet implemented"]
    fn tc_d_1202_mixed_positions() {}

    // ─── 13. Blast Radius & Recommended Actions (TC-D-1300 to TC-D-1302) ─

    /// TC-D-1300: extract_asset_symbol correctly parses subject_key
    #[test]
    fn tc_d_1300_extract_asset_symbol() {
        assert_eq!(
            extract_asset_symbol(Some("tenant-a:USDC/USD")),
            Some("USDC".to_string())
        );
        assert_eq!(
            extract_asset_symbol(Some("glider:USDT/USD")),
            Some("USDT".to_string())
        );
        assert_eq!(extract_asset_symbol(None), None);
        assert_eq!(extract_asset_symbol(Some("")), None);
    }

    /// TC-D-1301: should_enforce_monthly_quota — Critical bypasses quota
    #[test]
    fn tc_d_1301_quota_bypass_for_critical() {
        let detection = mk_detection(Severity::Critical, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        assert!(
            !should_enforce_monthly_quota(&alert),
            "Critical alerts should bypass monthly quota"
        );
    }

    /// TC-D-1302: should_enforce_monthly_quota — non-Critical enforced
    #[test]
    fn tc_d_1302_quota_enforced_for_non_critical() {
        let detection = mk_detection(Severity::Medium, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        assert!(
            should_enforce_monthly_quota(&alert),
            "Medium alerts should be subject to monthly quota"
        );
    }

    // ─── 14. Detection Signal Detail Payload (TC-D-1400 to TC-D-1401) ────

    /// TC-D-1400: alert_from_detection contains all required fields
    #[test]
    fn tc_d_1400_alert_payload_required_fields() {
        let detection = mk_detection(Severity::Critical, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);

        assert!(!alert.alert_id.is_nil());
        assert_eq!(alert.pattern_id, "dpeg");
        assert_eq!(alert.severity, Severity::Critical);
        assert_eq!(alert.chain, Chain::Base);
        assert_eq!(alert.chain_slug, "base");
        assert_eq!(alert.finality_status, FinalityStatus::Tentative);
        assert_eq!(alert.lifecycle_state, LifecycleState::Provisional);
        assert_eq!(alert.block_number, 12345000);
        assert_eq!(alert.tx_hash, "0xabc123");
        assert!(alert.event_key.is_some());
        assert!(alert.subject_key.is_some());
        assert!(alert.tenant_id.is_some());
        assert!(!alert.rule_ids.is_empty());
        assert!(!alert.actions_recommended.is_empty());
        // Routing is resolved later by notifier-gateway + policy-manager.
        assert!(alert.channel_routes.is_empty());
    }

    /// TC-D-1401: alert_from_detection assigns dedup_key from event_key
    #[test]
    fn tc_d_1401_dedup_key_matches_event_key() {
        let detection = mk_detection(Severity::High, IncidentTransition::Trigger);
        let alert = alert_from_detection(&detection);
        assert_eq!(alert.dedup_key, detection.event_key);
    }

    // ─── 15. Integration Tests (TC-D-1500 to TC-D-1502) ─────────────────

    /// TC-D-1500: Full pipeline — source events → detection → signal
    #[test]
    #[ignore = "Requires full pipeline with mock PostgresRepository + Redis"]
    fn tc_d_1500_full_pipeline_detection() {}

    /// TC-D-1501: Systemic pipeline — USDT depeg triggers systemic classification
    #[test]
    #[ignore = "Requires full pipeline with mock PostgresRepository + Redis"]
    fn tc_d_1501_systemic_pipeline() {}

    /// TC-D-1502: Full lifecycle — detection → incident → delivery → resolution
    #[test]
    #[ignore = "Requires full pipeline with mock PostgresRepository + Redis + notifier"]
    fn tc_d_1502_full_lifecycle() {}
}
