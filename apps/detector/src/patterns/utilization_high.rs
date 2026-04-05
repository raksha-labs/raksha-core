//! High Utilization detection pattern.
//!
//! Monitors borrow-to-supply ratio in lending protocol markets (`UnifiedEvent`
//! with `event_type = "protocol_state"` and `metric = "utilization"`). Fires
//! a `DetectionResult` when utilization exceeds per-tenant thresholds.
//!
//! Implements a sustained-breach lifecycle:
//!   - Trigger   → first crossing of a threshold
//!   - Escalate  → threshold tier increases (Medium → High → Critical)
//!   - Resolve   → utilization remains below resolution threshold for
//!     `resolution_confirmation_blocks` consecutive blocks
//!
//! Protocol-pause events suspend auto-resolution and emit an Update signal.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use event_schema::{
    AttackFamily, Chain, ContextClassification, DetectionResult, DetectionSignal,
    IncidentTransition, LifecycleState, RiskScore, Severity, SignalType, UnifiedEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use state_manager::{PatternSnapshotInsert, PostgresRepository};
use uuid::Uuid;

use super::{append_snapshot_meta, simulation_metadata_from_event, DetectionPattern};

pub const PATTERN_ID: &str = "utilization_high";

// ─── Default thresholds ───────────────────────────────────────────────────────

const DEFAULT_MEDIUM_THRESHOLD_PCT: f64 = 90.0;
const DEFAULT_HIGH_THRESHOLD_PCT: f64 = 95.0;
const DEFAULT_CRITICAL_THRESHOLD_PCT: f64 = 99.0;
const DEFAULT_RESOLUTION_MEDIUM_PCT: f64 = 85.0;
const DEFAULT_RESOLUTION_HIGH_PCT: f64 = 88.0;
const DEFAULT_RESOLUTION_CRITICAL_PCT: f64 = 90.0;
const DEFAULT_RESOLUTION_CONFIRMATION_BLOCKS: u32 = 10;
const DEFAULT_MIN_TVL_FLOOR_USD: f64 = 500_000.0;

// ─── Config ──────────────────────────────────────────────────────────────────

/// Per-rule configuration for the High Utilization pattern.
///
/// One rule covers one `(protocol_id, chain_slug, scope, market_id)` tuple.
/// Multiple rules per tenant are supported so a single tenant can monitor
/// different markets with different thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtilizationHighRule {
    pub rule_id: String,
    pub protocol_id: String,
    pub chain_slug: String,
    /// `"protocol"` or `"market"`.  Market scope requires `market_id`.
    pub scope: String,
    pub market_id: Option<String>,

    #[serde(default = "default_medium_threshold")]
    pub medium_threshold_pct: f64,
    #[serde(default = "default_high_threshold")]
    pub high_threshold_pct: f64,
    #[serde(default = "default_critical_threshold")]
    pub critical_threshold_pct: f64,

    #[serde(default = "default_resolution_medium")]
    pub resolution_medium_pct: f64,
    #[serde(default = "default_resolution_high")]
    pub resolution_high_pct: f64,
    #[serde(default = "default_resolution_critical")]
    pub resolution_critical_pct: f64,

    #[serde(default = "default_resolution_confirmation_blocks")]
    pub resolution_confirmation_blocks: u32,

    #[serde(default = "default_min_tvl_floor_usd")]
    pub min_tvl_floor_usd: f64,

    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_medium_threshold() -> f64 {
    DEFAULT_MEDIUM_THRESHOLD_PCT
}
fn default_high_threshold() -> f64 {
    DEFAULT_HIGH_THRESHOLD_PCT
}
fn default_critical_threshold() -> f64 {
    DEFAULT_CRITICAL_THRESHOLD_PCT
}
fn default_resolution_medium() -> f64 {
    DEFAULT_RESOLUTION_MEDIUM_PCT
}
fn default_resolution_high() -> f64 {
    DEFAULT_RESOLUTION_HIGH_PCT
}
fn default_resolution_critical() -> f64 {
    DEFAULT_RESOLUTION_CRITICAL_PCT
}
fn default_resolution_confirmation_blocks() -> u32 {
    DEFAULT_RESOLUTION_CONFIRMATION_BLOCKS
}
fn default_min_tvl_floor_usd() -> f64 {
    DEFAULT_MIN_TVL_FLOOR_USD
}
fn default_true() -> bool {
    true
}

impl UtilizationHighRule {
    fn validate(&self) -> Result<()> {
        if self.rule_id.trim().is_empty() {
            return Err(anyhow!("rule_id must be non-empty"));
        }
        if self.protocol_id.trim().is_empty() {
            return Err(anyhow!("protocol_id must be non-empty"));
        }
        if self.chain_slug.trim().is_empty() {
            return Err(anyhow!("chain_slug must be non-empty"));
        }
        if self.scope.eq_ignore_ascii_case("market")
            && self.market_id.as_deref().unwrap_or("").trim().is_empty()
        {
            return Err(anyhow!("market scope requires market_id"));
        }
        if self.medium_threshold_pct >= self.high_threshold_pct {
            return Err(anyhow!("medium_threshold_pct must be < high_threshold_pct"));
        }
        if self.high_threshold_pct >= self.critical_threshold_pct {
            return Err(anyhow!(
                "high_threshold_pct must be < critical_threshold_pct"
            ));
        }
        if self.resolution_medium_pct >= self.medium_threshold_pct {
            return Err(anyhow!(
                "resolution_medium_pct must be < medium_threshold_pct (hysteresis gap)"
            ));
        }
        if self.resolution_high_pct >= self.high_threshold_pct {
            return Err(anyhow!(
                "resolution_high_pct must be < high_threshold_pct (hysteresis gap)"
            ));
        }
        if self.resolution_critical_pct >= self.critical_threshold_pct {
            return Err(anyhow!(
                "resolution_critical_pct must be < critical_threshold_pct (hysteresis gap)"
            ));
        }
        if self.resolution_confirmation_blocks == 0 {
            return Err(anyhow!("resolution_confirmation_blocks must be >= 1"));
        }
        if self.min_tvl_floor_usd < 0.0 {
            return Err(anyhow!("min_tvl_floor_usd must be >= 0"));
        }
        Ok(())
    }

    fn normalized_scope(&self) -> &str {
        if self.scope.eq_ignore_ascii_case("market") {
            "market"
        } else {
            "protocol"
        }
    }

    fn matches(&self, protocol_id: &str, chain_slug: &str, market_id: Option<&str>) -> bool {
        if !self.protocol_id.eq_ignore_ascii_case(protocol_id) {
            return false;
        }
        if !self.chain_slug.eq_ignore_ascii_case(chain_slug) {
            return false;
        }
        if self.normalized_scope() == "market" {
            let expected = self.market_id.as_deref().unwrap_or_default();
            let observed = market_id.unwrap_or_default();
            return expected.eq_ignore_ascii_case(observed);
        }
        true
    }

    fn subject_for_event(&self, market_id: Option<&str>) -> Option<(String, String, String)> {
        let protocol_key = format!(
            "{}:{}",
            self.protocol_id.to_ascii_lowercase(),
            self.chain_slug.to_ascii_lowercase()
        );
        if self.normalized_scope() == "market" {
            let market = self
                .market_id
                .as_deref()
                .or(market_id)
                .map(str::trim)
                .filter(|v| !v.is_empty())?
                .to_ascii_lowercase();
            let subject_key = format!("{protocol_key}:{market}");
            return Some(("market".to_string(), subject_key, protocol_key));
        }
        Some(("protocol".to_string(), protocol_key.clone(), protocol_key))
    }

    /// Return the resolution threshold for `severity`.
    fn resolution_threshold_for(&self, severity: &str) -> f64 {
        match severity {
            "critical" => self.resolution_critical_pct,
            "high" => self.resolution_high_pct,
            _ => self.resolution_medium_pct,
        }
    }
}

// ─── Per-rule state ───────────────────────────────────────────────────────────

/// Rolling utilization sample for rate-of-change computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UtilizationSample {
    observed_at: DateTime<Utc>,
    utilization_pct: f64,
}

/// Persisted state for one `(rule_id, subject_key)` pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct UtilizationRuleState {
    /// Severity of the currently active incident, e.g. `"medium"`, `"high"`, `"critical"`.
    last_severity: Option<String>,
    /// When the current incident was first triggered.
    active_since: Option<DateTime<Utc>>,
    /// Timestamp of the most recent block that breached a threshold.
    last_breach_at: Option<DateTime<Utc>>,
    /// Timestamp of the most recent `DetectionResult` emitted.
    last_transition_at: Option<DateTime<Utc>>,
    /// Whether the protocol is currently paused (auto-resolution suspended).
    paused_status: bool,
    /// Consecutive blocks below resolution threshold (resets if utilization bounces).
    resolution_block_counter: u32,
    /// Rolling window of utilization samples for rate-of-change.
    samples: Vec<UtilizationSample>,
    /// Number of severity escalations during the current incident lifetime.
    #[serde(default)]
    escalation_count: u32,
}

/// Machine-readable recommended action per Spec Section 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RecommendedAction {
    EmergencyWithdraw,
    MonitorForUnpause,
    WithdrawMaxAvailable,
    Monitor,
}

impl RecommendedAction {
    fn as_str(&self) -> &'static str {
        match self {
            Self::EmergencyWithdraw => "EMERGENCY_WITHDRAW",
            Self::MonitorForUnpause => "MONITOR_FOR_UNPAUSE",
            Self::WithdrawMaxAvailable => "WITHDRAW_MAX_AVAILABLE",
            Self::Monitor => "MONITOR",
        }
    }
}

fn recommended_action_for(severity: &Severity, paused: bool) -> RecommendedAction {
    if paused {
        return RecommendedAction::MonitorForUnpause;
    }
    match severity {
        Severity::Critical => RecommendedAction::EmergencyWithdraw,
        Severity::High => RecommendedAction::WithdrawMaxAvailable,
        _ => RecommendedAction::Monitor,
    }
}

/// Outcome of evaluating a single utilization reading against one rule.
/// Returned by [`evaluate_utilization_state`] and used by `process_event`
/// and unit tests.
#[derive(Debug, Clone, Default)]
pub(crate) struct UtilizationEvalResult {
    /// The event was suppressed by the TVL floor gate.
    pub tvl_floor_suppressed: bool,
    /// The lifecycle transition to emit, if any.
    pub transition: Option<IncidentTransition>,
    /// The severity to attach to the emitted detection (Trigger/Escalate only).
    pub emit_severity: Option<Severity>,
    /// The incident was auto-resolved in this evaluation.
    pub resolved: bool,
    /// The severity from which the incident resolved (set only when `resolved`).
    pub resolved_from_severity: Option<Severity>,
    /// Machine-readable recommended action (Spec Section 10).
    pub recommended_action: Option<RecommendedAction>,
    /// When the incident was first triggered (carried into resolution for duration calc).
    pub incident_active_since: Option<DateTime<Utc>>,
    /// Number of escalations that occurred during the incident lifetime.
    pub escalation_count: u32,
}

// ─── Parsed event helpers ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct UtilizationStateEvent {
    protocol_id: String,
    chain_slug: String,
    market_id: Option<String>,
    /// Utilization as a percentage (0.0 – 100.0).
    utilization_pct: f64,
    tvl_usd: f64,
    total_supplied_tokens: f64,
    total_borrowed_tokens: f64,
    block_number: i64,
    tx_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct UtilizationPauseEvent {
    protocol_id: String,
    chain_slug: String,
    market_id: Option<String>,
    paused: bool,
    block_number: i64,
    tx_hash: Option<String>,
}

