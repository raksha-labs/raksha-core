//! Risk Scoring Service
//!
//! Consumes detection results from the detection engine and assigns 0-100 risk scores
//! based on:
//! - Detection severity
//! - Historical patterns
//! - Cross-protocol correlations
//! - Market context
//!
//! # Architecture
//!
//! ```text
//! Detection Results Stream → Risk Scorer → Risk-Scored Alerts Stream → Orchestrator
//! ```
//!
//! # Configuration
//!
//! Environment variables:
//! - `REDIS_URL`: Redis connection string
//! - `DATABASE_URL`: PostgreSQL connection string
//! - `RISK_SCORER_STREAM_CONSUMER`: Unique consumer ID for this instance
//! - `HEALTH_CHECK_PORT`: Health check HTTP server port (default: 8083)
//! - `RUST_LOG`: Logging level
//!
//! # Scaling
//!
//! This service uses Redis Streams consumer groups for horizontal scaling.
//! Run multiple instances with different `RISK_SCORER_STREAM_CONSUMER` values
//! to distribute load.

use anyhow::{Context, Result};
use common::health_check::{start_health_check_server, HealthStatus};
use common::shutdown::ShutdownSignal;
use event_schema::{DetectionResult, RiskScoredAlert};
use state_manager::redis::RedisStreamClient;
use std::env;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,scorer=debug".to_string()),
        )
        .init();

    // Load environment
    dotenv::dotenv().ok();

    info!("Starting Risk Scoring Service");

    // Get configuration
    let redis_url = env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let consumer_id = env::var("RISK_SCORER_STREAM_CONSUMER")
        .unwrap_or_else(|_| format!("scorer-{}", uuid::Uuid::new_v4()));

    info!("Configuration:");
    info!("  Redis URL: {}", redis_url);
    info!("  Consumer ID: {}", consumer_id);

    // Initialize health check
    let health_status = HealthStatus::new();
    let health_handle = start_health_check_server(health_status.clone())
        .await
        .context("Failed to start health check server")?;

    // Install shutdown signal handler
    let shutdown = ShutdownSignal::install();

    // Connect to Redis
    let redis_client = RedisStreamClient::new(&redis_url)
        .await
        .context("Failed to connect to Redis")?;

    info!("Connected to Redis Streams");
    health_status.set_redis_connected(true);

    // Mark ready
    health_status.set_ready(true);
    info!("Risk Scoring Service is ready");

    // Main processing loop
    let mut error_count = 0;
    const MAX_ERRORS: u32 = 10;

    while !shutdown.is_shutdown_requested() {
        match process_detections(&redis_client, &consumer_id).await {
            Ok(processed) => {
                if processed > 0 {
                    error_count = 0; // Reset error count on success
                } else {
                    // No events, sleep briefly
                    sleep(Duration::from_millis(100)).await;
                }
            }
            Err(e) => {
                error_count += 1;
                error!("Error processing detections: {:#}", e);

                if error_count >= MAX_ERRORS {
                    error!("Too many consecutive errors, marking unhealthy");
                    health_status.set_ready(false);
                    sleep(Duration::from_secs(5)).await;
                    error_count = 0; // Reset after backing off
                } else {
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    // Graceful shutdown
    info!("Shutdown signal received, cleaning up...");
    health_status.set_ready(false);
    health_handle.abort();

    info!("Risk Scoring Service stopped");
    Ok(())
}

/// Process detection results and assign risk scores
async fn process_detections(
    redis_client: &RedisStreamClient,
    consumer_id: &str,
) -> Result<usize> {
    // Read from detection-results stream using consumer group
    let detections: Vec<DetectionResult> = redis_client
        .read_stream_with_consumer(
            "detection-results",
            "risk-scorers",
            consumer_id,
            10, // Batch size
        )
        .await?;

    if detections.is_empty() {
        return Ok(0);
    }

    info!("Processing {} detection results", detections.len());

    for detection in &detections {
        // Calculate risk score
        let risk_score = calculate_risk_score(detection);

        // Create risk-scored alert
        let scored_alert = RiskScoredAlert {
            detection_id: detection.id.clone(),
            rule_id: detection.rule_id.clone(),
            chain: detection.chain.clone(),
            protocol: detection.protocol.clone(),
            event_key: detection.event_key.clone(),
            severity: detection.severity.clone(),
            risk_score,
            timestamp: chrono::Utc::now(),
            metadata: detection.metadata.clone(),
        };

        // Publish to risk-scored-alerts stream
        redis_client
            .publish("risk-scored-alerts", &scored_alert)
            .await
            .context("Failed to publish scored alert")?;

        info!(
            "Scored alert: {} (severity: {:?}, score: {})",
            detection.rule_id, detection.severity, risk_score
        );
    }

    Ok(detections.len())
}

/// Calculate risk score (0-100) based on detection severity and context
///
/// # MVP Implementation
///
/// Simple severity-based mapping:
/// - Critical: 90-100
/// - High: 70-89
/// - Medium: 40-69
/// - Low: 20-39
/// - Info: 0-19
///
/// # Post-MVP Enhancements
///
/// - Historical pattern analysis
/// - Cross-protocol correlation
/// - Market volatility context
/// - Protocol-specific risk models
/// - ML-based scoring
fn calculate_risk_score(detection: &DetectionResult) -> u8 {
    // MVP: Simple severity mapping
    let base_score = match detection.severity.as_str() {
        "critical" => 95,
        "high" => 75,
        "medium" => 50,
        "low" => 30,
        "info" => 10,
        _ => 50, // Default to medium
    };

    // TODO: Add context-based adjustments
    // - Increase score if similar detections in last hour
    // - Increase score if multiple protocols affected
    // - Decrease score if event is during known maintenance
    // - Adjust based on protocol TVL

    base_score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_risk_score_calculation() {
        let detection = DetectionResult {
            id: "test-1".to_string(),
            rule_id: "oracle-divergence".to_string(),
            chain: "ethereum".to_string(),
            protocol: "aave-v3".to_string(),
            event_key: "tx-123".to_string(),
            severity: "critical".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        };

        let score = calculate_risk_score(&detection);
        assert!(score >= 90 && score <= 100);
    }

    #[test]
    fn test_risk_score_ranges() {
        let test_cases = vec![
            ("critical", 90, 100),
            ("high", 70, 89),
            ("medium", 40, 69),
            ("low", 20, 39),
            ("info", 0, 19),
        ];

        for (severity, min, max) in test_cases {
            let detection = DetectionResult {
                id: "test".to_string(),
                rule_id: "test-rule".to_string(),
                chain: "ethereum".to_string(),
                protocol: "test".to_string(),
                event_key: "test-key".to_string(),
                severity: severity.to_string(),
                timestamp: chrono::Utc::now(),
                metadata: serde_json::json!({}),
            };

            let score = calculate_risk_score(&detection);
            assert!(
                score >= min && score <= max,
                "Score {} for severity {} not in range {}-{}",
                score,
                severity,
                min,
                max
            );
        }
    }
}
