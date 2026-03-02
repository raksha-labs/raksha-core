use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use common::DataSourceConfig;
use event_schema::{SourceType, UnifiedEvent};
use serde_json::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Universal connector trait
// ---------------------------------------------------------------------------

/// Every data source — EVM chain, CEX WebSocket, DEX API, Oracle — implements
/// this trait. The indexer holds a `Vec<Box<dyn DataSourceConnector>>` and
/// drives them all through the same event loop.
#[async_trait]
pub trait DataSourceConnector: Send {
    fn source_id(&self) -> &str;
    fn source_type(&self) -> SourceType;
    fn tenant_id(&self) -> &str;

    async fn connect(&mut self) -> Result<()>;
    async fn next_event(&mut self) -> Result<UnifiedEvent>;
    fn is_healthy(&self) -> bool;
}

// ---------------------------------------------------------------------------
// CEX WebSocket connector (Binance, Coinbase, Uniswap-v3, Curve, Chainlink, Pyth)
// Consolidates the old market-connectors crate.
// ---------------------------------------------------------------------------

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type QuoteParser = fn(&Value) -> Result<ParsedQuote>;
type SubscriptionBuilder = fn(&[String]) -> Vec<Value>;

#[derive(Debug, Clone)]
pub struct ParsedQuote {
    pub market_key: String,
    pub price: f64,
    pub observed_at: chrono::DateTime<Utc>,
    pub metadata: Value,
}

pub struct CexWebsocketConnector {
    tenant_id: String,
    source_id: String,
    ws_endpoint: String,
    market_symbols: Vec<String>,
    parser: QuoteParser,
    subscription_builder: SubscriptionBuilder,
    stream: Option<WsStream>,
    healthy: bool,
}

impl CexWebsocketConnector {
    /// Build a connector from a `DataSourceConfig`. The `connection_config` JSON
    /// is expected to contain `"ws_endpoint"` and `"source_name"`. The `filters`
    /// JSON is expected to contain `"market_symbols": [...]`.
    pub fn from_config(cfg: DataSourceConfig) -> Result<Self> {
        let ws_endpoint = cfg
            .connection_config
            .get("ws_endpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("source '{}': missing ws_endpoint", cfg.source_id))?
            .to_string();

        let source_name = cfg
            .connection_config
            .get("source_name")
            .and_then(Value::as_str)
            .unwrap_or(&cfg.source_name)
            .to_ascii_lowercase();

        let (parser, subscription_builder): (QuoteParser, SubscriptionBuilder) =
            match source_name.as_str() {
                "binance" => (parse_binance_quote, subscribe_binance),
                "coinbase" => (parse_coinbase_quote, subscribe_coinbase),
                "uniswap-v3" | "uniswap" => {
                    (parse_uniswap_quote, subscribe_generic_symbols)
                }
                "curve" => (parse_curve_quote, subscribe_generic_symbols),
                "chainlink" => (parse_chainlink_quote, subscribe_generic_symbols),
                "pyth" => (parse_pyth_quote, subscribe_generic_symbols),
                other => {
                    return Err(anyhow!(
                        "unsupported CEX source '{}': no connector",
                        other
                    ))
                }
            };

        let market_symbols: Vec<String> = cfg
            .filters
            .as_ref()
            .and_then(|f| f.get("market_symbols"))
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default();

        Ok(Self {
            tenant_id: cfg.tenant_id,
            source_id: cfg.source_id,
            ws_endpoint,
            market_symbols,
            parser,
            subscription_builder,
            stream: None,
            healthy: false,
        })
    }
}

#[async_trait]
impl DataSourceConnector for CexWebsocketConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn source_type(&self) -> SourceType {
        SourceType::CexWebsocket
    }

    fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    async fn connect(&mut self) -> Result<()> {
        let (stream, _) = connect_async(self.ws_endpoint.as_str())
            .await
            .with_context(|| {
                format!(
                    "failed websocket connect for source '{}' at {}",
                    self.source_id, self.ws_endpoint
                )
            })?;
        self.stream = Some(stream);

        // Send subscription messages
        if !self.market_symbols.is_empty() {
            let messages = (self.subscription_builder)(&self.market_symbols);
            if let Some(ws) = self.stream.as_mut() {
                for payload in messages {
                    ws.send(Message::Text(payload.to_string()))
                        .await
                        .with_context(|| {
                            format!(
                                "failed to send subscription for source {}",
                                self.source_id
                            )
                        })?;
                }
            }
        }

        self.healthy = true;
        Ok(())
    }

