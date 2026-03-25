use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};
use event_schema::{SourceType, UnifiedEvent};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use state_manager::{
    IngestFailureRecord, IngestOperationalEventRecord, PostgresRawRepository, PostgresRepository,
    RawRecordPointer, RedisStreamPublisher, SourceEnvelopeV1,
};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use url::Url;
use uuid::Uuid;

use crate::stream_connector::{
    http_poll::HttpPollConnector, rpc_logs::RpcLogsConnector, rpc_state::RpcStateConnector,
    websocket::WebsocketStreamConnector,
};
use crate::stream_parser::{parse_payload, ParsedFeedEvent, ParserInput};

const FX_LOOKUP_MARKET_KEY: &str = "USDT/USD";
const FX_LOOKUP_FRESHNESS_SECONDS: i64 = 30;
const FX_CACHE_TTL_SECONDS: i64 = 3;
const MARKET_TRUTH_FRESHNESS_SECONDS: i64 = 30;
const MARKET_TRUTH_PEG_TARGET: f64 = 1.0;
const DEFAULT_RPC_LOGS_POLL_INTERVAL_MS: u64 = 2_000;
const DEFAULT_RPC_STATE_POLL_INTERVAL_MS: u64 = 5_000;
const DEFAULT_HTTP_POLL_INTERVAL_MS: u64 = 5_000;
const DEFAULT_RAW_LANDING_TIMEOUT_MS: u64 = 150;
const TEST_MODE_MOCK_POLL_INTERVAL_MS: u64 = 200;
const MIN_POLL_INTERVAL_MS: u64 = 200;
const MAX_POLL_INTERVAL_MS: u64 = 60_000;
const SOURCE_HEALTH_SUCCESS_REPORT_INTERVAL_SECS: u64 = 30;
const SOURCE_HEALTH_FAILURE_REPORT_INTERVAL_SECS: u64 = 30;

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
struct MarketTruthSnapshot {
    market_key: String,
    median_price: f64,
    deviation_pct: f64,
    source_count: usize,
    peg_target: f64,
}

#[derive(Debug, Clone)]
pub struct RuntimeStreamConfig {
    pub stream_config_id: String,
    pub source_id: String,
    pub source_type: String,
    pub source_name: String,
    pub connection_config: Value,
    pub operating_mode_profile: String,
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

#[derive(Debug, Clone, Copy)]
enum RawLandingStatus {
    Persisted,
    Deferred,
    Disabled,
    TimedOut,
    Failed,
}

impl RawLandingStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Persisted => "persisted",
            Self::Deferred => "deferred",
            Self::Disabled => "disabled",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
        }
    }

    fn persisted(self) -> bool {
        matches!(self, Self::Persisted)
    }
}

#[derive(Debug, Clone)]
struct RawLandingOutcome {
    pointer: Option<RawRecordPointer>,
    status: RawLandingStatus,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct TestModeLogState {
    payload_logs_emitted: u32,
    unsimulated_warning_emitted: bool,
}

#[derive(Debug, Default)]
struct SourceHealthReportState {
    last_success_report_at: Option<tokio::time::Instant>,
    last_failure_report_at: Option<tokio::time::Instant>,
    last_failure_message: Option<String>,
    last_reported_healthy: Option<bool>,
}

struct PayloadProcessingContext<'a> {
    stream: &'a RedisStreamPublisher,
    chain_id_hint: Option<i64>,
    simulation_run_id_hint: Option<&'a str>,
    fx_cache: &'a mut FxRateCache,
    source_health_report_state: &'a mut SourceHealthReportState,
    test_mode_log_state: &'a mut TestModeLogState,
}

struct HttpPollProcessingContext<'a> {
    config: &'a RuntimeStreamConfig,
    repo: &'a PostgresRepository,
    raw_repo: Option<&'a PostgresRawRepository>,
    payload_ctx: PayloadProcessingContext<'a>,
}

struct TestModePayloadLogContext<'a> {
    inserted: bool,
    is_simulated: bool,
    simulation_run_id: Option<&'a str>,
    parsed: &'a ParsedFeedEvent,
    dedup_key: Option<&'a str>,
    raw_landing_status: RawLandingStatus,
    raw_landing_error: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct UnifiedEventMeta<'a> {
    dedup_key: Option<&'a str>,
    ingest_persisted: bool,
    raw_landing_status: RawLandingStatus,
    raw_landing_error: Option<&'a str>,
    is_simulated: bool,
    simulation_run_id: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
struct EndpointLogContext {
    host: Option<String>,
    path: String,
    is_mock_endpoint: bool,
    tenant_id: Option<String>,
    stream_name: Option<String>,
    simulation_run_id: Option<String>,
}

fn resolve_poll_interval_duration(configured_ms: Option<u64>, default_ms: u64) -> Duration {
    let resolved_ms = configured_ms
        .unwrap_or(default_ms)
        .clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS);
    Duration::from_millis(resolved_ms)
}

fn resolve_runtime_poll_interval_duration(
    config: &RuntimeStreamConfig,
    endpoint_context: &EndpointLogContext,
    default_ms: u64,
) -> Duration {
    let configured_ms =
        if config.operating_mode_profile == "test" && endpoint_context.is_mock_endpoint {
            Some(
                config
                    .poll_interval_ms
                    .unwrap_or(default_ms)
                    .min(TEST_MODE_MOCK_POLL_INTERVAL_MS),
            )
        } else {
            config.poll_interval_ms
        };
    let effective_default_ms =
        if config.operating_mode_profile == "test" && endpoint_context.is_mock_endpoint {
            default_ms.min(TEST_MODE_MOCK_POLL_INTERVAL_MS)
        } else {
            default_ms
        };
    resolve_poll_interval_duration(configured_ms, effective_default_ms)
}

