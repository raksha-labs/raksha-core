use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::ChainAdapter;
use ethers::{
    providers::{Middleware, Provider, Ws},
    types::{Address, Filter, H256, Log, U64},
};
use event_schema::{NormalizedEvent, ProtocolCategory};
use tracing::{info, warn};

use crate::{
    config::FlashLoanSourceConfig,
    decode_chainlink::{answer_updated_topic, decode_answer_updated_raw, scale_answer},
    decode_flashloan::{decode_flash_loan_log, event_topic_from_signature},
    normalize::{normalize_answer_updated_event, normalize_flash_loan_candidate_event},
    protocol_map::{parse_oracle_addresses, OracleProtocolMap},
};

#[derive(Debug, Clone)]
pub struct RpcProviderStatus {
    pub endpoint: String,
    pub healthy: bool,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct RpcProviderClient {
    endpoint: String,
    provider: Arc<Provider<Ws>>,
    status: RpcProviderStatus,
}

#[derive(Debug)]
struct ProviderLog {
    log: Log,
    provider_index: usize,
}

#[derive(Debug)]
struct LogBatch {
    logs: Vec<ProviderLog>,
}

#[derive(Debug)]
struct ProviderFetchBatch {
    logs: Vec<Log>,
    provider_index: usize,
}

#[derive(Debug, Clone)]
struct FlashLoanSourceBinding {
    id: String,
    protocol: String,
    protocol_category: ProtocolCategory,
    event_signature: String,
    event_topic0: H256,
    asset_pair_hint: Option<String>,
}

type FlashLoanSourceMap = HashMap<Address, Vec<FlashLoanSourceBinding>>;

pub struct EvmChainAdapter {
    providers: Vec<RpcProviderClient>,
    chain_slug: String,
    chain_id: u64,
    confirmation_depth: u64,
    lookback_blocks: u64,
    filter_chunk_size: usize,
    active_provider_index: usize,
    oracle_protocol_map: OracleProtocolMap,
    flash_loan_source_map: FlashLoanSourceMap,
    seen_event_keys: HashSet<String>,
}

impl EvmChainAdapter {
    pub async fn new(
        ws_url: &str,
        chain_slug: impl Into<String>,
        chain_id: u64,
        lookback_blocks: u64,
        oracle_protocol_map: OracleProtocolMap,
    ) -> Result<Self> {
        Self::new_with_urls(
            vec![ws_url.to_string()],
            chain_slug,
            chain_id,
            lookback_blocks,
            oracle_protocol_map,
        )
        .await
    }

    pub async fn new_with_urls<I, S>(
        ws_urls: I,
        chain_slug: impl Into<String>,
        chain_id: u64,
        lookback_blocks: u64,
        oracle_protocol_map: OracleProtocolMap,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let chain_slug = chain_slug.into();
        let endpoints = dedupe_endpoints(ws_urls);
        if endpoints.is_empty() {
            return Err(anyhow!(
                "at least one websocket endpoint is required for chain '{}'",
                chain_slug
            ));
        }

        let mut providers = Vec::new();
        let mut failures = Vec::new();

        for endpoint in endpoints {
            match Ws::connect(&endpoint)
                .await
                .with_context(|| format!("failed to connect websocket endpoint {endpoint}"))
            {
                Ok(ws) => {
                    let provider = Arc::new(Provider::new(ws));
                    providers.push(RpcProviderClient {
                        endpoint: endpoint.clone(),
                        provider,
                        status: RpcProviderStatus {
                            endpoint,
                            healthy: true,
                            consecutive_failures: 0,
                            last_error: None,
                            last_success_at: Some(Utc::now()),
                            last_failure_at: None,
                        },
                    });
                }
                Err(err) => failures.push(err.to_string()),
            }
        }

        if providers.is_empty() {
            return Err(anyhow!(
                "failed to connect all rpc providers for chain '{}': {}",
                chain_slug,
                failures.join(" | ")
            ));
        }
        if !failures.is_empty() {
            warn!(
                chain = %chain_slug,
                failed_provider_count = failures.len(),
                "some rpc providers failed during startup; continuing with available providers"
            );
        }

        Ok(Self {
            providers,
            confirmation_depth: default_confirmation_depth_for_chain(&chain_slug),
            chain_slug,
            chain_id,
            lookback_blocks,
            filter_chunk_size: 250,
            active_provider_index: 0,
            oracle_protocol_map,
            flash_loan_source_map: HashMap::new(),
            seen_event_keys: HashSet::new(),
        })
    }

