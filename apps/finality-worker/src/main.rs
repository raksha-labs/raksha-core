use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use common_types::Chain;
use core_interfaces::FinalityEngine;
use dotenv::dotenv;
use event_stream::RedisStreamPublisher;
use finality_engine::{ChainFinalityTracker, InMemoryFinalityEngine};
use tracing::{info, warn};

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

    let Some(stream) = init_stream_publisher().await else {
        warn!("REDIS_URL not set or unavailable; finality-worker requires Redis Streams");
        return Ok(());
    };

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
        .unwrap_or(false);

    let mut normalized_last_id =
        std::env::var("FINALITY_NORMALIZED_START_ID").unwrap_or_else(|_| "0-0".to_string());
    let mut reorg_last_id =
        std::env::var("FINALITY_REORG_START_ID").unwrap_or_else(|_| "0-0".to_string());
    let finality_group =
        std::env::var("FINALITY_STREAM_GROUP").unwrap_or_else(|_| "finality-workers".to_string());
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

    info!("finality-worker started");

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

fn default_confirmation_depth_for_chain(chain: &Chain) -> u64 {
    let env_name = match chain {
        Chain::Ethereum => "ETH_CONFIRMATION_DEPTH",
        Chain::Base => "BASE_CONFIRMATION_DEPTH",
        Chain::Unknown => "CONFIRMATION_DEPTH",
    };

    std::env::var(env_name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(match chain {
            Chain::Base => 64,
            _ => 3,
        })
        .max(1)
}