fn simulation_metadata_from_payload(payload: &Value) -> (bool, Option<String>) {
    let simulation = payload.get("simulation").and_then(Value::as_object);
    let run_id = simulation
        .and_then(|meta| {
            meta.get("run_id")
                .or_else(|| meta.get("simulation_run_id"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let is_simulated = simulation
        .and_then(|meta| meta.get("is_simulated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || payload
            .get("is_simulated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || run_id.is_some();
    (is_simulated, run_id)
}

fn effective_simulation_metadata(
    payload: &Value,
    simulation_run_id_hint: Option<&str>,
) -> (bool, Option<String>) {
    let (is_simulated, simulation_run_id) = simulation_metadata_from_payload(payload);
    let resolved_run_id = simulation_run_id.or_else(|| {
        simulation_run_id_hint
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });
    (is_simulated || resolved_run_id.is_some(), resolved_run_id)
}

fn accelerated_simulation_timestamp(
    payload: &Value,
    simulation_run_id_hint: Option<&str>,
) -> Option<DateTime<Utc>> {
    let (is_simulated, simulation_run_id) =
        effective_simulation_metadata(payload, simulation_run_id_hint);
    if !is_simulated && simulation_run_id.is_none() {
        return None;
    }

    let simulation = payload.get("simulation").and_then(Value::as_object)?;
    let speed_factor = simulation
        .get("speed_factor")
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(raw) => raw.trim().parse::<f64>().ok(),
            _ => None,
        })
        .unwrap_or(1.0);
    if !speed_factor.is_finite() || speed_factor <= 1.0 {
        return None;
    }

    simulation
        .get("event_ts")
        .and_then(parse_simulation_timestamp_value)
        .or_else(|| {
            simulation
                .get("event_timestamp")
                .and_then(parse_simulation_timestamp_value)
        })
        .or_else(|| {
            simulation
                .get("event_ts_ms")
                .and_then(parse_simulation_timestamp_value)
        })
}

fn parse_simulation_timestamp_value(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(raw) => chrono::DateTime::parse_from_rfc3339(raw.trim())
            .ok()
            .map(|ts| ts.with_timezone(&Utc)),
        Value::Number(number) => number
            .as_i64()
            .and_then(|millis| Utc.timestamp_millis_opt(millis).single()),
        _ => None,
    }
}

fn apply_accelerated_simulation_timestamp(
    parsed: &mut ParsedFeedEvent,
    payload: &Value,
    simulation_run_id_hint: Option<&str>,
) {
    let Some(replay_ts) = accelerated_simulation_timestamp(payload, simulation_run_id_hint) else {
        return;
    };
    parsed.payload_event_ts.get_or_insert(replay_ts);
    parsed.observed_at = replay_ts;
}

fn is_static_worker_config_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("rpc_state connector missing calls configuration")
        || message.contains("unsupported_connector_mode:")
}

pub async fn run_stream_worker(
    config: RuntimeStreamConfig,
    repo: PostgresRepository,
    raw_repo: Option<PostgresRawRepository>,
    stream: RedisStreamPublisher,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut fx_cache = FxRateCache::default();
    let mut source_health_report_state = SourceHealthReportState::default();
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    info!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        connector_mode = %config.connector_mode,
        operating_mode_profile = %config.operating_mode_profile,
        tenant_target_count = config.tenant_targets.len(),
        "stream worker started",
    );

    loop {
        if *shutdown.borrow() {
            break;
        }

        let result = match config.connector_mode.as_str() {
            "websocket" => {
                run_websocket_loop(
                    &config,
                    &repo,
                    raw_repo.as_ref(),
                    &stream,
                    &mut shutdown,
                    &mut fx_cache,
                    &mut source_health_report_state,
                )
                .await
            }
            "rpc_logs" => {
                run_rpc_logs_loop(
                    &config,
                    &repo,
                    raw_repo.as_ref(),
                    &stream,
                    &mut shutdown,
                    &mut fx_cache,
                    &mut source_health_report_state,
                )
                .await
            }
            "rpc_state" => {
                run_rpc_state_loop(
                    &config,
                    &repo,
                    raw_repo.as_ref(),
                    &stream,
                    &mut shutdown,
                    &mut fx_cache,
                    &mut source_health_report_state,
                )
                .await
            }
            "http_poll" => {
                run_http_poll_loop(
                    &config,
                    &repo,
                    raw_repo.as_ref(),
                    &stream,
                    &mut shutdown,
                    &mut fx_cache,
                    &mut source_health_report_state,
                )
                .await
            }
            mode => Err(anyhow!("unsupported_connector_mode:{mode}")),
        };

        if *shutdown.borrow() {
            break;
        }

        if let Err(error) = result {
            report_source_health_failure(&repo, &config, &mut source_health_report_state, &error)
                .await;
            if is_static_worker_config_error(&error) {
                common::log_error!(
                    warn,
                    error,
                    "stream worker configuration invalid; waiting for catalog reload",
                    stream_config_id = %config.stream_config_id,
                    source_id = %config.source_id,
                );
                break;
            }
            common::log_error!(
                warn,
                error,
                "stream worker loop failed; reconnecting",
                stream_config_id = %config.stream_config_id,
                source_id = %config.source_id,
                retry_after_sec = backoff.as_secs()
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
    raw_repo: Option<&PostgresRawRepository>,
    stream: &RedisStreamPublisher,
    shutdown: &mut watch::Receiver<bool>,
    fx_cache: &mut FxRateCache,
    source_health_report_state: &mut SourceHealthReportState,
) -> Result<()> {
    let endpoint = endpoint_from_runtime_config(config)?;
    let endpoint_log_context = log_test_mode_connector_endpoint(config, &endpoint);
    let mut connector = WebsocketStreamConnector::new(
        endpoint,
        config.stream_name.clone(),
        config.subscription_key.clone(),
        config.filter_config.clone(),
    );
    connector.connect().await?;
    log_test_mode_connector_connected(config, &endpoint_log_context);
    let mut test_mode_log_state = TestModeLogState::default();

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return Ok(());
                }
            }
            raw = connector.next_payload() => {
                let payload = raw?;
                let mut payload_ctx = PayloadProcessingContext {
                    stream,
                    chain_id_hint: None,
                    simulation_run_id_hint: endpoint_log_context.simulation_run_id.as_deref(),
                    fx_cache,
                    source_health_report_state,
                    test_mode_log_state: &mut test_mode_log_state,
                };
                process_payload(config, repo, raw_repo, payload, &mut payload_ctx).await?;
            }
        }
    }
}

async fn run_rpc_logs_loop(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    raw_repo: Option<&PostgresRawRepository>,
    stream: &RedisStreamPublisher,
    shutdown: &mut watch::Receiver<bool>,
    fx_cache: &mut FxRateCache,
    source_health_report_state: &mut SourceHealthReportState,
) -> Result<()> {
    let endpoint = endpoint_from_runtime_config(config)?;
    let endpoint_log_context = log_test_mode_connector_endpoint(config, &endpoint);
    let poll_interval = resolve_runtime_poll_interval_duration(
        config,
        &endpoint_log_context,
        DEFAULT_RPC_LOGS_POLL_INTERVAL_MS,
    );
    let mut connector =
        RpcLogsConnector::new(endpoint, config.filter_config.clone(), poll_interval);
    connector.connect().await?;
    log_test_mode_connector_connected(config, &endpoint_log_context);
    let mut test_mode_log_state = TestModeLogState::default();

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
                let mut payload_ctx = PayloadProcessingContext {
                    stream,
                    chain_id_hint: connector.chain_id(),
                    simulation_run_id_hint: endpoint_log_context.simulation_run_id.as_deref(),
                    fx_cache,
                    source_health_report_state,
                    test_mode_log_state: &mut test_mode_log_state,
                };
                process_payload(config, repo, raw_repo, payload, &mut payload_ctx).await?;
            }
        }
    }
}

async fn run_http_poll_loop(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    raw_repo: Option<&PostgresRawRepository>,
    stream: &RedisStreamPublisher,
    shutdown: &mut watch::Receiver<bool>,
    fx_cache: &mut FxRateCache,
    source_health_report_state: &mut SourceHealthReportState,
) -> Result<()> {
    let endpoint = endpoint_from_runtime_config(config)?;
    let endpoint_log_context = log_test_mode_connector_endpoint(config, &endpoint);
    let poll_interval = resolve_runtime_poll_interval_duration(
        config,
        &endpoint_log_context,
        DEFAULT_HTTP_POLL_INTERVAL_MS,
    );
    let mut connector = HttpPollConnector::new(endpoint, poll_interval);
    connector.connect().await?;
    log_test_mode_connector_connected(config, &endpoint_log_context);
    let mut test_mode_log_state = TestModeLogState::default();
    let mut fetch_due = true;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return Ok(());
                }
            }
            _ = tokio::time::sleep(poll_interval), if !fetch_due => {
                fetch_due = true;
            }
        }

        if fetch_due {
            fetch_due = false;
            match connector.fetch_payload(connector.endpoint()).await {
                Ok(Some(payload)) => {
                    let mut processing_ctx = HttpPollProcessingContext {
                        config,
                        repo,
                        raw_repo,
                        payload_ctx: PayloadProcessingContext {
                            stream,
                            chain_id_hint: None,
                            simulation_run_id_hint: endpoint_log_context
                                .simulation_run_id
                                .as_deref(),
                            fx_cache,
                            source_health_report_state,
                            test_mode_log_state: &mut test_mode_log_state,
                        },
                    };
                    process_http_poll_payload(&mut processing_ctx, payload).await?;
                }
                Ok(None) => {}
                Err(error) => {
                    common::log_error!(
                        warn,
                        error,
                        "http_poll connector returned error",
                        stream_config_id = %config.stream_config_id,
                        source_id = %config.source_id,
                        endpoint = %connector.endpoint()
                    );
                }
            }
        }
    }
}

async fn run_rpc_state_loop(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    raw_repo: Option<&PostgresRawRepository>,
    stream: &RedisStreamPublisher,
    shutdown: &mut watch::Receiver<bool>,
    fx_cache: &mut FxRateCache,
    source_health_report_state: &mut SourceHealthReportState,
) -> Result<()> {
    let endpoint = endpoint_from_runtime_config(config)?;
    let endpoint_log_context = log_test_mode_connector_endpoint(config, &endpoint);
    let poll_interval = resolve_runtime_poll_interval_duration(
        config,
        &endpoint_log_context,
        DEFAULT_RPC_STATE_POLL_INTERVAL_MS,
    );
    let mut connector =
        RpcStateConnector::new(endpoint, config.filter_config.clone(), poll_interval);
    connector.connect().await?;
    log_test_mode_connector_connected(config, &endpoint_log_context);
    let mut test_mode_log_state = TestModeLogState::default();

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
                        map.entry("chainId".to_string()).or_insert_with(|| json!(chain_id));
                    }
                }
                let mut payload_ctx = PayloadProcessingContext {
                    stream,
                    chain_id_hint: connector.chain_id(),
                    simulation_run_id_hint: endpoint_log_context.simulation_run_id.as_deref(),
                    fx_cache,
                    source_health_report_state,
                    test_mode_log_state: &mut test_mode_log_state,
                };
                process_payload(config, repo, raw_repo, payload, &mut payload_ctx).await?;
            }
        }
    }
}

