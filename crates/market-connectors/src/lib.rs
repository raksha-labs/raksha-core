use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::Message,
    MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSourceConfig {
    pub tenant_id: String,
    pub source_id: String,
    pub source_kind: String,
    pub source_name: String,
    pub ws_endpoint: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedQuote {
    pub source_symbol: String,
    pub price: f64,
    pub observed_at: DateTime<Utc>,
    pub metadata: Value,
}

#[async_trait]
pub trait MarketWsConnector: Send {
    async fn connect(&mut self) -> Result<()>;
    async fn subscribe(&mut self, market_symbols: &[String]) -> Result<()>;
    async fn next_quote(&mut self) -> Result<ParsedQuote>;
    fn source_config(&self) -> &MarketSourceConfig;
}

type QuoteParser = fn(&Value) -> Result<ParsedQuote>;
type SubscriptionBuilder = fn(&[String]) -> Vec<Value>;
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct WsJsonConnector {
    config: MarketSourceConfig,
    parser: QuoteParser,
    subscription_builder: SubscriptionBuilder,
    stream: Option<WsStream>,
}

impl WsJsonConnector {
    pub fn new(
        config: MarketSourceConfig,
        parser: QuoteParser,
        subscription_builder: SubscriptionBuilder,
    ) -> Self {
        Self {
            config,
            parser,
            subscription_builder,
            stream: None,
        }
    }
}

#[async_trait]
impl MarketWsConnector for WsJsonConnector {
    async fn connect(&mut self) -> Result<()> {
        let (stream, _) = connect_async(self.config.ws_endpoint.as_str())
            .await
            .with_context(|| {
                format!(
                    "failed websocket connect for source '{}' at {}",
                    self.config.source_id, self.config.ws_endpoint
                )
            })?;
        self.stream = Some(stream);
        Ok(())
    }

    async fn subscribe(&mut self, market_symbols: &[String]) -> Result<()> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(anyhow!(
                "cannot subscribe before connect for source {}",
                self.config.source_id
            ));
        };

        let messages = (self.subscription_builder)(market_symbols);
        for payload in messages {
            stream
                .send(Message::Text(payload.to_string()))
                .await
                .with_context(|| {
                    format!(
                        "failed to send subscription payload for source {}",
                        self.config.source_id
                    )
                })?;
        }

        Ok(())
    }

    async fn next_quote(&mut self) -> Result<ParsedQuote> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(anyhow!(
                "cannot read quote before connect for source {}",
                self.config.source_id
            ));
        };

        while let Some(msg) = stream.next().await {
            let msg = msg.with_context(|| {
                format!(
                    "websocket message read failed for source {}",
                    self.config.source_id
                )
            })?;

            match msg {
                Message::Text(text) => {
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(err) => {
                            warn!(
                                source_id = %self.config.source_id,
                                error = ?err,
                                "failed to parse source websocket payload as json",
                            );
                            continue;
                        }
                    };

                    match (self.parser)(&parsed) {
                        Ok(quote) => return Ok(quote),
                        Err(err) => {
                            debug!(
                                source_id = %self.config.source_id,
                                error = ?err,
                                "source payload ignored by parser",
                            );
                        }
                    }
                }
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(frame) => {
                    return Err(anyhow!(
                        "websocket closed for source {}: {:?}",
                        self.config.source_id,
                        frame
                    ));
                }
                Message::Frame(_) => {}
            }
        }

        Err(anyhow!(
            "websocket stream ended for source {}",
            self.config.source_id
        ))
    }

    fn source_config(&self) -> &MarketSourceConfig {
        &self.config
    }
}

pub fn build_connector(config: MarketSourceConfig) -> Result<Box<dyn MarketWsConnector>> {
    let source_name = config.source_name.to_ascii_lowercase();
    let connector = match source_name.as_str() {
        "binance" => WsJsonConnector::new(config, parse_binance_quote, subscribe_binance),
        "coinbase" => WsJsonConnector::new(config, parse_coinbase_quote, subscribe_coinbase),
        "uniswap-v3" | "uniswap" => {
            WsJsonConnector::new(config, parse_uniswap_quote, subscribe_generic_symbols)
        }
        "curve" => WsJsonConnector::new(config, parse_curve_quote, subscribe_generic_symbols),
        "chainlink" => {
            WsJsonConnector::new(config, parse_chainlink_quote, subscribe_generic_symbols)
        }
        "pyth" => WsJsonConnector::new(config, parse_pyth_quote, subscribe_generic_symbols),
        other => {
            return Err(anyhow!("unsupported market source '{}': no connector", other));
        }
    };

    Ok(Box::new(connector))
}