    pub fn with_confirmation_depth(mut self, confirmation_depth: u64) -> Self {
        self.confirmation_depth = confirmation_depth.max(1);
        self
    }

    pub fn with_filter_chunk_size(mut self, filter_chunk_size: usize) -> Self {
        if filter_chunk_size > 0 {
            self.filter_chunk_size = filter_chunk_size;
        }
        self
    }

    pub fn with_flash_loan_sources(
        mut self,
        sources: Vec<FlashLoanSourceConfig>,
        default_protocol_category: ProtocolCategory,
    ) -> Result<Self> {
        let mut source_map: FlashLoanSourceMap = HashMap::new();

        for source in sources.into_iter().filter(|entry| entry.enabled) {
            let source_id = source.id.trim();
            if source_id.is_empty() {
                warn!(chain = %self.chain_slug, "skipping flash source with empty id");
                continue;
            }
            if source.protocol.trim().is_empty() {
                warn!(chain = %self.chain_slug, source_id, "skipping flash source with empty protocol");
                continue;
            }
            if source.event_signature.trim().is_empty() {
                warn!(chain = %self.chain_slug, source_id, "skipping flash source with empty event_signature");
                continue;
            }

            let addresses = parse_oracle_addresses(&source.contract_addresses).with_context(|| {
                format!("invalid contract addresses for flash loan source '{}'", source.id)
            })?;
            if addresses.is_empty() {
                warn!(chain = %self.chain_slug, source_id, "skipping flash source with no contract addresses");
                continue;
            }

            let protocol_category = source
                .protocol_category
                .as_deref()
                .map(crate::config::parse_protocol_category)
                .unwrap_or_else(|| default_protocol_category.clone());

            let binding = FlashLoanSourceBinding {
                id: source.id.clone(),
                protocol: source.protocol.clone(),
                protocol_category,
                event_signature: source.event_signature.clone(),
                event_topic0: event_topic_from_signature(&source.event_signature),
                asset_pair_hint: source.asset_pair_hint.clone(),
            };

            for address in addresses {
                source_map.entry(address).or_default().push(binding.clone());
            }
        }

        self.flash_loan_source_map = source_map;
        Ok(self)
    }

    pub fn provider_statuses(&self) -> Vec<RpcProviderStatus> {
        self.providers
            .iter()
            .map(|client| client.status.clone())
            .collect()
    }

    async fn fetch_logs_in_range(&mut self, from: U64, to: U64) -> Result<LogBatch> {
        let mut logs = self.fetch_oracle_logs_in_range(from, to).await?;
        logs.extend(self.fetch_flash_logs_in_range(from, to).await?);
        Ok(LogBatch {
            logs: dedupe_provider_logs(logs),
        })
    }

    async fn fetch_oracle_logs_in_range(&mut self, from: U64, to: U64) -> Result<Vec<ProviderLog>> {
        let topic0 = answer_updated_topic();
        let all_addresses: Vec<Address> = self.oracle_protocol_map.keys().copied().collect();
        let mut logs = Vec::new();

        for chunk in all_addresses.chunks(self.filter_chunk_size.max(1)) {
            let filter = Filter::new()
                .address(chunk.to_vec())
                .topic0(topic0)
                .from_block(from)
                .to_block(to);

            let batch = self.fetch_logs_with_failover(&filter).await?;
            logs.extend(batch.logs.into_iter().map(|log| ProviderLog {
                log,
                provider_index: batch.provider_index,
            }));
        }

        Ok(logs)
    }

