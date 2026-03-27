use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use super::{observed_at, parse_f64, parse_i64, scale_price, ParsedFeedEvent, ParserInput};

fn parse_timestamp(value: Option<&Value>) -> Option<chrono::DateTime<Utc>> {
    parse_i64(value).and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let decimals = input
        .filter_config
        .get("price_decimals")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .or_else(|| {
            payload
                .get("price_decimals")
                .and_then(Value::as_i64)
                .and_then(|value| i32::try_from(value).ok())
        })
        .unwrap_or(8);

    let raw_benchmark_price = parse_f64(payload.get("benchmark_price"))
        .or_else(|| parse_f64(payload.get("price")))
        .ok_or_else(|| "chainlink_data_streams_v3 missing benchmark_price".to_string())?;
    let price = scale_price(raw_benchmark_price, decimals)
        .ok_or_else(|| "chainlink_data_streams_v3 failed to scale benchmark_price".to_string())?;
    let bid = parse_f64(payload.get("bid")).and_then(|raw| scale_price(raw, decimals));
    let ask = parse_f64(payload.get("ask")).and_then(|raw| scale_price(raw, decimals));
    let payload_event_ts = parse_timestamp(payload.get("observations_timestamp"));
    let valid_from_ts = parse_timestamp(payload.get("valid_from_timestamp"));
    let feed_id = payload
        .get("feed_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    Ok(ParsedFeedEvent {
        event_type: input.event_type.to_string(),
        event_id: feed_id
            .as_ref()
            .zip(parse_i64(payload.get("observations_timestamp")))
            .map(|(feed_id, ts)| format!("{feed_id}:{ts}"))
            .or(feed_id.clone()),
        market_key: payload
            .get("market_key")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| input.market_key_hint.map(ToOwned::to_owned)),
        asset_pair: payload
            .get("asset_pair")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| input.asset_pair_hint.map(ToOwned::to_owned)),
        price: Some(price),
        chain_id: None,
        block_number: None,
        tx_hash: None,
        log_index: None,
        topic0: None,
        payload_event_ts,
        observed_at: observed_at(payload_event_ts),
        normalized_fields: json!({
            "decoded_by": "chainlink_data_streams_v3",
            "feed_id": feed_id,
            "valid_from_timestamp": valid_from_ts.map(|ts| ts.to_rfc3339()),
            "observations_timestamp": payload_event_ts.map(|ts| ts.to_rfc3339()),
            "price_decimals": decimals,
            "benchmark_price_raw": raw_benchmark_price,
            "bid_price_usd": bid,
            "ask_price_usd": ask,
            "quote_asset": "USD",
        }),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_scales_benchmark_price() {
        let input = ParserInput {
            parser_name: "chainlink_data_streams_v3",
            event_type: "oracle_update",
            market_key_hint: Some("USDC/USD"),
            asset_pair_hint: Some("USDCUSD"),
            payload_ts_path: None,
            payload_ts_unit: "s",
            filter_config: &json!({"price_decimals": 8}),
        };
        let payload = json!({
            "feed_id": "0x0003deadbeef",
            "observations_timestamp": 1713984280,
            "valid_from_timestamp": 1713984278,
            "benchmark_price": "99985000",
            "bid": "99980000",
            "ask": "99990000"
        });

        let parsed = parse(&input, &payload).expect("payload should parse");

        assert_eq!(parsed.price, Some(0.99985));
        assert_eq!(parsed.market_key.as_deref(), Some("USDC/USD"));
        assert_eq!(parsed.asset_pair.as_deref(), Some("USDCUSD"));
        assert_eq!(
            parsed.event_id.as_deref(),
            Some("0x0003deadbeef:1713984280")
        );
    }
}
