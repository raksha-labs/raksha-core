use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use ingestion::{
    build_oracle_protocol_map, default_eth_oracles, parse_oracle_addresses,
    parse_oracle_addresses_csv, parse_protocol_category, EvmChainAdapter, EvmChainConfig,
    EvmMockAdapter, EvmProtocolConfigFile, MockProtocol, ProtocolBinding,
    DEFAULT_FILTER_CHUNK_SIZE, DEFAULT_LOOKBACK_BLOCKS, DEFAULT_ORACLE_DECIMALS,
};
use chrono::Utc;
use event_schema::{NormalizedEvent, ReorgNotice};
use common::ChainAdapter;
use dotenv::dotenv;
use state_manager::RedisStreamPublisher;
use serde::de::DeserializeOwned;
use tokio_postgres::{Client, NoTls};
use tracing::{info, warn};

const DEFAULT_RULE_RELATIVE_ROOTS: [&str; 3] = [
    "../defi-surv-rules",
    "../../defi-surv-rules",
    "../../../defi-surv-rules",
];
const DEFAULT_INGESTION_POLL_INTERVAL_SECS: u64 = 5;
const DEFAULT_INGESTION_BLOCK_BATCH_SIZE: u64 = 8;
const DEFAULT_INGESTION_MAX_RETRIES: usize = 5;
const DEFAULT_INGESTION_RETRY_BACKOFF_MS: u64 = 500;
const INGESTION_CHAIN_LOCK_NAMESPACE: i32 = 42_017;
const ZERO_BLOCK_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug)]
struct DiscoveredChainConfig {
    chain_config_path: PathBuf,
    protocol_config_path: PathBuf,
    chain: EvmChainConfig,
    protocols: EvmProtocolConfigFile,
}

struct RuntimeChainAdapter {
    chain_slug: String,
    confirmation_depth: u64,
    bootstrap_lookback_blocks: u64,
    current_batch_size: u64,
    last_indexed_block: u64,
    last_block_hash: String,
    adapter: Box<dyn ChainAdapter>,
}

#[derive(Debug, Default)]
struct ChainTrackerState {
    block_hash_by_number: HashMap<u64, String>,
    event_keys_by_block: BTreeMap<u64, Vec<String>>,
}

impl ChainTrackerState {
    fn observe_event(&mut self, event: &event_schema::NormalizedEvent) -> Option<ReorgNotice> {
        if let Some(incoming_hash) = event.block_hash.as_ref() {
            if let Some(existing_hash) = self.block_hash_by_number.get(&event.block_number) {
                if existing_hash != incoming_hash {
                    let affected_event_keys: Vec<String> = self
                        .event_keys_by_block
                        .range(event.block_number..)
                        .flat_map(|(_, keys)| keys.iter().cloned())
                        .collect();

                    self.event_keys_by_block.split_off(&event.block_number);
                    self.block_hash_by_number
                        .retain(|block_number, _| *block_number < event.block_number);
                    self.block_hash_by_number
                        .insert(event.block_number, incoming_hash.clone());
                    self.event_keys_by_block
                        .insert(event.block_number, vec![event.event_key.clone()]);

                    if !affected_event_keys.is_empty() {
                        return Some(ReorgNotice {
                            chain: event.chain.clone(),
                            orphaned_from_block: event.block_number,
                            common_ancestor_block: event.block_number.saturating_sub(1),
                            affected_event_keys,
                            noticed_at: Utc::now(),
                        });
                    }
                }
            } else {
                self.block_hash_by_number
                    .insert(event.block_number, incoming_hash.clone());
            }
        }

        let keys = self
            .event_keys_by_block
            .entry(event.block_number)
            .or_default();
        if !keys.iter().any(|key| key == &event.event_key) {
            keys.push(event.event_key.clone());
        }

        None
    }
}

struct IndexerStateStore {
    client: Client,
}

