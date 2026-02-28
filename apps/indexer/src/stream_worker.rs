use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::Utc;
use event_schema::{SourceType, UnifiedEvent};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use state_manager::{PostgresRepository, RedisStreamPublisher, SourceFeedEventRecord};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::stream_connector::{
    http_poll::HttpPollConnector, rpc_logs::RpcLogsConnector, websocket::WebsocketStreamConnector,
};
use crate::stream_parser::{parse_payload, ParsedFeedEvent, ParserInput};

const FX_LOOKUP_MARKET_KEY: &str = "USDT/USD";
const FX_LOOKUP_FRESHNESS_SECONDS: i64 = 30;
const FX_CACHE_TTL_SECONDS: i64 = 3;
const DEFAULT_RPC_LOGS_POLL_INTERVAL_MS: u64 = 2_000;
const DEFAULT_HTTP_POLL_INTERVAL_MS: u64 = 5_000;
const MIN_POLL_INTERVAL_MS: u64 = 200;
const MAX_POLL_INTERVAL_MS: u64 = 60_000;

#[derive(Debug, Clone)]
struct CachedFxRate {
    rate: f64,
    cached_at_ms: i64,
}

#[derive(Debug, Default, Clone)]
struct FxRateCache {
    usdt_usd: Option<CachedFxRate>,
}

#[derive(Debug, Clone)]
pub struct RuntimeStreamConfig {
    pub stream_config_id: String,
    pub source_id: String,
    pub source_type: String,
    pub source_name: String,
    pub connection_config: Value,
    pub auth_secret_ref: Option<String>,
    pub auth_config: Value,
    pub connector_mode: String,
    pub stream_name: String,
    pub subscription_key: Option<String>,
    pub event_type: String,
    pub parser_name: String,
    pub market_key: Option<String>,
    pub asset_pair: Option<String>,
    pub filter_config: Value,
    pub payload_ts_path: Option<String>,
    pub payload_ts_unit: String,
    pub poll_interval_ms: Option<u64>,
    pub tenant_targets: Vec<String>,
}

fn resolve_poll_interval_duration(configured_ms: Option<u64>, default_ms: u64) -> Duration {
    let resolved_ms = configured_ms
        .unwrap_or(default_ms)
        .clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS);
    Duration::from_millis(resolved_ms)
}

pub async fn run_stream_worker(
    config: RuntimeStreamConfig,
    repo: PostgresRepository,
    stream: RedisStreamPublisher,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut fx_cache = FxRateCache::default();
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    info!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        connector_mode = %config.connector_mode,
        tenant_target_count = config.tenant_targets.len(),
        "stream worker started",
    );

    loop {
        if *shutdown.borrow() {
            break;
        }

        let result = match config.connector_mode.as_str() {
            "websocket" => run_websocket_loop(&config, &repo, &stream, &mut shutdown, &mut fx_cache).await,
            "rpc_logs" => run_rpc_logs_loop(&config, &repo, &stream, &mut shutdown, &mut fx_cache).await,
            "http_poll" => run_http_poll_loop(&config, &repo, &stream, &mut shutdown, &mut fx_cache).await,
            mode => Err(anyhow!("unsupported_connector_mode:{mode}")),
        };

        if *shutdown.borrow() {
            break;
        }

        if let Err(error) = result {
            warn!(
                stream_config_id = %config.stream_config_id,
                source_id = %config.source_id,
                error = ?error,
                retry_after_sec = backoff.as_secs(),
                "stream worker loop failed; reconnecting",
            );
        }

        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep(backoff) => {}
        }

        backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
    }

    info!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        "stream worker stopped",
    );
}

async fn run_websocket_loop(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    stream: &RedisStreamPublisher,
    shutdown: &mut watch::Receiver<bool>,
    fx_cache: &mut FxRateCache,
) -> Result<()> {
    let endpoint = endpoint_from_runtime_config(config)?;
    let mut connector = WebsocketStreamConnector::new(
        endpoint,
        config.stream_name.clone(),
        config.subscription_key.clone(),
        config.filter_config.clone(),
    );
    connector.connect().await?;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return Ok(());
                }
            }
            raw = connector.next_payload() => {
                let payload = raw?;
                process_payload(config, repo, stream, payload, None, fx_cache).await?;
            }
        }
    }
}

