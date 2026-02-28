use serde_json::{json, Value};

use super::{
    decode_hex_words, evm_log, parse_i256_word_to_f64, parse_u256_word_to_f64, ParsedFeedEvent,
    ParserInput,
};

fn read_string(config: &Value, key: &str, default_value: &str) -> String {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| default_value.to_string())
}

fn read_i32(config: &Value, key: &str, default_value: i32) -> i32 {
    config
        .get(key)
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(default_value)
}

fn derive_price_from_sqrt(
    sqrt_price_x96: f64,
    token0_decimals: i32,
    token1_decimals: i32,
) -> Option<f64> {
    if !(sqrt_price_x96.is_finite() && sqrt_price_x96 > 0.0) {
        return None;
    }

    let ratio_x192 = sqrt_price_x96 * sqrt_price_x96;
    if !(ratio_x192.is_finite() && ratio_x192 > 0.0) {
        return None;
    }

    let q192 = 2f64.powi(192);
    if !(q192.is_finite() && q192 > 0.0) {
        return None;
    }

    let decimal_adjustment = 10f64.powi(token0_decimals - token1_decimals);
    let price = (ratio_x192 / q192) * decimal_adjustment;
    if price.is_finite() && price > 0.0 {
        Some(price)
    } else {
        None
    }
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut parsed = evm_log::parse(input, payload)?;

    let data_hex = payload
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing_uniswap_v3_swap_data".to_string())?;
    let words = decode_hex_words(data_hex);
    if words.len() < 3 {
        return Err("invalid_uniswap_v3_swap_data".to_string());
    }

    let token0_symbol = read_string(input.filter_config, "token0_symbol", "TOKEN0").to_ascii_uppercase();
    let token1_symbol = read_string(input.filter_config, "token1_symbol", "TOKEN1").to_ascii_uppercase();
    let base_symbol = read_string(input.filter_config, "base_symbol", &token0_symbol).to_ascii_uppercase();
    let token0_decimals = read_i32(input.filter_config, "token0_decimals", 18);
    let token1_decimals = read_i32(input.filter_config, "token1_decimals", 18);

    let amount0 = parse_i256_word_to_f64(&words[0]).unwrap_or_default();
    let amount1 = parse_i256_word_to_f64(&words[1]).unwrap_or_default();
    let sqrt_price_x96 = parse_u256_word_to_f64(&words[2]).unwrap_or_default();

    let token0_price_in_token1 = derive_price_from_sqrt(sqrt_price_x96, token0_decimals, token1_decimals)
        .or_else(|| {
            let abs0 = amount0.abs() / 10f64.powi(token0_decimals);
            let abs1 = amount1.abs() / 10f64.powi(token1_decimals);
            if abs0 > 0.0 && abs1 > 0.0 {
                Some(abs1 / abs0)
            } else {
                None
            }
        })
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| "unable_to_derive_uniswap_v3_price".to_string())?;

    let (raw_price, quote_asset) = if base_symbol == token0_symbol {
        (token0_price_in_token1, token1_symbol.clone())
    } else if base_symbol == token1_symbol {
        (1.0 / token0_price_in_token1, token0_symbol.clone())
    } else {
        (token0_price_in_token1, token1_symbol.clone())
    };

    parsed.event_type = input.event_type.to_string();
    parsed.price = Some(raw_price);
    parsed.market_key = input
        .market_key_hint
        .map(ToString::to_string)
        .or_else(|| {
            if matches!(quote_asset.as_str(), "USD" | "USDT" | "USDC") {
                Some(format!("{base_symbol}/USD"))
            } else {
                Some(format!("{base_symbol}/{quote_asset}"))
            }
        });
    parsed.asset_pair = parsed.asset_pair.or_else(|| {
        input
            .asset_pair_hint
            .map(ToString::to_string)
            .or_else(|| Some(format!("{token0_symbol}{token1_symbol}")))
    });

    parsed.normalized_fields = json!({
        "decoded_by": "uniswap_v3_swap_price_v1",
        "token0_symbol": token0_symbol,
        "token1_symbol": token1_symbol,
        "base_symbol": base_symbol,
        "quote_asset": quote_asset,
        "raw_price": raw_price,
        "amount0": amount0,
        "amount1": amount1,
        "sqrt_price_x96": sqrt_price_x96,
        "token0_decimals": token0_decimals,
        "token1_decimals": token1_decimals
    });

    Ok(parsed)
}
