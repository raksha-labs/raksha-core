use serde_json::{json, Value};

use super::{
    observed_at, parse_f64, parse_ts_from_path, source_event_id, split_symbol_pair, symbol_to_market_key,
    ParsedFeedEvent, ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut inst_id = payload
        .get("instId")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut price = parse_f64(payload.get("last").or_else(|| payload.get("lastPrice")));
    let mut ts = payload.get("ts").cloned();

    if (inst_id.is_none() || price.is_none())
        && payload.get("data").and_then(Value::as_array).is_some()
    {
        if let Some(items) = payload.get("data").and_then(Value::as_array) {
            for item in items {
                if inst_id.is_none() {
                    inst_id = item
                        .get("instId")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                }
                if price.is_none() {
                    price = parse_f64(item.get("last").or_else(|| item.get("lastPrice")));
                }
                if ts.is_none() {
                    ts = item.get("ts").cloned();
                }
                if inst_id.is_some() && price.is_some() {
                    break;
                }
            }
        }
    }

    let inst_id = inst_id
        .or_else(|| input.asset_pair_hint.map(ToString::to_string))
        .ok_or_else(|| "missing_okx_inst_id".to_string())?;
    let price = price.ok_or_else(|| "missing_okx_last".to_string())?;

    let payload_event_ts = parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
        .or_else(|| ts.as_ref().and_then(|value| super::parse_ts_value(value, "ms")));
    let observed_at = observed_at(payload_event_ts);

    let market_key = input
        .market_key_hint
        .map(ToString::to_string)
        .or_else(|| symbol_to_market_key(&inst_id));
    let quote_asset = split_symbol_pair(&inst_id).map(|(_, quote)| quote);

    Ok(ParsedFeedEvent {
        event_type: input.event_type.to_string(),
        event_id: source_event_id(payload),
        market_key,
        asset_pair: Some(inst_id.clone()),
        price: Some(price),
        chain_id: None,
        block_number: None,
        tx_hash: None,
        log_index: None,
        topic0: None,
        payload_event_ts,
        observed_at,
        normalized_fields: json!({
            "inst_id": inst_id,
            "raw_price": price,
            "quote_asset": quote_asset
        }),
    })
}
