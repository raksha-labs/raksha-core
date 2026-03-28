use std::{collections::VecDeque, time::Duration};

use anyhow::{anyhow, Context, Result};
use ethers::{
    providers::{Middleware, Provider, Ws},
    types::{Address, BlockNumber, Filter, ValueOrArray, H256, U64},
};
use serde_json::Value;
use tokio::time::sleep;
use url::Url;

#[derive(Clone, Debug, Default)]
struct TransportSimulationMetadata {
    is_simulated: bool,
    simulation_run_id: Option<String>,
}

pub struct RpcLogsConnector {
    endpoint: String,
    filter_config: Value,
    poll_interval: Duration,
    provider: Option<Provider<Ws>>,
    pending: VecDeque<Value>,
    last_block: Option<U64>,
    chain_id: Option<i64>,
    simulation_metadata: TransportSimulationMetadata,
}

impl RpcLogsConnector {
    pub fn new(endpoint: String, filter_config: Value, poll_interval: Duration) -> Self {
        let simulation_metadata = parse_transport_simulation_metadata(&endpoint);
        Self {
            endpoint,
            filter_config,
            poll_interval,
            provider: None,
            pending: VecDeque::new(),
            last_block: None,
            chain_id: None,
            simulation_metadata,
        }
    }

    pub fn chain_id(&self) -> Option<i64> {
        self.chain_id
    }

    pub async fn connect(&mut self) -> Result<()> {
        let ws = Ws::connect(self.endpoint.as_str()).await.with_context(|| {
            format!(
                "failed connecting rpc websocket endpoint: {}",
                self.endpoint
            )
        })?;
        let provider = Provider::new(ws);
        let chain_id = provider
            .get_chainid()
            .await
            .ok()
            .map(|value| value.as_u64() as i64);
        self.chain_id = chain_id;
        self.provider = Some(provider);
        Ok(())
    }

    pub async fn next_payload(&mut self) -> Result<Value> {
        loop {
            if let Some(payload) = self.pending.pop_front() {
                return Ok(payload);
            }
            self.refresh_pending_logs().await?;
            if self.pending.is_empty() {
                sleep(self.poll_interval).await;
            }
        }
    }

    async fn refresh_pending_logs(&mut self) -> Result<()> {
        let Some(provider) = self.provider.as_ref() else {
            return Err(anyhow!("rpc_logs connector is not connected"));
        };

        let head = provider
            .get_block_number()
            .await
            .context("failed to fetch latest block for rpc_logs connector")?;

        let from = self
            .last_block
            .map(|block| block.saturating_add(U64::one()))
            .unwrap_or_else(|| head.saturating_sub(U64::from(2_u64)));
        if from > head {
            self.last_block = Some(head);
            return Ok(());
        }

        let filter = build_filter(&self.filter_config, from, head)?;
        let logs = provider
            .get_logs(&filter)
            .await
            .context("failed to fetch logs for rpc_logs connector")?;
        self.last_block = Some(head);

        for log in logs {
            let mut payload =
                serde_json::to_value(&log).context("failed to serialize log payload")?;
            if let Some(chain_id) = self.chain_id {
                if let Some(object) = payload.as_object_mut() {
                    object.insert("chainId".to_string(), serde_json::json!(chain_id));
                }
            }
            inject_simulation_metadata(&mut payload, &self.simulation_metadata);
            self.pending.push_back(payload);
        }

        Ok(())
    }
}

fn parse_transport_simulation_metadata(endpoint: &str) -> TransportSimulationMetadata {
    let Ok(url) = Url::parse(endpoint) else {
        return TransportSimulationMetadata::default();
    };

    let mut metadata = TransportSimulationMetadata::default();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "is_simulated" => {
                let normalized = value.trim().to_ascii_lowercase();
                metadata.is_simulated = matches!(normalized.as_str(), "1" | "true" | "yes" | "on");
            }
            "simulation_run_id" => {
                let run_id = value.trim();
                if !run_id.is_empty() {
                    metadata.simulation_run_id = Some(run_id.to_string());
                }
            }
            _ => {}
        }
    }
    metadata
}

fn inject_simulation_metadata(payload: &mut Value, metadata: &TransportSimulationMetadata) {
    if !metadata.is_simulated && metadata.simulation_run_id.is_none() {
        return;
    }

    let Some(object) = payload.as_object_mut() else {
        return;
    };

    let mut simulation = serde_json::Map::new();
    simulation.insert(
        "is_simulated".to_string(),
        serde_json::json!(metadata.is_simulated || metadata.simulation_run_id.is_some()),
    );
    if let Some(run_id) = metadata.simulation_run_id.as_ref() {
        simulation.insert("run_id".to_string(), serde_json::json!(run_id));
    }
    object.insert("simulation".to_string(), Value::Object(simulation));
}

fn build_filter(filter_config: &Value, from: U64, to: U64) -> Result<Filter> {
    let mut filter = Filter::new()
        .from_block(BlockNumber::Number(from))
        .to_block(BlockNumber::Number(to));

    let mut addresses: Vec<Address> = Vec::new();
    for key in ["addresses", "contracts", "contract_addresses"] {
        if let Some(items) = filter_config.get(key).and_then(Value::as_array) {
            for item in items.iter().filter_map(Value::as_str) {
                let address = item.parse::<Address>().with_context(|| {
                    format!("invalid contract address in filter config: {item}")
                })?;
                addresses.push(address);
            }
            if !addresses.is_empty() {
                break;
            }
        }
    }
    if !addresses.is_empty() {
        filter = filter.address(ValueOrArray::Array(addresses));
    }

    if let Some(topics) = filter_config.get("topics").and_then(Value::as_array) {
        if let Some(topic0) = parse_topic_filter(topics.first(), 0)? {
            filter = filter.topic0(topic0);
        }
        if let Some(topic1) = parse_topic_filter(topics.get(1), 1)? {
            filter = filter.topic1(topic1);
        }
        if let Some(topic2) = parse_topic_filter(topics.get(2), 2)? {
            filter = filter.topic2(topic2);
        }
        if let Some(topic3) = parse_topic_filter(topics.get(3), 3)? {
            filter = filter.topic3(topic3);
        }
    }

    Ok(filter)
}

fn parse_topic_filter(
    value: Option<&Value>,
    index: usize,
) -> Result<Option<ValueOrArray<Option<H256>>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }

    if let Some(raw) = value.as_str() {
        return Ok(Some(ValueOrArray::Value(Some(parse_topic_hash(
            raw, index,
        )?))));
    }

    if let Some(items) = value.as_array() {
        let mut topics = Vec::new();
        for item in items {
            let Some(raw) = item.as_str() else {
                return Err(anyhow!(
                    "invalid topic{} entry in filter config: expected string values",
                    index
                ));
            };
            topics.push(Some(parse_topic_hash(raw, index)?));
        }
        if topics.is_empty() {
            return Ok(None);
        }
        return Ok(Some(ValueOrArray::Array(topics)));
    }

    Err(anyhow!(
        "invalid topic{} in filter config: expected string, array, or null",
        index
    ))
}

fn parse_topic_hash(raw: &str, index: usize) -> Result<H256> {
    let trimmed = raw.trim();
    let body = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    if body.is_empty() || body.len() > 64 {
        return Err(anyhow!(
            "invalid topic{} in filter config: {}",
            index,
            trimmed
        ));
    }

    let normalized = format!("0x{:0>64}", body);
    normalized
        .parse::<H256>()
        .with_context(|| format!("invalid topic{} in filter config: {}", index, trimmed))
}
