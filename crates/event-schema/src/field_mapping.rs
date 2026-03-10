// ─────────────────────────────────────────────────────────────────────────────
// Field mapping — canonical internal message schema
//
// Defines the mapping from raw source payload fields to canonical internal
// fields used by detection patterns. Mirrors the TypeScript types in
// raksha-simlab/web/frontend/components/pattern-creator/types.ts.
//
// Field mappings are stored in source_stream_configs.filter_config["field_mappings"]
// as a JSON array and consumed by stream parsers when building ParsedFeedEvent.
// ─────────────────────────────────────────────────────────────────────────────

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Canonical field names ─────────────────────────────────────────────────────

/// All canonical fields available in the internal message schema.
/// Common fields are used by any pattern; pattern-specific fields are noted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalField {
    // ── Common ────────────────────────────────────────────────────────────────
    /// Normalized market key, e.g. "USDC/USD". Primary DPEG key.
    MarketKey,
    /// Normalized asset price as f64.
    Price,
    /// Event timestamp in ISO 8601 (UTC).
    Timestamp,
    /// Auto-filled by indexer: source identifier.
    SourceId,
    /// Auto-filled by parser: event type string.
    EventType,
    /// EVM chain ID (e.g. 1 = Ethereum mainnet).
    ChainId,
    /// EVM block number.
    BlockNumber,
    /// EVM transaction hash (hex string).
    TxHash,
    // ── TVL Drop ──────────────────────────────────────────────────────────────
    /// Protocol identifier, e.g. "aave_v3", "morpho_blue".
    ProtocolId,
    /// Chain slug, e.g. "ethereum", "base", "arbitrum".
    ChainSlug,
    /// Market within protocol, e.g. "usdc", "weth".
    MarketId,
    /// Total value locked in USD (f64).
    TvlUsd,
    // ── Flash Loan ────────────────────────────────────────────────────────────
    /// Flash loan amount in USD.
    LoanAmountUsd,
    /// Extracted profit in USD.
    ProfitUsd,
    /// Token contract address.
    TokenAddress,
}

// ── Transform operations ──────────────────────────────────────────────────────

/// How to convert the raw source field value to the canonical field type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldTransform {
    /// Direct string copy, no conversion.
    Identity,
    /// Parse string numeric value to f64.
    ToF64,
    /// Parse string numeric value to u64 (block numbers, etc.).
    ToU64,
    /// Convert exchange symbol to market key: "USDCUSDT" → "USDC/USD".
    SymbolToMarketKey,
    /// Scale raw integer by 10^(-decimals): Chainlink 8-decimal format.
    ScalePrice,
    /// Parse epoch milliseconds to ISO 8601.
    ParseTsMs,
    /// Parse epoch seconds to ISO 8601.
    ParseTsS,
    /// Pass through / normalize ISO 8601 string.
    ParseTsIso8601,
    /// Convert Uniswap V3 sqrtPriceX96 to price f64.
    SqrtRatioToPrice,
}

// ── Field mapping ─────────────────────────────────────────────────────────────

/// Maps a source payload field (by JSONPath) to a canonical internal field.
///
/// Example for `binance_miniticker_v1`:
/// ```json
/// [
///   { "source_field": "$.c",  "canonical_field": "price",      "transform": "to_f64" },
///   { "source_field": "$.s",  "canonical_field": "market_key", "transform": "symbol_to_market_key" },
///   { "source_field": "$.E",  "canonical_field": "timestamp",  "transform": "parse_ts_ms" }
/// ]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldMapping {
    /// JSONPath expression into the raw source payload, e.g. `"$.c"`, `"$.data[0].last"`.
    pub source_field: String,
    /// Target canonical field in the internal message schema.
    pub canonical_field: CanonicalField,
    /// Transform to apply when extracting the value.
    pub transform: FieldTransform,
    /// Optional human-readable description for tooling/UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ── Per-parser default mappings ───────────────────────────────────────────────