impl IndexerStateStore {
    async fn from_env() -> Option<Self> {
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) => url,
            Err(_) => return None,
        };

        match Self::from_database_url(&database_url).await {
            Ok(store) => Some(store),
            Err(err) => {
                warn!(error = ?err, "failed to initialize durable indexer store; falling back to legacy polling mode");
                None
            }
        }
    }

    async fn from_database_url(database_url: &str) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;

        tokio::spawn(async move {
            if let Err(err) = connection.await {
                tracing::error!(error = ?err, "indexer postgres background connection error");
            }
        });

        let store = Self { client };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> Result<()> {
        self.client
            .batch_execute(
                r#"
                CREATE TABLE IF NOT EXISTS indexer_state (
                    chain TEXT PRIMARY KEY,
                    last_indexed_block BIGINT NOT NULL,
                    last_block_hash TEXT NOT NULL,
                    last_block_timestamp TIMESTAMPTZ,
                    processed_events_count BIGINT NOT NULL DEFAULT 0,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                );

                CREATE TABLE IF NOT EXISTS processed_events (
                    event_id UUID PRIMARY KEY,
                    tx_hash TEXT NOT NULL,
                    block_number BIGINT NOT NULL,
                    block_hash TEXT NOT NULL,
                    chain TEXT NOT NULL,
                    processed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    reverted BOOLEAN NOT NULL DEFAULT FALSE,
                    UNIQUE (tx_hash, block_number, chain)
                );

                ALTER TABLE processed_events
                    ADD COLUMN IF NOT EXISTS event_key TEXT;

                CREATE UNIQUE INDEX IF NOT EXISTS idx_processed_events_event_key
                    ON processed_events (event_key);

                CREATE INDEX IF NOT EXISTS idx_processed_events_chain_block
                    ON processed_events (chain, block_number);

                INSERT INTO indexer_state (chain, last_indexed_block, last_block_hash)
                VALUES
                    ('ethereum', 0, '0x0000000000000000000000000000000000000000000000000000000000000000'),
                    ('base', 0, '0x0000000000000000000000000000000000000000000000000000000000000000')
                ON CONFLICT (chain) DO NOTHING;
                "#,
            )
            .await?;

        Ok(())
    }

    async fn load_or_init_chain_state(&self, chain_slug: &str) -> Result<(u64, String)> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT last_indexed_block, last_block_hash
                FROM indexer_state
                WHERE chain = $1
                "#,
                &[&chain_slug],
            )
            .await?;

        if let Some(row) = row {
            let last_indexed_block: i64 = row.get(0);
            let last_block_hash: String = row.get(1);
            return Ok((
                u64::try_from(last_indexed_block).unwrap_or(0),
                last_block_hash,
            ));
        }

        self.upsert_chain_state(chain_slug, 0, ZERO_BLOCK_HASH, 0)
            .await?;
        Ok((0, ZERO_BLOCK_HASH.to_string()))
    }

    async fn upsert_chain_state(
        &self,
        chain_slug: &str,
        last_indexed_block: u64,
        last_block_hash: &str,
        processed_delta: i64,
    ) -> Result<()> {
        let last_indexed_block = to_i64(last_indexed_block, "last_indexed_block")?;

        self.client
            .execute(
                r#"
                INSERT INTO indexer_state (chain, last_indexed_block, last_block_hash, processed_events_count)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (chain) DO UPDATE
                SET last_indexed_block = EXCLUDED.last_indexed_block,
                    last_block_hash = EXCLUDED.last_block_hash,
                    processed_events_count = indexer_state.processed_events_count + EXCLUDED.processed_events_count,
                    updated_at = NOW()
                "#,
                &[&chain_slug, &last_indexed_block, &last_block_hash, &processed_delta],
            )
            .await?;

        Ok(())
    }

    async fn is_event_processed(&self, event_key: &str) -> Result<bool> {
        let row = self
            .client
            .query_opt(
                r#"
                SELECT 1
                FROM processed_events
                WHERE event_key = $1
                  AND reverted = FALSE
                LIMIT 1
                "#,
                &[&event_key],
            )
            .await?;

        Ok(row.is_some())
    }

    async fn has_conflicting_block_hash(
        &self,
        chain_slug: &str,
        block_number: u64,
        incoming_hash: &str,
    ) -> Result<bool> {
        let block_number = to_i64(block_number, "block_number")?;
        let row = self
            .client
            .query_opt(
                r#"
                SELECT 1
                FROM processed_events
                WHERE chain = $1
                  AND block_number = $2
                  AND reverted = FALSE
                  AND block_hash <> $3
                LIMIT 1
                "#,
                &[&chain_slug, &block_number, &incoming_hash],
            )
            .await?;

        Ok(row.is_some())
    }

    async fn mark_reverted_from_block(
        &self,
        chain_slug: &str,
        orphaned_from_block: u64,
    ) -> Result<Vec<String>> {
        let orphaned_from_block = to_i64(orphaned_from_block, "orphaned_from_block")?;

        let rows = self
            .client
            .query(
                r#"
                UPDATE processed_events
                SET reverted = TRUE
                WHERE chain = $1
                  AND block_number >= $2
                  AND reverted = FALSE
                RETURNING event_key
                "#,
                &[&chain_slug, &orphaned_from_block],
            )
            .await?;

        let mut affected = Vec::new();
        for row in rows {
            if let Ok(Some(event_key)) = row.try_get::<_, Option<String>>(0) {
                affected.push(event_key);
            }
        }

        Ok(affected)
    }

    async fn upsert_processed_event(&self, event: &NormalizedEvent) -> Result<()> {
        let block_number = to_i64(event.block_number, "event.block_number")?;
        let block_hash = event.block_hash.as_deref().unwrap_or(ZERO_BLOCK_HASH);

        self.client
            .execute(
                r#"
                INSERT INTO processed_events (
                    event_id,
                    event_key,
                    tx_hash,
                    block_number,
                    block_hash,
                    chain,
                    reverted
                )
                VALUES ($1::uuid, $2, $3, $4, $5, $6, FALSE)
                ON CONFLICT (event_key) DO UPDATE
                SET block_hash = EXCLUDED.block_hash,
                    reverted = FALSE,
                    processed_at = NOW()
                "#,
                &[
                    &event.event_id.to_string(),
                    &event.event_key,
                    &event.tx_hash,
                    &block_number,
                    &block_hash,
                    &event.chain_slug,
                ],
            )
            .await?;

        Ok(())
    }

    async fn try_acquire_chain_lock(&self, chain_slug: &str) -> Result<bool> {
        let row = self
            .client
            .query_one(
                r#"
                SELECT pg_try_advisory_lock($1::integer, hashtext($2))
                "#,
                &[&INGESTION_CHAIN_LOCK_NAMESPACE, &chain_slug],
            )
            .await?;

        Ok(row.get(0))
    }
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

    let mut adapters = build_adapters().await?;
    if adapters.is_empty() {
        warn!("no chain adapters available; exiting ingestion");
        return Ok(());
    }

    for runtime in adapters.iter_mut() {
        runtime.adapter.subscribe_heads().await?;
        runtime.adapter.subscribe_logs().await?;
    }

    let Some(stream) = init_stream_publisher().await else {
        warn!("REDIS_URL not set or unavailable; ingestion requires Redis Streams");
        return Ok(());
    };

    let poll_interval_secs = std::env::var("INGESTION_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INGESTION_POLL_INTERVAL_SECS);
    let run_once = std::env::var("INGESTION_RUN_ONCE")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let configured_batch_size = std::env::var("INGESTION_BLOCK_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INGESTION_BLOCK_BATCH_SIZE)
        .max(1);
    let max_retries = std::env::var("INGESTION_MAX_RETRIES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_INGESTION_MAX_RETRIES)
        .max(1);
    let retry_backoff_ms = std::env::var("INGESTION_RETRY_BACKOFF_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INGESTION_RETRY_BACKOFF_MS)
        .max(1);

    let state_store = IndexerStateStore::from_env().await;
    if state_store.is_none() {
        warn!("DATABASE_URL not set or unavailable; durable indexing disabled, using legacy lookback polling");
    }

    for runtime in adapters.iter_mut() {
        runtime.current_batch_size = configured_batch_size;
        if let Some(store) = state_store.as_ref() {
            let (last_indexed_block, last_block_hash) =
                store.load_or_init_chain_state(&runtime.chain_slug).await?;
            runtime.last_indexed_block = last_indexed_block;
            runtime.last_block_hash = last_block_hash;
            info!(
                chain_slug = %runtime.chain_slug,
                last_indexed_block = runtime.last_indexed_block,
                "loaded durable chain checkpoint",
            );
        }
    }

    let mut trackers: HashMap<String, ChainTrackerState> = HashMap::new();
    let mut chain_lock_state: HashMap<String, bool> = HashMap::new();
    info!(
        adapter_count = adapters.len(),
        poll_interval_secs,
        configured_batch_size,
        max_retries,
        retry_backoff_ms,
        "ingestion started"
    );

    loop {
        let mut emitted_count = 0usize;

        for runtime in adapters.iter_mut() {
            if let Some(store) = state_store.as_ref() {
                let has_lock = match store.try_acquire_chain_lock(&runtime.chain_slug).await {
                    Ok(acquired) => acquired,
                    Err(err) => {
                        warn!(
                            chain_slug = %runtime.chain_slug,
                            error = ?err,
                            "failed to evaluate chain advisory lock; skipping this iteration"
                        );
                        continue;
                    }
                };

                let previous = chain_lock_state.insert(runtime.chain_slug.clone(), has_lock);
                if previous != Some(has_lock) {
                    if has_lock {
                        info!(
                            chain_slug = %runtime.chain_slug,
                            lock_namespace = INGESTION_CHAIN_LOCK_NAMESPACE,
                            "acquired chain advisory lock"
                        );
                    } else {
                        info!(
                            chain_slug = %runtime.chain_slug,
                            lock_namespace = INGESTION_CHAIN_LOCK_NAMESPACE,
                            "chain advisory lock is owned by another indexer instance; skipping chain"
                        );
                    }
                }

                if !has_lock {
                    continue;
                }
            }

            match sync_runtime_chain(
                runtime,
                &stream,
                state_store.as_ref(),
                &mut trackers,
                max_retries,
                retry_backoff_ms,
            )
            .await
            {
                Ok(count) => emitted_count += count,
                Err(err) => warn!(
                    chain_slug = %runtime.chain_slug,
                    error = ?err,
                    "failed to sync chain adapter"
                ),
            }
        }

        if run_once {
            info!(
                emitted_count,
                "INGESTION_RUN_ONCE=true; stopping after first pass"
            );
            break;
        }

        if emitted_count == 0 {
            info!("no new normalized events in this polling interval");
        }
        tokio::time::sleep(Duration::from_secs(poll_interval_secs)).await;
    }

    Ok(())
}

async fn sync_runtime_chain(
    runtime: &mut RuntimeChainAdapter,
    stream: &RedisStreamPublisher,
    state_store: Option<&IndexerStateStore>,
    trackers: &mut HashMap<String, ChainTrackerState>,
    max_retries: usize,
    retry_backoff_ms: u64,
) -> Result<usize> {
    let latest_block_opt = runtime.adapter.latest_block_number().await?;

    if state_store.is_none() || latest_block_opt.is_none() {
        return legacy_poll_events(
            runtime.adapter.as_mut(),
            stream,
            state_store,
            trackers,
            &runtime.chain_slug,
        )
        .await;
    }

    let Some(latest_block) = latest_block_opt else {
        return Ok(0);
    };

    let safe_head = latest_block.saturating_sub(runtime.confirmation_depth);

    if runtime.last_indexed_block == 0 {
        let bootstrap_cursor = safe_head.saturating_sub(runtime.bootstrap_lookback_blocks);
        runtime.last_indexed_block = bootstrap_cursor;
        runtime.last_block_hash = ZERO_BLOCK_HASH.to_string();

        if let Some(store) = state_store {
            store
                .upsert_chain_state(
                    &runtime.chain_slug,
                    runtime.last_indexed_block,
                    &runtime.last_block_hash,
                    0,
                )
                .await?;
        }

        info!(
            chain_slug = %runtime.chain_slug,
            latest_block,
            safe_head,
            bootstrap_cursor,
            bootstrap_lookback_blocks = runtime.bootstrap_lookback_blocks,
            "initialized durable index cursor",
        );
    }

    if runtime.last_indexed_block >= safe_head {
        return Ok(0);
    }

    let mut emitted_total = 0usize;

    while runtime.last_indexed_block < safe_head {
        let from_block = runtime.last_indexed_block.saturating_add(1);
        let mut to_block = from_block
            .saturating_add(runtime.current_batch_size.saturating_sub(1))
            .min(safe_head);
        let mut single_block_attempt = 0usize;

        loop {
            match runtime.adapter.backfill_range(from_block, to_block).await {
                Ok(events) => {
                    let emitted =
                        publish_events(&events, stream, state_store, trackers, &runtime.chain_slug)
                            .await?;
                    emitted_total += emitted;

                    runtime.last_block_hash =
                        checkpoint_hash_after_range(&runtime.last_block_hash, to_block, &events);
                    runtime.last_indexed_block = to_block;

                    if let Some(store) = state_store {
                        let delta = i64::try_from(emitted).unwrap_or(i64::MAX);
                        store
                            .upsert_chain_state(
                                &runtime.chain_slug,
                                runtime.last_indexed_block,
                                &runtime.last_block_hash,
                                delta,
                            )
                            .await?;
                    }

                    break;
                }
                Err(err) => {
                    if to_block > from_block {
                        let current_window = to_block - from_block + 1;
                        let reduced_window = (current_window / 2).max(1);
                        to_block = from_block + reduced_window - 1;
                        runtime.current_batch_size =
                            runtime.current_batch_size.min(reduced_window).max(1);
                        warn!(
                            chain_slug = %runtime.chain_slug,
                            from_block,
                            to_block,
                            reduced_window,
                            error = ?err,
                            "backfill range failed; reducing range and retrying",
                        );
                        continue;
                    }

                    single_block_attempt = single_block_attempt.saturating_add(1);
                    if single_block_attempt >= max_retries {
                        return Err(anyhow!(
                            "failed indexing chain '{}' at block {} after {} attempts: {err}",
                            runtime.chain_slug,
                            from_block,
                            single_block_attempt
                        ));
                    }

                    let backoff_ms = retry_backoff_ms.saturating_mul(single_block_attempt as u64);
                    warn!(
                        chain_slug = %runtime.chain_slug,
                        block = from_block,
                        attempt = single_block_attempt,
                        backoff_ms,
                        error = ?err,
                        "single-block backfill failed; retrying",
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }

    Ok(emitted_total)
}

async fn legacy_poll_events(
    adapter: &mut dyn ChainAdapter,
    stream: &RedisStreamPublisher,
    state_store: Option<&IndexerStateStore>,
    trackers: &mut HashMap<String, ChainTrackerState>,
    chain_slug_hint: &str,
) -> Result<usize> {
    let events = match adapter.next_events().await {
        Ok(events) => events,
        Err(err) => {
            warn!(
                chain_slug = %chain_slug_hint,
                error = ?err,
                "failed to fetch events from adapter"
            );
            return Ok(0);
        }
    };

    publish_events(&events, stream, state_store, trackers, chain_slug_hint).await
}

async fn publish_events(
    events: &[NormalizedEvent],
    stream: &RedisStreamPublisher,
    state_store: Option<&IndexerStateStore>,
    trackers: &mut HashMap<String, ChainTrackerState>,
    chain_slug_hint: &str,
) -> Result<usize> {
    let mut emitted_count = 0usize;

    for event in events {
        if let Some(store) = state_store {
            if store.is_event_processed(&event.event_key).await? {
                continue;
            }

            if let Some(incoming_hash) = event.block_hash.as_deref() {
                if store
                    .has_conflicting_block_hash(
                        &event.chain_slug,
                        event.block_number,
                        incoming_hash,
                    )
                    .await?
                {
                    let affected_event_keys = store
                        .mark_reverted_from_block(&event.chain_slug, event.block_number)
                        .await?;
                    if !affected_event_keys.is_empty() {
                        let notice = ReorgNotice {
                            chain: event.chain.clone(),
                            orphaned_from_block: event.block_number,
                            common_ancestor_block: event.block_number.saturating_sub(1),
                            affected_event_keys,
                            noticed_at: Utc::now(),
                        };
                        warn!(
                            chain = ?notice.chain,
                            orphaned_from_block = notice.orphaned_from_block,
                            affected = notice.affected_event_keys.len(),
                            "detected persisted reorg from checkpointed data",
                        );
                        stream.publish_reorg_notice(&notice).await?;
                    }
                }
            }

            stream.publish_normalized_event(event).await?;
            store.upsert_processed_event(event).await?;
        } else {
            stream.publish_normalized_event(event).await?;
            let tracker = trackers.entry(event.chain_slug.clone()).or_default();
            if let Some(notice) = tracker.observe_event(event) {
                warn!(
                    chain = ?notice.chain,
                    orphaned_from_block = notice.orphaned_from_block,
                    affected = notice.affected_event_keys.len(),
                    "detected reorg notice from in-memory tracker",
                );
                stream.publish_reorg_notice(&notice).await?;
            }
        }

        emitted_count = emitted_count.saturating_add(1);
        info!(
            chain_slug = %event.chain_slug,
            block_number = event.block_number,
            tx_hash = %event.tx_hash,
            event_key = %event.event_key,
            "published normalized event"
        );
    }

    if emitted_count == 0 && !events.is_empty() {
        info!(
            chain_slug = %chain_slug_hint,
            total_events = events.len(),
            "all events in batch were already processed"
        );
    }

    Ok(emitted_count)
}

fn checkpoint_hash_after_range(
    current_hash: &str,
    to_block: u64,
    events: &[NormalizedEvent],
) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.block_number == to_block)
        .and_then(|event| event.block_hash.clone())
        .unwrap_or_else(|| current_hash.to_string())
}

fn to_i64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).with_context(|| format!("{field} exceeds i64 range"))
}

async fn init_stream_publisher() -> Option<RedisStreamPublisher> {
    let Some(publisher_result) = RedisStreamPublisher::from_env() else {
        return None;
    };

    let publisher = match publisher_result {
        Ok(publisher) => publisher,
        Err(err) => {
            warn!(error = ?err, "invalid REDIS_URL; redis publishing disabled");
            return None;
        }
    };

    if let Err(err) = publisher.healthcheck().await {
        warn!(error = ?err, "redis healthcheck failed; redis publishing disabled");
        None
    } else {
        Some(publisher)
    }
}

async fn build_adapters() -> Result<Vec<RuntimeChainAdapter>> {
    let rules_root = find_rules_repo_root();
    let env_lookback_override = std::env::var("LOOKBACK_BLOCKS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let env_decimals_override = std::env::var("ORACLE_DECIMALS")
        .ok()
        .and_then(|value| value.parse::<u8>().ok());

    if let Some(root) = rules_root.as_ref() {
        let discovered = discover_chain_configs(root)?;
        let mut adapters: Vec<RuntimeChainAdapter> = Vec::new();

        for discovered_chain in discovered {
            match build_chain_adapter_from_rules(
                discovered_chain,
                env_lookback_override,
                env_decimals_override,
            )
            .await
            {
                Ok(Some(adapter)) => adapters.push(adapter),
                Ok(None) => {}
                Err(err) => warn!(error = ?err, "failed to build chain adapter from rules config"),
            }
        }

        if !adapters.is_empty() {
            return Ok(adapters);
        }

        warn!("no chain adapters built from rules config; falling back to env adapters");
    } else {
        warn!("rules repo not found; using env fallback adapters");
    }

    build_legacy_env_fallback_adapters(env_lookback_override, env_decimals_override).await
}

async fn build_chain_adapter_from_rules(
    discovered: DiscoveredChainConfig,
    env_lookback_override: Option<u64>,
    env_decimals_override: Option<u8>,
) -> Result<Option<RuntimeChainAdapter>> {
    let chain = discovered.chain;

    if !chain.family.eq_ignore_ascii_case("evm") {
        warn!(
            chain_slug = %chain.chain_slug,
            family = %chain.family,
            "unsupported chain family; skipping",
        );
        return Ok(None);
    }

    chain.validate()?;
    let chain_slug = chain.chain_slug.clone();
    let chain_id = chain.chain_id;
    let confirmation_depth = confirmation_depth_for_chain(&chain_slug);

    let lookback_blocks = env_lookback_override
        .or(chain.lookback_blocks)
        .or(discovered.protocols.lookback_blocks)
        .unwrap_or(DEFAULT_LOOKBACK_BLOCKS);
    let default_oracle_decimals = env_decimals_override
        .or(chain.default_oracle_decimals)
        .or(discovered.protocols.default_oracle_decimals)
        .unwrap_or(DEFAULT_ORACLE_DECIMALS);
    let default_protocol_category = chain.protocol_category_default();

    let mut map_entries = Vec::new();
    let mut mock_protocols = Vec::new();

    for protocol in discovered
        .protocols
        .protocols
        .into_iter()
        .filter(|entry| entry.enabled)
    {
        let protocol_category = protocol
            .protocol_category
            .as_deref()
            .map(parse_protocol_category)
            .unwrap_or_else(|| default_protocol_category.clone());
        let oracle_decimals = protocol.oracle_decimals.unwrap_or(default_oracle_decimals);

        let oracle_addresses = if protocol.oracle_addresses.is_empty() && chain_slug == "ethereum" {
            default_eth_oracles()?
        } else {
            parse_oracle_addresses(&protocol.oracle_addresses)?
        };

        if oracle_addresses.is_empty() {
            continue;
        }

        map_entries.push((
            oracle_addresses,
            ProtocolBinding {
                protocol: protocol.protocol.clone(),
                oracle_decimals,
                protocol_category: protocol_category.clone(),
            },
        ));

        let (oracle_price, reference_price) = default_mock_prices(&chain_slug);
        mock_protocols.push(MockProtocol {
            protocol: protocol.protocol,
            protocol_category,
            oracle_price,
            reference_price,
        });
    }

    if map_entries.is_empty() {
        warn!(
            chain_slug = %chain_slug,
            chain_config = %discovered.chain_config_path.display(),
            protocol_config = %discovered.protocol_config_path.display(),
            "no enabled protocols with oracle addresses",
        );
        return Ok(None);
    }

    let ws_urls = resolve_chain_ws_urls(&chain);
    if ws_urls.is_empty() {
        info!(
            chain_slug = %chain_slug,
            ws_env_names = ?chain.ws_env_names(),
            "no chain websocket endpoints configured; using mock adapter",
        );
        return Ok(Some(RuntimeChainAdapter {
            chain_slug: chain_slug.clone(),
            confirmation_depth,
            bootstrap_lookback_blocks: lookback_blocks,
            current_batch_size: 0,
            last_indexed_block: 0,
            last_block_hash: ZERO_BLOCK_HASH.to_string(),
            adapter: Box::new(
                EvmMockAdapter::new(chain_slug.clone(), chain_id, mock_protocols)
                    .with_confirmation_depth(confirmation_depth),
            ),
        }));
    }

    let protocol_map = build_oracle_protocol_map(map_entries);
    match EvmChainAdapter::new_with_urls(
        ws_urls.clone(),
        &chain_slug,
        chain_id,
        lookback_blocks,
        protocol_map,
    )
    .await
    {
        Ok(adapter) => {
            info!(
                chain_slug = %chain_slug,
                ws_provider_count = ws_urls.len(),
                "using EVM live adapter with rpc failover",
            );
            Ok(Some(RuntimeChainAdapter {
                chain_slug: chain_slug.clone(),
                confirmation_depth,
                bootstrap_lookback_blocks: lookback_blocks,
                current_batch_size: 0,
                last_indexed_block: 0,
                last_block_hash: ZERO_BLOCK_HASH.to_string(),
                adapter: Box::new(
                    adapter
                        .with_filter_chunk_size(DEFAULT_FILTER_CHUNK_SIZE)
                        .with_confirmation_depth(confirmation_depth),
                ),
            }))
        }
        Err(err) => {
            warn!(
                chain_slug = %chain_slug,
                error = ?err,
                "failed to init EVM live adapter; falling back to mock",
            );
            Ok(Some(RuntimeChainAdapter {
                chain_slug: chain_slug.clone(),
                confirmation_depth,
                bootstrap_lookback_blocks: lookback_blocks,
                current_batch_size: 0,
                last_indexed_block: 0,
                last_block_hash: ZERO_BLOCK_HASH.to_string(),
                adapter: Box::new(
                    EvmMockAdapter::new(chain_slug.clone(), chain_id, mock_protocols)
                        .with_confirmation_depth(confirmation_depth),
                ),
            }))
        }
    }
}

async fn build_legacy_env_fallback_adapters(
    env_lookback_override: Option<u64>,
    env_decimals_override: Option<u8>,
) -> Result<Vec<RuntimeChainAdapter>> {
    let mut adapters: Vec<RuntimeChainAdapter> = Vec::new();

    let lookback_blocks = env_lookback_override.unwrap_or(DEFAULT_LOOKBACK_BLOCKS);
    let oracle_decimals = env_decimals_override.unwrap_or(DEFAULT_ORACLE_DECIMALS);

    let eth_protocol = std::env::var("ETH_PROTOCOL").unwrap_or_else(|_| "aave-v3".to_string());
    let eth_confirmation_depth = confirmation_depth_for_chain("ethereum");
    let eth_ws_urls = resolve_legacy_ws_urls("ETH_WS_URLS", "ETH_WS_URL");
    let eth_adapter: Box<dyn ChainAdapter> = if !eth_ws_urls.is_empty() {
        let oracle_addresses = match std::env::var("ETH_ORACLE_ADDRESSES") {
            Ok(raw) => parse_oracle_addresses_csv(&raw)?,
            Err(_) => default_eth_oracles()?,
        };

        let protocol_map = build_oracle_protocol_map(oracle_addresses.into_iter().map(|address| {
            (
                vec![address],
                ProtocolBinding {
                    protocol: eth_protocol.clone(),
                    oracle_decimals,
                    protocol_category: event_schema::ProtocolCategory::Lending,
                },
            )
        }));

        match EvmChainAdapter::new_with_urls(
            eth_ws_urls.clone(),
            "ethereum",
            1,
            lookback_blocks,
            protocol_map,
        )
        .await
        {
            Ok(adapter) => Box::new(
                adapter
                    .with_filter_chunk_size(DEFAULT_FILTER_CHUNK_SIZE)
                    .with_confirmation_depth(eth_confirmation_depth),
            ),
            Err(err) => {
                warn!(error = ?err, "failed to init ETH live adapter; falling back to mock");
                Box::new(
                    EvmMockAdapter::single_protocol(
                        "ethereum",
                        1,
                        eth_protocol,
                        event_schema::ProtocolCategory::Lending,
                        3280.0,
                        3120.0,
                    )
                    .with_confirmation_depth(eth_confirmation_depth),
                )
            }
        }
    } else {
        Box::new(
            EvmMockAdapter::single_protocol(
                "ethereum",
                1,
                eth_protocol,
                event_schema::ProtocolCategory::Lending,
                3280.0,
                3120.0,
            )
            .with_confirmation_depth(eth_confirmation_depth),
        )
    };
    adapters.push(RuntimeChainAdapter {
        chain_slug: "ethereum".to_string(),
        confirmation_depth: eth_confirmation_depth,
        bootstrap_lookback_blocks: lookback_blocks,
        current_batch_size: 0,
        last_indexed_block: 0,
        last_block_hash: ZERO_BLOCK_HASH.to_string(),
        adapter: eth_adapter,
    });

    let base_protocol =
        std::env::var("BASE_PROTOCOL").unwrap_or_else(|_| "perp-protocol-base".to_string());
    let base_confirmation_depth = confirmation_depth_for_chain("base");
    let base_ws_urls = resolve_legacy_ws_urls("BASE_WS_URLS", "BASE_WS_URL");
    let base_adapter: Box<dyn ChainAdapter> = if !base_ws_urls.is_empty() {
        let oracle_addresses = match std::env::var("BASE_ORACLE_ADDRESSES") {
            Ok(raw) => parse_oracle_addresses_csv(&raw)?,
            Err(_) => Vec::new(),
        };

        if oracle_addresses.is_empty() {
            Box::new(
                EvmMockAdapter::single_protocol(
                    "base",
                    8453,
                    base_protocol,
                    event_schema::ProtocolCategory::PerpDex,
                    3010.0,
                    3000.0,
                )
                .with_confirmation_depth(base_confirmation_depth),
            )
        } else {
            let protocol_map =
                build_oracle_protocol_map(oracle_addresses.into_iter().map(|address| {
                    (
                        vec![address],
                        ProtocolBinding {
                            protocol: base_protocol.clone(),
                            oracle_decimals,
                            protocol_category: event_schema::ProtocolCategory::PerpDex,
                        },
                    )
                }));

            match EvmChainAdapter::new_with_urls(
                base_ws_urls.clone(),
                "base",
                8453,
                lookback_blocks,
                protocol_map,
            )
            .await
            {
                Ok(adapter) => Box::new(
                    adapter
                        .with_filter_chunk_size(DEFAULT_FILTER_CHUNK_SIZE)
                        .with_confirmation_depth(base_confirmation_depth),
                ),
                Err(err) => {
                    warn!(error = ?err, "failed to init Base live adapter; falling back to mock");
                    Box::new(
                        EvmMockAdapter::single_protocol(
                            "base",
                            8453,
                            base_protocol,
                            event_schema::ProtocolCategory::PerpDex,
                            3010.0,
                            3000.0,
                        )
                        .with_confirmation_depth(base_confirmation_depth),
                    )
                }
            }
        }
    } else {
        Box::new(
            EvmMockAdapter::single_protocol(
                "base",
                8453,
                base_protocol,
                event_schema::ProtocolCategory::PerpDex,
                3010.0,
                3000.0,
            )
            .with_confirmation_depth(base_confirmation_depth),
        )
    };
    adapters.push(RuntimeChainAdapter {
        chain_slug: "base".to_string(),
        confirmation_depth: base_confirmation_depth,
        bootstrap_lookback_blocks: lookback_blocks,
        current_batch_size: 0,
        last_indexed_block: 0,
        last_block_hash: ZERO_BLOCK_HASH.to_string(),
        adapter: base_adapter,
    });

    Ok(adapters)
}

fn default_mock_prices(chain_slug: &str) -> (f64, f64) {
    match chain_slug {
        "ethereum" => (3280.0, 3120.0),
        "base" => (3010.0, 3000.0),
        _ => (100.0, 99.0),
    }
}

fn resolve_chain_ws_urls(chain: &EvmChainConfig) -> Vec<String> {
    let mut urls = Vec::new();
    for env_name in chain.ws_env_names() {
        match std::env::var(&env_name) {
            Ok(url) => {
                let normalized = url.trim().to_string();
                if !normalized.is_empty() && !urls.contains(&normalized) {
                    urls.push(normalized);
                }
            }
            Err(_) => {
                info!(
                    chain_slug = %chain.chain_slug,
                    ws_env = %env_name,
                    "rpc websocket env not set for chain"
                );
            }
        }
    }
    urls
}

fn resolve_legacy_ws_urls(csv_env: &str, single_env: &str) -> Vec<String> {
    let mut urls = Vec::new();

    if let Ok(csv) = std::env::var(csv_env) {
        for item in csv.split(',') {
            let normalized = item.trim().to_string();
            if !normalized.is_empty() && !urls.contains(&normalized) {
                urls.push(normalized);
            }
        }
    }

    if let Ok(single) = std::env::var(single_env) {
        let normalized = single.trim().to_string();
        if !normalized.is_empty() && !urls.contains(&normalized) {
            urls.push(normalized);
        }
    }

    urls
}

fn discover_chain_configs(rules_root: &Path) -> Result<Vec<DiscoveredChainConfig>> {
    let chains_root = rules_root.join("chains");
    if !chains_root.exists() {
        return Ok(Vec::new());
    }

    let mut discovered = Vec::new();

    for entry in fs::read_dir(&chains_root)
        .with_context(|| format!("failed to read chains directory {}", chains_root.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }

        let chain_config_path = entry_path.join("chain_config.yaml");
        let protocol_config_path = entry_path.join("protocol_config.yaml");
        if !chain_config_path.exists() || !protocol_config_path.exists() {
            continue;
        }

        discovered.push(DiscoveredChainConfig {
            chain: load_yaml_file(&chain_config_path)?,
            protocols: load_yaml_file(&protocol_config_path)?,
            chain_config_path,
            protocol_config_path,
        });
    }

    discovered.sort_by(|a, b| a.chain.chain_slug.cmp(&b.chain.chain_slug));
    Ok(discovered)
}

fn load_yaml_file<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn find_rules_repo_root() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RULES_REPO_PATH") {
        let path = PathBuf::from(explicit);
        if path.exists() {
            return Some(path);
        }
    }

    for candidate in DEFAULT_RULE_RELATIVE_ROOTS {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }

    None
}

fn confirmation_depth_for_chain(chain_slug: &str) -> u64 {
    let chain_specific_env = match chain_slug {
        "ethereum" => "ETH_CONFIRMATION_DEPTH",
        "base" => "BASE_CONFIRMATION_DEPTH",
        _ => "CONFIRMATION_DEPTH",
    };

    std::env::var(chain_specific_env)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| match chain_slug {
            "base" => 64,
            _ => 3,
        })
        .max(1)
}
