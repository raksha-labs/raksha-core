use serde_json::{json, Value};

use super::{decode_hex_words, evm_log, parse_u256_word_to_f64, ParsedFeedEvent, ParserInput};

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

fn scale_amount(raw: f64, decimals: i32) -> Option<f64> {
    if !raw.is_finite() {
        return None;
    }
    let divisor = 10f64.powi(decimals);
    if !divisor.is_finite() || divisor == 0.0 {
        return None;
    }
    Some(raw / divisor)
}

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut parsed = evm_log::parse(input, payload)?;

    let data_hex = payload
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing_uniswap_v2_swap_data".to_string())?;
    let words = decode_hex_words(data_hex);
    if words.len() < 4 {
        return Err("invalid_uniswap_v2_swap_data".to_string());
    }

    let amount0_in_raw =
        parse_u256_word_to_f64(&words[0]).ok_or_else(|| "invalid_amount0_in".to_string())?;
    let amount1_in_raw =
        parse_u256_word_to_f64(&words[1]).ok_or_else(|| "invalid_amount1_in".to_string())?;
    let amount0_out_raw =
        parse_u256_word_to_f64(&words[2]).ok_or_else(|| "invalid_amount0_out".to_string())?;
    let amount1_out_raw =
        parse_u256_word_to_f64(&words[3]).ok_or_else(|| "invalid_amount1_out".to_string())?;

    let token0_symbol =
        read_string(input.filter_config, "token0_symbol", "TOKEN0").to_ascii_uppercase();
    let token1_symbol =
        read_string(input.filter_config, "token1_symbol", "TOKEN1").to_ascii_uppercase();
    let base_symbol =
        read_string(input.filter_config, "base_symbol", &token0_symbol).to_ascii_uppercase();
    let token0_decimals = read_i32(input.filter_config, "token0_decimals", 18);
    let token1_decimals = read_i32(input.filter_config, "token1_decimals", 18);

    let amount0_in = scale_amount(amount0_in_raw, token0_decimals)
        .ok_or_else(|| "invalid_scaled_amount0_in".to_string())?;
    let amount1_in = scale_amount(amount1_in_raw, token1_decimals)
        .ok_or_else(|| "invalid_scaled_amount1_in".to_string())?;
    let amount0_out = scale_amount(amount0_out_raw, token0_decimals)
        .ok_or_else(|| "invalid_scaled_amount0_out".to_string())?;
    let amount1_out = scale_amount(amount1_out_raw, token1_decimals)
        .ok_or_else(|| "invalid_scaled_amount1_out".to_string())?;

    let token0_price_in_token1 = if amount0_in > 0.0 && amount1_out > 0.0 {
        Some(amount1_out / amount0_in)
    } else if amount1_in > 0.0 && amount0_out > 0.0 {
        Some(amount1_in / amount0_out)
    } else {
        let token0_total = amount0_in + amount0_out;
        let token1_total = amount1_in + amount1_out;
        if token0_total > 0.0 && token1_total > 0.0 {
            Some(token1_total / token0_total)
        } else {
            None
        }
    }
    .filter(|value| value.is_finite() && *value > 0.0)
    .ok_or_else(|| "unable_to_derive_uniswap_v2_price".to_string())?;

    let (raw_price, quote_asset) = if base_symbol == token0_symbol {
        (token0_price_in_token1, token1_symbol.clone())
    } else if base_symbol == token1_symbol {
        (1.0 / token0_price_in_token1, token0_symbol.clone())
    } else {
        (token0_price_in_token1, token1_symbol.clone())
    };

    parsed.event_type = input.event_type.to_string();
    parsed.price = Some(raw_price);
    parsed.market_key = input.market_key_hint.map(ToString::to_string).or_else(|| {
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
        "decoded_by": "uniswap_v2_swap_price_v1",
        "token0_symbol": token0_symbol,
        "token1_symbol": token1_symbol,
        "base_symbol": base_symbol,
        "quote_asset": quote_asset,
        "raw_price": raw_price,
        "amount0_in": amount0_in,
        "amount1_in": amount1_in,
        "amount0_out": amount0_out,
        "amount1_out": amount1_out,
        "token0_decimals": token0_decimals,
        "token1_decimals": token1_decimals
    });

    Ok(parsed)
}
