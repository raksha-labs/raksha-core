use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use common::{init_logging, start_health_check_server};
use dotenvy::dotenv;
use state_manager::{describe_redis_url, PostgresRepository, RedisStreamPublisher};
use tracing::{info, warn};

mod stream_connector;
mod stream_parser;
mod stream_supervisor;
mod stream_worker;

use stream_supervisor::run_stream_supervisor;

const REDIS_STARTUP_RETRY_ATTEMPTS: usize = 3;
const REDIS_STARTUP_RETRY_DELAY_MS: u64 = 2_000;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    init_logging("info");
    let health_status = start_health_check_server("indexer");

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL not set; indexer requires DB-driven stream configuration")?;
    let stream = init_stream_publisher().await?;
    let repo = PostgresRepository::from_database_url(&database_url)
        .await
        .context("failed to initialize indexer stream configuration repository")?;
    let stream_purge_enabled = read_bool_env("STREAM_PURGE_ENABLED", false);

    if let Some(status) = health_status.as_ref() {
        let mut health = status.write().await;
        health.redis_connected = true;
        health.postgres_connected = true;
        health.details = vec![
            "config_source=catalog.source_stream_configs".to_string(),
            "reload_triggers=LISTEN source_stream_config_changed + periodic reconcile".to_string(),
            format!("stream_purge_enabled={stream_purge_enabled}"),
        ];
        health.is_ready = true;
    }

    info!(stream_purge_enabled, "db-driven stream supervisor started");
    run_stream_supervisor(repo, stream, database_url, stream_purge_enabled).await
}

fn read_bool_env(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => match parse_bool_env(&value) {
            Some(parsed) => parsed,
            None => {
                warn!(
                    env_var = name,
                    env_value = %value,
                    default_value = default,
                    "invalid bool env value; using default"
                );
                default
            }
        },
        Err(_) => default,
    }
}

fn parse_bool_env(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

async fn init_stream_publisher() -> Result<RedisStreamPublisher> {
    let redis_url = std::env::var("REDIS_URL")
        .context("REDIS_URL not set; ingestion requires Redis Streams")?;
    info!(redis_url = %describe_redis_url(&redis_url), "attempting redis connection");

    let publisher = RedisStreamPublisher::from_url(&redis_url)
        .context("invalid REDIS_URL; redis publishing is required for indexer startup")?;

    for attempt in 1..=REDIS_STARTUP_RETRY_ATTEMPTS {
        match publisher.healthcheck().await {
            Ok(()) => return Ok(publisher),
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
                return Err(err).context(
                    "redis healthcheck failed; redis publishing is required for indexer startup",
                );
            }
        }
    }

    Err(anyhow!(
        "redis startup retry loop exited unexpectedly without initializing the stream publisher"
    ))
}
