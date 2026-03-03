use chrono::{DateTime, TimeZone, Utc};
use ethers::types::U256;
use serde_json::{json, Value};

mod aerodrome_swap_price;
mod binance_miniticker;
mod bybit_tickers_v5;
mod chainlink_answer_updated;
mod coinbase_ticker;
mod evm_log;
mod gate_ticker_v4;
mod gemini_marketdata_v1;
mod kraken_ticker_v2;
mod okx_tickers_v5;
mod protocol_pause;
mod protocol_tvl;
mod pyth_hermes_v2;
mod uniswap_v2_swap;
mod uniswap_v2_swap_price;
mod uniswap_v3_swap_price;

#[derive(Debug, Clone)]
pub struct ParserInput<'a> {
    pub parser_name: &'a str,
    pub event_type: &'a str,
    pub market_key_hint: Option<&'a str>,
    pub asset_pair_hint: Option<&'a str>,
    pub payload_ts_path: Option<&'a str>,
    pub payload_ts_unit: &'a str,
    pub filter_config: &'a Value,
}

#[derive(Debug, Clone)]
pub struct ParsedFeedEvent {
    pub event_type: String,
    pub event_id: Option<String>,
    pub market_key: Option<String>,
    pub asset_pair: Option<String>,
    pub price: Option<f64>,
    pub chain_id: Option<i64>,
    pub block_number: Option<i64>,
    pub tx_hash: Option<String>,
    pub log_index: Option<i64>,
    pub topic0: Option<String>,
    pub payload_event_ts: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub normalized_fields: Value,
}

pub fn parse_payload(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    match input.parser_name {
        "binance_miniticker_v1" => binance_miniticker::parse(input, payload),
        "coinbase_ticker_v1" => coinbase_ticker::parse(input, payload),
        "kraken_ticker_v2" => kraken_ticker_v2::parse(input, payload),
        "okx_tickers_v5" => okx_tickers_v5::parse(input, payload),
        "bybit_tickers_v5" => bybit_tickers_v5::parse(input, payload),
        "gate_ticker_v4" => gate_ticker_v4::parse(input, payload),
        "gemini_marketdata_v1" => gemini_marketdata_v1::parse(input, payload),
        "evm_log_v1" => evm_log::parse(input, payload),
        "chainlink_answer_updated_v1" => chainlink_answer_updated::parse(input, payload),
        "pyth_hermes_v2" => pyth_hermes_v2::parse(input, payload),
        "uniswap_v2_swap_v1" => uniswap_v2_swap::parse(input, payload),
        "uniswap_v2_swap_price_v1" => uniswap_v2_swap_price::parse(input, payload),
        "uniswap_v3_swap_price_v1" => uniswap_v3_swap_price::parse(input, payload),
        "aerodrome_swap_price_v1" => aerodrome_swap_price::parse(input, payload),
        "protocol_tvl_v1" => protocol_tvl::parse(input, payload),
        "protocol_pause_v1" => protocol_pause::parse(input, payload),
        other => Err(format!("unsupported_parser:{other}")),
    }
}

pub(super) fn parse_i64(value: Option<&Value>) -> Option<i64> {
    let raw = value?;
    if let Some(number) = raw.as_i64() {
        return Some(number);
    }
    if let Some(text) = raw.as_str() {
        if let Some(hex) = text.strip_prefix("0x") {
            return i64::from_str_radix(hex, 16).ok();
        }
        return text.parse::<i64>().ok();
    }
    None
}

pub(super) fn parse_f64(value: Option<&Value>) -> Option<f64> {
    let raw = value?;
    if let Some(number) = raw.as_f64() {
        return Some(number);
    }
    if let Some(text) = raw.as_str() {
        return text.parse::<f64>().ok();
    }
    None
}

pub(super) fn parse_ts_from_path(
    payload: &Value,
    path: Option<&str>,
    unit: &str,
) -> Option<DateTime<Utc>> {
    let path = path?;
    let path = path.strip_prefix("$.").unwrap_or(path);
    let value = payload.get(path)?;
    parse_ts_value(value, unit)
}