    async fn next_event(&mut self) -> Result<UnifiedEvent> {
        let Some(ws) = self.stream.as_mut() else {
            return Err(anyhow!(
                "cannot read before connect for source {}",
                self.source_id
            ));
        };

        while let Some(msg) = ws.next().await {
            let msg = msg.with_context(|| {
                format!(
                    "websocket read failed for source {}",
                    self.source_id
                )
            })?;

            match msg {
                Message::Text(text) => {
                    let raw: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(err) => {
                            warn!(source_id = %self.source_id, error = ?err, "invalid json");
                            continue;
                        }
                    };

                    match (self.parser)(&raw) {
                        Ok(quote) => {
                            return Ok(UnifiedEvent {
                                event_id: Uuid::new_v4().to_string(),
                                tenant_id: self.tenant_id.clone(),
                                source_id: self.source_id.clone(),
                                source_type: SourceType::CexWebsocket,
                                event_type: "quote".into(),
                                timestamp: quote.observed_at,
                                payload: quote.metadata,
                                chain_id: None,
                                block_number: None,
                                tx_hash: None,
                                market_key: Some(quote.market_key),
                                price: Some(quote.price),
                            });
                        }
                        Err(err) => {
                            debug!(
                                source_id = %self.source_id,
                                error = ?err,
                                "payload ignored by parser"
                            );
                        }
                    }
                }
                Message::Close(frame) => {
                    self.healthy = false;
                    return Err(anyhow!(
                        "websocket closed for source {}: {:?}",
                        self.source_id,
                        frame
                    ));
                }
                Message::Binary(_)
                | Message::Ping(_)
                | Message::Pong(_)
                | Message::Frame(_) => {}
            }
        }

        self.healthy = false;
        Err(anyhow!(
            "websocket stream ended for source {}",
            self.source_id
        ))
    }

    fn is_healthy(&self) -> bool {
        self.healthy
    }
}

// ---------------------------------------------------------------------------
// EVM chain connector: wraps EvmChainAdapter into DataSourceConnector.
// ---------------------------------------------------------------------------

use crate::adapter::EvmChainAdapter;

pub struct EvmChainConnector {
    tenant_id: String,
    source_id: String,
    _chain_id: i64,
    adapter: EvmChainAdapter,
    healthy: bool,
}

impl EvmChainConnector {
    pub fn new(
        tenant_id: String,
        source_id: String,
        chain_id: i64,
        adapter: EvmChainAdapter,
    ) -> Self {
        Self {
            tenant_id,
            source_id,
            _chain_id: chain_id,
            adapter,
            healthy: true,
        }
    }
}

#[async_trait]
impl DataSourceConnector for EvmChainConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn source_type(&self) -> SourceType {
        SourceType::EvmChain
    }

    fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    async fn connect(&mut self) -> Result<()> {
        // EvmChainAdapter maintains its own reconnect logic; nothing to do here.
        self.healthy = true;
        Ok(())
    }

    async fn next_event(&mut self) -> Result<UnifiedEvent> {
        use common::ChainAdapter;

        let events = self.adapter.next_events().await?;
        // Return the first event; remaining are buffered by the adapter.
        // If no events returned yet, the caller should retry.
        let event = events
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no events from EVM adapter yet"))?;

        Ok(UnifiedEvent {
            event_id: event.event_id.to_string(),
            tenant_id: self.tenant_id.clone(),
            source_id: self.source_id.clone(),
            source_type: SourceType::EvmChain,
            event_type: format!("{:?}", event.event_type).to_ascii_lowercase(),
            timestamp: event.observed_at,
            payload: serde_json::to_value(&event)?,
            chain_id: event.chain_id.map(|c| c as i64),
            block_number: Some(event.block_number as i64),
            tx_hash: Some(event.tx_hash.clone()),
            market_key: None,
            price: event.oracle_price,
        })
    }

    fn is_healthy(&self) -> bool {
        self.healthy
    }
}

// ---------------------------------------------------------------------------
// Quote parsers (migrated from crates/market-connectors)
// ---------------------------------------------------------------------------

use chrono::{TimeZone, DateTime};

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
    let millis =
        value.and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse::<i64>().ok()))?;
    Utc.timestamp_millis_opt(millis).single()
}

