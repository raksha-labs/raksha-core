/// Parser for Gate.io Spot Tickers WebSocket v4 API.
///
/// Gate.io sends ticker updates over the `spot.tickers` channel with the
/// following payload shape:
///
/// ```json
/// {
///   "time": 1611130490,
///   "time_ms": 1611130490123,
///   "channel": "spot.tickers",
///   "event": "update",
///   "result": {
///     "currency_pair": "USDC_USDT",
///     "last": "1.001",
///     "lowest_ask": "1.001",
///     "highest_bid": "1.0005",
///     "change_percentage": "0.01",
///     "base_volume": "12345.6",
///     "quote_volume": "12347.2",
///     "high_24h": "1.005",
///     "low_24h": "0.995"
///   }
/// }
/// ```
///
/// Gate.io uses an underscore `_` to separate the base and quote assets in
/// `currency_pair` (e.g. `USDC_USDT`), unlike most exchanges that use `/`.
use serde_json::{json, Value};

use super::{
    observed_at, parse_f64, parse_ts_from_path, source_event_id, symbol_to_market_key,
    ParsedFeedEvent, ParserInput,
};

/// Normalise a Gate.io `currency_pair` string like `USDC_USDT` into the
/// canonical `USDC/USDT` form so that downstream symbol helpers work correctly.
fn normalise_gate_pair(pair: &str) -> String {
    pair.replace('_', "/").to_ascii_uppercase()
}

/// Split `BASE_QUOTE` or `BASE/QUOTE` into `(base, quote)`.
fn split_gate_pair(pair: &str) -> Option<(String, String)> {
    let normalised = normalise_gate_pair(pair);
    let mut parts = normalised.splitn(2, '/').filter(|p| !p.is_empty());
    let base = parts.next()?.to_string();
    let quote = parts.next()?.to_string();
    Some((base, quote))
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    // The ticker data can live directly in the payload or nested under "result".
    let ticker = if let Some(result) = payload.get("result") {
        if result.is_object() {
            result
        } else if let Some(first) = result.as_array().and_then(|a| a.first()) {
            first
        } else {
            payload
        }
    } else {
        payload
    };

    let raw_pair = ticker
        .get("currency_pair")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            // Some subscription messages echo the pair in the outer payload or
            // in a "s" shorthand field used by some exchange SDKs.
            payload
                .get("s")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| input.asset_pair_hint.map(ToString::to_string));

    let price = parse_f64(ticker.get("last"))
        .or_else(|| parse_f64(ticker.get("close")))
        .or_else(|| parse_f64(ticker.get("lastPrice")));

    let raw_pair = raw_pair.ok_or_else(|| "missing_gate_currency_pair".to_string())?;
    let price = price.ok_or_else(|| "missing_gate_last_price".to_string())?;

    // Normalise to "BASE/QUOTE" form.
    let symbol = normalise_gate_pair(&raw_pair);
    let (base, quote) =
        split_gate_pair(&raw_pair).ok_or_else(|| format!("unparseable_gate_pair:{raw_pair}"))?;

    let payload_event_ts =
        parse_ts_from_path(payload, input.payload_ts_path, input.payload_ts_unit)
            .or_else(|| super::parse_ts_from_path(payload, Some("$.time_ms"), "ms"))
            .or_else(|| super::parse_ts_from_path(payload, Some("$.time"), "s"));
    let observed_at = observed_at(payload_event_ts);

    let market_key = input
        .market_key_hint
        .map(ToString::to_string)
        .or_else(|| symbol_to_market_key(&symbol));

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
            "decoded_by": "gate_ticker_v4",
            "symbol": symbol,
            "raw_pair": raw_pair,
            "raw_price": price,
            "base_asset": base,
            "quote_asset": quote,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_input(filter_config: &serde_json::Value) -> ParserInput<'_> {
        ParserInput {
            parser_name: "gate_ticker_v4",
            event_type: "quote",
            market_key_hint: None,
            asset_pair_hint: None,
            payload_ts_path: None,
            payload_ts_unit: "ms",
            filter_config,
        }
    }

    #[test]
    fn parse_standard_update() {
        let filter = serde_json::Value::Null;
        let payload = json!({
            "time": 1611130490_u64,
            "time_ms": 1611130490123_u64,
            "channel": "spot.tickers",
            "event": "update",
            "result": {
                "currency_pair": "USDC_USDT",
                "last": "1.001",
                "highest_bid": "1.0005",
                "lowest_ask": "1.001",
                "base_volume": "12345.6"
            }
        });
        let result = parse(&make_input(&filter), &payload).expect("should parse");
        assert_eq!(result.asset_pair.as_deref(), Some("USDC/USDT"));
        assert_eq!(result.market_key.as_deref(), Some("USDC/USD"));
        assert!((result.price.unwrap() - 1.001).abs() < 1e-9);
    }

    #[test]
    fn parse_flat_payload() {
        let filter = serde_json::Value::Null;
        let payload = json!({
            "currency_pair": "USDT_USD",
            "last": "0.9998"
        });
        let result = parse(&make_input(&filter), &payload).expect("should parse");
        assert_eq!(result.asset_pair.as_deref(), Some("USDT/USD"));
        assert!((result.price.unwrap() - 0.9998).abs() < 1e-9);
    }

    #[test]
    fn missing_price_returns_error() {
        let filter = serde_json::Value::Null;
        let payload = json!({"result": {"currency_pair": "USDC_USDT"}});
        assert!(parse(&make_input(&filter), &payload).is_err());
    }

    #[test]
    fn missing_pair_returns_error() {
        let filter = serde_json::Value::Null;
        let payload = json!({"result": {"last": "1.0"}});
        assert!(parse(&make_input(&filter), &payload).is_err());
    }
}