/// Returns the canonical default field mappings for a given parser name.
/// These mirror the hardcoded extraction logic in each parser module.
/// Stream configs may override these via filter_config["field_mappings"].
pub fn default_mappings_for_parser(parser_name: &str) -> Vec<FieldMapping> {
    match parser_name {
        "binance_miniticker_v1" => vec![
            FieldMapping {
                source_field: "$.c".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ToF64,
                description: Some("Close price (string float)".into()),
            },
            FieldMapping {
                source_field: "$.s".into(),
                canonical_field: CanonicalField::MarketKey,
                transform: FieldTransform::SymbolToMarketKey,
                description: Some("Symbol → market key".into()),
            },
            FieldMapping {
                source_field: "$.E".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsMs,
                description: Some("Event time (epoch ms)".into()),
            },
        ],
        "coinbase_ticker_v1" => vec![
            FieldMapping {
                source_field: "$.price".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ToF64,
                description: Some("Last trade price".into()),
            },
            FieldMapping {
                source_field: "$.product_id".into(),
                canonical_field: CanonicalField::MarketKey,
                transform: FieldTransform::SymbolToMarketKey,
                description: Some("Product ID → market key".into()),
            },
            FieldMapping {
                source_field: "$.time".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsIso8601,
                description: Some("Event time (ISO 8601)".into()),
            },
        ],
        "kraken_ticker_v2" => vec![
            FieldMapping {
                source_field: "$.data[0].last".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ToF64,
                description: Some("Last trade price".into()),
            },
            FieldMapping {
                source_field: "$.data[0].symbol".into(),
                canonical_field: CanonicalField::MarketKey,
                transform: FieldTransform::SymbolToMarketKey,
                description: Some("Symbol → market key".into()),
            },
        ],
        "okx_tickers_v5" => vec![
            FieldMapping {
                source_field: "$.data[0].last".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ToF64,
                description: Some("Last price (string float)".into()),
            },
            FieldMapping {
                source_field: "$.data[0].instId".into(),
                canonical_field: CanonicalField::MarketKey,
                transform: FieldTransform::SymbolToMarketKey,
                description: Some("Instrument ID → market key".into()),
            },
            FieldMapping {
                source_field: "$.data[0].ts".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsMs,
                description: Some("Event time (epoch ms)".into()),
            },
        ],
        "bybit_tickers_v5" => vec![
            FieldMapping {
                source_field: "$.data.lastPrice".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ToF64,
                description: Some("Last price".into()),
            },
            FieldMapping {
                source_field: "$.data.symbol".into(),
                canonical_field: CanonicalField::MarketKey,
                transform: FieldTransform::SymbolToMarketKey,
                description: Some("Symbol → market key".into()),
            },
            FieldMapping {
                source_field: "$.ts".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsMs,
                description: Some("Event time (epoch ms)".into()),
            },
        ],
        "gemini_marketdata_v1" => vec![
            FieldMapping {
                source_field: "$.price".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ToF64,
                description: Some("Trade price".into()),
            },
            FieldMapping {
                source_field: "$.timestampms".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsMs,
                description: Some("Event time (epoch ms)".into()),
            },
        ],
        "chainlink_answer_updated_v1" => vec![
            FieldMapping {
                source_field: "$.answer".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ScalePrice,
                description: Some("Oracle answer ÷ 10^8 → price".into()),
            },
            FieldMapping {
                source_field: "$.updatedAt".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsS,
                description: Some("Update time (epoch sec)".into()),
            },
        ],
        "uniswap_v2_swap_price_v1" => vec![
            FieldMapping {
                source_field: "$.amount0In".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::ToF64,
                description: Some("Derived swap price from amounts + decimals".into()),
            },
            FieldMapping {
                source_field: "$.block.timestamp".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsS,
                description: Some("Block timestamp (epoch sec)".into()),
            },
            FieldMapping {
                source_field: "$.transactionHash".into(),
                canonical_field: CanonicalField::TxHash,
                transform: FieldTransform::Identity,
                description: Some("Transaction hash".into()),
            },
            FieldMapping {
                source_field: "$.blockNumber".into(),
                canonical_field: CanonicalField::BlockNumber,
                transform: FieldTransform::ToU64,
                description: Some("Block number".into()),
            },
        ],
        "uniswap_v3_swap_price_v1" => vec![
            FieldMapping {
                source_field: "$.sqrtPriceX96".into(),
                canonical_field: CanonicalField::Price,
                transform: FieldTransform::SqrtRatioToPrice,
                description: Some("sqrtPriceX96 → price".into()),
            },
            FieldMapping {
                source_field: "$.block.timestamp".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsS,
                description: Some("Block timestamp (epoch sec)".into()),
            },
            FieldMapping {
                source_field: "$.transactionHash".into(),
                canonical_field: CanonicalField::TxHash,
                transform: FieldTransform::Identity,
                description: Some("Transaction hash".into()),
            },
            FieldMapping {
                source_field: "$.blockNumber".into(),
                canonical_field: CanonicalField::BlockNumber,
                transform: FieldTransform::ToU64,
                description: Some("Block number".into()),
            },
        ],
        "protocol_tvl_v1" => vec![
            FieldMapping {
                source_field: "$.tvl_usd".into(),
                canonical_field: CanonicalField::TvlUsd,
                transform: FieldTransform::ToF64,
                description: Some("Protocol TVL in USD".into()),
            },
            FieldMapping {
                source_field: "$.protocol_id".into(),
                canonical_field: CanonicalField::ProtocolId,
                transform: FieldTransform::Identity,
                description: Some("Protocol identifier".into()),
            },
            FieldMapping {
                source_field: "$.chain_slug".into(),
                canonical_field: CanonicalField::ChainSlug,
                transform: FieldTransform::Identity,
                description: Some("Chain slug".into()),
            },
            FieldMapping {
                source_field: "$.market_id".into(),
                canonical_field: CanonicalField::MarketId,
                transform: FieldTransform::Identity,
                description: Some("Market identifier".into()),
            },
            FieldMapping {
                source_field: "$.block_number".into(),
                canonical_field: CanonicalField::BlockNumber,
                transform: FieldTransform::ToU64,
                description: Some("Block number".into()),
            },
        ],
        "protocol_pause_v1" => vec![
            FieldMapping {
                source_field: "$.protocol_id".into(),
                canonical_field: CanonicalField::ProtocolId,
                transform: FieldTransform::Identity,
                description: Some("Protocol identifier".into()),
            },
            FieldMapping {
                source_field: "$.chain_slug".into(),
                canonical_field: CanonicalField::ChainSlug,
                transform: FieldTransform::Identity,
                description: Some("Chain slug".into()),
            },
        ],
        "evm_block_v1" => vec![
            FieldMapping {
                source_field: "$.timestamp".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsS,
                description: Some("Block timestamp (epoch sec)".into()),
            },
            FieldMapping {
                source_field: "$.number".into(),
                canonical_field: CanonicalField::BlockNumber,
                transform: FieldTransform::ToU64,
                description: Some("Block number".into()),
            },
        ],
        "evm_log_v1" => vec![
            FieldMapping {
                source_field: "$.blockNumber".into(),
                canonical_field: CanonicalField::BlockNumber,
                transform: FieldTransform::ToU64,
                description: Some("Block number".into()),
            },
            FieldMapping {
                source_field: "$.transactionHash".into(),
                canonical_field: CanonicalField::TxHash,
                transform: FieldTransform::Identity,
                description: Some("Transaction hash".into()),
            },
            FieldMapping {
                source_field: "$.block.timestamp".into(),
                canonical_field: CanonicalField::Timestamp,
                transform: FieldTransform::ParseTsS,
                description: Some("Block timestamp".into()),
            },
        ],
        _ => vec![],
    }
}

