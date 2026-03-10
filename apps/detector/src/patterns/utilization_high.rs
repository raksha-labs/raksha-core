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
//!                 `resolution_confirmation_blocks` consecutive blocks
//!
//! Protocol-pause events suspend auto-resolution and emit an Update signal.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use event_schema::{
    AttackFamily, Chain, ContextClassification, DetectionResult, DetectionSignal,
    IncidentTransition, LifecycleState, RiskScore, Severity, SignalType, UnifiedEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use state_manager::PostgresRepository;
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
            return Err(anyhow!(
                "medium_threshold_pct must be < high_threshold_pct"
            ));
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
            return Err(anyhow!(
                "resolution_confirmation_blocks must be >= 1"
            ));
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

    fn subject_for_event(
        &self,
        market_id: Option<&str>,
    ) -> Option<(String, String, String)> {
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
struct UtilizationRuleState {
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
    let raw_rules: Vec<Value> = if let Some(arr) = config
        .get("rules")
        .and_then(|v| v.as_array())
    {
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

fn classify_severity(
    utilization_pct: f64,
    rule: &UtilizationHighRule,
) -> Option<Severity> {
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

// ─── Pattern struct ───────────────────────────────────────────────────────────

#[derive(Default)]
pub struct UtilizationHighPattern {
    /// tenant_id → rules
    configs: HashMap<String, Vec<UtilizationHighRule>>,
    /// simulation_run_id → rules
    simulation_configs: HashMap<String, Vec<UtilizationHighRule>>,
    /// `{tenant_id}:{rule_id}:{subject_key}` → state
    state_cache: HashMap<String, UtilizationRuleState>,
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
        simulation_run_id: Option<&str>,
        repo: &PostgresRepository,
    ) -> Result<Option<Vec<UtilizationHighRule>>> {
        let Some(run_id) = simulation_run_id.filter(|v| !v.trim().is_empty()) else {
            return Ok(self.configs.get(tenant_id).cloned());
        };

        if let Some(cached) = self.simulation_configs.get(run_id) {
            return Ok(Some(cached.clone()));
        }

        if let Some(config) = repo
            .load_simulation_run_pattern_config(run_id, PATTERN_ID)
            .await?
        {
            let rules = parse_utilization_rules(&config, tenant_id);
            self.simulation_configs
                .insert(run_id.to_string(), rules.clone());
            return Ok(Some(rules));
        }

        Ok(self.configs.get(tenant_id).cloned())
    }

    // ─── DetectionResult builders ─────────────────────────────────────────────

    fn build_utilization_detection(
        event: &UnifiedEvent,
        rule: &UtilizationHighRule,
        subject_type: &str,
        subject_key: &str,
        severity: &Severity,
        transition: &IncidentTransition,
        sample: &UtilizationStateEvent,
        state: &UtilizationRuleState,
        now: DateTime<Utc>,
    ) -> DetectionResult {
        let (is_simulated, simulation_run_id, simulation_operating_mode) =
            simulation_metadata_from_event(event);

        let rate_10min = rate_of_change(&state.samples, now, 10).unwrap_or(0.0);
        let rate_60min = rate_of_change(&state.samples, now, 60).unwrap_or(0.0);

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
        oracle_context.insert("rate_of_change_10min".to_string(), json!(rate_10min));
        oracle_context.insert("rate_of_change_60min".to_string(), json!(rate_60min));
        oracle_context.insert(
            "paused_status".to_string(),
            json!(state.paused_status),
        );
        oracle_context.insert(
            "transition".to_string(),
            json!(format!("{transition:?}").to_ascii_lowercase()),
        );

        let risk_score_val = risk_score_for_severity(severity);

        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: PATTERN_ID.to_string(),
            event_key: Some(format!(
                "utilization_high:{}:{}:{}",
                event.tenant_id, rule.rule_id, subject_key
            )),
            subject_type: Some(subject_type.to_string()),
            subject_key: Some(subject_key.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain: chain_from_slug(&sample.chain_slug),
            chain_slug: sample.chain_slug.clone(),
            protocol: sample.protocol_id.clone(),
            lifecycle_state: LifecycleState::Confirmed,
            requires_confirmation: false,
            attack_family: AttackFamily::HighUtilization,
            severity: severity.clone(),
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
            incident_transition: Some(transition.clone()),
            context_classification: Some(ContextClassification::Isolated),
            confidence_breakdown: HashMap::from([(
                "utilization_pct".to_string(),
                sample.utilization_pct,
            )]),
            oracle_context,
            actions_recommended: actions_for_severity(severity, state.paused_status),
            is_simulated,
            simulation_run_id,
            simulation_operating_mode,
            created_at: now,
        }
    }

    fn build_resolve_detection(
        event: &UnifiedEvent,
        rule: &UtilizationHighRule,
        subject_type: &str,
        subject_key: &str,
        resolved_severity: &Severity,
        sample: &UtilizationStateEvent,
        now: DateTime<Utc>,
    ) -> DetectionResult {
        let (is_simulated, simulation_run_id, simulation_operating_mode) =
            simulation_metadata_from_event(event);

        let mut oracle_context = HashMap::new();
        oracle_context.insert("protocol_id".to_string(), json!(sample.protocol_id));
        oracle_context.insert("chain_slug".to_string(), json!(sample.chain_slug));
        oracle_context.insert("market_id".to_string(), json!(sample.market_id));
        oracle_context.insert(
            "current_utilization_pct".to_string(),
            json!(sample.utilization_pct),
        );
        oracle_context.insert("transition".to_string(), json!("resolve"));

        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: PATTERN_ID.to_string(),
            event_key: Some(format!(
                "utilization_high:{}:{}:{}",
                event.tenant_id, rule.rule_id, subject_key
            )),
            subject_type: Some(subject_type.to_string()),
            subject_key: Some(subject_key.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain: chain_from_slug(&sample.chain_slug),
            chain_slug: sample.chain_slug.clone(),
            protocol: sample.protocol_id.clone(),
            lifecycle_state: LifecycleState::Resolved,
            requires_confirmation: false,
            attack_family: AttackFamily::HighUtilization,
            severity: resolved_severity.clone(),
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
            simulation_operating_mode,
            created_at: now,
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
        let (is_simulated, simulation_run_id, simulation_operating_mode) =
            simulation_metadata_from_event(event);

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
            simulation_operating_mode,
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

    async fn reload_config(
        &mut self,
        config_map: &HashMap<(String, String), Value>,
    ) -> Result<()> {
        let mut next = HashMap::new();
        for ((tenant_id, pattern_id), config) in config_map {
            if pattern_id != PATTERN_ID {
                continue;
            }
            let rules = parse_utilization_rules(config, tenant_id);
            next.insert(tenant_id.clone(), rules);
        }
        self.configs = next;
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
        let (_, simulation_run_id, _) = simulation_metadata_from_event(event);
        let Some(rules) = self
            .effective_rules(&event.tenant_id, simulation_run_id.as_deref(), repo)
            .await?
        else {
            return Ok(None);
        };

        // ── Branch 1: utilization state update ───────────────────────────────
        if let Some(sample) = parse_utilization_state_event(event) {
            // Gate: TVL floor
            if sample.tvl_usd < DEFAULT_MIN_TVL_FLOOR_USD.min(1.0) {
                // Per-rule floor is checked inside the loop; this is a fast-path for
                // obvious zero-TVL events.
            }

            let mut emitted: Option<DetectionResult> = None;

            for rule in rules.iter().filter(|r| {
                r.enabled
                    && r.matches(
                        &sample.protocol_id,
                        &sample.chain_slug,
                        sample.market_id.as_deref(),
                    )
            }) {
                // Gate: per-rule TVL floor
                if sample.tvl_usd < rule.min_tvl_floor_usd && sample.tvl_usd > 0.0 {
                    tracing::debug!(
                        tenant_id = %event.tenant_id,
                        rule_id = %rule.rule_id,
                        tvl_usd = sample.tvl_usd,
                        floor = rule.min_tvl_floor_usd,
                        "utilization event suppressed — market below TVL floor"
                    );
                    continue;
                }

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

                // Update rolling utilization window (keep last 61 minutes)
                state.samples.push(UtilizationSample {
                    observed_at: now,
                    utilization_pct: sample.utilization_pct,
                });
                state.samples.sort_by_key(|s| s.observed_at);
                let window_cutoff = now - Duration::minutes(61);
                state.samples.retain(|s| s.observed_at >= window_cutoff);

                let previous_severity = severity_from_str(state.last_severity.as_deref());

                // Step 2: classify new severity
                let new_severity = classify_severity(sample.utilization_pct, rule);

                // Determine what transition (if any) to emit
                let mut transition: Option<IncidentTransition> = None;
                let mut do_resolve = false;

                match (&previous_severity, &new_severity) {
                    (None, Some(_sev)) => {
                        // First breach — Trigger
                        transition = Some(IncidentTransition::Trigger);
                        state.active_since = Some(now);
                        state.last_breach_at = Some(now);
                        state.last_severity =
                            Some(severity_to_str(new_severity.as_ref().unwrap()).to_string());
                        state.resolution_block_counter = 0;
                    }
                    (Some(prev), Some(new)) => {
                        let prev_rank = severity_rank(Some(prev));
                        let new_rank = severity_rank(Some(new));
                        if new_rank > prev_rank {
                            // Escalate
                            transition = Some(IncidentTransition::Escalate);
                            state.last_severity = Some(severity_to_str(new).to_string());
                            state.last_breach_at = Some(now);
                            state.resolution_block_counter = 0;
                        } else {
                            // Same or lower severity while incident is active → suppress
                            state.last_breach_at = Some(now);
                            state.resolution_block_counter = 0;
                        }
                    }
                    (Some(prev_sev_str), None) => {
                        // Below detection threshold — check resolution path
                        let resolution_threshold =
                            rule.resolution_threshold_for(state.last_severity.as_deref().unwrap_or("medium"));

                        if sample.utilization_pct < resolution_threshold
                            && !state.paused_status
                        {
                            state.resolution_block_counter += 1;
                            if state.resolution_block_counter
                                >= rule.resolution_confirmation_blocks
                            {
                                // Emit resolution
                                do_resolve = true;
                                let resolved_severity_enum = prev_sev_str.clone();
                                let resolve_detection = Self::build_resolve_detection(
                                    event,
                                    rule,
                                    &subject_type,
                                    &subject_key,
                                    &resolved_severity_enum,
                                    &sample,
                                    now,
                                );
                                state.last_severity = None;
                                state.active_since = None;
                                state.last_breach_at = None;
                                state.last_transition_at = Some(now);
                                state.resolution_block_counter = 0;
                                emitted = pick_higher(emitted, resolve_detection);
                            }
                            // else: counter incrementing — keep monitoring
                        } else if sample.utilization_pct >= resolution_threshold {
                            // Still above resolution threshold — reset counter
                            state.resolution_block_counter = 0;
                        }
                        // paused_status == true: don't advance the counter
                    }
                    (None, None) => {
                        // No incident, no breach — nothing to do
                    }
                }

                if let Some(ref t) = transition {
                    state.last_transition_at = Some(now);
                    let sev_ref = new_severity.as_ref().unwrap();
                    let detection = Self::build_utilization_detection(
                        event,
                        rule,
                        &subject_type,
                        &subject_key,
                        sev_ref,
                        t,
                        &sample,
                        &state,
                        now,
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
                    "incident_transition": transition.as_ref().map(|t| format!("{t:?}").to_ascii_lowercase()),
                    "do_resolve": do_resolve,
                });
                let _ = repo
                    .insert_pattern_snapshot(
                        &event.tenant_id,
                        PATTERN_ID,
                        &state_key,
                        append_snapshot_meta(event, snapshot),
                        Some(sample.utilization_pct),
                        state.last_severity.as_deref(),
                        event.timestamp,
                    )
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
                    .insert_pattern_snapshot(
                        &event.tenant_id,
                        PATTERN_ID,
                        &state_key,
                        append_snapshot_meta(event, snapshot),
                        None,
                        Some(&active_severity_str),
                        event.timestamp,
                    )
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
