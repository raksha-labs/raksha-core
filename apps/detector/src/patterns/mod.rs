//! Pattern trait and registry for the unified detector.
//!
//! Each pattern is responsible for processing `UnifiedEvent`s and emitting
//! `DetectionResult`s when anomalous conditions are detected.

pub mod dpeg;
pub mod flash_loan;
pub mod generic_rule;
pub mod tvl_drop;
pub mod utilization_high;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use event_schema::{DetectionResult, UnifiedEvent};
use serde_json::{Map, Value};
use state_manager::{PostgresRepository, RedisStreamPublisher};

/// Trait implemented by every detection pattern.
///
/// Patterns receive individual `UnifiedEvent`s from the stream.  They maintain
/// their own internal state (cached in memory, persisted via `repo`) and emit
/// a `DetectionResult` whenever a detection fires.
#[async_trait]
pub trait DetectionPattern: Send {
    /// Stable identifier for this pattern (matches `patterns.pattern_id` in the DB).
    fn pattern_id(&self) -> &str;

    /// Reload the per-tenant configuration from the DB.
    ///
    /// `config_map` maps `(tenant_id, pattern_id)` → JSONB config.  Each
    /// pattern extracts the keys it cares about.
    async fn reload_config(
        &mut self,
        config_map: &std::collections::HashMap<(String, String), serde_json::Value>,
    ) -> Result<()>;

    /// Inspect a single `UnifiedEvent`.
    ///
    /// Returns `Some(DetectionResult)` when the pattern fires, `None` otherwise.
    async fn process_event(
        &mut self,
        event: &UnifiedEvent,
        now: DateTime<Utc>,
        repo: &PostgresRepository,
    ) -> Result<Option<DetectionResult>>;
}

/// Extract the detection config from a composite full_config blob.
///
/// The `load_tenant_pattern_configs()` query wraps each pattern's stored JSONB
/// under `detection_config`.  This helper extracts that key so individual parse
/// functions still work when they look for their top-level fields (e.g. `rules`,
/// `policies`).  Falls back to the whole blob for backward compatibility with
/// tests and older flat configs.
pub(crate) fn extract_detection_config(config: &Value) -> &Value {
    config.get("detection_config").unwrap_or(config)
}