// ── Config extraction helper ──────────────────────────────────────────────────

/// Extract field_mappings from a stream's filter_config JSON.
/// Returns the override mappings if present, otherwise falls back to parser defaults.
pub fn resolve_field_mappings(
    parser_name: &str,
    filter_config: Option<&Value>,
) -> Vec<FieldMapping> {
    if let Some(cfg) = filter_config {
        if let Some(arr) = cfg.get("field_mappings") {
            if let Ok(mappings) = serde_json::from_value::<Vec<FieldMapping>>(arr.clone()) {
                if !mappings.is_empty() {
                    return mappings;
                }
            }
        }
    }
    default_mappings_for_parser(parser_name)
}

// ── Apply mappings to a payload ───────────────────────────────────────────────

/// Extracted canonical values from a raw payload, ready for ParsedFeedEvent construction.
#[derive(Debug, Default)]
pub struct MappedFields {
    pub market_key: Option<String>,
    pub price: Option<f64>,
    pub timestamp_raw: Option<Value>,
    pub chain_id: Option<u64>,
    pub block_number: Option<u64>,
    pub tx_hash: Option<String>,
    pub protocol_id: Option<String>,
    pub chain_slug: Option<String>,
    pub market_id: Option<String>,
    pub tvl_usd: Option<f64>,
    pub loan_amount_usd: Option<f64>,
    pub profit_usd: Option<f64>,
    pub token_address: Option<String>,
    /// Any extra extracted fields stored as key→value map.
    pub extras: std::collections::HashMap<String, Value>,
}

