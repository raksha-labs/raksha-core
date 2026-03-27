use chrono::{DateTime, TimeZone, Utc};
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

fn decoded_json(payload: &Value) -> Option<&serde_json::Map<String, Value>> {
    payload.get("decoded_json").and_then(Value::as_object)
}

fn parse_rfc3339_ts(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|ts| ts.with_timezone(&Utc))
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut parsed = evm_log::parse(input, payload)?;
    let decoded = decoded_json(payload);

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
        })
        .or_else(|| {
            decoded
                .and_then(|json| json.get("raw_answer"))
                .and_then(Value::as_f64)
        });
    let round_id = topics
        .get(2)
        .and_then(Value::as_str)
        .and_then(parse_u256_word_to_f64)
        .or_else(|| {
            data_words
                .get(1)
                .and_then(|word| parse_u256_word_to_f64(word))
        })
        .or_else(|| {
            decoded
                .and_then(|json| json.get("round_id"))
                .and_then(Value::as_f64)
        });

    let updated_at_seconds = parse_i64(payload.get("updatedAt")).or_else(|| {
        data_words
            .last()
            .and_then(|word| parse_u256_word_to_f64(word))
            .and_then(|value| i64::try_from(value as i128).ok())
    });
    let updated_at = updated_at_seconds
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
        .or_else(|| decoded.and_then(|json| parse_rfc3339_ts(json.get("updated_at"))));

    let decimals = as_i32(input.filter_config.get("decimals"))
        .or_else(|| as_i32(input.filter_config.get("price_decimals")))
        .or_else(|| decoded.and_then(|json| as_i32(json.get("price_decimals"))))
        .unwrap_or(8);
    let price = raw_answer
        .and_then(|raw| scale_price(raw, decimals))
        .or_else(|| {
            decoded
                .and_then(|json| json.get("oracle_price"))
                .and_then(Value::as_f64)
        })
        .or_else(|| {
            decoded
                .and_then(|json| json.get("normalized_price_usd"))
                .and_then(Value::as_f64)
        });

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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_uses_decoded_json_fallback_for_mock_replay_payloads() {
        let input = ParserInput {
            parser_name: "chainlink_answer_updated_v1",
            event_type: "oracle_update",
            market_key_hint: Some("USDC/USD"),
            asset_pair_hint: Some("USDCUSD"),
            payload_ts_path: None,
            payload_ts_unit: "iso8601",
            filter_config: &json!({"decimals": 8}),
        };
        let payload = json!({
            "blockNumber": 19000032,
            "logIndex": 32,
            "topics": ["0x0559884f"],
            "data": "0x53ec600",
            "decoded_json": {
                "round_id": 32,
                "market_key": "USDC/USD",
                "raw_answer": 88000000,
                "oracle_price": 0.88,
                "price_decimals": 8,
                "normalized_price_usd": 0.88,
                "updated_at": "2023-03-11T08:01:04Z"
            }
        });

        let parsed = parse(&input, &payload).expect("mock replay payload should parse");

        assert_eq!(parsed.market_key.as_deref(), Some("USDC/USD"));
        assert_eq!(parsed.asset_pair.as_deref(), Some("USDCUSD"));
        assert_eq!(parsed.price, Some(0.88));
        assert_eq!(
            parsed.payload_event_ts,
            Some(
                DateTime::parse_from_rfc3339("2023-03-11T08:01:04Z")
                    .expect("valid timestamp")
                    .with_timezone(&Utc)
            )
        );
        assert_eq!(parsed.normalized_fields["raw_answer"], json!(88000000.0));
        assert_eq!(parsed.normalized_fields["round_id"], json!(32.0));
    }
}
