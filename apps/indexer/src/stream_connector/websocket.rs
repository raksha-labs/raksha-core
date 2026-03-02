use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct WebsocketStreamConnector {
    endpoint: String,
    stream_name: String,
    subscription_key: Option<String>,
    filter_config: Value,
    ping_message: Option<Value>,
    ping_interval: Option<Duration>,
    pong_timeout: Option<Duration>,
    pending_ping_sent_at: Option<tokio::time::Instant>,
    stream: Option<WsStream>,
}

impl WebsocketStreamConnector {
    pub fn new(
        endpoint: String,
        stream_name: String,
        subscription_key: Option<String>,
        filter_config: Value,
    ) -> Self {
        let ping_message = filter_config.get("ping_message").cloned();
        let ping_interval = parse_duration_from_seconds(filter_config.get("ping_interval_sec"));
        let pong_timeout = parse_duration_from_seconds(filter_config.get("pong_timeout_sec"));

        Self {
            endpoint,
            stream_name,
            subscription_key,
            filter_config,
            ping_message,
            ping_interval,
            pong_timeout,
            pending_ping_sent_at: None,
            stream: None,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        let endpoint = resolve_endpoint(&self.endpoint, self.subscription_key.as_deref());
        let (stream, _) = connect_async(endpoint.as_str())
            .await
            .with_context(|| format!("failed to connect websocket endpoint: {endpoint}"))?;
        self.stream = Some(stream);
        self.pending_ping_sent_at = None;

        if let Some(ws) = self.stream.as_mut() {
            for subscribe_message in build_subscribe_messages(
                &self.stream_name,
                self.subscription_key.as_deref(),
                &self.filter_config,
            ) {
                ws.send(Message::Text(subscribe_message.to_string()))
                    .await
                    .context("failed to send websocket subscription payload")?;
            }
        }

        Ok(())
    }

    pub async fn next_payload(&mut self) -> Result<Value> {
        loop {
            self.send_heartbeat_if_due().await?;
            self.ensure_heartbeat_not_timed_out()?;

            let maybe_message = match timeout(Duration::from_secs(1), async {
                let ws = self.stream.as_mut()?;
                ws.next().await
            })
            .await
            {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(message) = maybe_message else {
                return Err(anyhow!("websocket stream ended"));
            };
            let message = message.context("websocket read failed")?;
            match message {
                Message::Text(text) => {
                    let payload: Value = serde_json::from_str(&text)
                        .with_context(|| format!("invalid websocket JSON payload: {text}"))?;
                    self.pending_ping_sent_at = None;
                    return Ok(payload);
                }
                Message::Binary(binary) => {
                    let payload: Value = serde_json::from_slice(&binary)
                        .context("invalid websocket binary JSON payload")?;
                    self.pending_ping_sent_at = None;
                    return Ok(payload);
                }
                Message::Ping(payload) => {
                    let Some(ws) = self.stream.as_mut() else {
                        return Err(anyhow!("websocket connector disconnected while sending pong"));
                    };
                    ws.send(Message::Pong(payload))
                        .await
                        .context("failed to send websocket pong")?;
                    self.pending_ping_sent_at = None;
                    continue;
                }
                Message::Pong(_) => {
                    self.pending_ping_sent_at = None;
                    continue;
                }
                Message::Frame(_) => continue,
                Message::Close(frame) => {
                    return Err(anyhow!("websocket closed: {frame:?}"));
                }
            }
        }
    }

    async fn send_heartbeat_if_due(&mut self) -> Result<()> {
        let Some(interval) = self.ping_interval else {
            return Ok(());
        };
        if let Some(sent_at) = self.pending_ping_sent_at {
            if sent_at.elapsed() < interval {
                return Ok(());
            }
        }

        let Some(ws) = self.stream.as_mut() else {
            return Ok(());
        };

        let ping_message = self
            .ping_message
            .as_ref()
            .map(to_websocket_message)
            .unwrap_or_else(|| Message::Ping(Vec::new()));
        ws.send(ping_message)
            .await
            .context("failed sending websocket heartbeat ping")?;
        self.pending_ping_sent_at = Some(tokio::time::Instant::now());
        Ok(())
    }

    fn ensure_heartbeat_not_timed_out(&self) -> Result<()> {
        let Some(timeout) = self.pong_timeout else {
            return Ok(());
        };
        let Some(sent_at) = self.pending_ping_sent_at else {
            return Ok(());
        };
        if sent_at.elapsed() > timeout {
            return Err(anyhow!(
                "websocket heartbeat timeout exceeded (>{}s)",
                timeout.as_secs()
            ));
        }
        Ok(())
    }
}

fn resolve_endpoint(endpoint: &str, subscription_key: Option<&str>) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    let Some(subscription_key) = subscription_key else {
        return endpoint.to_string();
    };
    let subscription_key = subscription_key.trim();
    if subscription_key.is_empty() {
        return endpoint.to_string();
    }
    if endpoint.contains("{subscription_key}") {
        return endpoint.replace("{subscription_key}", subscription_key);
    }
    if endpoint.contains("binance.com") && endpoint.contains("/ws") {
        return format!("{endpoint}/{subscription_key}");
    }
    endpoint.to_string()
}

fn build_subscribe_messages(
    stream_name: &str,
    subscription_key: Option<&str>,
    filter_config: &Value,
) -> Vec<Value> {
    let subscription_key = subscription_key.map(str::trim).filter(|value| !value.is_empty());
    if let Some(custom_message) = filter_config.get("subscribe_message") {
        return match custom_message {
            Value::Array(items) => items
                .iter()
                .map(|item| template_value(item.clone(), subscription_key))
                .collect::<Vec<_>>(),
            Value::Object(_) => vec![template_value(custom_message.clone(), subscription_key)],
            _ => Vec::new(),
        };
    }

    let mut symbols: Vec<String> = filter_config
        .get("symbols")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if symbols.is_empty() {
        symbols = filter_config
            .get("market_symbols")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
    }

    if symbols.is_empty() {
        return Vec::new();
    }

    let params = if stream_name.eq_ignore_ascii_case("miniTicker")
        || stream_name.eq_ignore_ascii_case("ticker")
    {
        symbols
            .iter()
            .map(|symbol| {
                if symbol.contains('@') {
                    symbol.to_ascii_lowercase()
                } else {
                    format!("{}@{}", symbol.to_ascii_lowercase(), stream_name)
                }
            })
            .collect::<Vec<_>>()
    } else if let Some(subscription_key) = subscription_key {
        vec![subscription_key.to_string()]
    } else {
        symbols
    };

    vec![template_value(
        serde_json::json!({
        "method": "SUBSCRIBE",
        "params": params,
        "id": 1
        }),
        subscription_key,
    )]
}

fn parse_duration_from_seconds(value: Option<&Value>) -> Option<Duration> {
    let secs = value
        .and_then(|raw| raw.as_f64().or_else(|| raw.as_str().and_then(|text| text.parse::<f64>().ok())))
        .filter(|raw| raw.is_finite() && *raw > 0.0)?;
    Some(Duration::from_secs_f64(secs))
}

fn template_value(value: Value, subscription_key: Option<&str>) -> Value {
    let Some(subscription_key) = subscription_key else {
        return value;
    };

    match value {
        Value::String(text) => Value::String(text.replace("{subscription_key}", subscription_key)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| template_value(item, Some(subscription_key)))
                .collect::<Vec<_>>(),
        ),
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, item)| (key, template_value(item, Some(subscription_key))))
                .collect(),
        ),
        other => other,
    }
}

fn to_websocket_message(value: &Value) -> Message {
    match value {
        Value::String(text) => Message::Text(text.clone()),
        Value::Array(_) | Value::Object(_) => Message::Text(value.to_string()),
        Value::Null => Message::Ping(Vec::new()),
        Value::Bool(boolean) => Message::Text(boolean.to_string()),
        Value::Number(number) => Message::Text(number.to_string()),
    }
}
