use std::env;

use anyhow::{anyhow, Context, Result};
use chainlink_data_streams_report::{
    feed_id::ID,
    report::{decode_full_report, v3::ReportDataV3, Report},
};
use chainlink_data_streams_sdk::{
    client::Client,
    config::{Config, WebSocketHighAvailability},
    stream::Stream as DataStreamsStream,
};
use serde_json::{json, Value};

const DEFAULT_REST_URL: &str = "https://api.dataengine.chain.link";
const DEFAULT_WS_URL: &str = "wss://ws.dataengine.chain.link";
const DEFAULT_API_KEY_ENVS: &[&str] = &["CHAINLINK_DATA_STREAMS_API_KEY", "STREAMS_API_KEY"];
const DEFAULT_USER_SECRET_ENVS: &[&str] =
    &["CHAINLINK_DATA_STREAMS_USER_SECRET", "STREAMS_API_SECRET"];

pub struct ChainlinkDataStreamsConnector {
    client: Client,
    sdk_config: Config,
    rest_url: String,
    ws_url: String,
    feed_id: ID,
    feed_id_hex: String,
    market_key: Option<String>,
    asset_pair: Option<String>,
    price_decimals: i32,
    stream: Option<DataStreamsStream>,
}

impl ChainlinkDataStreamsConnector {
    pub fn new(
        connection_config: &Value,
        auth_config: &Value,
        filter_config: &Value,
        market_key: Option<&str>,
        asset_pair: Option<&str>,
    ) -> Result<Self> {
        let api_key = resolve_secret(auth_config, "api_key", "api_key_env", DEFAULT_API_KEY_ENVS)?;
        let user_secret = resolve_secret(
            auth_config,
            "user_secret",
            "user_secret_env",
            DEFAULT_USER_SECRET_ENVS,
        )?;
        let rest_url = resolve_connection_endpoint(
            connection_config,
            &["endpoint", "rest_url", "http_url"],
            DEFAULT_REST_URL,
        );
        let ws_url = resolve_connection_endpoint(
            connection_config,
            &["ws_endpoint", "ws_url"],
            DEFAULT_WS_URL,
        );
        let feed_id_hex = resolve_secret(filter_config, "feed_id", "feed_id_env", &[])
            .with_context(|| {
                let market = market_key.unwrap_or("unknown");
                format!("missing Chainlink Data Streams feed id for market {market}")
            })?;
        let feed_id = ID::from_hex_str(&feed_id_hex).map_err(|error| {
            anyhow!("invalid Chainlink Data Streams feed id {feed_id_hex}: {error}")
        })?;

        let mut builder = Config::new(api_key, user_secret, rest_url.clone(), ws_url.clone());
        let ws_ha_enabled = auth_config
            .get("ws_high_availability")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| ws_url.contains(','));
        if ws_ha_enabled {
            builder = builder.with_ws_ha(WebSocketHighAvailability::Enabled);
        }
        if let Some(ws_max_reconnect) = auth_config
            .get("ws_max_reconnect")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
        {
            builder = builder.with_ws_max_reconnect(ws_max_reconnect);
        }
        let sdk_config = builder
            .build()
            .context("failed building Chainlink Data Streams SDK config")?;
        let client = Client::new(sdk_config.clone())
            .context("failed constructing Chainlink Data Streams SDK client")?;

        Ok(Self {
            client,
            sdk_config,
            rest_url,
            ws_url,
            feed_id,
            feed_id_hex,
            market_key: market_key.map(ToOwned::to_owned),
            asset_pair: asset_pair.map(ToOwned::to_owned),
            price_decimals: filter_config
                .get("price_decimals")
                .and_then(Value::as_i64)
                .and_then(|value| i32::try_from(value).ok())
                .unwrap_or(8),
            stream: None,
        })
    }

    pub async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    pub async fn connect_stream(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let mut stream = DataStreamsStream::new(&self.sdk_config, vec![self.feed_id])
            .await
            .context("failed creating Chainlink Data Streams websocket stream")?;
        stream
            .listen()
            .await
            .context("failed starting Chainlink Data Streams websocket stream")?;
        self.stream = Some(stream);
        Ok(())
    }

    pub async fn close_stream(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.as_mut() {
            stream
                .close()
                .await
                .context("failed closing Chainlink Data Streams websocket stream")?;
        }
        self.stream = None;
        Ok(())
    }

    pub fn endpoint(&self) -> &str {
        if self.stream.is_some() {
            &self.ws_url
        } else {
            &self.rest_url
        }
    }

    pub async fn fetch_payload(&self) -> Result<Option<Value>> {
        let response = self
            .client
            .get_latest_report(self.feed_id)
            .await
            .with_context(|| {
                format!(
                    "Chainlink Data Streams get_latest_report failed for feed {}",
                    self.feed_id_hex
                )
            })?;

        self.decode_report(&response.report).map(Some)
    }

    pub async fn next_payload(&mut self) -> Result<Value> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow!("Chainlink Data Streams websocket stream is not connected"))?;
        let response = stream
            .read()
            .await
            .context("failed reading Chainlink Data Streams websocket report")?;
        self.decode_report(&response.report)
    }

    fn decode_report(&self, report: &Report) -> Result<Value> {
        let full_report = report.full_report.trim_start_matches("0x");
        let full_report_bytes =
            hex::decode(full_report).context("Chainlink Data Streams report was not valid hex")?;
        let (_report_context, report_blob) = decode_full_report(&full_report_bytes)
            .context("failed decoding Chainlink Data Streams full report")?;
        let report_data = ReportDataV3::decode(&report_blob)
            .context("failed decoding Chainlink Data Streams v3 report payload")?;

        Ok(json!({
            "feed_id": report.feed_id.to_hex_string(),
            "valid_from_timestamp": report.valid_from_timestamp,
            "observations_timestamp": report.observations_timestamp,
            "full_report": report.full_report,
            "market_key": self.market_key,
            "asset_pair": self.asset_pair,
            "price_decimals": self.price_decimals,
            "benchmark_price": report_data.benchmark_price.to_string(),
            "bid": report_data.bid.to_string(),
            "ask": report_data.ask.to_string(),
        }))
    }
}

fn resolve_secret(
    config: &Value,
    value_key: &str,
    env_key: &str,
    default_envs: &[&str],
) -> Result<String> {
    if let Some(value) = config
        .get(value_key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(value.to_string());
    }

    if let Some(env_name) = config
        .get(env_key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return env::var(env_name)
            .with_context(|| format!("required env var {env_name} is not set"));
    }

    for env_name in default_envs {
        if let Ok(value) = env::var(env_name) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }

    if !default_envs.is_empty() {
        return Err(anyhow!(
            "missing config value {value_key}; checked env vars {}",
            default_envs.join(", ")
        ));
    }

    Err(anyhow!(
        "missing config value {value_key} or env var pointer {env_key}"
    ))
}

fn resolve_connection_endpoint(
    connection_config: &Value,
    keys: &[&str],
    default_value: &str,
) -> String {
    for key in keys {
        if let Some(value) = connection_config
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return value.to_string();
        }
    }
    default_value.to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn resolve_secret_prefers_inline_value() {
        let value = resolve_secret(
            &json!({"api_key": "direct-value", "api_key_env": "IGNORED"}),
            "api_key",
            "api_key_env",
            DEFAULT_API_KEY_ENVS,
        )
        .expect("inline value should resolve");

        assert_eq!(value, "direct-value");
    }
}