/// Collect the set of enabled source_ids from a composite full_config blob.
///
/// Returns `None` when no `source_bindings` key is present (meaning: no
/// binding restriction — accept events from any source).  Returns `Some(set)`
/// when bindings are explicitly configured; an empty set means all sources
/// were disabled and events from every source should be skipped.
pub(crate) fn extract_bound_source_ids(
    config: &Value,
) -> Option<std::collections::HashSet<String>> {
    let arr = config.get("source_bindings")?.as_array()?;
    let set = arr
        .iter()
        .filter(|b| b.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|b| {
            b.get("source_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    Some(set)
}

pub(crate) fn append_snapshot_meta(event: &UnifiedEvent, data: Value) -> Value {
    let mut snapshot = match data {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };

    let mut meta = match snapshot.remove("_meta") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    meta.insert(
        "event_timestamp".to_string(),
        Value::String(event.timestamp.to_rfc3339()),
    );

    if let Some(simulation) = event.payload.get("simulation").cloned() {
        meta.insert("simulation".to_string(), simulation);
    }

    snapshot.insert("_meta".to_string(), Value::Object(meta));
    Value::Object(snapshot)
}

pub(crate) fn simulation_metadata_from_event(event: &UnifiedEvent) -> (bool, Option<String>) {
    let simulation = event
        .payload
        .get("simulation")
        .and_then(|value| value.as_object());
    let run_id = simulation
        .and_then(|meta| {
            meta.get("run_id")
                .or_else(|| meta.get("simulation_run_id"))
                .and_then(|value| value.as_str())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let is_simulated = simulation
        .and_then(|meta| meta.get("is_simulated"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        || event
            .payload
            .get("is_simulated")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        || run_id.is_some();
    (is_simulated, run_id)
}

fn log_test_mode_event_received(event: &UnifiedEvent, simulation_run_id: Option<&str>) {
    tracing::info!(
        pipeline_mode = "test",
        component = "detector",
        tenant_id = %event.tenant_id,
        source_id = %event.source_id,
        event_id = %event.event_id,
        event_type = %event.event_type,
        market_key = event.market_key.as_deref(),
        tx_hash = event.tx_hash.as_deref(),
        block_number = event.block_number,
        simulation_run_id,
        "test-mode event received by detector"
    );
}

fn log_test_mode_detection_emitted(detection: &DetectionResult) {
    tracing::info!(
        pipeline_mode = "test",
        component = "detector",
        detection_id = %detection.detection_id,
        pattern_id = %detection.pattern_id,
        tenant_id = detection.tenant_id.as_deref(),
        event_key = detection.event_key.as_deref(),
        subject_key = detection.subject_key.as_deref(),
        severity = ?detection.severity,
        lifecycle_state = ?detection.lifecycle_state,
        incident_transition = ?detection.incident_transition,
        simulation_run_id = detection.simulation_run_id.as_deref(),
        "test-mode detection emitted"
    );
}

/// Owns all registered patterns and dispatches events to each in turn.
pub struct PatternRegistry {
    patterns: Vec<Box<dyn DetectionPattern>>,
}

impl PatternRegistry {
    /// Build a registry pre-loaded with all built-in patterns.
    pub fn new() -> Self {
        Self {
            patterns: vec![
                Box::new(dpeg::DpegPattern::default()),
                Box::new(flash_loan::FlashLoanPattern::default()),
                Box::new(tvl_drop::TvlDropPattern::default()),
                Box::new(generic_rule::GenericRulePattern::default()),
                Box::new(utilization_high::UtilizationHighPattern::default()),
            ],
        }
    }

    /// Reload configuration for every registered pattern.
    pub async fn reload_all(
        &mut self,
        config_map: &std::collections::HashMap<(String, String), serde_json::Value>,
    ) -> Result<()> {
        for pattern in &mut self.patterns {
            if let Err(err) = pattern.reload_config(config_map).await {
                common::log_error!(
                    warn,
                    err,
                    "failed to reload pattern config",
                    pattern_id = pattern.pattern_id()
                );
            }
        }
        Ok(())
    }

    /// Dispatch one event through every pattern.  Fire-and-persist each
    /// `DetectionResult` to Postgres and the detections Redis stream.
    pub async fn process_event(
        &mut self,
        event: &UnifiedEvent,
        repo: &PostgresRepository,
        stream: &RedisStreamPublisher,
    ) -> Result<()> {
        let (is_simulated, simulation_run_id) = simulation_metadata_from_event(event);
        if is_simulated {
            log_test_mode_event_received(event, simulation_run_id.as_deref());
        }

        let raw_persisted = event
            .payload
            .get("raw_persisted")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !raw_persisted {
            if let Err(err) = repo.insert_raw_event(event).await {
                common::log_error!(
                    warn,
                    err,
                    "failed to persist source feed event",
                    event_id = %event.event_id,
                    source_id = %event.source_id
                );
            }
        }

        let now = Utc::now();
        for pattern in &mut self.patterns {
            match pattern.process_event(event, now, repo).await {
                Ok(Some(detection)) => {
                    if detection.is_simulated || detection.simulation_run_id.is_some() {
                        log_test_mode_detection_emitted(&detection);
                    }
                    if let Err(err) = repo.save_detection(&detection).await {
                        common::log_error!(
                            warn,
                            err,
                            "failed to persist detection",
                            pattern_id = pattern.pattern_id()
                        );
                    }
                    if let Err(err) = stream.publish_detection(&detection).await {
                        common::log_error!(
                            warn,
                            err,
                            "failed to publish detection",
                            pattern_id = pattern.pattern_id()
                        );
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    common::log_error!(
                        warn,
                        err,
                        "pattern evaluation error — skipping event",
                        pattern_id = pattern.pattern_id(),
                        event_id = %event.event_id
                    );
                }
            }
        }
        Ok(())
    }
}
