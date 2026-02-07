use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use alert_client::{DiscordSink, SlackSink, TelegramSink, WebhookSink};
use anyhow::{Context, Result};
use chain_adapter_evm::{
    build_oracle_protocol_map, default_eth_oracles, parse_oracle_addresses,
    parse_oracle_addresses_csv, parse_protocol_category, EvmChainAdapter, EvmChainConfig,
    EvmMockAdapter, EvmProtocolConfigFile, MockProtocol, ProtocolBinding,
    DEFAULT_FILTER_CHUNK_SIZE, DEFAULT_LOOKBACK_BLOCKS, DEFAULT_ORACLE_DECIMALS,
};
use chrono::Utc;
use common_types::{AlertEvent, NormalizedEvent, ProtocolCategory};
use core_interfaces::{AlertSink, ChainAdapter, RiskScorer, RuleEvaluator};
use dotenv::dotenv;
use ethers::types::Address;
use event_stream::RedisStreamPublisher;
use persistence::PostgresRepository;
use reference_price::{LiveReferencePriceProvider, ReferencePriceProvider};
use risk_scorer::DeterministicRiskScorer;
use rule_engine::{RuleDefinition, RuleEngine};
use serde::de::DeserializeOwned;
use trace_enricher::enrich_with_trace;
use tracing::{info, warn};
use uuid::Uuid;

const DEFAULT_RULE_RELATIVE_ROOTS: [&str; 3] = [
    "../defi-surv-rules",
    "../../defi-surv-rules",
    "../../../defi-surv-rules",
];
const DEFAULT_STREAM_BATCH_SIZE: usize = 100;
const DEFAULT_STREAM_BLOCK_MS: usize = 1000;

type DynReferencePriceProvider = Arc<dyn ReferencePriceProvider>;

#[derive(Debug)]
struct DiscoveredChainConfig {
    chain_config_path: PathBuf,
    protocol_config_path: PathBuf,
    chain: EvmChainConfig,
    protocols: EvmProtocolConfigFile,
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

    info!("starting detector-worker");

    let evaluator = load_rule_engine()?;
    let risk_scorer = DeterministicRiskScorer;
    let emit_alerts = std::env::var("DETECTOR_EMIT_ALERTS")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let sinks: Vec<Box<dyn AlertSink>> = vec![
        Box::new(WebhookSink::default()),
        Box::new(SlackSink::default()),
        Box::new(TelegramSink::default()),
        Box::new(DiscordSink::default()),
    ];

    let repository = init_repository().await;
    let stream_publisher = init_stream_publisher().await;
    let reference_price_provider = init_reference_price_provider();
    let oracle_pair_overrides = load_oracle_pair_overrides();
    if should_use_stream_mode() {
        if let Some(stream) = stream_publisher.as_ref() {
            info!("detector-worker running in stream-consumer mode");
            run_stream_mode(
                stream,
                &evaluator,
                &risk_scorer,
                &sinks,
                repository.as_ref(),
                emit_alerts,
                reference_price_provider.as_deref(),
                &oracle_pair_overrides,
            )
            .await?;
            return Ok(());
        }
        warn!(
            "stream mode requested but REDIS_URL is not available; falling back to direct adapter mode"
        );
    }

    let mut adapters = build_adapters().await?;
    for adapter in adapters.iter_mut() {
        info!(chain = adapter.chain_name(), "pulling events from adapter");
        let events = adapter.next_events().await?;
        for event in events {
            process_event(
                event,
                &evaluator,
                &risk_scorer,
                &sinks,
                repository.as_ref(),
                stream_publisher.as_ref(),
                emit_alerts,
                reference_price_provider.as_deref(),
                &oracle_pair_overrides,
            )
            .await?;
        }
    }

    Ok(())
}

