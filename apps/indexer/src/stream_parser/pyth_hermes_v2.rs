/// Parser for the Pyth Network Hermes HTTP/SSE price-feed API (v2).
///
/// Hermes delivers individual price updates as JSON objects with the following shape:
///
/// ```json
/// {
///   "id": "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
///   "price": {
///     "price": "99985000",
///     "conf":  "68000",
///     "expo":  -8,
///     "publish_time": 1713984280
///   },
///   "ema_price": {
///     "price": "99976953",
///     "conf":  "47256",
///     "expo":  -8,
///     "publish_time": 1713984280
///   },
///   "metadata": {
///     "symbol": "Crypto.USDC/USD",
///     "asset_type": "Crypto"
///   }
/// }
/// ```
///
/// The actual price value is `price_mantissa × 10^expo` (expo is typically negative).
///
/// By default this parser uses **EMA price** (`ema_price`) which is more robust to
/// flash spikes — set `use_spot_price = true` in `filter_config` to prefer spot price.
///
/// The `filter_config` object may supply `market_key` to override the derived key,
/// and `asset_pair` to supply an explicit trading pair label.
///
/// ## price_feed wrapper
///
/// Some endpoints wrap the above in a `price_feed` envelope:
/// `{ "price_feed": { "id": ..., "price": {...}, "ema_price": {...} } }`.
/// Both shapes are handled transparently.
use serde_json::{json, Value};
use chrono::TimeZone;

use super::{observed_at, ParsedFeedEvent, ParserInput};

/// Decode a Pyth fixed-point numeric object into a `f64`.
///
/// The object takes the form `{"price": "<mantissa>", "expo": <exp>, ...}`.
/// Returns `None` if either field is absent or unparseable.
fn decode_pyth_price(obj: &Value) -> Option<f64> {
    let mantissa_str = obj.get("price").and_then(Value::as_str)?;
    let mantissa: f64 = mantissa_str.parse().ok()?;
    let expo: i32 = obj.get("expo").and_then(Value::as_i64).and_then(|e| i32::try_from(e).ok())?;
    let result = mantissa * 10f64.powi(expo);
    if result.is_finite() && result > 0.0 {
        Some(result)
    } else {
        None
    }
}

/// Extract the publish_time from a Pyth price object.
fn decode_pyth_ts(obj: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let secs = obj
        .get("publish_time")
        .and_then(Value::as_i64)?;
    chrono::Utc.timestamp_opt(secs, 0).single()
}