/// Simple JSONPath evaluation (supports `$.field` and `$.field[0].nested` patterns).
/// Returns `None` if the path does not resolve.
fn eval_jsonpath<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    if path == "$" {
        return Some(payload);
    }
    let path = path.strip_prefix("$.")?;
    let mut current = payload;
    for segment in path.split('.') {
        // Handle array index: field[0]
        if let Some((key, rest)) = segment.split_once('[') {
            current = current.get(key)?;
            let idx_str = rest.trim_end_matches(']');
            let idx: usize = idx_str.parse().ok()?;
            current = current.get(idx)?;
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current)
}

/// Convert a symbol string to a market key using the same logic as `symbol_to_market_key` in parsers.
fn symbol_to_market_key(symbol: &str) -> Option<String> {
    let s = symbol.to_uppercase().replace(['-', '/'], "");
    let known: &[(&str, &str)] = &[
        ("USDCUSDT", "USDC/USD"),
        ("USDCUSD", "USDC/USD"),
        ("USDTUSDC", "USDT/USD"),
        ("USDTUSD", "USDT/USD"),
        ("DAIUSDT", "DAI/USD"),
        ("DAIUSD", "DAI/USD"),
    ];
    known
        .iter()
        .find(|(sym, _)| *sym == s.as_str())
        .map(|(_, mk)| mk.to_string())
        .or_else(|| {
            // Fallback: if already in USDC/USD form
            if symbol.contains('/') {
                Some(symbol.to_string())
            } else {
                None
            }
        })
}

