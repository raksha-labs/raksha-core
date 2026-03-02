use serde_json::{json, Value};

use super::{evm_log, ParsedFeedEvent, ParserInput};

pub(super) fn parse(input: &ParserInput<'_>, payload: &Value) -> Result<ParsedFeedEvent, String> {
    let mut parsed = evm_log::parse(input, payload)?;
    let mut normalized = parsed
        .normalized_fields
        .as_object()
        .cloned()
        .unwrap_or_default();
    normalized.insert(
        "swap_amounts".to_string(),
        parse_swap_amounts(payload.get("data").and_then(Value::as_str)),
    );
    normalized.insert(
        "decoded_by".to_string(),
        Value::String("uniswap_v2_swap_v1".to_string()),
    );
    parsed.normalized_fields = Value::Object(normalized);
    Ok(parsed)
}

fn parse_swap_amounts(data_hex: Option<&str>) -> Value {
    let Some(data_hex) = data_hex else {
        return json!([]);
    };
    let Some(body) = data_hex.strip_prefix("0x") else {
        return json!([]);
    };
    if body.len() < 64 {
        return json!([]);
    }

    let mut values: Vec<String> = Vec::new();
    for chunk in body.as_bytes().chunks(64).take(4) {
        let Ok(text) = std::str::from_utf8(chunk) else {
            continue;
        };
        match u128::from_str_radix(text, 16) {
            Ok(amount) => values.push(amount.to_string()),
            Err(_) => values.push(format!("0x{text}")),
        }
    }
    json!(values)
}