async fn run_rpc_logs_loop(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    stream: &RedisStreamPublisher,
    shutdown: &mut watch::Receiver<bool>,
    fx_cache: &mut FxRateCache,
) -> Result<()> {
    let endpoint = endpoint_from_runtime_config(config)?;
    let poll_interval =
        resolve_poll_interval_duration(config.poll_interval_ms, DEFAULT_RPC_LOGS_POLL_INTERVAL_MS);
    let mut connector = RpcLogsConnector::new(
        endpoint,
        config.filter_config.clone(),
        poll_interval,
    );
    connector.connect().await?;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return Ok(());
                }
            }
            raw = connector.next_payload() => {
                let mut payload = raw?;
                if let Some(chain_id) = connector.chain_id() {
                    if let Some(map) = payload.as_object_mut() {
                        map.insert("chainId".to_string(), json!(chain_id));
                    }
                }
                process_payload(config, repo, stream, payload, connector.chain_id(), fx_cache).await?;
            }
        }
    }
}

async fn run_http_poll_loop(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    stream: &RedisStreamPublisher,
    shutdown: &mut watch::Receiver<bool>,
    fx_cache: &mut FxRateCache,
) -> Result<()> {
    let endpoint = endpoint_from_runtime_config(config)?;
    let poll_interval =
        resolve_poll_interval_duration(config.poll_interval_ms, DEFAULT_HTTP_POLL_INTERVAL_MS);
    let mut connector = HttpPollConnector::new(endpoint, poll_interval);
    connector.connect().await?;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return Ok(());
                }
            }
            raw = connector.next_payload() => {
                match raw {
                    Ok(payload) => process_payload(config, repo, stream, payload, None, fx_cache).await?,
                    Err(error) => {
                        warn!(
                            stream_config_id = %config.stream_config_id,
                            source_id = %config.source_id,
                            error = ?error,
                            "http_poll connector returned error",
                        );
                    }
                }
            }
        }
    }
}

async fn process_payload(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    stream: &RedisStreamPublisher,
    payload: Value,
    chain_id_hint: Option<i64>,
    fx_cache: &mut FxRateCache,
) -> Result<()> {
    let parser_input = ParserInput {
        parser_name: &config.parser_name,
        event_type: &config.event_type,
        market_key_hint: config.market_key.as_deref(),
        asset_pair_hint: config.asset_pair.as_deref(),
        payload_ts_path: config.payload_ts_path.as_deref(),
        payload_ts_unit: &config.payload_ts_unit,
        filter_config: &config.filter_config,
    };

    match parse_payload(&parser_input, &payload) {
        Ok(mut parsed) => {
            if parsed.chain_id.is_none() {
                parsed.chain_id = chain_id_hint;
            }
            let mut payload_for_storage = payload.clone();
            let (parse_status, parse_error, should_fanout) =
                apply_usdt_normalization(repo, &mut parsed, &mut payload_for_storage, fx_cache).await?;

            let dedup_key = build_dedup_key(config, &parsed, &payload);
            let record = to_source_feed_record(
                config,
                &parsed,
                payload_for_storage.clone(),
                dedup_key.clone(),
                parse_status,
                parse_error,
            );
            let inserted = repo.insert_source_feed_event_record(&record).await?;
            if !inserted {
                debug!(
                    stream_config_id = %config.stream_config_id,
                    source_id = %config.source_id,
                    "duplicate source feed event skipped by dedup key",
                );
                return Ok(());
            }
            if should_fanout {
                fanout_unified_events(config, stream, &payload_for_storage, &parsed, dedup_key).await?;
            }
            Ok(())
        }
        Err(parse_error) => {
            let observed_at = Utc::now();
            let dedup_key = Some(hash_payload_only(config, &payload, observed_at));
            let record = SourceFeedEventRecord {
                stream_config_id: Some(config.stream_config_id.clone()),
                source_id: config.source_id.clone(),
                source_type: config.source_type.clone(),
                event_type: config.event_type.clone(),
                event_id: None,
                market_key: config.market_key.clone(),
                asset_pair: config.asset_pair.clone(),
                chain_id: chain_id_hint,
                block_number: None,
                tx_hash: None,
                log_index: None,
                topic0: None,
                price: None,
                payload_event_ts: None,
                observed_at,
                parse_status: "error".to_string(),
                parse_error: Some(parse_error),
                payload,
                normalized_fields: json!({}),
                dedup_key,
            };
            let _ = repo.insert_source_feed_event_record(&record).await?;
            Ok(())
        }
    }
}