/// Derive a normalised `market_key` from the Pyth symbol string.
///
/// Pyth symbols follow the pattern `"Asset.BASE/QUOTE"` (e.g. `"Crypto.USDC/USD"`).
/// This function strips the asset-class prefix and returns `"BASE/USD"` when the
/// quote is a USD-stable variant.
fn market_key_from_pyth_symbol(raw_symbol: &str) -> String {
    let symbol = raw_symbol
        .split('.')
        .next_back()
        .unwrap_or(raw_symbol)
        .to_ascii_uppercase();

    // Normalise USD-stable quote assets into plain "USD".
    if let Some(idx) = symbol.find('/') {
        let base = &symbol[..idx];
        let quote = &symbol[idx + 1..];
        let normalised_quote = if matches!(quote, "USD" | "USDT" | "USDC") {
            "USD"
        } else {
            quote
        };
        return format!("{base}/{normalised_quote}");
    }

    symbol
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    // Unwrap optional `price_feed` envelope.
    let feed = payload
        .get("price_feed")
        .filter(|v| v.is_object())
        .unwrap_or(payload);

    // Decide whether to use EMA price (default) or spot price.
    let use_spot = input
        .filter_config
        .get("use_spot_price")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let price_obj = if use_spot {
        feed.get("price")
            .or_else(|| feed.get("ema_price"))
    } else {
        feed.get("ema_price")
            .or_else(|| feed.get("price"))
    }
    .ok_or_else(|| "missing_pyth_price_object".to_string())?;

    let price = decode_pyth_price(price_obj)
        .ok_or_else(|| "invalid_pyth_price_mantissa_or_expo".to_string())?;

    // Determine confidence interval (used for diagnostics, not price).
    let conf_str = price_obj.get("conf").and_then(Value::as_str);
    let expo: i32 = price_obj
        .get("expo")
        .and_then(Value::as_i64)
        .and_then(|e| i32::try_from(e).ok())
        .unwrap_or(-8);
    let conf_usd = conf_str
        .and_then(|s| s.parse::<f64>().ok())
        .map(|c| c * 10f64.powi(expo))
        .filter(|c| c.is_finite());

    // Derive publish timestamp.
    let payload_event_ts = decode_pyth_ts(price_obj)
        .or_else(|| decode_pyth_ts(feed));
    let observed_at = observed_at(payload_event_ts);

    // Symbol / market key.
    let raw_symbol = feed
        .get("metadata")
        .and_then(|m| m.get("symbol"))
        .and_then(Value::as_str)
        .or_else(|| feed.get("symbol").and_then(Value::as_str))
        .map(ToString::to_string);

    let feed_id = feed
        .get("id")
        .and_then(Value::as_str)
        .map(ToString::to_string);

    let market_key = input
        .market_key_hint
        .map(ToString::to_string)
        .or_else(|| raw_symbol.as_deref().map(market_key_from_pyth_symbol));

    let asset_pair = input
        .asset_pair_hint
        .map(ToString::to_string)
        .or_else(|| {
            raw_symbol.as_deref().map(|s| {
                s.split('.').next_back().unwrap_or(s).to_ascii_uppercase()
            })
        });

    // Spot price for cross-check (always include in normalized_fields).
    let spot_price = feed
        .get("price")
        .and_then(decode_pyth_price);

    let ema_price = feed
        .get("ema_price")
        .and_then(decode_pyth_price);

    Ok(ParsedFeedEvent {
        event_type: input.event_type.to_string(),
        event_id: feed_id.clone(),
        market_key,
        asset_pair,
        price: Some(price),
        chain_id: None,
        block_number: None,
        tx_hash: None,
        log_index: None,
        topic0: None,
        payload_event_ts,
        observed_at,
        normalized_fields: json!({
            "decoded_by": "pyth_hermes_v2",
            "feed_id": feed_id,
            "raw_symbol": raw_symbol,
            "price_type": if use_spot { "spot" } else { "ema" },
            "ema_price_usd": ema_price,
            "spot_price_usd": spot_price,
            "conf_usd": conf_usd,
            "expo": expo,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_input<'a>(filter_config: &'a Value) -> ParserInput<'a> {
        ParserInput {
            parser_name: "pyth_hermes_v2",
            event_type: "quote",
            market_key_hint: None,
            asset_pair_hint: None,
            payload_ts_path: None,
            payload_ts_unit: "s",
            filter_config,
        }
    }

    fn hermes_payload() -> Value {
        json!({
            "id": "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
            "price": {
                "price": "99990000",
                "conf": "50000",
                "expo": -8,
                "publish_time": 1713984280_i64
            },
            "ema_price": {
                "price": "99985000",
                "conf": "48000",
                "expo": -8,
                "publish_time": 1713984280_i64
            },
            "metadata": {
                "symbol": "Crypto.USDC/USD",
                "asset_type": "Crypto"
            }
        })
    }

    #[test]
    fn ema_price_is_default() {
        let filter = Value::Null;
        let result = parse(&make_input(&filter), &hermes_payload()).expect("should parse");
        // EMA price: 99985000 × 10^-8 = 0.9998500
        let expected = 99985000.0 * 10f64.powi(-8);
        assert!((result.price.unwrap() - expected).abs() < 1e-10);
        assert_eq!(result.market_key.as_deref(), Some("USDC/USD"));
    }

    #[test]
    fn use_spot_price_override() {
        let filter = json!({"use_spot_price": true});
        let result = parse(&make_input(&filter), &hermes_payload()).expect("should parse");
        // Spot price: 99990000 × 10^-8 = 0.9999000
        let expected = 99990000.0 * 10f64.powi(-8);
        assert!((result.price.unwrap() - expected).abs() < 1e-10);
    }

    #[test]
    fn price_feed_envelope_unwrapped() {
        let filter = Value::Null;
        let wrapped = json!({ "price_feed": hermes_payload() });
        let result = parse(&make_input(&filter), &wrapped).expect("should parse with envelope");
        let expected = 99985000.0 * 10f64.powi(-8);
        assert!((result.price.unwrap() - expected).abs() < 1e-10);
    }

    #[test]
    fn market_key_normalised_from_symbol() {
        assert_eq!(market_key_from_pyth_symbol("Crypto.USDT/USD"), "USDT/USD");
        assert_eq!(market_key_from_pyth_symbol("Crypto.DAI/USD"), "DAI/USD");
        assert_eq!(market_key_from_pyth_symbol("FX.USDC/USDT"), "USDC/USD");
        assert_eq!(market_key_from_pyth_symbol("USDC/USD"), "USDC/USD");
    }

    #[test]
    fn missing_price_object_returns_error() {
        let filter = Value::Null;
        let payload = json!({"id": "0xabc"});
        assert!(parse(&make_input(&filter), &payload).is_err());
    }
}
