use serde_json::{json, Value};

use super::{
    observed_at, parse_f64, parse_ts_from_path, source_event_id, split_symbol_pair,
    symbol_to_market_key, ParsedFeedEvent, ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut product_id = payload
        .get("product_id")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut price = parse_f64(payload.get("price"));

    if (product_id.is_none() || price.is_none())
        && payload.get("events").and_then(Value::as_array).is_some()
    {
        if let Some(events) = payload.get("events").and_then(Value::as_array) {
            for event in events {
                if let Some(tickers) = event.get("tickers").and_then(Value::as_array) {
                    for ticker in tickers {
                        if product_id.is_none() {
                            product_id = ticker
                                .get("product_id")
                                .and_then(Value::as_str)
                                .map(ToString::to_string);
                        }
                        if price.is_none() {
                            price = parse_f64(ticker.get("price").or_else(|| ticker.get("last")));
                        }
                        if product_id.is_some() && price.is_some() {
                            break;
                        }
                    }
                }
                if product_id.is_some() && price.is_some() {
                    break;
                }
            }
        }
    }

    let product_id = product_id
        .or_else(|| input.asset_pair_hint.map(ToString::to_string))
        .ok_or_else(|| "missing_coinbase_product_id".to_string())?;
    let price = price.ok_or_else(|| "missing_coinbase_price".to_string())?;

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| super::parse_ts_from_path(payload, Some("$.timestamp"), "iso8601"));
    let observed_at = observed_at(payload_event_ts);

    let market_key = input
        .market_key_hint
        .map(ToString::to_string)
        .or_else(|| symbol_to_market_key(&product_id));
    let quote_asset = split_symbol_pair(&product_id).map(|(_, quote)| quote);

    Ok(ParsedFeedEvent {
        event_type: input.event_type.to_string(),
        event_id: source_event_id(payload),
        market_key,
        asset_pair: Some(product_id.clone()),
        price: Some(price),
        chain_id: None,
        block_number: None,
        tx_hash: None,
        log_index: None,
        topic0: None,
        payload_event_ts,
        observed_at,
        normalized_fields: json!({
            "product_id": product_id,
            "raw_price": price,
            "quote_asset": quote_asset
        }),
    })
}