fn ensure_object_payload(payload: &mut Value) {
    if payload.is_object() {
        return;
    }
    let raw_copy = payload.clone();
    *payload = json!({ "raw_payload": raw_copy });
}

fn upsert_normalized_metadata(
    parsed: &mut ParsedFeedEvent,
    raw_quote_price: Option<f64>,
    quote_asset: Option<&str>,
    normalized_price_usd: Option<f64>,
    fx_rate_usdt_usd: Option<f64>,
    fx_adjusted: bool,
) {
    let mut normalized = parsed
        .normalized_fields
        .as_object()
        .cloned()
        .unwrap_or_default();

    normalized.insert("raw_quote_price".to_string(), raw_quote_price.map_or(Value::Null, |v| json!(v)));
    normalized.insert(
        "quote_asset".to_string(),
        quote_asset
            .map(|value| Value::String(value.to_string()))
            .unwrap_or(Value::Null),
    );
    normalized.insert(
        "normalized_price_usd".to_string(),
        normalized_price_usd.map_or(Value::Null, |v| json!(v)),
    );
    normalized.insert(
        "fx_rate_usdt_usd".to_string(),
        fx_rate_usdt_usd.map_or(Value::Null, |v| json!(v)),
    );
    normalized.insert("fx_adjusted".to_string(), Value::Bool(fx_adjusted));

    parsed.normalized_fields = Value::Object(normalized);
}

fn upsert_payload_normalization(
    payload: &mut Value,
    raw_quote_price: Option<f64>,
    quote_asset: Option<&str>,
    normalized_price_usd: Option<f64>,
    fx_rate_usdt_usd: Option<f64>,
    fx_adjusted: bool,
) {
    ensure_object_payload(payload);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("raw_quote_price".to_string(), raw_quote_price.map_or(Value::Null, |v| json!(v)));
        obj.insert(
            "quote_asset".to_string(),
            quote_asset
                .map(|value| Value::String(value.to_string()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "normalized_price_usd".to_string(),
            normalized_price_usd.map_or(Value::Null, |v| json!(v)),
        );
        obj.insert(
            "fx_rate_usdt_usd".to_string(),
            fx_rate_usdt_usd.map_or(Value::Null, |v| json!(v)),
        );
        obj.insert("fx_adjusted".to_string(), Value::Bool(fx_adjusted));
    }
}

fn derive_quote_asset(parsed: &ParsedFeedEvent) -> Option<String> {
    if let Some(value) = parsed
        .normalized_fields
        .get("quote_asset")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_ascii_uppercase());
    }

    if let Some(asset_pair) = parsed.asset_pair.as_deref() {
        let cleaned = asset_pair.trim().to_ascii_uppercase();
        for delimiter in ['/', '-'] {
            if cleaned.contains(delimiter) {
                let mut parts = cleaned.split(delimiter).filter(|part| !part.is_empty());
                let _base = parts.next();
                if let Some(quote) = parts.next() {
                    return Some(quote.to_string());
                }
            }
        }
        for suffix in ["USDT", "USDC", "USD"] {
            if cleaned.ends_with(suffix) {
                return Some(suffix.to_string());
            }
        }
    }

    parsed
        .market_key
        .as_deref()
        .and_then(|market_key| market_key.split('/').nth(1))
        .map(|quote| quote.trim().to_ascii_uppercase())
        .filter(|quote| !quote.is_empty())
}

async fn lookup_usdt_usd_rate(repo: &PostgresRepository, fx_cache: &mut FxRateCache) -> Result<Option<f64>> {
    let now_ms = Utc::now().timestamp_millis();
    if let Some(cached) = fx_cache.usdt_usd.as_ref() {
        if now_ms - cached.cached_at_ms <= FX_CACHE_TTL_SECONDS * 1_000 {
            return Ok(Some(cached.rate));
        }
    }

    let latest = repo
        .latest_market_price(FX_LOOKUP_MARKET_KEY, FX_LOOKUP_FRESHNESS_SECONDS)
        .await?;
    if let Some(rate) = latest.filter(|value| value.is_finite() && *value > 0.0) {
        fx_cache.usdt_usd = Some(CachedFxRate {
            rate,
            cached_at_ms: now_ms,
        });
        return Ok(Some(rate));
    }

    Ok(None)
}