fn parse_utilization_state_event(event: &UnifiedEvent) -> Option<UtilizationStateEvent> {
    if !event.event_type.eq_ignore_ascii_case("protocol_state") {
        return None;
    }
    let payload = event.payload.as_object()?;
    let metric = payload.get("metric")?.as_str()?;
    if !metric.eq_ignore_ascii_case("utilization") {
        return None;
    }

    let protocol_id = payload.get("protocol_id")?.as_str()?.to_string();
    let chain_slug = payload
        .get("chain_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("base")
        .to_string();
    let market_id = payload
        .get("market_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Accept utilization as either a ratio (0.0–1.0) or a percentage (0.0–100.0).
    // The spec uses ratio internally; convert to percentage for threshold comparison.
    let raw_utilization = payload
        .get("utilization")
        .or_else(|| payload.get("current_utilization"))
        .and_then(|v| v.as_f64())?;
    let utilization_pct = if raw_utilization <= 1.0 {
        raw_utilization * 100.0
    } else {
        raw_utilization
    };

    let tvl_usd = payload
        .get("tvl_usd")
        .or_else(|| payload.get("window_start_tvl_usd"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let total_supplied_tokens = payload
        .get("total_supplied_tokens")
        .or_else(|| payload.get("total_supplied"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let total_borrowed_tokens = payload
        .get("total_borrowed_tokens")
        .or_else(|| payload.get("total_borrowed"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let block_number = payload
        .get("block_number")
        .and_then(|v| v.as_i64())
        .or(event.block_number)
        .unwrap_or(0);
    let tx_hash = payload
        .get("tx_hash")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| event.tx_hash.clone());

    Some(UtilizationStateEvent {
        protocol_id,
        chain_slug,
        market_id,
        utilization_pct,
        tvl_usd,
        total_supplied_tokens,
        total_borrowed_tokens,
        block_number,
        tx_hash,
    })
}

fn parse_utilization_pause_event(event: &UnifiedEvent) -> Option<UtilizationPauseEvent> {
    if !event.event_type.eq_ignore_ascii_case("protocol_pause") {
        return None;
    }
    let payload = event.payload.as_object()?;
    let protocol_id = payload.get("protocol_id")?.as_str()?.to_string();
    let chain_slug = payload
        .get("chain_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("base")
        .to_string();
    let market_id = payload
        .get("market_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let paused = payload
        .get("paused")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let block_number = payload
        .get("block_number")
        .and_then(|v| v.as_i64())
        .or(event.block_number)
        .unwrap_or(0);
    let tx_hash = payload
        .get("tx_hash")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| event.tx_hash.clone());

    Some(UtilizationPauseEvent {
        protocol_id,
        chain_slug,
        market_id,
        paused,
        block_number,
        tx_hash,
    })
}

// ─── Config parsing ───────────────────────────────────────────────────────────

fn parse_utilization_rules(config: &Value, tenant_id: &str) -> Vec<UtilizationHighRule> {
    // Accept either:
    //   { "rules": [ {...}, ... ] }   (list of rules)
    //   { "rule_id": "...", ... }     (single rule object — wrap in vec)
    let raw_rules: Vec<Value> = if let Some(arr) = config.get("rules").and_then(|v| v.as_array()) {
        arr.clone()
    } else if config.is_object() {
        vec![config.clone()]
    } else {
        return Vec::new();
    };

    raw_rules
        .into_iter()
        .filter_map(|raw| {
            let mut rule: UtilizationHighRule = serde_json::from_value(raw).ok()?;
            if rule.rule_id.trim().is_empty() {
                rule.rule_id = format!("{}-utilization-high-default", tenant_id);
            }
            match rule.validate() {
                Ok(()) => Some(rule),
                Err(err) => {
                    tracing::warn!(
                        tenant_id,
                        rule_id = rule.rule_id,
                        error = %err,
                        "skipping invalid utilization_high rule"
                    );
                    None
                }
            }
        })
        .collect()
}

// ─── Utility helpers ──────────────────────────────────────────────────────────

fn severity_from_str(s: Option<&str>) -> Option<Severity> {
    match s? {
        "critical" => Some(Severity::Critical),
        "high" => Some(Severity::High),
        "medium" => Some(Severity::Medium),
        _ => None,
    }
}

fn severity_to_str(s: &Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
        Severity::Info => "info",
    }
}

fn severity_rank(s: Option<&Severity>) -> u8 {
    match s {
        Some(Severity::Critical) => 4,
        Some(Severity::High) => 3,
        Some(Severity::Medium) => 2,
        Some(Severity::Low) => 1,
        _ => 0,
    }
}

fn classify_severity(utilization_pct: f64, rule: &UtilizationHighRule) -> Option<Severity> {
    if utilization_pct >= rule.critical_threshold_pct {
        Some(Severity::Critical)
    } else if utilization_pct >= rule.high_threshold_pct {
        Some(Severity::High)
    } else if utilization_pct >= rule.medium_threshold_pct {
        Some(Severity::Medium)
    } else {
        None
    }
}

fn chain_from_slug(slug: &str) -> Chain {
    match slug.to_ascii_lowercase().as_str() {
        "base" => Chain::Base,
        "ethereum" | "mainnet" => Chain::Ethereum,
        "arbitrum" => Chain::Arbitrum,
        "optimism" => Chain::Optimism,
        "polygon" => Chain::Polygon,
        "avalanche" => Chain::Avalanche,
        "bsc" => Chain::BSC,
        _ => Chain::Unknown,
    }
}

fn risk_score_for_severity(severity: &Severity) -> f64 {
    match severity {
        Severity::Critical => 95.0,
        Severity::High => 80.0,
        Severity::Medium => 60.0,
        Severity::Low => 30.0,
        Severity::Info => 10.0,
    }
}

fn actions_for_severity(severity: &Severity, paused: bool) -> Vec<String> {
    if paused {
        return vec![
            "Protocol is paused. Monitor for unpause.".to_string(),
            "Withdrawal not currently possible — pool locked or governance-paused.".to_string(),
        ];
    }
    match severity {
        Severity::Critical => vec![
            "EMERGENCY_WITHDRAW: Exit the market immediately.".to_string(),
            "Available liquidity may disappear within minutes.".to_string(),
        ],
        Severity::High => vec![
            "WITHDRAW_MAX_AVAILABLE: Withdraw the maximum available portion.".to_string(),
            "Full exit may not be possible — take out what you can now.".to_string(),
        ],
        Severity::Medium => vec![
            "MONITOR: Utilization is elevated but withdrawal is fully available.".to_string(),
            "Watch for further deterioration.".to_string(),
        ],
        _ => vec!["MONITOR: Continue monitoring.".to_string()],
    }
}

/// Compute utilization 10 minutes and 60 minutes ago from the rolling window.
fn rate_of_change(
    samples: &[UtilizationSample],
    now: DateTime<Utc>,
    window_minutes: i64,
) -> Option<f64> {
    let cutoff = now - Duration::minutes(window_minutes);
    // Find the oldest sample within the window as the baseline
    let baseline = samples
        .iter()
        .filter(|s| s.observed_at >= cutoff)
        .min_by_key(|s| s.observed_at)?;
    // Current is the most recent sample
    let current = samples.iter().max_by_key(|s| s.observed_at)?;
    Some(current.utilization_pct - baseline.utilization_pct)
}

// ─── Pure evaluation logic ────────────────────────────────────────────────────

/// Pure evaluation of a utilization reading against one rule.
///
/// Mutates `state` in place (rolling window, counters, severity tracking)
/// and returns the lifecycle outcome without performing any I/O.  Used by
/// `process_event` and directly by unit tests.
pub(crate) fn evaluate_utilization_state(
    rule: &UtilizationHighRule,
    state: &mut UtilizationRuleState,
    utilization_pct: f64,
    tvl_usd: f64,
    now: DateTime<Utc>,
) -> UtilizationEvalResult {
    let mut result = UtilizationEvalResult::default();

    // Gate: TVL floor
    if tvl_usd < rule.min_tvl_floor_usd && tvl_usd > 0.0 {
        result.tvl_floor_suppressed = true;
        return result;
    }

    // Update rolling utilization window (keep last 61 minutes)
    state.samples.push(UtilizationSample {
        observed_at: now,
        utilization_pct,
    });
    state.samples.sort_by_key(|s| s.observed_at);
    let window_cutoff = now - Duration::minutes(61);
    state.samples.retain(|s| s.observed_at >= window_cutoff);

    let previous_severity = severity_from_str(state.last_severity.as_deref());
    let new_severity = classify_severity(utilization_pct, rule);

    match (&previous_severity, &new_severity) {
        (None, Some(sev)) => {
            // First breach — Trigger
            result.transition = Some(IncidentTransition::Trigger);
            result.emit_severity = Some(sev.clone());
            result.recommended_action = Some(recommended_action_for(sev, state.paused_status));
            state.active_since = Some(now);
            state.last_breach_at = Some(now);
            state.last_severity = Some(severity_to_str(sev).to_string());
            state.resolution_block_counter = 0;
            state.escalation_count = 0;
            state.last_transition_at = Some(now);
        }
        (Some(prev), Some(new)) => {
            let prev_rank = severity_rank(Some(prev));
            let new_rank = severity_rank(Some(new));
            if new_rank > prev_rank {
                // Escalate
                result.transition = Some(IncidentTransition::Escalate);
                result.emit_severity = Some(new.clone());
                result.recommended_action = Some(recommended_action_for(new, state.paused_status));
                state.last_severity = Some(severity_to_str(new).to_string());
                state.last_breach_at = Some(now);
                state.resolution_block_counter = 0;
                state.escalation_count += 1;
                state.last_transition_at = Some(now);
            } else {
                // Same or lower severity while incident is active → suppress
                state.last_breach_at = Some(now);
                state.resolution_block_counter = 0;
            }
        }
        (Some(prev_sev), None) => {
            // Below detection threshold — check resolution path
            let resolution_threshold =
                rule.resolution_threshold_for(state.last_severity.as_deref().unwrap_or("medium"));

            if utilization_pct < resolution_threshold && !state.paused_status {
                state.resolution_block_counter += 1;
                if state.resolution_block_counter >= rule.resolution_confirmation_blocks {
                    // Resolve — capture incident metadata before clearing state
                    result.transition = Some(IncidentTransition::Resolve);
                    result.resolved = true;
                    result.resolved_from_severity = Some(prev_sev.clone());
                    result.incident_active_since = state.active_since;
                    result.escalation_count = state.escalation_count;
                    state.last_severity = None;
                    state.active_since = None;
                    state.last_breach_at = None;
                    state.last_transition_at = Some(now);
                    state.resolution_block_counter = 0;
                    state.escalation_count = 0;
                }
            } else if utilization_pct >= resolution_threshold {
                // Still above resolution threshold — reset counter
                state.resolution_block_counter = 0;
            }
            // paused_status == true: don't advance the counter
        }
        (None, None) => {
            // No incident, no breach — nothing to do
        }
    }

    // Populate recommended_action for active incidents that didn't transition
    if result.recommended_action.is_none() {
        if let Some(ref sev_str) = state.last_severity {
            if let Some(sev) = severity_from_str(Some(sev_str)) {
                result.recommended_action = Some(recommended_action_for(&sev, state.paused_status));
            }
        }
    }
    result.incident_active_since = result.incident_active_since.or(state.active_since);
    result.escalation_count = if state.escalation_count > 0 {
        state.escalation_count
    } else {
        result.escalation_count
    };

    result
}

// ─── Pattern struct ───────────────────────────────────────────────────────────

#[derive(Default)]
pub struct UtilizationHighPattern {
    /// tenant_id → rules
    configs: HashMap<String, Vec<UtilizationHighRule>>,
    /// `{tenant_id}:{rule_id}:{subject_key}` → state
    state_cache: HashMap<String, UtilizationRuleState>,
    /// tenant_id → set of enabled source_ids (None = unrestricted)
    source_bindings: HashMap<String, HashSet<String>>,
}

struct UtilizationDetectionContext<'a> {
    subject_type: &'a str,
    subject_key: &'a str,
    severity: &'a Severity,
    transition: &'a IncidentTransition,
    observed_at: DateTime<Utc>,
}

struct ResolveDetectionContext<'a> {
    subject_type: &'a str,
    subject_key: &'a str,
    resolved_severity: &'a Severity,
    observed_at: DateTime<Utc>,
    incident_active_since: Option<DateTime<Utc>>,
    escalation_count: u32,
}

impl UtilizationHighPattern {
    fn state_key(rule_id: &str, subject_key: &str) -> String {
        format!("{rule_id}:{subject_key}")
    }

    fn cache_key(tenant_id: &str, state_key: &str) -> String {
        format!("{tenant_id}:{state_key}")
    }

    async fn effective_rules(
        &mut self,
        tenant_id: &str,
        _repo: &PostgresRepository,
    ) -> Result<Option<Vec<UtilizationHighRule>>> {
        Ok(self.configs.get(tenant_id).cloned())
    }

    // ─── DetectionResult builders ─────────────────────────────────────────────

    fn build_utilization_detection(
        event: &UnifiedEvent,
        rule: &UtilizationHighRule,
        context: &UtilizationDetectionContext<'_>,
        sample: &UtilizationStateEvent,
        state: &UtilizationRuleState,
    ) -> DetectionResult {
        let (is_simulated, simulation_run_id) = simulation_metadata_from_event(event);

        let rate_10min = rate_of_change(&state.samples, context.observed_at, 10).unwrap_or(0.0);
        let rate_60min = rate_of_change(&state.samples, context.observed_at, 60).unwrap_or(0.0);
        let available_liquidity_usd =
            (sample.tvl_usd * (1.0 - (sample.utilization_pct / 100.0))).max(0.0);
        let breached_threshold_pct = match context.severity {
            Severity::Critical => rule.critical_threshold_pct,
            Severity::High => rule.high_threshold_pct,
            Severity::Medium | Severity::Low | Severity::Info => rule.medium_threshold_pct,
        };
        let resolution_threshold_pct =
            rule.resolution_threshold_for(severity_to_str(context.severity));
        let exit_feasibility = if state.paused_status {
            "protocol_paused"
        } else if sample.utilization_pct >= rule.critical_threshold_pct {
            "severely_constrained"
        } else if sample.utilization_pct >= rule.high_threshold_pct {
            "constrained"
        } else {
            "open"
        };

        let mut oracle_context = HashMap::new();
        oracle_context.insert("protocol_id".to_string(), json!(sample.protocol_id));
        oracle_context.insert("chain_slug".to_string(), json!(sample.chain_slug));
        oracle_context.insert("market_id".to_string(), json!(sample.market_id));
        oracle_context.insert(
            "current_utilization_pct".to_string(),
            json!(sample.utilization_pct),
        );
        oracle_context.insert(
            "total_supplied_tokens".to_string(),
            json!(sample.total_supplied_tokens),
        );
        oracle_context.insert(
            "total_borrowed_tokens".to_string(),
            json!(sample.total_borrowed_tokens),
        );
        oracle_context.insert("tvl_usd".to_string(), json!(sample.tvl_usd));
        oracle_context.insert(
            "available_liquidity_usd".to_string(),
            json!(available_liquidity_usd),
        );
        oracle_context.insert("exit_feasibility".to_string(), json!(exit_feasibility));
        oracle_context.insert(
            "breached_threshold_pct".to_string(),
            json!(breached_threshold_pct),
        );
        oracle_context.insert(
            "resolution_threshold_pct".to_string(),
            json!(resolution_threshold_pct),
        );
        oracle_context.insert(
            "medium_threshold_pct".to_string(),
            json!(rule.medium_threshold_pct),
        );
        oracle_context.insert(
            "high_threshold_pct".to_string(),
            json!(rule.high_threshold_pct),
        );
        oracle_context.insert(
            "critical_threshold_pct".to_string(),
            json!(rule.critical_threshold_pct),
        );
        oracle_context.insert("rate_of_change_10min".to_string(), json!(rate_10min));
        oracle_context.insert("rate_of_change_60min".to_string(), json!(rate_60min));
        oracle_context.insert("paused_status".to_string(), json!(state.paused_status));
        oracle_context.insert(
            "transition".to_string(),
            json!(format!("{:?}", context.transition).to_ascii_lowercase()),
        );
        let action = recommended_action_for(context.severity, state.paused_status);
        oracle_context.insert("recommended_action".to_string(), json!(action.as_str()));
        oracle_context.insert(
            "escalation_count".to_string(),
            json!(state.escalation_count),
        );

        let risk_score_val = risk_score_for_severity(context.severity);

        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: PATTERN_ID.to_string(),
            event_key: Some(format!(
                "utilization_high:{}:{}:{}",
                event.tenant_id, rule.rule_id, context.subject_key
            )),
            subject_type: Some(context.subject_type.to_string()),
            subject_key: Some(context.subject_key.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain: chain_from_slug(&sample.chain_slug),
            chain_slug: sample.chain_slug.clone(),
            protocol: sample.protocol_id.clone(),
            lifecycle_state: LifecycleState::Confirmed,
            requires_confirmation: false,
            attack_family: AttackFamily::HighUtilization,
            severity: context.severity.clone(),
            description: Some(format!(
                "High utilization detected: {:.2}% in {}/{} ({}). Rule: '{}'.",
                sample.utilization_pct,
                sample.protocol_id,
                sample.market_id.as_deref().unwrap_or("protocol"),
                sample.chain_slug,
                rule.rule_id,
            )),
            triggered_rule_ids: vec![format!("utilization_high.{}", rule.rule_id)],
            tx_hash: sample
                .tx_hash
                .clone()
                .or_else(|| event.tx_hash.clone())
                .unwrap_or_else(|| format!("utilization-high-{}", Uuid::new_v4())),
            block_number: if sample.block_number > 0 {
                sample.block_number
            } else {
                event.block_number.unwrap_or_default()
            },
            signals: vec![DetectionSignal {
                signal_type: SignalType::UtilizationHighDetected,
                value: sample.utilization_pct,
                label: Some(format!("{:.2}% utilization", sample.utilization_pct)),
                source_id: Some(event.source_id.clone()),
            }],
            risk_score: RiskScore {
                score: risk_score_val,
                confidence: 0.90,
                rationale: vec![
                    format!("utilization={:.2}%", sample.utilization_pct),
                    format!(
                        "rate_10min={:+.2}pp, rate_60min={:+.2}pp",
                        rate_10min, rate_60min
                    ),
                ],
                attribution: Vec::new(),
            },
            incident_transition: Some(context.transition.clone()),
            context_classification: Some(ContextClassification::Isolated),
            confidence_breakdown: HashMap::from([(
                "utilization_pct".to_string(),
                sample.utilization_pct,
            )]),
            oracle_context,
            actions_recommended: actions_for_severity(context.severity, state.paused_status),
            is_simulated,
            simulation_run_id,
            detected_at: context.observed_at,
            created_at: context.observed_at,
        }
    }

    fn build_resolve_detection(
        event: &UnifiedEvent,
        rule: &UtilizationHighRule,
        context: &ResolveDetectionContext<'_>,
        sample: &UtilizationStateEvent,
    ) -> DetectionResult {
        let (is_simulated, simulation_run_id) = simulation_metadata_from_event(event);

        let duration_sec = context
            .incident_active_since
            .map(|start| (context.observed_at - start).num_seconds())
            .unwrap_or(0);
        let available_liquidity_usd =
            (sample.tvl_usd * (1.0 - (sample.utilization_pct / 100.0))).max(0.0);
        let breached_threshold_pct = match context.resolved_severity {
            Severity::Critical => rule.critical_threshold_pct,
            Severity::High => rule.high_threshold_pct,
            Severity::Medium | Severity::Low | Severity::Info => rule.medium_threshold_pct,
        };
        let resolution_threshold_pct =
            rule.resolution_threshold_for(severity_to_str(context.resolved_severity));

        let mut oracle_context = HashMap::new();
        oracle_context.insert("protocol_id".to_string(), json!(sample.protocol_id));
        oracle_context.insert("chain_slug".to_string(), json!(sample.chain_slug));
        oracle_context.insert("market_id".to_string(), json!(sample.market_id));
        oracle_context.insert(
            "current_utilization_pct".to_string(),
            json!(sample.utilization_pct),
        );
        oracle_context.insert(
            "available_liquidity_usd".to_string(),
            json!(available_liquidity_usd),
        );
        oracle_context.insert("exit_feasibility".to_string(), json!("recovering"));
        oracle_context.insert(
            "breached_threshold_pct".to_string(),
            json!(breached_threshold_pct),
        );
        oracle_context.insert(
            "resolution_threshold_pct".to_string(),
            json!(resolution_threshold_pct),
        );
        oracle_context.insert("transition".to_string(), json!("resolve"));
        oracle_context.insert("incident_duration_sec".to_string(), json!(duration_sec));
        oracle_context.insert(
            "escalation_occurred".to_string(),
            json!(context.escalation_count > 0),
        );
        oracle_context.insert(
            "escalation_count".to_string(),
            json!(context.escalation_count),
        );
        oracle_context.insert(
            "original_severity".to_string(),
            json!(severity_to_str(context.resolved_severity)),
        );

        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: PATTERN_ID.to_string(),
            event_key: Some(format!(
                "utilization_high:{}:{}:{}",
                event.tenant_id, rule.rule_id, context.subject_key
            )),
            subject_type: Some(context.subject_type.to_string()),
            subject_key: Some(context.subject_key.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain: chain_from_slug(&sample.chain_slug),
            chain_slug: sample.chain_slug.clone(),
            protocol: sample.protocol_id.clone(),
            lifecycle_state: LifecycleState::Resolved,
            requires_confirmation: false,
            attack_family: AttackFamily::HighUtilization,
            severity: context.resolved_severity.clone(),
            description: Some(format!(
                "High utilization incident resolved: {:.2}% in {}/{} ({}).",
                sample.utilization_pct,
                sample.protocol_id,
                sample.market_id.as_deref().unwrap_or("protocol"),
                sample.chain_slug,
            )),
            triggered_rule_ids: vec![format!("utilization_high.{}", rule.rule_id)],
            tx_hash: sample
                .tx_hash
                .clone()
                .or_else(|| event.tx_hash.clone())
                .unwrap_or_else(|| format!("utilization-high-resolve-{}", Uuid::new_v4())),
            block_number: if sample.block_number > 0 {
                sample.block_number
            } else {
                event.block_number.unwrap_or_default()
            },
            signals: vec![DetectionSignal {
                signal_type: SignalType::UtilizationHighDetected,
                value: sample.utilization_pct,
                label: Some("incident resolved".to_string()),
                source_id: Some(event.source_id.clone()),
            }],
            risk_score: RiskScore {
                score: 10.0,
                confidence: 0.90,
                rationale: vec![format!(
                    "utilization fell to {:.2}% — below resolution threshold",
                    sample.utilization_pct
                )],
                attribution: Vec::new(),
            },
            incident_transition: Some(IncidentTransition::Resolve),
            context_classification: Some(ContextClassification::None),
            confidence_breakdown: HashMap::new(),
            oracle_context,
            actions_recommended: vec![
                "Incident auto-resolved. Liquidity has recovered.".to_string()
            ],
            is_simulated,
            simulation_run_id,
            detected_at: context.observed_at,
            created_at: context.observed_at,
        }
    }

    fn build_pause_detection(
        event: &UnifiedEvent,
        rule: &UtilizationHighRule,
        subject_type: &str,
        subject_key: &str,
        active_severity: &Severity,
        pause: &UtilizationPauseEvent,
        now: DateTime<Utc>,
    ) -> DetectionResult {
        let (is_simulated, simulation_run_id) = simulation_metadata_from_event(event);

        let state_label = if pause.paused { "paused" } else { "unpaused" };
        let mut oracle_context = HashMap::new();
        oracle_context.insert("protocol_id".to_string(), json!(pause.protocol_id));
        oracle_context.insert("chain_slug".to_string(), json!(pause.chain_slug));
        oracle_context.insert("market_id".to_string(), json!(pause.market_id));
        oracle_context.insert("pause_state".to_string(), json!(state_label));

        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: PATTERN_ID.to_string(),
            event_key: Some(format!(
                "utilization_high:{}:{}:{}:pause",
                event.tenant_id, rule.rule_id, subject_key
            )),
            subject_type: Some(subject_type.to_string()),
            subject_key: Some(subject_key.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain: chain_from_slug(&pause.chain_slug),
            chain_slug: pause.chain_slug.clone(),
            protocol: pause.protocol_id.clone(),
            lifecycle_state: LifecycleState::Confirmed,
            requires_confirmation: false,
            attack_family: AttackFamily::HighUtilization,
            severity: active_severity.clone(),
            description: Some(format!(
                "Protocol {} {} while a high-utilization incident is active.",
                pause.protocol_id, state_label
            )),
            triggered_rule_ids: vec![format!("utilization_high.{}", rule.rule_id)],
            tx_hash: pause
                .tx_hash
                .clone()
                .or_else(|| event.tx_hash.clone())
                .unwrap_or_else(|| format!("utilization-high-pause-{}", Uuid::new_v4())),
            block_number: if pause.block_number > 0 {
                pause.block_number
            } else {
                event.block_number.unwrap_or_default()
            },
            signals: vec![DetectionSignal {
                signal_type: SignalType::ProtocolPauseState,
                value: if pause.paused { 1.0 } else { 0.0 },
                label: Some(format!("protocol_{state_label}")),
                source_id: Some(event.source_id.clone()),
            }],
            risk_score: RiskScore {
                score: 70.0,
                confidence: 0.85,
                rationale: vec![format!(
                    "protocol marked {state_label} during active high-utilization incident"
                )],
                attribution: Vec::new(),
            },
            incident_transition: Some(IncidentTransition::Update),
            context_classification: Some(ContextClassification::Isolated),
            confidence_breakdown: HashMap::new(),
            oracle_context,
            actions_recommended: if pause.paused {
                vec![
                    "MONITOR_FOR_UNPAUSE: Withdrawal not currently possible.".to_string(),
                    "Pool is paused by governance. Monitor for recovery.".to_string(),
                ]
            } else {
                vec!["Protocol unpaused. Auto-resolution tracking resumed.".to_string()]
            },
            is_simulated,
            simulation_run_id,
            detected_at: now,
            created_at: now,
        }
    }
}

// ─── Trait implementation ─────────────────────────────────────────────────────

#[async_trait]
impl DetectionPattern for UtilizationHighPattern {
    fn pattern_id(&self) -> &str {
        PATTERN_ID
    }

    async fn reload_config(&mut self, config_map: &HashMap<(String, String), Value>) -> Result<()> {
        let mut next = HashMap::new();
        let mut next_bindings = HashMap::new();
        for ((tenant_id, pattern_id), config) in config_map {
            if pattern_id != PATTERN_ID {
                continue;
            }
            let detection_config = super::extract_detection_config(config);
            let rules = parse_utilization_rules(detection_config, tenant_id);
            next.insert(tenant_id.clone(), rules);
            if let Some(bound) = super::extract_bound_source_ids(config) {
                next_bindings.insert(tenant_id.clone(), bound);
            }
        }
        self.configs = next;
        self.source_bindings = next_bindings;
        tracing::info!(
            tenant_count = self.configs.len(),
            "utilization_high configs reloaded"
        );
        Ok(())
    }

    async fn process_event(
        &mut self,
        event: &UnifiedEvent,
        now: DateTime<Utc>,
        repo: &PostgresRepository,
    ) -> Result<Option<DetectionResult>> {
        // Enforce source bindings: only process events from sources the tenant has bound to
        // this pattern in the Gateway tab.  Mode switching (live ↔ test) is handled at the
        // indexer level — live tenants receive live-profile streams, test tenants receive
        // test-profile streams.  No simulation bypass needed here.
        if let Some(bound) = self.source_bindings.get(&event.tenant_id) {
            if !bound.is_empty() && !bound.contains(&event.source_id) {
                return Ok(None);
            }
        }
        let Some(rules) = self.effective_rules(&event.tenant_id, repo).await? else {
            return Ok(None);
        };

        // ── Branch 1: utilization state update ───────────────────────────────
        if let Some(sample) = parse_utilization_state_event(event) {
            let mut emitted: Option<DetectionResult> = None;

            for rule in rules.iter().filter(|r| {
                r.enabled
                    && r.matches(
                        &sample.protocol_id,
                        &sample.chain_slug,
                        sample.market_id.as_deref(),
                    )
            }) {
                let Some((subject_type, subject_key, _protocol_chain_key)) =
                    rule.subject_for_event(sample.market_id.as_deref())
                else {
                    continue;
                };

                let state_key = Self::state_key(&rule.rule_id, &subject_key);
                let cache_key = Self::cache_key(&event.tenant_id, &state_key);

                // Load persisted state
                let mut state: UtilizationRuleState = repo
                    .load_pattern_state(&event.tenant_id, PATTERN_ID, &state_key)
                    .await?
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();

                // Pure evaluation — all lifecycle logic delegated here
                let eval = evaluate_utilization_state(
                    rule,
                    &mut state,
                    sample.utilization_pct,
                    sample.tvl_usd,
                    now,
                );

                if eval.tvl_floor_suppressed {
                    tracing::debug!(
                        tenant_id = %event.tenant_id,
                        rule_id = %rule.rule_id,
                        tvl_usd = sample.tvl_usd,
                        floor = rule.min_tvl_floor_usd,
                        "utilization event suppressed — market below TVL floor"
                    );
                    continue;
                }

                // Build detection result from evaluation outcome
                if eval.resolved {
                    let resolve_context = ResolveDetectionContext {
                        subject_type: &subject_type,
                        subject_key: &subject_key,
                        resolved_severity: eval.resolved_from_severity.as_ref().unwrap(),
                        observed_at: now,
                        incident_active_since: eval.incident_active_since,
                        escalation_count: eval.escalation_count,
                    };
                    let resolve_detection =
                        Self::build_resolve_detection(event, rule, &resolve_context, &sample);
                    emitted = pick_higher(emitted, resolve_detection);
                } else if let (Some(ref transition), Some(ref severity)) =
                    (&eval.transition, &eval.emit_severity)
                {
                    let detection = Self::build_utilization_detection(
                        event,
                        rule,
                        &UtilizationDetectionContext {
                            subject_type: &subject_type,
                            subject_key: &subject_key,
                            severity,
                            transition,
                            observed_at: now,
                        },
                        &sample,
                        &state,
                    );
                    emitted = pick_higher(emitted, detection);
                }

                // Snapshot
                let snapshot = json!({
                    "rule_id": rule.rule_id,
                    "subject_key": subject_key,
                    "utilization_pct": sample.utilization_pct,
                    "tvl_usd": sample.tvl_usd,
                    "total_supplied_tokens": sample.total_supplied_tokens,
                    "total_borrowed_tokens": sample.total_borrowed_tokens,
                    "last_severity": state.last_severity,
                    "resolution_block_counter": state.resolution_block_counter,
                    "paused_status": state.paused_status,
                    "incident_transition": eval.transition.as_ref().map(|t| format!("{t:?}").to_ascii_lowercase()),
                    "do_resolve": eval.resolved,
                });
                let _ = repo
                    .insert_pattern_snapshot(PatternSnapshotInsert {
                        tenant_id: &event.tenant_id,
                        pattern_id: PATTERN_ID,
                        snapshot_key: &state_key,
                        data: append_snapshot_meta(event, snapshot),
                        score: Some(sample.utilization_pct),
                        severity: state.last_severity.as_deref(),
                        observed_at: event.timestamp,
                    })
                    .await;
                let _ = repo
                    .upsert_pattern_state(
                        &event.tenant_id,
                        PATTERN_ID,
                        &state_key,
                        serde_json::to_value(&state)?,
                    )
                    .await;
                self.state_cache.insert(cache_key, state);
            }

            // Enrich with concurrent_market_alerts: list other active incidents
            // for the same tenant + protocol (Spec Sections 11.3, 12).
            if let Some(ref mut detection) = emitted {
                let tenant_prefix = format!("{}:", event.tenant_id);
                let concurrent: Vec<Value> = self
                    .state_cache
                    .iter()
                    .filter(|(k, s)| {
                        k.starts_with(&tenant_prefix)
                            && s.last_severity.is_some()
                            && Some(k.as_str())
                                != detection.event_key.as_deref().map(|ek| {
                                    // strip "utilization_high:" prefix to match cache key format
                                    ek.strip_prefix("utilization_high:").unwrap_or(ek)
                                })
                    })
                    .map(|(k, s)| {
                        json!({
                            "state_key": k,
                            "severity": s.last_severity,
                            "active_since": s.active_since,
                        })
                    })
                    .collect();
                if !concurrent.is_empty() {
                    detection
                        .oracle_context
                        .insert("concurrent_market_alerts".to_string(), json!(concurrent));
                }
            }

            return Ok(emitted);
        }

        // ── Branch 2: protocol pause/unpause ─────────────────────────────────
        if let Some(pause) = parse_utilization_pause_event(event) {
            for rule in rules.iter().filter(|r| {
                r.enabled
                    && r.matches(
                        &pause.protocol_id,
                        &pause.chain_slug,
                        pause.market_id.as_deref(),
                    )
            }) {
                let Some((subject_type, subject_key, _protocol_chain_key)) =
                    rule.subject_for_event(pause.market_id.as_deref())
                else {
                    continue;
                };

                let state_key = Self::state_key(&rule.rule_id, &subject_key);
                let cache_key = Self::cache_key(&event.tenant_id, &state_key);

                let mut state: UtilizationRuleState = repo
                    .load_pattern_state(&event.tenant_id, PATTERN_ID, &state_key)
                    .await?
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();

                // Only emit a pause signal if there is an active incident
                let Some(active_severity_str) = state.last_severity.clone() else {
                    continue;
                };
                let active_severity =
                    severity_from_str(Some(&active_severity_str)).unwrap_or(Severity::Medium);

                state.paused_status = pause.paused;
                state.last_transition_at = Some(now);

                let snapshot = json!({
                    "rule_id": rule.rule_id,
                    "subject_key": subject_key,
                    "pause_state": if pause.paused { "paused" } else { "unpaused" },
                    "incident_transition": "update",
                });
                let _ = repo
                    .insert_pattern_snapshot(PatternSnapshotInsert {
                        tenant_id: &event.tenant_id,
                        pattern_id: PATTERN_ID,
                        snapshot_key: &state_key,
                        data: append_snapshot_meta(event, snapshot),
                        score: None,
                        severity: Some(&active_severity_str),
                        observed_at: event.timestamp,
                    })
                    .await;
                let _ = repo
                    .upsert_pattern_state(
                        &event.tenant_id,
                        PATTERN_ID,
                        &state_key,
                        serde_json::to_value(&state)?,
                    )
                    .await;
                self.state_cache.insert(cache_key, state);

                let detection = Self::build_pause_detection(
                    event,
                    rule,
                    &subject_type,
                    &subject_key,
                    &active_severity,
                    &pause,
                    now,
                );
                return Ok(Some(detection));
            }
        }

        Ok(None)
    }
}

// ─── Pick the detection with the higher severity ──────────────────────────────

fn severity_rank_detection(d: &DetectionResult) -> u8 {
    severity_rank(Some(&d.severity))
}

fn pick_higher(
    existing: Option<DetectionResult>,
    candidate: DetectionResult,
) -> Option<DetectionResult> {
    match existing {
        None => Some(candidate),
        Some(prev) => {
            if severity_rank_detection(&candidate) >= severity_rank_detection(&prev) {
                Some(candidate)
            } else {
                Some(prev)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_schema::SourceType;
    use serde_json::json;

    // ─── Helpers ──────────────────────────────────────────────────────────

    fn base_rule() -> UtilizationHighRule {
        UtilizationHighRule {
            rule_id: "util-default".to_string(),
            protocol_id: "aave_v3".to_string(),
            chain_slug: "base".to_string(),
            scope: "market".to_string(),
            market_id: Some("usdc".to_string()),
            medium_threshold_pct: 90.0,
            high_threshold_pct: 95.0,
            critical_threshold_pct: 99.0,
            resolution_medium_pct: 85.0,
            resolution_high_pct: 88.0,
            resolution_critical_pct: 90.0,
            resolution_confirmation_blocks: 10,
            min_tvl_floor_usd: 500_000.0,
            enabled: true,
        }
    }

    fn state_with_incident(severity: &str, now: DateTime<Utc>) -> UtilizationRuleState {
        UtilizationRuleState {
            last_severity: Some(severity.to_string()),
            active_since: Some(now - Duration::minutes(5)),
            last_breach_at: Some(now - Duration::seconds(12)),
            last_transition_at: Some(now - Duration::minutes(5)),
            paused_status: false,
            resolution_block_counter: 0,
            samples: vec![],
            escalation_count: 0,
        }
    }

    /// Shorthand: evaluate with TVL well above floor.
    fn eval(
        rule: &UtilizationHighRule,
        state: &mut UtilizationRuleState,
        utilization_pct: f64,
    ) -> UtilizationEvalResult {
        evaluate_utilization_state(rule, state, utilization_pct, 100_000_000.0, Utc::now())
    }

    fn eval_at(
        rule: &UtilizationHighRule,
        state: &mut UtilizationRuleState,
        utilization_pct: f64,
        tvl_usd: f64,
        now: DateTime<Utc>,
    ) -> UtilizationEvalResult {
        evaluate_utilization_state(rule, state, utilization_pct, tvl_usd, now)
    }

    // ═══ Section 2: Gate Checks — Spec Section 8 Step 1 ══════════════════

    /// TC-GATE-01: TVL below minimum floor — skip detection
    #[test]
    fn tc_gate_01_tvl_below_floor_suppresses() {
        let rule = base_rule(); // floor = $500K
        let mut state = UtilizationRuleState::default();
        let result = eval_at(&rule, &mut state, 97.0, 400_000.0, Utc::now());
        assert!(result.tvl_floor_suppressed);
        assert!(result.transition.is_none());
    }

    /// TC-GATE-02: TVL at exactly the floor — proceed with detection
    #[test]
    fn tc_gate_02_tvl_at_floor_proceeds() {
        let rule = base_rule();
        let mut state = UtilizationRuleState::default();
        let result = eval_at(&rule, &mut state, 96.0, 500_000.0, Utc::now());
        assert!(!result.tvl_floor_suppressed);
        assert!(matches!(
            result.transition,
            Some(IncidentTransition::Trigger)
        ));
        assert!(matches!(result.emit_severity, Some(Severity::High)));
    }

    /// TC-GATE-03: Utilization data null — parser returns None, no signal
    #[test]
    fn tc_gate_03_null_utilization_returns_none() {
        let event = UnifiedEvent {
            event_id: "evt-1".into(),
            tenant_id: "t".into(),
            source_id: "s".into(),
            source_type: SourceType::EvmChain,
            event_type: "protocol_state".into(),
            timestamp: Utc::now(),
            payload: json!({
                "protocol_id": "aave_v3",
                "metric": "utilization",
                "total_supplied": 0
                // utilization field omitted → division by zero guard
            }),
            chain_id: Some(8453),
            block_number: Some(1),
            tx_hash: None,
            market_key: None,
            price: None,
        };
        assert!(parse_utilization_state_event(&event).is_none());
    }

    /// TC-GATE-04: tenant_has_deposit defaults to true in MVP (gate is a no-op)
    #[test]
    fn tc_gate_04_tenant_has_deposit_always_true() {
        let rule = base_rule();
        let mut state = UtilizationRuleState::default();
        let result = eval(&rule, &mut state, 96.0);
        // No deposit gate in MVP — detection fires regardless
        assert!(matches!(
            result.transition,
            Some(IncidentTransition::Trigger)
        ));
    }

    // ═══ Section 3: Severity Classification — Spec Section 8 Step 2 ══════

    /// TC-SEV-01: CRITICAL — at or above 99%
    #[test]
    fn tc_sev_01_critical_at_99_5() {
        assert!(matches!(
            classify_severity(99.5, &base_rule()),
            Some(Severity::Critical)
        ));
    }

    /// TC-SEV-02: HIGH — at or above 95%, below 99%
    #[test]
    fn tc_sev_02_high_at_96_2() {
        assert!(matches!(
            classify_severity(96.2, &base_rule()),
            Some(Severity::High)
        ));
    }

    /// TC-SEV-03: MEDIUM — at or above 90%, below 95%
    #[test]
    fn tc_sev_03_medium_at_92() {
        assert!(matches!(
            classify_severity(92.0, &base_rule()),
            Some(Severity::Medium)
        ));
    }

    /// TC-SEV-04: Below medium threshold — no alert
    #[test]
    fn tc_sev_04_below_medium_no_alert() {
        assert!(classify_severity(88.0, &base_rule()).is_none());
    }

    /// TC-SEV-05: Boundary — exactly at medium threshold (>= condition)
    #[test]
    fn tc_sev_05_exactly_medium() {
        assert!(matches!(
            classify_severity(90.0, &base_rule()),
            Some(Severity::Medium)
        ));
    }

    /// TC-SEV-06: Boundary — exactly at high threshold
    #[test]
    fn tc_sev_06_exactly_high() {
        assert!(matches!(
            classify_severity(95.0, &base_rule()),
            Some(Severity::High)
        ));
    }

    /// TC-SEV-07: Boundary — exactly at critical threshold
    #[test]
    fn tc_sev_07_exactly_critical() {
        assert!(matches!(
            classify_severity(99.0, &base_rule()),
            Some(Severity::Critical)
        ));
    }

    /// TC-SEV-08: Boundary — just below medium threshold
    #[test]
    fn tc_sev_08_just_below_medium() {
        assert!(classify_severity(89.9, &base_rule()).is_none());
    }

    /// TC-SEV-09: Rate of change is informational only — does not affect severity
    #[test]
    fn tc_sev_09_rate_of_change_informational() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();
        // Pre-fill sample showing rapid rise
        state.samples.push(UtilizationSample {
            observed_at: now - Duration::minutes(10),
            utilization_pct: 77.5,
        });
        let result = eval_at(&rule, &mut state, 92.0, 100_000_000.0, now);
        // Severity is MEDIUM (92% >= 90%), NOT escalated by rate of change
        assert!(matches!(result.emit_severity, Some(Severity::Medium)));
        // Rate of change is computed from samples (informational only)
        let roc = rate_of_change(&state.samples, now, 10);
        assert!(roc.is_some());
        assert!((roc.unwrap() - 14.5).abs() < 0.01);
    }

    // ═══ Section 4: Deduplication — Spec Section 8 Step 3 ════════════════

    /// TC-DUP-01: Same severity — suppress, no duplicate
    #[test]
    fn tc_dup_01_same_severity_suppressed() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);
        let result = eval_at(&rule, &mut state, 96.0, 100_000_000.0, now);
        assert!(result.transition.is_none());
        assert!(result.emit_severity.is_none());
        assert_eq!(state.last_severity.as_deref(), Some("high"));
    }

    /// TC-DUP-02: Higher severity — escalate existing incident
    #[test]
    fn tc_dup_02_higher_severity_escalates() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("medium", now);
        let result = eval_at(&rule, &mut state, 96.0, 100_000_000.0, now);
        assert!(matches!(
            result.transition,
            Some(IncidentTransition::Escalate)
        ));
        assert!(matches!(result.emit_severity, Some(Severity::High)));
        assert_eq!(state.last_severity.as_deref(), Some("high"));
    }

    /// TC-DUP-03: Lower severity — no de-escalation
    #[test]
    fn tc_dup_03_lower_severity_no_deescalation() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);
        // Utilization at 92% → MEDIUM, but incident stays HIGH
        let result = eval_at(&rule, &mut state, 92.0, 100_000_000.0, now);
        assert!(result.transition.is_none());
        assert_eq!(state.last_severity.as_deref(), Some("high"));
    }

    /// TC-DUP-04: Dedup key includes market_id — different markets are separate
    #[test]
    fn tc_dup_04_different_markets_separate_incidents() {
        let mut rule_usdc = base_rule();
        rule_usdc.market_id = Some("usdc".to_string());

        let mut rule_weth = base_rule();
        rule_weth.rule_id = "util-weth".to_string();
        rule_weth.market_id = Some("weth".to_string());

        let now = Utc::now();
        let mut state_usdc = UtilizationRuleState::default();
        let mut state_weth = UtilizationRuleState::default();

        let r1 = eval_at(&rule_usdc, &mut state_usdc, 96.0, 100_000_000.0, now);
        let r2 = eval_at(&rule_weth, &mut state_weth, 92.0, 100_000_000.0, now);

        assert!(matches!(r1.emit_severity, Some(Severity::High)));
        assert!(matches!(r2.emit_severity, Some(Severity::Medium)));

        // State keys would differ
        let key1 = UtilizationHighPattern::state_key("util-default", "aave_v3:base:usdc");
        let key2 = UtilizationHighPattern::state_key("util-weth", "aave_v3:base:weth");
        assert_ne!(key1, key2);
    }

    // ═══ Section 5: Auto-Resolution — Spec Section 8 Step 4, Section 13 ══

    /// TC-RES-01: CRITICAL resolves below resolution_critical_pct for 10 blocks
    #[test]
    fn tc_res_01_critical_resolves_after_10_blocks() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);

        for i in 0..9 {
            let t = now + Duration::seconds(12 * (i + 1));
            let r = eval_at(&rule, &mut state, 89.0, 100_000_000.0, t);
            assert!(!r.resolved, "should not resolve at block {}", i + 1);
            assert_eq!(state.resolution_block_counter, (i + 1) as u32);
        }
        // 10th block → resolves
        let t = now + Duration::seconds(120);
        let r = eval_at(&rule, &mut state, 89.0, 100_000_000.0, t);
        assert!(r.resolved);
        assert!(matches!(r.transition, Some(IncidentTransition::Resolve)));
        assert!(matches!(r.resolved_from_severity, Some(Severity::Critical)));
        assert!(state.last_severity.is_none());
    }

    /// TC-RES-02: HIGH resolves below resolution_high_pct for 10 blocks
    #[test]
    fn tc_res_02_high_resolves_after_10_blocks() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);

        for i in 0..9 {
            let t = now + Duration::seconds(12 * (i + 1));
            eval_at(&rule, &mut state, 87.0, 100_000_000.0, t);
        }
        let t = now + Duration::seconds(120);
        let r = eval_at(&rule, &mut state, 87.0, 100_000_000.0, t);
        assert!(r.resolved);
        assert!(matches!(r.resolved_from_severity, Some(Severity::High)));
    }

    /// TC-RES-03: MEDIUM resolves below resolution_medium_pct for 10 blocks
    #[test]
    fn tc_res_03_medium_resolves_after_10_blocks() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("medium", now);

        for i in 0..9 {
            let t = now + Duration::seconds(12 * (i + 1));
            eval_at(&rule, &mut state, 84.0, 100_000_000.0, t);
        }
        let t = now + Duration::seconds(120);
        let r = eval_at(&rule, &mut state, 84.0, 100_000_000.0, t);
        assert!(r.resolved);
        assert!(matches!(r.resolved_from_severity, Some(Severity::Medium)));
    }

    /// TC-RES-04: Counter resets when utilization rises back above resolution threshold
    #[test]
    fn tc_res_04_counter_resets_on_bounce() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);

        // 7 blocks below resolution_high_pct (88%)
        for i in 0..7 {
            let t = now + Duration::seconds(12 * (i + 1));
            eval_at(&rule, &mut state, 87.0, 100_000_000.0, t);
        }
        assert_eq!(state.resolution_block_counter, 7);

        // Block 8: utilization rises to 89% (above resolution_high_pct 88%)
        let t = now + Duration::seconds(96);
        let r = eval_at(&rule, &mut state, 89.0, 100_000_000.0, t);
        assert!(!r.resolved);
        assert_eq!(state.resolution_block_counter, 0);
        assert_eq!(state.last_severity.as_deref(), Some("high"));
    }

    /// TC-RES-05: No resolution at exactly the resolution threshold
    /// Condition is `< resolution_threshold`, NOT `<=`
    #[test]
    fn tc_res_05_no_resolution_at_exact_threshold() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);

        // utilization = 88.0% (exactly resolution_high_pct)
        let r = eval_at(&rule, &mut state, 88.0, 100_000_000.0, now);
        assert!(!r.resolved);
        assert_eq!(state.resolution_block_counter, 0); // NOT incremented
    }

    /// TC-RES-06: CRITICAL at 91% — still above resolution threshold
    #[test]
    fn tc_res_06_critical_91_pct_stays_active() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);

        // 91% is above resolution_critical_pct (90%)
        // 91% >= 90% medium threshold → new_severity=Medium → lower → suppress
        let r = eval_at(&rule, &mut state, 91.0, 100_000_000.0, now);
        assert!(!r.resolved);
        assert!(r.transition.is_none()); // no de-escalation
        assert_eq!(state.last_severity.as_deref(), Some("critical"));
    }

    /// TC-RES-07: Protocol pause suspends auto-resolution
    #[test]
    fn tc_res_07_pause_suspends_resolution() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);
        state.paused_status = true;

        for i in 0..20 {
            let t = now + Duration::seconds(12 * (i + 1));
            let r = eval_at(&rule, &mut state, 50.0, 100_000_000.0, t);
            assert!(!r.resolved, "must never resolve while paused");
        }
        assert_eq!(state.resolution_block_counter, 0);
        assert_eq!(state.last_severity.as_deref(), Some("critical"));
    }

    /// TC-RES-08: Auto-resolution resumes after unpause
    #[test]
    fn tc_res_08_resolution_resumes_after_unpause() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);
        state.paused_status = true;

        // 5 blocks while paused — counter stays 0
        for i in 0..5 {
            let t = now + Duration::seconds(12 * (i + 1));
            eval_at(&rule, &mut state, 85.0, 100_000_000.0, t);
        }
        assert_eq!(state.resolution_block_counter, 0);

        // Unpause
        state.paused_status = false;

        // 10 blocks below resolution_critical_pct (90%)
        for i in 0..10 {
            let t = now + Duration::seconds(12 * (i + 6));
            let r = eval_at(&rule, &mut state, 85.0, 100_000_000.0, t);
            if i < 9 {
                assert!(!r.resolved);
            } else {
                assert!(r.resolved);
                assert!(matches!(r.resolved_from_severity, Some(Severity::Critical)));
            }
        }
    }

    /// TC-RES-09: Resolution clears all incident state
    #[test]
    fn tc_res_09_resolution_clears_incident_state() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);
        state.active_since = Some(now - Duration::minutes(30));

        for i in 0..10 {
            let t = now + Duration::seconds(12 * (i + 1));
            eval_at(&rule, &mut state, 87.0, 100_000_000.0, t);
        }

        assert!(state.last_severity.is_none());
        assert!(state.active_since.is_none());
        assert!(state.last_breach_at.is_none());
        assert_eq!(state.resolution_block_counter, 0);
    }

    // ═══ Section 6: Escalation — Spec Section 11 ═════════════════════════

    /// TC-ESC-01: MEDIUM escalates to HIGH
    #[test]
    fn tc_esc_01_medium_to_high() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("medium", now);

        let r = eval_at(&rule, &mut state, 96.0, 100_000_000.0, now);
        assert!(matches!(r.transition, Some(IncidentTransition::Escalate)));
        assert!(matches!(r.emit_severity, Some(Severity::High)));
        assert_eq!(state.last_severity.as_deref(), Some("high"));
        // Resolution threshold now tracks HIGH's threshold
        assert_eq!(rule.resolution_threshold_for("high"), 88.0);
    }

    /// TC-ESC-02: HIGH escalates to CRITICAL
    #[test]
    fn tc_esc_02_high_to_critical() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);

        let r = eval_at(&rule, &mut state, 99.2, 100_000_000.0, now);
        assert!(matches!(r.transition, Some(IncidentTransition::Escalate)));
        assert!(matches!(r.emit_severity, Some(Severity::Critical)));
        assert_eq!(state.last_severity.as_deref(), Some("critical"));
    }

    /// TC-ESC-03: MEDIUM escalates directly to CRITICAL (skip HIGH)
    #[test]
    fn tc_esc_03_medium_to_critical_skip_high() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("medium", now);

        let r = eval_at(&rule, &mut state, 99.5, 100_000_000.0, now);
        assert!(matches!(r.transition, Some(IncidentTransition::Escalate)));
        assert!(matches!(r.emit_severity, Some(Severity::Critical)));
        assert_eq!(state.last_severity.as_deref(), Some("critical"));
    }

    /// TC-ESC-04: No de-escalation — resolves entirely instead
    #[test]
    fn tc_esc_04_no_deescalation_resolves_instead() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);

        // Utilization drops to 87% — below medium threshold
        let r = eval_at(&rule, &mut state, 87.0, 100_000_000.0, now);
        assert!(!r.resolved); // counter = 1, need 10
        assert!(r.transition.is_none());
        assert_eq!(state.last_severity.as_deref(), Some("critical")); // NOT de-escalated

        // 9 more blocks → resolves entirely (no de-escalation to HIGH)
        for i in 1..10 {
            let t = now + Duration::seconds(12 * i);
            eval_at(&rule, &mut state, 87.0, 100_000_000.0, t);
        }
        assert!(state.last_severity.is_none()); // fully resolved
    }

    // ═══ Section 7: Multi-Market and Contagion — Spec Sections 11.3, 12 ══

    /// TC-MULTI-01: Multiple markets create independent incidents
    #[test]
    fn tc_multi_01_independent_per_market() {
        let mut rule_usdc = base_rule();
        rule_usdc.market_id = Some("usdc".to_string());

        let mut rule_eth = base_rule();
        rule_eth.rule_id = "util-eth".to_string();
        rule_eth.market_id = Some("eth".to_string());

        let now = Utc::now();
        let mut s_usdc = UtilizationRuleState::default();
        let mut s_eth = UtilizationRuleState::default();

        let r_usdc = eval_at(&rule_usdc, &mut s_usdc, 96.0, 100_000_000.0, now);
        let r_eth = eval_at(&rule_eth, &mut s_eth, 92.0, 100_000_000.0, now);

        // USDC: HIGH, ETH: MEDIUM — independent
        assert!(matches!(r_usdc.emit_severity, Some(Severity::High)));
        assert!(matches!(r_eth.emit_severity, Some(Severity::Medium)));
    }

    /// TC-MULTI-02: No cross-protocol linking for utilization alerts
    #[test]
    fn tc_multi_02_no_cross_protocol_linking() {
        let rule_aave = base_rule();

        let mut rule_morpho = base_rule();
        rule_morpho.rule_id = "util-morpho".to_string();
        rule_morpho.protocol_id = "morpho_blue".to_string();

        let now = Utc::now();
        let mut s_aave = UtilizationRuleState::default();
        let mut s_morpho = UtilizationRuleState::default();

        let r1 = eval_at(&rule_aave, &mut s_aave, 96.0, 100_000_000.0, now);
        let r2 = eval_at(&rule_morpho, &mut s_morpho, 97.0, 100_000_000.0, now);

        assert!(matches!(r1.emit_severity, Some(Severity::High)));
        assert!(matches!(r2.emit_severity, Some(Severity::High)));
        // No cross-protocol contagion flag
    }

    /// TC-MULTI-03: Utilization incident is independent from TVL Drop
    #[test]
    fn tc_multi_03_independent_from_tvl_drop() {
        // Utilization pattern only produces HIGH_UTILIZATION incidents.
        // TVL Drop is a separate pattern with its own state and dedup key.
        let rule = base_rule();
        let mut state = UtilizationRuleState::default();
        let r = eval(&rule, &mut state, 98.0);
        // 98% → HIGH (>= 95%, < 99%)
        assert!(matches!(r.emit_severity, Some(Severity::High)));
    }

    // ═══ Section 8: Protocol Pause — Spec Section 8 Step 5 ═══════════════

    /// TC-PAUSE-01: Pause suspends auto-resolution
    #[test]
    fn tc_pause_01_pause_suspends_resolution() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);
        state.paused_status = true;

        for i in 0..15 {
            let t = now + Duration::seconds(12 * (i + 1));
            let r = eval_at(&rule, &mut state, 50.0, 100_000_000.0, t);
            assert!(!r.resolved);
        }
        assert_eq!(state.resolution_block_counter, 0);
        assert_eq!(state.last_severity.as_deref(), Some("critical"));
    }

    /// TC-PAUSE-02: Unpause → auto-resolution resumes
    #[test]
    fn tc_pause_02_unpause_resumes_resolution() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);
        state.paused_status = true;

        // While paused — no progress
        eval_at(&rule, &mut state, 85.0, 100_000_000.0, now);
        assert_eq!(state.resolution_block_counter, 0);

        // Unpause
        state.paused_status = false;
        let r = eval_at(
            &rule,
            &mut state,
            85.0,
            100_000_000.0,
            now + Duration::seconds(12),
        );
        assert!(!r.resolved);
        assert_eq!(state.resolution_block_counter, 1); // counter advances
    }

    /// Pause event parser works correctly
    #[test]
    fn tc_pause_parser() {
        let event = UnifiedEvent {
            event_id: "evt-p".into(),
            tenant_id: "t".into(),
            source_id: "s".into(),
            source_type: SourceType::EvmChain,
            event_type: "protocol_pause".into(),
            timestamp: Utc::now(),
            payload: json!({
                "protocol_id": "euler_v2",
                "chain_slug": "base",
                "market_id": "usdc",
                "paused": true,
                "block_number": 1000
            }),
            chain_id: Some(8453),
            block_number: Some(1000),
            tx_hash: None,
            market_key: None,
            price: None,
        };
        let parsed = parse_utilization_pause_event(&event).unwrap();
        assert_eq!(parsed.protocol_id, "euler_v2");
        assert!(parsed.paused);
    }

    // ═══ Section 9: Tenant Isolation — Spec Section 7.2 ══════════════════

    /// TC-TENANT-01: Two tenants, same market, different thresholds
    #[test]
    fn tc_tenant_01_different_thresholds() {
        let mut rule_a = base_rule();
        rule_a.medium_threshold_pct = 85.0;
        rule_a.high_threshold_pct = 92.0;
        rule_a.critical_threshold_pct = 97.0;
        rule_a.resolution_medium_pct = 80.0;
        rule_a.resolution_high_pct = 85.0;
        rule_a.resolution_critical_pct = 88.0;

        let mut rule_b = base_rule();
        rule_b.medium_threshold_pct = 95.0;
        rule_b.high_threshold_pct = 97.0;
        rule_b.critical_threshold_pct = 99.0;
        rule_b.resolution_medium_pct = 90.0;
        rule_b.resolution_high_pct = 92.0;
        rule_b.resolution_critical_pct = 94.0;

        let now = Utc::now();
        let mut state_a = UtilizationRuleState::default();
        let mut state_b = UtilizationRuleState::default();

        let r_a = eval_at(&rule_a, &mut state_a, 90.0, 100_000_000.0, now);
        let r_b = eval_at(&rule_b, &mut state_b, 90.0, 100_000_000.0, now);

        // Tenant A: 90% >= 85% → MEDIUM
        assert!(matches!(r_a.emit_severity, Some(Severity::Medium)));
        // Tenant B: 90% < 95% → no alert
        assert!(r_b.transition.is_none());
    }

    /// TC-TENANT-02: Same utilization data, independent evaluation
    #[test]
    fn tc_tenant_02_independent_evaluation() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state_a = UtilizationRuleState::default();
        let mut state_b = UtilizationRuleState::default();

        // Both tenants trigger independently from the same data
        let r_a = eval_at(&rule, &mut state_a, 96.0, 100_000_000.0, now);
        let r_b = eval_at(&rule, &mut state_b, 96.0, 100_000_000.0, now);

        assert!(matches!(r_a.transition, Some(IncidentTransition::Trigger)));
        assert!(matches!(r_b.transition, Some(IncidentTransition::Trigger)));
        assert_eq!(state_a.last_severity, state_b.last_severity);
    }

    // ═══ Section 10: Recommended Actions — Spec Section 10 ═══════════════

    #[test]
    fn tc_action_critical_not_paused_emergency_withdraw() {
        let actions = actions_for_severity(&Severity::Critical, false);
        assert!(actions[0].contains("EMERGENCY_WITHDRAW"));
    }

    #[test]
    fn tc_action_critical_paused_monitor_for_unpause() {
        let actions = actions_for_severity(&Severity::Critical, true);
        assert!(actions[0].contains("paused") || actions[0].contains("Monitor"));
    }

    #[test]
    fn tc_action_high_withdraw_max() {
        let actions = actions_for_severity(&Severity::High, false);
        assert!(actions[0].contains("WITHDRAW_MAX_AVAILABLE"));
    }

    #[test]
    fn tc_action_medium_monitor() {
        let actions = actions_for_severity(&Severity::Medium, false);
        assert!(actions[0].contains("MONITOR"));
    }

    // ═══ Section 12: Section 15 Scenarios Rewritten in GWT ═══════════════

    /// TC-S15-01: Classic threshold breach and auto-resolution
    #[test]
    fn tc_s15_01_classic_breach_escalation_resolution() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();

        // Step 1: utilization rises to 90% → MEDIUM trigger
        let r1 = eval_at(&rule, &mut state, 90.0, 100_000_000.0, now);
        assert!(matches!(r1.transition, Some(IncidentTransition::Trigger)));
        assert!(matches!(r1.emit_severity, Some(Severity::Medium)));

        // Step 2: utilization rises to 96% → escalate to HIGH
        let r2 = eval_at(
            &rule,
            &mut state,
            96.0,
            100_000_000.0,
            now + Duration::seconds(12),
        );
        assert!(matches!(r2.transition, Some(IncidentTransition::Escalate)));
        assert!(matches!(r2.emit_severity, Some(Severity::High)));

        // Step 3: peaks at 96.2% → suppress (same severity)
        let r3 = eval_at(
            &rule,
            &mut state,
            96.2,
            100_000_000.0,
            now + Duration::seconds(24),
        );
        assert!(r3.transition.is_none());

        // Step 4: drops to 84%, remains for 10 blocks → resolve
        for i in 0..10 {
            let t = now + Duration::seconds(36 + 12 * i);
            let r = eval_at(&rule, &mut state, 84.0, 100_000_000.0, t);
            if i < 9 {
                assert!(!r.resolved);
            } else {
                assert!(r.resolved);
                assert!(matches!(r.resolved_from_severity, Some(Severity::High)));
            }
        }
    }

    /// TC-S15-02: Direct critical breach — no intermediate alerts
    #[test]
    fn tc_s15_02_direct_critical_no_intermediates() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();
        // Pre-fill sample for rate-of-change context
        state.samples.push(UtilizationSample {
            observed_at: now - Duration::minutes(4),
            utilization_pct: 85.0,
        });

        let r = eval_at(&rule, &mut state, 99.5, 100_000_000.0, now);
        assert!(matches!(r.transition, Some(IncidentTransition::Trigger)));
        assert!(matches!(r.emit_severity, Some(Severity::Critical)));
        // No intermediate MEDIUM or HIGH
    }

    /// TC-S15-03: Oscillation near threshold — flap prevention
    #[test]
    fn tc_s15_03_oscillation_flap_prevention() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();

        // Crosses 95% → HIGH trigger
        let r1 = eval_at(&rule, &mut state, 96.0, 100_000_000.0, now);
        assert!(matches!(r1.transition, Some(IncidentTransition::Trigger)));
        assert!(matches!(r1.emit_severity, Some(Severity::High)));

        // Drops to 94% — still above resolution_high_pct (88%)
        let r2 = eval_at(
            &rule,
            &mut state,
            94.0,
            100_000_000.0,
            now + Duration::seconds(12),
        );
        assert!(r2.transition.is_none());
        assert_eq!(state.resolution_block_counter, 0); // counter NOT started

        // Rises back to 96% — no duplicate
        let r3 = eval_at(
            &rule,
            &mut state,
            96.0,
            100_000_000.0,
            now + Duration::seconds(24),
        );
        assert!(r3.transition.is_none()); // dedup suppresses
    }

    /// TC-S15-04: Multi-market same protocol
    #[test]
    fn tc_s15_04_multi_market_same_protocol() {
        let mut rule_usdc = base_rule();
        rule_usdc.market_id = Some("usdc".to_string());

        let mut rule_eth = base_rule();
        rule_eth.rule_id = "util-eth".to_string();
        rule_eth.market_id = Some("eth".to_string());

        let now = Utc::now();
        let mut s_usdc = UtilizationRuleState::default();
        let mut s_eth = UtilizationRuleState::default();

        let r1 = eval_at(&rule_usdc, &mut s_usdc, 96.0, 100_000_000.0, now);
        let r2 = eval_at(&rule_eth, &mut s_eth, 92.0, 100_000_000.0, now);

        assert!(matches!(r1.emit_severity, Some(Severity::High)));
        assert!(matches!(r2.emit_severity, Some(Severity::Medium)));
    }

    /// TC-S15-05: Protocol paused during active incident
    #[test]
    fn tc_s15_05_protocol_pause_lifecycle() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();

        // Trigger CRITICAL
        let r = eval_at(&rule, &mut state, 99.5, 100_000_000.0, now);
        assert!(matches!(r.emit_severity, Some(Severity::Critical)));

        // Pause
        state.paused_status = true;

        // Resolution suspended despite low utilization
        for i in 0..15 {
            let t = now + Duration::seconds(12 * (i + 1));
            let r = eval_at(&rule, &mut state, 50.0, 100_000_000.0, t);
            assert!(!r.resolved);
        }

        // Unpause
        state.paused_status = false;

        // 10 blocks → resolves
        let base_t = now + Duration::seconds(12 * 16);
        for i in 0..10 {
            let t = base_t + Duration::seconds(12 * i);
            let r = eval_at(&rule, &mut state, 85.0, 100_000_000.0, t);
            if i < 9 {
                assert!(!r.resolved);
            } else {
                assert!(r.resolved);
            }
        }
    }

    /// TC-S15-06: Below TVL floor
    #[test]
    fn tc_s15_06_below_tvl_floor() {
        let rule = base_rule();
        let mut state = UtilizationRuleState::default();
        let r = eval_at(&rule, &mut state, 97.0, 400_000.0, Utc::now());
        assert!(r.tvl_floor_suppressed);
        assert!(r.transition.is_none());
    }

    /// TC-S15-07: Two tenants, same market, different thresholds
    #[test]
    fn tc_s15_07_two_tenants_different_thresholds() {
        let mut rule_a = base_rule();
        rule_a.medium_threshold_pct = 85.0;
        rule_a.high_threshold_pct = 92.0;
        rule_a.critical_threshold_pct = 97.0;
        rule_a.resolution_medium_pct = 80.0;
        rule_a.resolution_high_pct = 85.0;
        rule_a.resolution_critical_pct = 88.0;

        let mut rule_b = base_rule();
        rule_b.medium_threshold_pct = 95.0;
        rule_b.high_threshold_pct = 97.0;
        rule_b.critical_threshold_pct = 99.0;
        rule_b.resolution_medium_pct = 90.0;
        rule_b.resolution_high_pct = 92.0;
        rule_b.resolution_critical_pct = 94.0;

        let now = Utc::now();
        let mut state_a = UtilizationRuleState::default();
        let mut state_b = UtilizationRuleState::default();

        let r_a = eval_at(&rule_a, &mut state_a, 90.0, 100_000_000.0, now);
        let r_b = eval_at(&rule_b, &mut state_b, 90.0, 100_000_000.0, now);

        // Tenant A: 90% >= 85% → MEDIUM
        assert!(matches!(r_a.transition, Some(IncidentTransition::Trigger)));
        assert!(matches!(r_a.emit_severity, Some(Severity::Medium)));
        // Tenant B: 90% < 95% → no alert
        assert!(r_b.transition.is_none());
    }

    /// TC-S15-08: Concurrent utilization and TVL drop — independence
    /// 98% utilization → HIGH per Section 8 Step 2 (98% >= 95%, < 99%)
    #[test]
    fn tc_s15_08_utilization_independent_of_tvl_drop() {
        let rule = base_rule();
        let mut state = UtilizationRuleState::default();
        let r = eval(&rule, &mut state, 98.0);
        assert!(matches!(r.emit_severity, Some(Severity::High)));
        // TVL_DROP pattern has separate state/lifecycle
    }

    // ═══ Section 13: No-Alert Scenarios ══════════════════════════════════

    /// TC-NONE-01: Normal utilization — no alert
    #[test]
    fn tc_none_01_normal_utilization() {
        let rule = base_rule();
        let mut state = UtilizationRuleState::default();
        let r = eval(&rule, &mut state, 75.0);
        assert!(r.transition.is_none());
        assert!(r.emit_severity.is_none());
    }

    /// TC-NONE-02: Utilization fluctuating below medium threshold
    #[test]
    fn tc_none_02_fluctuating_below_threshold() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();

        for i in 0..100 {
            let utilization = 85.0 + (i as f64 % 5.0); // 85%–89%
            let t = now + Duration::seconds(12 * i);
            let r = eval_at(&rule, &mut state, utilization, 100_000_000.0, t);
            assert!(
                r.transition.is_none(),
                "should never alert at {:.1}%",
                utilization
            );
        }
    }

    // ═══ Existing / Config Tests (preserved) ═════════════════════════════

    #[test]
    fn parser_supports_rule_list_shape() {
        let rules = parse_utilization_rules(
            &json!({
                "rules": [{
                    "rule_id": "util-market",
                    "protocol_id": "morpho_blue",
                    "chain_slug": "ethereum",
                    "scope": "market",
                    "market_id": "usdc",
                    "medium_threshold_pct": 88,
                    "high_threshold_pct": 93,
                    "critical_threshold_pct": 97,
                    "resolution_medium_pct": 83,
                    "resolution_high_pct": 86,
                    "resolution_critical_pct": 89,
                    "resolution_confirmation_blocks": 6,
                    "min_tvl_floor_usd": 750000,
                    "enabled": true
                }]
            }),
            "tenant-a",
        );

        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        assert_eq!(rule.rule_id, "util-market");
        assert_eq!(rule.protocol_id, "morpho_blue");
        assert_eq!(rule.chain_slug, "ethereum");
        assert_eq!(rule.scope, "market");
        assert_eq!(rule.market_id.as_deref(), Some("usdc"));
        assert_eq!(rule.medium_threshold_pct, 88.0);
        assert_eq!(rule.resolution_confirmation_blocks, 6);
    }

    #[tokio::test]
    async fn reload_config_keeps_tenant_rules_isolated_for_same_subject() {
        let mut pattern = UtilizationHighPattern::default();
        let mut config_map = HashMap::new();

        config_map.insert(
            ("tenant-a".to_string(), PATTERN_ID.to_string()),
            json!({
                "rules": [{
                    "rule_id": "util-a",
                    "protocol_id": "aave_v3",
                    "chain_slug": "base",
                    "scope": "protocol",
                    "medium_threshold_pct": 90,
                    "high_threshold_pct": 95,
                    "critical_threshold_pct": 99,
                    "resolution_medium_pct": 85,
                    "resolution_high_pct": 88,
                    "resolution_critical_pct": 90,
                    "resolution_confirmation_blocks": 10,
                    "min_tvl_floor_usd": 500000,
                    "enabled": true
                }]
            }),
        );
        config_map.insert(
            ("tenant-b".to_string(), PATTERN_ID.to_string()),
            json!({
                "rules": [{
                    "rule_id": "util-b",
                    "protocol_id": "aave_v3",
                    "chain_slug": "base",
                    "scope": "protocol",
                    "medium_threshold_pct": 97,
                    "high_threshold_pct": 98,
                    "critical_threshold_pct": 99.5,
                    "resolution_medium_pct": 92,
                    "resolution_high_pct": 94,
                    "resolution_critical_pct": 95,
                    "resolution_confirmation_blocks": 3,
                    "min_tvl_floor_usd": 1000000,
                    "enabled": true
                }]
            }),
        );

        pattern
            .reload_config(&config_map)
            .await
            .expect("reload config");

        let tenant_a = pattern.configs.get("tenant-a").expect("tenant-a rules");
        let tenant_b = pattern.configs.get("tenant-b").expect("tenant-b rules");

        assert_eq!(tenant_a[0].medium_threshold_pct, 90.0);
        assert_eq!(tenant_b[0].medium_threshold_pct, 97.0);
        assert_eq!(tenant_a[0].resolution_confirmation_blocks, 10);
        assert_eq!(tenant_b[0].resolution_confirmation_blocks, 3);
    }

    #[test]
    fn classify_severity_uses_tenant_specific_thresholds_independently() {
        let tenant_a_rule = base_rule();

        let mut tenant_b_rule = base_rule();
        tenant_b_rule.medium_threshold_pct = 97.0;
        tenant_b_rule.high_threshold_pct = 98.0;
        tenant_b_rule.critical_threshold_pct = 99.5;

        let tenant_a = classify_severity(91.0, &tenant_a_rule);
        let tenant_b = classify_severity(91.0, &tenant_b_rule);

        assert!(matches!(tenant_a, Some(Severity::Medium)));
        assert!(tenant_b.is_none());
    }

    // ═══ Gap: Recommended Action Enum (Spec Section 10) ══════════════════

    #[test]
    fn tc_action_enum_critical_not_paused() {
        assert_eq!(
            recommended_action_for(&Severity::Critical, false),
            RecommendedAction::EmergencyWithdraw
        );
        assert_eq!(
            RecommendedAction::EmergencyWithdraw.as_str(),
            "EMERGENCY_WITHDRAW"
        );
    }

    #[test]
    fn tc_action_enum_critical_paused() {
        assert_eq!(
            recommended_action_for(&Severity::Critical, true),
            RecommendedAction::MonitorForUnpause
        );
        assert_eq!(
            RecommendedAction::MonitorForUnpause.as_str(),
            "MONITOR_FOR_UNPAUSE"
        );
    }

    #[test]
    fn tc_action_enum_high() {
        assert_eq!(
            recommended_action_for(&Severity::High, false),
            RecommendedAction::WithdrawMaxAvailable
        );
    }

    #[test]
    fn tc_action_enum_medium() {
        assert_eq!(
            recommended_action_for(&Severity::Medium, false),
            RecommendedAction::Monitor
        );
    }

    /// Trigger evaluation populates recommended_action
    #[test]
    fn tc_action_trigger_carries_recommended_action() {
        let rule = base_rule();
        let mut state = UtilizationRuleState::default();
        let r = eval(&rule, &mut state, 96.0);
        assert_eq!(
            r.recommended_action,
            Some(RecommendedAction::WithdrawMaxAvailable)
        );
    }

    /// Escalation updates recommended_action
    #[test]
    fn tc_action_escalation_updates_recommended_action() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);
        let r = eval_at(&rule, &mut state, 99.2, 100_000_000.0, now);
        assert!(matches!(r.transition, Some(IncidentTransition::Escalate)));
        assert_eq!(
            r.recommended_action,
            Some(RecommendedAction::EmergencyWithdraw)
        );
    }

    /// Paused incident returns MONITOR_FOR_UNPAUSE
    #[test]
    fn tc_action_paused_incident() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("critical", now);
        state.paused_status = true;
        let r = eval_at(&rule, &mut state, 50.0, 100_000_000.0, now);
        assert_eq!(
            r.recommended_action,
            Some(RecommendedAction::MonitorForUnpause)
        );
    }

    // ═══ Gap: Resolution Notification Fields (TC-RES-09 enriched) ════════

    /// Resolution carries incident_active_since for duration calculation
    #[test]
    fn tc_res_09_resolution_carries_duration_metadata() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = state_with_incident("high", now);
        let trigger_time = now - Duration::minutes(30);
        state.active_since = Some(trigger_time);

        for i in 0..10 {
            let t = now + Duration::seconds(12 * (i + 1));
            let r = eval_at(&rule, &mut state, 87.0, 100_000_000.0, t);
            if i == 9 {
                assert!(r.resolved);
                assert_eq!(r.incident_active_since, Some(trigger_time));
                assert_eq!(r.escalation_count, 0);
            }
        }
    }

    /// Resolution after escalation carries escalation_count > 0
    #[test]
    fn tc_res_09_resolution_with_escalation_history() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();

        // Trigger at MEDIUM
        eval_at(&rule, &mut state, 92.0, 100_000_000.0, now);
        assert_eq!(state.escalation_count, 0);

        // Escalate to HIGH
        let t1 = now + Duration::seconds(12);
        eval_at(&rule, &mut state, 96.0, 100_000_000.0, t1);
        assert_eq!(state.escalation_count, 1);

        // Escalate to CRITICAL
        let t2 = now + Duration::seconds(24);
        eval_at(&rule, &mut state, 99.5, 100_000_000.0, t2);
        assert_eq!(state.escalation_count, 2);

        // Resolve after 10 blocks
        let mut last_result = UtilizationEvalResult::default();
        for i in 0..10 {
            let t = now + Duration::seconds(36 + 12 * i);
            last_result = eval_at(&rule, &mut state, 85.0, 100_000_000.0, t);
        }
        assert!(last_result.resolved);
        assert_eq!(last_result.escalation_count, 2);
        assert!(last_result.incident_active_since.is_some());
    }

    /// After resolution, escalation_count resets to 0
    #[test]
    fn tc_escalation_count_resets_after_resolution() {
        let rule = base_rule();
        let now = Utc::now();
        let mut state = UtilizationRuleState::default();

        // Trigger → Escalate
        eval_at(&rule, &mut state, 92.0, 100_000_000.0, now);
        eval_at(
            &rule,
            &mut state,
            96.0,
            100_000_000.0,
            now + Duration::seconds(12),
        );
        assert_eq!(state.escalation_count, 1);

        // Resolve
        for i in 0..10 {
            let t = now + Duration::seconds(24 + 12 * i);
            eval_at(&rule, &mut state, 84.0, 100_000_000.0, t);
        }
        assert!(state.last_severity.is_none());
        assert_eq!(state.escalation_count, 0); // reset

        // New trigger — escalation_count starts fresh
        let t = now + Duration::seconds(200);
        let r = eval_at(&rule, &mut state, 92.0, 100_000_000.0, t);
        assert!(matches!(r.transition, Some(IncidentTransition::Trigger)));
        assert_eq!(state.escalation_count, 0);
    }
}