    async fn fetch_flash_logs_in_range(&mut self, from: U64, to: U64) -> Result<Vec<ProviderLog>> {
        if self.flash_loan_source_map.is_empty() {
            return Ok(Vec::new());
        }

        let mut addresses_by_topic: HashMap<H256, Vec<Address>> = HashMap::new();
        for (address, bindings) in &self.flash_loan_source_map {
            for binding in bindings {
                let addresses = addresses_by_topic.entry(binding.event_topic0).or_default();
                if !addresses.contains(address) {
                    addresses.push(*address);
                }
            }
        }

        let mut logs = Vec::new();
        for (topic0, addresses) in addresses_by_topic {
            for chunk in addresses.chunks(self.filter_chunk_size.max(1)) {
                let filter = Filter::new()
                    .address(chunk.to_vec())
                    .topic0(topic0)
                    .from_block(from)
                    .to_block(to);
                let batch = self.fetch_logs_with_failover(&filter).await?;
                logs.extend(batch.logs.into_iter().map(|log| ProviderLog {
                    log,
                    provider_index: batch.provider_index,
                }));
            }
        }

        Ok(logs)
    }

    async fn fetch_recent_logs(&mut self) -> Result<LogBatch> {
        let latest = self.get_block_number_with_failover().await?;
        let from = latest.saturating_sub(U64::from(self.lookback_blocks));
        self.fetch_logs_in_range(from, latest).await
    }

    fn decode_logs(
        &mut self,
        logs: Vec<ProviderLog>,
        source: &str,
        dedupe_on_event_key: bool,
    ) -> Vec<NormalizedEvent> {
        let mut events = Vec::new();
        let oracle_topic = answer_updated_topic();

        for provider_log in logs {
            let ProviderLog {
                log,
                provider_index,
            } = provider_log;
            let provider_endpoint = self
                .providers
                .get(provider_index)
                .map(|provider| provider.endpoint.as_str())
                .unwrap_or("unknown");
            let topic0 = log.topics.first().copied();

            if topic0 == Some(oracle_topic) {
                if let Some(bindings) = self.oracle_protocol_map.get(&log.address) {
                    if let Some(raw_answer) = decode_answer_updated_raw(&log) {
                        for (binding_index, binding) in bindings.iter().enumerate() {
                            let price = scale_answer(&raw_answer, binding.oracle_decimals);
                            let mut event = normalize_answer_updated_event(
                                &self.chain_slug,
                                self.chain_id,
                                self.confirmation_depth,
                                binding,
                                &log,
                                price,
                                source,
                                binding_index,
                                bindings.len(),
                            );
                            event.metadata.insert(
                                "rpc_provider_index".to_string(),
                                serde_json::json!(provider_index),
                            );
                            event.metadata.insert(
                                "rpc_provider_endpoint".to_string(),
                                serde_json::json!(provider_endpoint),
                            );

                            if dedupe_on_event_key
                                && !self.seen_event_keys.insert(event.event_key.clone())
                            {
                                continue;
                            }
                            events.push(event);
                        }
                    }
                }
            }

            if let Some(topic0) = topic0 {
                if let Some(bindings) = self.flash_loan_source_map.get(&log.address) {
                    for binding in bindings.iter().filter(|entry| entry.event_topic0 == topic0) {
                        let Some(decoded) = decode_flash_loan_log(&binding.protocol, &log) else {
                            continue;
                        };
                        let mut event = normalize_flash_loan_candidate_event(
                            &self.chain_slug,
                            self.chain_id,
                            self.confirmation_depth,
                            &binding.id,
                            &binding.protocol,
                            binding.protocol_category.clone(),
                            binding.asset_pair_hint.as_deref(),
                            &log,
                            &decoded,
                            source,
                        );
                        event.metadata.insert(
                            "rpc_provider_index".to_string(),
                            serde_json::json!(provider_index),
                        );
                        event.metadata.insert(
                            "rpc_provider_endpoint".to_string(),
                            serde_json::json!(provider_endpoint),
                        );
                        event.metadata.insert(
                            "flash_loan.event_signature".to_string(),
                            serde_json::json!(binding.event_signature),
                        );

                        if dedupe_on_event_key
                            && !self.seen_event_keys.insert(event.event_key.clone())
                        {
                            continue;
                        }
                        events.push(event);
                    }
                }
            }
        }

        events
    }