async fn apply_usdt_normalization(
    repo: &PostgresRepository,
    parsed: &mut ParsedFeedEvent,
    payload: &mut Value,
    fx_cache: &mut FxRateCache,
) -> Result<(&'static str, Option<String>, bool)> {
    let Some(raw_price) = parsed.price else {
        return Ok(("parsed", None, true));
    };

    let quote_asset = derive_quote_asset(parsed);
    let quote_asset_ref = quote_asset.as_deref();

    if quote_asset_ref != Some("USDT") {
        upsert_normalized_metadata(parsed, Some(raw_price), quote_asset_ref, Some(raw_price), None, false);
        upsert_payload_normalization(payload, Some(raw_price), quote_asset_ref, Some(raw_price), None, false);
        return Ok(("parsed", None, true));
    }

    let fx_rate = lookup_usdt_usd_rate(repo, fx_cache).await?;
    let Some(fx_rate) = fx_rate else {
        parsed.price = None;
        upsert_normalized_metadata(
            parsed,
            Some(raw_price),
            quote_asset_ref,
            None,
            None,
            true,
        );
        upsert_payload_normalization(
            payload,
            Some(raw_price),
            quote_asset_ref,
            None,
            None,
            true,
        );
        return Ok((
            "partial",
            Some("missing_fresh_usdt_usd_rate".to_string()),
            false,
        ));
    };

    let normalized_price = raw_price * fx_rate;
    parsed.price = Some(normalized_price);
    upsert_normalized_metadata(
        parsed,
        Some(raw_price),
        quote_asset_ref,
        Some(normalized_price),
        Some(fx_rate),
        true,
    );
    upsert_payload_normalization(
        payload,
        Some(raw_price),
        quote_asset_ref,
        Some(normalized_price),
        Some(fx_rate),
        true,
    );
    Ok(("parsed", None, true))
}

fn to_source_feed_record(
    config: &RuntimeStreamConfig,
    parsed: &ParsedFeedEvent,
    payload: Value,
    dedup_key: Option<String>,
    parse_status: &str,
    parse_error: Option<String>,
) -> SourceFeedEventRecord {
    SourceFeedEventRecord {
        stream_config_id: Some(config.stream_config_id.clone()),
        source_id: config.source_id.clone(),
        source_type: config.source_type.clone(),
        event_type: parsed.event_type.clone(),
        event_id: parsed.event_id.clone(),
        market_key: parsed.market_key.clone(),
        asset_pair: parsed.asset_pair.clone(),
        chain_id: parsed.chain_id,
        block_number: parsed.block_number,
        tx_hash: parsed.tx_hash.clone(),
        log_index: parsed.log_index,
        topic0: parsed.topic0.clone(),
        price: parsed.price,
        payload_event_ts: parsed.payload_event_ts,
        observed_at: parsed.observed_at,
        parse_status: parse_status.to_string(),
        parse_error,
        payload,
        normalized_fields: parsed.normalized_fields.clone(),
        dedup_key,
    }
}

async fn fanout_unified_events(
    config: &RuntimeStreamConfig,
    stream: &RedisStreamPublisher,
    payload: &Value,
    parsed: &ParsedFeedEvent,
    dedup_key: Option<String>,
) -> Result<()> {
    let source_type = map_source_type(&config.source_type);

    for tenant_id in &config.tenant_targets {
        let event_id = parsed
            .event_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let enriched_payload = enrich_payload_for_unified(config, payload, parsed, dedup_key.as_deref());
        let event = UnifiedEvent {
            event_id,
            tenant_id: tenant_id.to_string(),
            source_id: config.source_id.clone(),
            source_type: source_type.clone(),
            event_type: parsed.event_type.clone(),
            timestamp: parsed.observed_at,
            payload: enriched_payload,
            chain_id: parsed.chain_id,
            block_number: parsed.block_number,
            tx_hash: parsed.tx_hash.clone(),
            market_key: parsed.market_key.clone(),
            price: parsed.price,
        };
        stream.publish_unified_event(&event).await?;
    }

    Ok(())
}

