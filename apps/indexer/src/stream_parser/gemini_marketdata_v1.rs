use serde_json::{json, Value};

use super::{
    observed_at, parse_f64, parse_ts_from_path, parse_ts_value, source_event_id, split_symbol_pair,
    symbol_to_market_key, ParsedFeedEvent, ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let symbol = payload
        .get("symbol")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| input.asset_pair_hint.map(ToString::to_string))
        .ok_or_else(|| "missing_gemini_symbol".to_string())?;

    let mut price = parse_f64(
        payload
            .get("price")
            .or_else(|| payload.get("last"))
            .or_else(|| payload.get("last_price")),
    );
    let mut event_ts: Option<Value> = payload.get("timestampms").cloned();

    if let Some(events) = payload.get("events").and_then(Value::as_array) {
        for event in events {
            if price.is_none() {
                price = parse_f64(
                    event
                        .get("price")
                        .or_else(|| event.get("last"))
                        .or_else(|| event.get("last_price")),
                );
            }
            if event_ts.is_none() {
                event_ts = event
                    .get("timestampms")
                    .cloned()
                    .or_else(|| event.get("timestamp").cloned());
            }
            if price.is_some() && event_ts.is_some() {
                break;
            }
        }
    }

    let price = price.ok_or_else(|| "missing_gemini_price".to_string())?;
    let payload_event_ts = parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
        .or_else(|| {
            event_ts
                .as_ref()
                .and_then(|value| parse_ts_value(value, "ms").or_else(|| parse_ts_value(value, "s")))
        });
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
