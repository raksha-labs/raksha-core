use serde_json::{json, Value};

use super::{
    observed_at, parse_ts_from_path, source_event_id, split_symbol_pair, symbol_to_market_key,
    ParsedFeedEvent, ParserInput,
};

fn extract_kraken_last(value: Option<&Value>) -> Option<f64> {
    let raw = value?;
    if let Some(number) = raw.as_f64() {
        return Some(number);
    }
    if let Some(text) = raw.as_str() {
        return text.parse::<f64>().ok();
    }
    if let Some(object) = raw.as_object() {
        if let Some(price) = object.get("price") {
            return extract_kraken_last(Some(price));
        }
    }
    None
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut symbol = payload
        .get("symbol")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut price = extract_kraken_last(payload.get("last"));

    if (symbol.is_none() || price.is_none())
        && payload.get("data").and_then(Value::as_array).is_some()
    {
        if let Some(items) = payload.get("data").and_then(Value::as_array) {
            for item in items {
                if symbol.is_none() {
                    symbol = item
                        .get("symbol")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                }
                if price.is_none() {
                    price = extract_kraken_last(
                        item.get("last")
                            .or_else(|| item.get("last_price"))
                            .or_else(|| item.get("price")),
                    );
                }
                if symbol.is_some() && price.is_some() {
                    break;
                }
            }
        }
    }

    let symbol = symbol
        .or_else(|| input.asset_pair_hint.map(ToString::to_string))
        .ok_or_else(|| "missing_kraken_symbol".to_string())?;
    let price = price.ok_or_else(|| "missing_kraken_last".to_string())?;

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| super::parse_ts_from_path(payload, Some("$.time_in"), "iso8601"))
            .or_else(|| super::parse_ts_from_path(payload, Some("$.time_out"), "iso8601"));
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