fn enrich_payload_for_unified(
    config: &RuntimeStreamConfig,
    payload: &Value,
    parsed: &ParsedFeedEvent,
    dedup_key: Option<&str>,
) -> Value {
    let mut enriched = payload.clone();
    if !enriched.is_object() {
        enriched = json!({ "raw_payload": payload });
    }
    if let Some(obj) = enriched.as_object_mut() {
        obj.insert("raw_persisted".to_string(), Value::Bool(true));
        obj.insert(
            "stream_config_id".to_string(),
            Value::String(config.stream_config_id.clone()),
        );
        obj.insert("source_id".to_string(), Value::String(config.source_id.clone()));
        obj.insert("parser_name".to_string(), Value::String(config.parser_name.clone()));
        if let Some(market_key) = parsed.market_key.as_ref() {
            obj.insert("market_key".to_string(), Value::String(market_key.clone()));
        }
        if let Some(asset_pair) = parsed.asset_pair.as_ref() {
            obj.insert("asset_pair".to_string(), Value::String(asset_pair.clone()));
        }
        if let Some(price) = parsed.price {
            obj.insert("price".to_string(), json!(price));
        }
        if let Some(chain_id) = parsed.chain_id {
            obj.insert("chainId".to_string(), json!(chain_id));
        }
        if let Some(dedup_key) = dedup_key {
            obj.insert("dedup_key".to_string(), Value::String(dedup_key.to_string()));
        }
    }
    enriched
}

fn endpoint_from_runtime_config(config: &RuntimeStreamConfig) -> Result<String> {
    let endpoint_template = endpoint_from_connection_config(&config.connection_config)?;
    resolve_endpoint_template(&endpoint_template, &config.auth_config, config.auth_secret_ref.as_deref())
}

fn endpoint_from_connection_config(config: &Value) -> Result<String> {
    for key in ["ws_endpoint", "rpc_url", "ws_url", "endpoint", "http_url"] {
        if let Some(value) = config.get(key).and_then(Value::as_str) {
            let endpoint = value.trim();
            if !endpoint.is_empty() {
                return Ok(endpoint.to_string());
            }
        }
    }
    Err(anyhow!("missing source endpoint in connection_config"))
}

fn resolve_endpoint_template(
    endpoint_template: &str,
    auth_config: &Value,
    auth_secret_ref: Option<&str>,
) -> Result<String> {
    let mut endpoint = endpoint_template.trim().to_string();

    if let Some(auth_values) = auth_config.as_object() {
        for (key, value) in auth_values {
            if let Some(replacement) = auth_placeholder_value(value) {
                endpoint = endpoint.replace(&format!("{{{key}}}"), &replacement);
            }
        }
    }

    if endpoint.contains("YOUR_ALCHEMY_KEY") {
        if let Some(value) = extract_alchemy_key(auth_config) {
            endpoint = endpoint.replace("YOUR_ALCHEMY_KEY", &value);
        } else {
            return Err(anyhow!(
                "missing auth_config.alchemy_api_key (or auth_config.api_key) for endpoint template"
            ));
        }
    }

    let unresolved_placeholders = collect_unresolved_placeholders(&endpoint)
        .into_iter()
        .filter(|key| key != "subscription_key")
        .collect::<Vec<_>>();
    if !unresolved_placeholders.is_empty() {
        return Err(anyhow!(
            "missing auth_config values for endpoint template placeholders: {}{}",
            unresolved_placeholders.join(", "),
            auth_secret_ref
                .map(|secret_ref| format!(" (auth_secret_ref={secret_ref})"))
                .unwrap_or_default()
        ));
    }

    Ok(endpoint)
}

fn auth_placeholder_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(boolean) => Some(boolean.to_string()),
        _ => None,
    }
}

fn extract_alchemy_key(auth_config: &Value) -> Option<String> {
    let object = auth_config.as_object()?;
    for key in ["alchemy_api_key", "api_key", "apikey"] {
        let value = object.get(key)?;
        if let Some(parsed) = auth_placeholder_value(value) {
            return Some(parsed);
        }
    }
    None
}