fn parse_any_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let raw = value?;
    if let Some(v) = raw.as_i64() {
        return Utc.timestamp_opt(v, 0).single();
    }
    if let Some(v) = raw.as_u64() {
        let secs = i64::try_from(v).ok()?;
        return Utc.timestamp_opt(secs, 0).single();
    }
    if let Some(v) = raw.as_str() {
        if let Ok(i) = v.parse::<i64>() {
            return Utc.timestamp_opt(i, 0).single();
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

fn subscribe_generic_symbols(symbols: &[String]) -> Vec<Value> {
    if symbols.is_empty() {
        return Vec::new();
    }
    vec![serde_json::json!({"type": "subscribe", "symbols": symbols})]
}

fn subscribe_binance(symbols: &[String]) -> Vec<Value> {
    let params: Vec<String> = symbols
        .iter()
        .map(|s| {
            s.replace('/', "").replace('-', "").to_ascii_lowercase() + "@ticker"
        })
        .collect();
    if params.is_empty() {
        return Vec::new();
    }
    vec![serde_json::json!({"method": "SUBSCRIBE", "params": params, "id": 1})]
}

fn subscribe_coinbase(symbols: &[String]) -> Vec<Value> {
    if symbols.is_empty() {
        return Vec::new();
    }
    vec![serde_json::json!({
        "type": "subscribe",
        "channels": [{"name": "ticker", "product_ids": symbols}]
    })]
}

fn parse_binance_quote(value: &Value) -> Result<ParsedQuote> {
    let root = value.get("data").unwrap_or(value);
    let source_symbol = root
        .get("s")
        .and_then(Value::as_str)
        .or_else(|| root.get("symbol").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("missing binance symbol"))?
        .to_string();
    let price = parse_number(
        root.get("c")
            .or_else(|| root.get("p"))
            .or_else(|| root.get("price"))
            .ok_or_else(|| anyhow!("missing binance price"))?,
    )?;
    let observed_at =
        parse_millis_timestamp(root.get("E").or_else(|| root.get("eventTime")))
            .unwrap_or_else(Utc::now);
    Ok(ParsedQuote {
        market_key: source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}

fn parse_coinbase_quote(value: &Value) -> Result<ParsedQuote> {
    let qt = value.get("type").and_then(Value::as_str).unwrap_or("");
    if qt != "ticker" && qt != "ticker_batch" {
        return Err(anyhow!("ignoring coinbase non-ticker message"));
    }
    let source_symbol = value
        .get("product_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing coinbase product_id"))?
        .to_string();
    let price = parse_number(value.get("price").ok_or_else(|| anyhow!("missing price"))?)?;
    let observed_at = value
        .get("time")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or_else(Utc::now);
    Ok(ParsedQuote {
        market_key: source_symbol,
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
        value.get("price").or_else(|| value.get("mid_price"))
            .ok_or_else(|| anyhow!("missing uniswap price"))?,
    )?;
    let observed_at =
        parse_any_timestamp(value.get("timestamp").or_else(|| value.get("time")))
            .unwrap_or_else(Utc::now);
    Ok(ParsedQuote {
        market_key: source_symbol,
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
        value.get("price").or_else(|| value.get("virtual_price"))
            .ok_or_else(|| anyhow!("missing curve price"))?,
    )?;
    let observed_at =
        parse_any_timestamp(value.get("timestamp").or_else(|| value.get("time")))
            .unwrap_or_else(Utc::now);
    Ok(ParsedQuote {
        market_key: source_symbol,
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
        value.get("answer").or_else(|| value.get("price"))
            .ok_or_else(|| anyhow!("missing chainlink answer/price"))?,
    )?;
    let observed_at = parse_any_timestamp(
        value.get("updated_at").or_else(|| value.get("timestamp")),
    )
    .unwrap_or_else(Utc::now);
    Ok(ParsedQuote {
        market_key: source_symbol,
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
        feed.get("price").ok_or_else(|| anyhow!("missing pyth price"))?,
    )?;
    let observed_at =
        parse_any_timestamp(feed.get("publish_time").or_else(|| feed.get("timestamp")))
            .unwrap_or_else(Utc::now);
    Ok(ParsedQuote {
        market_key: source_symbol,
        price,
        observed_at,
        metadata: value.clone(),
    })
}
