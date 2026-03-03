use std::{collections::VecDeque, time::Duration};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use ethers::{
    providers::{Http, Middleware, Provider},
    types::U256,
};
use serde_json::{json, Value};
use tokio::time::sleep;

#[derive(Debug, Clone)]
struct StateCallSpec {
    protocol_id: String,
    chain_slug: Option<String>,
    market_id: Option<String>,
    contract_address: String,
    call_data: String,
    block_tag: Value,
    decimals: i32,
    price_usd: Option<f64>,
    metric: String,
    event_type: Option<String>,
}

pub struct RpcStateConnector {
    endpoint: String,
    filter_config: Value,
    poll_interval: Duration,
    provider: Option<Provider<Http>>,
    pending: VecDeque<Value>,
    chain_id: Option<i64>,
}

impl RpcStateConnector {
    pub fn new(endpoint: String, filter_config: Value, poll_interval: Duration) -> Self {
        Self {
            endpoint,
            filter_config,
            poll_interval,
            provider: None,
            pending: VecDeque::new(),
            chain_id: None,
        }
    }

    pub fn chain_id(&self) -> Option<i64> {
        self.chain_id
    }

    pub async fn connect(&mut self) -> Result<()> {
        let provider = Provider::<Http>::try_from(self.endpoint.as_str())
            .with_context(|| format!("failed connecting rpc http endpoint: {}", self.endpoint))?;
        self.chain_id = provider
            .get_chainid()
            .await
            .ok()
            .map(|value| value.as_u64() as i64);
        self.provider = Some(provider);
        Ok(())
    }

    pub async fn next_payload(&mut self) -> Result<Value> {
        loop {
            if let Some(payload) = self.pending.pop_front() {
                return Ok(payload);
            }
            self.refresh_pending_calls().await?;
            if self.pending.is_empty() {
                sleep(self.poll_interval).await;
            }
        }
    }

    async fn refresh_pending_calls(&mut self) -> Result<()> {
        let Some(provider) = self.provider.as_ref() else {
            return Err(anyhow!("rpc_state connector is not connected"));
        };

        let call_specs = parse_state_call_specs(&self.filter_config)?;
        if call_specs.is_empty() {
            return Err(anyhow!("rpc_state connector missing calls configuration"));
        }

        let block_number = provider
            .get_block_number()
            .await
            .context("failed to fetch latest block for rpc_state connector")?;
        let observed_at = Utc::now();

        for spec in call_specs {
            let params = json!([
                {
                    "to": spec.contract_address,
                    "data": spec.call_data
                },
                spec.block_tag
            ]);

            let raw_result = match provider.request::<_, String>("eth_call", params).await {
                Ok(value) => value,
                Err(error) => {
                    self.pending.push_back(json!({
                        "event_type": spec.event_type.unwrap_or_else(|| "protocol_state".to_string()),
                        "metric": spec.metric,
                        "protocol_id": spec.protocol_id,
                        "chain_slug": spec.chain_slug,
                        "market_id": spec.market_id,
                        "contract_address": spec.contract_address,
                        "call_data": spec.call_data,
                        "chainId": self.chain_id,
                        "block_number": block_number.as_u64(),
                        "timestamp": observed_at.timestamp_millis(),
                        "rpc_state_error": error.to_string(),
                    }));
                    continue;
                }
            };

            let raw_trimmed = raw_result.trim();
            let raw_u256 = parse_hex_to_u256(raw_trimmed);
            let tvl_native = raw_u256
                .as_ref()
                .and_then(|value| u256_to_scaled_f64(value, spec.decimals));
            let tvl_usd = match (tvl_native, spec.price_usd) {
                (Some(native), Some(price_usd)) if price_usd.is_finite() => {
                    Some(native * price_usd)
                }
                _ => None,
            };
            let paused = if spec.metric.eq_ignore_ascii_case("pause")
                || spec.metric.eq_ignore_ascii_case("protocol_pause")
            {
                raw_u256.as_ref().map(|value| !value.is_zero())
            } else {
                None
            };

            let event_type = spec.event_type.unwrap_or_else(|| {
                if paused == Some(true) {
                    "protocol_pause".to_string()
                } else if paused == Some(false) {
                    "protocol_unpause".to_string()
                } else {
                    "protocol_state".to_string()
                }
            });

            self.pending.push_back(json!({
                "event_type": event_type,
                "metric": spec.metric,
                "protocol_id": spec.protocol_id,
                "chain_slug": spec.chain_slug,
                "market_id": spec.market_id,
                "contract_address": spec.contract_address,
                "call_data": spec.call_data,
                "eth_call_result": raw_trimmed,
                "tvl_raw": raw_u256.map(|value| value.to_string()),
                "tvl_native": tvl_native,
                "price_usd": spec.price_usd,
                "tvl_usd": tvl_usd,
                "paused": paused,
                "decimals": spec.decimals,
                "chainId": self.chain_id,
                "block_number": block_number.as_u64(),
                "timestamp": observed_at.timestamp_millis(),
            }));
        }

        Ok(())
    }
}