    async fn get_block_number_with_failover(&mut self) -> Result<U64> {
        let provider_count = self.providers.len();
        for offset in 0..provider_count {
            let idx = (self.active_provider_index + offset) % provider_count;
            let provider = self.providers[idx].provider.clone();

            match provider.get_block_number().await {
                Ok(number) => {
                    self.mark_provider_success(idx, "get_block_number");
                    return Ok(number);
                }
                Err(err) => {
                    self.mark_provider_failure(
                        idx,
                        anyhow!(err).context("get_block_number request failed"),
                        "get_block_number",
                    );
                }
            }
        }

        Err(anyhow!(
            "all rpc providers failed get_block_number for chain '{}'; statuses: {}",
            self.chain_slug,
            self.provider_status_summary()
        ))
    }

    async fn fetch_logs_with_failover(&mut self, filter: &Filter) -> Result<ProviderFetchBatch> {
        let provider_count = self.providers.len();
        for offset in 0..provider_count {
            let idx = (self.active_provider_index + offset) % provider_count;
            let provider = self.providers[idx].provider.clone();
            let filter_clone = filter.clone();

            match provider.get_logs(&filter_clone).await {
                Ok(logs) => {
                    self.mark_provider_success(idx, "get_logs");
                    return Ok(ProviderFetchBatch {
                        logs,
                        provider_index: idx,
                    });
                }
                Err(err) => {
                    self.mark_provider_failure(
                        idx,
                        anyhow!(err).context("get_logs request failed"),
                        "get_logs",
                    );
                }
            }
        }

        Err(anyhow!(
            "all rpc providers failed get_logs for chain '{}'; statuses: {}",
            self.chain_slug,
            self.provider_status_summary()
        ))
    }

    fn mark_provider_success(&mut self, idx: usize, operation: &str) {
        if let Some(provider) = self.providers.get_mut(idx) {
            let recovered = !provider.status.healthy;
            provider.status.healthy = true;
            provider.status.consecutive_failures = 0;
            provider.status.last_error = None;
            provider.status.last_success_at = Some(Utc::now());
            if recovered {
                info!(
                    chain = %self.chain_slug,
                    operation,
                    provider_endpoint = %provider.endpoint,
                    "rpc provider recovered"
                );
            }
        }

        if self.active_provider_index != idx {
            let previous_endpoint = self
                .providers
                .get(self.active_provider_index)
                .map(|provider| provider.endpoint.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let next_endpoint = self
                .providers
                .get(idx)
                .map(|provider| provider.endpoint.clone())
                .unwrap_or_else(|| "unknown".to_string());
            self.active_provider_index = idx;
            info!(
                chain = %self.chain_slug,
                operation,
                previous_endpoint = %previous_endpoint,
                next_endpoint = %next_endpoint,
                "switched active rpc provider"
            );
        }
    }

    fn mark_provider_failure(&mut self, idx: usize, err: anyhow::Error, operation: &str) {
        if let Some(provider) = self.providers.get_mut(idx) {
            provider.status.healthy = false;
            provider.status.consecutive_failures =
                provider.status.consecutive_failures.saturating_add(1);
            provider.status.last_error = Some(err.to_string());
            provider.status.last_failure_at = Some(Utc::now());
            warn!(
                chain = %self.chain_slug,
                operation,
                provider_endpoint = %provider.endpoint,
                consecutive_failures = provider.status.consecutive_failures,
                error = ?err,
                "rpc provider request failed"
            );
        }
    }

    fn provider_status_summary(&self) -> String {
        self.providers
            .iter()
            .enumerate()
            .map(|(idx, provider)| {
                format!(
                    "#{idx} {} healthy={} failures={}",
                    provider.endpoint,
                    provider.status.healthy,
                    provider.status.consecutive_failures
                )
            })
            .collect::<Vec<String>>()
            .join("; ")
    }
}

#[async_trait]
impl ChainAdapter for EvmChainAdapter {
    async fn next_events(&mut self) -> Result<Vec<NormalizedEvent>> {
        if self.oracle_protocol_map.is_empty() && self.flash_loan_source_map.is_empty() {
            warn!(
                chain = self.chain_slug,
                "evm adapter has no oracle or flash loan sources configured"
            );
            return Ok(vec![]);
        }

        let batch = self.fetch_recent_logs().await?;
        if batch.logs.is_empty() {
            info!(
                chain = self.chain_slug,
                "evm adapter found no oracle or flash loan updates in lookback window"
            );
            return Ok(vec![]);
        }

        Ok(self.decode_logs(batch.logs, "chain_adapter_evm_live", true))
    }

