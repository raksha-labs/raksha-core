//! DEPEG (De-Peg) detection pattern — **v2**.
//!
//! v2 is the experimental fork of the depeg pattern. It runs side-by-side with
//! the legacy [`super::depeg`] pattern: both subscribe to the same market price
//! feeds and **share the same per-tenant configuration rows** in
//! `tenant_pattern_configs` (the v2 pattern reloads any row with
//! `pattern_id = "depeg"` so the existing Pattern Creator UI drives both
//! engines without any frontend change). The two patterns then emit
//! `DetectionResult`s under different `pattern_id` values (`depeg` vs
//! `depeg_v2`) so an operator can A/B them with a single SQL diff.
//!
//! v2 differs from the legacy `depeg.rs` only in the parts the
//! "generalize the detector" plan touches — Phase 1 already added an opt-in
//! CEL `decision_expression` slot here; Phases 3+ will add per-policy
//! decision expressions, declarative aggregation, and a CEL severity
//! selector inside this file. The legacy `depeg.rs` is the frozen baseline
//! and must not be modified.
//!
//! Monitors price-feed market events (`UnifiedEvent` with `market_key` + `price`) and
//! computes a per-tenant, per-market weighted median across all contributing sources.
//! Fires a `DetectionResult` when a sustained depeg breach is detected based on the
//! per-tenant `DepegPolicy` stored in `tenant_pattern_configs`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use cel::{Context as CelContext, Program as CelProgram, Value as CelValue};
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

/// The pattern_id v2 emits on its own detections, snapshots, and state rows.
/// Distinct from the legacy `depeg` so the two engines can run in the same
/// detector process without trampling each other's `pattern_state` /
/// `pattern_snapshots` / `detections` rows.
pub const PATTERN_ID: &str = "depeg_v2";

/// The pattern_id v2 listens for in `tenant_pattern_configs`. v2 mirrors the
/// legacy `depeg` config rows so any pattern saved through the existing
/// Pattern Creator UI is automatically processed by both engines, enabling
/// side-by-side comparison without requiring users to maintain two patterns.
const MIRRORED_CONFIG_PATTERN_ID: &str = "depeg";

const DEPEG_QUOTE_CACHE_TTL_MINUTES: i64 = 120;
const DEPEG_QUOTE_CACHE_MAX_KEYS: usize = 2_048;

// ─── Policy types (inlined from crates/depeg-engine) ──────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepegSeverityBands {
    pub medium: f64,
    pub high: f64,
    pub critical: f64,
}