fn parse_state_call_specs(filter_config: &Value) -> Result<Vec<StateCallSpec>> {
    let mut specs = Vec::new();

    if let Some(items) = filter_config.get("calls").and_then(Value::as_array) {
        for item in items {
            if let Some(spec) = parse_state_call(item, filter_config)? {
                specs.push(spec);
            }
        }
        return Ok(specs);
    }

    if let Some(items) = filter_config.get("markets").and_then(Value::as_array) {
        for item in items {
            if let Some(spec) = parse_state_call(item, filter_config)? {
                specs.push(spec);
            }
        }
        return Ok(specs);
    }

    if let Some(spec) = parse_state_call(filter_config, filter_config)? {
        specs.push(spec);
    }

    Ok(specs)
}

fn parse_state_call(candidate: &Value, defaults: &Value) -> Result<Option<StateCallSpec>> {
    let Some(object) = candidate.as_object() else {
        return Ok(None);
    };

    let protocol_id = read_str(
        object
            .get("protocol_id")
            .or_else(|| defaults.get("protocol_id")),
    )
    .unwrap_or("unknown");
    let contract_address = read_str(
        object
            .get("to")
            .or_else(|| object.get("contract_address"))
            .or_else(|| object.get("address"))
            .or_else(|| defaults.get("to"))
            .or_else(|| defaults.get("contract_address"))
            .or_else(|| defaults.get("address")),
    )
    .unwrap_or_default()
    .to_string();
    let call_data = read_str(
        object
            .get("data")
            .or_else(|| object.get("calldata"))
            .or_else(|| defaults.get("data"))
            .or_else(|| defaults.get("calldata")),
    )
    .unwrap_or_default()
    .to_string();

    if protocol_id.trim().is_empty()
        || contract_address.trim().is_empty()
        || call_data.trim().is_empty()
    {
        return Ok(None);
    }

    let block_tag = object
        .get("block_tag")
        .or_else(|| defaults.get("block_tag"))
        .cloned()
        .unwrap_or_else(|| Value::String("latest".to_string()));
    let decimals = read_i32(
        object
            .get("decimals")
            .or_else(|| object.get("tvl_decimals"))
            .or_else(|| defaults.get("decimals"))
            .or_else(|| defaults.get("tvl_decimals")),
    )
    .unwrap_or(18);
    let price_usd = read_f64(
        object
            .get("price_usd")
            .or_else(|| defaults.get("price_usd")),
    );
    let metric = read_str(object.get("metric").or_else(|| defaults.get("metric")))
        .unwrap_or("tvl")
        .to_ascii_lowercase();
    let event_type = read_str(
        object
            .get("event_type")
            .or_else(|| defaults.get("event_type")),
    )
    .map(ToString::to_string);

    Ok(Some(StateCallSpec {
        protocol_id: protocol_id.to_ascii_lowercase(),
        chain_slug: read_str(
            object
                .get("chain_slug")
                .or_else(|| defaults.get("chain_slug")),
        )
        .map(|value| value.to_ascii_lowercase()),
        market_id: read_str(
            object
                .get("market_id")
                .or_else(|| defaults.get("market_id")),
        )
        .filter(|value| !value.eq_ignore_ascii_case("all"))
        .map(|value| value.to_ascii_lowercase()),
        contract_address,
        call_data,
        block_tag,
        decimals,
        price_usd,
        metric,
        event_type,
    }))
}

fn read_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

fn read_f64(value: Option<&Value>) -> Option<f64> {
    if let Some(number) = value.and_then(Value::as_f64) {
        return Some(number);
    }
    value
        .and_then(Value::as_str)
        .and_then(|item| item.trim().parse::<f64>().ok())
}

fn read_i32(value: Option<&Value>) -> Option<i32> {
    value
        .and_then(Value::as_i64)
        .and_then(|number| i32::try_from(number).ok())
        .or_else(|| {
            value
                .and_then(Value::as_str)
                .and_then(|item| item.trim().parse::<i32>().ok())
        })
}

fn parse_hex_to_u256(raw: &str) -> Option<U256> {
    let body = raw.strip_prefix("0x").unwrap_or(raw).trim();
    if body.is_empty() {
        return None;
    }
    U256::from_str_radix(body, 16).ok()
}

fn u256_to_scaled_f64(value: &U256, decimals: i32) -> Option<f64> {
    let raw = value.to_string().parse::<f64>().ok()?;
    let divisor = 10f64.powi(decimals.max(0));
    if !divisor.is_finite() || divisor <= 0.0 {
        return None;
    }
    Some(raw / divisor)
}
