//! Unified Detector -- consumes UnifiedEvents from the Redis stream and runs every
//! registered DetectionPattern against each event.
//!
//! Patterns are DB-configured: configs are loaded from tenant_pattern_configs at startup
//! and refreshed every CONFIG_RELOAD_INTERVAL_SECS.

use std::time::Duration;

use anyhow::{Context, Result};
use common::{init_logging, start_health_check_server};
use dotenvy::dotenv;
use state_manager::{PostgresRepository, RedisStreamPublisher};
use tokio::{signal, time::interval};
use tracing::{error, info, warn};

mod patterns;

/// Consumer group name for the unified-events stream.
const CONSUMER_GROUP: &str = "detector-workers";
/// Stream batch size per poll.
const STREAM_BATCH_SIZE: usize = 100;
/// Max milliseconds to block waiting for new stream entries.
const STREAM_BLOCK_MS: usize = 2_000;
/// How often to reload pattern configs from the DB.
const CONFIG_RELOAD_INTERVAL_SECS: u64 = 30;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    init_logging("info");
    let health_status = start_health_check_server("detector");

    let redis_url = std::env::var("REDIS_URL").context("REDIS_URL not set")?;
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL not set")?;

    let stream =
        RedisStreamPublisher::from_url(&redis_url).context("failed to connect to Redis")?;

    let repo = PostgresRepository::from_database_url(&database_url)
        .await
        .context("failed to connect to Postgres")?;

    // Unique consumer name for horizontal scaling.
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string());
    let consumer_name = format!("detector-{hostname}");

    // Ensure the unified-events consumer group exists.
    stream
        .ensure_unified_events_group(CONSUMER_GROUP)
        .await
        .context("failed to ensure consumer group")?;

    if let Some(status) = health_status.as_ref() {
        let mut health = status.write().await;
        health.redis_connected = true;
        health.postgres_connected = true;
        health.details = vec![format!("consumer_group={CONSUMER_GROUP}")];
        health.is_ready = true;
    }

    info!(
        consumer = %consumer_name,
        group = CONSUMER_GROUP,
        "detector started -- consuming unified-events stream"
    );

    // Build pattern registry with all built-in patterns.
    let mut registry = patterns::PatternRegistry::new();

    // Initial config load.
    match repo.load_tenant_pattern_configs().await {
        Ok(cfg) => {
            if let Err(err) = registry.reload_all(&cfg).await {
                warn!(error = ?err, "initial pattern config reload failed");
            } else {
                info!("initial pattern configs loaded");
            }
        }
        Err(err) => warn!(error = ?err, "failed to load initial pattern configs"),
    }

    let mut reload_ticker = interval(Duration::from_secs(CONFIG_RELOAD_INTERVAL_SECS));

    loop {
        tokio::select! {
            // Graceful shutdown.
            _ = signal::ctrl_c() => {
                info!("shutdown signal received -- stopping detector");
                break;
            }

            // Periodic config reload.
            _ = reload_ticker.tick() => {
                match repo.load_tenant_pattern_configs().await {
                    Ok(cfg) => {
                        if let Err(err) = registry.reload_all(&cfg).await {
                            warn!(error = ?err, "pattern config reload failed");
                        } else {
                            info!("pattern configs reloaded");
                        }
                    }
                    Err(err) => warn!(error = ?err, "failed to reload pattern configs"),
                }
            }

            // Poll the unified-events consumer group.
            entries = stream.read_unified_events_group(
                CONSUMER_GROUP,
                &consumer_name,
                STREAM_BATCH_SIZE,
                STREAM_BLOCK_MS,
            ) => {
                let entries = match entries {
                    Ok(v) => v,
                    Err(err) => {
                        error!(error = ?err, "failed to read from unified-events stream");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                for (entry_id, event) in entries {
                    if let Err(err) = registry.process_event(&event, &repo, &stream).await {
                        warn!(
                            entry_id = %entry_id,
                            event_id = %event.event_id,
                            error = ?err,
                            "error processing event"
                        );
                    }
                    // Acknowledge regardless to avoid indefinite replays on errors.
                    if let Err(err) = stream.ack_unified_event(CONSUMER_GROUP, &entry_id).await {
                        warn!(entry_id = %entry_id, error = ?err, "failed to ack event");
                    }
                }
            }
        }
    }

    info!("detector stopped");
    Ok(())
}
