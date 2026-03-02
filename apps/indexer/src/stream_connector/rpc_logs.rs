use std::{collections::VecDeque, time::Duration};

use anyhow::{anyhow, Context, Result};
use ethers::{
    providers::{Middleware, Provider, Ws},
    types::{Address, BlockNumber, Filter, ValueOrArray, H256, U64},
};
use serde_json::Value;
use tokio::time::sleep;

pub struct RpcLogsConnector {
    endpoint: String,
    filter_config: Value,
    poll_interval: Duration,
    provider: Option<Provider<Ws>>,
    pending: VecDeque<Value>,
    last_block: Option<U64>,
    chain_id: Option<i64>,
}

impl RpcLogsConnector {
    pub fn new(endpoint: String, filter_config: Value, poll_interval: Duration) -> Self {
        Self {
            endpoint,
            filter_config,
            poll_interval,
            provider: None,
            pending: VecDeque::new(),
            last_block: None,
            chain_id: None,
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
            self.pending.push_back(payload);
        }

        Ok(())
    }
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

    if let Some(topic0) = filter_config
        .get("topics")
        .and_then(Value::as_array)
        .and_then(|topics| topics.first())
        .and_then(Value::as_str)
    {
        let topic0 = topic0
            .parse::<H256>()
            .with_context(|| format!("invalid topic0 in filter config: {topic0}"))?;
        filter = filter.topic0(topic0);
    }

    Ok(filter)
}
