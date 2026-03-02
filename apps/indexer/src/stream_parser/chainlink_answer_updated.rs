use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use super::{
    decode_hex_words, evm_log, observed_at, parse_i256_word_to_f64, parse_i64,
    parse_u256_word_to_f64, scale_price, ParsedFeedEvent, ParserInput,
};

fn as_i32(value: Option<&Value>) -> Option<i32> {
    value
        .and_then(Value::as_i64)
        .and_then(|raw| i32::try_from(raw).ok())
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut parsed = evm_log::parse(input, payload)?;

    let topics = payload
        .get("topics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let data_words = payload
        .get("data")
        .and_then(Value::as_str)
        .map(decode_hex_words)
        .unwrap_or_default();

    let raw_answer = topics
        .get(1)
        .and_then(Value::as_str)
        .and_then(parse_i256_word_to_f64)
        .or_else(|| {
            data_words
                .first()
                .and_then(|word| parse_i256_word_to_f64(word))
        });
    let round_id = topics
        .get(2)
        .and_then(Value::as_str)
        .and_then(parse_u256_word_to_f64)
        .or_else(|| {
            data_words
                .get(1)
                .and_then(|word| parse_u256_word_to_f64(word))
        });

    let updated_at_seconds = parse_i64(payload.get("updatedAt")).or_else(|| {
        data_words
            .last()
            .and_then(|word| parse_u256_word_to_f64(word))
            .and_then(|value| i64::try_from(value as i128).ok())
    });
    let updated_at = updated_at_seconds.and_then(|seconds| Utc.timestamp_opt(seconds, 0).single());

    let decimals = as_i32(input.filter_config.get("decimals"))
        .or_else(|| as_i32(input.filter_config.get("price_decimals")))
        .unwrap_or(8);
    let price = raw_answer.and_then(|raw| scale_price(raw, decimals));

    parsed.event_type = input.event_type.to_string();
    parsed.market_key = input.market_key_hint.map(ToString::to_string).or_else(|| {
        input
            .filter_config
            .get("market_key")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    });
    parsed.asset_pair = parsed.asset_pair.or_else(|| {
        input.asset_pair_hint.map(ToString::to_string).or_else(|| {
            input
                .filter_config
                .get("asset_pair")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
    });
    parsed.price = price;
    parsed.payload_event_ts = parsed.payload_event_ts.or(updated_at);
    parsed.observed_at = observed_at(parsed.payload_event_ts);

    parsed.normalized_fields = json!({
        "decoded_by": "chainlink_answer_updated_v1",
        "raw_answer": raw_answer,
        "price_decimals": decimals,
        "round_id": round_id,
        "updated_at": updated_at.map(|ts| ts.to_rfc3339()),
        "quote_asset": "USD"
    });

    Ok(parsed)
}
