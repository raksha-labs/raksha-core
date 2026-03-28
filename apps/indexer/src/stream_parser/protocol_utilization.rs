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

    let raw_utilization = payload
        .get("utilization")
        .or_else(|| payload.get("current_utilization"))
        .and_then(value_to_f64)
        .ok_or_else(|| "missing_utilization".to_string())?;
    let utilization = if raw_utilization <= 1.0 {
        raw_utilization * 100.0
    } else {
        raw_utilization
    };

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| parse_ts_from_path(payload, Some("$.timestamp"), "ms"));
    let observed_at = observed_at(payload_event_ts);

    Ok(ParsedFeedEvent {
        event_type: payload
            .get("event_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(input.event_type)
            .to_string(),
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
            "metric": "utilization",
            "protocol_id": protocol_id,
            "chain_slug": chain_slug,
            "market_id": market_id,
            "utilization_pct": utilization,
            "utilization": utilization,
            "tvl_usd": payload.get("tvl_usd").and_then(value_to_f64),
            "total_supplied_tokens": payload
                .get("total_supplied_tokens")
                .or_else(|| payload.get("total_supplied"))
                .and_then(value_to_f64),
            "total_borrowed_tokens": payload
                .get("total_borrowed_tokens")
                .or_else(|| payload.get("total_borrowed"))
                .and_then(value_to_f64),
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