    fn chain_name(&self) -> &'static str {
        "evm"
    }

    async fn subscribe_heads(&mut self) -> Result<()> {
        info!(
            chain = self.chain_slug,
            active_provider = %self.providers[self.active_provider_index].endpoint,
            provider_count = self.providers.len(),
            "evm adapter subscribed to heads"
        );
        Ok(())
    }

    async fn subscribe_logs(&mut self) -> Result<()> {
        info!(
            chain = self.chain_slug,
            active_provider = %self.providers[self.active_provider_index].endpoint,
            provider_count = self.providers.len(),
            "evm adapter subscribed to logs"
        );
        Ok(())
    }

    async fn backfill_range(
        &mut self,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<NormalizedEvent>> {
        if to_block < from_block {
            return Ok(Vec::new());
        }
        let batch = self
            .fetch_logs_in_range(U64::from(from_block), U64::from(to_block))
            .await?;
        Ok(self.decode_logs(batch.logs, "chain_adapter_evm_backfill", false))
    }

    async fn latest_block_number(&mut self) -> Result<Option<u64>> {
        Ok(Some(self.get_block_number_with_failover().await?.as_u64()))
    }

    fn chain_id(&self) -> Option<u64> {
        Some(self.chain_id)
    }
}

fn dedupe_endpoints<I, S>(ws_urls: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut endpoints = Vec::new();
    for endpoint in ws_urls.into_iter().map(Into::into) {
        let normalized = endpoint.trim().to_string();
        if normalized.is_empty() {
            continue;
        }
        if !endpoints.contains(&normalized) {
            endpoints.push(normalized);
        }
    }
    endpoints
}

fn dedupe_provider_logs(logs: Vec<ProviderLog>) -> Vec<ProviderLog> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(logs.len());

    for entry in logs {
        let key = provider_log_key(&entry.log);
        if seen.insert(key) {
            deduped.push(entry);
        }
    }

    deduped
}

fn provider_log_key(log: &Log) -> (String, u64, Address, H256) {
    let tx_hash = log
        .transaction_hash
        .map(|hash| format!("{hash:?}"))
        .unwrap_or_default();
    let log_index = log
        .log_index
        .map(|index| index.as_u64())
        .unwrap_or(u64::MAX);
    let topic0 = log.topics.first().copied().unwrap_or_default();
    (tx_hash, log_index, log.address, topic0)
}

fn default_confirmation_depth_for_chain(chain_slug: &str) -> u64 {
    match chain_slug {
        "base" => 64,
        _ => 3,
    }
}
