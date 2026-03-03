use serde_json::{json, Value};

use super::{
    observed_at, parse_i64, parse_ts_from_path, source_event_id, ParsedFeedEvent, ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let protocol_id = payload
        .get("protocol_id")
        .or_else(|| payload.get("protocol"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing_protocol_id".to_string())?
        .to_ascii_lowercase();

    let chain_slug = payload
        .get("chain_slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .or_else(|| {
            chain_slug_from_chain_id(parse_i64(
                payload.get("chainId").or_else(|| payload.get("chain_id")),
            ))
        })
        .unwrap_or_else(|| "unknown".to_string());

    let market_id = payload
        .get("market_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("all"))
        .map(|value| value.to_ascii_lowercase());

    let decimals = payload
        .get("decimals")
        .or_else(|| payload.get("tvl_decimals"))
        .and_then(value_to_i32)
        .unwrap_or(18);
    let price_usd = payload
        .get("price_usd")
        .and_then(value_to_f64)
        .filter(|value| value.is_finite() && *value > 0.0);

    let tvl_raw_f64 = payload.get("tvl_raw").and_then(value_to_f64);
    let tvl_native = payload
        .get("tvl_native")
        .and_then(value_to_f64)
        .or_else(|| tvl_raw_f64.and_then(|raw| scale_by_decimals(raw, decimals)));
    let tvl_usd = payload
        .get("tvl_usd")
        .and_then(value_to_f64)
        .or_else(|| match (tvl_native, price_usd) {
            (Some(native), Some(price)) => Some(native * price),
            _ => None,
        })
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| "missing_tvl_usd".to_string())?;

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| parse_ts_from_path(payload, Some("$.timestamp"), "ms"));
    let observed_at = observed_at(payload_event_ts);

    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(input.event_type)
        .to_string();

    Ok(ParsedFeedEvent {
        event_type,
        event_id: source_event_id(payload),
        market_key: input.market_key_hint.map(ToString::to_string),
        asset_pair: input.asset_pair_hint.map(ToString::to_string),
        price: None,
        chain_id: parse_i64(payload.get("chainId").or_else(|| payload.get("chain_id"))),
        block_number: parse_i64(
            payload
                .get("block_number")
                .or_else(|| payload.get("blockNumber")),
        ),
        tx_hash: payload
            .get("tx_hash")
            .or_else(|| payload.get("transactionHash"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        log_index: parse_i64(payload.get("log_index").or_else(|| payload.get("logIndex"))),
        topic0: payload
            .get("topic0")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        payload_event_ts,
        observed_at,
        normalized_fields: json!({
            "metric": "tvl",
            "protocol_id": protocol_id,
            "chain_slug": chain_slug,
            "market_id": market_id,
            "tvl_usd": tvl_usd,
            "tvl_native": tvl_native,
            "tvl_raw": tvl_raw_f64,
            "price_usd": price_usd,
            "decimals": decimals,
            "quote_asset": "USD"
        }),
    })
}

fn value_to_f64(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value
        .as_str()
        .and_then(|text| text.trim().parse::<f64>().ok())
}

fn value_to_i32(value: &Value) -> Option<i32> {
    if let Some(number) = value.as_i64() {
        return i32::try_from(number).ok();
    }
    value
        .as_str()
        .and_then(|text| text.trim().parse::<i32>().ok())
}

fn scale_by_decimals(raw: f64, decimals: i32) -> Option<f64> {
    if !raw.is_finite() {
        return None;
    }
    let divisor = 10f64.powi(decimals.max(0));
    if !divisor.is_finite() || divisor <= 0.0 {
        return None;
    }
    Some(raw / divisor)
}

fn chain_slug_from_chain_id(chain_id: Option<i64>) -> Option<String> {
    match chain_id {
        Some(1) => Some("ethereum".to_string()),
        Some(42161) => Some("arbitrum".to_string()),
        Some(10) => Some("optimism".to_string()),
        Some(8453) => Some("base".to_string()),
        Some(137) => Some("polygon".to_string()),
        Some(43114) => Some("avalanche".to_string()),
        Some(56) => Some("bsc".to_string()),
        _ => None,
    }
}