fn raw_landing_timeout_ms() -> u64 {
    std::env::var("RAW_LANDING_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_RAW_LANDING_TIMEOUT_MS)
        .max(1)
}

fn raw_landing_required() -> bool {
    std::env::var("RAW_LANDING_REQUIRED")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

async fn persist_raw_envelope(
    raw_repo: Option<&PostgresRawRepository>,
    envelope: &SourceEnvelopeV1,
) -> RawLandingOutcome {
    let Some(writer) = raw_repo else {
        return RawLandingOutcome {
            pointer: None,
            status: RawLandingStatus::Disabled,
            error: Some("raw_repo_not_configured".to_string()),
        };
    };

    let timeout_ms = raw_landing_timeout_ms();
    match tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        writer.write_source_envelope(envelope),
    )
    .await
    {
        Ok(Ok(pointer)) => {
            if pointer.is_some() {
                RawLandingOutcome {
                    pointer,
                    status: RawLandingStatus::Persisted,
                    error: None,
                }
            } else {
                RawLandingOutcome {
                    pointer: None,
                    status: RawLandingStatus::Failed,
                    error: Some("raw_pointer_missing".to_string()),
                }
            }
        }
        Ok(Err(error)) => RawLandingOutcome {
            pointer: None,
            status: RawLandingStatus::Failed,
            error: Some(error.to_string()),
        },
        Err(_) => RawLandingOutcome {
            pointer: None,
            status: RawLandingStatus::TimedOut,
            error: Some(format!("raw_landing_timeout_ms={timeout_ms}")),
        },
    }
}

fn defer_raw_envelope_persist(
    raw_repo: Option<&PostgresRawRepository>,
    envelope: &SourceEnvelopeV1,
) {
    let Some(writer) = raw_repo.cloned() else {
        return;
    };

    let envelope = envelope.clone();
    let timeout_ms = raw_landing_timeout_ms();
    tokio::spawn(async move {
        match tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            writer.write_source_envelope(&envelope),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                common::log_error!(
                    warn,
                    error,
                    "background raw landing failed",
                    source_id = %envelope.source_id,
                    stream_id = %envelope.stream_id,
                    event_type = %envelope.event_type,
                    envelope_id = %envelope.envelope_id
                );
            }
            Err(_) => {
                warn!(
                    source_id = %envelope.source_id,
                    stream_id = %envelope.stream_id,
                    event_type = %envelope.event_type,
                    envelope_id = %envelope.envelope_id,
                    timeout_ms,
                    "background raw landing timed out"
                );
            }
        }
    });
}

async fn start_raw_landing(
    raw_repo: Option<&PostgresRawRepository>,
    envelope: &SourceEnvelopeV1,
) -> RawLandingOutcome {
    if raw_landing_required() {
        return persist_raw_envelope(raw_repo, envelope).await;
    }

    if raw_repo.is_some() {
        defer_raw_envelope_persist(raw_repo, envelope);
        return RawLandingOutcome {
            pointer: None,
            status: RawLandingStatus::Deferred,
            error: None,
        };
    }

    RawLandingOutcome {
        pointer: None,
        status: RawLandingStatus::Disabled,
        error: Some("raw_repo_not_configured".to_string()),
    }
}

async fn process_http_poll_payload(
    processing_ctx: &mut HttpPollProcessingContext<'_>,
    payload: Value,
) -> Result<()> {
    match payload {
        Value::Array(items) => {
            for item in items {
                process_payload(
                    processing_ctx.config,
                    processing_ctx.repo,
                    processing_ctx.raw_repo,
                    item,
                    &mut processing_ctx.payload_ctx,
                )
                .await?;
            }
            Ok(())
        }
        Value::Null => Ok(()),
        other => {
            process_payload(
                processing_ctx.config,
                processing_ctx.repo,
                processing_ctx.raw_repo,
                other,
                &mut processing_ctx.payload_ctx,
            )
            .await
        }
    }
}

async fn process_payload(
    config: &RuntimeStreamConfig,
    repo: &PostgresRepository,
    raw_repo: Option<&PostgresRawRepository>,
    payload: Value,
    payload_ctx: &mut PayloadProcessingContext<'_>,
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
                parsed.chain_id = payload_ctx.chain_id_hint;
            }
            apply_accelerated_simulation_timestamp(
                &mut parsed,
                &payload,
                payload_ctx.simulation_run_id_hint,
            );
            let (is_simulated, simulation_run_id) =
                effective_simulation_metadata(&payload, payload_ctx.simulation_run_id_hint);
            let mut payload_for_storage = payload.clone();
            let (parse_status, parse_error, should_fanout) = apply_usdt_normalization(
                repo,
                &mut parsed,
                &mut payload_for_storage,
                payload_ctx.fx_cache,
            )
            .await?;
            apply_market_truth_context(repo, config, &mut parsed, &mut payload_for_storage).await?;

            let dedup_key = build_dedup_key(
                config,
                &parsed,
                &payload,
                payload_ctx.simulation_run_id_hint,
            );
            let envelope = to_source_envelope(
                config,
                &parsed,
                payload_for_storage.clone(),
                dedup_key.as_deref(),
                parsed.observed_at,
                is_simulated,
                simulation_run_id.clone(),
            );
            let raw_landing = start_raw_landing(raw_repo, &envelope).await;
            if raw_landing_required() && !raw_landing.status.persisted() {
                return Err(anyhow!(
                    "raw_landing_required_but_not_persisted status={} source_id={} stream_config_id={} error={}",
                    raw_landing.status.as_str(),
                    config.source_id,
                    config.stream_config_id,
                    raw_landing.error.clone().unwrap_or_else(|| "unknown".to_string()),
                ));
            }
            annotate_ingestion_meta(
                &mut parsed.normalized_fields,
                "realtime",
                raw_landing.status,
                raw_landing.error.as_deref(),
            );
            let record = to_operational_record(
                config,
                &parsed,
                payload_for_storage.clone(),
                dedup_key.clone(),
                parse_status,
                parse_error,
                raw_landing.pointer.clone(),
                parsed.observed_at,
                is_simulated,
                simulation_run_id.clone(),
            );
            let inserted = repo.insert_ingest_operational_event(&record).await?;
            maybe_log_test_mode_payload(
                config,
                payload_ctx.test_mode_log_state,
                &TestModePayloadLogContext {
                    inserted,
                    is_simulated,
                    simulation_run_id: simulation_run_id.as_deref(),
                    parsed: &parsed,
                    dedup_key: dedup_key.as_deref(),
                    raw_landing_status: raw_landing.status,
                    raw_landing_error: raw_landing.error.as_deref(),
                },
            );
            maybe_log_live_mode_payload(
                config,
                payload_ctx.test_mode_log_state,
                &TestModePayloadLogContext {
                    inserted,
                    is_simulated,
                    simulation_run_id: simulation_run_id.as_deref(),
                    parsed: &parsed,
                    dedup_key: dedup_key.as_deref(),
                    raw_landing_status: raw_landing.status,
                    raw_landing_error: raw_landing.error.as_deref(),
                },
            );
            if !inserted {
                debug!(
                    stream_config_id = %config.stream_config_id,
                    source_id = %config.source_id,
                    tenant_id = (config.tenant_targets.len() == 1).then(|| config.tenant_targets[0].as_str()),
                    simulation_run_id = simulation_run_id.as_deref(),
                    source_pk = payload_for_storage
                        .get("simulation")
                        .and_then(|value| value.get("source_pk"))
                        .and_then(|value| value.as_str()),
                    event_id = parsed.event_id.as_deref(),
                    dedup_key = dedup_key.as_deref(),
                    payload_event_ts = ?parsed.payload_event_ts,
                    observed_at = ?parsed.observed_at,
                    "source feed event skipped due to ingest uniqueness conflict",
                );
                return Ok(());
            }
            if should_fanout {
                let fanout_meta = UnifiedEventMeta {
                    dedup_key: dedup_key.as_deref(),
                    ingest_persisted: inserted,
                    raw_landing_status: raw_landing.status,
                    raw_landing_error: raw_landing.error.as_deref(),
                    is_simulated,
                    simulation_run_id: simulation_run_id.as_deref(),
                };
                fanout_unified_events(
                    config,
                    payload_ctx.stream,
                    &payload_for_storage,
                    &parsed,
                    fanout_meta,
                )
                .await?;
            }
            report_source_health_success(repo, config, payload_ctx.source_health_report_state)
                .await;
            Ok(())
        }
        Err(parse_error) => {
            let observed_at =
                accelerated_simulation_timestamp(&payload, payload_ctx.simulation_run_id_hint)
                    .unwrap_or_else(Utc::now);
            let dedup_key = hash_payload_only(
                config,
                &payload,
                observed_at,
                payload_ctx.simulation_run_id_hint,
            );
            let (is_simulated, simulation_run_id) =
                effective_simulation_metadata(&payload, payload_ctx.simulation_run_id_hint);
            let envelope = SourceEnvelopeV1 {
                envelope_id: Uuid::new_v4().to_string(),
                source_id: config.source_id.clone(),
                source_type: config.source_type.clone(),
                stream_id: config.stream_config_id.clone(),
                schema_version: "v1".to_string(),
                event_type: config.event_type.clone(),
                event_ts: observed_at,
                observed_at,
                partition_key: observed_at.date_naive().to_string(),
                idempotency_key: dedup_key.clone(),
                payload: payload.clone(),
                chain_id: payload_ctx.chain_id_hint,
                block_number: None,
                tx_hash: None,
                log_index: None,
                topic0: None,
                market_key: config.market_key.clone(),
                price: None,
                is_simulated,
                simulation_run_id: simulation_run_id.clone(),
            };
            let raw_landing = start_raw_landing(raw_repo, &envelope).await;
            if raw_landing_required() && !raw_landing.status.persisted() {
                return Err(anyhow!(
                    "raw_landing_required_but_not_persisted status={} source_id={} stream_config_id={} error={}",
                    raw_landing.status.as_str(),
                    config.source_id,
                    config.stream_config_id,
                    raw_landing.error.clone().unwrap_or_else(|| "unknown".to_string()),
                ));
            }
            let mut normalized_fields = json!({});
            annotate_ingestion_meta(
                &mut normalized_fields,
                "realtime",
                raw_landing.status,
                raw_landing.error.as_deref(),
            );
            let record = IngestOperationalEventRecord {
                stream_id: Some(config.stream_config_id.clone()),
                source_id: config.source_id.clone(),
                source_type: config.source_type.clone(),
                tenant_id: (config.tenant_targets.len() == 1)
                    .then(|| config.tenant_targets[0].clone()),
                event_type: config.event_type.clone(),
                event_id: None,
                market_key: config.market_key.clone(),
                asset_pair: config.asset_pair.clone(),
                chain_id: payload_ctx.chain_id_hint,
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
                normalized_fields,
                dedup_key: Some(dedup_key),
                raw_ref_type: raw_landing.pointer.as_ref().map(|p| p.raw_ref_type.clone()),
                raw_ref_id: raw_landing.pointer.as_ref().map(|p| p.raw_ref_id.clone()),
                raw_s3_uri: None,
                is_simulated,
                simulation_run_id: simulation_run_id.clone(),
            };
            let _ = repo.insert_ingest_operational_event(&record).await?;
            maybe_log_test_mode_parse_failure(
                config,
                payload_ctx.test_mode_log_state,
                is_simulated,
                simulation_run_id.as_deref(),
                record.parse_error.as_deref(),
            );
            if let Some(writer) = raw_repo {
                let _ = writer
                    .record_ingest_failure(&IngestFailureRecord {
                        stream_id: Some(config.stream_config_id.clone()),
                        source_id: config.source_id.clone(),
                        source_type: config.source_type.clone(),
                        event_type: Some(config.event_type.clone()),
                        payload_excerpt: record.payload.clone(),
                        error_kind: "parse".to_string(),
                        error_message: record
                            .parse_error
                            .clone()
                            .unwrap_or_else(|| "unknown_parse_error".to_string()),
                        retryable: false,
                        observed_at,
                    })
                    .await;
            }
            report_source_health_success(repo, config, payload_ctx.source_health_report_state)
                .await;
            Ok(())
        }
    }
}

