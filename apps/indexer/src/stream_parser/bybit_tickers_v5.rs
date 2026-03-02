use serde_json::{json, Value};

use super::{
    extract_symbol_from_topic, observed_at, parse_f64, parse_ts_from_path, source_event_id,
    split_symbol_pair, symbol_to_market_key, ParsedFeedEvent, ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut symbol = payload
        .get("symbol")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            payload
                .get("topic")
                .and_then(Value::as_str)
                .and_then(extract_symbol_from_topic)
        });
    let mut price = parse_f64(payload.get("lastPrice").or_else(|| payload.get("last")));

    if payload.get("data").is_some() {
        if let Some(item) = payload.get("data") {
            if item.is_object() {
                if symbol.is_none() {
                    symbol = item
                        .get("symbol")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                }
                if price.is_none() {
                    price = parse_f64(item.get("lastPrice").or_else(|| item.get("last")));
                }
            } else if let Some(items) = item.as_array() {
                for entry in items {
                    if symbol.is_none() {
                        symbol = entry
                            .get("symbol")
                            .and_then(Value::as_str)
                            .map(ToString::to_string);
                    }
                    if price.is_none() {
                        price = parse_f64(entry.get("lastPrice").or_else(|| entry.get("last")));
                    }
                    if symbol.is_some() && price.is_some() {
                        break;
                    }
                }
            }
        }
    }

    let symbol = symbol
        .or_else(|| input.asset_pair_hint.map(ToString::to_string))
        .ok_or_else(|| "missing_bybit_symbol".to_string())?;
    let price = price.ok_or_else(|| "missing_bybit_last_price".to_string())?;

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| super::parse_ts_from_path(payload, Some("$.ts"), "ms"));
    let observed_at = observed_at(payload_event_ts);

    let market_key = input
        .market_key_hint
        .map(ToString::to_string)
        .or_else(|| symbol_to_market_key(&symbol));
    let quote_asset = split_symbol_pair(&symbol).map(|(_, quote)| quote);

    Ok(ParsedFeedEvent {
        event_type: input.event_type.to_string(),
        event_id: source_event_id(payload),
        market_key,
        asset_pair: Some(symbol.clone()),
        price: Some(price),
        chain_id: None,
        block_number: None,
        tx_hash: None,
        log_index: None,
        topic0: None,
        payload_event_ts,
        observed_at,
        normalized_fields: json!({
            "symbol": symbol,
            "raw_price": price,
            "quote_asset": quote_asset
        }),
    })
}