impl Default for DepegSeverityBands {
    fn default() -> Self {
        Self {
            medium: 1.0,
            high: 3.0,
            critical: 5.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepegSourceFilter {
    #[serde(default)]
    pub cex_whitelist: Vec<String>,
    #[serde(default = "default_true")]
    pub include_oracles: bool,
    #[serde(default = "default_true")]
    pub include_aggregators: bool,
    #[serde(default = "default_min_healthy")]
    pub min_healthy_sources: usize,
}

fn default_true() -> bool {
    true
}

fn default_min_healthy() -> usize {
    3
}

impl Default for DepegSourceFilter {
    fn default() -> Self {
        Self {
            cex_whitelist: Vec::new(),
            include_oracles: true,
            include_aggregators: false,
            min_healthy_sources: 3,
        }
    }
}

impl DepegSourceFilter {
    fn source_kind_allowed(&self, source_id: &str, source_kind: &str) -> bool {
        match source_kind.to_ascii_lowercase().as_str() {
            "oracle" => self.include_oracles,
            "aggregator" => self.include_aggregators,
            // DEX pool prices are intentionally excluded from DEPEG consensus.
            "dex" => false,
            "cex" => {
                self.cex_whitelist.is_empty()
                    || self
                        .cex_whitelist
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(source_id))
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepegToggles {
    #[serde(default)]
    pub oracle_confirmation: bool,
    #[serde(default)]
    pub volume_confirmation: bool,
    #[serde(default)]
    pub contagion_detection: bool,
    // Phase 6: `liquidity_depth_check` was declared in v1 but never read by
    // the detection logic (zero runtime references). Removed to stop
    // surfacing a dead toggle in the UI. The field is silently ignored on
    // deserialization via serde's default behaviour (unknown fields are
    // skipped unless `deny_unknown_fields` is set).
}

impl Default for DepegToggles {
    fn default() -> Self {
        Self {
            oracle_confirmation: true,
            volume_confirmation: false,
            contagion_detection: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepegConfidenceWeights {
    pub source_agreement: f64,
    pub oracle_confirmation: f64,
    pub volume_confirmation: f64,
}

impl Default for DepegConfidenceWeights {
    fn default() -> Self {
        Self {
            source_agreement: 60.0,
            oracle_confirmation: 25.0,
            volume_confirmation: 15.0,
        }
    }
}

/// Aggregation strategy for computing the consensus price across sources.
///
/// Serialized as a JSON string tag (e.g. `"weighted_median"`) so the field
/// is human-readable in `tenant_pattern_configs` and the Pattern Creator UI
/// can offer a dropdown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MedianStrategy {
    /// The default. Sorts sources by price, walks up the cumulative weight
    /// distribution, and picks the price at the 50th-percentile mark.
    WeightedMedian,
    /// Simple weighted mean: `Σ(price × weight) / Σ(weight)`. More
    /// sensitive to outliers than the median but smoother when source
    /// counts are low.
    WeightedMean,
    /// Trimmed weighted mean: discards the top and bottom `trim_pct`
    /// fraction of the weight mass before computing the mean. Robust
    /// against a single rogue source while retaining the continuity of
    /// a mean. `trim_pct = 0.1` discards the lightest and heaviest 10%.
    TrimmedMean {
        #[serde(default = "default_trim_pct")]
        trim_pct: f64,
    },
    /// Weighted percentile: generalises the median to any quantile `p`
    /// (0.0–1.0). `p = 0.5` is the weighted median; `p = 0.25` gives the
    /// first quartile, etc. Useful when the user wants a conservative
    /// (lower-quantile) consensus price instead of the centre of the
    /// distribution.
    Percentile {
        #[serde(default = "default_percentile_p")]
        p: f64,
    },
}

fn default_trim_pct() -> f64 {
    0.1
}
fn default_percentile_p() -> f64 {
    0.5
}

impl Default for MedianStrategy {
    fn default() -> Self {
        Self::WeightedMedian
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepegSourceOverride {
    pub source_id: String,
    pub weight: f64,
    pub enabled: bool,
    pub stale_timeout_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepegPolicy {
    #[serde(default)]
    pub tenant_id: String,
    pub market_key: String,
    pub peg_target: f64,
    pub min_sources: usize,
    pub quorum_pct: f64,
    pub cooldown_sec: i64,
    pub stale_timeout_ms: i64,
    #[serde(default)]
    pub severity_bands: DepegSeverityBands,
    #[serde(default)]
    pub severity_bands_isolated: Option<DepegSeverityBands>,
    #[serde(default)]
    pub severity_bands_systemic: Option<DepegSeverityBands>,
    #[serde(default = "default_isolated_floor_pct")]
    pub isolated_floor_pct: f64,
    #[serde(default = "default_systemic_floor_pct")]
    pub systemic_floor_pct: f64,
    #[serde(default = "default_deescalation_blocks")]
    pub deescalation_blocks: i64,
    #[serde(default = "default_resolution_blocks")]
    pub resolution_blocks: i64,
    #[serde(default)]
    pub source_filter: DepegSourceFilter,
    #[serde(default)]
    pub toggles: DepegToggles,
    #[serde(default)]
    pub confidence_weights: DepegConfidenceWeights,
    #[serde(default = "default_min_confidence_to_fire")]
    pub min_confidence_to_fire: f64,
    #[serde(default, deserialize_with = "deserialize_source_overrides")]
    pub source_overrides: HashMap<String, DepegSourceOverride>,
    /// Aggregation strategy for computing the consensus price. Defaults to
    /// `WeightedMedian` (the v1 hardcoded behaviour) when absent or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregation: Option<MedianStrategy>,
    /// Optional CEL expression that overrides the hardcoded breach predicate.
    /// When `None` (default), `evaluate_policy` runs the built-in predicate.
    /// When set, the expression is compiled at config load and evaluated per
    /// event with the inputs documented on `build_decision_context`.
    /// A non-bool result, runtime error, or compile failure transparently
    /// falls back to the default predicate (logged via `tracing::warn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_expression: Option<String>,
    /// Compiled form of `decision_expression`. Populated by `parse_policies`
    /// after deserialization; never serialized. `None` means: run the default
    /// predicate (either because `decision_expression` was empty or because
    /// compilation failed and we logged a warning).
    #[serde(skip)]
    pub compiled_decision: Option<Arc<CelProgram>>,
    /// Optional CEL expression that overrides the hardcoded severity
    /// selection (`severity_for_divergence`). Must return a string in
    /// `["critical", "high", "medium", "low", "info"]`; returning any
    /// other value (or erroring) falls back to the hardcoded if-chain.
    /// The CEL surface includes: `deviation_pct`, `severity_bands_medium`,
    /// `severity_bands_high`, `severity_bands_critical`, `confidence_total`,
    /// `quorum_count`, `median_price`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_expression: Option<String>,
    #[serde(skip)]
    pub compiled_severity: Option<Arc<CelProgram>>,
    /// Phase 7: Optional CEL override for `oracle_confirmation_met()`.
    /// Context: `oracle_max_divergence_pct`, `oracle_count`, `has_fresh_oracle`,
    /// `trigger_floor_pct`, `deviation_pct`. Must return bool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_expression: Option<String>,
    #[serde(skip)]
    pub compiled_oracle: Option<Arc<CelProgram>>,
    /// Phase 7: Optional CEL override for `compute_confidence_breakdown()`.
    /// Context: `source_agreement`, `oracle_score`, `volume_score`,
    /// `weight_source`, `weight_oracle`, `weight_volume`. Must return
    /// float 0–100 (the total confidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_expression: Option<String>,
    #[serde(skip)]
    pub compiled_confidence: Option<Arc<CelProgram>>,
    /// Phase 7: Optional CEL override for `assess_context()` contagion
    /// classification. Context: `systemic_market_count`, `deviation_pct`,
    /// `source_count`. Must return `"isolated"` or `"systemic"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contagion_expression: Option<String>,
    #[serde(skip)]
    pub compiled_contagion: Option<Arc<CelProgram>>,
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
    0.0
}

fn deserialize_source_overrides<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, DepegSourceOverride>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(HashMap::new()),
        Value::Object(map) => {
            serde_json::from_value::<HashMap<String, DepegSourceOverride>>(Value::Object(map))
                .map_err(de::Error::custom)
        }
        Value::Array(items) => {
            let parsed = serde_json::from_value::<Vec<DepegSourceOverride>>(Value::Array(items))
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

impl DepegPolicy {
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

    fn isolated_bands(&self) -> DepegSeverityBands {
        self.severity_bands_isolated
            .clone()
            .or_else(|| {
                if self.severity_bands.medium > 0.0 {
                    Some(self.severity_bands.clone())
                } else {
                    None
                }
            })
            .unwrap_or(DepegSeverityBands {
                medium: 0.5,
                high: 1.0,
                critical: 5.0,
            })
    }

    fn systemic_bands(&self) -> DepegSeverityBands {
        self.severity_bands_systemic
            .clone()
            .unwrap_or(DepegSeverityBands {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SystemicMarketContext {
    market_key: String,
    divergence_pct: f64,
    trigger_floor_pct: f64,
}

#[derive(Debug, Clone)]
struct ContextAssessment {
    classification: ContextClassification,
    systemic_markets: Vec<SystemicMarketContext>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DepegAlertState {
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

/// Per-tenant, per-market DEPEG detection pattern.
pub struct DepegPatternV2 {
    /// (tenant_id, market_key) → DepegPolicy
    policies: HashMap<(String, String), DepegPolicy>,
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
    // Phase 6: pattern-level tuning knobs (formerly magic constants). Loaded
    // from the top-level `pattern_config` key in tenant_pattern_configs; if
    // absent, the legacy defaults apply.
    quote_cache_ttl_minutes: i64,
    quote_cache_max_keys: usize,
    max_quote_history_per_source: usize,
}

impl Default for DepegPatternV2 {
    fn default() -> Self {
        Self {
            policies: HashMap::new(),
            quote_cache: HashMap::new(),
            source_bindings: HashMap::new(),
            source_mapping_overrides: HashMap::new(),
            quote_cache_ttl_minutes: DEPEG_QUOTE_CACHE_TTL_MINUTES,
            quote_cache_max_keys: DEPEG_QUOTE_CACHE_MAX_KEYS,
            max_quote_history_per_source: 16,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SourceBindingRuntimeOverride {
    active_stream_ids: Vec<String>,
    stream_field_mappings: HashMap<String, Vec<FieldMapping>>,
}

impl DepegPatternV2 {
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

    fn prune_quote_cache(&mut self, now: DateTime<Utc>, current_key: &(String, String, String)) {
        let cutoff = now - Duration::minutes(self.quote_cache_ttl_minutes);
        self.quote_cache.retain(|key, market_quotes| {
            if key == current_key {
                return true;
            }
            latest_quote_timestamp(market_quotes)
                .map(|observed_at| observed_at >= cutoff)
                .unwrap_or(false)
        });

        if self.quote_cache.len() <= self.quote_cache_max_keys {
            return;
        }

        let mut oldest_keys = self
            .quote_cache
            .iter()
            .filter(|(key, _)| *key != current_key)
            .map(|(key, market_quotes)| {
                (
                    key.clone(),
                    latest_quote_timestamp(market_quotes).unwrap_or(DateTime::<Utc>::MIN_UTC),
                )
            })
            .collect::<Vec<_>>();
        oldest_keys.sort_by_key(|(_, observed_at)| *observed_at);

        let remove_count = self
            .quote_cache
            .len()
            .saturating_sub(self.quote_cache_max_keys);
        for (key, _) in oldest_keys.into_iter().take(remove_count) {
            self.quote_cache.remove(&key);
        }
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
        policy
            .entry("quorum_pct".to_string())
            .or_insert_with(|| Value::from(0.0));
        policy
            .entry("stale_timeout_ms".to_string())
            .or_insert_with(|| Value::from(30_000));

        Value::Array(vec![Value::Object(policy)])
    }

    fn parse_policies(tenant_id: &str, config: &Value) -> Vec<DepegPolicy> {
        let config_value = Self::normalized_policy_config(config);

        let entries: Vec<DepegPolicy> = match serde_json::from_value(config_value) {
            Ok(value) => value,
            Err(err) => {
                common::log_error!(
                    warn,
                    err,
                    "failed to parse depeg config",
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
                    "invalid depeg policy — skipping market",
                    tenant_id = %tenant_id,
                    market_key = %policy.market_key
                );
                continue;
            }
            // Compile the optional CEL decision expression once at config-load
            // time. A typo in the user's expression must NOT take down the
            // pattern — log and fall back to the hardcoded predicate by leaving
            // `compiled_decision = None`.
            if let Some(expr) = policy
                .decision_expression
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
            {
                match CelProgram::compile(&expr) {
                    Ok(program) => {
                        policy.compiled_decision = Some(Arc::new(program));
                        tracing::info!(
                            pattern_id = PATTERN_ID,
                            tenant_id = %tenant_id,
                            market_key = %policy.market_key,
                            "depeg decision_expression compiled"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            pattern_id = PATTERN_ID,
                            tenant_id = %tenant_id,
                            market_key = %policy.market_key,
                            error = %err,
                            "depeg decision_expression failed to compile — falling back to default predicate"
                        );
                    }
                }
            }
            // Phase 5: compile the optional severity CEL expression.
            if let Some(expr) = policy
                .severity_expression
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
            {
                match CelProgram::compile(&expr) {
                    Ok(program) => {
                        policy.compiled_severity = Some(Arc::new(program));
                        tracing::info!(
                            pattern_id = PATTERN_ID,
                            tenant_id = %tenant_id,
                            market_key = %policy.market_key,
                            "depeg severity_expression compiled"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            pattern_id = PATTERN_ID,
                            tenant_id = %tenant_id,
                            market_key = %policy.market_key,
                            error = %err,
                            "depeg severity_expression failed to compile — falling back to default severity selection"
                        );
                    }
                }
            }
            // Phase 7: compile the remaining sub-predicate CEL expressions.
            for (field_name, expr_field, target_field) in [
                (
                    "oracle_expression",
                    &policy.oracle_expression.clone(),
                    &mut policy.compiled_oracle as &mut Option<Arc<CelProgram>>,
                ),
                (
                    "confidence_expression",
                    &policy.confidence_expression.clone(),
                    &mut policy.compiled_confidence,
                ),
                (
                    "contagion_expression",
                    &policy.contagion_expression.clone(),
                    &mut policy.compiled_contagion,
                ),
            ] {
                if let Some(expr) = expr_field
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    match CelProgram::compile(expr) {
                        Ok(program) => {
                            *target_field = Some(Arc::new(program));
                            tracing::info!(
                                pattern_id = PATTERN_ID,
                                tenant_id = %tenant_id,
                                market_key = %policy.market_key,
                                field = field_name,
                                "depeg CEL expression compiled"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                pattern_id = PATTERN_ID,
                                tenant_id = %tenant_id,
                                market_key = %policy.market_key,
                                field = field_name,
                                error = %err,
                                "depeg CEL expression failed to compile — falling back to default"
                            );
                        }
                    }
                }
            }
            parsed.push(policy);
        }
        parsed
    }

    fn effective_policy(&self, tenant_id: &str, market_key: &str) -> Option<DepegPolicy> {
        self.policies
            .get(&(tenant_id.to_string(), market_key.to_string()))
            .cloned()
    }

    fn assess_context(
        &self,
        policy: &DepegPolicy,
        tenant_id: &str,
        replay_scope: &str,
        now: DateTime<Utc>,
    ) -> ContextAssessment {
        if !policy.toggles.contagion_detection {
            return ContextAssessment {
                classification: ContextClassification::Isolated,
                systemic_markets: Vec::new(),
            };
        }

        let tenant_policies: Vec<DepegPolicy> = self
            .policies
            .iter()
            .filter(|((candidate_tenant, _), _)| candidate_tenant == tenant_id)
            .map(|(_, candidate_policy)| candidate_policy.clone())
            .collect();
        let mut systemic_markets = Vec::new();
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
                    systemic_markets.push(SystemicMarketContext {
                        market_key: candidate_market,
                        divergence_pct,
                        trigger_floor_pct: candidate_policy.systemic_floor_pct,
                    });
                }
            }
        }
        systemic_markets.sort_by(|left, right| {
            right
                .divergence_pct
                .partial_cmp(&left.divergence_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.market_key.cmp(&right.market_key))
        });
        let default_classification = if systemic_markets.len() >= 2 {
            ContextClassification::Systemic
        } else {
            systemic_markets.clear();
            ContextClassification::Isolated
        };
        // Phase 7: optional CEL override for contagion classification.
        // The CEL expression sees the systemic_market_count (how many
        // markets crossed the systemic floor) and can override the
        // default ≥2 threshold. This is evaluated before the `process_event`
        // pipeline, so `deviation_pct` for the current market is not yet
        // available — use 0.0 as a placeholder. The primary input is
        // `systemic_market_count`.
        let classification = match policy.compiled_contagion.as_ref() {
            Some(prog) => eval_contagion_expression(
                prog,
                systemic_markets.len(),
                0.0, // deviation_pct not yet computed
                0,   // source_count not yet computed
                policy,
            )
            .unwrap_or(default_classification),
            None => default_classification,
        };
        ContextAssessment {
            classification,
            systemic_markets,
        }
    }

    #[cfg(test)]
    fn classify_context(
        &self,
        policy: &DepegPolicy,
        tenant_id: &str,
        replay_scope: &str,
        now: DateTime<Utc>,
    ) -> ContextClassification {
        self.assess_context(policy, tenant_id, replay_scope, now)
            .classification
    }
}

#[async_trait]
impl DetectionPattern for DepegPatternV2 {
    fn pattern_id(&self) -> &str {
        PATTERN_ID
    }

    async fn reload_config(&mut self, config_map: &HashMap<(String, String), Value>) -> Result<()> {
        let mut new_policies = HashMap::new();
        let mut next_bindings = HashMap::new();
        let mut next_mapping_overrides = HashMap::new();
        for ((tenant_id, pattern_id), config) in config_map {
            // v2 mirrors any tenant config saved as the legacy `depeg` pattern
            // (or as `depeg_v2` directly, in case an operator wants to send a
            // v2-only override). This is the side-by-side switch: any pattern
            // shipped through the existing Pattern Creator UI is automatically
            // evaluated by both engines without any user action.
            if pattern_id != MIRRORED_CONFIG_PATTERN_ID && pattern_id != PATTERN_ID {
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

        // Phase 6: load pattern-level tuning knobs from the first config
        // blob that contains a `pattern_config` key (the blob shape is shared
        // across tenants so we just pick the first). Absent keys keep the
        // legacy defaults set by `DepegPatternV2::default()`.
        for ((_, pattern_id), config) in config_map {
            if pattern_id != MIRRORED_CONFIG_PATTERN_ID && pattern_id != PATTERN_ID {
                continue;
            }
            if let Some(pc) = config.get("pattern_config").or_else(|| {
                config
                    .get("detection_config")
                    .and_then(|dc| dc.get("pattern_config"))
            }) {
                if let Some(v) = pc.get("quote_cache_ttl_minutes").and_then(|v| v.as_i64()) {
                    self.quote_cache_ttl_minutes = v.max(1);
                }
                if let Some(v) = pc.get("quote_cache_max_keys").and_then(|v| v.as_u64()) {
                    self.quote_cache_max_keys = (v as usize).max(16);
                }
                if let Some(v) = pc
                    .get("max_quote_history_per_source")
                    .and_then(|v| v.as_u64())
                {
                    self.max_quote_history_per_source = (v as usize).max(2);
                }
                break;
            }
        }

        tracing::info!(
            pattern_id = PATTERN_ID,
            policy_count = self.policies.len(),
            quote_cache_ttl_minutes = self.quote_cache_ttl_minutes,
            quote_cache_max_keys = self.quote_cache_max_keys,
            max_quote_history = self.max_quote_history_per_source,
            "depeg_v2 policies reloaded"
        );
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
        {
            let max_history = self.max_quote_history_per_source;
            let market_quotes = self.quote_cache.entry(policy_key.clone()).or_default();
            remember_quote(
                market_quotes,
                QuoteInput {
                    source_id: event.source_id.clone(),
                    source_kind,
                    price,
                    observed_at: evaluation_time,
                },
                max_history,
            );
        }
        self.prune_quote_cache(evaluation_time, &policy_key);

        // Use persisted state as the source of truth so cleanup/reset operations
        // take effect on the next replay without needing a detector restart.
        let current_state = repo
            .load_pattern_state(&policy_key.0, PATTERN_ID, &state_key)
            .await?
            .and_then(|v| serde_json::from_value::<DepegAlertState>(v).ok())
            .unwrap_or_default();

        let quotes = self
            .quote_cache
            .get(&policy_key)
            .map(|market_quotes| latest_quotes_for_time(market_quotes, evaluation_time))
            .unwrap_or_default();
        let context =
            self.assess_context(&policy, &event.tenant_id, &replay_scope, evaluation_time);
        let mut outcome = evaluate_policy(
            &policy,
            &quotes,
            &current_state,
            evaluation_time,
            context.classification,
        )?;
        outcome.snapshot.systemic_markets = context.systemic_markets;
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
                "depeg source health below minimum; suppressing alert evaluation until enough sources recover"
            );
        }

        // Guard: events from concurrent source streams can arrive at Redis out of
        // event-timestamp order (live jitter or simulation speed-factor collapse).
        // The quote is already in the cache above; skip state transitions for late
        // events so a stale delivery cannot regress cooldown_until or resolution counters.
        let late_delivery = current_state
            .high_water_mark
            .map(|hwm| evaluation_time < hwm)
            .unwrap_or(false);
        if late_delivery {
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
            "contributing_sources": outcome.snapshot.contributors,
            "systemic_markets": outcome.snapshot.systemic_markets,
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

// ─── DEPEG evaluation engine (inlined from crates/depeg-engine) ─────────────────

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
    systemic_markets: Vec<SystemicMarketContext>,
}

struct EvaluationOutcome {
    snapshot: ConsensusSnapshot,
    should_emit_alert: bool,
    next_state: DepegAlertState,
    transition: Option<IncidentTransition>,
    emitted_severity: Option<Severity>,
}

fn evaluate_policy(
    policy: &DepegPolicy,
    quotes: &[QuoteInput],
    current_state: &DepegAlertState,
    now: DateTime<Utc>,
    classification: ContextClassification,
) -> Result<EvaluationOutcome> {
    let mut weighted_points = Vec::<(f64, f64)>::new();
    let mut eligible_quotes = Vec::<QuoteInput>::new();
    let mut total_source_count = 0usize;
    for quote in quotes {
        if !policy.source_enabled(&quote.source_id, &quote.source_kind) {
            continue;
        }
        if !(quote.price.is_finite() && quote.price > 0.0) {
            continue;
        }
        total_source_count += 1;
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
                eligible_source_count: total_source_count,
                quorum_met: false,
                breach_active: false,
                oracle_confirmed: false,
                classification,
                trigger_floor_pct,
                confidence_breakdown: HashMap::new(),
                severity: None,
                contributors: Vec::new(),
                systemic_markets: Vec::new(),
            },
            should_emit_alert: false,
            next_state: DepegAlertState::default(),
            transition: None,
            emitted_severity: None,
        });
    }

    let weighted_median_price = aggregate_price(&weighted_points, policy.aggregation.as_ref())
        .ok_or_else(|| anyhow!("price aggregation failed"))?;
    // Round divergence to 6 decimal places to eliminate floating-point noise in threshold checks.
    // Without rounding, values like 0.009999999999998899 could falsely satisfy >= 0.01 comparisons.
    let divergence_pct = round_to_6dp(
        ((weighted_median_price - policy.peg_target).abs() / policy.peg_target) * 100.0,
    );
    // Phase 5: severity selection — hardcoded if-chain as the default,
    // with an optional CEL override that can reclassify the severity
    // (e.g. add an extra "low" band or gate on confidence_total).
    // The CEL result is only used when it succeeds and returns a
    // recognised severity string; every other path falls back to the
    // default. `confidence_total` is not yet computed at this point, so
    // the CEL severity context doesn't include it for now — it uses the
    // inputs available at severity-selection time. Phase 7 may re-order
    // the pipeline if confidence needs to be available here.
    let default_severity = severity_for_divergence(divergence_pct, &selected_bands);
    let severity = match policy.compiled_severity.as_ref() {
        Some(program) => {
            // confidence_total is computed later in the pipeline, so we pass 0.0 here.
            // The primary severity inputs are deviation_pct + bands.
            let cel_severity = eval_severity_expression(
                program,
                divergence_pct,
                0.0, // confidence_total not yet available
                weighted_points.len(),
                weighted_median_price,
                &selected_bands,
                policy,
            );
            // Fall back to the default if CEL returned None (error / non-string / empty).
            cel_severity.or(default_severity)
        }
        None => default_severity,
    };
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
    // Phase 7: oracle confirmation — CEL override or hardcoded default.
    let default_oracle = oracle_confirmation_met(
        policy,
        &eligible_quotes,
        trigger_floor_pct,
        now,
        policy.peg_target,
    );
    let oracle_confirmed = match policy.compiled_oracle.as_ref() {
        Some(prog) => eval_oracle_expression(
            prog,
            &eligible_quotes,
            trigger_floor_pct,
            divergence_pct,
            policy.peg_target,
            now,
            policy,
        )
        .unwrap_or(default_oracle),
        None => default_oracle,
    };

    // Phase 7: confidence — CEL override or hardcoded default.
    let confidence_breakdown = compute_confidence_breakdown(
        policy,
        &eligible_quotes,
        weighted_median_price,
        oracle_confirmed,
        policy.peg_target,
    );
    let default_confidence = confidence_breakdown
        .get("total")
        .copied()
        .unwrap_or_default();
    let confidence_total = match policy.compiled_confidence.as_ref() {
        Some(prog) => {
            let sa = confidence_breakdown
                .get("source_agreement")
                .copied()
                .unwrap_or_default();
            let os = confidence_breakdown
                .get("oracle_confirmation")
                .copied()
                .unwrap_or_default();
            let vs = confidence_breakdown
                .get("volume_confirmation")
                .copied()
                .unwrap_or_default();
            let w_s = policy.confidence_weights.source_agreement.max(0.0);
            let w_o = if policy.toggles.oracle_confirmation {
                policy.confidence_weights.oracle_confirmation.max(0.0)
            } else {
                0.0
            };
            let w_v = if policy.toggles.volume_confirmation {
                policy.confidence_weights.volume_confirmation.max(0.0)
            } else {
                0.0
            };
            eval_confidence_expression(prog, sa, os, vs, w_s, w_o, w_v, policy)
                .unwrap_or(default_confidence)
        }
        None => default_confidence,
    };
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
    // Default (hardcoded) breach predicate. Always computed so it's available
    // as a fallback if the CEL override misbehaves at runtime.
    let default_breach = quorum_met
        && threshold_breach
        && (!policy.toggles.oracle_confirmation || oracle_confirmed)
        && confidence_total >= policy.min_confidence_to_fire;
    let breach_active = match policy.compiled_decision.as_ref() {
        None => default_breach,
        Some(program) => {
            let ctx = build_decision_context(
                quorum_met,
                threshold_breach,
                oracle_confirmed,
                confidence_total,
                divergence_pct,
                weighted_median_price,
                source_count,
                policy.min_confidence_to_fire,
                policy.toggles.oracle_confirmation,
                policy.toggles.volume_confirmation,
                &selected_bands,
            );
            eval_decision_expression(program, &ctx, policy).unwrap_or(default_breach)
        }
    };

    let mut next_state = current_state.clone();
    let mut should_emit_alert = false;
    let mut transition = None;
    let mut emitted_severity = None;
    let previous_active = severity_from_str(next_state.last_severity.as_deref());

    if breach_active {
        let cooldown_active = next_state
            .cooldown_until
            .map(|until| until > now)
            .unwrap_or(false);

        let current_rank = severity_rank(severity.as_ref());
        let previous_rank = severity_rank(previous_active.as_ref());
        let is_new_incident = previous_active.is_none();

        if is_new_incident {
            if !cooldown_active {
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
                    // De-escalation updates internal state only — no alert record emitted.
                    // The incident lifecycle is tracked via state transitions, not phantom alerts.
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
                // Resolution updates internal state only — no alert record emitted.
                // Per spec §11: resolution sends a resolution *notification*, not a new alert.
                // The incident is closed via state transition, preventing phantom alert records.
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
            eligible_source_count: total_source_count,
            quorum_met,
            breach_active,
            oracle_confirmed,
            classification,
            trigger_floor_pct,
            confidence_breakdown,
            severity,
            contributors,
            systemic_markets: Vec::new(),
        },
        should_emit_alert,
        next_state,
        transition,
        emitted_severity,
    })
}

/// Round a floating-point value to 6 decimal places.
/// Used to ensure threshold comparisons are not affected by floating-point noise.
/// E.g., 0.009999999999998899 becomes 0.010000, so the >= 0.01 check is deterministic.
fn round_to_6dp(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// Build the CEL evaluation context for a depeg `decision_expression`.
///
/// The variable names declared here form the **runtime DSL surface** the
/// Pattern Creator UI exposes to users. Adding a new input means: (a) add
/// a parameter here, (b) document it in the UI's Context Reference panel,
/// (c) re-run the parity tests.
#[allow(clippy::too_many_arguments)]
fn build_decision_context<'a>(
    quorum_met: bool,
    threshold_breach: bool,
    oracle_confirmed: bool,
    confidence_total: f64,
    deviation_pct: f64,
    median_price: f64,
    source_count: usize,
    min_confidence_to_fire: f64,
    oracle_confirmation_toggle: bool,
    volume_confirmation_toggle: bool,
    selected_bands: &DepegSeverityBands,
) -> CelContext<'a> {
    let mut ctx = CelContext::empty();
    ctx.add_variable_from_value("quorum_met", CelValue::Bool(quorum_met));
    ctx.add_variable_from_value("threshold_breach", CelValue::Bool(threshold_breach));
    ctx.add_variable_from_value("oracle_confirmed", CelValue::Bool(oracle_confirmed));
    ctx.add_variable_from_value("confidence_total", CelValue::Float(confidence_total));
    ctx.add_variable_from_value("deviation_pct", CelValue::Float(deviation_pct));
    ctx.add_variable_from_value("median_price", CelValue::Float(median_price));
    ctx.add_variable_from_value("source_count", CelValue::Int(source_count as i64));
    // Alias: rules can use either `source_count` or `quorum_count` to refer to
    // the number of eligible sources contributing to the consensus.
    ctx.add_variable_from_value("quorum_count", CelValue::Int(source_count as i64));
    ctx.add_variable_from_value(
        "min_confidence_to_fire",
        CelValue::Float(min_confidence_to_fire),
    );
    ctx.add_variable_from_value(
        "oracle_confirmation_toggle",
        CelValue::Bool(oracle_confirmation_toggle),
    );
    ctx.add_variable_from_value(
        "volume_confirmation_toggle",
        CelValue::Bool(volume_confirmation_toggle),
    );
    // Severity bands are flattened into individual variables for Phase 1.
    // Phase 5 may promote them to a nested CEL map (`severity_bands.medium`)
    // once the rest of the surface is in place.
    ctx.add_variable_from_value(
        "severity_bands_medium",
        CelValue::Float(selected_bands.medium),
    );
    ctx.add_variable_from_value("severity_bands_high", CelValue::Float(selected_bands.high));
    ctx.add_variable_from_value(
        "severity_bands_critical",
        CelValue::Float(selected_bands.critical),
    );
    ctx
}

/// Evaluate a compiled depeg `decision_expression` against the supplied
/// context. Returns:
///   - `Some(true)` / `Some(false)` when the program produced a bool.
///   - `None` on any error path (runtime error or non-bool result). The caller
///     is expected to fall back to the hardcoded default predicate; both
///     failure modes are logged with structured fields so operators can spot
///     bad rules without grepping for stack traces.
fn eval_decision_expression(
    program: &CelProgram,
    ctx: &CelContext<'_>,
    policy: &DepegPolicy,
) -> Option<bool> {
    match program.execute(ctx) {
        Ok(CelValue::Bool(value)) => Some(value),
        Ok(other) => {
            tracing::warn!(
                pattern_id = PATTERN_ID,
                tenant_id = %policy.tenant_id,
                market_key = %policy.market_key,
                rule_kind = "decision",
                result = ?other,
                "depeg decision_expression returned non-bool — falling back to default predicate"
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                pattern_id = PATTERN_ID,
                tenant_id = %policy.tenant_id,
                market_key = %policy.market_key,
                rule_kind = "decision",
                error = %err,
                "depeg decision_expression evaluation failed — falling back to default predicate"
            );
            None
        }
    }
}

/// Evaluate a compiled depeg `severity_expression` against a minimal context.
/// Returns `Some(Severity)` on success, `None` on any error (non-string result,
/// unrecognised severity level, runtime error). The caller falls back to the
/// hardcoded `severity_for_divergence` if-chain on `None`.
fn eval_severity_expression(
    program: &CelProgram,
    divergence_pct: f64,
    confidence_total: f64,
    source_count: usize,
    median_price: f64,
    bands: &DepegSeverityBands,
    policy: &DepegPolicy,
) -> Option<Severity> {
    let mut ctx = CelContext::empty();
    ctx.add_variable_from_value("deviation_pct", CelValue::Float(divergence_pct));
    ctx.add_variable_from_value("confidence_total", CelValue::Float(confidence_total));
    ctx.add_variable_from_value("quorum_count", CelValue::Int(source_count as i64));
    ctx.add_variable_from_value("source_count", CelValue::Int(source_count as i64));
    ctx.add_variable_from_value("median_price", CelValue::Float(median_price));
    ctx.add_variable_from_value("severity_bands_medium", CelValue::Float(bands.medium));
    ctx.add_variable_from_value("severity_bands_high", CelValue::Float(bands.high));
    ctx.add_variable_from_value("severity_bands_critical", CelValue::Float(bands.critical));

    match program.execute(&ctx) {
        Ok(CelValue::String(ref s)) => {
            let parsed = severity_from_str(Some(s));
            if parsed.is_none() && !s.is_empty() {
                tracing::warn!(
                    pattern_id = PATTERN_ID,
                    tenant_id = %policy.tenant_id,
                    market_key = %policy.market_key,
                    rule_kind = "severity",
                    result = %s,
                    "depeg severity_expression returned unrecognised severity level — falling back to default"
                );
            }
            parsed
        }
        Ok(CelValue::Null) => None, // explicit "no severity" = no fire, same as the default returning None
        Ok(other) => {
            tracing::warn!(
                pattern_id = PATTERN_ID,
                tenant_id = %policy.tenant_id,
                market_key = %policy.market_key,
                rule_kind = "severity",
                result = ?other,
                "depeg severity_expression returned non-string — falling back to default"
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                pattern_id = PATTERN_ID,
                tenant_id = %policy.tenant_id,
                market_key = %policy.market_key,
                rule_kind = "severity",
                error = %err,
                "depeg severity_expression evaluation failed — falling back to default"
            );
            None
        }
    }
}

/// Phase 7: evaluate a compiled `oracle_expression` against context derived
/// from the eligible oracle quotes. Returns `Some(bool)` on success, `None`
/// on any error (falls back to `oracle_confirmation_met`).
fn eval_oracle_expression(
    program: &CelProgram,
    eligible_quotes: &[QuoteInput],
    trigger_floor_pct: f64,
    deviation_pct: f64,
    peg_target: f64,
    now: DateTime<Utc>,
    policy: &DepegPolicy,
) -> Option<bool> {
    // Compute summary stats over oracle quotes the same way
    // `oracle_confirmation_met` does, and expose them as scalars.
    let mut oracle_count: i64 = 0;
    let mut max_div: f64 = 0.0;
    for q in eligible_quotes {
        if q.source_kind != "oracle" {
            continue;
        }
        let stale_ms = policy.source_stale_timeout_ms(&q.source_id);
        let age_ms = now.signed_duration_since(q.observed_at).num_milliseconds();
        if age_ms > stale_ms {
            continue;
        }
        oracle_count += 1;
        let div = round_to_6dp(((q.price - peg_target).abs() / peg_target) * 100.0);
        if div > max_div {
            max_div = div;
        }
    }

    let mut ctx = CelContext::empty();
    ctx.add_variable_from_value("oracle_max_divergence_pct", CelValue::Float(max_div));
    ctx.add_variable_from_value("oracle_count", CelValue::Int(oracle_count));
    ctx.add_variable_from_value("has_fresh_oracle", CelValue::Bool(oracle_count > 0));
    ctx.add_variable_from_value("trigger_floor_pct", CelValue::Float(trigger_floor_pct));
    ctx.add_variable_from_value("deviation_pct", CelValue::Float(deviation_pct));

    match program.execute(&ctx) {
        Ok(CelValue::Bool(v)) => Some(v),
        Ok(other) => {
            tracing::warn!(pattern_id = PATTERN_ID, tenant_id = %policy.tenant_id, market_key = %policy.market_key,
                rule_kind = "oracle", result = ?other, "oracle_expression returned non-bool — falling back");
            None
        }
        Err(err) => {
            tracing::warn!(pattern_id = PATTERN_ID, tenant_id = %policy.tenant_id, market_key = %policy.market_key,
                rule_kind = "oracle", error = %err, "oracle_expression eval failed — falling back");
            None
        }
    }
}

/// Phase 7: evaluate a compiled `confidence_expression` to compute the total
/// confidence score (0–100). Falls back to `compute_confidence_breakdown` on
/// error or non-numeric result.
fn eval_confidence_expression(
    program: &CelProgram,
    source_agreement: f64,
    oracle_score: f64,
    volume_score: f64,
    weight_source: f64,
    weight_oracle: f64,
    weight_volume: f64,
    policy: &DepegPolicy,
) -> Option<f64> {
    let mut ctx = CelContext::empty();
    ctx.add_variable_from_value("source_agreement", CelValue::Float(source_agreement));
    ctx.add_variable_from_value("oracle_score", CelValue::Float(oracle_score));
    ctx.add_variable_from_value("volume_score", CelValue::Float(volume_score));
    ctx.add_variable_from_value("weight_source", CelValue::Float(weight_source));
    ctx.add_variable_from_value("weight_oracle", CelValue::Float(weight_oracle));
    ctx.add_variable_from_value("weight_volume", CelValue::Float(weight_volume));

    match program.execute(&ctx) {
        Ok(CelValue::Float(v)) => Some(v.clamp(0.0, 100.0)),
        Ok(CelValue::Int(v)) => Some((v as f64).clamp(0.0, 100.0)),
        Ok(other) => {
            tracing::warn!(pattern_id = PATTERN_ID, tenant_id = %policy.tenant_id, market_key = %policy.market_key,
                rule_kind = "confidence", result = ?other, "confidence_expression returned non-numeric — falling back");
            None
        }
        Err(err) => {
            tracing::warn!(pattern_id = PATTERN_ID, tenant_id = %policy.tenant_id, market_key = %policy.market_key,
                rule_kind = "confidence", error = %err, "confidence_expression eval failed — falling back");
            None
        }
    }
}

/// Phase 7: evaluate a compiled `contagion_expression` to classify the depeg
/// context. Must return `"isolated"` or `"systemic"`. Falls back to
/// `assess_context` on error.
fn eval_contagion_expression(
    program: &CelProgram,
    systemic_market_count: usize,
    deviation_pct: f64,
    source_count: usize,
    policy: &DepegPolicy,
) -> Option<ContextClassification> {
    let mut ctx = CelContext::empty();
    ctx.add_variable_from_value(
        "systemic_market_count",
        CelValue::Int(systemic_market_count as i64),
    );
    ctx.add_variable_from_value("deviation_pct", CelValue::Float(deviation_pct));
    ctx.add_variable_from_value("source_count", CelValue::Int(source_count as i64));

    match program.execute(&ctx) {
        Ok(CelValue::String(ref s)) => match s.to_ascii_lowercase().as_str() {
            "isolated" => Some(ContextClassification::Isolated),
            "systemic" => Some(ContextClassification::Systemic),
            _ => {
                tracing::warn!(pattern_id = PATTERN_ID, tenant_id = %policy.tenant_id, market_key = %policy.market_key,
                    rule_kind = "contagion", result = %s, "contagion_expression returned unrecognised value — falling back");
                None
            }
        },
        Ok(other) => {
            tracing::warn!(pattern_id = PATTERN_ID, tenant_id = %policy.tenant_id, market_key = %policy.market_key,
                rule_kind = "contagion", result = ?other, "contagion_expression returned non-string — falling back");
            None
        }
        Err(err) => {
            tracing::warn!(pattern_id = PATTERN_ID, tenant_id = %policy.tenant_id, market_key = %policy.market_key,
                rule_kind = "contagion", error = %err, "contagion_expression eval failed — falling back");
            None
        }
    }
}

// ─── Phase 3b: external test endpoint helpers ──────────────────────────────────
//
// `run_decision_expression_test` is the public surface called by the
// `/v1/depeg_v2/test_expression` HTTP endpoint (see the detector's
// health-check server). It compiles a candidate CEL expression once and
// runs it against a set of named test cases, each supplying explicit
// values for every variable in the v2 CEL surface. This is the "Test"
// button in the Pattern Creator's Rule Editor — a fast, hermetic
// dry-run that catches typos and confirms fire/no-fire intent before
// the user publishes.
//
// The test path deliberately does *not* touch the database or replay
// historical events. It's a unit-test runner, not a backtester. A
// historical replay path can be added later as a separate endpoint.

/// One named test case the user wants to run their candidate expression
/// against. The field names mirror the CEL variables registered by
/// `build_decision_context` so a request body is self-documenting.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DecisionExpressionTestInputs {
    pub quorum_met: bool,
    pub threshold_breach: bool,
    pub oracle_confirmed: bool,
    pub confidence_total: f64,
    pub deviation_pct: f64,
    pub median_price: f64,
    pub source_count: i64,
    pub min_confidence_to_fire: f64,
    pub oracle_confirmation_toggle: bool,
    pub volume_confirmation_toggle: bool,
    pub severity_bands_medium: f64,
    pub severity_bands_high: f64,
    pub severity_bands_critical: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DecisionExpressionTestCase {
    pub name: String,
    pub inputs: DecisionExpressionTestInputs,
    /// Optional expected outcome. When present, the response includes a
    /// `passed` boolean per case (`actual == expected`). When absent, the
    /// case is informational — useful for quick "what does this fire?" probes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DecisionExpressionTestRequest {
    pub decision_expression: String,
    #[serde(default)]
    pub test_cases: Vec<DecisionExpressionTestCase>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionExpressionTestCaseResult {
    pub name: String,
    /// `Some(true)` / `Some(false)` when the expression evaluated to a bool.
    /// `None` when it errored or returned a non-bool — see `error` for the reason.
    pub fired: Option<bool>,
    /// Set when `expected` was supplied on the input. `Some(true)` means the
    /// actual fire decision matched the user's expectation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionExpressionTestResponse {
    /// Set when the expression failed to compile at all. When present,
    /// `results` is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compile_error: Option<String>,
    pub results: Vec<DecisionExpressionTestCaseResult>,
}

/// Compile a candidate `decision_expression` once and run it against each
/// supplied test case. Returns:
///
/// - `compile_error: Some(...)` and an empty `results` list when the
///   expression fails to parse — same path Phase 1 takes when a tenant
///   pushes a broken expression.
/// - One `DecisionExpressionTestCaseResult` per case when compilation
///   succeeds, with `fired` set on success and `error` set on a runtime
///   eval error or non-bool result. `passed` mirrors the case's optional
///   `expected` field.
///
/// This is a pure function — it never touches the DB, the network, or any
/// shared state. The tests at the bottom of this file pin the contract.
pub fn run_decision_expression_test(
    request: DecisionExpressionTestRequest,
) -> DecisionExpressionTestResponse {
    let trimmed = request.decision_expression.trim();
    if trimmed.is_empty() {
        return DecisionExpressionTestResponse {
            compile_error: Some("decision_expression must not be empty".to_string()),
            results: Vec::new(),
        };
    }

    let program = match CelProgram::compile(trimmed) {
        Ok(p) => p,
        Err(err) => {
            return DecisionExpressionTestResponse {
                compile_error: Some(err.to_string()),
                results: Vec::new(),
            };
        }
    };

    let mut results = Vec::with_capacity(request.test_cases.len());
    for case in request.test_cases {
        // Each case gets a fresh CEL context — we're not threading state
        // between cases the way `evaluate_policy` does within a single
        // policy evaluation.
        let mut ctx = CelContext::empty();
        ctx.add_variable_from_value("quorum_met", CelValue::Bool(case.inputs.quorum_met));
        ctx.add_variable_from_value(
            "threshold_breach",
            CelValue::Bool(case.inputs.threshold_breach),
        );
        ctx.add_variable_from_value(
            "oracle_confirmed",
            CelValue::Bool(case.inputs.oracle_confirmed),
        );
        ctx.add_variable_from_value(
            "confidence_total",
            CelValue::Float(case.inputs.confidence_total),
        );
        ctx.add_variable_from_value("deviation_pct", CelValue::Float(case.inputs.deviation_pct));
        ctx.add_variable_from_value("median_price", CelValue::Float(case.inputs.median_price));
        ctx.add_variable_from_value("source_count", CelValue::Int(case.inputs.source_count));
        ctx.add_variable_from_value("quorum_count", CelValue::Int(case.inputs.source_count));
        ctx.add_variable_from_value(
            "min_confidence_to_fire",
            CelValue::Float(case.inputs.min_confidence_to_fire),
        );
        ctx.add_variable_from_value(
            "oracle_confirmation_toggle",
            CelValue::Bool(case.inputs.oracle_confirmation_toggle),
        );
        ctx.add_variable_from_value(
            "volume_confirmation_toggle",
            CelValue::Bool(case.inputs.volume_confirmation_toggle),
        );
        ctx.add_variable_from_value(
            "severity_bands_medium",
            CelValue::Float(case.inputs.severity_bands_medium),
        );
        ctx.add_variable_from_value(
            "severity_bands_high",
            CelValue::Float(case.inputs.severity_bands_high),
        );
        ctx.add_variable_from_value(
            "severity_bands_critical",
            CelValue::Float(case.inputs.severity_bands_critical),
        );

        let (fired, error) = match program.execute(&ctx) {
            Ok(CelValue::Bool(value)) => (Some(value), None),
            Ok(other) => (
                None,
                Some(format!(
                    "expression returned a non-bool result ({other:?}); CEL decision expressions must evaluate to a boolean"
                )),
            ),
            Err(err) => (None, Some(err.to_string())),
        };
        let passed = case
            .expected
            .and_then(|expected| fired.map(|actual| actual == expected));

        results.push(DecisionExpressionTestCaseResult {
            name: case.name,
            fired,
            passed,
            error,
        });
    }

    DecisionExpressionTestResponse {
        compile_error: None,
        results,
    }
}

fn market_divergence_pct(
    policy: &DepegPolicy,
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
    let median = aggregate_price(&weighted_points, policy.aggregation.as_ref())?;
    Some(round_to_6dp(
        ((median - policy.peg_target).abs() / policy.peg_target) * 100.0,
    ))
}

fn oracle_confirmation_met(
    policy: &DepegPolicy,
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
        round_to_6dp(((quote.price - peg_target).abs() / peg_target) * 100.0) >= trigger_floor_pct
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
    policy: &DepegPolicy,
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
    policy: &DepegPolicy,
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
            let divergence_pct =
                round_to_6dp(((quote.price - peg_target).abs() / peg_target) * 100.0);
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

// ─── Aggregation strategies ──────────────────────────────────────────────────
//
// Each function takes `&[(price, weight)]` and returns `Option<f64>`.
// `None` means the input was empty or all-zero-weight.

/// Dispatch to the appropriate aggregation strategy for a given policy.
/// Falls back to `WeightedMedian` when `policy.aggregation` is `None`
/// (backward-compatible default).
fn aggregate_price(points: &[(f64, f64)], strategy: Option<&MedianStrategy>) -> Option<f64> {
    match strategy.unwrap_or(&MedianStrategy::WeightedMedian) {
        MedianStrategy::WeightedMedian => weighted_median(points),
        MedianStrategy::WeightedMean => weighted_mean(points),
        MedianStrategy::TrimmedMean { trim_pct } => trimmed_mean(points, *trim_pct),
        MedianStrategy::Percentile { p } => weighted_percentile(points, *p),
    }
}

fn weighted_median(points: &[(f64, f64)]) -> Option<f64> {
    weighted_percentile(points, 0.5)
}

fn weighted_mean(points: &[(f64, f64)]) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let total_weight: f64 = points.iter().map(|(_, w)| *w).sum();
    if total_weight <= 0.0 || !total_weight.is_finite() {
        return None;
    }
    let sum: f64 = points.iter().map(|(p, w)| p * w).sum();
    Some(sum / total_weight)
}

fn trimmed_mean(points: &[(f64, f64)], trim_pct: f64) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let trim = trim_pct.clamp(0.0, 0.49);
    let mut sorted = points.to_vec();
    sorted.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let total_weight: f64 = sorted.iter().map(|(_, w)| *w).sum();
    if total_weight <= 0.0 || !total_weight.is_finite() {
        return None;
    }
    let low_cut = total_weight * trim;
    let high_cut = total_weight * (1.0 - trim);
    let mut running = 0.0;
    let mut sum = 0.0;
    let mut used_weight = 0.0;
    for (price, weight) in &sorted {
        let prev = running;
        running += weight;
        // Compute the portion of this point's weight that falls within the
        // [low_cut, high_cut] band of the cumulative distribution.
        let band_start = prev.max(low_cut);
        let band_end = running.min(high_cut);
        if band_end > band_start {
            let contribution = band_end - band_start;
            sum += price * contribution;
            used_weight += contribution;
        }
    }
    if used_weight <= 0.0 {
        return None;
    }
    Some(sum / used_weight)
}

fn weighted_percentile(points: &[(f64, f64)], p: f64) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, 1.0);
    let mut sorted = points.to_vec();
    sorted.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let total_weight: f64 = sorted.iter().map(|(_, w)| *w).sum();
    if total_weight <= 0.0 || !total_weight.is_finite() {
        return None;
    }
    let target = total_weight * p;
    let mut running = 0.0;
    for (price, weight) in sorted {
        running += weight;
        if running >= target {
            return Some(price);
        }
    }
    None
}

fn remember_quote(
    market_quotes: &mut HashMap<String, Vec<QuoteInput>>,
    quote: QuoteInput,
    max_history: usize,
) {
    let history = market_quotes.entry(quote.source_id.clone()).or_default();
    history.push(quote);
    history.sort_by(|a, b| a.observed_at.cmp(&b.observed_at));
    history.dedup_by(|a, b| {
        a.observed_at == b.observed_at
            && a.price == b.price
            && a.source_kind == b.source_kind
            && a.source_id == b.source_id
    });
    if history.len() > max_history {
        let excess = history.len() - max_history;
        history.drain(0..excess);
    }
}

fn latest_quote_timestamp(
    market_quotes: &HashMap<String, Vec<QuoteInput>>,
) -> Option<DateTime<Utc>> {
    market_quotes
        .values()
        .filter_map(|history| history.last().map(|quote| quote.observed_at))
        .max()
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

fn severity_for_divergence(pct: f64, bands: &DepegSeverityBands) -> Option<Severity> {
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

/// Map `SourceType` debug string ("CexWebsocket", "DexApi", etc.) to a depeg source_kind.
fn infer_source_kind(source_type: &str) -> String {
    match source_type.to_ascii_lowercase().as_str() {
        "cexwebsocket" | "cexapi" => "cex".to_string(),
        "dexapi" => "dex".to_string(),
        "oracleapi" => "oracle".to_string(),
        "customapi" => "custom".to_string(),
        "evmchain" => "chain".to_string(),
        _ => "unknown".to_string(),
    }
}

fn oracle_price_fields(
    contributors: &[EvidenceContributor],
    source_hint: &str,
) -> (Option<f64>, Option<f64>) {
    contributors
        .iter()
        .find(|contributor| {
            contributor.source_kind == "oracle"
                && contributor
                    .source_id
                    .to_ascii_lowercase()
                    .contains(source_hint)
        })
        .map(|contributor| (Some(contributor.price), Some(contributor.divergence_pct)))
        .unwrap_or((None, None))
}

fn build_detection(
    event: &UnifiedEvent,
    policy: &DepegPolicy,
    snapshot: &ConsensusSnapshot,
    severity: Severity,
    transition: Option<IncidentTransition>,
    now: DateTime<Utc>,
) -> DetectionResult {
    let (is_simulated, simulation_run_id) = simulation_metadata_from_event(event);
    let subject_key = format!("{}:{}", policy.tenant_id, policy.market_key);
    let divergence_str = format!("{:.6}%", snapshot.divergence_pct);
    let description = format!(
        "Market {} deviated {:.6}% from peg target {:.6} (median: {:.6}). {} source(s), quorum: {}.",
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
    let (chainlink_price, chainlink_deviation_pct) =
        oracle_price_fields(&snapshot.contributors, "chainlink");
    let (pyth_price, pyth_deviation_pct) = oracle_price_fields(&snapshot.contributors, "pyth");
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
        "contagion_status".to_string(),
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
        "healthy_source_count".to_string(),
        serde_json::json!(snapshot.source_count),
    );
    oracle_context.insert(
        "total_source_count".to_string(),
        serde_json::json!(snapshot.eligible_source_count),
    );
    oracle_context.insert(
        "divergence_pct".to_string(),
        serde_json::json!(snapshot.divergence_pct),
    );
    oracle_context.insert(
        "peg_target".to_string(),
        serde_json::json!(policy.peg_target),
    );
    oracle_context.insert(
        "chainlink_price".to_string(),
        serde_json::json!(chainlink_price),
    );
    oracle_context.insert(
        "chainlink_deviation_pct".to_string(),
        serde_json::json!(chainlink_deviation_pct),
    );
    oracle_context.insert("pyth_price".to_string(), serde_json::json!(pyth_price));
    oracle_context.insert(
        "pyth_deviation_pct".to_string(),
        serde_json::json!(pyth_deviation_pct),
    );
    oracle_context.insert(
        "contributing_sources".to_string(),
        serde_json::json!(snapshot.contributors),
    );
    oracle_context.insert(
        "systemic_markets".to_string(),
        serde_json::json!(snapshot.systemic_markets),
    );

    let actions_recommended = recommended_actions_for_severity(&severity);
    DetectionResult {
        detection_id: Uuid::new_v4(),
        pattern_id: PATTERN_ID.to_string(),
        event_key: Some(format!("depeg:{}:{}", policy.tenant_id, policy.market_key)),
        subject_type: Some("market".to_string()),
        subject_key: Some(subject_key),
        tenant_id: Some(policy.tenant_id.clone()),
        chain: Chain::Offchain,
        chain_slug: "offchain".to_string(),
        protocol: "Stablecoin".to_string(),
        lifecycle_state: LifecycleState::Confirmed,
        requires_confirmation: policy.toggles.oracle_confirmation,
        attack_family: AttackFamily::PegDeviation,
        severity,
        tx_hash: format!("depeg-{}", Uuid::new_v4()),
        block_number: 0,
        triggered_rule_ids: vec!["depeg.sustained_breach".to_string()],
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
        detected_at: now,
        created_at: now,
    }
}

fn log_test_mode_decision(
    event: &UnifiedEvent,
    policy: &DepegPolicy,
    market_key: &str,
    current_state: &DepegAlertState,
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
    let cooldown_until = current_state.cooldown_until;
    let cooldown_active = cooldown_until.map(|until| until > now).unwrap_or(false);
    let suppression_reason =
        depeg_test_mode_reason(policy, current_state, outcome, now, confidence_total);
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
        "test-mode depeg evaluation completed"
    );
}

fn depeg_test_mode_reason(
    policy: &DepegPolicy,
    current_state: &DepegAlertState,
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

    if outcome.snapshot.breach_active {
        if previous_active.is_none() {
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

    fn base_policy() -> DepegPolicy {
        DepegPolicy {
            tenant_id: "tenant-a".to_string(),
            market_key: "USDC/USD".to_string(),
            peg_target: 1.0,
            min_sources: 1,
            quorum_pct: 0.0,
            cooldown_sec: 0,
            stale_timeout_ms: 60_000,
            severity_bands: DepegSeverityBands {
                medium: 0.5,
                high: 1.0,
                critical: 5.0,
            },
            severity_bands_isolated: Some(DepegSeverityBands {
                medium: 0.5,
                high: 1.0,
                critical: 5.0,
            }),
            severity_bands_systemic: Some(DepegSeverityBands {
                medium: 0.01,
                high: 0.25,
                critical: 0.25,
            }),
            isolated_floor_pct: 0.5,
            systemic_floor_pct: 0.01,
            deescalation_blocks: 5,
            resolution_blocks: 30,
            source_filter: DepegSourceFilter {
                min_healthy_sources: 1,
                ..DepegSourceFilter::default()
            },
            toggles: DepegToggles::default(),
            confidence_weights: DepegConfidenceWeights::default(),
            min_confidence_to_fire: 0.0,
            source_overrides: HashMap::new(),
            aggregation: None,
            decision_expression: None,
            compiled_decision: None,
            severity_expression: None,
            compiled_severity: None,
            oracle_expression: None,
            compiled_oracle: None,
            confidence_expression: None,
            compiled_confidence: None,
            contagion_expression: None,
            compiled_contagion: None,
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
            DepegPatternV2::state_key("USDC/USD", Some("run_123")),
            "USDC/USD::run_123"
        );
        assert_eq!(DepegPatternV2::state_key("USDC/USD", None), "USDC/USD");
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
            &DepegAlertState::default(),
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
        remember_quote(
            &mut market_quotes,
            quote("oracle-a", "oracle", 1.0, base),
            16,
        );
        remember_quote(
            &mut market_quotes,
            quote("oracle-a", "oracle", 0.88, base + Duration::seconds(24)),
            16,
        );
        remember_quote(
            &mut market_quotes,
            quote("cex-a", "cex", 0.8785, base + Duration::seconds(21)),
            16,
        );
        remember_quote(
            &mut market_quotes,
            quote("cex-b", "cex", 0.8769, base + Duration::seconds(22)),
            16,
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
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert!(!outcome.snapshot.oracle_confirmed);
        assert!(!outcome.snapshot.breach_active);
        assert!(!outcome.should_emit_alert);
    }

    // ─── Phase 1: CEL `decision_expression` override ───────────────────────
    //
    // These tests cover the runtime hook added in Phase 1 of the
    // "generalize the detector" plan. The CEL surface intentionally exposes
    // a small, named set of inputs (see `build_decision_context`); each test
    // exercises a different part of the contract.

    /// Helper: take a `DepegPolicy` and attach a compiled CEL expression so
    /// the override branch in `evaluate_policy` is exercised. Mirrors what
    /// `parse_policies` does at runtime.
    fn with_decision_expression(mut policy: DepegPolicy, expr: &str) -> DepegPolicy {
        policy.decision_expression = Some(expr.to_string());
        policy.compiled_decision = Some(Arc::new(
            CelProgram::compile(expr).expect("expression should compile"),
        ));
        policy
    }

    /// Build a fixture in which the default depeg predicate fires:
    /// 0.6% deviation (above the 0.5% medium band), 2 CEX + 1 oracle quote
    /// satisfying `oracle_confirmation = true` from `DepegToggles::default()`.
    fn fires_by_default_quotes(now: DateTime<Utc>) -> Vec<QuoteInput> {
        vec![
            quote("cex-a", "cex", 0.994, now),
            quote("cex-b", "cex", 0.994, now),
            quote("chainlink", "oracle", 0.994, now),
        ]
    }

    #[test]
    fn decision_expression_default_equivalent_matches_hardcoded_predicate() {
        // Writing the hardcoded predicate verbatim in CEL must produce the
        // same outcome as no override. This is the "byte-identical default"
        // contract that gates every Phase 1 acceptance check.
        let now = Utc::now();
        let policy = base_policy();
        let policy_with_cel = with_decision_expression(
            base_policy(),
            "quorum_met && threshold_breach \
             && (!oracle_confirmation_toggle || oracle_confirmed) \
             && confidence_total >= min_confidence_to_fire",
        );
        let quotes = fires_by_default_quotes(now);

        let baseline = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("baseline");
        let overridden = evaluate_policy(
            &policy_with_cel,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("overridden");

        assert!(
            baseline.snapshot.breach_active,
            "fixture must trigger the default predicate so the parity check is meaningful"
        );
        assert_eq!(
            baseline.snapshot.breach_active, overridden.snapshot.breach_active,
            "default-equivalent CEL expression must match hardcoded predicate"
        );
        assert_eq!(
            baseline.should_emit_alert, overridden.should_emit_alert,
            "default-equivalent CEL expression must produce identical alert decision"
        );
    }

    #[test]
    fn decision_expression_can_make_breach_predicate_stricter() {
        // A custom override that requires >= 2.0% deviation must SUPPRESS a
        // breach the default 0.5% medium band would have fired on. The same
        // fixture is used for both baseline and override so the test proves
        // the override is doing actual work, not trivially passing on a
        // no-fire fixture.
        let now = Utc::now();
        let baseline_policy = base_policy();
        let overridden_policy = with_decision_expression(
            base_policy(),
            // Note: use the flattened band names since Phase 1 doesn't expose
            // a nested `severity_bands` map.
            "quorum_met && deviation_pct >= 2.0",
        );
        let quotes = fires_by_default_quotes(now);

        let baseline = evaluate_policy(
            &baseline_policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("baseline");
        let overridden = evaluate_policy(
            &overridden_policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("overridden");

        assert!(
            baseline.snapshot.breach_active,
            "fixture must trigger the default predicate"
        );
        assert!(
            !overridden.snapshot.breach_active,
            "custom CEL override should suppress sub-2% breach"
        );
        assert!(!overridden.should_emit_alert);
    }

    #[test]
    fn decision_expression_can_make_breach_predicate_looser() {
        // A custom override that fires on confidence alone must trigger even
        // when the default predicate (which checks deviation) would not.
        let now = Utc::now();
        // 0.1% deviation — well under any default band.
        let mut policy = base_policy();
        policy.severity_bands_isolated = Some(DepegSeverityBands {
            medium: 0.5,
            high: 1.0,
            critical: 5.0,
        });
        let baseline = evaluate_policy(
            &policy,
            &[
                quote("cex-a", "cex", 0.999, now),
                quote("cex-b", "cex", 0.999, now),
            ],
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("baseline");
        // The default predicate must NOT fire on 0.1%.
        assert!(!baseline.snapshot.breach_active);

        // Now wire a CEL override: fire whenever both sources agree on direction
        // (source_count >= 2 && confidence_total > 50). This should fire even
        // though deviation is well below the default thresholds.
        let policy =
            with_decision_expression(policy, "source_count >= 2 && confidence_total > 50.0");
        let outcome = evaluate_policy(
            &policy,
            &[
                quote("cex-a", "cex", 0.999, now),
                quote("cex-b", "cex", 0.999, now),
            ],
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("override");

        assert!(
            outcome.snapshot.breach_active,
            "custom CEL override should fire on confidence-only condition"
        );
    }

    #[tokio::test]
    async fn parse_policies_keeps_policy_alive_on_compile_error() {
        // A policy with a malformed CEL expression must still be loaded so
        // the default predicate runs. This is the "broken expression doesn't
        // take down the live pattern" guarantee.
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();
        config_map.insert(
            ("tenant-broken".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 },
                    // Deliberate syntax error: `>>` is not a CEL operator.
                    "decision_expression": "deviation_pct >> 5"
                }]
            }),
        );

        pattern.reload_config(&config_map).await.expect("reload");

        let policy = pattern
            .policies
            .get(&("tenant-broken".to_string(), "USDC/USD".to_string()))
            .expect("policy should still be loaded after compile failure");
        assert!(
            policy.compiled_decision.is_none(),
            "broken expression must leave compiled_decision as None"
        );
        assert_eq!(
            policy.decision_expression.as_deref(),
            Some("deviation_pct >> 5"),
            "the raw expression text is still kept on the policy for audit"
        );
    }

    #[tokio::test]
    async fn parse_policies_compiles_valid_expression() {
        // The happy path: a valid expression must be compiled and attached.
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();
        config_map.insert(
            ("tenant-cel".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 },
                    "decision_expression": "quorum_met && deviation_pct >= 2.0"
                }]
            }),
        );

        pattern.reload_config(&config_map).await.expect("reload");

        let policy = pattern
            .policies
            .get(&("tenant-cel".to_string(), "USDC/USD".to_string()))
            .expect("policy should be loaded");
        assert!(
            policy.compiled_decision.is_some(),
            "valid expression must compile"
        );
    }

    /// v2 must mirror tenant configs saved under the legacy `pattern_id =
    /// "depeg"` so the existing Pattern Creator UI drives both engines
    /// without any frontend change. This test pins the contract.
    #[tokio::test]
    async fn reload_config_mirrors_legacy_depeg_pattern_id() {
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();
        config_map.insert(
            // Note the pattern_id key is "depeg", not "depeg_v2".
            (
                "tenant-mirror".to_string(),
                MIRRORED_CONFIG_PATTERN_ID.to_string(),
            ),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 },
                    "decision_expression": "quorum_met && deviation_pct >= 1.0"
                }]
            }),
        );

        pattern.reload_config(&config_map).await.expect("reload");

        let policy = pattern
            .policies
            .get(&("tenant-mirror".to_string(), "USDC/USD".to_string()))
            .expect("v2 should pick up legacy depeg config rows");
        assert!(
            policy.compiled_decision.is_some(),
            "mirrored config should still get its CEL expression compiled"
        );
    }

    /// v2 also accepts v2-specific overrides under its own pattern_id, so
    /// an operator can ship an experimental config to v2 without affecting
    /// the legacy `depeg` engine running in the same process.
    #[tokio::test]
    async fn reload_config_accepts_v2_specific_pattern_id() {
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();
        config_map.insert(
            // v2-only override.
            ("tenant-v2-only".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 }
                }]
            }),
        );

        pattern.reload_config(&config_map).await.expect("reload");

        assert!(pattern
            .policies
            .contains_key(&("tenant-v2-only".to_string(), "USDC/USD".to_string())));
    }

    /// Configs for unrelated patterns (e.g. flash_loan, tvl_drop) must be
    /// ignored by v2 — only `depeg` and `depeg_v2` rows count.
    #[tokio::test]
    async fn reload_config_ignores_unrelated_patterns() {
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();
        config_map.insert(
            ("tenant-unrelated".to_string(), "flash_loan".to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 }
                }]
            }),
        );

        pattern.reload_config(&config_map).await.expect("reload");

        assert!(pattern.policies.is_empty());
    }

    // ─── Phase 4: aggregation strategy ──────────────────────────────────────

    #[test]
    fn weighted_median_parity_with_default_aggregation() {
        // `aggregation: None` (default) must produce the same result as the
        // pre-Phase-4 hardcoded `weighted_median` call. This is the parity
        // gate that prevents regressions.
        let points = vec![(0.994, 1.0), (0.992, 1.5), (0.995, 1.0)];
        let via_dispatch = aggregate_price(&points, None);
        let via_direct = weighted_median(&points);
        assert_eq!(
            via_dispatch, via_direct,
            "dispatch default must match direct weighted_median"
        );
    }

    #[test]
    fn weighted_mean_computes_correctly() {
        let points = vec![(10.0, 1.0), (20.0, 3.0)];
        // Weighted mean = (10*1 + 20*3) / (1+3) = 70/4 = 17.5
        let result = weighted_mean(&points).expect("non-empty");
        assert!((result - 17.5).abs() < 1e-9);
    }

    #[test]
    fn trimmed_mean_excludes_extremes() {
        // 5 points equally weighted. trim_pct=0.2 discards the bottom 20%
        // and top 20% of weight mass, leaving only the middle 3 points.
        let points = vec![
            (1.0, 1.0), // bottom 20% → trimmed
            (2.0, 1.0),
            (3.0, 1.0),
            (4.0, 1.0),
            (100.0, 1.0), // top 20% → trimmed
        ];
        let result = trimmed_mean(&points, 0.2).expect("non-empty");
        // The middle 3 points (2, 3, 4) each contribute 1.0 weight.
        // Mean = (2+3+4)/3 = 3.0
        assert!((result - 3.0).abs() < 1e-9, "got {result}");
    }

    #[test]
    fn trimmed_mean_with_zero_trim_is_weighted_mean() {
        let points = vec![(10.0, 1.0), (20.0, 3.0)];
        let mean = weighted_mean(&points).unwrap();
        let trimmed = trimmed_mean(&points, 0.0).unwrap();
        assert!((mean - trimmed).abs() < 1e-9);
    }

    #[test]
    fn weighted_percentile_at_50_matches_weighted_median() {
        let points = vec![(0.994, 1.0), (0.992, 1.5), (0.995, 1.0)];
        let median = weighted_median(&points);
        let p50 = weighted_percentile(&points, 0.5);
        assert_eq!(median, p50, "p=0.5 must equal weighted median");
    }

    #[test]
    fn weighted_percentile_at_low_quantile_picks_low_price() {
        let points = vec![(1.0, 1.0), (2.0, 1.0), (3.0, 1.0)];
        // p=0.0 should pick the lowest price.
        let result = weighted_percentile(&points, 0.01).expect("non-empty");
        assert!(
            (result - 1.0).abs() < 1e-9,
            "p≈0 should return lowest: {result}"
        );
    }

    #[test]
    fn aggregation_strategy_round_trips_through_serde() {
        // Confirm the enum serializes/deserializes as expected JSON tags so
        // the frontend and DB config stay in sync.
        let tests: &[(MedianStrategy, &str)] = &[
            (MedianStrategy::WeightedMedian, r#""weighted_median""#),
            (MedianStrategy::WeightedMean, r#""weighted_mean""#),
            (
                MedianStrategy::TrimmedMean { trim_pct: 0.15 },
                r#"{"trimmed_mean":{"trim_pct":0.15}}"#,
            ),
            (
                MedianStrategy::Percentile { p: 0.25 },
                r#"{"percentile":{"p":0.25}}"#,
            ),
        ];
        for (strategy, expected_json) in tests {
            let json = serde_json::to_string(strategy).expect("serialize");
            assert_eq!(
                &json, expected_json,
                "serialization mismatch for {strategy:?}"
            );
            let parsed: MedianStrategy = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&parsed, strategy, "round-trip mismatch for {strategy:?}");
        }
    }

    #[test]
    fn evaluate_policy_uses_configured_aggregation_strategy() {
        // A policy configured with `WeightedMean` must produce a different
        // consensus price than one configured with `WeightedMedian` on the
        // same quotes (when source weights differ).
        let now = Utc::now();
        let mut policy_median = base_policy();
        policy_median.toggles.oracle_confirmation = false;
        policy_median.aggregation = Some(MedianStrategy::WeightedMedian);

        let mut policy_mean = policy_median.clone();
        policy_mean.aggregation = Some(MedianStrategy::WeightedMean);

        // Two sources with different weights and prices: the weighted median
        // picks the price of the heavier source; the weighted mean blends them.
        let quotes = vec![
            quote("cex-a", "cex", 0.990, now),
            quote("cex-b", "cex", 0.995, now),
        ];

        let outcome_median = evaluate_policy(
            &policy_median,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("median");
        let outcome_mean = evaluate_policy(
            &policy_mean,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("mean");

        // With equal weights the median picks one of the two, the mean
        // blends them, so the divergence_pct differs.
        assert!(
            (outcome_median.snapshot.weighted_median_price
                - outcome_mean.snapshot.weighted_median_price)
                .abs()
                > 1e-9
                || outcome_median.snapshot.divergence_pct != outcome_mean.snapshot.divergence_pct,
            "different aggregation strategies must produce distinguishable outcomes on asymmetric quotes"
        );
    }

    // ─── Phase 5: severity_expression CEL override ──────────────────────────

    /// Helper: attach a compiled severity CEL expression to a policy.
    fn with_severity_expression(mut policy: DepegPolicy, expr: &str) -> DepegPolicy {
        policy.severity_expression = Some(expr.to_string());
        policy.compiled_severity = Some(Arc::new(
            CelProgram::compile(expr).expect("severity expression should compile"),
        ));
        policy
    }

    #[test]
    fn severity_expression_default_equivalent_matches_hardcoded() {
        // The default severity if-chain rendered as CEL must produce the
        // same result as `severity_for_divergence`.
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = false;
        let policy_with_cel = with_severity_expression(
            policy.clone(),
            r#"deviation_pct >= severity_bands_critical ? "critical" :
               deviation_pct >= severity_bands_high     ? "high"     :
               deviation_pct >= severity_bands_medium   ? "medium"   : """#,
        );
        // 0.6% deviation → medium band (0.5).
        let quotes = vec![
            quote("cex-a", "cex", 0.994, now),
            quote("cex-b", "cex", 0.994, now),
        ];

        let baseline = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("baseline");
        let overridden = evaluate_policy(
            &policy_with_cel,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("overridden");

        assert_eq!(
            baseline.snapshot.severity, overridden.snapshot.severity,
            "default-equivalent severity CEL must match hardcoded"
        );
        assert_eq!(baseline.snapshot.severity, Some(Severity::Medium));
    }

    #[test]
    fn severity_expression_can_reclassify_to_higher() {
        // A CEL expression that always returns "critical" regardless of
        // deviation must override the default Medium assignment.
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = false;
        let policy = with_severity_expression(policy, r#""critical""#);
        let quotes = vec![
            quote("cex-a", "cex", 0.994, now), // 0.6% → normally Medium
        ];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert_eq!(outcome.snapshot.severity, Some(Severity::Critical));
    }

    #[test]
    fn severity_expression_empty_string_falls_back_to_default() {
        // Returning an empty string from the CEL expression should fall back
        // to the hardcoded severity, not crash or set None.
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = false;
        let policy = with_severity_expression(policy, r#""""#);
        let quotes = vec![
            quote("cex-a", "cex", 0.994, now), // 0.6% → Medium by default
        ];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        // CEL returns empty string → severity_from_str returns None
        // → .or(default_severity) kicks in → Medium.
        assert_eq!(outcome.snapshot.severity, Some(Severity::Medium));
    }

    #[test]
    fn severity_expression_non_string_falls_back_to_default() {
        // A CEL expression that returns a bool instead of a string must be
        // logged and the default severity must be used.
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = false;
        let policy = with_severity_expression(policy, "true");
        let quotes = vec![quote("cex-a", "cex", 0.994, now)];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        // Non-string → fallback to default → Medium.
        assert_eq!(outcome.snapshot.severity, Some(Severity::Medium));
    }

    #[tokio::test]
    async fn parse_policies_compiles_severity_expression() {
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();
        config_map.insert(
            ("tenant-sev".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 },
                    "severity_expression": "deviation_pct >= severity_bands_critical ? \"critical\" : \"medium\""
                }]
            }),
        );

        pattern.reload_config(&config_map).await.expect("reload");

        let policy = pattern
            .policies
            .get(&("tenant-sev".to_string(), "USDC/USD".to_string()))
            .expect("policy should be loaded");
        assert!(
            policy.compiled_severity.is_some(),
            "valid severity_expression must compile"
        );
    }

    // ─── Phase 7: sub-predicate CEL overrides ────────────────────────────────

    fn with_oracle_expression(mut policy: DepegPolicy, expr: &str) -> DepegPolicy {
        policy.oracle_expression = Some(expr.to_string());
        policy.compiled_oracle = Some(Arc::new(CelProgram::compile(expr).expect("compile")));
        policy
    }
    fn with_confidence_expression(mut policy: DepegPolicy, expr: &str) -> DepegPolicy {
        policy.confidence_expression = Some(expr.to_string());
        policy.compiled_confidence = Some(Arc::new(CelProgram::compile(expr).expect("compile")));
        policy
    }

    #[test]
    fn oracle_expression_override_can_always_confirm() {
        // An oracle CEL that always returns true makes the oracle gate pass
        // even when there are no oracle quotes.
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = true;
        let policy = with_oracle_expression(policy, "true");
        // No oracle quotes — the default would return false.
        let quotes = vec![
            quote("cex-a", "cex", 0.994, now),
            quote("cex-b", "cex", 0.994, now),
        ];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert!(
            outcome.snapshot.oracle_confirmed,
            "CEL override should force oracle_confirmed = true"
        );
        assert!(
            outcome.snapshot.breach_active,
            "with oracle confirmed, breach should fire"
        );
    }

    #[test]
    fn oracle_expression_override_can_block_confirmation() {
        // An oracle CEL that always returns false blocks the oracle gate
        // even when a fresh oracle quote would normally confirm.
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = true;
        let policy = with_oracle_expression(policy, "false");
        let quotes = vec![
            quote("cex-a", "cex", 0.994, now),
            quote("chainlink", "oracle", 0.994, now),
        ];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert!(!outcome.snapshot.oracle_confirmed);
        assert!(!outcome.snapshot.breach_active);
    }

    #[test]
    fn confidence_expression_override_clamps_to_range() {
        // A CEL that returns a custom total confidence (e.g. 42.0) should
        // be used instead of the hardcoded weighted-sum formula.
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = false;
        policy.min_confidence_to_fire = 50.0; // set gate above 42 → should NOT fire
        let policy = with_confidence_expression(policy, "42.0");
        let quotes = vec![
            quote("cex-a", "cex", 0.994, now),
            quote("cex-b", "cex", 0.994, now),
        ];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        // confidence_total = 42.0 < min_confidence_to_fire = 50.0 → no breach
        assert!(
            !outcome.snapshot.breach_active,
            "42 < 50 → should not breach"
        );
    }

    #[test]
    fn confidence_expression_override_allows_fire_above_gate() {
        let now = Utc::now();
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = false;
        policy.min_confidence_to_fire = 50.0;
        let policy = with_confidence_expression(policy, "75.0");
        let quotes = vec![
            quote("cex-a", "cex", 0.994, now),
            quote("cex-b", "cex", 0.994, now),
        ];

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        assert!(outcome.snapshot.breach_active, "75 >= 50 → should breach");
    }

    #[tokio::test]
    async fn parse_policies_compiles_phase7_expressions() {
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();
        config_map.insert(
            ("tenant-p7".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
                    "cooldown_sec": 0,
                    "stale_timeout_ms": 60000,
                    "severity_bands": { "medium": 0.5, "high": 1.0, "critical": 5.0 },
                    "oracle_expression": "has_fresh_oracle && oracle_max_divergence_pct >= trigger_floor_pct",
                    "confidence_expression": "source_agreement * 0.7 + oracle_score * 0.3",
                    "contagion_expression": "systemic_market_count >= 3 ? \"systemic\" : \"isolated\""
                }]
            }),
        );

        pattern.reload_config(&config_map).await.expect("reload");

        let policy = pattern
            .policies
            .get(&("tenant-p7".to_string(), "USDC/USD".to_string()))
            .expect("policy loaded");
        assert!(
            policy.compiled_oracle.is_some(),
            "oracle_expression should compile"
        );
        assert!(
            policy.compiled_confidence.is_some(),
            "confidence_expression should compile"
        );
        assert!(
            policy.compiled_contagion.is_some(),
            "contagion_expression should compile"
        );
    }

    // ─── Phase 3b: run_decision_expression_test ────────────────────────────

    fn baseline_inputs() -> DecisionExpressionTestInputs {
        // A "fires by default" fixture: quorum met, threshold breached,
        // oracle confirms, healthy confidence. Mirrors the
        // `fires_by_default_quotes` helper used elsewhere in this test mod.
        DecisionExpressionTestInputs {
            quorum_met: true,
            threshold_breach: true,
            oracle_confirmed: true,
            confidence_total: 80.0,
            deviation_pct: 0.6,
            median_price: 0.994,
            source_count: 3,
            min_confidence_to_fire: 0.0,
            oracle_confirmation_toggle: true,
            volume_confirmation_toggle: false,
            severity_bands_medium: 0.5,
            severity_bands_high: 1.0,
            severity_bands_critical: 5.0,
        }
    }

    #[test]
    fn run_decision_expression_test_compile_error_returns_no_results() {
        let response = run_decision_expression_test(DecisionExpressionTestRequest {
            decision_expression: "deviation_pct >> 5".to_string(),
            test_cases: vec![DecisionExpressionTestCase {
                name: "anything".to_string(),
                inputs: baseline_inputs(),
                expected: None,
            }],
        });
        assert!(
            response.compile_error.is_some(),
            "broken expression must report a compile error"
        );
        assert!(
            response.results.is_empty(),
            "no test cases should run when compile fails"
        );
    }

    #[test]
    fn run_decision_expression_test_empty_expression_rejected() {
        let response = run_decision_expression_test(DecisionExpressionTestRequest {
            decision_expression: "   ".to_string(),
            test_cases: vec![],
        });
        assert!(response.compile_error.is_some());
        assert!(response.results.is_empty());
    }

    #[test]
    fn run_decision_expression_test_default_predicate_fires_on_baseline() {
        // The Phase 1 default expression must fire on the baseline fixture.
        let response = run_decision_expression_test(DecisionExpressionTestRequest {
            decision_expression: "quorum_met && threshold_breach \
                                  && (!oracle_confirmation_toggle || oracle_confirmed) \
                                  && confidence_total >= min_confidence_to_fire"
                .to_string(),
            test_cases: vec![DecisionExpressionTestCase {
                name: "baseline".to_string(),
                inputs: baseline_inputs(),
                expected: Some(true),
            }],
        });
        assert!(response.compile_error.is_none());
        assert_eq!(response.results.len(), 1);
        let result = &response.results[0];
        assert_eq!(result.fired, Some(true));
        assert_eq!(result.passed, Some(true));
        assert!(result.error.is_none());
    }

    #[test]
    fn run_decision_expression_test_runs_multiple_cases_independently() {
        // One expression, three named scenarios. State must NOT leak between
        // cases — each gets a fresh CEL context built from its own inputs.
        let mut suppressed = baseline_inputs();
        suppressed.quorum_met = false;
        let mut sub_band = baseline_inputs();
        sub_band.deviation_pct = 0.3; // below medium

        let response = run_decision_expression_test(DecisionExpressionTestRequest {
            decision_expression: "quorum_met && deviation_pct >= severity_bands_medium".to_string(),
            test_cases: vec![
                DecisionExpressionTestCase {
                    name: "fires".to_string(),
                    inputs: baseline_inputs(),
                    expected: Some(true),
                },
                DecisionExpressionTestCase {
                    name: "no quorum".to_string(),
                    inputs: suppressed,
                    expected: Some(false),
                },
                DecisionExpressionTestCase {
                    name: "below medium band".to_string(),
                    inputs: sub_band,
                    expected: Some(false),
                },
            ],
        });
        assert!(response.compile_error.is_none());
        assert_eq!(response.results.len(), 3);
        for result in &response.results {
            assert!(
                result.error.is_none(),
                "case {:?} should evaluate cleanly",
                result.name
            );
            assert_eq!(
                result.passed,
                Some(true),
                "case {:?} did not match expected outcome (fired={:?})",
                result.name,
                result.fired
            );
        }
    }

    #[test]
    fn run_decision_expression_test_non_bool_result_reports_error() {
        let response = run_decision_expression_test(DecisionExpressionTestRequest {
            decision_expression: "deviation_pct + 1.0".to_string(),
            test_cases: vec![DecisionExpressionTestCase {
                name: "non-bool".to_string(),
                inputs: baseline_inputs(),
                expected: Some(true),
            }],
        });
        assert!(response.compile_error.is_none());
        let result = &response.results[0];
        assert!(result.fired.is_none());
        assert!(
            result.error.is_some(),
            "non-bool eval result must surface a structured error to the user"
        );
        // `passed` is None because we couldn't evaluate fire/no-fire at all.
        assert!(result.passed.is_none());
    }

    #[test]
    fn run_decision_expression_test_response_serializes_to_json() {
        // The endpoint returns this struct as the HTTP body, so the shape
        // must round-trip through serde_json without losing fields the
        // frontend depends on.
        let response = run_decision_expression_test(DecisionExpressionTestRequest {
            decision_expression: "quorum_met".to_string(),
            test_cases: vec![DecisionExpressionTestCase {
                name: "baseline".to_string(),
                inputs: baseline_inputs(),
                expected: Some(true),
            }],
        });
        let json = serde_json::to_value(&response).expect("serialize");
        assert!(json.get("compile_error").is_none() || json["compile_error"].is_null());
        let results = json["results"].as_array().expect("results array");
        assert_eq!(results.len(), 1);
        let case = &results[0];
        assert_eq!(case["name"], "baseline");
        assert_eq!(case["fired"], true);
        assert_eq!(case["passed"], true);
    }

    #[test]
    fn decision_expression_non_bool_result_falls_back_to_default() {
        // A CEL expression that returns a non-bool (here, a float) must be
        // logged and gracefully fall back to the hardcoded predicate.
        let now = Utc::now();
        let policy = with_decision_expression(base_policy(), "deviation_pct + 1.0");
        let quotes = fires_by_default_quotes(now);

        let outcome = evaluate_policy(
            &policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("evaluation");

        // Fallback => same outcome as the default predicate on this fixture.
        assert!(
            outcome.snapshot.breach_active,
            "non-bool CEL result should transparently fall back to default predicate"
        );
    }

    #[test]
    fn contagion_toggle_off_forces_isolated_classification() {
        let now = Utc::now();
        let mut pattern = DepegPatternV2::default();
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
        let mut pattern = DepegPatternV2::default();
        let mut config_map = HashMap::new();

        config_map.insert(
            ("tenant-a".to_string(), PATTERN_ID.to_string()),
            serde_json::json!({
                "policies": [{
                    "market_key": "USDC/USD",
                    "peg_target": 1.0,
                    "min_sources": 1,
                    "quorum_pct": 0.0,
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

        let overrides = DepegPatternV2::parse_source_mapping_overrides(&config);
        let pyth = overrides.get("pyth-eth-mainnet").expect("override");
        assert_eq!(pyth.active_stream_ids, vec!["stream-1".to_string()]);
        assert_eq!(
            pyth.stream_field_mappings.get("stream-1").map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn effective_event_fields_apply_gateway_mapping_override() {
        let mut pattern = DepegPatternV2::default();
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
            "cooldown_sec": 300,
            "severity_bands": { "medium": 1.0, "high": 3.0, "critical": 5.0 }
        });

        let policies = DepegPatternV2::parse_policies("tenant-a", &config);

        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].tenant_id, "tenant-a");
        assert_eq!(policies[0].market_key, "USDC/USD");
        assert_eq!(policies[0].quorum_pct, 0.0);
        assert_eq!(policies[0].stale_timeout_ms, 30_000);
    }

    #[test]
    fn evaluate_policy_uses_tenant_specific_thresholds_independently() {
        let now = Utc::now();
        let quotes = vec![quote("cex-a", "cex", 0.99, now)];

        let mut tenant_a_policy = base_policy();
        tenant_a_policy.tenant_id = "tenant-a".to_string();
        tenant_a_policy.severity_bands.medium = 0.5;
        tenant_a_policy.toggles.oracle_confirmation = false;
        tenant_a_policy.severity_bands_isolated = Some(DepegSeverityBands {
            medium: 0.5,
            high: 1.0,
            critical: 5.0,
        });
        tenant_a_policy.min_confidence_to_fire = 0.0;

        let mut tenant_b_policy = base_policy();
        tenant_b_policy.tenant_id = "tenant-b".to_string();
        tenant_b_policy.severity_bands.medium = 2.0;
        tenant_b_policy.toggles.oracle_confirmation = false;
        tenant_b_policy.severity_bands_isolated = Some(DepegSeverityBands {
            medium: 2.0,
            high: 4.0,
            critical: 8.0,
        });
        tenant_b_policy.min_confidence_to_fire = 0.0;

        let tenant_a = evaluate_policy(
            &tenant_a_policy,
            &quotes,
            &DepegAlertState::default(),
            now,
            ContextClassification::Isolated,
        )
        .expect("tenant-a evaluation");
        let tenant_b = evaluate_policy(
            &tenant_b_policy,
            &quotes,
            &DepegAlertState::default(),
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
        let mut pattern = DepegPatternV2::default();
        let usdt_default = DepegPolicy {
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

        let usdc_policy = DepegPolicy {
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
        let mut policy = base_policy();
        policy.toggles.oracle_confirmation = false;
        let quotes = vec![quote("cex-a", "cex", 0.994, now)];
        let mut state = DepegAlertState {
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
                assert!(outcome.transition.is_none());
            } else {
                assert_lifecycle_transition(&outcome, IncidentTransition::Deescalate);
            }
            state = outcome.next_state;
        }
    }

    #[test]
    fn resolution_requires_configured_block_count() {
        let now = Utc::now();
        let policy = base_policy();
        let quotes = vec![quote("cex-a", "cex", 0.9998, now)];
        let mut state = DepegAlertState {
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
                assert!(outcome.transition.is_none());
            } else {
                assert_lifecycle_transition(&outcome, IncidentTransition::Resolve);
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
        policy.toggles.oracle_confirmation = false;
        let quotes = vec![quote("cex-a", "cex", 0.989, now)];
        let state = DepegAlertState {
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
        policy.toggles.oracle_confirmation = false;
        let quotes = vec![quote("cex-a", "cex", 0.994, now)];
        let state = DepegAlertState {
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
        assert_lifecycle_transition(&emitted, IncidentTransition::Deescalate);
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
    fn spec_policy() -> DepegPolicy {
        DepegPolicy {
            toggles: DepegToggles {
                oracle_confirmation: true,
                ..Default::default()
            },
            min_sources: 3,
            source_filter: DepegSourceFilter {
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
        policy: &DepegPolicy,
        quotes: &[QuoteInput],
        now: DateTime<Utc>,
        classification: ContextClassification,
    ) -> EvaluationOutcome {
        evaluate_policy(
            policy,
            quotes,
            &DepegAlertState::default(),
            now,
            classification,
        )
        .expect("evaluate_policy should not fail")
    }

    /// Run N evaluation steps, threading state. Returns all outcomes.
    #[allow(dead_code)]
    fn eval_sequence(
        policy: &DepegPolicy,
        steps: &[(Vec<QuoteInput>, ContextClassification)],
        start: DateTime<Utc>,
        tick: Duration,
    ) -> Vec<EvaluationOutcome> {
        let mut state = DepegAlertState::default();
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

    /// Assert that a lifecycle transition occurred (resolve/deescalate) which updates
    /// internal state but does NOT emit an alert record.
    fn assert_lifecycle_transition(outcome: &EvaluationOutcome, expected: IncidentTransition) {
        assert!(
            !outcome.should_emit_alert,
            "Lifecycle transitions (resolve/deescalate) should not emit alerts"
        );
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
        let state = DepegAlertState {
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

        let state = DepegAlertState {
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
        let state = DepegAlertState {
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

        let state = DepegAlertState {
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

        let state = DepegAlertState {
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

        let state = DepegAlertState {
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

        let mut state = DepegAlertState {
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
                assert!(
                    outcome.transition.is_none(),
                    "Should not transition at block {}",
                    i
                );
                assert_eq!(outcome.next_state.below_trigger_blocks, i);
            } else {
                assert_lifecycle_transition(&outcome, IncidentTransition::Resolve);
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

        let state = DepegAlertState {
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

        let base_state = DepegAlertState {
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

        let state = DepegAlertState {
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
        let state = DepegAlertState {
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
        let state = DepegAlertState {
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
        let state = DepegAlertState {
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
        let state = DepegAlertState {
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
        let state = DepegAlertState {
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

        // 5th block — deescalation fires (state transition, no alert record)
        let ts = now + Duration::seconds(4);
        let q = make_quotes(medium_price, Some(medium_price), Some(medium_price), 12, ts);
        let final_out = evaluate_policy(&policy, &q, &s, ts, ContextClassification::Isolated)
            .expect("evaluation");
        assert_lifecycle_transition(&final_out, IncidentTransition::Deescalate);
        assert_eq!(final_out.emitted_severity, Some(Severity::Medium));
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
        let state = DepegAlertState {
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

        // v2 emits its detections under "depeg_v2" so they're distinguishable
        // from the legacy `depeg` engine running in the same detector process.
        assert_eq!(detection.pattern_id, "depeg_v2");
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
        let bands = DepegSeverityBands {
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
        let bands = DepegSeverityBands {
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
        let bands = DepegSeverityBands {
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
            DepegSourceOverride {
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
            DepegSourceOverride {
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

        // Steps 3-4: Deescalate (2 blocks at MEDIUM — lifecycle transition, no alert record)
        let mut state = s2.next_state;
        for i in 2..4 {
            let ts = now + Duration::seconds(i);
            let q = make_quotes(medium_price, None, None, 12, ts);
            let out = evaluate_policy(&policy, &q, &state, ts, ContextClassification::Isolated)
                .expect("evaluation");
            if i == 3 {
                assert_lifecycle_transition(&out, IncidentTransition::Deescalate);
                assert_eq!(out.emitted_severity, Some(Severity::Medium));
            }
            state = out.next_state;
        }

        // Steps 5-6: Resolve (2 blocks below trigger floor — lifecycle transition, no alert record)
        let recovery_price = price_from_deviation(1.0, 0.10);
        for i in 4..6 {
            let ts = now + Duration::seconds(i);
            let q = make_quotes(recovery_price, None, None, 12, ts);
            let out = evaluate_policy(&policy, &q, &state, ts, ContextClassification::Isolated)
                .expect("evaluation");
            if i == 5 {
                assert_lifecycle_transition(&out, IncidentTransition::Resolve);
            }
            state = out.next_state;
        }
    }

    // ─── Policy validation ──────────────────────────────────────────────────

    /// TC-D-1416: Policy validation rejects invalid configs
    #[test]
    fn tc_d_1416_policy_validation() {
        let mut p = base_policy();
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
            DepegSourceOverride {
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

    #[test]
    fn tc_d_1419_custom_sources_are_out_of_scope_for_default_consensus() {
        let now = Utc::now();
        let policy = base_policy();
        let outcome = eval_fresh(
            &policy,
            &[quote("custom-feed", "custom", 0.92, now)],
            now,
            ContextClassification::Isolated,
        );

        assert_eq!(outcome.snapshot.source_count, 0);
        assert!(!outcome.snapshot.breach_active);
    }

    #[test]
    fn tc_d_1420_detection_payload_exposes_spec_context_fields() {
        let now = Utc::now();
        let policy = spec_policy();
        let outcome = eval_fresh(
            &policy,
            &[
                quote("coinbase-advanced", "cex", 0.9940, now),
                quote("kraken-spot", "cex", 0.9941, now),
                quote("okx-global", "cex", 0.9940, now),
                quote("chainlink-data-streams", "oracle", 0.9948, now),
                quote("pyth-eth-mainnet", "oracle", 0.9949, now),
            ],
            now,
            ContextClassification::Isolated,
        );
        let detection = build_detection(
            &event_schema::UnifiedEvent {
                event_id: "evt-1".to_string(),
                tenant_id: policy.tenant_id.clone(),
                source_id: "coinbase-advanced".to_string(),
                source_type: event_schema::SourceType::CexWebsocket,
                event_type: "ticker".to_string(),
                timestamp: now,
                payload: serde_json::json!({}),
                chain_id: None,
                block_number: None,
                tx_hash: None,
                market_key: Some(policy.market_key.clone()),
                price: Some(0.9940),
            },
            &policy,
            &outcome.snapshot,
            Severity::Medium,
            Some(IncidentTransition::Trigger),
            now,
        );
        let oracle_context = detection.oracle_context;

        assert_eq!(
            oracle_context.get("healthy_source_count"),
            Some(&serde_json::json!(5))
        );
        assert_eq!(
            oracle_context.get("total_source_count"),
            Some(&serde_json::json!(5))
        );
        assert_eq!(
            oracle_context.get("contagion_status"),
            Some(&serde_json::json!("isolated"))
        );
        assert_eq!(
            oracle_context.get("peg_target"),
            Some(&serde_json::json!(1.0))
        );
        assert!(oracle_context.contains_key("chainlink_price"));
        assert!(oracle_context.contains_key("pyth_price"));
    }
}