fn subscribe_generic_symbols(symbols: &[String]) -> Vec<Value> {
    if symbols.is_empty() {
        return Vec::new();
    }

    vec![json!({
        "type": "subscribe",
        "symbols": symbols,
    })]
}

fn subscribe_binance(symbols: &[String]) -> Vec<Value> {
    let params: Vec<String> = symbols
        .iter()
        .map(|symbol| {
            symbol
                .replace('/', "")
                .replace('-', "")
                .to_ascii_lowercase()
                + "@ticker"
        })
        .collect();

    if params.is_empty() {
        return Vec::new();
    }

    vec![json!({
        "method": "SUBSCRIBE",
        "params": params,
        "id": 1,
    })]
}

fn subscribe_coinbase(symbols: &[String]) -> Vec<Value> {
    if symbols.is_empty() {
        return Vec::new();
    }

    vec![json!({
        "type": "subscribe",
        "channels": [
            {
                "name": "ticker",
                "product_ids": symbols,
            }
        ]
    })]
}

fn parse_binance_quote(value: &Value) -> Result<ParsedQuote> {
    let root = value.get("data").unwrap_or(value);
    let source_symbol = root
        .get("s")
        .and_then(Value::as_str)
        .or_else(|| root.get("symbol").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("missing binance symbol field"))?
        .to_string();

    let price = parse_number(
        root.get("c")
            .or_else(|| root.get("p"))
            .or_else(|| root.get("price"))
            .ok_or_else(|| anyhow!("missing binance price field"))?,
    )?;

    let observed_at = parse_millis_timestamp(root.get("E").or_else(|| root.get("eventTime")))
        .unwrap_or_else(Utc::now);

    Ok(ParsedQuote {
        source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}

fn parse_coinbase_quote(value: &Value) -> Result<ParsedQuote> {
    let quote_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing coinbase type"))?;
    if quote_type != "ticker" && quote_type != "ticker_batch" {
        return Err(anyhow!("ignoring coinbase non-ticker message"));
    }

    let source_symbol = value
        .get("product_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing coinbase product_id"))?
        .to_string();
    let price = parse_number(
        value
            .get("price")
            .ok_or_else(|| anyhow!("missing coinbase price"))?,
    )?;

    let observed_at = value
        .get("time")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or_else(Utc::now);

    Ok(ParsedQuote {
        source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}

fn parse_uniswap_quote(value: &Value) -> Result<ParsedQuote> {
    let source_symbol = value
        .get("pair")
        .and_then(Value::as_str)
        .or_else(|| value.get("symbol").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("missing uniswap pair/symbol"))?
        .to_string();

    let price = parse_number(
        value
            .get("price")
            .or_else(|| value.get("mid_price"))
            .ok_or_else(|| anyhow!("missing uniswap price"))?,
    )?;

    let observed_at = parse_any_timestamp(
        value
            .get("timestamp")
            .or_else(|| value.get("observed_at"))
            .or_else(|| value.get("time")),
    )
    .unwrap_or_else(Utc::now);

    Ok(ParsedQuote {
        source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}

fn parse_curve_quote(value: &Value) -> Result<ParsedQuote> {
    let source_symbol = value
        .get("pair")
        .and_then(Value::as_str)
        .or_else(|| value.get("symbol").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("missing curve pair/symbol"))?
        .to_string();
    let price = parse_number(
        value
            .get("price")
            .or_else(|| value.get("virtual_price"))
            .ok_or_else(|| anyhow!("missing curve price"))?,
    )?;

    let observed_at = parse_any_timestamp(
        value
            .get("timestamp")
            .or_else(|| value.get("observed_at"))
            .or_else(|| value.get("time")),
    )
    .unwrap_or_else(Utc::now);

    Ok(ParsedQuote {
        source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}

fn parse_chainlink_quote(value: &Value) -> Result<ParsedQuote> {
    let source_symbol = value
        .get("feed")
        .and_then(Value::as_str)
        .or_else(|| value.get("symbol").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("missing chainlink feed/symbol"))?
        .to_string();

    let price = parse_number(
        value
            .get("answer")
            .or_else(|| value.get("price"))
            .ok_or_else(|| anyhow!("missing chainlink answer/price"))?,
    )?;

    let observed_at = parse_any_timestamp(
        value
            .get("updated_at")
            .or_else(|| value.get("timestamp"))
            .or_else(|| value.get("time")),
    )
    .unwrap_or_else(Utc::now);

    Ok(ParsedQuote {
        source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}

fn parse_pyth_quote(value: &Value) -> Result<ParsedQuote> {
    let feed = value.get("price_feed").unwrap_or(value);
    let source_symbol = feed
        .get("symbol")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing pyth symbol"))?
        .to_string();

    let price = parse_number(
        feed.get("price")
            .ok_or_else(|| anyhow!("missing pyth price"))?,
    )?;

    let observed_at = parse_any_timestamp(
        feed
            .get("publish_time")
            .or_else(|| feed.get("timestamp"))
            .or_else(|| feed.get("time")),
    )
    .unwrap_or_else(Utc::now);

    Ok(ParsedQuote {
        source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}

fn parse_number(value: &Value) -> Result<f64> {
    if let Some(num) = value.as_f64() {
        return Ok(num);
    }

    if let Some(raw) = value.as_str() {
        return raw
            .parse::<f64>()
            .with_context(|| format!("invalid numeric string '{raw}'"));
    }

    Err(anyhow!("unsupported numeric value: {}", value))
}

fn parse_millis_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let millis = value.and_then(|raw| raw.as_i64().or_else(|| raw.as_str()?.parse::<i64>().ok()))?;
    Utc.timestamp_millis_opt(millis).single()
}

fn parse_any_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let Some(raw) = value else {
        return None;
    };

    if let Some(v) = raw.as_i64() {
        return Utc.timestamp_opt(v, 0).single();
    }

    if let Some(v) = raw.as_u64() {
        let secs = i64::try_from(v).ok()?;
        return Utc.timestamp_opt(secs, 0).single();
    }

    if let Some(v) = raw.as_str() {
        if let Ok(parsed_int) = v.parse::<i64>() {
            return Utc.timestamp_opt(parsed_int, 0).single();
        }
        return parse_rfc3339(v);
    }

    None
}

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|ts| ts.with_timezone(&Utc))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_payload() {
        let payload = json!({
            "stream": "ethusdt@ticker",
            "data": {
                "s": "ETHUSDT",
                "c": "3150.12",
                "E": 1700000000000_i64
            }
        });

        let quote = parse_binance_quote(&payload).expect("binance payload should parse");
        assert_eq!(quote.source_symbol, "ETHUSDT");
        assert!(quote.price > 3000.0);
    }

    #[test]
    fn parses_coinbase_payload() {
        let payload = json!({
            "type": "ticker",
            "product_id": "USDC-USD",
            "price": "0.998",
            "time": "2026-02-16T12:34:56Z"
        });

        let quote = parse_coinbase_quote(&payload).expect("coinbase payload should parse");
        assert_eq!(quote.source_symbol, "USDC-USD");
        assert!(quote.price > 0.9);
    }

    #[test]
    fn parses_chainlink_payload() {
        let payload = json!({
            "feed": "USDT/USD",
            "answer": "1.001",
            "updated_at": 1700000000_i64
        });

        let quote = parse_chainlink_quote(&payload).expect("chainlink payload should parse");
        assert_eq!(quote.source_symbol, "USDT/USD");
        assert!(quote.price > 0.9);
    }

    #[test]
    fn parses_pyth_payload() {
        let payload = json!({
            "price_feed": {
                "symbol": "USDC/USD",
                "price": "1.002",
                "publish_time": 1700000000_i64
            }
        });

        let quote = parse_pyth_quote(&payload).expect("pyth payload should parse");
        assert_eq!(quote.source_symbol, "USDC/USD");
        assert!(quote.price > 0.9);
    }
}