fn collect_unresolved_placeholders(endpoint: &str) -> Vec<String> {
    let mut placeholders: Vec<String> = Vec::new();
    let chars = endpoint.as_bytes();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != b'{' {
            index += 1;
            continue;
        }
        let Some(end_offset) = chars[index + 1..].iter().position(|value| *value == b'}') else {
            break;
        };
        let end_index = index + 1 + end_offset;
        if end_index > index + 1 {
            let key = &endpoint[index + 1..end_index];
            if key.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
                let candidate = key.to_string();
                if !placeholders.iter().any(|existing| existing == &candidate) {
                    placeholders.push(candidate);
                }
            }
        }
        index = end_index + 1;
    }
    placeholders
}

fn map_source_type(source_type: &str) -> SourceType {
    match source_type.to_ascii_lowercase().as_str() {
        "cex_websocket" => SourceType::CexWebsocket,
        "evm_chain" => SourceType::EvmChain,
        "dex_api" => SourceType::DexApi,
        "oracle_api" => SourceType::OracleApi,
        _ => SourceType::CustomApi,
    }
}

fn build_dedup_key(config: &RuntimeStreamConfig, parsed: &ParsedFeedEvent, payload: &Value) -> Option<String> {
    if let Some(event_id) = parsed.event_id.as_ref() {
        return Some(format!(
            "provider:{}:{}:{}",
            config.source_id, config.stream_config_id, event_id
        ));
    }

    let mut hasher = Sha256::new();
    hasher.update(config.source_id.as_bytes());
    hasher.update(b"|");
    hasher.update(config.stream_config_id.as_bytes());
    hasher.update(b"|");
    hasher.update(parsed.event_type.as_bytes());
    hasher.update(b"|");
    hasher.update(parsed.tx_hash.as_deref().unwrap_or_default().as_bytes());
    hasher.update(b"|");
    hasher.update(parsed.log_index.unwrap_or_default().to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(parsed.observed_at.timestamp_millis().to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(parsed.asset_pair.as_deref().unwrap_or_default().as_bytes());
    hasher.update(b"|");
    hasher.update(parsed.market_key.as_deref().unwrap_or_default().as_bytes());
    hasher.update(b"|");
    hasher.update(parsed.topic0.as_deref().unwrap_or_default().as_bytes());
    hasher.update(b"|");
    hasher.update(payload.to_string().as_bytes());
    Some(hex::encode(hasher.finalize()))
}

fn hash_payload_only(config: &RuntimeStreamConfig, payload: &Value, observed_at: chrono::DateTime<Utc>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(config.source_id.as_bytes());
    hasher.update(b"|");
    hasher.update(config.stream_config_id.as_bytes());
    hasher.update(b"|");
    hasher.update(config.event_type.as_bytes());
    hasher.update(b"|");
    hasher.update(observed_at.timestamp_millis().to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(payload.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_endpoint_template_replaces_auth_tokens() {
        let endpoint = resolve_endpoint_template(
            "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}",
            &json!({ "alchemy_api_key": "abc123" }),
            None,
        )
        .expect("endpoint should resolve");
        assert_eq!(endpoint, "wss://eth-mainnet.g.alchemy.com/v2/abc123");
    }

    #[test]
    fn resolve_endpoint_template_supports_legacy_alchemy_token() {
        let endpoint = resolve_endpoint_template(
            "wss://eth-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY",
            &json!({ "alchemy_api_key": "legacy-key" }),
            None,
        )
        .expect("legacy endpoint should resolve");
        assert_eq!(endpoint, "wss://eth-mainnet.g.alchemy.com/v2/legacy-key");
    }

    #[test]
    fn resolve_endpoint_template_allows_subscription_placeholder() {
        let endpoint = resolve_endpoint_template(
            "wss://api.example.com/ws/{subscription_key}",
            &json!({}),
            None,
        )
        .expect("subscription placeholder should be deferred");
        assert_eq!(endpoint, "wss://api.example.com/ws/{subscription_key}");
    }

    #[test]
    fn resolve_endpoint_template_errors_on_missing_auth_placeholder() {
        let error = resolve_endpoint_template(
            "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}",
            &json!({}),
            Some("vault://alchemy/prod"),
        )
        .expect_err("missing placeholder should error");

        let message = error.to_string();
        assert!(message.contains("alchemy_api_key"));
        assert!(message.contains("auth_secret_ref=vault://alchemy/prod"));
    }
}