fn should_skip_source_health_updates(config: &RuntimeStreamConfig) -> bool {
    config.operating_mode_profile != "live" || config.tenant_targets.is_empty()
}

fn should_report_source_health_success(state: &SourceHealthReportState) -> bool {
    if state.last_reported_healthy != Some(true) {
        return true;
    }

    state
        .last_success_report_at
        .map(|reported_at| {
            reported_at.elapsed() >= Duration::from_secs(SOURCE_HEALTH_SUCCESS_REPORT_INTERVAL_SECS)
        })
        .unwrap_or(true)
}

fn should_report_source_health_failure(
    state: &SourceHealthReportState,
    failure_message: &str,
) -> bool {
    if state.last_reported_healthy != Some(false) {
        return true;
    }
    if state.last_failure_message.as_deref() != Some(failure_message) {
        return true;
    }

    state
        .last_failure_report_at
        .map(|reported_at| {
            reported_at.elapsed() >= Duration::from_secs(SOURCE_HEALTH_FAILURE_REPORT_INTERVAL_SECS)
        })
        .unwrap_or(true)
}

async fn report_source_health_success(
    repo: &PostgresRepository,
    config: &RuntimeStreamConfig,
    state: &mut SourceHealthReportState,
) {
    if should_skip_source_health_updates(config) || !should_report_source_health_success(state) {
        return;
    }

    for tenant_id in &config.tenant_targets {
        if let Err(error) = repo
            .update_source_health(tenant_id, &config.source_id, true, None)
            .await
        {
            common::log_error!(
                warn,
                error,
                "failed updating live source health after payload",
                tenant_id = %tenant_id,
                source_id = %config.source_id,
                stream_config_id = %config.stream_config_id,
            );
        }
    }

    state.last_success_report_at = Some(tokio::time::Instant::now());
    state.last_failure_report_at = None;
    state.last_failure_message = None;
    state.last_reported_healthy = Some(true);
}

async fn report_source_health_failure(
    repo: &PostgresRepository,
    config: &RuntimeStreamConfig,
    state: &mut SourceHealthReportState,
    error: &anyhow::Error,
) {
    if should_skip_source_health_updates(config) {
        return;
    }

    let failure_message = error.to_string();
    if !should_report_source_health_failure(state, &failure_message) {
        return;
    }

    for tenant_id in &config.tenant_targets {
        if let Err(update_error) = repo
            .update_source_health(
                tenant_id,
                &config.source_id,
                false,
                Some(failure_message.clone()),
            )
            .await
        {
            common::log_error!(
                warn,
                update_error,
                "failed updating live source health after stream failure",
                tenant_id = %tenant_id,
                source_id = %config.source_id,
                stream_config_id = %config.stream_config_id,
            );
        }
    }

    state.last_failure_report_at = Some(tokio::time::Instant::now());
    state.last_failure_message = Some(failure_message);
    state.last_reported_healthy = Some(false);
}

fn maybe_log_test_mode_payload(
    config: &RuntimeStreamConfig,
    test_mode_log_state: &mut TestModeLogState,
    payload_log: &TestModePayloadLogContext<'_>,
) {
    if config.operating_mode_profile != "test" {
        return;
    }

    if !payload_log.is_simulated
        && payload_log.simulation_run_id.is_none()
        && !test_mode_log_state.unsimulated_warning_emitted
    {
        test_mode_log_state.unsimulated_warning_emitted = true;
        warn!(
            stream_config_id = %config.stream_config_id,
            source_id = %config.source_id,
            stream_name = %config.stream_name,
            connector_mode = %config.connector_mode,
            "test-mode worker received payload without simulation metadata",
        );
    }

    if test_mode_log_state.payload_logs_emitted >= 5 {
        return;
    }
    test_mode_log_state.payload_logs_emitted += 1;
    info!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        stream_name = %config.stream_name,
        connector_mode = %config.connector_mode,
        simulation_run_id = payload_log.simulation_run_id,
        is_simulated = payload_log.is_simulated,
        event_type = %payload_log.parsed.event_type,
        event_id = payload_log.parsed.event_id.as_deref(),
        market_key = payload_log.parsed.market_key.as_deref(),
        asset_pair = payload_log.parsed.asset_pair.as_deref(),
        payload_event_ts = ?payload_log.parsed.payload_event_ts,
        observed_at = ?payload_log.parsed.observed_at,
        dedup_key = payload_log.dedup_key,
        ingest_inserted = payload_log.inserted,
        raw_landing_status = payload_log.raw_landing_status.as_str(),
        raw_landing_error = payload_log.raw_landing_error,
        "test-mode payload observed by indexer",
    );
}

fn maybe_log_live_mode_payload(
    config: &RuntimeStreamConfig,
    test_mode_log_state: &mut TestModeLogState,
    payload_log: &TestModePayloadLogContext<'_>,
) {
    if config.operating_mode_profile != "live" {
        return;
    }

    if test_mode_log_state.payload_logs_emitted >= 5 {
        return;
    }
    test_mode_log_state.payload_logs_emitted += 1;
    info!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        stream_name = %config.stream_name,
        connector_mode = %config.connector_mode,
        tenant_target_count = config.tenant_targets.len(),
        event_type = %payload_log.parsed.event_type,
        event_id = payload_log.parsed.event_id.as_deref(),
        market_key = payload_log.parsed.market_key.as_deref(),
        asset_pair = payload_log.parsed.asset_pair.as_deref(),
        payload_event_ts = ?payload_log.parsed.payload_event_ts,
        observed_at = ?payload_log.parsed.observed_at,
        dedup_key = payload_log.dedup_key,
        ingest_inserted = payload_log.inserted,
        raw_landing_status = payload_log.raw_landing_status.as_str(),
        raw_landing_error = payload_log.raw_landing_error,
        "live-mode payload observed by indexer",
    );
}

fn maybe_log_test_mode_parse_failure(
    config: &RuntimeStreamConfig,
    test_mode_log_state: &mut TestModeLogState,
    is_simulated: bool,
    simulation_run_id: Option<&str>,
    parse_error: Option<&str>,
) {
    if config.operating_mode_profile != "test" {
        return;
    }
    if test_mode_log_state.payload_logs_emitted >= 5 {
        return;
    }
    test_mode_log_state.payload_logs_emitted += 1;
    warn!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        stream_name = %config.stream_name,
        connector_mode = %config.connector_mode,
        simulation_run_id,
        is_simulated,
        parse_error,
        "test-mode payload failed to parse in indexer",
    );
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

    normalized.insert(
        "raw_quote_price".to_string(),
        raw_quote_price.map_or(Value::Null, |v| json!(v)),
    );
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
        obj.insert(
            "raw_quote_price".to_string(),
            raw_quote_price.map_or(Value::Null, |v| json!(v)),
        );
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