pub(super) fn parse_ts_value(value: &Value, unit: &str) -> Option<DateTime<Utc>> {
    match unit {
        "iso8601" => value
            .as_str()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|ts| ts.with_timezone(&Utc)),
        "s" => parse_i64(Some(value)).and_then(|secs| Utc.timestamp_opt(secs, 0).single()),
        _ => parse_i64(Some(value)).and_then(|millis| Utc.timestamp_millis_opt(millis).single()),
    }
}

pub(super) fn observed_at(payload_event_ts: Option<DateTime<Utc>>) -> DateTime<Utc> {
    payload_event_ts.unwrap_or_else(Utc::now)
}

pub(super) fn source_event_id(payload: &Value) -> Option<String> {
    payload
        .get("id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            payload
                .get("event_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            payload.get("sequence_num").and_then(|value| {
                value
                    .as_str()
                    .map(ToString::to_string)
                    .or_else(|| value.as_u64().map(|number| number.to_string()))
            })
        })
        .or_else(|| {
            payload.get("socket_sequence").and_then(|value| {
                value
                    .as_str()
                    .map(ToString::to_string)
                    .or_else(|| value.as_u64().map(|number| number.to_string()))
            })
        })
}

pub(super) fn symbol_to_market_key(symbol: &str) -> Option<String> {
    let (base, quote) = split_symbol_pair(symbol)?;
    if matches!(quote.as_str(), "USD" | "USDT" | "USDC") {
        return Some(format!("{base}/USD"));
    }
    Some(format!("{base}/{quote}"))
}

pub(super) fn split_symbol_pair(symbol: &str) -> Option<(String, String)> {
    let cleaned = symbol.trim().to_ascii_uppercase().replace([':', ' '], "");
    if cleaned.is_empty() {
        return None;
    }

    for delimiter in ['/', '-'] {
        if cleaned.contains(delimiter) {
            let mut parts = cleaned.split(delimiter).filter(|part| !part.is_empty());
            let base = parts.next()?.to_string();
            let quote = parts.next()?.to_string();
            if !base.is_empty() && !quote.is_empty() {
                return Some((base, quote));
            }
        }
    }

    for suffix in ["USDT", "USDC", "USD"] {
        if let Some(base) = cleaned.strip_suffix(suffix) {
            if !base.is_empty() {
                return Some((base.to_string(), suffix.to_string()));
            }
        }
    }

    None
}

pub(super) fn extract_symbol_from_topic(topic: &str) -> Option<String> {
    let topic = topic.trim();
    let value = topic.strip_prefix("tickers.")?;
    if value.is_empty() {
        return None;
    }
    Some(value.to_ascii_uppercase())
}

pub(super) fn decode_hex_words(data_hex: &str) -> Vec<String> {
    let Some(body) = data_hex.trim().strip_prefix("0x") else {
        return Vec::new();
    };
    if body.len() < 64 {
        return Vec::new();
    }

    body.as_bytes()
        .chunks(64)
        .filter_map(|chunk| std::str::from_utf8(chunk).ok())
        .map(|chunk| chunk.to_string())
        .collect::<Vec<_>>()
}

pub(super) fn parse_u256_word_to_f64(word: &str) -> Option<f64> {
    let value = U256::from_str_radix(word.trim_start_matches("0x"), 16).ok()?;
    value.to_string().parse::<f64>().ok()
}

pub(super) fn parse_i256_word_to_f64(word: &str) -> Option<f64> {
    let trimmed = word.trim_start_matches("0x");
    let value = U256::from_str_radix(trimmed, 16).ok()?;
    let negative = trimmed
        .chars()
        .next()
        .and_then(|ch| ch.to_digit(16))
        .map(|digit| digit >= 8)
        .unwrap_or(false);

    if !negative {
        return value.to_string().parse::<f64>().ok();
    }

    let magnitude = (!value).overflowing_add(U256::from(1u8)).0;
    magnitude.to_string().parse::<f64>().ok().map(|v| -v)
}

pub(super) fn scale_price(raw_value: f64, decimals: i32) -> Option<f64> {
    if !raw_value.is_finite() {
        return None;
    }
    let divisor = 10f64.powi(decimals);
    if !divisor.is_finite() || divisor == 0.0 {
        return None;
    }
    Some(raw_value / divisor)
}

pub(super) fn empty_normalized() -> Value {
    json!({})
}
