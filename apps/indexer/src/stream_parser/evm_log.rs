use serde_json::json;
use serde_json::Value;

use super::{
    empty_normalized, observed_at, parse_i64, parse_ts_from_path, source_event_id, ParsedFeedEvent,
    ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let tx_hash = payload
        .get("transactionHash")
        .and_then(Value::as_str)
        .or_else(|| payload.get("tx_hash").and_then(Value::as_str))
        .map(ToString::to_string);
    let block_number = parse_i64(
        payload
            .get("blockNumber")
            .or_else(|| payload.get("block_number")),
    );
    let log_index = parse_i64(payload.get("logIndex").or_else(|| payload.get("log_index")));
    let chain_id = parse_i64(payload.get("chainId").or_else(|| payload.get("chain_id")));
    let topic0 = payload
        .get("topics")
        .and_then(Value::as_array)
        .and_then(|topics| topics.first())
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            payload
                .get("topic0")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| parse_ts_from_path(payload, Some("$.timestamp"), "s"));
    let observed_at = observed_at(payload_event_ts);

    let mut normalized = empty_normalized();
    if let Some(obj) = normalized.as_object_mut() {
        obj.insert(
            "address".to_string(),
            payload.get("address").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "topics".to_string(),
            payload
                .get("topics")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        );
        obj.insert(
            "data".to_string(),
            payload.get("data").cloned().unwrap_or(Value::Null),
        );
    }

    Ok(ParsedFeedEvent {
        event_type: input.event_type.to_string(),
        event_id: source_event_id(payload),
        market_key: input.market_key_hint.map(ToString::to_string),
        asset_pair: input.asset_pair_hint.map(ToString::to_string),
        price: None,
        chain_id,
        block_number,
        tx_hash,
        log_index,
        topic0,
        payload_event_ts,
        observed_at,
        normalized_fields: json!(normalized),
    })
}