fn fallback_usdt_usd_rate(parsed: &ParsedFeedEvent, quote_asset: Option<&str>) -> Option<f64> {
    if quote_asset != Some("USDT") {
        return None;
    }

    let market_key = parsed.market_key.as_deref()?.trim().to_ascii_uppercase();
    if market_key == FX_LOOKUP_MARKET_KEY {
        return None;
    }
    if !market_key.ends_with("/USD") {
        return None;
    }

    Some(1.0)
}

async fn lookup_usdt_usd_rate(
    repo: &PostgresRepository,
    fx_cache: &mut FxRateCache,
) -> Result<Option<f64>> {
    let now_ms = Utc::now().timestamp_millis();
    if let Some(cached) = fx_cache.usdt_usd.as_ref() {
        if now_ms - cached.cached_at_ms <= FX_CACHE_TTL_SECONDS * 1_000 {
            return Ok(Some(cached.rate));
        }
    }

    let latest = repo
        .latest_operational_market_price(FX_LOOKUP_MARKET_KEY, FX_LOOKUP_FRESHNESS_SECONDS)
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
        upsert_normalized_metadata(
            parsed,
            Some(raw_price),
            quote_asset_ref,
            Some(raw_price),
            None,
            false,
        );
        upsert_payload_normalization(
            payload,
            Some(raw_price),
            quote_asset_ref,
            Some(raw_price),
            None,
            false,
        );
        return Ok(("parsed", None, true));
    }

    let fx_rate = lookup_usdt_usd_rate(repo, fx_cache).await?;
    let fx_rate = fx_rate.or_else(|| fallback_usdt_usd_rate(parsed, quote_asset_ref));
    let Some(fx_rate) = fx_rate else {
        parsed.price = None;
        upsert_normalized_metadata(parsed, Some(raw_price), quote_asset_ref, None, None, true);
        upsert_payload_normalization(payload, Some(raw_price), quote_asset_ref, None, None, true);
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

fn compute_median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

fn build_market_truth_snapshot(
    market_key: &str,
    source_prices: &std::collections::HashMap<String, f64>,
) -> Option<MarketTruthSnapshot> {
    if source_prices.is_empty() {
        return None;
    }

    let mut values: Vec<f64> = source_prices
        .values()
        .copied()
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    let source_count = values.len();
    let median_price = compute_median(&mut values)?;
    let deviation_pct =
        ((median_price - MARKET_TRUTH_PEG_TARGET).abs() / MARKET_TRUTH_PEG_TARGET) * 100.0;

    Some(MarketTruthSnapshot {
        market_key: market_key.to_string(),
        median_price,
        deviation_pct,
        source_count,
        peg_target: MARKET_TRUTH_PEG_TARGET,
    })
}

fn upsert_market_truth_payload(payload: &mut Value, snapshot: &MarketTruthSnapshot) {
    ensure_object_payload(payload);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("median_price".to_string(), json!(snapshot.median_price));
        obj.insert("deviation_pct".to_string(), json!(snapshot.deviation_pct));
        obj.insert(
            "true_price_median".to_string(),
            json!(snapshot.median_price),
        );
        obj.insert(
            "true_price_deviation_pct".to_string(),
            json!(snapshot.deviation_pct),
        );
        obj.insert(
            "true_price_peg_target".to_string(),
            json!(snapshot.peg_target),
        );
        obj.insert(
            "true_price_source_count".to_string(),
            json!(snapshot.source_count),
        );
        obj.insert(
            "true_price_scope".to_string(),
            Value::String("platform_global".to_string()),
        );
        obj.insert(
            "true_price_market_key".to_string(),
            Value::String(snapshot.market_key.clone()),
        );
        obj.insert(
            "platform_context".to_string(),
            json!({
                "market_truth": {
                    "market_key": snapshot.market_key.clone(),
                    "median_price": snapshot.median_price,
                    "deviation_pct": snapshot.deviation_pct,
                    "peg_target": snapshot.peg_target,
                    "source_count": snapshot.source_count,
                    "scope": "platform_global",
                }
            }),
        );
    }
}

fn upsert_market_truth_normalized_fields(
    parsed: &mut ParsedFeedEvent,
    snapshot: &MarketTruthSnapshot,
) {
    let mut normalized = parsed
        .normalized_fields
        .as_object()
        .cloned()
        .unwrap_or_default();
    normalized.insert("median_price".to_string(), json!(snapshot.median_price));
    normalized.insert("deviation_pct".to_string(), json!(snapshot.deviation_pct));
    normalized.insert(
        "true_price_median".to_string(),
        json!(snapshot.median_price),
    );
    normalized.insert(
        "true_price_deviation_pct".to_string(),
        json!(snapshot.deviation_pct),
    );
    normalized.insert(
        "true_price_peg_target".to_string(),
        json!(snapshot.peg_target),
    );
    normalized.insert(
        "true_price_source_count".to_string(),
        json!(snapshot.source_count),
    );
    normalized.insert(
        "true_price_scope".to_string(),
        Value::String("platform_global".to_string()),
    );
    normalized.insert(
        "true_price_market_key".to_string(),
        Value::String(snapshot.market_key.clone()),
    );
    parsed.normalized_fields = Value::Object(normalized);
}

async fn apply_market_truth_context(
    repo: &PostgresRepository,
    config: &RuntimeStreamConfig,
    parsed: &mut ParsedFeedEvent,
    payload: &mut Value,
) -> Result<()> {
    let Some(market_key) = parsed.market_key.as_deref() else {
        return Ok(());
    };
    let Some(current_price) = parsed
        .price
        .filter(|value| value.is_finite() && *value > 0.0)
    else {
        return Ok(());
    };

    let latest = repo
        .latest_operational_source_prices(market_key, MARKET_TRUTH_FRESHNESS_SECONDS)
        .await?;
    let mut source_prices = std::collections::HashMap::<String, f64>::new();
    for item in latest {
        if item.price.is_finite() && item.price > 0.0 {
            source_prices.insert(item.source_id, item.price);
        }
    }

    // Ensure the current event participates even before it is persisted.
    source_prices.insert(config.source_id.clone(), current_price);

    if let Some(snapshot) = build_market_truth_snapshot(market_key, &source_prices) {
        upsert_market_truth_normalized_fields(parsed, &snapshot);
        upsert_market_truth_payload(payload, &snapshot);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn to_operational_record(
    config: &RuntimeStreamConfig,
    parsed: &ParsedFeedEvent,
    payload: Value,
    dedup_key: Option<String>,
    parse_status: &str,
    parse_error: Option<String>,
    raw_pointer: Option<RawRecordPointer>,
    observed_at: chrono::DateTime<Utc>,
    is_simulated: bool,
    simulation_run_id: Option<String>,
) -> IngestOperationalEventRecord {
    IngestOperationalEventRecord {
        stream_id: Some(config.stream_config_id.clone()),
        source_id: config.source_id.clone(),
        source_type: config.source_type.clone(),
        tenant_id: (config.tenant_targets.len() == 1).then(|| config.tenant_targets[0].clone()),
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
        observed_at,
        parse_status: parse_status.to_string(),
        parse_error,
        payload,
        normalized_fields: parsed.normalized_fields.clone(),
        dedup_key,
        raw_ref_type: raw_pointer
            .as_ref()
            .map(|pointer| pointer.raw_ref_type.clone()),
        raw_ref_id: raw_pointer
            .as_ref()
            .map(|pointer| pointer.raw_ref_id.clone()),
        raw_s3_uri: None,
        is_simulated,
        simulation_run_id,
    }
}

fn annotate_ingestion_meta(
    normalized_fields: &mut Value,
    processing_mode: &str,
    raw_landing_status: RawLandingStatus,
    raw_landing_error: Option<&str>,
) {
    if !normalized_fields.is_object() {
        *normalized_fields = json!({});
    }
    if let Some(obj) = normalized_fields.as_object_mut() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "processing_mode".to_string(),
            Value::String(processing_mode.to_string()),
        );
        meta.insert(
            "raw_landing_status".to_string(),
            Value::String(raw_landing_status.as_str().to_string()),
        );
        meta.insert(
            "raw_persisted".to_string(),
            Value::Bool(raw_landing_status.persisted()),
        );
        if let Some(error) = raw_landing_error {
            if !error.trim().is_empty() {
                meta.insert(
                    "raw_landing_error".to_string(),
                    Value::String(error.to_string()),
                );
            }
        }
        obj.insert("ingestion_meta".to_string(), Value::Object(meta));
    }
}

