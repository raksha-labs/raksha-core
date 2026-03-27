use serde_json::json;
use serde_json::Value;

use super::{
    observed_at, parse_f64, parse_ts_from_path, source_event_id, split_symbol_pair,
    symbol_to_market_key, ParsedFeedEvent, ParserInput,
};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let symbol = payload
        .get("s")
        .or_else(|| payload.get("symbol"))
        .and_then(Value::as_str)
        .or(input.asset_pair_hint)
        .ok_or_else(|| "missing_binance_symbol".to_string())?
        .to_string();

    let price = parse_f64(payload.get("c"))
        .or_else(|| parse_f64(payload.get("lastPrice")))
        .or_else(|| parse_f64(payload.get("price")))
        .ok_or_else(|| "missing_binance_price".to_string())?;

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| super::parse_ts_from_path(payload, Some("$.E"), "ms"));
    let payload_event_ts =
        payload_event_ts.or_else(|| super::parse_ts_from_path(payload, Some("$.closeTime"), "ms"));
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_supports_rest_24hr_ticker_payload() {
        let input = ParserInput {
            parser_name: "binance_miniticker_v1",
            event_type: "quote",
            market_key_hint: Some("USDC/USD"),
            asset_pair_hint: Some("USDCUSDT"),
            payload_ts_path: None,
            payload_ts_unit: "ms",
            filter_config: &json!({}),
        };
        let payload = json!({
            "symbol": "USDCUSDT",
            "lastPrice": "1.00040000",
            "closeTime": 1774600314427_i64,
        });

        let parsed = parse(&input, &payload).expect("parsed binance rest ticker");

        assert_eq!(parsed.asset_pair.as_deref(), Some("USDCUSDT"));
        assert_eq!(parsed.price, Some(1.0004));
        assert_eq!(
            parsed
                .payload_event_ts
                .expect("payload event ts")
                .timestamp_millis(),
            1774600314427
        );
    }
}
