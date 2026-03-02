use serde_json::json;
use serde_json::Value;

use super::{
    observed_at, parse_f64, parse_ts_from_path, source_event_id, split_symbol_pair,
    symbol_to_market_key, ParsedFeedEvent, ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let symbol = payload
        .get("s")
        .and_then(Value::as_str)
        .or(input.asset_pair_hint)
        .ok_or_else(|| "missing_binance_symbol".to_string())?
        .to_string();

    let price = parse_f64(payload.get("c"))
        .or_else(|| parse_f64(payload.get("price")))
        .ok_or_else(|| "missing_binance_price".to_string())?;

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| super::parse_ts_from_path(payload, Some("$.E"), "ms"));
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
