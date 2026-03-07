use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use common::{init_logging, start_health_check_server, FinalityEngine, ShutdownSignal};
use dotenvy::dotenv;
use event_schema::Chain;
use state_manager::RedisStreamPublisher;
use state_manager::{ChainFinalityTracker, InMemoryFinalityEngine, PostgresRepository};
use tracing::{info, warn};

const DEFAULT_BATCH_SIZE: usize = 100;
const DEFAULT_BLOCK_MS: usize = 1000;
const PERSISTENCE_INTERVAL: usize = 100; // Save state every N events
const REDIS_STARTUP_RETRY_ATTEMPTS: usize = 30;
const REDIS_STARTUP_RETRY_DELAY_MS: u64 = 2_000;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    init_logging("info");
    let health_status = start_health_check_server("finality");

    let Some(stream) = init_stream_publisher().await else {
        warn!("REDIS_URL not set or unavailable; state-manager requires Redis Streams");
        return Ok(());
    };

    // Install graceful shutdown handler
    let shutdown = ShutdownSignal::install();

    let batch_size = std::env::var("FINALITY_STREAM_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let block_ms = std::env::var("FINALITY_STREAM_BLOCK_MS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BLOCK_MS);
    let run_once = std::env::var("FINALITY_RUN_ONCE")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let use_consumer_group = std::env::var("FINALITY_USE_CONSUMER_GROUP")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(true); // Enable consumer groups by default for horizontal scaling

    let mut normalized_last_id =
        std::env::var("FINALITY_NORMALIZED_START_ID").unwrap_or_else(|_| "0-0".to_string());
    let mut reorg_last_id =
        std::env::var("FINALITY_REORG_START_ID").unwrap_or_else(|_| "0-0".to_string());
    let finality_group = std::env::var("FINALITY_STREAM_GROUP")
        .unwrap_or_else(|_| "state-manager-workers".to_string());
    let finality_consumer = std::env::var("FINALITY_STREAM_CONSUMER")
        .unwrap_or_else(|_| default_consumer_name("finality"));

    if use_consumer_group {
        stream
            .ensure_normalized_events_group(&finality_group)
            .await?;
        stream.ensure_reorg_notices_group(&finality_group).await?;
        info!(
            group = %finality_group,
            consumer = %finality_consumer,
            "finality stream consumer-group mode enabled"
        );
    }

    let engine = InMemoryFinalityEngine::new();
    let mut trackers: HashMap<Chain, ChainFinalityTracker> = HashMap::new();

    // Initialize PostgreSQL persistence for fault tolerance
    let repository = init_repository().await;
    if repository.is_some() {
        info!("finality state persistence enabled");
    } else {
        warn!("DATABASE_URL not set; finality state will not be persisted");
    }

    if let Some(status) = health_status.as_ref() {
        let mut health = status.write().await;
        health.redis_connected = true;
        health.postgres_connected = repository.is_some();
        health.details = vec![
            format!("consumer_group_enabled={use_consumer_group}"),
            format!("stream_group={finality_group}"),
        ];
        health.is_ready = true;
    }

    info!("state-manager started");

    let mut event_count = 0usize;

    loop {
        let mut processed = 0usize;

        let normalized_entries = if use_consumer_group {
            stream
                .read_normalized_events_group(
                    &finality_group,
                    &finality_consumer,
                    batch_size,
                    block_ms,
                )
                .await?
        } else {
            stream
                .read_normalized_events(&normalized_last_id, batch_size, block_ms)
                .await?
        };
        for (entry_id, event) in normalized_entries {
            if !use_consumer_group {
                normalized_last_id = entry_id.clone();
            }

            let tracker = trackers.entry(event.chain.clone()).or_insert_with(|| {
                // Try to load existing state from database
                if let Some(repo) = &repository {
                    let chain_str = format!("{:?}", event.chain).to_lowercase();
                    match load_tracker_from_db(repo, &event.chain, &chain_str) {
                        Ok(Some(tracker)) => {
                            info!(chain = ?event.chain, "loaded finality state from database");
                            return tracker;
                        }
                        Ok(None) => {
                            info!(chain = ?event.chain, "no existing finality state found");
                        }
                        Err(e) => {
                            warn!(chain = ?event.chain, error = ?e, "failed to load finality state");
                        }
                    }
                }

                // Create new tracker if loading failed or no database
                ChainFinalityTracker::new(
                    event.chain.clone(),
                    default_confirmation_depth_for_chain(&event.chain),
                )
            });

            let batch = tracker.observe_event(&event);
            if let Some(notice) = batch.reorg_notice {
                stream.publish_reorg_notice(&notice).await?;
                engine.handle_reorg_notice(&notice).await?;
                warn!(
                    chain = ?notice.chain,
                    orphaned_from_block = notice.orphaned_from_block,
                    affected = notice.affected_event_keys.len(),
                    "published reorg notice"
                );
            }

            for update in batch.updates {
                stream.publish_finality_update(&update).await?;
                engine.apply_update(&update).await?;
                processed += 1;
            }

            if use_consumer_group {
                stream
                    .ack_normalized_event(&finality_group, &entry_id)
                    .await?;
            }
        }

        let reorg_entries = if use_consumer_group {
            stream
                .read_reorg_notices_group(&finality_group, &finality_consumer, batch_size, block_ms)
                .await?
        } else {
            stream
                .read_reorg_notices(&reorg_last_id, batch_size, block_ms)
                .await?
        };
        for (entry_id, notice) in reorg_entries {
            if !use_consumer_group {
                reorg_last_id = entry_id.clone();
            }

            let tracker = trackers.entry(notice.chain.clone()).or_insert_with(|| {
                ChainFinalityTracker::new(
                    notice.chain.clone(),
                    default_confirmation_depth_for_chain(&notice.chain),
                )
            });
            let updates = tracker.apply_reorg_notice(&notice);

            engine.handle_reorg_notice(&notice).await?;
            for update in updates {
                stream.publish_finality_update(&update).await?;
                engine.apply_update(&update).await?;
                processed += 1;
            }

            if use_consumer_group {
                stream.ack_reorg_notice(&finality_group, &entry_id).await?;
            }
        }

        // Periodically persist finality state for fault tolerance
        event_count += processed;
        if event_count >= PERSISTENCE_INTERVAL {
            if let Some(repo) = &repository {
                for (chain, tracker) in &trackers {
                    if let Err(e) = persist_tracker(repo, chain, tracker).await {
                        warn!(chain = ?chain, error = ?e, "failed to persist finality state");
                    }
                }
                event_count = 0;
            }
        }

        // Check for graceful shutdown
        if shutdown.is_shutdown_requested() {
            info!("shutdown signal received; persisting state before exit");
            // Final state persistence
            if let Some(repo) = &repository {
                for (chain, tracker) in &trackers {
                    if let Err(e) = persist_tracker(repo, chain, tracker).await {
                        warn!(chain = ?chain, error = ?e, "failed to persist finality state on shutdown");
                    } else {
                        info!(chain = ?chain, "finality state persisted on shutdown");
                    }
                }
            }
            break;
        }

        if run_once {
            info!(processed, "FINALITY_RUN_ONCE=true; stopping after one loop");
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

async fn init_stream_publisher() -> Option<RedisStreamPublisher> {
    let publisher_result = RedisStreamPublisher::from_env()?;

    let publisher = match publisher_result {
        Ok(publisher) => publisher,
        Err(err) => {
            warn!(error = ?err, "invalid REDIS_URL; redis streams disabled");
            return None;
        }
    };

    for attempt in 1..=REDIS_STARTUP_RETRY_ATTEMPTS {
        match publisher.healthcheck().await {
            Ok(()) => return Some(publisher),
            Err(err) if attempt < REDIS_STARTUP_RETRY_ATTEMPTS => {
                warn!(
                    error = ?err,
                    attempt,
                    max_attempts = REDIS_STARTUP_RETRY_ATTEMPTS,
                    "redis healthcheck failed during startup; retrying"
                );
                tokio::time::sleep(Duration::from_millis(REDIS_STARTUP_RETRY_DELAY_MS)).await;
            }
            Err(err) => {
                warn!(error = ?err, "redis healthcheck failed");
                return None;
            }
        }
    }

    None
}

fn default_confirmation_depth_for_chain(chain: &Chain) -> u64 {
    let env_name = match chain {
        Chain::Ethereum => "ETH_CONFIRMATION_DEPTH",
        Chain::Arbitrum => "ARB_CONFIRMATION_DEPTH",
        Chain::Optimism => "OP_CONFIRMATION_DEPTH",
        Chain::Base => "BASE_CONFIRMATION_DEPTH",
        Chain::Polygon => "POLYGON_CONFIRMATION_DEPTH",
        Chain::Avalanche => "AVAX_CONFIRMATION_DEPTH",
        Chain::BSC => "BSC_CONFIRMATION_DEPTH",
        Chain::Offchain => "OFFCHAIN_CONFIRMATION_DEPTH",
        Chain::Unknown => "CONFIRMATION_DEPTH",
    };

    std::env::var(env_name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(match chain {
            Chain::Base => 64,
            Chain::Offchain => 1,
            _ => 3,
        })
        .max(1)
}

async fn init_repository() -> Option<PostgresRepository> {
    let database_url = PostgresRepository::from_env()?;

    match PostgresRepository::from_database_url(&database_url).await {
        Ok(repo) => Some(repo),
        Err(err) => {
            warn!(error = ?err, "failed to connect to postgres");
            None
        }
    }
}

fn load_tracker_from_db(
    repo: &PostgresRepository,
    chain: &Chain,
    chain_str: &str,
) -> Result<Option<ChainFinalityTracker>> {
    let runtime = tokio::runtime::Handle::current();
    let state = runtime.block_on(repo.load_finality_state(chain_str))?;

    let Some(state) = state else {
        return Ok(None);
    };

    let confirmation_depth = state.confirmation_depth as u64;
    let combined_json = serde_json::json!({
        "head_block": state.head_block,
        "blocks": state.blocks,
        "states": state.states,
    });

    let tracker =
        ChainFinalityTracker::from_json(chain.clone(), confirmation_depth, combined_json)?;

    Ok(Some(tracker))
}

async fn persist_tracker(
    repo: &PostgresRepository,
    chain: &Chain,
    tracker: &ChainFinalityTracker,
) -> Result<()> {
    let chain_str = format!("{:?}", chain).to_lowercase();
    let state_json = tracker.to_json()?;

    let head_block = state_json["head_block"].as_i64().unwrap_or(0);
    let blocks_json = state_json["blocks"].clone();
    let states_json = state_json["states"].clone();
    let confirmation_depth = default_confirmation_depth_for_chain(chain) as i32;

    repo.save_finality_state(
        &chain_str,
        confirmation_depth,
        head_block,
        blocks_json,
        states_json,
    )
    .await?;

    Ok(())
}