fn to_source_envelope(
    config: &RuntimeStreamConfig,
    parsed: &ParsedFeedEvent,
    payload: Value,
    dedup_key: Option<&str>,
    observed_at: chrono::DateTime<Utc>,
    is_simulated: bool,
    simulation_run_id: Option<String>,
) -> SourceEnvelopeV1 {
    SourceEnvelopeV1 {
        envelope_id: Uuid::new_v4().to_string(),
        source_id: config.source_id.clone(),
        source_type: config.source_type.clone(),
        stream_id: config.stream_config_id.clone(),
        schema_version: "v1".to_string(),
        event_type: parsed.event_type.clone(),
        event_ts: parsed.payload_event_ts.unwrap_or(observed_at),
        observed_at,
        partition_key: observed_at.date_naive().to_string(),
        idempotency_key: dedup_key
            .map(ToString::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        payload,
        chain_id: parsed.chain_id,
        block_number: parsed.block_number,
        tx_hash: parsed.tx_hash.clone(),
        log_index: parsed.log_index,
        topic0: parsed.topic0.clone(),
        market_key: parsed.market_key.clone(),
        price: parsed.price,
        is_simulated,
        simulation_run_id,
    }
}

async fn fanout_unified_events(
    config: &RuntimeStreamConfig,
    stream: &RedisStreamPublisher,
    payload: &Value,
    parsed: &ParsedFeedEvent,
    meta: UnifiedEventMeta<'_>,
) -> Result<()> {
    let source_type = map_source_type(&config.source_type);

    for tenant_id in &config.tenant_targets {
        let event_id = parsed
            .event_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let enriched_payload = enrich_payload_for_unified(config, payload, parsed, &meta);
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
    meta: &UnifiedEventMeta<'_>,
) -> Value {
    let mut enriched = payload.clone();
    if !enriched.is_object() {
        enriched = json!({ "raw_payload": payload });
    }
    if let Some(obj) = enriched.as_object_mut() {
        if meta.is_simulated {
            obj.insert("is_simulated".to_string(), Value::Bool(true));
            let simulation = obj
                .entry("simulation".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !simulation.is_object() {
                *simulation = Value::Object(serde_json::Map::new());
            }
            if let Some(simulation_meta) = simulation.as_object_mut() {
                simulation_meta.insert("is_simulated".to_string(), Value::Bool(true));
                if let Some(run_id) = meta.simulation_run_id {
                    simulation_meta.insert("run_id".to_string(), Value::String(run_id.to_string()));
                    simulation_meta.insert(
                        "simulation_run_id".to_string(),
                        Value::String(run_id.to_string()),
                    );
                }
            }
        }
        obj.insert(
            "raw_persisted".to_string(),
            Value::Bool(meta.raw_landing_status.persisted()),
        );
        obj.insert(
            "ingest_persisted".to_string(),
            Value::Bool(meta.ingest_persisted),
        );
        obj.insert(
            "raw_landing_status".to_string(),
            Value::String(meta.raw_landing_status.as_str().to_string()),
        );
        if let Some(error) = meta.raw_landing_error {
            if !error.trim().is_empty() {
                obj.insert(
                    "raw_landing_error".to_string(),
                    Value::String(error.to_string()),
                );
            }
        }
        obj.insert(
            "processing_mode".to_string(),
            Value::String("realtime".to_string()),
        );
        obj.insert(
            "stream_config_id".to_string(),
            Value::String(config.stream_config_id.clone()),
        );
        obj.insert(
            "source_id".to_string(),
            Value::String(config.source_id.clone()),
        );
        obj.insert(
            "parser_name".to_string(),
            Value::String(config.parser_name.clone()),
        );
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
        if let Some(dedup_key) = meta.dedup_key {
            obj.insert(
                "dedup_key".to_string(),
                Value::String(dedup_key.to_string()),
            );
        }
    }
    enriched
}

fn endpoint_from_runtime_config(config: &RuntimeStreamConfig) -> Result<String> {
    let endpoint_template = endpoint_from_connection_config(&config.connection_config)?;
    resolve_endpoint_template(
        &endpoint_template,
        &config.auth_config,
        config.auth_secret_ref.as_deref(),
    )
}

fn endpoint_log_context(endpoint: &str) -> EndpointLogContext {
    let Ok(parsed) = Url::parse(endpoint) else {
        return EndpointLogContext {
            host: None,
            path: endpoint.to_string(),
            is_mock_endpoint: endpoint.contains("/api/simulation/mock/"),
            tenant_id: None,
            stream_name: None,
            simulation_run_id: None,
        };
    };

    let mut context = EndpointLogContext {
        host: parsed.host_str().map(ToOwned::to_owned),
        path: parsed.path().to_string(),
        is_mock_endpoint: parsed.path().contains("/api/simulation/mock/"),
        tenant_id: None,
        stream_name: None,
        simulation_run_id: None,
    };

    for (key, value) in parsed.query_pairs() {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.as_ref() {
            "tenant_id" => context.tenant_id = Some(value.to_string()),
            "stream_name" => context.stream_name = Some(value.to_string()),
            "simulation_run_id" => context.simulation_run_id = Some(value.to_string()),
            _ => {}
        }
    }

    context
}

fn log_test_mode_connector_endpoint(
    config: &RuntimeStreamConfig,
    endpoint: &str,
) -> EndpointLogContext {
    let context = endpoint_log_context(endpoint);
    if config.operating_mode_profile != "test" {
        return context;
    }

    info!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        stream_name = %config.stream_name,
        connector_mode = %config.connector_mode,
        endpoint_host = context.host.as_deref(),
        endpoint_path = %context.path,
        is_mock_endpoint = context.is_mock_endpoint,
        endpoint_tenant_id = context.tenant_id.as_deref(),
        endpoint_stream_name = context.stream_name.as_deref(),
        endpoint_simulation_run_id = context.simulation_run_id.as_deref(),
        "resolved test-mode stream endpoint",
    );

    if !context.is_mock_endpoint {
        warn!(
            stream_config_id = %config.stream_config_id,
            source_id = %config.source_id,
            stream_name = %config.stream_name,
            connector_mode = %config.connector_mode,
            endpoint_host = context.host.as_deref(),
            endpoint_path = %context.path,
            "test-mode stream resolved to a non-mock endpoint",
        );
    } else if context.tenant_id.is_none() || context.stream_name.is_none() {
        warn!(
            stream_config_id = %config.stream_config_id,
            source_id = %config.source_id,
            stream_name = %config.stream_name,
            connector_mode = %config.connector_mode,
            endpoint_host = context.host.as_deref(),
            endpoint_path = %context.path,
            endpoint_tenant_id = context.tenant_id.as_deref(),
            endpoint_stream_name = context.stream_name.as_deref(),
            endpoint_simulation_run_id = context.simulation_run_id.as_deref(),
            "test-mode mock endpoint is missing tenant-scoped routing parameters",
        );
    }

    context
}

fn log_test_mode_connector_connected(
    config: &RuntimeStreamConfig,
    endpoint_context: &EndpointLogContext,
) {
    if config.operating_mode_profile != "test" {
        return;
    }

    info!(
        stream_config_id = %config.stream_config_id,
        source_id = %config.source_id,
        stream_name = %config.stream_name,
        connector_mode = %config.connector_mode,
        endpoint_host = endpoint_context.host.as_deref(),
        endpoint_path = %endpoint_context.path,
        is_mock_endpoint = endpoint_context.is_mock_endpoint,
        endpoint_tenant_id = endpoint_context.tenant_id.as_deref(),
        endpoint_stream_name = endpoint_context.stream_name.as_deref(),
        endpoint_simulation_run_id = endpoint_context.simulation_run_id.as_deref(),
        "connected test-mode stream connector",
    );
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
        if let Some(value) = extract_alchemy_key(auth_config).or_else(load_alchemy_key_from_env) {
            endpoint = endpoint.replace("YOUR_ALCHEMY_KEY", &value);
        } else {
            return Err(anyhow!(
                "missing auth_config.alchemy_api_key (or auth_config.api_key) for endpoint template"
            ));
        }
    }

    for placeholder in collect_unresolved_placeholders(&endpoint) {
        if placeholder == "subscription_key" {
            continue;
        }
        if let Some(value) = resolve_placeholder_from_env(&placeholder) {
            endpoint = endpoint.replace(&format!("{{{placeholder}}}"), &value);
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

fn load_alchemy_key_from_env() -> Option<String> {
    for key in ["ALCHEMY_API_KEY", "INDEXER_ALCHEMY_API_KEY"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn resolve_placeholder_from_env(placeholder: &str) -> Option<String> {
    let normalized = placeholder
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();

    let candidates = [normalized.clone(), format!("INDEXER_{normalized}")];
    for key in candidates {
        if let Ok(value) = std::env::var(&key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
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
            if key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
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

fn build_dedup_key(
    config: &RuntimeStreamConfig,
    parsed: &ParsedFeedEvent,
    payload: &Value,
    simulation_run_id_hint: Option<&str>,
) -> Option<String> {
    let (is_simulated, simulation_run_id) =
        effective_simulation_metadata(payload, simulation_run_id_hint);
    let payload_tx_hash = payload
        .get("transactionHash")
        .or_else(|| payload.get("transaction_hash"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let payload_log_index = payload
        .get("logIndex")
        .or_else(|| payload.get("log_index"))
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(raw) => {
                let token = raw.trim();
                if token.is_empty() {
                    None
                } else if let Some(hex) = token.strip_prefix("0x") {
                    i64::from_str_radix(hex, 16).ok()
                } else {
                    token.parse::<i64>().ok()
                }
            }
            _ => None,
        });
    let tx_hash = parsed
        .tx_hash
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(payload_tx_hash);
    let log_index = parsed.log_index.or(payload_log_index);
    if let Some(tx_hash) = tx_hash {
        let mut provider_key = format!(
            "provider:{}:{}:{}:{}",
            config.source_id,
            config.stream_config_id,
            tx_hash,
            log_index.unwrap_or_default()
        );
        if let Some(payload_event_ts) = parsed.payload_event_ts {
            provider_key.push_str(":ts:");
            provider_key.push_str(&payload_event_ts.timestamp_millis().to_string());
        }
        if is_simulated {
            if let Some(run_id) = simulation_run_id.as_deref() {
                provider_key.push_str(":sim:");
                provider_key.push_str(run_id);
            }
        }
        return Some(provider_key);
    }
    if let Some(event_id) = parsed.event_id.as_ref() {
        let mut provider_key = format!(
            "provider:{}:{}:{}",
            config.source_id, config.stream_config_id, event_id
        );
        if let Some(payload_event_ts) = parsed.payload_event_ts {
            provider_key.push_str(":ts:");
            provider_key.push_str(&payload_event_ts.timestamp_millis().to_string());
        }
        if is_simulated {
            if let Some(run_id) = simulation_run_id.as_deref() {
                provider_key.push_str(":sim:");
                provider_key.push_str(run_id);
            }
        }
        return Some(provider_key);
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
    if is_simulated {
        hasher.update(simulation_run_id.as_deref().unwrap_or_default().as_bytes());
    }
    hasher.update(b"|");
    hasher.update(payload.to_string().as_bytes());
    Some(hex::encode(hasher.finalize()))
}

fn hash_payload_only(
    config: &RuntimeStreamConfig,
    payload: &Value,
    observed_at: chrono::DateTime<Utc>,
    simulation_run_id_hint: Option<&str>,
) -> String {
    let (is_simulated, simulation_run_id) =
        effective_simulation_metadata(payload, simulation_run_id_hint);
    let mut hasher = Sha256::new();
    hasher.update(config.source_id.as_bytes());
    hasher.update(b"|");
    hasher.update(config.stream_config_id.as_bytes());
    hasher.update(b"|");
    hasher.update(config.event_type.as_bytes());
    hasher.update(b"|");
    hasher.update(observed_at.timestamp_millis().to_string().as_bytes());
    hasher.update(b"|");
    if is_simulated {
        hasher.update(simulation_run_id.as_deref().unwrap_or_default().as_bytes());
    }
    hasher.update(b"|");
    hasher.update(payload.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

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

    #[test]
    fn build_dedup_key_is_scoped_by_simulation_run() {
        let config = RuntimeStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "binance-global".to_string(),
            source_type: "cex_api".to_string(),
            source_name: "Binance".to_string(),
            connection_config: json!({"endpoint": "http://example.invalid"}),
            operating_mode_profile: "test".to_string(),
            auth_secret_ref: None,
            auth_config: json!({}),
            connector_mode: "http_poll".to_string(),
            stream_name: "ticker-usdc-usd".to_string(),
            subscription_key: None,
            event_type: "quote".to_string(),
            parser_name: "binance_miniticker_v1".to_string(),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSDT".to_string()),
            filter_config: json!({}),
            poll_interval_ms: Some(5000),
            payload_ts_path: Some("$.E".to_string()),
            payload_ts_unit: "ms".to_string(),
            tenant_targets: vec!["raksha-demo".to_string()],
        };
        let parsed = ParsedFeedEvent {
            event_type: "quote".to_string(),
            event_id: None,
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSDT".to_string()),
            price: Some(0.88),
            chain_id: None,
            block_number: None,
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: None,
            observed_at: Utc.timestamp_opt(1_678_521_640, 0).single().unwrap(),
            normalized_fields: json!({}),
        };
        let payload_a = json!({
            "s": "USDCUSDT",
            "c": "0.880000",
            "E": 1678521640000_i64,
            "simulation": {
                "is_simulated": true,
                "run_id": "run-a"
            }
        });
        let payload_b = json!({
            "s": "USDCUSDT",
            "c": "0.880000",
            "E": 1678521640000_i64,
            "simulation": {
                "is_simulated": true,
                "run_id": "run-b"
            }
        });

        let key_a = build_dedup_key(&config, &parsed, &payload_a, None).expect("key");
        let key_b = build_dedup_key(&config, &parsed, &payload_b, None).expect("key");
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn build_dedup_key_provider_event_ids_are_scoped_by_simulation_run() {
        let config = RuntimeStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "pyth-eth-mainnet".to_string(),
            source_type: "oracle_api".to_string(),
            source_name: "Pyth".to_string(),
            connection_config: json!({"endpoint": "http://example.invalid"}),
            operating_mode_profile: "test".to_string(),
            auth_secret_ref: None,
            auth_config: json!({}),
            connector_mode: "rpc_logs".to_string(),
            stream_name: "usdc-usd-feed".to_string(),
            subscription_key: None,
            event_type: "oracle_update".to_string(),
            parser_name: "pyth_price_feed_v1".to_string(),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSD".to_string()),
            filter_config: json!({}),
            poll_interval_ms: Some(5000),
            payload_ts_path: Some("$.observed_at".to_string()),
            payload_ts_unit: "ms".to_string(),
            tenant_targets: vec!["raksha-demo".to_string()],
        };
        let parsed = ParsedFeedEvent {
            event_type: "oracle_update".to_string(),
            event_id: Some("oracle-event-1".to_string()),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSD".to_string()),
            price: Some(0.88),
            chain_id: Some(1),
            block_number: Some(123),
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: Some(Utc.timestamp_opt(1_678_521_640, 0).single().unwrap()),
            observed_at: Utc.timestamp_opt(1_678_521_640, 0).single().unwrap(),
            normalized_fields: json!({}),
        };
        let payload_a = json!({"simulation": {"is_simulated": true, "run_id": "run-a"}});
        let payload_b = json!({"simulation": {"is_simulated": true, "run_id": "run-b"}});

        let key_a = build_dedup_key(&config, &parsed, &payload_a, None).expect("key");
        let key_b = build_dedup_key(&config, &parsed, &payload_b, None).expect("key");
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn hash_payload_only_is_scoped_by_simulation_run() {
        let config = RuntimeStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "custom-source".to_string(),
            source_type: "custom_api".to_string(),
            source_name: "Custom".to_string(),
            connection_config: json!({"endpoint": "http://example.invalid"}),
            operating_mode_profile: "test".to_string(),
            auth_secret_ref: None,
            auth_config: json!({}),
            connector_mode: "http_poll".to_string(),
            stream_name: "custom-stream".to_string(),
            subscription_key: None,
            event_type: "custom_event".to_string(),
            parser_name: "custom".to_string(),
            market_key: None,
            asset_pair: None,
            filter_config: json!({}),
            poll_interval_ms: Some(5000),
            payload_ts_path: None,
            payload_ts_unit: "ms".to_string(),
            tenant_targets: vec!["raksha-demo".to_string()],
        };
        let observed_at = Utc.timestamp_opt(1_678_521_640, 0).single().unwrap();
        let payload_a = json!({"simulation": {"is_simulated": true, "run_id": "run-a"}});
        let payload_b = json!({"simulation": {"is_simulated": true, "run_id": "run-b"}});

        let key_a = hash_payload_only(&config, &payload_a, observed_at, None);
        let key_b = hash_payload_only(&config, &payload_b, observed_at, None);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn build_dedup_key_uses_endpoint_simulation_run_hint() {
        let config = RuntimeStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "pyth-eth-mainnet".to_string(),
            source_type: "oracle_api".to_string(),
            source_name: "Pyth".to_string(),
            connection_config: json!({"endpoint": "http://example.invalid"}),
            operating_mode_profile: "test".to_string(),
            auth_secret_ref: None,
            auth_config: json!({}),
            connector_mode: "rpc_logs".to_string(),
            stream_name: "usdc-usd-feed".to_string(),
            subscription_key: None,
            event_type: "oracle_update".to_string(),
            parser_name: "pyth_price_feed_v1".to_string(),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSD".to_string()),
            filter_config: json!({}),
            poll_interval_ms: Some(5000),
            payload_ts_path: Some("$.observed_at".to_string()),
            payload_ts_unit: "ms".to_string(),
            tenant_targets: vec!["raksha-demo".to_string()],
        };
        let parsed = ParsedFeedEvent {
            event_type: "oracle_update".to_string(),
            event_id: Some("oracle-event-1".to_string()),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSD".to_string()),
            price: Some(0.88),
            chain_id: Some(1),
            block_number: Some(123),
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: Some(Utc.timestamp_opt(1_678_521_640, 0).single().unwrap()),
            observed_at: Utc.timestamp_opt(1_678_521_640, 0).single().unwrap(),
            normalized_fields: json!({}),
        };
        let payload = json!({"price": "0.88"});

        let key_a = build_dedup_key(&config, &parsed, &payload, Some("run-a")).expect("key");
        let key_b = build_dedup_key(&config, &parsed, &payload, Some("run-b")).expect("key");
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn build_dedup_key_uses_payload_transaction_hash_when_parser_does_not_surface_it() {
        let config = RuntimeStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "pyth-eth-mainnet".to_string(),
            source_type: "oracle_api".to_string(),
            source_name: "Pyth".to_string(),
            connection_config: json!({"endpoint": "http://example.invalid"}),
            operating_mode_profile: "test".to_string(),
            auth_secret_ref: None,
            auth_config: json!({}),
            connector_mode: "rpc_logs".to_string(),
            stream_name: "usdc-usd-feed".to_string(),
            subscription_key: None,
            event_type: "oracle_update".to_string(),
            parser_name: "pyth_price_feed_v1".to_string(),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSD".to_string()),
            filter_config: json!({}),
            poll_interval_ms: Some(5000),
            payload_ts_path: Some("$.observed_at".to_string()),
            payload_ts_unit: "ms".to_string(),
            tenant_targets: vec!["raksha-demo".to_string()],
        };
        let parsed = ParsedFeedEvent {
            event_type: "oracle_update".to_string(),
            event_id: Some("oracle-event-1".to_string()),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSD".to_string()),
            price: Some(0.88),
            chain_id: Some(1),
            block_number: Some(123),
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: Some(Utc.timestamp_opt(1_678_521_640, 0).single().unwrap()),
            observed_at: Utc.timestamp_opt(1_678_521_640, 0).single().unwrap(),
            normalized_fields: json!({}),
        };
        let payload = json!({
            "transactionHash": "0xabc123",
            "logIndex": "0x7",
        });

        let key = build_dedup_key(&config, &parsed, &payload, None).expect("key");
        assert!(key.contains("0xabc123"));
        assert!(key.contains(":7:"));
    }

    #[test]
    fn hash_payload_only_uses_endpoint_simulation_run_hint() {
        let config = RuntimeStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "custom-source".to_string(),
            source_type: "custom_api".to_string(),
            source_name: "Custom".to_string(),
            connection_config: json!({"endpoint": "http://example.invalid"}),
            operating_mode_profile: "test".to_string(),
            auth_secret_ref: None,
            auth_config: json!({}),
            connector_mode: "http_poll".to_string(),
            stream_name: "custom-stream".to_string(),
            subscription_key: None,
            event_type: "custom_event".to_string(),
            parser_name: "custom".to_string(),
            market_key: None,
            asset_pair: None,
            filter_config: json!({}),
            poll_interval_ms: Some(5000),
            payload_ts_path: None,
            payload_ts_unit: "ms".to_string(),
            tenant_targets: vec!["raksha-demo".to_string()],
        };
        let observed_at = Utc.timestamp_opt(1_678_521_640, 0).single().unwrap();
        let payload = json!({"raw": true});

        let key_a = hash_payload_only(&config, &payload, observed_at, Some("run-a"));
        let key_b = hash_payload_only(&config, &payload, observed_at, Some("run-b"));
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn fallback_usdt_usd_rate_assumes_parity_for_non_usdt_markets() {
        let parsed = ParsedFeedEvent {
            event_type: "quote".to_string(),
            event_id: None,
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSDT".to_string()),
            price: Some(0.88),
            chain_id: None,
            block_number: None,
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: None,
            observed_at: Utc.timestamp_opt(1_678_521_640, 0).single().unwrap(),
            normalized_fields: json!({}),
        };

        assert_eq!(fallback_usdt_usd_rate(&parsed, Some("USDT")), Some(1.0));
    }

    #[test]
    fn fallback_usdt_usd_rate_does_not_mask_usdt_market() {
        let parsed = ParsedFeedEvent {
            event_type: "quote".to_string(),
            event_id: None,
            market_key: Some("USDT/USD".to_string()),
            asset_pair: Some("USDTUSDC".to_string()),
            price: Some(0.88),
            chain_id: None,
            block_number: None,
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: None,
            observed_at: Utc.timestamp_opt(1_678_521_640, 0).single().unwrap(),
            normalized_fields: json!({}),
        };

        assert_eq!(fallback_usdt_usd_rate(&parsed, Some("USDT")), None);
    }

    #[test]
    fn enrich_payload_for_unified_preserves_simulation_metadata_from_endpoint_hint() {
        let config = RuntimeStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "binance-global".to_string(),
            source_type: "cex_api".to_string(),
            source_name: "Binance".to_string(),
            connection_config: json!({"endpoint": "http://example.invalid"}),
            operating_mode_profile: "test".to_string(),
            auth_secret_ref: None,
            auth_config: json!({}),
            connector_mode: "websocket".to_string(),
            stream_name: "ticker-usdc-usd".to_string(),
            subscription_key: Some("usdcusdt@miniticker".to_string()),
            event_type: "quote".to_string(),
            parser_name: "binance_miniticker_v1".to_string(),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSDT".to_string()),
            filter_config: json!({}),
            payload_ts_path: Some("$.E".to_string()),
            payload_ts_unit: "ms".to_string(),
            poll_interval_ms: Some(200),
            tenant_targets: vec!["raksha-demo".to_string()],
        };
        let parsed = ParsedFeedEvent {
            event_type: "quote".to_string(),
            event_id: Some("quote-1".to_string()),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSDT".to_string()),
            price: Some(0.95),
            chain_id: None,
            block_number: None,
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: Some(Utc.timestamp_opt(1_678_521_760, 0).single().unwrap()),
            observed_at: Utc.timestamp_opt(1_678_521_760, 0).single().unwrap(),
            normalized_fields: json!({}),
        };
        let payload = json!({
            "e": "24hrMiniTicker",
            "E": 1678521760000_i64,
            "s": "USDCUSDT",
            "c": "0.950000"
        });

        let enriched = enrich_payload_for_unified(
            &config,
            &payload,
            &parsed,
            &UnifiedEventMeta {
                dedup_key: Some("dedup-1"),
                ingest_persisted: true,
                raw_landing_status: RawLandingStatus::Deferred,
                raw_landing_error: None,
                is_simulated: true,
                simulation_run_id: Some("run-test-1"),
            },
        );

        assert_eq!(
            enriched.get("is_simulated").and_then(Value::as_bool),
            Some(true)
        );
        let simulation = enriched
            .get("simulation")
            .and_then(Value::as_object)
            .expect("simulation metadata");
        assert_eq!(
            simulation.get("is_simulated").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            simulation.get("run_id").and_then(Value::as_str),
            Some("run-test-1")
        );
        assert_eq!(
            simulation.get("simulation_run_id").and_then(Value::as_str),
            Some("run-test-1")
        );
    }

    #[test]
    fn apply_accelerated_simulation_timestamp_uses_replay_time_for_fast_runs() {
        let replay_ts = Utc.timestamp_opt(1_678_521_760, 0).single().unwrap();
        let mut parsed = ParsedFeedEvent {
            event_type: "quote".to_string(),
            event_id: Some("quote-1".to_string()),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSDT".to_string()),
            price: Some(0.95),
            chain_id: None,
            block_number: None,
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: None,
            observed_at: Utc::now(),
            normalized_fields: json!({}),
        };
        let payload = json!({
            "simulation": {
                "is_simulated": true,
                "run_id": "run-fast",
                "speed_factor": 1000,
                "event_ts": replay_ts.to_rfc3339(),
            }
        });

        apply_accelerated_simulation_timestamp(&mut parsed, &payload, None);

        assert_eq!(parsed.payload_event_ts, Some(replay_ts));
        assert_eq!(parsed.observed_at, replay_ts);
    }

    #[test]
    fn apply_accelerated_simulation_timestamp_leaves_one_x_runs_unchanged() {
        let original_observed_at = Utc.timestamp_opt(1_678_521_640, 0).single().unwrap();
        let mut parsed = ParsedFeedEvent {
            event_type: "quote".to_string(),
            event_id: Some("quote-1".to_string()),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDCUSDT".to_string()),
            price: Some(0.95),
            chain_id: None,
            block_number: None,
            tx_hash: None,
            log_index: None,
            topic0: None,
            payload_event_ts: None,
            observed_at: original_observed_at,
            normalized_fields: json!({}),
        };
        let payload = json!({
            "simulation": {
                "is_simulated": true,
                "run_id": "run-live-speed",
                "speed_factor": 1,
                "event_ts": "2023-03-11T08:02:40Z",
            }
        });

        apply_accelerated_simulation_timestamp(&mut parsed, &payload, None);

        assert_eq!(parsed.payload_event_ts, None);
        assert_eq!(parsed.observed_at, original_observed_at);
    }
}
