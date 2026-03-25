//! DPEG (De-Peg) detection pattern.
//!
//! Monitors price-feed market events (`UnifiedEvent` with `market_key` + `price`) and
//! computes a per-tenant, per-market weighted median across all contributing sources.
//! Fires a `DetectionResult` when a sustained depeg breach is detected based on the
//! per-tenant `DpegPolicy` stored in `tenant_pattern_configs`.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use event_schema::{
    apply_mappings, resolve_field_mappings, AttackFamily, Chain, ContextClassification,
    DetectionResult, DetectionSignal, FieldMapping, IncidentTransition, LifecycleState, RiskScore,
    Severity, SignalType, UnifiedEvent,
};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use state_manager::{PatternSnapshotInsert, PostgresRepository};
use uuid::Uuid;

use super::{append_snapshot_meta, simulation_metadata_from_event, DetectionPattern};

pub const PATTERN_ID: &str = "dpeg";

// ─── Policy types (inlined from crates/dpeg-engine) ──────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpegSeverityBands {
    pub medium: f64,
    pub high: f64,
    pub critical: f64,
}

impl Default for DpegSeverityBands {
    fn default() -> Self {
        Self {
            medium: 1.0,
            high: 3.0,
            critical: 5.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpegSourceFilter {
    #[serde(default)]
    pub cex_whitelist: Vec<String>,
    #[serde(default = "default_true")]
    pub include_oracles: bool,
    #[serde(default = "default_true")]
    pub include_aggregators: bool,
    #[serde(default = "default_true")]
    pub include_dex: bool,
    #[serde(default = "default_min_healthy")]
    pub min_healthy_sources: usize,
}

fn default_true() -> bool {
    true
}

fn default_min_healthy() -> usize {
    3
}

impl Default for DpegSourceFilter {
    fn default() -> Self {
        Self {
            cex_whitelist: Vec::new(),
            include_oracles: true,
            include_aggregators: true,
            include_dex: true,
            min_healthy_sources: 3,
        }
    }
}

impl DpegSourceFilter {
    fn source_kind_allowed(&self, source_id: &str, source_kind: &str) -> bool {
        match source_kind.to_ascii_lowercase().as_str() {
            "oracle" => self.include_oracles,
            "aggregator" => self.include_aggregators,
            "dex" => self.include_dex,
            "cex" => {
                self.cex_whitelist.is_empty()
                    || self
                        .cex_whitelist
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(source_id))
            }
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DpegToggles {
    #[serde(default)]
    pub oracle_confirmation: bool,
    #[serde(default)]
    pub volume_confirmation: bool,
    #[serde(default)]
    pub contagion_detection: bool,
    #[serde(default)]
    pub liquidity_depth_check: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpegConfidenceWeights {
    pub source_agreement: f64,
    pub oracle_confirmation: f64,
    pub volume_confirmation: f64,
}

impl Default for DpegConfidenceWeights {
    fn default() -> Self {
        Self {
            source_agreement: 60.0,
            oracle_confirmation: 25.0,
            volume_confirmation: 15.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpegSourceOverride {
    pub source_id: String,
    pub weight: f64,
    pub enabled: bool,
    pub stale_timeout_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpegPolicy {
    #[serde(default)]
    pub tenant_id: String,
    pub market_key: String,
    pub peg_target: f64,
    pub min_sources: usize,
    pub quorum_pct: f64,
    pub sustained_window_ms: i64,
    pub cooldown_sec: i64,
    pub stale_timeout_ms: i64,
    #[serde(default)]
    pub severity_bands: DpegSeverityBands,
    #[serde(default)]
    pub severity_bands_isolated: Option<DpegSeverityBands>,
    #[serde(default)]
    pub severity_bands_systemic: Option<DpegSeverityBands>,
    #[serde(default = "default_isolated_floor_pct")]
    pub isolated_floor_pct: f64,
    #[serde(default = "default_systemic_floor_pct")]
    pub systemic_floor_pct: f64,
    #[serde(default = "default_deescalation_blocks")]
    pub deescalation_blocks: i64,
    #[serde(default = "default_resolution_blocks")]
    pub resolution_blocks: i64,
    #[serde(default)]
    pub source_filter: DpegSourceFilter,
    #[serde(default)]
    pub toggles: DpegToggles,
    #[serde(default)]
    pub confidence_weights: DpegConfidenceWeights,
    #[serde(default = "default_min_confidence_to_fire")]
    pub min_confidence_to_fire: f64,
    #[serde(default, deserialize_with = "deserialize_source_overrides")]
    pub source_overrides: HashMap<String, DpegSourceOverride>,
}

fn default_isolated_floor_pct() -> f64 {
    0.5
}

fn default_systemic_floor_pct() -> f64 {
    0.01
}

fn default_deescalation_blocks() -> i64 {
    5
}

fn default_resolution_blocks() -> i64 {
    30
}

fn default_min_confidence_to_fire() -> f64 {
    50.0
}

fn deserialize_source_overrides<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, DpegSourceOverride>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(HashMap::new()),
        Value::Object(map) => {
            serde_json::from_value::<HashMap<String, DpegSourceOverride>>(Value::Object(map))
                .map_err(de::Error::custom)
        }
        Value::Array(items) => {
            let parsed = serde_json::from_value::<Vec<DpegSourceOverride>>(Value::Array(items))
                .map_err(de::Error::custom)?;
            let mut mapped = HashMap::with_capacity(parsed.len());
            for entry in parsed {
                mapped.insert(entry.source_id.clone(), entry);
            }
            Ok(mapped)
        }
        _ => Err(de::Error::custom(
            "source_overrides must be a JSON object or array",
        )),
    }
}

impl DpegPolicy {
    fn validate(&self) -> Result<()> {
        if self.peg_target <= 0.0 {
            return Err(anyhow!("peg_target must be > 0"));
        }
        if self.min_sources == 0 {
            return Err(anyhow!("min_sources must be > 0"));
        }
        if !(0.0..=1.0).contains(&self.quorum_pct) {
            return Err(anyhow!("quorum_pct must be between 0 and 1"));
        }
        if self.sustained_window_ms <= 0 {
            return Err(anyhow!("sustained_window_ms must be > 0"));
        }
        if self.cooldown_sec < 0 {
            return Err(anyhow!("cooldown_sec must be >= 0"));
        }
        if self.stale_timeout_ms <= 0 {
            return Err(anyhow!("stale_timeout_ms must be > 0"));
        }
        if self.isolated_floor_pct <= 0.0 {
            return Err(anyhow!("isolated_floor_pct must be > 0"));
        }
        if self.systemic_floor_pct <= 0.0 {
            return Err(anyhow!("systemic_floor_pct must be > 0"));
        }
        if self.deescalation_blocks <= 0 {
            return Err(anyhow!("deescalation_blocks must be > 0"));
        }
        if self.resolution_blocks <= 0 {
            return Err(anyhow!("resolution_blocks must be > 0"));
        }
        if !(0.0..=100.0).contains(&self.min_confidence_to_fire) {
            return Err(anyhow!("min_confidence_to_fire must be between 0 and 100"));
        }
        if self.confidence_weights.source_agreement < 0.0
            || self.confidence_weights.oracle_confirmation < 0.0
            || self.confidence_weights.volume_confirmation < 0.0
        {
            return Err(anyhow!("confidence_weights must be non-negative"));
        }
        if self.confidence_weights.source_agreement
            + self.confidence_weights.oracle_confirmation
            + self.confidence_weights.volume_confirmation
            <= 0.0
        {
            return Err(anyhow!("confidence_weights total must be > 0"));
        }
        Ok(())
    }

    fn source_enabled(&self, source_id: &str, source_kind: &str) -> bool {
        if !self
            .source_filter
            .source_kind_allowed(source_id, source_kind)
        {
            return false;
        }
        self.source_overrides
            .get(source_id)
            .map(|v| v.enabled)
            .unwrap_or(true)
    }

    fn source_weight(&self, source_id: &str) -> f64 {
        self.source_overrides
            .get(source_id)
            .map(|v| v.weight)
            .unwrap_or(1.0)
            .max(0.0)
    }

    fn source_stale_timeout_ms(&self, source_id: &str) -> i64 {
        self.source_overrides
            .get(source_id)
            .and_then(|v| v.stale_timeout_ms)
            .unwrap_or(self.stale_timeout_ms)
            .max(1)
    }

    fn enabled_source_count(&self) -> usize {
        let configured_enabled = self.source_overrides.values().filter(|v| v.enabled).count();
        if configured_enabled == 0 {
            self.min_sources
        } else {
            configured_enabled
        }
    }

    fn isolated_bands(&self) -> DpegSeverityBands {
        self.severity_bands_isolated
            .clone()
            .or_else(|| {
                if self.severity_bands.medium > 0.0 {
                    Some(self.severity_bands.clone())
                } else {
                    None
                }
            })
            .unwrap_or(DpegSeverityBands {
                medium: 0.5,
                high: 1.0,
                critical: 5.0,
            })
    }

    fn systemic_bands(&self) -> DpegSeverityBands {
        self.severity_bands_systemic
            .clone()
            .unwrap_or(DpegSeverityBands {
                medium: 0.01,
                high: 0.25,
                critical: 0.25,
            })
    }
}

// ─── Cached quote + state ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct QuoteInput {
    source_id: String,
    source_kind: String,
    price: f64,
    observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct EvidenceContributor {
    source_id: String,
    source_kind: String,
    price: f64,
    observed_at: DateTime<Utc>,
    age_ms: i64,
    weight: f64,
    divergence_pct: f64,
    supports_alert_direction: bool,
    contributes_to_quorum: bool,
    confirms_oracle: bool,
    contributes_to_breach: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DpegAlertState {
    pub breach_started_at: Option<DateTime<Utc>>,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_alerted_at: Option<DateTime<Utc>>,
    pub last_divergence_pct: Option<f64>,
    pub last_severity: Option<String>,
    pub last_classification: Option<String>,
    pub trigger_floor_pct: Option<f64>,
    pub below_severity_blocks: i64,
    pub below_trigger_blocks: i64,
    /// Highest event timestamp seen so far for this market key + replay scope.
    /// Events arriving with a timestamp below this mark are late deliveries from
    /// concurrent source streams and must not regress state transitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high_water_mark: Option<DateTime<Utc>>,
}

// ─── Pattern impl ─────────────────────────────────────────────────────────────

/// Per-tenant, per-market DPEG detection pattern.
#[derive(Default)]
pub struct DpegPattern {
    /// (tenant_id, market_key) → DpegPolicy
    policies: HashMap<(String, String), DpegPolicy>,
    /// (tenant_id, market_key, replay_scope) → recent quotes per source_id.
    ///
    /// Replay streams can arrive slightly out of payload timestamp order across
    /// sources. Keeping a short history lets us evaluate each event against the
    /// latest quote that existed at that event timestamp instead of letting a
    /// future quote temporarily collapse quorum. Simulated runs can also rewind
    /// time, so cached quotes must not bleed across different replay runs.
    quote_cache: HashMap<(String, String, String), HashMap<String, Vec<QuoteInput>>>,
    /// tenant_id → set of enabled source_ids (None = unrestricted)
    source_bindings: HashMap<String, HashSet<String>>,
    /// tenant_id → source_id → gateway field mapping overrides
    source_mapping_overrides: HashMap<String, HashMap<String, SourceBindingRuntimeOverride>>,
}

#[derive(Debug, Clone, Default)]
struct SourceBindingRuntimeOverride {
    active_stream_ids: Vec<String>,
    stream_field_mappings: HashMap<String, Vec<FieldMapping>>,
}

impl DpegPattern {
    const MAX_QUOTE_HISTORY_PER_SOURCE: usize = 16;

    fn state_key(market_key: &str, simulation_run_id: Option<&str>) -> String {
        simulation_run_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|run_id| format!("{market_key}::{run_id}"))
            .unwrap_or_else(|| market_key.to_string())
    }

    fn parse_source_mapping_overrides(
        config: &Value,
    ) -> HashMap<String, SourceBindingRuntimeOverride> {
        let mut overrides = HashMap::new();
        let Some(bindings) = config.get("source_bindings").and_then(Value::as_array) else {
            return overrides;
        };

        for binding in bindings {
            if !binding
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                continue;
            }
            let Some(source_id) = binding
                .get("source_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let binding_config = binding
                .get("binding_config")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let active_stream_ids = binding_config
                .get("active_stream_ids")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut stream_field_mappings = HashMap::new();
            if let Some(streams) = binding_config.get("streams").and_then(Value::as_array) {
                for stream in streams {
                    let Some(stream_id) = stream
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    else {
                        continue;
                    };
                    let Some(field_mapping_value) = stream.get("field_mappings") else {
                        continue;
                    };
                    let Ok(field_mappings) =
                        serde_json::from_value::<Vec<FieldMapping>>(field_mapping_value.clone())
                    else {
                        continue;
                    };
                    if field_mappings.is_empty() {
                        continue;
                    }
                    stream_field_mappings.insert(stream_id.to_string(), field_mappings);
                }
            }
            if active_stream_ids.is_empty() && stream_field_mappings.is_empty() {
                continue;
            }
            overrides.insert(
                source_id.to_string(),
                SourceBindingRuntimeOverride {
                    active_stream_ids,
                    stream_field_mappings,
                },
            );
        }

        overrides
    }

    fn effective_event_fields(
        &self,
        event: &UnifiedEvent,
    ) -> (Option<String>, Option<f64>, DateTime<Utc>) {
        let mut market_key = event.market_key.clone();
        let mut price = event.price;
        let mut timestamp = event.timestamp;

        let Some(tenant_overrides) = self.source_mapping_overrides.get(&event.tenant_id) else {
            return (market_key, price, timestamp);
        };
        let Some(binding_override) = tenant_overrides.get(&event.source_id) else {
            return (market_key, price, timestamp);
        };

        let stream_config_id = event
            .payload
            .get("stream_config_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let parser_name = event
            .payload
            .get("parser_name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mapping_override = stream_config_id
            .and_then(|id| binding_override.stream_field_mappings.get(id))
            .or_else(|| {
                binding_override
                    .active_stream_ids
                    .iter()
                    .find_map(|id| binding_override.stream_field_mappings.get(id))
            })
            .or_else(|| {
                (binding_override.stream_field_mappings.len() == 1)
                    .then(|| binding_override.stream_field_mappings.values().next())
                    .flatten()
            });

        let Some(mappings) = mapping_override else {
            return (market_key, price, timestamp);
        };
        let override_config = serde_json::json!({
            "field_mappings": mappings,
        });
        let resolved = resolve_field_mappings(parser_name, Some(&override_config));
        if resolved.is_empty() {
            return (market_key, price, timestamp);
        }
        let mapped = apply_mappings(&resolved, &event.payload, Some(&override_config));
        if let Some(mapped_market_key) = mapped
            .market_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            market_key = Some(mapped_market_key.to_string());
        }
        if let Some(mapped_price) = mapped
            .price
            .filter(|value| value.is_finite() && *value > 0.0)
        {
            price = Some(mapped_price);
        }
        if let Some(mapped_timestamp) = mapped.timestamp {
            timestamp = mapped_timestamp;
        }

        (market_key, price, timestamp)
    }

    fn normalized_policy_config(config: &Value) -> Value {
        let Some(object) = config.as_object() else {
            return config.clone();
        };

        if let Some(policies) = object.get("policies") {
            return policies.clone();
        }

        if !object.contains_key("market_key") {
            return config.clone();
        }

        let mut policy = object.clone();
        if !policy.contains_key("sustained_window_ms") {
            if let Some(window_sec) = policy.remove("window_sec") {
                if let Some(seconds) = window_sec.as_i64() {
                    policy.insert(
                        "sustained_window_ms".to_string(),
                        Value::from(seconds.saturating_mul(1000)),
                    );
                }
            }
        }
        policy
            .entry("quorum_pct".to_string())
            .or_insert_with(|| Value::from(0.5));
        policy
            .entry("stale_timeout_ms".to_string())
            .or_insert_with(|| Value::from(30_000));

        Value::Array(vec![Value::Object(policy)])
    }

    fn parse_policies(tenant_id: &str, config: &Value) -> Vec<DpegPolicy> {
        let config_value = Self::normalized_policy_config(config);

        let entries: Vec<DpegPolicy> = match serde_json::from_value(config_value) {
            Ok(value) => value,
            Err(err) => {
                common::log_error!(
                    warn,
                    err,
                    "failed to parse dpeg config",
                    tenant_id = %tenant_id
                );
                return Vec::new();
            }
        };

        let mut parsed = Vec::new();
        for mut policy in entries {
            policy.tenant_id = tenant_id.to_string();
            if let Err(err) = policy.validate() {
                common::log_error!(
                    warn,
                    err,
                    "invalid dpeg policy — skipping market",
                    tenant_id = %tenant_id,
                    market_key = %policy.market_key
                );
                continue;
            }
            parsed.push(policy);
        }
        parsed
    }

    fn effective_policy(&self, tenant_id: &str, market_key: &str) -> Option<DpegPolicy> {
        self.policies
            .get(&(tenant_id.to_string(), market_key.to_string()))
            .cloned()
    }

    fn accelerated_simulation_run(event: &UnifiedEvent) -> bool {
        event
            .payload
            .get("simulation")
            .and_then(Value::as_object)
            .and_then(|simulation| simulation.get("speed_factor"))
            .and_then(|value| match value {
                Value::Number(number) => number.as_f64(),
                Value::String(raw) => raw.trim().parse::<f64>().ok(),
                _ => None,
            })
            .map(|speed_factor| speed_factor.is_finite() && speed_factor > 1.0)
            .unwrap_or(false)
    }

    fn classify_context(
        &self,
        policy: &DpegPolicy,
        tenant_id: &str,
        replay_scope: &str,
        now: DateTime<Utc>,
    ) -> ContextClassification {
        if !policy.toggles.contagion_detection {
            return ContextClassification::Isolated;
        }

        let tenant_policies: Vec<DpegPolicy> = self
            .policies
            .iter()
            .filter(|((candidate_tenant, _), _)| candidate_tenant == tenant_id)
            .map(|(_, candidate_policy)| candidate_policy.clone())
            .collect();
        let mut systemic_markets = 0usize;
        for candidate_policy in tenant_policies {
            let candidate_market = candidate_policy.market_key.clone();
            if !candidate_market.to_ascii_uppercase().ends_with("/USD") {
                continue;
            }
            let Some(quotes) = self.quote_cache.get(&(
                tenant_id.to_string(),
                candidate_market.clone(),
                replay_scope.to_string(),
            )) else {
                continue;
            };
            let quote_values = latest_quotes_for_time(quotes, now);
            if let Some(divergence_pct) =
                market_divergence_pct(&candidate_policy, &quote_values, now)
            {
                if divergence_pct >= candidate_policy.systemic_floor_pct {
                    systemic_markets += 1;
                }
            }
            if systemic_markets >= 2 {
                return ContextClassification::Systemic;
            }
        }
        ContextClassification::Isolated
    }
}

#[async_trait]
impl DetectionPattern for DpegPattern {
    fn pattern_id(&self) -> &str {
        PATTERN_ID
    }

    async fn reload_config(&mut self, config_map: &HashMap<(String, String), Value>) -> Result<()> {
        let mut new_policies = HashMap::new();
        let mut next_bindings = HashMap::new();
        let mut next_mapping_overrides = HashMap::new();
        for ((tenant_id, pattern_id), config) in config_map {
            if pattern_id != PATTERN_ID {
                continue;
            }
            let detection_config = super::extract_detection_config(config);
            for policy in Self::parse_policies(tenant_id, detection_config) {
                new_policies.insert((tenant_id.clone(), policy.market_key.clone()), policy);
            }
            if let Some(bound) = super::extract_bound_source_ids(config) {
                next_bindings.insert(tenant_id.clone(), bound);
            }
            let overrides = Self::parse_source_mapping_overrides(config);
            if !overrides.is_empty() {
                next_mapping_overrides.insert(tenant_id.clone(), overrides);
            }
        }
        self.policies = new_policies;
        self.source_bindings = next_bindings;
        self.source_mapping_overrides = next_mapping_overrides;
        tracing::info!(policy_count = self.policies.len(), "dpeg policies reloaded");
        Ok(())
    }

    async fn process_event(
        &mut self,
        event: &UnifiedEvent,
        _now: DateTime<Utc>,
        repo: &PostgresRepository,
    ) -> Result<Option<DetectionResult>> {
        let (effective_market_key, effective_price, evaluation_time) =
            self.effective_event_fields(event);

        // Enforce source bindings: only process events from sources the tenant has bound to
        // this pattern in the Gateway tab.  Mode switching (live ↔ test) is handled at the
        // indexer level — live tenants receive live-profile streams, test tenants receive
        // test-profile streams.  No simulation bypass needed here.
        if let Some(bound) = self.source_bindings.get(&event.tenant_id) {
            if !bound.is_empty() && !bound.contains(&event.source_id) {
                return Ok(None);
            }
        }

        // Only care about market price feed events.
        let (Some(market_key), Some(price)) = (effective_market_key.as_deref(), effective_price)
        else {
            return Ok(None);
        };
        if !(price.is_finite() && price > 0.0) {
            return Ok(None);
        }

        let policy = self.effective_policy(&event.tenant_id, market_key);
        let Some(policy) = policy else {
            return Ok(None);
        };
        let (is_simulated, simulation_run_id) = super::simulation_metadata_from_event(event);
        let accelerated_simulation_run =
            simulation_run_id.is_some() && Self::accelerated_simulation_run(event);
        let replay_scope = simulation_run_id
            .clone()
            .unwrap_or_else(|| "__live__".to_string());
        let state_key = Self::state_key(market_key, simulation_run_id.as_deref());
        let policy_key = (
            event.tenant_id.clone(),
            market_key.to_string(),
            replay_scope.clone(),
        );

        if simulation_run_id.is_some() {
            self.quote_cache
                .retain(|(tenant_id, cached_market_key, cached_scope), _| {
                    tenant_id != &event.tenant_id
                        || cached_market_key != market_key
                        || cached_scope == &replay_scope
                        || cached_scope == "__live__"
                });
        }

        // Infer source_kind from source_type string.
        let source_kind = infer_source_kind(&format!("{:?}", event.source_type));

        // Update quote cache for this source.
        let market_quotes = self.quote_cache.entry(policy_key.clone()).or_default();
        remember_quote(
            market_quotes,
            QuoteInput {
                source_id: event.source_id.clone(),
                source_kind,
                price,
                observed_at: evaluation_time,
            },
        );

        // Use persisted state as the source of truth so cleanup/reset operations
        // take effect on the next replay without needing a detector restart.
        let current_state = repo
            .load_pattern_state(&policy_key.0, PATTERN_ID, &state_key)
            .await?
            .and_then(|v| serde_json::from_value::<DpegAlertState>(v).ok())
            .unwrap_or_default();

        let quotes = latest_quotes_for_time(market_quotes, evaluation_time);
        let classification =
            self.classify_context(&policy, &event.tenant_id, &replay_scope, evaluation_time);
        let mut outcome = evaluate_policy(
            &policy,
            &quotes,
            &current_state,
            evaluation_time,
            classification,
        )?;
        let min_healthy_sources = policy
            .source_filter
            .min_healthy_sources
            .max(policy.min_sources)
            .max(3);
        if outcome.snapshot.source_count < min_healthy_sources {
            tracing::warn!(
                component = "detector",
                pattern_id = PATTERN_ID,
                tenant_id = %event.tenant_id,
                market_key,
                source_count = outcome.snapshot.source_count,
                min_healthy_sources,
                oracle_confirmed = outcome.snapshot.oracle_confirmed,
                "dpeg source health below minimum; suppressing alert evaluation until enough sources recover"
            );
        }

        // Guard: events from concurrent source streams can arrive at Redis out of
        // event-timestamp order (live jitter or simulation speed-factor collapse).
        // The quote is already in the cache above; skip state transitions for late
        // events so a stale delivery cannot clear breach_started_at or cooldown_until.
        let late_delivery = current_state
            .high_water_mark
            .map(|hwm| evaluation_time < hwm)
            .unwrap_or(false);
        if late_delivery {
            if accelerated_simulation_run {
                if let Some(backfilled_state) =
                    backfill_late_simulated_breach_start(&current_state, &outcome, evaluation_time)
                {
                    let state_value = serde_json::to_value(&backfilled_state)?;
                    let _ = repo
                        .upsert_pattern_state(&policy_key.0, PATTERN_ID, &state_key, state_value)
                        .await;
                    tracing::info!(
                        pipeline_mode = "test",
                        component = "detector",
                        pattern_id = PATTERN_ID,
                        tenant_id = %event.tenant_id,
                        source_id = %event.source_id,
                        event_id = %event.event_id,
                        event_type = %event.event_type,
                        market_key,
                        simulation_run_id = simulation_run_id.as_deref(),
                        evaluation_time = %evaluation_time,
                        high_water_mark = ?current_state.high_water_mark,
                        "backfilled late simulated dpeg breach start"
                    );
                }
            }
            return Ok(None);
        }

        // Advance high-water mark so state only moves forward in event time.
        outcome.next_state.high_water_mark = Some(
            current_state
                .high_water_mark
                .map_or(evaluation_time, |hwm| hwm.max(evaluation_time)),
        );
        if is_simulated {
            log_test_mode_decision(
                event,
                &policy,
                market_key,
                &current_state,
                &outcome,
                evaluation_time,
                simulation_run_id.as_deref(),
            );
        }

        // Persist snapshot regardless of whether an alert fired.
        let snapshot_data = serde_json::json!({
            "weighted_median_price": outcome.snapshot.weighted_median_price,
            "divergence_pct": outcome.snapshot.divergence_pct,
            "source_count": outcome.snapshot.source_count,
            "eligible_source_count": outcome.snapshot.eligible_source_count,
            "quorum_met": outcome.snapshot.quorum_met,
            "breach_active": outcome.snapshot.breach_active,
            "oracle_confirmed": outcome.snapshot.oracle_confirmed,
            "context_classification": outcome.snapshot.classification,
            "trigger_floor_pct": outcome.snapshot.trigger_floor_pct,
            "confidence_breakdown": outcome.snapshot.confidence_breakdown,
            "incident_transition": outcome.transition,
            "peg_target": policy.peg_target,
            "sustained_window_ms": policy.sustained_window_ms,
            "contributing_sources": outcome.snapshot.contributors,
        });
        let severity_str = outcome
            .snapshot
            .severity
            .as_ref()
            .map(|s| format!("{:?}", s).to_lowercase());
        let _ = repo
            .insert_pattern_snapshot(PatternSnapshotInsert {
                tenant_id: &policy_key.0,
                pattern_id: PATTERN_ID,
                snapshot_key: market_key,
                data: append_snapshot_meta(event, snapshot_data),
                score: Some(outcome.snapshot.divergence_pct),
                severity: severity_str.as_deref(),
                observed_at: evaluation_time,
            })
            .await;

        // Persist updated alert state to DB.
        let state_value = serde_json::to_value(&outcome.next_state)?;
        let _ = repo
            .upsert_pattern_state(&policy_key.0, PATTERN_ID, &state_key, state_value)
            .await;

        // Emit detection if needed.
        if outcome.should_emit_alert {
            if let Some(severity) = outcome.emitted_severity.clone() {
                return Ok(Some(build_detection(
                    event,
                    &policy,
                    &outcome.snapshot,
                    severity,
                    outcome.transition,
                    evaluation_time,
                )));
            }
        }

        Ok(None)
    }
}

// ─── DPEG evaluation engine (inlined from crates/dpeg-engine) ─────────────────

struct ConsensusSnapshot {
    weighted_median_price: f64,
    divergence_pct: f64,
    source_count: usize,
    eligible_source_count: usize,
    quorum_met: bool,
    breach_active: bool,
    oracle_confirmed: bool,
    classification: ContextClassification,
    trigger_floor_pct: f64,
    confidence_breakdown: HashMap<String, f64>,
    severity: Option<Severity>,
    contributors: Vec<EvidenceContributor>,
}

struct EvaluationOutcome {
    snapshot: ConsensusSnapshot,
    should_emit_alert: bool,
    next_state: DpegAlertState,
    transition: Option<IncidentTransition>,
    emitted_severity: Option<Severity>,
}

fn evaluate_policy(
    policy: &DpegPolicy,
    quotes: &[QuoteInput],
    current_state: &DpegAlertState,
    now: DateTime<Utc>,
    classification: ContextClassification,
) -> Result<EvaluationOutcome> {
    let mut weighted_points = Vec::<(f64, f64)>::new();
    let mut eligible_quotes = Vec::<QuoteInput>::new();
    for quote in quotes {
        if !policy.source_enabled(&quote.source_id, &quote.source_kind) {
            continue;
        }
        if !(quote.price.is_finite() && quote.price > 0.0) {
            continue;
        }
        let stale_ms = policy.source_stale_timeout_ms(&quote.source_id);
        let age_ms = now
            .signed_duration_since(quote.observed_at)
            .num_milliseconds();
        if age_ms < 0 {
            continue;
        }
        if age_ms > stale_ms {
            continue;
        }
        let weight = policy.source_weight(&quote.source_id);
        if weight <= 0.0 || !weight.is_finite() {
            continue;
        }
        weighted_points.push((quote.price, weight));
        eligible_quotes.push(quote.clone());
    }

    let selected_bands = match classification {
        ContextClassification::Systemic => policy.systemic_bands(),
        _ => policy.isolated_bands(),
    };
    let trigger_floor_pct = match classification {
        ContextClassification::Systemic => policy.systemic_floor_pct,
        _ => policy.isolated_floor_pct,
    };

    if weighted_points.is_empty() {
        return Ok(EvaluationOutcome {
            snapshot: ConsensusSnapshot {
                weighted_median_price: policy.peg_target,
                divergence_pct: 0.0,
                source_count: 0,
                eligible_source_count: 0,
                quorum_met: false,
                breach_active: false,
                oracle_confirmed: false,
                classification,
                trigger_floor_pct,
                confidence_breakdown: HashMap::new(),
                severity: None,
                contributors: Vec::new(),
            },
            should_emit_alert: false,
            next_state: DpegAlertState::default(),
            transition: None,
            emitted_severity: None,
        });
    }

    let weighted_median_price =
        weighted_median(&weighted_points).ok_or_else(|| anyhow!("weighted median failed"))?;
    let divergence_pct =
        ((weighted_median_price - policy.peg_target).abs() / policy.peg_target) * 100.0;
    let severity = severity_for_divergence(divergence_pct, &selected_bands);
    let alert_direction = (weighted_median_price - policy.peg_target).signum();

    let source_count = weighted_points.len();
    let enabled_source_count = policy.enabled_source_count().max(1);
    let min_healthy = policy
        .source_filter
        .min_healthy_sources
        .max(policy.min_sources)
        .max(1);
    let source_ratio = source_count as f64 / enabled_source_count as f64;
    let quorum_met = source_count >= min_healthy && source_ratio >= policy.quorum_pct;
    let oracle_confirmed = oracle_confirmation_met(
        policy,
        &eligible_quotes,
        trigger_floor_pct,
        now,
        policy.peg_target,
    );
    let confidence_breakdown = compute_confidence_breakdown(
        policy,
        &eligible_quotes,
        weighted_median_price,
        oracle_confirmed,
        policy.peg_target,
    );
    let confidence_total = confidence_breakdown
        .get("total")
        .copied()
        .unwrap_or_default();
    let threshold_breach = divergence_pct >= trigger_floor_pct && severity.is_some();
    let contributors = build_evidence_contributors(
        policy,
        &eligible_quotes,
        now,
        policy.peg_target,
        trigger_floor_pct,
        alert_direction,
        threshold_breach,
    );
    let breach_active = quorum_met
        && threshold_breach
        && (!policy.toggles.oracle_confirmation || oracle_confirmed)
        && confidence_total >= policy.min_confidence_to_fire;

    let mut next_state = current_state.clone();
    let mut should_emit_alert = false;
    let mut transition = None;
    let mut emitted_severity = None;
    let previous_active = severity_from_str(next_state.last_severity.as_deref());

    if breach_active {
        if next_state.breach_started_at.is_none() {
            next_state.breach_started_at = Some(now);
        }
        let breach_started = next_state.breach_started_at.unwrap();
        let sustained = now.signed_duration_since(breach_started).num_milliseconds()
            >= policy.sustained_window_ms;

        let cooldown_active = next_state
            .cooldown_until
            .map(|until| until > now)
            .unwrap_or(false);

        let current_rank = severity_rank(severity.as_ref());
        let previous_rank = severity_rank(previous_active.as_ref());
        let is_new_incident = previous_active.is_none();

        if is_new_incident {
            if sustained && !cooldown_active {
                if let Some(curr_severity) = severity.clone() {
                    should_emit_alert = true;
                    transition = Some(IncidentTransition::Trigger);
                    emitted_severity = Some(curr_severity.clone());
                    next_state.last_alerted_at = Some(now);
                    next_state.last_divergence_pct = Some(divergence_pct);
                    next_state.last_severity = Some(format!("{:?}", curr_severity).to_lowercase());
                    next_state.last_classification =
                        Some(context_classification_str(&classification).to_string());
                    next_state.trigger_floor_pct = Some(trigger_floor_pct);
                    next_state.below_trigger_blocks = 0;
                    next_state.below_severity_blocks = 0;
                    next_state.cooldown_until = Some(now + Duration::seconds(policy.cooldown_sec));
                }
            }
        } else if current_rank > previous_rank {
            next_state.below_severity_blocks = 0;
            next_state.last_divergence_pct = Some(divergence_pct);
            next_state.last_classification =
                Some(context_classification_str(&classification).to_string());
            if !cooldown_active {
                if let Some(curr_severity) = severity.clone() {
                    should_emit_alert = true;
                    transition = Some(IncidentTransition::Escalate);
                    emitted_severity = Some(curr_severity.clone());
                    next_state.last_alerted_at = Some(now);
                    next_state.last_severity = Some(format!("{:?}", curr_severity).to_lowercase());
                    next_state.cooldown_until = Some(now + Duration::seconds(policy.cooldown_sec));
                    if next_state.trigger_floor_pct.is_none() {
                        next_state.trigger_floor_pct = Some(trigger_floor_pct);
                    }
                }
            }
        } else if current_rank < previous_rank {
            next_state.below_severity_blocks += 1;
            next_state.last_divergence_pct = Some(divergence_pct);
            next_state.last_classification =
                Some(context_classification_str(&classification).to_string());
            if next_state.below_severity_blocks >= policy.deescalation_blocks && !cooldown_active {
                if let Some(curr_severity) = severity.clone() {
                    should_emit_alert = true;
                    transition = Some(IncidentTransition::Deescalate);
                    emitted_severity = Some(curr_severity.clone());
                    next_state.last_alerted_at = Some(now);
                    next_state.last_severity = Some(format!("{:?}", curr_severity).to_lowercase());
                    next_state.below_severity_blocks = 0;
                    next_state.cooldown_until = Some(now + Duration::seconds(policy.cooldown_sec));
                }
            }
        } else {
            next_state.below_severity_blocks = 0;
            next_state.last_divergence_pct = Some(divergence_pct);
            next_state.last_classification =
                Some(context_classification_str(&classification).to_string());
        }
        next_state.below_trigger_blocks = 0;
    } else {
        next_state.breach_started_at = None;
        next_state.last_divergence_pct = Some(divergence_pct);
        next_state.below_severity_blocks = 0;

        if previous_active.is_some() {
            let resolution_floor = next_state.trigger_floor_pct.unwrap_or(trigger_floor_pct);
            let resolution_ready = divergence_pct < resolution_floor
                && available_oracles_within_resolution_floor(
                    &eligible_quotes,
                    resolution_floor,
                    policy.peg_target,
                );
            if resolution_ready {
                next_state.below_trigger_blocks += 1;
            } else {
                next_state.below_trigger_blocks = 0;
            }

            if next_state.below_trigger_blocks >= policy.resolution_blocks {
                should_emit_alert = true;
                transition = Some(IncidentTransition::Resolve);
                emitted_severity = previous_active.clone();
                next_state.last_alerted_at = Some(now);
                next_state.last_severity = None;
                next_state.last_classification = None;
                next_state.trigger_floor_pct = None;
                next_state.below_trigger_blocks = 0;
                next_state.cooldown_until = Some(now + Duration::seconds(policy.cooldown_sec));
            }
        }
    }

    Ok(EvaluationOutcome {
        snapshot: ConsensusSnapshot {
            weighted_median_price,
            divergence_pct,
            source_count,
            eligible_source_count: enabled_source_count,
            quorum_met,
            breach_active,
            oracle_confirmed,
            classification,
            trigger_floor_pct,
            confidence_breakdown,
            severity,
            contributors,
        },
        should_emit_alert,
        next_state,
        transition,
        emitted_severity,
    })
}

fn market_divergence_pct(
    policy: &DpegPolicy,
    quotes: &[QuoteInput],
    now: DateTime<Utc>,
) -> Option<f64> {
    let mut weighted_points = Vec::<(f64, f64)>::new();
    for quote in quotes {
        if !policy.source_enabled(&quote.source_id, &quote.source_kind) {
            continue;
        }
        let stale_ms = policy.source_stale_timeout_ms(&quote.source_id);
        let age_ms = now
            .signed_duration_since(quote.observed_at)
            .num_milliseconds();
        if age_ms > stale_ms {
            continue;
        }
        let weight = policy.source_weight(&quote.source_id);
        if weight <= 0.0 || !weight.is_finite() {
            continue;
        }
        weighted_points.push((quote.price, weight));
    }
    let median = weighted_median(&weighted_points)?;
    Some(((median - policy.peg_target).abs() / policy.peg_target) * 100.0)
}

fn oracle_confirmation_met(
    policy: &DpegPolicy,
    eligible_quotes: &[QuoteInput],
    trigger_floor_pct: f64,
    now: DateTime<Utc>,
    peg_target: f64,
) -> bool {
    eligible_quotes.iter().any(|quote| {
        if quote.source_kind != "oracle" {
            return false;
        }
        let stale_ms = policy.source_stale_timeout_ms(&quote.source_id);
        let age_ms = now
            .signed_duration_since(quote.observed_at)
            .num_milliseconds();
        if age_ms > stale_ms {
            return false;
        }
        ((quote.price - peg_target).abs() / peg_target) * 100.0 >= trigger_floor_pct
    })
}

fn available_oracles_within_resolution_floor(
    eligible_quotes: &[QuoteInput],
    resolution_floor_pct: f64,
    peg_target: f64,
) -> bool {
    eligible_quotes
        .iter()
        .filter(|quote| quote.source_kind == "oracle")
        .all(|quote| ((quote.price - peg_target).abs() / peg_target) * 100.0 < resolution_floor_pct)
}

fn compute_confidence_breakdown(
    policy: &DpegPolicy,
    eligible_quotes: &[QuoteInput],
    weighted_median_price: f64,
    oracle_confirmed: bool,
    peg_target: f64,
) -> HashMap<String, f64> {
    let mut breakdown = HashMap::new();
    if eligible_quotes.is_empty() {
        breakdown.insert("source_agreement".to_string(), 0.0);
        breakdown.insert("oracle_confirmation".to_string(), 0.0);
        breakdown.insert("volume_confirmation".to_string(), 50.0);
        breakdown.insert("total".to_string(), 0.0);
        return breakdown;
    }

    let direction = (weighted_median_price - peg_target).signum();
    let agreement_count = eligible_quotes
        .iter()
        .filter(|quote| (quote.price - peg_target).signum() == direction)
        .count();
    let source_agreement = (agreement_count as f64 / eligible_quotes.len() as f64) * 100.0;
    let oracle_score = if policy.toggles.oracle_confirmation {
        if oracle_confirmed {
            100.0
        } else {
            0.0
        }
    } else {
        50.0
    };
    let volume_score = 50.0;
    let source_weight: f64 = policy.confidence_weights.source_agreement.max(0.0);
    let oracle_weight: f64 = if policy.toggles.oracle_confirmation {
        policy.confidence_weights.oracle_confirmation.max(0.0)
    } else {
        0.0
    };
    let volume_weight: f64 = if policy.toggles.volume_confirmation {
        policy.confidence_weights.volume_confirmation.max(0.0)
    } else {
        0.0
    };
    let total_weight = (source_weight + oracle_weight + volume_weight).max(f64::EPSILON);
    let total = ((source_agreement * source_weight)
        + (oracle_score * oracle_weight)
        + (volume_score * volume_weight))
        / total_weight;

    breakdown.insert("source_agreement".to_string(), source_agreement);
    breakdown.insert("oracle_confirmation".to_string(), oracle_score);
    breakdown.insert("volume_confirmation".to_string(), volume_score);
    breakdown.insert("total".to_string(), total);
    breakdown
}

fn build_evidence_contributors(
    policy: &DpegPolicy,
    eligible_quotes: &[QuoteInput],
    now: DateTime<Utc>,
    peg_target: f64,
    trigger_floor_pct: f64,
    alert_direction: f64,
    threshold_breach: bool,
) -> Vec<EvidenceContributor> {
    let mut contributors = eligible_quotes
        .iter()
        .map(|quote| {
            let age_ms = now
                .signed_duration_since(quote.observed_at)
                .num_milliseconds()
                .max(0);
            let divergence_pct = ((quote.price - peg_target).abs() / peg_target) * 100.0;
            let supports_alert_direction =
                alert_direction != 0.0 && (quote.price - peg_target).signum() == alert_direction;
            let confirms_oracle =
                quote.source_kind == "oracle" && divergence_pct >= trigger_floor_pct;
            EvidenceContributor {
                source_id: quote.source_id.clone(),
                source_kind: quote.source_kind.clone(),
                price: quote.price,
                observed_at: quote.observed_at,
                age_ms,
                weight: policy.source_weight(&quote.source_id),
                divergence_pct,
                supports_alert_direction,
                contributes_to_quorum: true,
                confirms_oracle,
                contributes_to_breach: threshold_breach
                    && (supports_alert_direction || confirms_oracle),
            }
        })
        .collect::<Vec<_>>();
    contributors.sort_by(|left, right| {
        left.observed_at
            .cmp(&right.observed_at)
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    contributors
}

fn weighted_median(points: &[(f64, f64)]) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let mut sorted = points.to_vec();
    sorted.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let total_weight: f64 = sorted.iter().map(|(_, w)| *w).sum();
    if total_weight <= 0.0 || !total_weight.is_finite() {
        return None;
    }
    let mut running = 0.0;
    for (price, weight) in sorted {
        running += weight;
        if running >= total_weight / 2.0 {
            return Some(price);
        }
    }
    None
}

fn remember_quote(market_quotes: &mut HashMap<String, Vec<QuoteInput>>, quote: QuoteInput) {
    let history = market_quotes.entry(quote.source_id.clone()).or_default();
    history.push(quote);
    history.sort_by(|a, b| a.observed_at.cmp(&b.observed_at));
    history.dedup_by(|a, b| {
        a.observed_at == b.observed_at
            && a.price == b.price
            && a.source_kind == b.source_kind
            && a.source_id == b.source_id
    });
    if history.len() > DpegPattern::MAX_QUOTE_HISTORY_PER_SOURCE {
        let excess = history.len() - DpegPattern::MAX_QUOTE_HISTORY_PER_SOURCE;
        history.drain(0..excess);
    }
}

fn latest_quotes_for_time(
    market_quotes: &HashMap<String, Vec<QuoteInput>>,
    now: DateTime<Utc>,
) -> Vec<QuoteInput> {
    market_quotes
        .values()
        .filter_map(|history| {
            history
                .iter()
                .rev()
                .find(|quote| quote.observed_at <= now)
                .cloned()
        })
        .collect()
}

fn severity_for_divergence(pct: f64, bands: &DpegSeverityBands) -> Option<Severity> {
    if pct >= bands.critical {
        return Some(Severity::Critical);
    }
    if pct >= bands.high {
        return Some(Severity::High);
    }
    if pct >= bands.medium {
        return Some(Severity::Medium);
    }
    None
}

fn severity_rank(s: Option<&Severity>) -> u8 {
    match s {
        Some(Severity::Critical) => 5,
        Some(Severity::High) => 4,
        Some(Severity::Medium) => 3,
        Some(Severity::Low) => 2,
        Some(Severity::Info) => 1,
        None => 0,
    }
}

fn severity_from_str(s: Option<&str>) -> Option<Severity> {
    match s {
        Some(value) if value.eq_ignore_ascii_case("critical") => Some(Severity::Critical),
        Some(value) if value.eq_ignore_ascii_case("high") => Some(Severity::High),
        Some(value) if value.eq_ignore_ascii_case("medium") => Some(Severity::Medium),
        Some(value) if value.eq_ignore_ascii_case("low") => Some(Severity::Low),
        Some(value) if value.eq_ignore_ascii_case("info") => Some(Severity::Info),
        _ => None,
    }
}

fn context_classification_str(value: &ContextClassification) -> &'static str {
    match value {
        ContextClassification::Isolated => "isolated",
        ContextClassification::Systemic => "systemic",
        ContextClassification::None => "none",
    }
}

/// Map `SourceType` debug string ("CexWebsocket", "DexApi", etc.) to a dpeg source_kind.
fn infer_source_kind(source_type: &str) -> String {
    match source_type.to_ascii_lowercase().as_str() {
        "cexwebsocket" => "cex".to_string(),
        "dexapi" => "dex".to_string(),
        "oracleapi" => "oracle".to_string(),
        _ => "unknown".to_string(),
    }
}

fn build_detection(
    event: &UnifiedEvent,
    policy: &DpegPolicy,
    snapshot: &ConsensusSnapshot,
    severity: Severity,
    transition: Option<IncidentTransition>,
    now: DateTime<Utc>,
) -> DetectionResult {
    let (is_simulated, simulation_run_id) = simulation_metadata_from_event(event);
    let subject_key = format!("{}:{}", policy.tenant_id, policy.market_key);
    let divergence_str = format!("{:.3}%", snapshot.divergence_pct);
    let description = format!(
        "Market {} deviated {:.3}% from peg target {:.4} (weighted median: {:.4}). {} source(s), quorum: {}.",
        policy.market_key,
        snapshot.divergence_pct,
        policy.peg_target,
        snapshot.weighted_median_price,
        snapshot.source_count,
        if snapshot.quorum_met { "met" } else { "not met" }
    );

    let confidence_pct = snapshot
        .confidence_breakdown
        .get("total")
        .copied()
        .unwrap_or(0.0);
    let confidence = (confidence_pct / 100.0).clamp(0.0, 1.0);
    let risk_score = RiskScore {
        score: snapshot.divergence_pct.min(100.0),
        confidence,
        rationale: vec![
            format!(
                "context={} oracle_confirmed={} quorum_met={}",
                context_classification_str(&snapshot.classification),
                snapshot.oracle_confirmed,
                snapshot.quorum_met
            ),
            format!(
                "weighted_median={:.6} peg_target={:.6}",
                snapshot.weighted_median_price, policy.peg_target
            ),
        ],
        attribution: Vec::new(),
    };

    let mut oracle_context = std::collections::HashMap::new();
    oracle_context.insert(
        "oracle_confirmed".to_string(),
        serde_json::json!(snapshot.oracle_confirmed),
    );
    oracle_context.insert(
        "weighted_median_price".to_string(),
        serde_json::json!(snapshot.weighted_median_price),
    );
    oracle_context.insert(
        "trigger_floor_pct".to_string(),
        serde_json::json!(snapshot.trigger_floor_pct),
    );
    oracle_context.insert(
        "context_classification".to_string(),
        serde_json::json!(context_classification_str(&snapshot.classification)),
    );
    oracle_context.insert(
        "source_count".to_string(),
        serde_json::json!(snapshot.source_count),
    );
    oracle_context.insert(
        "eligible_source_count".to_string(),
        serde_json::json!(snapshot.eligible_source_count),
    );
    oracle_context.insert(
        "sustained_window_ms".to_string(),
        serde_json::json!(policy.sustained_window_ms),
    );
    oracle_context.insert(
        "contributing_sources".to_string(),
        serde_json::json!(snapshot.contributors),
    );

    let actions_recommended = recommended_actions_for_severity(&severity);
    DetectionResult {
        detection_id: Uuid::new_v4(),
        pattern_id: PATTERN_ID.to_string(),
        event_key: Some(format!("dpeg:{}:{}", policy.tenant_id, policy.market_key)),
        subject_type: Some("market".to_string()),
        subject_key: Some(subject_key),
        tenant_id: Some(policy.tenant_id.clone()),
        chain: Chain::Offchain,
        chain_slug: "offchain".to_string(),
        protocol: format!("market:{}", policy.market_key),
        lifecycle_state: LifecycleState::Confirmed,
        requires_confirmation: policy.toggles.oracle_confirmation,
        attack_family: AttackFamily::PegDeviation,
        severity,
        tx_hash: format!("dpeg-{}", Uuid::new_v4()),
        block_number: 0,
        triggered_rule_ids: vec!["dpeg.sustained_breach".to_string()],
        description: Some(description),
        signals: vec![DetectionSignal {
            signal_type: SignalType::PriceDeviation,
            value: snapshot.divergence_pct,
            label: Some(divergence_str),
            source_id: None,
        }],
        risk_score,
        incident_transition: transition,
        context_classification: Some(snapshot.classification.clone()),
        confidence_breakdown: snapshot.confidence_breakdown.clone(),
        oracle_context,
        actions_recommended,
        is_simulated,
        simulation_run_id,
        created_at: now,
    }
}

fn log_test_mode_decision(
    event: &UnifiedEvent,
    policy: &DpegPolicy,
    market_key: &str,
    current_state: &DpegAlertState,
    outcome: &EvaluationOutcome,
    now: DateTime<Utc>,
    simulation_run_id: Option<&str>,
) {
    let previous_severity = severity_from_str(current_state.last_severity.as_deref());
    let previous_rank = severity_rank(previous_severity.as_ref());
    let current_rank = severity_rank(outcome.snapshot.severity.as_ref());
    let confidence_total = outcome
        .snapshot
        .confidence_breakdown
        .get("total")
        .copied()
        .unwrap_or_default();
    let breach_started_at = outcome.next_state.breach_started_at;
    let breach_age_ms = breach_started_at
        .map(|started| now.signed_duration_since(started).num_milliseconds())
        .unwrap_or(0);
    let sustained_met =
        outcome.snapshot.breach_active && breach_age_ms >= policy.sustained_window_ms;
    let cooldown_until = current_state.cooldown_until;
    let cooldown_active = cooldown_until.map(|until| until > now).unwrap_or(false);
    let suppression_reason =
        dpeg_test_mode_reason(policy, current_state, outcome, now, confidence_total);
    let transition = outcome
        .transition
        .as_ref()
        .map(|value| format!("{value:?}").to_ascii_lowercase());
    let severity = outcome
        .snapshot
        .severity
        .as_ref()
        .map(|value| format!("{value:?}").to_ascii_lowercase());
    let previous_severity =
        previous_severity.map(|value| format!("{value:?}").to_ascii_lowercase());

    tracing::info!(
        pipeline_mode = "test",
        component = "detector",
        pattern_id = PATTERN_ID,
        tenant_id = %event.tenant_id,
        source_id = %event.source_id,
        event_id = %event.event_id,
        event_type = %event.event_type,
        market_key,
        simulation_run_id,
        divergence_pct = outcome.snapshot.divergence_pct,
        weighted_median_price = outcome.snapshot.weighted_median_price,
        confidence_total,
        confidence_threshold = policy.min_confidence_to_fire,
        source_count = outcome.snapshot.source_count,
        eligible_source_count = outcome.snapshot.eligible_source_count,
        min_healthy_sources = policy
            .source_filter
            .min_healthy_sources
            .max(policy.min_sources)
            .max(1),
        quorum_pct = policy.quorum_pct,
        quorum_met = outcome.snapshot.quorum_met,
        oracle_confirmed = outcome.snapshot.oracle_confirmed,
        breach_active = outcome.snapshot.breach_active,
        breach_started_at = ?breach_started_at,
        breach_age_ms,
        sustained_window_ms = policy.sustained_window_ms,
        sustained_met,
        current_severity = severity.as_deref(),
        previous_severity = previous_severity.as_deref(),
        current_rank,
        previous_rank,
        cooldown_until = ?cooldown_until,
        cooldown_active,
        below_severity_blocks = outcome.next_state.below_severity_blocks,
        deescalation_blocks = policy.deescalation_blocks,
        below_trigger_blocks = outcome.next_state.below_trigger_blocks,
        resolution_blocks = policy.resolution_blocks,
        incident_transition = transition.as_deref(),
        should_emit_alert = outcome.should_emit_alert,
        suppression_reason,
        "test-mode dpeg evaluation completed"
    );
}

fn dpeg_test_mode_reason(
    policy: &DpegPolicy,
    current_state: &DpegAlertState,
    outcome: &EvaluationOutcome,
    now: DateTime<Utc>,
    confidence_total: f64,
) -> &'static str {
    if outcome.should_emit_alert {
        return match outcome.transition {
            Some(IncidentTransition::Trigger) => "emitted_trigger",
            Some(IncidentTransition::Escalate) => "emitted_escalate",
            Some(IncidentTransition::Deescalate) => "emitted_deescalate",
            Some(IncidentTransition::Resolve) => "emitted_resolve",
            Some(IncidentTransition::Retract) => "emitted_retract",
            Some(IncidentTransition::Update) => "emitted_update",
            None => "emitted_detection",
        };
    }

    let previous_active = severity_from_str(current_state.last_severity.as_deref());
    let previous_rank = severity_rank(previous_active.as_ref());
    let current_rank = severity_rank(outcome.snapshot.severity.as_ref());
    let cooldown_active = current_state
        .cooldown_until
        .map(|until| until > now)
        .unwrap_or(false);
    let breach_age_ms = outcome
        .next_state
        .breach_started_at
        .map(|started| now.signed_duration_since(started).num_milliseconds())
        .unwrap_or(0);

    if outcome.snapshot.breach_active {
        if previous_active.is_none() {
            if breach_age_ms < policy.sustained_window_ms {
                return "sustained_window_not_met";
            }
            if cooldown_active {
                return "cooldown_active_for_trigger";
            }
            return "trigger_suppressed";
        }
        if current_rank > previous_rank {
            if cooldown_active {
                return "cooldown_active_for_escalation";
            }
            return "escalation_suppressed";
        }
        if current_rank < previous_rank {
            if outcome.next_state.below_severity_blocks < policy.deescalation_blocks {
                return "deescalation_window_not_met";
            }
            if cooldown_active {
                return "cooldown_active_for_deescalation";
            }
            return "deescalation_suppressed";
        }
        return "already_active_same_severity";
    }

    if !outcome.snapshot.quorum_met {
        return "quorum_not_met";
    }
    if policy.toggles.oracle_confirmation && !outcome.snapshot.oracle_confirmed {
        return "oracle_confirmation_not_met";
    }
    if confidence_total < policy.min_confidence_to_fire {
        return "confidence_below_threshold";
    }
    if outcome.snapshot.severity.is_none() {
        return "trigger_floor_not_met";
    }
    if previous_active.is_some() {
        if outcome.next_state.below_trigger_blocks > 0
            && outcome.next_state.below_trigger_blocks < policy.resolution_blocks
        {
            return "resolution_window_not_met";
        }
        if cooldown_active {
            return "cooldown_active_for_resolution";
        }
        return "resolution_gates_not_met";
    }
    "breach_inactive"
}

fn backfill_late_simulated_breach_start(
    current_state: &DpegAlertState,
    outcome: &EvaluationOutcome,
    evaluation_time: DateTime<Utc>,
) -> Option<DpegAlertState> {
    if !outcome.snapshot.breach_active || current_state.last_severity.is_some() {
        return None;
    }

    let should_backfill = current_state
        .breach_started_at
        .map(|started_at| evaluation_time < started_at)
        .unwrap_or(true);
    if !should_backfill {
        return None;
    }

    let mut next_state = current_state.clone();
    next_state.breach_started_at = Some(evaluation_time);
    next_state.last_divergence_pct = Some(outcome.snapshot.divergence_pct);
    next_state.last_classification =
        Some(context_classification_str(&outcome.snapshot.classification).to_string());
    if next_state.trigger_floor_pct.is_none() {
        next_state.trigger_floor_pct = Some(outcome.snapshot.trigger_floor_pct);
    }
    next_state.below_trigger_blocks = 0;
    next_state.below_severity_blocks = 0;
    Some(next_state)
}

fn recommended_actions_for_severity(severity: &Severity) -> Vec<String> {
    match severity {
        Severity::Critical => vec![
            "Immediate Rebalance: move all affected stablecoin positions to safe assets (ETH, BTC, or actively pegged stablecoins)".to_string(),
            "Exit all positions in the affected stablecoin immediately to prevent further capital erosion".to_string(),
            "Withdraw to Owner wallet: transfer funds to a secure wallet outside the affected protocol".to_string(),
        ],
        Severity::High => vec![
            "Partial Exit: reduce stablecoin exposure by 50% to limit downside risk".to_string(),
            "Rebalance to ETH or other correlated assets to maintain market exposure".to_string(),
            "Hold and Monitor: set a recovery price alert at the peg target threshold".to_string(),
        ],
        Severity::Medium => vec![
            "Hold and Monitor: depeg is within manageable range, no immediate action required".to_string(),
            "Rebalance to maintain target stablecoin allocation if exposure exceeds risk tolerance".to_string(),
        ],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_schema::SourceType;

    fn base_policy() -> DpegPolicy {
        DpegPolicy {
            tenant_id: "tenant-a".to_string(),
            market_key: "USDC/USD".to_string(),
            peg_target: 1.0,
            min_sources: 1,
            quorum_pct: 0.0,
            sustained_window_ms: 0,
            cooldown_sec: 0,
            stale_timeout_ms: 60_000,
            severity_bands: DpegSeverityBands {
                medium: 0.5,
                high: 1.0,
                critical: 5.0,
            },
            severity_bands_isolated: Some(DpegSeverityBands {
                medium: 0.5,
                high: 1.0,
                critical: 5.0,
            }),
            severity_bands_systemic: Some(DpegSeverityBands {
                medium: 0.01,
                high: 0.25,
                critical: 0.25,
            }),
            isolated_floor_pct: 0.5,
            systemic_floor_pct: 0.01,
            deescalation_blocks: 5,
            resolution_blocks: 30,
            source_filter: DpegSourceFilter {
                min_healthy_sources: 1,
                ..DpegSourceFilter::default()
            },
            toggles: DpegToggles::default(),
            confidence_weights: DpegConfidenceWeights::default(),
            min_confidence_to_fire: 50.0,
            source_overrides: HashMap::new(),
        }
    }

    fn quote(
        source_id: &str,
        source_kind: &str,
        price: f64,
        observed_at: DateTime<Utc>,
    ) -> QuoteInput {
        QuoteInput {
            source_id: source_id.to_string(),
            source_kind: source_kind.to_string(),
            price,
            observed_at,
        }
    }

    #[test]
    fn simulation_state_key_is_scoped_by_run() {
        assert_eq!(
            DpegPattern::state_key("USDC/USD", Some("run_123")),
            "USDC/USD::run_123"
        );
        assert_eq!(DpegPattern::state_key("USDC/USD", None), "USDC/USD");
    }

    #[test]
    fn future_quotes_are_ignored_for_consensus() {
        let now = Utc::now();
        let policy = base_policy();
        let quotes = vec![
            quote("cex-a", "cex", 0.99, now),
            quote("cex-b", "cex", 0.99, now),
            quote("cex-future", "cex", 0.50, now + Duration::seconds(30)),
        ];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DpegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert_eq!(outcome.snapshot.source_count, 2);
        assert!((outcome.snapshot.weighted_median_price - 0.99).abs() < 1e-9);
    }

    #[test]
    fn latest_quotes_for_time_uses_last_quote_at_or_before_event_time() {
        let base = Utc::now();
        let mut market_quotes = HashMap::new();
        remember_quote(&mut market_quotes, quote("oracle-a", "oracle", 1.0, base));
        remember_quote(
            &mut market_quotes,
            quote("oracle-a", "oracle", 0.88, base + Duration::seconds(24)),
        );
        remember_quote(
            &mut market_quotes,
            quote("cex-a", "cex", 0.8785, base + Duration::seconds(21)),
        );
        remember_quote(
            &mut market_quotes,
            quote("cex-b", "cex", 0.8769, base + Duration::seconds(22)),
        );

        let quotes = latest_quotes_for_time(&market_quotes, base + Duration::seconds(22));
        let oracle = quotes
            .iter()
            .find(|quote| quote.source_id == "oracle-a")
            .expect("oracle quote selected");

        assert_eq!(quotes.len(), 3);
        assert_eq!(oracle.price, 1.0);
        assert_eq!(oracle.observed_at, base);
    }

    #[test]
    fn oracle_confirmation_gate_blocks_alert_without_oracle_quote() {
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = true;
        let quotes = vec![quote("cex-a", "cex", 0.99, now)];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DpegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert!(!outcome.snapshot.oracle_confirmed);
        assert!(!outcome.snapshot.breach_active);
        assert!(!outcome.should_emit_alert);
    }

    #[test]
    fn late_simulated_breach_start_is_backfilled_for_new_incidents() {
        let now = Utc::now();
        let state = DpegAlertState {
            high_water_mark: Some(now + Duration::seconds(30)),
            ..DpegAlertState::default()
        };
        let outcome = EvaluationOutcome {
            snapshot: ConsensusSnapshot {
                weighted_median_price: 0.88,
                divergence_pct: 12.0,
                source_count: 5,
                eligible_source_count: 5,
                quorum_met: true,
                breach_active: true,
                oracle_confirmed: true,
                classification: ContextClassification::Isolated,
                trigger_floor_pct: 0.5,
                confidence_breakdown: HashMap::new(),
                severity: Some(Severity::Critical),
                contributors: Vec::new(),
            },
            should_emit_alert: false,
            next_state: state.clone(),
            transition: None,
            emitted_severity: None,
        };

        let backfilled =
            backfill_late_simulated_breach_start(&state, &outcome, now).expect("backfilled state");

        assert_eq!(backfilled.breach_started_at, Some(now));
        assert_eq!(backfilled.last_divergence_pct, Some(12.0));
        assert_eq!(backfilled.last_classification.as_deref(), Some("isolated"));
    }

    #[test]
    fn late_simulated_breach_start_does_not_overwrite_active_incident_state() {
        let now = Utc::now();
        let state = DpegAlertState {
            breach_started_at: Some(now + Duration::seconds(10)),
            last_severity: Some("critical".to_string()),
            ..DpegAlertState::default()
        };
        let outcome = EvaluationOutcome {
            snapshot: ConsensusSnapshot {
                weighted_median_price: 0.88,
                divergence_pct: 12.0,
                source_count: 5,
                eligible_source_count: 5,
                quorum_met: true,
                breach_active: true,
                oracle_confirmed: true,
                classification: ContextClassification::Isolated,
                trigger_floor_pct: 0.5,
                confidence_breakdown: HashMap::new(),
                severity: Some(Severity::Critical),
                contributors: Vec::new(),
            },
            should_emit_alert: false,
            next_state: state.clone(),
            transition: None,
            emitted_severity: None,
        };

        assert!(backfill_late_simulated_breach_start(&state, &outcome, now).is_none());
    }

    #[test]
    fn contagion_toggle_off_forces_isolated_classification() {
        let now = Utc::now();
        let mut pattern = DpegPattern::default();
        let mut policy = base_policy();
        policy.toggles.contagion_detection = false;
        pattern.policies.insert(
            (policy.tenant_id.clone(), policy.market_key.clone()),
            policy.clone(),
        );

        let classification = pattern.classify_context(&policy, &policy.tenant_id, "__test__", now);
        assert!(matches!(classification, ContextClassification::Isolated));
    }

    #[tokio::test]
    async fn reload_config_keeps_tenant_policies_isolated_for_same_market() {
        let mut pattern = DpegPattern::default();
        let mut config_map = HashMap::new();

        config_map.insert(
            ("tenant-a".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "sustained_window_ms": 1,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 }
                }]
            }),
        );
        config_map.insert(
            ("tenant-b".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 0.98,
                    "min_sources": 3,
                    "quorum_pct": 0.8,
                    "sustained_window_ms": 30000,
                    "cooldown_sec": 600,
                    "stale_timeout_ms": 120000,
                    "severity_bands": { "medium": 2.0, "high": 4.0, "critical": 8.0 }
                }]
            }),
        );

        pattern
            .reload_config(&config_map)
            .await
            .expect("reload config");

        let tenant_a = pattern
            .policies
            .get(&(String::from("tenant-a"), String::from("USDC/USD")))
            .expect("tenant-a policy");
        let tenant_b = pattern
            .policies
            .get(&(String::from("tenant-b"), String::from("USDC/USD")))
            .expect("tenant-b policy");

        assert_eq!(tenant_a.peg_target, 1.0);
        assert_eq!(tenant_a.min_sources, 1);
        assert_eq!(tenant_b.peg_target, 0.98);
        assert_eq!(tenant_b.min_sources, 3);
    }

    #[test]
    fn parse_source_mapping_overrides_reads_gateway_stream_mappings() {
        let config = serde_json::json!({
            "source_bindings": [{
                "source_id": "pyth-eth-mainnet",
                "enabled": true,
                "binding_config": {
                    "active_stream_ids": ["stream-1"],
                    "streams": [{
                        "id": "stream-1",
                        "field_mappings": [{
                            "source_field": "$.custom_price",
                            "canonical_field": "price",
                            "transform": "to_f64"
                        }]
                    }]
                }
            }]
        });

        let overrides = DpegPattern::parse_source_mapping_overrides(&config);
        let pyth = overrides.get("pyth-eth-mainnet").expect("override");
        assert_eq!(pyth.active_stream_ids, vec!["stream-1".to_string()]);
        assert_eq!(
            pyth.stream_field_mappings.get("stream-1").map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn effective_event_fields_apply_gateway_mapping_override() {
        let mut pattern = DpegPattern::default();
        pattern.source_mapping_overrides.insert(
            "tenant-a".to_string(),
            HashMap::from([(
                "pyth-eth-mainnet".to_string(),
                SourceBindingRuntimeOverride {
                    active_stream_ids: vec!["stream-1".to_string()],
                    stream_field_mappings: HashMap::from([(
                        "stream-1".to_string(),
                        serde_json::from_value(serde_json::json!([
                            {
                                "source_field": "$.custom_market",
                                "canonical_field": "market_key",
                                "transform": "identity"
                            },
                            {
                                "source_field": "$.custom_price",
                                "canonical_field": "price",
                                "transform": "to_f64"
                            },
                            {
                                "source_field": "$.custom_ts",
                                "canonical_field": "timestamp",
                                "transform": "parse_ts_iso8601"
                            }
                        ]))
                        .expect("field mappings"),
                    )]),
                },
            )]),
        );

        let event = UnifiedEvent {
            event_id: "evt-1".to_string(),
            tenant_id: "tenant-a".to_string(),
            source_id: "pyth-eth-mainnet".to_string(),
            source_type: SourceType::OracleApi,
            event_type: "oracle_update".to_string(),
            timestamp: Utc::now(),
            payload: serde_json::json!({
                "stream_config_id": "stream-1",
                "parser_name": "pyth_hermes_v2",
                "custom_market": "USDC/USD",
                "custom_price": "0.9987",
                "custom_ts": "2026-03-25T22:57:27Z"
            }),
            chain_id: None,
            block_number: None,
            tx_hash: None,
            market_key: Some("WRONG/USD".to_string()),
            price: Some(1.111),
        };

        let (market_key, price, timestamp) = pattern.effective_event_fields(&event);
        assert_eq!(market_key.as_deref(), Some("USDC/USD"));
        assert_eq!(price, Some(0.9987));
        assert_eq!(
            timestamp,
            DateTime::parse_from_rfc3339("2026-03-25T22:57:27Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn parse_policies_accepts_legacy_single_policy_object() {
        let config = serde_json::json!({
            "market_key": "USDC/USD",
            "peg_target": 1.0,
            "min_sources": 2,
            "window_sec": 60,
            "cooldown_sec": 300,
            "severity_bands": { "medium": 1.0, "high": 3.0, "critical": 5.0 }
        });

        let policies = DpegPattern::parse_policies("tenant-a", &config);

        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].tenant_id, "tenant-a");
        assert_eq!(policies[0].market_key, "USDC/USD");
        assert_eq!(policies[0].sustained_window_ms, 60_000);
        assert_eq!(policies[0].quorum_pct, 0.5);
        assert_eq!(policies[0].stale_timeout_ms, 30_000);
    }

    #[test]
    fn evaluate_policy_uses_tenant_specific_thresholds_independently() {
        let now = Utc::now();
        let quotes = vec![quote("cex-a", "cex", 0.99, now)];

        let mut tenant_a_policy = base_policy();
        tenant_a_policy.tenant_id = "tenant-a".to_string();
        tenant_a_policy.severity_bands.medium = 0.5;
        tenant_a_policy.severity_bands_isolated = Some(DpegSeverityBands {
            medium: 0.5,
            high: 1.0,
            critical: 5.0,
        });
        tenant_a_policy.min_confidence_to_fire = 0.0;

        let mut tenant_b_policy = base_policy();
        tenant_b_policy.tenant_id = "tenant-b".to_string();
        tenant_b_policy.severity_bands.medium = 2.0;
        tenant_b_policy.severity_bands_isolated = Some(DpegSeverityBands {
            medium: 2.0,
            high: 4.0,
            critical: 8.0,
        });
        tenant_b_policy.min_confidence_to_fire = 0.0;

        let tenant_a = evaluate_policy(
            &tenant_a_policy,
            &quotes,
            &DpegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("tenant-a evaluation");
        let tenant_b = evaluate_policy(
            &tenant_b_policy,
            &quotes,
            &DpegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("tenant-b evaluation");

        assert!(tenant_a.snapshot.severity.is_some());
        assert!(tenant_a.snapshot.breach_active);
        assert!(tenant_b.snapshot.severity.is_none());
        assert!(!tenant_b.snapshot.breach_active);
    }

    #[test]
    fn effective_policy_is_scoped_by_tenant_and_market() {
        let mut pattern = DpegPattern::default();
        let usdt_default = DpegPolicy {
            market_key: "USDT/USD".to_string(),
            ..base_policy()
        };
        pattern.policies.insert(
            (
                usdt_default.tenant_id.clone(),
                usdt_default.market_key.clone(),
            ),
            usdt_default,
        );

        let usdc_policy = DpegPolicy {
            market_key: "USDC/USD".to_string(),
            ..base_policy()
        };
        pattern.policies.insert(
            (
                usdc_policy.tenant_id.clone(),
                usdc_policy.market_key.clone(),
            ),
            usdc_policy,
        );

        assert!(pattern.effective_policy("tenant-a", "USDC/USD").is_some());
        assert!(pattern.effective_policy("tenant-a", "USDT/USD").is_some());
        assert!(pattern
            .effective_policy("other-tenant", "USDC/USD")
            .is_none());
    }

    #[test]
    fn deescalation_requires_configured_block_count() {
        let now = Utc::now();
        let policy = base_policy();
        let quotes = vec![quote("cex-a", "cex", 0.994, now)];
        let mut state = DpegAlertState {
            breach_started_at: Some(now),
            cooldown_until: None,
            last_alerted_at: Some(now),
            last_divergence_pct: Some(1.2),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        for i in 1..=policy.deescalation_blocks {
            let ts = now + Duration::seconds(i);
            let outcome = evaluate_policy(
                &policy,
                &quotes,
                &state,
                ts,
                ContextClassification::Isolated,
            )
            .expect("evaluation");
            if i < policy.deescalation_blocks {
                assert!(!outcome.should_emit_alert);
            } else {
                assert!(outcome.should_emit_alert);
                assert!(matches!(
                    outcome.transition,
                    Some(IncidentTransition::Deescalate)
                ));
            }
            state = outcome.next_state;
        }
    }

    #[test]
    fn resolution_requires_configured_block_count() {
        let now = Utc::now();
        let policy = base_policy();
        let quotes = vec![quote("cex-a", "cex", 0.9998, now)];
        let mut state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now),
            last_divergence_pct: Some(0.8),
            last_severity: Some("medium".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        for i in 1..=policy.resolution_blocks {
            let ts = now + Duration::seconds(i);
            let outcome = evaluate_policy(
                &policy,
                &quotes,
                &state,
                ts,
                ContextClassification::Isolated,
            )
            .expect("evaluation");
            if i < policy.resolution_blocks {
                assert!(!outcome.should_emit_alert);
            } else {
                assert!(outcome.should_emit_alert);
                assert!(matches!(
                    outcome.transition,
                    Some(IncidentTransition::Resolve)
                ));
            }
            state = outcome.next_state;
        }
    }

    #[test]
    fn escalation_respects_cooldown_before_emitting() {
        let now = Utc::now();
        let mut policy = base_policy();
        policy.cooldown_sec = 300;
        policy.min_confidence_to_fire = 0.0;
        policy.stale_timeout_ms = 300_000;
        let quotes = vec![quote("cex-a", "cex", 0.989, now)];
        let state = DpegAlertState {
            breach_started_at: Some(now - Duration::seconds(120)),
            cooldown_until: Some(now + Duration::seconds(120)),
            last_alerted_at: Some(now - Duration::seconds(10)),
            last_divergence_pct: Some(0.6),
            last_severity: Some("medium".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let suppressed = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert!(!suppressed.should_emit_alert);
        assert!(suppressed.transition.is_none());
        assert_eq!(
            suppressed.next_state.last_severity.as_deref(),
            Some("medium")
        );

        let emitted = evaluate_policy(
            &policy,
            &quotes,
            &suppressed.next_state,
            now + Duration::seconds(121),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert!(emitted.should_emit_alert);
        assert!(matches!(
            emitted.transition,
            Some(IncidentTransition::Escalate)
        ));
        assert_eq!(emitted.next_state.last_severity.as_deref(), Some("high"));
    }

    #[test]
    fn deescalation_waits_for_cooldown_expiry() {
        let now = Utc::now();
        let mut policy = base_policy();
        policy.cooldown_sec = 300;
        policy.deescalation_blocks = 2;
        policy.min_confidence_to_fire = 0.0;
        policy.stale_timeout_ms = 300_000;
        let quotes = vec![quote("cex-a", "cex", 0.994, now)];
        let state = DpegAlertState {
            breach_started_at: Some(now - Duration::seconds(120)),
            cooldown_until: Some(now + Duration::seconds(60)),
            last_alerted_at: Some(now - Duration::seconds(10)),
            last_divergence_pct: Some(1.2),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 1,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let suppressed = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert!(!suppressed.should_emit_alert);
        assert!(suppressed.transition.is_none());
        assert_eq!(suppressed.next_state.last_severity.as_deref(), Some("high"));
        assert_eq!(suppressed.next_state.below_severity_blocks, 2);

        let emitted = evaluate_policy(
            &policy,
            &quotes,
            &suppressed.next_state,
            now + Duration::seconds(61),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert!(emitted.should_emit_alert);
        assert!(matches!(
            emitted.transition,
            Some(IncidentTransition::Deescalate)
        ));
        assert_eq!(emitted.next_state.last_severity.as_deref(), Some("medium"));
        assert_eq!(emitted.next_state.below_severity_blocks, 0);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // BA/QA Test Suite — Test_Cases_DepegV1_0
    // Spec Reference: RAKSHA_Depeg_Detection_Rule_Spec_V1_0
    // ═══════════════════════════════════════════════════════════════════════

    // ─── Test Harness Helpers ──────────────────────────────────────────────

    /// Deviation percentage to price (e.g., 0.50 → $0.995 for peg_target=1.0).
    fn price_from_deviation(peg_target: f64, deviation_pct: f64) -> f64 {
        peg_target * (1.0 - deviation_pct / 100.0)
    }

    /// Build quotes matching the spec's make_snapshot() concept.
    /// Creates `cex_count` CEX quotes at `median_price`, plus optional oracle quotes.
    fn make_quotes(
        median_price: f64,
        chainlink_price: Option<f64>,
        pyth_price: Option<f64>,
        cex_count: usize,
        now: DateTime<Utc>,
    ) -> Vec<QuoteInput> {
        let mut quotes = Vec::new();
        for i in 0..cex_count {
            quotes.push(quote(&format!("cex-{}", i), "cex", median_price, now));
        }
        if let Some(cl) = chainlink_price {
            quotes.push(quote("chainlink", "oracle", cl, now));
        }
        if let Some(py) = pyth_price {
            quotes.push(quote("pyth", "oracle", py, now));
        }
        quotes
    }

    /// Build a policy matching the spec's test fixture defaults.
    /// oracle_confirmation = true, min_sources = 3, min_confidence = 0.
    fn spec_policy() -> DpegPolicy {
        DpegPolicy {
            toggles: DpegToggles {
                oracle_confirmation: true,
                ..Default::default()
            },
            min_sources: 3,
            source_filter: DpegSourceFilter {
                min_healthy_sources: 3,
                ..Default::default()
            },
            // Disable confidence gate so detection depends only on quorum+threshold+oracle
            min_confidence_to_fire: 0.0,
            ..base_policy()
        }
    }

    /// Evaluate with fresh (default) state.
    fn eval_fresh(
        policy: &DpegPolicy,
        quotes: &[QuoteInput],
        now: DateTime<Utc>,
        classification: ContextClassification,
    ) -> EvaluationOutcome {
        evaluate_policy(
            policy,
            quotes,
            &DpegAlertState::default(),
            now,
            classification,
        )
        .expect("evaluate_policy should not fail")
    }

    /// Run N evaluation steps, threading state. Returns all outcomes.
    #[allow(dead_code)]
    fn eval_sequence(
        policy: &DpegPolicy,
        steps: &[(Vec<QuoteInput>, ContextClassification)],
        start: DateTime<Utc>,
        tick: Duration,
    ) -> Vec<EvaluationOutcome> {
        let mut state = DpegAlertState::default();
        let mut outcomes = Vec::new();
        for (i, (quotes, class)) in steps.iter().enumerate() {
            let ts = start + tick * i as i32;
            let outcome = evaluate_policy(policy, quotes, &state, ts, class.clone())
                .expect("evaluate_policy should not fail");
            state = outcome.next_state.clone();
            outcomes.push(outcome);
        }
        outcomes
    }

    fn assert_no_alert(outcome: &EvaluationOutcome) {
        assert!(
            !outcome.should_emit_alert,
            "Expected no alert, but got one (severity={:?}, transition={:?})",
            outcome.emitted_severity, outcome.transition
        );
    }

    fn assert_alert_with_severity(outcome: &EvaluationOutcome, expected: Severity) {
        assert!(
            outcome.should_emit_alert,
            "Expected alert with severity {:?}, but no alert emitted",
            expected
        );
        assert_eq!(
            outcome.emitted_severity,
            Some(expected.clone()),
            "Severity mismatch"
        );
    }

    fn assert_transition(outcome: &EvaluationOutcome, expected: IncidentTransition) {
        assert_eq!(outcome.transition, Some(expected), "Transition mismatch");
    }

    fn assert_breach(outcome: &EvaluationOutcome, active: bool) {
        assert_eq!(
            outcome.snapshot.breach_active, active,
            "breach_active mismatch"
        );
    }

    // ─── 1. Detection Gates (TC-D-100 to TC-D-110) ────────────────────────

    /// TC-D-100: No detection — normal market conditions (ISOLATED)
    /// 0.10% is well below the isolated medium floor of 0.50%.
    #[test]
    fn tc_d_100_no_detection_normal_market() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.10);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_no_alert(&outcome);
        assert_breach(&outcome, false);
    }

    /// TC-D-101: No detection — below isolated medium floor (0.40% < 0.50%)
    #[test]
    fn tc_d_101_no_detection_below_isolated_floor() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.40);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_no_alert(&outcome);
        assert_breach(&outcome, false);
    }

    /// TC-D-102: No detection — insufficient healthy sources (2 < MIN_HEALTHY_SOURCES=3)
    #[test]
    fn tc_d_102_no_detection_insufficient_healthy_sources() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 1.50);
        // Only 2 CEX sources, policy requires min 3
        let mut policy = spec_policy();
        // Use 2 sources total with oracle off so quorum fails
        policy.toggles.oracle_confirmation = false;
        policy.source_filter.include_oracles = false;
        let quotes_no_oracle = vec![quote("cex-a", "cex", p, now), quote("cex-b", "cex", p, now)];
        let outcome = eval_fresh(
            &policy,
            &quotes_no_oracle,
            now,
            ContextClassification::Isolated,
        );
        assert_no_alert(&outcome);
        assert!(!outcome.snapshot.quorum_met);
    }

    /// TC-D-103: No detection — median breached but NO oracle breached
    #[test]
    fn tc_d_103_no_detection_median_breached_no_oracle() {
        let now = Utc::now();
        let cex_price = price_from_deviation(1.0, 1.50);
        let oracle_price = price_from_deviation(1.0, 0.20); // oracles see near-peg
        let quotes = make_quotes(cex_price, Some(oracle_price), Some(oracle_price), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_no_alert(&outcome);
        assert!(!outcome.snapshot.oracle_confirmed);
    }

    /// TC-D-104: No detection — oracle breached but median below threshold
    #[test]
    fn tc_d_104_no_detection_oracle_breached_median_below() {
        let now = Utc::now();
        let cex_price = price_from_deviation(1.0, 0.20); // CEX near peg
        let oracle_price = price_from_deviation(1.0, 1.50); // oracles see depeg
        let quotes = make_quotes(cex_price, Some(oracle_price), Some(oracle_price), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_no_alert(&outcome);
        assert_breach(&outcome, false);
    }

    /// TC-D-105: Detection fires — one oracle sufficient (Chainlink only)
    #[test]
    fn tc_d_105_detection_chainlink_only_confirms() {
        let now = Utc::now();
        let breach_price = price_from_deviation(1.0, 1.50);
        let safe_price = price_from_deviation(1.0, 0.20);
        let quotes = make_quotes(breach_price, Some(breach_price), Some(safe_price), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_breach(&outcome, true);
        assert_eq!(outcome.snapshot.severity, Some(Severity::High));
    }

    /// TC-D-106: Detection fires — Pyth only breached (Chainlink below)
    #[test]
    fn tc_d_106_detection_pyth_only_confirms() {
        let now = Utc::now();
        let breach_price = price_from_deviation(1.0, 1.50);
        let safe_price = price_from_deviation(1.0, 0.30);
        let quotes = make_quotes(breach_price, Some(safe_price), Some(breach_price), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_breach(&outcome, true);
        assert_eq!(outcome.snapshot.severity, Some(Severity::High));
    }

    /// TC-D-107: Detection fires — exactly at MIN_HEALTHY_SOURCES boundary (3)
    #[test]
    fn tc_d_107_detection_at_min_healthy_boundary() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 1.00);
        // 3 CEX + oracle = 5 total, but min_healthy=3 met by CEX count
        let quotes = make_quotes(p, Some(p), Some(p), 3, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert!(outcome.snapshot.quorum_met);
        assert_breach(&outcome, true);
        assert_eq!(outcome.snapshot.severity, Some(Severity::High));
    }

    /// TC-D-108: No detection — Chainlink absent, Pyth below threshold
    #[test]
    fn tc_d_108_no_detection_chainlink_absent_pyth_below() {
        let now = Utc::now();
        let cex_price = price_from_deviation(1.0, 1.50);
        let pyth_price = price_from_deviation(1.0, 0.20);
        let quotes = make_quotes(cex_price, None, Some(pyth_price), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert!(!outcome.snapshot.oracle_confirmed);
        assert_breach(&outcome, false);
    }

    /// TC-D-109: Detection fires — Chainlink absent, Pyth breaches
    #[test]
    fn tc_d_109_detection_chainlink_absent_pyth_breaches() {
        let now = Utc::now();
        let breach_price = price_from_deviation(1.0, 1.50);
        let quotes = make_quotes(breach_price, None, Some(breach_price), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert!(outcome.snapshot.oracle_confirmed);
        assert_breach(&outcome, true);
        assert_eq!(outcome.snapshot.severity, Some(Severity::High));
    }

    /// TC-D-110: No detection — both oracles absent, even at critical median
    #[test]
    fn tc_d_110_no_detection_both_oracles_absent() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 5.50);
        let quotes = make_quotes(p, None, None, 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert!(!outcome.snapshot.oracle_confirmed);
        assert_breach(&outcome, false);
    }

    // ─── 2. Severity Computation — Isolated (TC-D-200 to TC-D-206) ────────

    /// TC-D-200: Isolated MEDIUM — at floor (0.50%)
    #[test]
    fn tc_d_200_isolated_medium_at_floor() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.50);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Medium));
    }

    /// TC-D-201: Isolated MEDIUM — below HIGH boundary (0.99%)
    #[test]
    fn tc_d_201_isolated_medium_below_high() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.99);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Medium));
    }

    /// TC-D-202: Isolated HIGH — at threshold (1.00%)
    #[test]
    fn tc_d_202_isolated_high_at_threshold() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 1.00);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::High));
    }

    /// TC-D-203: Isolated HIGH — moderate deviation (2.50%)
    #[test]
    fn tc_d_203_isolated_high_moderate() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 2.50);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::High));
    }

    /// TC-D-204: Isolated HIGH — just below CRITICAL (4.99%)
    #[test]
    fn tc_d_204_isolated_high_below_critical() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 4.99);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::High));
    }

    /// TC-D-205: Isolated CRITICAL — at threshold (5.00%)
    #[test]
    fn tc_d_205_isolated_critical_at_threshold() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 5.00);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Critical));
    }

    /// TC-D-206: Isolated CRITICAL — catastrophic deviation (5.50%)
    #[test]
    fn tc_d_206_isolated_critical_catastrophic() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 5.50);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Critical));
    }

    // ─── 3. Severity Computation — Systemic (TC-D-300 to TC-D-307) ────────

    /// TC-D-300: Systemic MEDIUM — at floor (0.01%)
    /// Uses 0.015% to clear floating-point boundary (0.01% exact hits fp precision).
    #[test]
    fn tc_d_300_systemic_medium_at_floor() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.015);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Systemic,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Medium));
    }

    /// TC-D-301: Systemic MEDIUM — between floor and CRITICAL (0.10%)
    #[test]
    fn tc_d_301_systemic_medium_between_floor_and_critical() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.10);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Systemic,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Medium));
    }

    /// TC-D-302: Systemic MEDIUM — just below CRITICAL (0.24%)
    #[test]
    fn tc_d_302_systemic_medium_below_critical() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.24);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Systemic,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Medium));
    }

    /// TC-D-303: Systemic at 0.25% maps to CRITICAL under the compatibility ladder
    /// (medium=0.01, high=0.25, critical=0.25). Uses 0.26% to clear fp boundary.
    #[test]
    fn tc_d_303_systemic_at_025_pct() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.26);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Systemic,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Critical));
    }

    /// TC-D-304: Systemic CRITICAL — large deviation (2.50%)
    #[test]
    fn tc_d_304_systemic_critical_large() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 2.50);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Systemic,
        );
        assert_eq!(outcome.snapshot.severity, Some(Severity::Critical));
    }

    /// TC-D-305: Same deviation (0.10%) produces different outcomes based on contagion
    #[test]
    fn tc_d_305_systemic_vs_isolated_same_deviation() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.10);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let policy = spec_policy();

        // Isolated: 0.10% < 0.50% floor → no severity
        let isolated = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_eq!(isolated.snapshot.severity, None);
        assert_breach(&isolated, false);

        // Systemic: 0.10% >= 0.01% floor → MEDIUM
        let systemic = eval_fresh(&policy, &quotes, now, ContextClassification::Systemic);
        assert_eq!(systemic.snapshot.severity, Some(Severity::Medium));
        assert_breach(&systemic, true);
    }

    /// TC-D-306: No detection — systemic but below systemic floor (0.005%)
    #[test]
    fn tc_d_306_no_detection_systemic_below_floor() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.005);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let outcome = eval_fresh(
            &spec_policy(),
            &quotes,
            now,
            ContextClassification::Systemic,
        );
        assert_eq!(outcome.snapshot.severity, None);
        assert_breach(&outcome, false);
    }

    /// TC-D-307: Contagion UNKNOWN (None) treated as ISOLATED
    #[test]
    fn tc_d_307_unknown_contagion_treated_as_isolated() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.10);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        // ContextClassification::None is the "Unknown" equivalent
        let outcome = eval_fresh(&spec_policy(), &quotes, now, ContextClassification::None);
        // Under isolated thresholds, 0.10% < 0.50% → no detection
        assert_eq!(outcome.snapshot.severity, None);
        assert_breach(&outcome, false);
    }

    // ─── 4. Contagion Escalation (TC-D-400 to TC-D-402) ───────────────────

    /// TC-D-400: Contagion flip escalates severity (MEDIUM→CRITICAL) with same price
    #[test]
    fn tc_d_400_contagion_flip_escalates_severity() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.60);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let policy = spec_policy();

        // Step 1: Isolated — 0.60% → MEDIUM, triggers
        let step1 = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_eq!(step1.snapshot.severity, Some(Severity::Medium));
        assert_transition(&step1, IncidentTransition::Trigger);

        // Step 2: Same price but contagion flips to SYSTEMIC → CRITICAL (0.60% >= 0.5% critical)
        let step2 = evaluate_policy(
            &policy,
            &quotes,
            &step1.next_state,
            now + Duration::seconds(1),
            ContextClassification::Systemic,
        )
        .expect("evaluation");
        assert_eq!(step2.snapshot.severity, Some(Severity::Critical));
        // Severity went up: should escalate
        assert_transition(&step2, IncidentTransition::Escalate);
    }

    /// TC-D-401: Contagion flip creates NEW alert that was previously suppressed
    #[test]
    fn tc_d_401_contagion_flip_creates_new_alert() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.02);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let policy = spec_policy();

        // Isolated: 0.02% < 0.50% → no detection
        let isolated = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_breach(&isolated, false);

        // Systemic: 0.02% >= 0.01% → MEDIUM, triggers new incident
        let systemic = eval_fresh(&policy, &quotes, now, ContextClassification::Systemic);
        assert_eq!(systemic.snapshot.severity, Some(Severity::Medium));
        assert_breach(&systemic, true);
    }

    /// TC-D-402: USDT recovers — contagion reverts but severity does NOT de-escalate immediately
    /// Severity is a high-water mark within deescalation_blocks window.
    #[test]
    fn tc_d_402_contagion_reverts_no_immediate_deescalation() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 1.50);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let policy = spec_policy();

        // Step 1: Systemic → CRITICAL
        let step1 = eval_fresh(&policy, &quotes, now, ContextClassification::Systemic);
        assert_eq!(step1.snapshot.severity, Some(Severity::Critical));
        assert_transition(&step1, IncidentTransition::Trigger);

        // Step 2: Contagion reverts to Isolated, same price → severity = HIGH (isolated bands)
        // But current severity is CRITICAL from step1, so equal rank → no emit
        let step2 = evaluate_policy(
            &policy,
            &quotes,
            &step1.next_state,
            now + Duration::seconds(1),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // Under isolated bands 1.50% → HIGH, which is lower than CRITICAL
        // below_severity_blocks increments but no deescalation until deescalation_blocks reached
        assert!(
            !step2.should_emit_alert
                || !matches!(step2.transition, Some(IncidentTransition::Deescalate))
        );
    }

    // ─── 5. Resolution Logic (TC-D-500 to TC-D-505) ──────────────────────

    /// TC-D-500: should_resolve returns true — all metrics below threshold
    #[test]
    fn tc_d_500_resolution_all_below_threshold() {
        let now = Utc::now();
        let recovery_price = price_from_deviation(1.0, 0.10);
        let quotes = make_quotes(
            recovery_price,
            Some(recovery_price),
            Some(recovery_price),
            12,
            now,
        );
        let policy = spec_policy();

        // Set up state as if there was an active ISOLATED incident
        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(120)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // Price is below trigger floor → below_trigger_blocks should increment
        assert!(outcome.next_state.below_trigger_blocks > 0);
    }

    /// TC-D-501: should_resolve returns false — still depegged
    #[test]
    fn tc_d_501_no_resolution_still_depegged() {
        let now = Utc::now();
        let depeg_price = price_from_deviation(1.0, 1.50);
        let quotes = make_quotes(depeg_price, Some(depeg_price), Some(depeg_price), 12, now);
        let policy = spec_policy();

        let state = DpegAlertState {
            breach_started_at: Some(now - Duration::seconds(60)),
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(30)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // Still breaching → below_trigger_blocks stays 0
        assert_eq!(outcome.next_state.below_trigger_blocks, 0);
        assert!(outcome.transition != Some(IncidentTransition::Resolve));
    }

    /// TC-D-502: Resolution uses SYSTEMIC threshold for systemically-triggered incident
    #[test]
    fn tc_d_502_resolution_uses_original_systemic_threshold() {
        let now = Utc::now();
        // Price at 0.005% deviation — below systemic floor (0.01%)
        let recovery_price = price_from_deviation(1.0, 0.005);
        let quotes = make_quotes(
            recovery_price,
            Some(recovery_price),
            Some(recovery_price),
            12,
            now,
        );
        let mut policy = spec_policy();
        policy.resolution_blocks = 1; // Fast resolution for test

        // Incident was triggered under SYSTEMIC (trigger_floor_pct = 0.01)
        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(60)),
            last_divergence_pct: Some(0.05),
            last_severity: Some("medium".to_string()),
            last_classification: Some("systemic".to_string()),
            trigger_floor_pct: Some(0.01), // Systemic floor stored from trigger
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // 0.005% < 0.01% (systemic trigger floor) → resolves
        assert_transition(&outcome, IncidentTransition::Resolve);
    }

    /// TC-D-503: No resolution for systemically-triggered incident above systemic floor
    #[test]
    fn tc_d_503_no_resolution_above_systemic_floor() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.02);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let mut policy = spec_policy();
        policy.resolution_blocks = 1;

        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(60)),
            last_divergence_pct: Some(0.05),
            last_severity: Some("medium".to_string()),
            last_classification: Some("systemic".to_string()),
            trigger_floor_pct: Some(0.01),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // 0.02% >= 0.01% trigger floor → no resolution
        assert_eq!(outcome.next_state.below_trigger_blocks, 0);
        assert!(outcome.transition != Some(IncidentTransition::Resolve));
    }

    #[test]
    fn tc_d_504_resolution_blocked_by_single_oracle() {
        let now = Utc::now();
        let median_recovery_price = price_from_deviation(1.0, 0.10);
        let blocking_oracle_price = price_from_deviation(1.0, 0.80);
        let quotes = vec![
            quote("cex-a", "cex", median_recovery_price, now),
            quote("cex-b", "cex", median_recovery_price, now),
            quote("chainlink", "oracle", median_recovery_price, now),
            quote("pyth", "oracle", blocking_oracle_price, now),
        ];
        let mut policy = spec_policy();
        policy.resolution_blocks = 1;

        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(60)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert_eq!(outcome.next_state.below_trigger_blocks, 0);
        assert!(outcome.transition != Some(IncidentTransition::Resolve));
    }

    /// TC-D-505: Resolution passes when unavailable oracle is not blocking
    #[test]
    fn tc_d_505_resolution_passes_with_absent_oracle() {
        let now = Utc::now();
        let recovery_price = price_from_deviation(1.0, 0.10);
        // No oracle quotes — only CEX
        let quotes = make_quotes(recovery_price, None, None, 12, now);
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false; // Don't require oracle for detection either
        policy.resolution_blocks = 1;

        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(60)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // 0.10% < 0.50% → resolves (absent oracles don't block)
        assert_transition(&outcome, IncidentTransition::Resolve);
    }

    // ─── 6. State Manager — Multi-Block Resolution Lifecycle (TC-D-600 to TC-D-603) ──

    /// TC-D-600: Full resolution countdown — ACTIVE → RESOLVING → RESOLVED
    #[test]
    fn tc_d_600_full_resolution_countdown() {
        let now = Utc::now();
        let recovery_price = price_from_deviation(1.0, 0.10);
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false;
        policy.resolution_blocks = 30;

        let mut state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(120)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        // Run 30 consecutive below-floor evaluations
        for i in 1..=30 {
            let ts = now + Duration::seconds(i);
            let quotes = make_quotes(recovery_price, None, None, 12, ts);
            let outcome = evaluate_policy(
                &policy,
                &quotes,
                &state,
                ts,
                ContextClassification::Isolated,
            )
            .expect("evaluation");
            if i < 30 {
                assert!(!outcome.should_emit_alert, "Should not emit at block {}", i);
                assert_eq!(outcome.next_state.below_trigger_blocks, i);
            } else {
                assert!(outcome.should_emit_alert, "Should emit resolve at block 30");
                assert_transition(&outcome, IncidentTransition::Resolve);
            }
            state = outcome.next_state;
        }
    }

    /// TC-D-601: Resolution interrupted — counter resets to ACTIVE
    #[test]
    fn tc_d_601_resolution_interrupted_resets() {
        let now = Utc::now();
        let breach_price = price_from_deviation(1.0, 1.50);
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false;
        policy.resolution_blocks = 30;

        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(120)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 15, // Halfway through resolution
            high_water_mark: None,
        };

        // Breach resumes — counter should reset
        let quotes = make_quotes(breach_price, None, None, 12, now);
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_eq!(outcome.next_state.below_trigger_blocks, 0);
    }

    /// TC-D-602: Resolution interrupted, then resumes from zero
    #[test]
    fn tc_d_602_resolution_resumes_from_zero() {
        let now = Utc::now();
        let recovery_price = price_from_deviation(1.0, 0.10);
        let breach_price = price_from_deviation(1.0, 1.50);
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false;
        policy.resolution_blocks = 30;

        let base_state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(120)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        // 15 blocks recovering
        let mut state = base_state;
        for i in 1..=15 {
            let ts = now + Duration::seconds(i);
            let quotes = make_quotes(recovery_price, None, None, 12, ts);
            let outcome = evaluate_policy(
                &policy,
                &quotes,
                &state,
                ts,
                ContextClassification::Isolated,
            )
            .expect("evaluation");
            state = outcome.next_state;
        }
        assert_eq!(state.below_trigger_blocks, 15);

        // Block 115: breach resumes
        let ts = now + Duration::seconds(16);
        let quotes = make_quotes(breach_price, None, None, 12, ts);
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            ts,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        state = outcome.next_state;
        assert_eq!(state.below_trigger_blocks, 0);

        // Block 116: recovery again — starts from 1, NOT 16
        let ts = now + Duration::seconds(17);
        let quotes = make_quotes(recovery_price, None, None, 12, ts);
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            ts,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_eq!(outcome.next_state.below_trigger_blocks, 1);
    }

    /// TC-D-603: Resolution payload structure — transition=Resolve, severity cleared
    #[test]
    fn tc_d_603_resolution_payload_structure() {
        let now = Utc::now();
        let recovery_price = price_from_deviation(1.0, 0.10);
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false;
        policy.resolution_blocks = 1; // Fast resolution

        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(60)),
            last_divergence_pct: Some(1.5),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let quotes = make_quotes(recovery_price, None, None, 12, now);
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert_transition(&outcome, IncidentTransition::Resolve);
        // Emitted severity is the original incident severity (high)
        assert_eq!(outcome.emitted_severity, Some(Severity::High));
        // After resolution, state is cleared
        assert!(outcome.next_state.last_severity.is_none());
        assert!(outcome.next_state.last_classification.is_none());
        assert!(outcome.next_state.trigger_floor_pct.is_none());
        assert_eq!(outcome.next_state.below_trigger_blocks, 0);
        // Cooldown is set
        assert!(outcome.next_state.cooldown_until.is_some());
    }

    // ─── 7. Dedup / Escalation / Suppression (TC-D-700 to TC-D-704) ──────

    /// TC-D-700: Duplicate detection suppressed — same severity, no new alert (cooldown)
    #[test]
    fn tc_d_700_duplicate_detection_suppressed() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 0.60);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let policy = spec_policy();

        // First trigger
        let step1 = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_transition(&step1, IncidentTransition::Trigger);

        // Second evaluation at same severity during cooldown — no new alert
        let step2 = evaluate_policy(
            &policy,
            &quotes,
            &step1.next_state,
            now + Duration::seconds(1),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_no_alert(&step2);
    }

    /// TC-D-701: Escalation — severity increases from MEDIUM → CRITICAL
    #[test]
    fn tc_d_701_escalation_higher_severity() {
        let now = Utc::now();
        let medium_price = price_from_deviation(1.0, 0.60);
        let critical_price = price_from_deviation(1.0, 5.50);
        let mut policy = spec_policy();
        policy.cooldown_sec = 0; // No cooldown for test

        // Step 1: Trigger at MEDIUM
        let quotes1 = make_quotes(
            medium_price,
            Some(medium_price),
            Some(medium_price),
            12,
            now,
        );
        let step1 = eval_fresh(&policy, &quotes1, now, ContextClassification::Isolated);
        assert_alert_with_severity(&step1, Severity::Medium);
        assert_transition(&step1, IncidentTransition::Trigger);

        // Step 2: Escalate to CRITICAL
        let quotes2 = make_quotes(
            critical_price,
            Some(critical_price),
            Some(critical_price),
            12,
            now,
        );
        let step2 = evaluate_policy(
            &policy,
            &quotes2,
            &step1.next_state,
            now + Duration::seconds(1),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_alert_with_severity(&step2, Severity::Critical);
        assert_transition(&step2, IncidentTransition::Escalate);
    }

    /// TC-D-702: Escalation at equal or lower severity — no-op
    #[test]
    fn tc_d_702_escalation_equal_severity_noop() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 5.50);
        let lower_p = price_from_deviation(1.0, 0.60);
        let mut policy = spec_policy();
        policy.cooldown_sec = 0;

        // Trigger at CRITICAL
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let step1 = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_alert_with_severity(&step1, Severity::Critical);

        // Same severity — no emit
        let step2 = evaluate_policy(
            &policy,
            &quotes,
            &step1.next_state,
            now + Duration::seconds(1),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_no_alert(&step2);

        // Lower severity (MEDIUM) — starts deescalation counter, no immediate emit
        let quotes_low = make_quotes(lower_p, Some(lower_p), Some(lower_p), 12, now);
        let step3 = evaluate_policy(
            &policy,
            &quotes_low,
            &step2.next_state,
            now + Duration::seconds(2),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // Below severity blocks increments, but not enough to deescalate
        assert!(
            !step3.should_emit_alert
                || !matches!(step3.transition, Some(IncidentTransition::Escalate))
        );
    }

    /// TC-D-703: Cooldown prevents re-trigger after resolution
    #[test]
    fn tc_d_703_cooldown_prevents_retrigger() {
        let now = Utc::now();
        let breach_price = price_from_deviation(1.0, 1.50);
        let mut policy = spec_policy();
        policy.cooldown_sec = 300;

        // State: just resolved, cooldown active
        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: Some(now + Duration::seconds(250)),
            last_alerted_at: Some(now - Duration::seconds(5)),
            last_divergence_pct: None,
            last_severity: None, // Cleared by resolution
            last_classification: None,
            trigger_floor_pct: None,
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let quotes = make_quotes(
            breach_price,
            Some(breach_price),
            Some(breach_price),
            12,
            now,
        );
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // Breach detected but cooldown blocks alert emission
        assert_no_alert(&outcome);
    }

    /// TC-D-704: Cooldown expired — new incident allowed
    #[test]
    fn tc_d_704_cooldown_expired_new_incident() {
        let now = Utc::now();
        let breach_price = price_from_deviation(1.0, 1.50);
        let mut policy = spec_policy();
        policy.cooldown_sec = 300;

        // State: resolved long ago, cooldown expired
        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: Some(now - Duration::seconds(10)), // Expired
            last_alerted_at: Some(now - Duration::seconds(310)),
            last_divergence_pct: None,
            last_severity: None,
            last_classification: None,
            trigger_floor_pct: None,
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let quotes = make_quotes(
            breach_price,
            Some(breach_price),
            Some(breach_price),
            12,
            now,
        );
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_alert_with_severity(&outcome, Severity::High);
        assert_transition(&outcome, IncidentTransition::Trigger);
    }

    // ─── 7b. Terminal States & Dedup (TC-D-705 to TC-D-708) ─────────────────

    /// TC-D-705: RESOLVED state — subsequent breach doesn't immediately re-escalate
    /// (simulates terminal state: after resolution + cooldown active, no transition)
    #[test]
    fn tc_d_705_resolved_cannot_transition_during_cooldown() {
        let now = Utc::now();
        let breach_price = price_from_deviation(1.0, 1.50);
        let mut policy = spec_policy();
        policy.cooldown_sec = 300;

        // State after resolution: severity cleared, cooldown active
        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: Some(now + Duration::seconds(250)),
            last_alerted_at: Some(now - Duration::seconds(5)),
            last_divergence_pct: None,
            last_severity: None,
            last_classification: None,
            trigger_floor_pct: None,
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let quotes = make_quotes(
            breach_price,
            Some(breach_price),
            Some(breach_price),
            12,
            now,
        );
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // Cooldown blocks any new trigger — terminal-like behavior
        assert_no_alert(&outcome);
        assert_eq!(outcome.transition, None);
    }

    /// TC-D-706: FALSE_POSITIVE terminal — once severity is cleared and cooldown
    /// is active, no re-trigger is possible (mirrors false-positive state).
    #[test]
    fn tc_d_706_false_positive_state_blocks_retrigger() {
        let now = Utc::now();
        let critical_price = price_from_deviation(1.0, 5.50);
        let mut policy = spec_policy();
        policy.cooldown_sec = 600;

        // Simulate a false-positive closure: severity cleared, long cooldown
        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: Some(now + Duration::seconds(500)),
            last_alerted_at: Some(now - Duration::seconds(10)),
            last_divergence_pct: None,
            last_severity: None,
            last_classification: None,
            trigger_floor_pct: None,
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let quotes = make_quotes(
            critical_price,
            Some(critical_price),
            Some(critical_price),
            12,
            now,
        );
        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_no_alert(&outcome);
    }

    /// TC-D-707: Deescalation requires configured block count — cannot skip
    #[test]
    fn tc_d_707_deescalation_requires_block_count() {
        let now = Utc::now();
        let medium_price = price_from_deviation(1.0, 0.60);
        let mut policy = spec_policy();
        policy.cooldown_sec = 0;
        policy.deescalation_blocks = 5;

        // Active incident at CRITICAL
        let state = DpegAlertState {
            breach_started_at: Some(now - Duration::seconds(120)),
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(60)),
            last_divergence_pct: Some(5.50),
            last_severity: Some("critical".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        // 1 block at MEDIUM — not enough for deescalation (needs 5)
        let quotes = make_quotes(
            medium_price,
            Some(medium_price),
            Some(medium_price),
            12,
            now,
        );
        let step1 = evaluate_policy(
            &policy,
            &quotes,
            &state,
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        // Should NOT emit deescalation after only 1 block
        assert_no_alert(&step1);
        assert_eq!(step1.next_state.below_severity_blocks, 1);

        // Continue for 3 more blocks (total = 4, still < 5)
        let mut s = step1.next_state;
        for i in 1..4 {
            let ts = now + Duration::seconds(i);
            let q = make_quotes(medium_price, Some(medium_price), Some(medium_price), 12, ts);
            let out = evaluate_policy(&policy, &q, &s, ts, ContextClassification::Isolated)
                .expect("evaluation");
            assert_no_alert(&out);
            s = out.next_state;
        }
        assert_eq!(s.below_severity_blocks, 4);

        // 5th block — deescalation fires
        let ts = now + Duration::seconds(4);
        let q = make_quotes(medium_price, Some(medium_price), Some(medium_price), 12, ts);
        let final_out = evaluate_policy(&policy, &q, &s, ts, ContextClassification::Isolated)
            .expect("evaluation");
        assert_alert_with_severity(&final_out, Severity::Medium);
        assert_transition(&final_out, IncidentTransition::Deescalate);
    }

    /// TC-D-708: Stale state recovery — if last_severity references a state that
    /// no longer makes sense (divergence recovered), resolution countdown runs.
    #[test]
    fn tc_d_708_stale_state_triggers_resolution_countdown() {
        let now = Utc::now();
        let recovery_price = price_from_deviation(1.0, 0.10);
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false;
        policy.resolution_blocks = 3;

        // Stale state: says HIGH but divergence is now 0.10%
        let state = DpegAlertState {
            breach_started_at: None,
            cooldown_until: None,
            last_alerted_at: Some(now - Duration::seconds(300)),
            last_divergence_pct: Some(1.50),
            last_severity: Some("high".to_string()),
            last_classification: Some("isolated".to_string()),
            trigger_floor_pct: Some(0.5),
            below_severity_blocks: 0,
            below_trigger_blocks: 0,
            high_water_mark: None,
        };

        let mut s = state;
        for i in 0..3 {
            let ts = now + Duration::seconds(i);
            let q = make_quotes(recovery_price, None, None, 12, ts);
            let out = evaluate_policy(&policy, &q, &s, ts, ContextClassification::Isolated)
                .expect("evaluation");
            if i < 2 {
                assert!(!out.should_emit_alert);
                assert_eq!(out.next_state.below_trigger_blocks, i + 1);
            } else {
                assert_transition(&out, IncidentTransition::Resolve);
            }
            s = out.next_state;
        }
    }

    // ─── 13. Blast Radius & Recommended Actions (TC-D-1301 to TC-D-1302) ──

    /// TC-D-1301: Recommended actions vary by severity
    #[test]
    fn tc_d_1301_recommended_actions_by_severity() {
        let critical_actions = recommended_actions_for_severity(&Severity::Critical);
        assert_eq!(critical_actions.len(), 3);
        assert!(critical_actions[0].contains("Immediate Rebalance"));
        assert!(critical_actions[1].contains("Exit all positions"));
        assert!(critical_actions[2].contains("Withdraw to Owner"));

        let high_actions = recommended_actions_for_severity(&Severity::High);
        assert_eq!(high_actions.len(), 3);
        assert!(high_actions[0].contains("Partial Exit"));
        assert!(high_actions[1].contains("Rebalance to ETH"));
        assert!(high_actions[2].contains("Hold and Monitor"));

        let medium_actions = recommended_actions_for_severity(&Severity::Medium);
        assert_eq!(medium_actions.len(), 2);
        assert!(medium_actions[0].contains("Hold and Monitor"));

        let info_actions = recommended_actions_for_severity(&Severity::Info);
        assert!(info_actions.is_empty());
    }

    // ─── 14. Signal Detail Payload (TC-D-1400 to TC-D-1401) ─────────────────

    /// TC-D-1400: Detection signal contains all required fields
    #[test]
    fn tc_d_1400_detection_signal_detail_fields() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 5.50);
        let quotes = make_quotes(p, Some(p), Some(p), 12, now);
        let policy = spec_policy();

        let outcome = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_alert_with_severity(&outcome, Severity::Critical);

        // Build the detection payload and verify structure
        let event = UnifiedEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            tenant_id: "test-tenant".to_string(),
            source_type: event_schema::SourceType::CexWebsocket,
            source_id: "binance".to_string(),
            event_type: "quote".to_string(),
            timestamp: now,
            payload: serde_json::Value::Null,
            chain_id: None,
            block_number: None,
            tx_hash: None,
            market_key: Some("USDC/USD".to_string()),
            price: Some(p),
        };

        let detection = build_detection(
            &event,
            &policy,
            &outcome.snapshot,
            Severity::Critical,
            Some(IncidentTransition::Trigger),
            now,
        );

        assert_eq!(detection.pattern_id, "dpeg");
        assert_eq!(detection.severity, Severity::Critical);
        assert_eq!(detection.chain, Chain::Offchain);
        assert_eq!(detection.attack_family, AttackFamily::PegDeviation);
        assert!(detection.subject_type.as_deref() == Some("market"));
        assert!(detection.subject_key.is_some());
        assert!(detection.tenant_id.is_some());
        assert!(detection.event_key.is_some());
        assert_eq!(
            detection.incident_transition,
            Some(IncidentTransition::Trigger)
        );
        assert_eq!(
            detection.context_classification,
            Some(ContextClassification::Isolated)
        );
        assert!(!detection.signals.is_empty());
        assert_eq!(detection.signals[0].signal_type, SignalType::PriceDeviation);
        assert!(detection.signals[0].value > 5.0);
        assert!(!detection.risk_score.rationale.is_empty());
        assert!(!detection.actions_recommended.is_empty());
        assert!(detection.oracle_context.contains_key("oracle_confirmed"));
        assert!(detection
            .oracle_context
            .contains_key("weighted_median_price"));
        assert!(detection.oracle_context.contains_key("trigger_floor_pct"));
        assert!(detection
            .oracle_context
            .contains_key("context_classification"));
        assert!(detection
            .oracle_context
            .contains_key("contributing_sources"));
        assert!(!detection.confidence_breakdown.is_empty());
        assert!(detection.description.is_some());
    }

    /// TC-D-1401: Oracle `breached` flag matches threshold comparison
    #[test]
    fn tc_d_1401_oracle_breached_flag_matches_threshold() {
        let now = Utc::now();
        // Chainlink at 0.60% deviation (above 0.50% floor) → breached
        // Pyth at 0.30% deviation (below 0.50% floor) → not breached
        let median_price = price_from_deviation(1.0, 0.60);
        let chainlink_price = price_from_deviation(1.0, 0.60);
        let pyth_price = price_from_deviation(1.0, 0.30);
        let quotes = make_quotes(
            median_price,
            Some(chainlink_price),
            Some(pyth_price),
            12,
            now,
        );
        let policy = spec_policy();

        let outcome = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);

        // Chainlink alone breaches → oracle_confirmed should be true
        assert!(outcome.snapshot.oracle_confirmed);
        assert!(outcome.snapshot.breach_active);

        // Now test with neither oracle breaching
        let low_chainlink = price_from_deviation(1.0, 0.20);
        let low_pyth = price_from_deviation(1.0, 0.30);
        let quotes2 = make_quotes(median_price, Some(low_chainlink), Some(low_pyth), 12, now);
        let outcome2 = eval_fresh(&policy, &quotes2, now, ContextClassification::Isolated);
        // Oracles don't breach → oracle_confirmed=false → no breach
        assert!(!outcome2.snapshot.oracle_confirmed);
        assert!(!outcome2.snapshot.breach_active);
    }

    // ─── Severity edge cases ────────────────────────────────────────────────

    /// TC-D-1402: severity_for_divergence returns None below all bands
    #[test]
    fn tc_d_1402_severity_none_below_all_bands() {
        let bands = DpegSeverityBands {
            medium: 0.5,
            high: 1.0,
            critical: 5.0,
        };
        assert_eq!(severity_for_divergence(0.0, &bands), None);
        assert_eq!(severity_for_divergence(0.49, &bands), None);
    }

    /// TC-D-1403: severity_for_divergence boundary tests
    #[test]
    fn tc_d_1403_severity_boundaries() {
        let bands = DpegSeverityBands {
            medium: 0.5,
            high: 1.0,
            critical: 5.0,
        };
        assert_eq!(severity_for_divergence(0.5, &bands), Some(Severity::Medium));
        assert_eq!(
            severity_for_divergence(0.99, &bands),
            Some(Severity::Medium)
        );
        assert_eq!(severity_for_divergence(1.0, &bands), Some(Severity::High));
        assert_eq!(severity_for_divergence(4.99, &bands), Some(Severity::High));
        assert_eq!(
            severity_for_divergence(5.0, &bands),
            Some(Severity::Critical)
        );
        assert_eq!(
            severity_for_divergence(99.0, &bands),
            Some(Severity::Critical)
        );
    }

    /// TC-D-1404: Systemic severity bands boundary tests
    #[test]
    fn tc_d_1404_systemic_severity_boundaries() {
        let bands = DpegSeverityBands {
            medium: 0.01,
            high: 0.25,
            critical: 0.25,
        };
        assert_eq!(severity_for_divergence(0.009, &bands), None);
        assert_eq!(
            severity_for_divergence(0.01, &bands),
            Some(Severity::Medium)
        );
        assert_eq!(
            severity_for_divergence(0.24, &bands),
            Some(Severity::Medium)
        );
        assert_eq!(
            severity_for_divergence(0.25, &bands),
            Some(Severity::Critical)
        );
        assert_eq!(
            severity_for_divergence(0.49, &bands),
            Some(Severity::Critical)
        );
        assert_eq!(
            severity_for_divergence(0.5, &bands),
            Some(Severity::Critical)
        );
    }

    // ─── Contagion classification helper ─────────────────────────────────────

    /// TC-D-1405: context_classification_str returns correct strings
    #[test]
    fn tc_d_1405_context_classification_str_values() {
        assert_eq!(
            context_classification_str(&ContextClassification::Isolated),
            "isolated"
        );
        assert_eq!(
            context_classification_str(&ContextClassification::Systemic),
            "systemic"
        );
        assert_eq!(
            context_classification_str(&ContextClassification::None),
            "none"
        );
    }

    /// TC-D-1406: severity_from_str round-trip
    #[test]
    fn tc_d_1406_severity_from_str_roundtrip() {
        assert_eq!(
            severity_from_str(Some("critical")),
            Some(Severity::Critical)
        );
        assert_eq!(severity_from_str(Some("HIGH")), Some(Severity::High));
        assert_eq!(severity_from_str(Some("Medium")), Some(Severity::Medium));
        assert_eq!(severity_from_str(Some("low")), Some(Severity::Low));
        assert_eq!(severity_from_str(Some("info")), Some(Severity::Info));
        assert_eq!(severity_from_str(Some("unknown")), None);
        assert_eq!(severity_from_str(None), None);
    }

    /// TC-D-1407: severity_rank ordering
    #[test]
    fn tc_d_1407_severity_rank_ordering() {
        assert!(severity_rank(Some(&Severity::Critical)) > severity_rank(Some(&Severity::High)));
        assert!(severity_rank(Some(&Severity::High)) > severity_rank(Some(&Severity::Medium)));
        assert!(severity_rank(Some(&Severity::Medium)) > severity_rank(Some(&Severity::Low)));
        assert!(severity_rank(Some(&Severity::Low)) > severity_rank(Some(&Severity::Info)));
        assert!(severity_rank(Some(&Severity::Info)) > severity_rank(None));
        assert_eq!(severity_rank(None), 0);
    }

    // ─── Resolution with oracle floor checks ────────────────────────────────

    /// TC-D-1408: available_oracles_within_resolution_floor — all below
    #[test]
    fn tc_d_1408_oracles_within_resolution_floor() {
        let now = Utc::now();
        let good_oracle = quote("chainlink", "oracle", 0.998, now);
        let good_pyth = quote("pyth", "oracle", 0.999, now);
        let quotes = vec![good_oracle, good_pyth];
        // Both oracles within 0.5% of peg target 1.0
        assert!(available_oracles_within_resolution_floor(&quotes, 0.5, 1.0));
    }

    /// TC-D-1409: available_oracles_within_resolution_floor — one above
    #[test]
    fn tc_d_1409_oracle_above_resolution_floor() {
        let now = Utc::now();
        let good_oracle = quote("chainlink", "oracle", 0.998, now);
        let bad_pyth = quote("pyth", "oracle", 0.990, now); // 1.0% deviation
        let quotes = vec![good_oracle, bad_pyth];
        // Pyth at 1.0% > floor 0.5% → resolution blocked
        assert!(!available_oracles_within_resolution_floor(
            &quotes, 0.5, 1.0
        ));
    }

    /// TC-D-1410: available_oracles_within_resolution_floor — no oracles → true
    #[test]
    fn tc_d_1410_no_oracles_resolution_passes() {
        let now = Utc::now();
        let quotes = vec![quote("binance", "cex", 0.998, now)];
        // No oracle quotes → .all() over empty iterator → true
        assert!(available_oracles_within_resolution_floor(&quotes, 0.5, 1.0));
    }

    // ─── Weighted median edge cases ─────────────────────────────────────────

    /// TC-D-1411: Single source still computes valid median
    #[test]
    fn tc_d_1411_single_source_valid_median() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 1.50);
        let mut policy = spec_policy();
        policy.min_sources = 1;
        policy.source_filter.min_healthy_sources = 1;
        policy.toggles.oracle_confirmation = false;

        let quotes = vec![quote("binance", "cex", p, now)];
        let outcome = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_eq!(outcome.snapshot.source_count, 1);
        assert!(outcome.snapshot.breach_active);
    }

    /// TC-D-1412: Zero-weight source excluded from median
    #[test]
    fn tc_d_1412_zero_weight_source_excluded() {
        let now = Utc::now();
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false;
        policy.min_sources = 1;
        policy.source_filter.min_healthy_sources = 1;
        policy.source_overrides.insert(
            "bad-source".to_string(),
            DpegSourceOverride {
                source_id: "bad-source".to_string(),
                weight: 0.0,
                enabled: true,
                stale_timeout_ms: None,
            },
        );

        let good_price = price_from_deviation(1.0, 0.10); // No breach
        let bad_price = price_from_deviation(1.0, 5.0); // Would breach if included
        let quotes = vec![
            quote("good-source", "cex", good_price, now),
            quote("bad-source", "cex", bad_price, now),
        ];

        let outcome = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        // Only good-source counted (weight > 0), 0.10% < 0.50% → no breach
        assert_eq!(outcome.snapshot.source_count, 1);
        assert!(!outcome.snapshot.breach_active);
    }

    /// TC-D-1413: Disabled source excluded
    #[test]
    fn tc_d_1413_disabled_source_excluded() {
        let now = Utc::now();
        let mut policy = spec_policy();
        policy.toggles.oracle_confirmation = false;
        policy.min_sources = 1;
        policy.source_filter.min_healthy_sources = 1;
        policy.source_overrides.insert(
            "disabled-cex".to_string(),
            DpegSourceOverride {
                source_id: "disabled-cex".to_string(),
                weight: 1.0,
                enabled: false,
                stale_timeout_ms: None,
            },
        );

        let good_price = price_from_deviation(1.0, 0.10);
        let breach_price = price_from_deviation(1.0, 5.0);
        let quotes = vec![
            quote("active-cex", "cex", good_price, now),
            quote("disabled-cex", "cex", breach_price, now),
        ];

        let outcome = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        assert_eq!(outcome.snapshot.source_count, 1);
        assert!(!outcome.snapshot.breach_active);
    }

    // ─── Confidence gate ────────────────────────────────────────────────────

    /// TC-D-1414: min_confidence_to_fire blocks low-confidence alerts
    #[test]
    fn tc_d_1414_confidence_gate_blocks_alert() {
        let now = Utc::now();
        let p = price_from_deviation(1.0, 1.50);
        let mut policy = spec_policy();
        policy.min_confidence_to_fire = 100.1; // Impossible to reach
        policy.min_sources = 1;
        policy.source_filter.min_healthy_sources = 1;
        policy.toggles.oracle_confirmation = false;

        let quotes = vec![quote("binance", "cex", p, now)];
        let outcome = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        // Confidence can never reach 100.1% → breach_active = false
        assert!(!outcome.snapshot.breach_active);
        assert_no_alert(&outcome);
    }

    // ─── Deescalation + resolution interaction ──────────────────────────────

    /// TC-D-1415: Full escalation → deescalation → resolution lifecycle
    #[test]
    fn tc_d_1415_full_escalation_deescalation_resolution_lifecycle() {
        let now = Utc::now();
        let mut policy = spec_policy();
        policy.cooldown_sec = 0;
        policy.deescalation_blocks = 2;
        policy.resolution_blocks = 2;
        policy.toggles.oracle_confirmation = false;

        // Step 1: Trigger at MEDIUM
        let medium_price = price_from_deviation(1.0, 0.60);
        let q1 = make_quotes(medium_price, None, None, 12, now);
        let s1 = eval_fresh(&policy, &q1, now, ContextClassification::Isolated);
        assert_transition(&s1, IncidentTransition::Trigger);
        assert_alert_with_severity(&s1, Severity::Medium);

        // Step 2: Escalate to CRITICAL
        let critical_price = price_from_deviation(1.0, 5.50);
        let q2 = make_quotes(critical_price, None, None, 12, now + Duration::seconds(1));
        let s2 = evaluate_policy(
            &policy,
            &q2,
            &s1.next_state,
            now + Duration::seconds(1),
            ContextClassification::Isolated,
        )
        .expect("evaluation");
        assert_transition(&s2, IncidentTransition::Escalate);
        assert_alert_with_severity(&s2, Severity::Critical);

        // Steps 3-4: Deescalate (2 blocks at MEDIUM)
        let mut state = s2.next_state;
        for i in 2..4 {
            let ts = now + Duration::seconds(i);
            let q = make_quotes(medium_price, None, None, 12, ts);
            let out = evaluate_policy(&policy, &q, &state, ts, ContextClassification::Isolated)
                .expect("evaluation");
            if i == 3 {
                assert_transition(&out, IncidentTransition::Deescalate);
                assert_alert_with_severity(&out, Severity::Medium);
            }
            state = out.next_state;
        }

        // Steps 5-6: Resolve (2 blocks below trigger floor)
        let recovery_price = price_from_deviation(1.0, 0.10);
        for i in 4..6 {
            let ts = now + Duration::seconds(i);
            let q = make_quotes(recovery_price, None, None, 12, ts);
            let out = evaluate_policy(&policy, &q, &state, ts, ContextClassification::Isolated)
                .expect("evaluation");
            if i == 5 {
                assert_transition(&out, IncidentTransition::Resolve);
            }
            state = out.next_state;
        }
    }

    // ─── Policy validation ──────────────────────────────────────────────────

    /// TC-D-1416: Policy validation rejects invalid configs
    #[test]
    fn tc_d_1416_policy_validation() {
        let mut p = base_policy();
        p.sustained_window_ms = 1; // base_policy uses 0, but validate requires > 0
        assert!(p.validate().is_ok());

        p.peg_target = 0.0;
        assert!(p.validate().is_err());
        p.peg_target = 1.0;

        p.min_sources = 0;
        assert!(p.validate().is_err());
        p.min_sources = 3;

        p.quorum_pct = 1.5;
        assert!(p.validate().is_err());
        p.quorum_pct = 0.5;

        p.sustained_window_ms = -1;
        assert!(p.validate().is_err());
        p.sustained_window_ms = 0; // Also invalid (must be > 0)
        assert!(p.validate().is_err());
        p.sustained_window_ms = 1;

        p.resolution_blocks = 0;
        assert!(p.validate().is_err());
        p.resolution_blocks = 30;

        p.min_confidence_to_fire = 101.0;
        assert!(p.validate().is_err());
    }

    // ─── Stale quote handling ───────────────────────────────────────────────

    /// TC-D-1417: Stale quotes are excluded from consensus
    #[test]
    fn tc_d_1417_stale_quotes_excluded() {
        let now = Utc::now();
        let mut policy = spec_policy();
        policy.stale_timeout_ms = 30_000; // 30s
        policy.min_sources = 1;
        policy.source_filter.min_healthy_sources = 1;
        policy.toggles.oracle_confirmation = false;

        // Quote from 60s ago — stale
        let stale_time = now - Duration::seconds(60);
        let breach_price = price_from_deviation(1.0, 5.0);
        let quotes = vec![quote("binance", "cex", breach_price, stale_time)];

        let outcome = eval_fresh(&policy, &quotes, now, ContextClassification::Isolated);
        // Stale quote excluded → source_count = 0
        assert_eq!(outcome.snapshot.source_count, 0);
        assert!(!outcome.snapshot.breach_active);
    }

    /// TC-D-1418: Per-source stale timeout override
    #[test]
    fn tc_d_1418_per_source_stale_override() {
        let now = Utc::now();
        let mut policy = spec_policy();
        policy.stale_timeout_ms = 30_000; // 30s default
        policy.min_sources = 1;
        policy.source_filter.min_healthy_sources = 1;
        policy.toggles.oracle_confirmation = false;
        policy.source_overrides.insert(
            "slow-source".to_string(),
            DpegSourceOverride {
                source_id: "slow-source".to_string(),
                weight: 1.0,
                enabled: true,
                stale_timeout_ms: Some(120_000), // 120s override
            },
        );

        let old_time = now - Duration::seconds(60);
        let breach_price = price_from_deviation(1.0, 1.50);

        // Normal source at 60s → stale (> 30s default)
        let quotes_normal = vec![quote("normal-source", "cex", breach_price, old_time)];
        let out1 = eval_fresh(
            &policy,
            &quotes_normal,
            now,
            ContextClassification::Isolated,
        );
        assert_eq!(out1.snapshot.source_count, 0);

        // Overridden source at 60s → NOT stale (< 120s override)
        let quotes_slow = vec![quote("slow-source", "cex", breach_price, old_time)];
        let out2 = eval_fresh(&policy, &quotes_slow, now, ContextClassification::Isolated);
        assert_eq!(out2.snapshot.source_count, 1);
        assert!(out2.snapshot.breach_active);
    }
}