/// Apply field mappings to a raw payload and populate `MappedFields`.
pub fn apply_mappings(
    mappings: &[FieldMapping],
    payload: &Value,
    filter_config: Option<&Value>,
) -> MappedFields {
    let decimals = filter_config
        .and_then(|c| c.get("decimals"))
        .and_then(Value::as_u64)
        .unwrap_or(8) as i32;

    let mut out = MappedFields::default();

    for mapping in mappings {
        let raw = match eval_jsonpath(payload, &mapping.source_field) {
            Some(v) => v,
            None => continue,
        };

        match &mapping.canonical_field {
            CanonicalField::Price => {
                let price = match &mapping.transform {
                    FieldTransform::ToF64 => raw
                        .as_f64()
                        .or_else(|| raw.as_str().and_then(|s| s.parse().ok())),
                    FieldTransform::ScalePrice => {
                        let raw_int_opt = raw
                            .as_i64()
                            .or_else(|| raw.as_str().and_then(|s| s.parse().ok()));
                        raw_int_opt.map(|v| v as f64 / 10f64.powi(decimals))
                    }
                    FieldTransform::SqrtRatioToPrice => {
                        let sqrt_price_opt: Option<u128> = raw
                            .as_str()
                            .and_then(|s| s.parse().ok())
                            .or_else(|| raw.as_u64().map(|v| v as u128));
                        sqrt_price_opt.map(|sqrt_price| {
                            let q96: f64 = 2f64.powi(96);
                            (sqrt_price as f64 / q96).powi(2)
                        })
                    }
                    _ => None,
                };
                if let Some(p) = price {
                    out.price = Some(p);
                }
            }
            CanonicalField::MarketKey => {
                let s = raw
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| Some(raw.to_string()));
                if let Some(sym) = s {
                    out.market_key = match &mapping.transform {
                        FieldTransform::SymbolToMarketKey => {
                            symbol_to_market_key(&sym).or(Some(sym))
                        }
                        _ => Some(sym),
                    };
                }
            }
            CanonicalField::Timestamp => {
                out.timestamp_raw = Some(raw.clone());
            }
            CanonicalField::BlockNumber => {
                out.block_number = raw
                    .as_u64()
                    .or_else(|| raw.as_str().and_then(|s| s.parse().ok()));
            }
            CanonicalField::TxHash => {
                out.tx_hash = raw.as_str().map(str::to_string);
            }
            CanonicalField::ProtocolId => {
                out.protocol_id = raw.as_str().map(str::to_string);
            }
            CanonicalField::ChainSlug => {
                out.chain_slug = raw.as_str().map(str::to_string);
            }
            CanonicalField::MarketId => {
                out.market_id = raw.as_str().map(str::to_string);
            }
            CanonicalField::TvlUsd => {
                out.tvl_usd = raw
                    .as_f64()
                    .or_else(|| raw.as_str().and_then(|s| s.parse().ok()));
            }
            CanonicalField::LoanAmountUsd => {
                out.loan_amount_usd = raw
                    .as_f64()
                    .or_else(|| raw.as_str().and_then(|s| s.parse().ok()));
            }
            CanonicalField::ProfitUsd => {
                out.profit_usd = raw
                    .as_f64()
                    .or_else(|| raw.as_str().and_then(|s| s.parse().ok()));
            }
            CanonicalField::TokenAddress => {
                out.token_address = raw.as_str().map(str::to_string);
            }
            CanonicalField::SourceId | CanonicalField::EventType | CanonicalField::ChainId => {
                // Auto-filled by indexer; skip
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_binance_defaults() {
        let mappings = default_mappings_for_parser("binance_miniticker_v1");
        assert_eq!(mappings.len(), 3);
    }

    #[test]
    fn test_apply_binance_payload() {
        let payload = json!({ "c": "0.9998", "s": "USDCUSDT", "E": 1710000000000u64 });
        let mappings = default_mappings_for_parser("binance_miniticker_v1");
        let fields = apply_mappings(&mappings, &payload, None);
        assert!((fields.price.unwrap() - 0.9998).abs() < 1e-9);
        assert_eq!(fields.market_key.as_deref(), Some("USDC/USD"));
    }

    #[test]
    fn test_chainlink_scale_price() {
        let payload = json!({ "answer": 99980000i64, "updatedAt": 1710000000i64 });
        let mappings = default_mappings_for_parser("chainlink_answer_updated_v1");
        let fields = apply_mappings(&mappings, &payload, None);
        assert!((fields.price.unwrap() - 0.9998).abs() < 1e-4);
    }

    #[test]
    fn test_jsonpath_nested() {
        let payload = json!({ "data": [{ "last": "1.0001", "instId": "USDC-USDT" }] });
        let val = eval_jsonpath(&payload, "$.data[0].last");
        assert_eq!(val.and_then(Value::as_str), Some("1.0001"));
    }

    #[test]
    fn test_resolve_with_override() {
        let filter = json!({ "field_mappings": [
            { "source_field": "$.p", "canonical_field": "price", "transform": "to_f64" }
        ]});
        let mappings = resolve_field_mappings("binance_miniticker_v1", Some(&filter));
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].source_field, "$.p");
    }
}