async fn run_stream_mode(
    stream: &RedisStreamPublisher,
    evaluator: &RuleEngine,
    risk_scorer: &DeterministicRiskScorer,
    sinks: &[Box<dyn AlertSink>],
    repository: Option<&PostgresRepository>,
    emit_alerts: bool,
    reference_price_provider: Option<&dyn ReferencePriceProvider>,
    oracle_pair_overrides: &HashMap<String, String>,
) -> Result<()> {
    let batch_size = std::env::var("DETECTOR_STREAM_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_STREAM_BATCH_SIZE);
    let block_ms = std::env::var("DETECTOR_STREAM_BLOCK_MS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_STREAM_BLOCK_MS);
    let run_once = std::env::var("DETECTOR_STREAM_RUN_ONCE")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let use_consumer_group = std::env::var("DETECTOR_STREAM_USE_CONSUMER_GROUP")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(true);

    if use_consumer_group {
        let group = std::env::var("DETECTOR_STREAM_GROUP")
            .unwrap_or_else(|_| "detector-workers".to_string());
        let consumer = std::env::var("DETECTOR_STREAM_CONSUMER")
            .unwrap_or_else(|_| default_consumer_name("detector"));
        stream.ensure_normalized_events_group(&group).await?;
        info!(group = %group, consumer = %consumer, "detector stream consumer-group mode enabled");

        loop {
            let entries = stream
                .read_normalized_events_group(&group, &consumer, batch_size, block_ms)
                .await?;
            if entries.is_empty() {
                if run_once {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }

            for (entry_id, event) in entries {
                process_event(
                    event,
                    evaluator,
                    risk_scorer,
                    sinks,
                    repository,
                    Some(stream),
                    emit_alerts,
                    reference_price_provider,
                    oracle_pair_overrides,
                )
                .await?;
                stream.ack_normalized_event(&group, &entry_id).await?;
            }

            if run_once {
                break;
            }
        }

        return Ok(());
    }

    let mut last_id =
        std::env::var("DETECTOR_STREAM_START_ID").unwrap_or_else(|_| "0-0".to_string());

    loop {
        let entries = stream
            .read_normalized_events(&last_id, batch_size, block_ms)
            .await?;
        if entries.is_empty() {
            if run_once {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        for (entry_id, event) in entries {
            last_id = entry_id;
            process_event(
                event,
                evaluator,
                risk_scorer,
                sinks,
                repository,
                Some(stream),
                emit_alerts,
                reference_price_provider,
                oracle_pair_overrides,
            )
            .await?;
        }

        if run_once {
            break;
        }
    }

    Ok(())
}

fn default_consumer_name(prefix: &str) -> String {
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string());
    format!("{prefix}-{hostname}")
}

async fn process_event(
    mut event: NormalizedEvent,
    evaluator: &RuleEngine,
    risk_scorer: &DeterministicRiskScorer,
    sinks: &[Box<dyn AlertSink>],
    repository: Option<&PostgresRepository>,
    stream_publisher: Option<&RedisStreamPublisher>,
    emit_alerts: bool,
    reference_price_provider: Option<&dyn ReferencePriceProvider>,
    oracle_pair_overrides: &HashMap<String, String>,
) -> Result<()> {
    enrich_reference_price(&mut event, reference_price_provider, oracle_pair_overrides).await;

    let mut detection = evaluator.evaluate(&event).await?;
    detection = enrich_with_trace(detection)?;
    detection.risk_score = risk_scorer.score(&detection).await?;

    if let Some(repo) = repository {
        if let Err(err) = repo.save_detection(&detection).await {
            warn!(error = ?err, "failed to persist detection");
        }
    }
    if let Some(stream) = stream_publisher {
        if let Err(err) = stream.publish_detection(&detection).await {
            warn!(error = ?err, "failed to publish detection to redis stream");
        }
    }

    if detection.triggered_rule_ids.is_empty() {
        info!(tx = %detection.tx_hash, "no triggered rules, skipping alert");
        return Ok(());
    }
    if !emit_alerts {
        return Ok(());
    }

    let alert = alert_from_detection(&detection);

    for sink in sinks {
        if let Err(err) = sink.send(&alert).await {
            warn!(sink = sink.sink_name(), error = ?err, "failed to send alert to sink");
        }
    }

    if let Some(repo) = repository {
        if let Err(err) = repo.save_alert(&alert).await {
            warn!(error = ?err, "failed to persist alert");
        }
        if let Err(err) = repo.save_alert_lifecycle(&alert).await {
            warn!(error = ?err, "failed to persist lifecycle alert");
        }
    }
    if let Some(stream) = stream_publisher {
        if let Err(err) = stream.publish_alert(&alert).await {
            warn!(error = ?err, "failed to publish alert to redis stream");
        }
        if let Err(err) = stream.publish_alert_lifecycle(&alert).await {
            warn!(error = ?err, "failed to publish alert lifecycle to redis stream");
        }
    }

    info!(tx = %alert.tx_hash, score = alert.risk_score, "alert pipeline completed");
    Ok(())
}

fn alert_from_detection(detection: &common_types::DetectionResult) -> AlertEvent {
    AlertEvent {
        alert_id: Uuid::new_v4(),
        incident_id: None,
        event_key: detection.event_key.clone(),
        tenant_id: detection.tenant_id.clone(),
        chain: detection.chain.clone(),
        chain_slug: detection.chain_slug.clone(),
        protocol: detection.protocol.clone(),
        lifecycle_state: detection.lifecycle_state.clone(),
        severity: detection.severity.clone(),
        risk_score: detection.risk_score.score,
        confidence: detection.risk_score.confidence,
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
        tx_hash: detection.tx_hash.clone(),
        block_number: detection.block_number,
        oracle_context: detection.oracle_context.clone(),
        actions_recommended: detection.actions_recommended.clone(),
        created_at: Utc::now(),
    }
}

fn should_use_stream_mode() -> bool {
    std::env::var("DETECTOR_USE_STREAM_MODE")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(true)
}

fn load_rule_engine() -> Result<RuleEngine> {
    if let Some(root) = find_rules_repo_root() {
        let rule_files = discover_rule_files(&root.join("chains"))?;
        if !rule_files.is_empty() {
            info!(
                rules_root = %root.display(),
                rule_count = rule_files.len(),
                "loading rules from rules repository",
            );
            return RuleEngine::from_paths(rule_files);
        }
        warn!(
            rules_root = %root.display(),
            "rules repository found but no rule files discovered",
        );
    }

    warn!("rules not found; using inline fallback rule");
    Ok(RuleEngine::with_rules(vec![RuleDefinition {
        id: "oracle-divergence-default".to_string(),
        version: 1,
        enabled: true,
        chain: "ethereum".to_string(),
        protocol: "aave-v3".to_string(),
        signals: vec!["oracle_divergence".to_string()],
        conditions: HashMap::from([("divergence_pct_gt".to_string(), 5.0)]),
        window_ms: 60000,
        severity: "high".to_string(),
        cooldown_sec: 300,
        routing_tags: vec!["mvp".to_string()],
    }]))
}

async fn build_adapters() -> Result<Vec<Box<dyn ChainAdapter>>> {
    let rules_root = find_rules_repo_root();

    let env_lookback_override = std::env::var("LOOKBACK_BLOCKS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let env_decimals_override = std::env::var("ORACLE_DECIMALS")
        .ok()
        .and_then(|value| value.parse::<u8>().ok());

    if let Some(root) = rules_root.as_ref() {
        let discovered = discover_chain_configs(root)?;
        let mut adapters: Vec<Box<dyn ChainAdapter>> = Vec::new();

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
) -> Result<Option<Box<dyn ChainAdapter>>> {
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
    let confirmation_depth = confirmation_depth_for_chain(&chain.chain_slug);

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
        if let Some(legacy_ws_env) = protocol.ws_url_env.as_deref() {
            if legacy_ws_env != chain.ws_url_env {
                warn!(
                    chain_slug = %chain.chain_slug,
                    protocol = %protocol.protocol,
                    legacy_ws_env = %legacy_ws_env,
                    chain_ws_env = %chain.ws_url_env,
                    "protocol-level ws_url_env is deprecated; chain-level ws_url_env is used",
                );
            }
        }

        let protocol_category = protocol
            .protocol_category
            .as_deref()
            .map(parse_protocol_category)
            .unwrap_or_else(|| default_protocol_category.clone());
        let oracle_decimals = protocol.oracle_decimals.unwrap_or(default_oracle_decimals);

        let oracle_addresses =
            if protocol.oracle_addresses.is_empty() && chain.chain_slug == "ethereum" {
                default_eth_oracles()?
            } else {
                parse_oracle_addresses(&protocol.oracle_addresses)?
            };

        if oracle_addresses.is_empty() {
            warn!(
                chain_slug = %chain.chain_slug,
                protocol = %protocol.protocol,
                "protocol has no oracle addresses; skipping",
            );
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

        let (oracle_price, reference_price) = default_mock_prices(&chain.chain_slug);
        mock_protocols.push(MockProtocol {
            protocol: protocol.protocol,
            protocol_category,
            oracle_price,
            reference_price,
        });
    }

    if map_entries.is_empty() {
        warn!(
            chain_slug = %chain.chain_slug,
            chain_config = %discovered.chain_config_path.display(),
            protocol_config = %discovered.protocol_config_path.display(),
            "no enabled protocols with oracle addresses",
        );
        return Ok(None);
    }

    let ws_urls = resolve_chain_ws_urls(&chain);
    if ws_urls.is_empty() {
        info!(
            chain_slug = %chain.chain_slug,
            ws_env_names = ?chain.ws_env_names(),
            "no chain websocket endpoints configured; using mock adapter",
        );
        return Ok(Some(Box::new(
            EvmMockAdapter::new(chain.chain_slug, chain.chain_id, mock_protocols)
                .with_confirmation_depth(confirmation_depth),
        )));
    }

    let protocol_map = build_oracle_protocol_map(map_entries);
    match EvmChainAdapter::new_with_urls(
        ws_urls.clone(),
        &chain.chain_slug,
        chain.chain_id,
        lookback_blocks,
        protocol_map,
    )
    .await
    {
        Ok(adapter) => {
            info!(
                chain_slug = %chain.chain_slug,
                chain_id = chain.chain_id,
                ws_provider_count = ws_urls.len(),
                "using EVM live adapter with rpc failover",
            );
            Ok(Some(Box::new(
                adapter
                    .with_filter_chunk_size(DEFAULT_FILTER_CHUNK_SIZE)
                    .with_confirmation_depth(confirmation_depth),
            )))
        }
        Err(err) => {
            warn!(
                chain_slug = %chain.chain_slug,
                error = ?err,
                "failed to init EVM live adapter; falling back to mock",
            );
            Ok(Some(Box::new(
                EvmMockAdapter::new(chain.chain_slug, chain.chain_id, mock_protocols)
                    .with_confirmation_depth(confirmation_depth),
            )))
        }
    }
}

async fn build_legacy_env_fallback_adapters(
    env_lookback_override: Option<u64>,
    env_decimals_override: Option<u8>,
) -> Result<Vec<Box<dyn ChainAdapter>>> {
    let mut adapters: Vec<Box<dyn ChainAdapter>> = Vec::new();

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
                    protocol_category: ProtocolCategory::Lending,
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
            Ok(adapter) => {
                info!("using ETH live adapter (env fallback)");
                Box::new(
                    adapter
                        .with_filter_chunk_size(DEFAULT_FILTER_CHUNK_SIZE)
                        .with_confirmation_depth(eth_confirmation_depth),
                )
            }
            Err(err) => {
                warn!(error = ?err, "failed to init ETH live adapter; falling back to mock");
                Box::new(
                    EvmMockAdapter::single_protocol(
                        "ethereum",
                        1,
                        eth_protocol,
                        ProtocolCategory::Lending,
                        3280.0,
                        3120.0,
                    )
                    .with_confirmation_depth(eth_confirmation_depth),
                )
            }
        }
    } else {
        info!("ETH_WS_URL/ETH_WS_URLS not set; using ETH mock adapter");
        Box::new(
            EvmMockAdapter::single_protocol(
                "ethereum",
                1,
                eth_protocol,
                ProtocolCategory::Lending,
                3280.0,
                3120.0,
            )
            .with_confirmation_depth(eth_confirmation_depth),
        )
    };
    adapters.push(eth_adapter);

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
            warn!("BASE_ORACLE_ADDRESSES not set; falling back to base mock adapter");
            Box::new(
                EvmMockAdapter::single_protocol(
                    "base",
                    8453,
                    base_protocol,
                    ProtocolCategory::PerpDex,
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
                            protocol_category: ProtocolCategory::PerpDex,
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
                Ok(adapter) => {
                    info!("using Base live adapter (env fallback)");
                    Box::new(
                        adapter
                            .with_filter_chunk_size(DEFAULT_FILTER_CHUNK_SIZE)
                            .with_confirmation_depth(base_confirmation_depth),
                    )
                }
                Err(err) => {
                    warn!(error = ?err, "failed to init Base live adapter; falling back to mock");
                    Box::new(
                        EvmMockAdapter::single_protocol(
                            "base",
                            8453,
                            base_protocol,
                            ProtocolCategory::PerpDex,
                            3010.0,
                            3000.0,
                        )
                        .with_confirmation_depth(base_confirmation_depth),
                    )
                }
            }
        }
    } else {
        info!("BASE_WS_URL/BASE_WS_URLS not set; using Base mock adapter");
        Box::new(
            EvmMockAdapter::single_protocol(
                "base",
                8453,
                base_protocol,
                ProtocolCategory::PerpDex,
                3010.0,
                3000.0,
            )
            .with_confirmation_depth(base_confirmation_depth),
        )
    };
    adapters.push(base_adapter);

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

fn init_reference_price_provider() -> Option<DynReferencePriceProvider> {
    if !env_bool("REFERENCE_PRICE_ENABLED", true) {
        info!("reference-price enrichment disabled");
        return None;
    }

    let binance_enabled = env_bool("BINANCE_ENABLED", true);
    let uniswap_enabled = env_bool("UNISWAP_ENABLED", false);
    let http_rpc_url = std::env::var("REFERENCE_HTTP_RPC_URL")
        .ok()
        .or_else(|| std::env::var("ETH_RPC_HTTP_URL").ok())
        .or_else(|| std::env::var("ALCHEMY_HTTP_URL").ok());
    let uniswap_pools = parse_reference_uniswap_pools_env();

    match LiveReferencePriceProvider::new(
        http_rpc_url,
        uniswap_pools,
        binance_enabled,
        uniswap_enabled,
    ) {
        Ok(provider) => {
            info!(
                binance_enabled,
                uniswap_enabled, "reference-price enrichment enabled"
            );
            Some(Arc::new(provider))
        }
        Err(err) => {
            warn!(
                error = ?err,
                "failed to initialize live reference-price provider; reference enrichment disabled"
            );
            None
        }
    }
}

fn parse_reference_uniswap_pools_env() -> HashMap<String, Address> {
    let mut pools = HashMap::new();

    let Ok(raw) = std::env::var("REFERENCE_UNISWAP_POOLS") else {
        return pools;
    };

    for entry in raw.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some((pair, address_raw)) = trimmed.split_once('=') else {
            warn!(
                entry = %trimmed,
                "invalid REFERENCE_UNISWAP_POOLS entry; expected PAIR=ADDRESS"
            );
            continue;
        };

        match Address::from_str(address_raw.trim()) {
            Ok(address) => {
                pools.insert(pair.trim().to_string(), address);
            }
            Err(err) => {
                warn!(
                    pair = %pair.trim(),
                    address = %address_raw.trim(),
                    error = ?err,
                    "invalid uniswap pool address in REFERENCE_UNISWAP_POOLS"
                );
            }
        }
    }

    pools
}

fn load_oracle_pair_overrides() -> HashMap<String, String> {
    let mut map = default_oracle_pair_map();
    let Ok(raw) = std::env::var("ORACLE_PAIR_OVERRIDES") else {
        return map;
    };

    for entry in raw.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some((oracle, pair)) = trimmed.split_once('=') else {
            warn!(
                entry = %trimmed,
                "invalid ORACLE_PAIR_OVERRIDES entry; expected ORACLE_ADDRESS=PAIR"
            );
            continue;
        };

        map.insert(normalize_address_key(oracle), pair.trim().to_string());
    }

    map
}

fn default_oracle_pair_map() -> HashMap<String, String> {
    HashMap::from([
        (
            "0x5f4ec3df9cbd43714fe2740f5e3616155c5b8419".to_string(),
            "ETH/USD".to_string(),
        ),
        (
            "0xf4030086522a5beea4988f8ca5b36dbc97bee88c".to_string(),
            "BTC/USD".to_string(),
        ),
        (
            "0x8fffffd4afb6115b954bd326cbe7b4ba576818f6".to_string(),
            "USDC/USD".to_string(),
        ),
    ])
}

fn normalize_address_key(raw: &str) -> String {
    raw.trim().trim_matches('"').to_ascii_lowercase()
}

fn resolve_reference_pair(
    event: &NormalizedEvent,
    oracle_pair_overrides: &HashMap<String, String>,
) -> Option<String> {
    if let Some(pair) = event
        .metadata
        .get("oracle_pair")
        .and_then(|value| value.as_str())
    {
        return Some(pair.to_string());
    }

    let oracle_address = event
        .metadata
        .get("oracle_address")
        .and_then(|value| value.as_str())
        .map(normalize_address_key)?;

    oracle_pair_overrides.get(&oracle_address).cloned()
}

async fn enrich_reference_price(
    event: &mut NormalizedEvent,
    reference_price_provider: Option<&dyn ReferencePriceProvider>,
    oracle_pair_overrides: &HashMap<String, String>,
) {
    if event.reference_price.is_some() {
        return;
    }

    let Some(provider) = reference_price_provider else {
        return;
    };

    let Some(pair) = resolve_reference_pair(event, oracle_pair_overrides) else {
        return;
    };

    match provider.get_price(&pair).await {
        Ok(price) if price.is_finite() && price > 0.0 => {
            event.reference_price = Some(price);
            event
                .metadata
                .insert("reference_price_pair".to_string(), serde_json::json!(pair));
            event.metadata.insert(
                "reference_price_source".to_string(),
                serde_json::json!("live_reference_price_provider"),
            );
        }
        Ok(price) => warn!(
            tx_hash = %event.tx_hash,
            pair = %pair,
            price,
            "reference-price provider returned invalid price"
        ),
        Err(err) => warn!(
            tx_hash = %event.tx_hash,
            pair = %pair,
            error = ?err,
            "failed to fetch reference price"
        ),
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(default)
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

        if !chain_config_path.exists() {
            warn!(
                chain_dir = %entry_path.display(),
                "missing chain_config.yaml; skipping chain",
            );
            continue;
        }
        if !protocol_config_path.exists() {
            warn!(
                chain_dir = %entry_path.display(),
                "missing protocol_config.yaml; skipping chain",
            );
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

async fn init_repository() -> Option<PostgresRepository> {
    let Some(database_url) = PostgresRepository::from_env() else {
        info!("DATABASE_URL not set; postgres persistence disabled");
        return None;
    };

    match PostgresRepository::from_database_url(&database_url).await {
        Ok(repo) => {
            info!("postgres persistence enabled");
            Some(repo)
        }
        Err(err) => {
            warn!(error = ?err, "failed to initialize postgres persistence; disabled");
            None
        }
    }
}

async fn init_stream_publisher() -> Option<RedisStreamPublisher> {
    let Some(publisher_result) = RedisStreamPublisher::from_env() else {
        info!("REDIS_URL not set; redis stream publishing disabled");
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
        info!("redis stream publishing enabled");
        Some(publisher)
    }
}

fn find_rules_repo_root() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RULES_REPO_PATH") {
        let path = PathBuf::from(explicit);
        if path.exists() {
            return Some(path);
        }
        warn!(
            path = %path.display(),
            "RULES_REPO_PATH does not exist; skipping explicit rules path",
        );
    }

    for candidate in DEFAULT_RULE_RELATIVE_ROOTS {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }

    None
}

fn discover_rule_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_rule_files(root, false, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_rule_files(path: &Path, in_protocol_tree: bool, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to read rules directory {}", path.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();

        if entry_path.is_dir() {
            let is_protocol_dir = entry_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.eq_ignore_ascii_case("protocols"))
                .unwrap_or(false);
            collect_rule_files(&entry_path, in_protocol_tree || is_protocol_dir, files)?;
            continue;
        }

        if !in_protocol_tree {
            continue;
        }

        let is_yaml = entry_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
            .unwrap_or(false);

        if is_yaml {
            files.push(entry_path);
        }
    }

    Ok(())
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
