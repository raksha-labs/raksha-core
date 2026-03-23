//! Unified Detector -- consumes UnifiedEvents from the Redis stream and runs every
//! registered DetectionPattern against each event.
//!
//! Patterns are DB-configured: configs are loaded from tenant_pattern_configs at startup
//! and refreshed every CONFIG_RELOAD_INTERVAL_SECS.

use std::time::Duration;

use anyhow::{Context, Result};
use common::{
    init_logging, make_postgres_tls_connector, start_health_check_server_with_commands,
    HealthCommand,
};
use dotenvy::dotenv;
use futures_util::future::poll_fn;
use state_manager::{describe_redis_url, PostgresRepository, RedisStreamPublisher};
use tokio::{signal, sync::mpsc, time::interval};
use tokio_postgres::AsyncMessage;
use tracing::{info, warn};

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
    let (command_tx, mut command_rx) = mpsc::channel(16);
    let command_token = std::env::var("AUTH_INTERNAL_SERVICE_TOKEN").ok();
    let health_status =
        start_health_check_server_with_commands("detector", command_tx, command_token);

    let redis_url = std::env::var("REDIS_URL").context("REDIS_URL not set")?;
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL not set")?;

    info!(redis_url = %describe_redis_url(&redis_url), "attempting redis connection");

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
                common::log_error!(warn, err, "initial pattern config reload failed");
            } else {
                info!("initial pattern configs loaded");
            }
        }
        Err(err) => common::log_error!(warn, err, "failed to load initial pattern configs"),
    }

    let mut reload_ticker = interval(Duration::from_secs(CONFIG_RELOAD_INTERVAL_SECS));
    let mut notify_rx = spawn_pattern_config_notify_listener(database_url.clone());

    loop {
        tokio::select! {
            // Graceful shutdown.
            _ = signal::ctrl_c() => {
                info!("shutdown signal received -- stopping detector");
                break;
            }

            // Periodic config reload.
            _ = reload_ticker.tick() => {
                reload_pattern_configs(&repo, &mut registry).await;
            }

            signal = notify_rx.recv() => {
                if signal.is_none() {
                    warn!("pattern config notify listener stopped; continuing with periodic reload only");
                }
                reload_pattern_configs(&repo, &mut registry).await;
            }

            command = command_rx.recv() => {
                match command {
                    Some(HealthCommand::TriggerReload) => {
                        reload_pattern_configs(&repo, &mut registry).await;
                    }
                    None => {
                        warn!("detector health command channel closed; continuing without explicit reload commands");
                    }
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
                        common::log_error!(error, err, "failed to read from unified-events stream");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                for (entry_id, event) in entries {
                    if let Err(err) = registry.process_event(&event, &repo, &stream).await {
                        common::log_error!(
                            warn,
                            err,
                            "error processing event",
                            entry_id = %entry_id,
                            event_id = %event.event_id
                        );
                    }
                    // Acknowledge regardless to avoid indefinite replays on errors.
                    if let Err(err) = stream.ack_unified_event(CONSUMER_GROUP, &entry_id).await {
                        common::log_error!(
                            warn,
                            err,
                            "failed to ack event",
                            entry_id = %entry_id
                        );
                    }
                }
            }
        }
    }

    info!("detector stopped");
    Ok(())
}

async fn reload_pattern_configs(
    repo: &PostgresRepository,
    registry: &mut patterns::PatternRegistry,
) {
    match repo.load_tenant_pattern_configs().await {
        Ok(cfg) => {
            if let Err(err) = registry.reload_all(&cfg).await {
                common::log_error!(warn, err, "pattern config reload failed");
            } else {
                info!("pattern configs reloaded");
            }
        }
        Err(err) => common::log_error!(warn, err, "failed to reload pattern configs"),
    }
}

fn spawn_pattern_config_notify_listener(database_url: String) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let connector = match make_postgres_tls_connector() {
                Ok(connector) => connector,
                Err(error) => {
                    common::log_error!(
                        warn,
                        error,
                        "failed to build postgres TLS connector for pattern config LISTEN"
                    );
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            let (client, mut connection) = match tokio_postgres::connect(&database_url, connector)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    common::log_error!(warn, error, "failed to connect for pattern config LISTEN");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            if let Err(error) = client.batch_execute("LISTEN pattern_config_changed").await {
                common::log_error!(
                    warn,
                    error,
                    "failed to execute LISTEN pattern_config_changed"
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            info!("pattern config listener connected (LISTEN pattern_config_changed)");

            loop {
                let message = poll_fn(|cx| connection.poll_message(cx)).await;
                match message {
                    Some(Ok(AsyncMessage::Notification(notification))) => {
                        if notification.channel() == "pattern_config_changed"
                            && tx.send(()).await.is_err()
                        {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        common::log_error!(
                            warn,
                            error,
                            "pattern config listener connection error; reconnecting"
                        );
                        break;
                    }
                    None => {
                        warn!("pattern config listener connection closed; reconnecting");
                        break;
                    }
                }
            }
        }
    });
    rx
}
