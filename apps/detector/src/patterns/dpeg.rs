//! DPEG (De-Peg) detection pattern.
//!
//! Monitors price-feed market events (`UnifiedEvent` with `market_key` + `price`) and
//! computes a per-tenant, per-market weighted median across all contributing sources.
//! Fires a `DetectionResult` when a sustained depeg breach is detected based on the
//! per-tenant `DpegPolicy` stored in `tenant_pattern_configs`.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use event_schema::{
    AttackFamily, Chain, ContextClassification, DetectionResult, DetectionSignal, IncidentTransition,
    LifecycleState, RiskScore, Severity, SignalType, UnifiedEvent,
};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use state_manager::{PostgresRepository};
use uuid::Uuid;

use super::DetectionPattern;

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
    1
}

impl Default for DpegSourceFilter {
    fn default() -> Self {
        Self {
            cex_whitelist: Vec::new(),
            include_oracles: true,
            include_aggregators: true,
            include_dex: true,
            min_healthy_sources: 1,
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

fn deserialize_source_overrides<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, DpegSourceOverride>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(HashMap::new()),
        Value::Object(map) => serde_json::from_value::<HashMap<String, DpegSourceOverride>>(
            Value::Object(map),
        )
        .map_err(de::Error::custom),
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
        Ok(())
    }

    fn source_enabled(&self, source_id: &str, source_kind: &str) -> bool {
        if !self.source_filter.source_kind_allowed(source_id, source_kind) {
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
        let configured_enabled = self
            .source_overrides
            .values()
            .filter(|v| v.enabled)
            .count();
        if configured_enabled == 0 { self.min_sources } else { configured_enabled }
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
}

// ─── Pattern impl ─────────────────────────────────────────────────────────────

/// Per-tenant, per-market DPEG detection pattern.
#[derive(Default)]
pub struct DpegPattern {
    /// (tenant_id, market_key) → DpegPolicy
    policies: HashMap<(String, String), DpegPolicy>,
    /// (tenant_id, market_key) → latest quote per source_id
    quote_cache: HashMap<(String, String), HashMap<String, QuoteInput>>,
    /// (tenant_id, market_key) → in-memory alert state
    state_cache: HashMap<(String, String), DpegAlertState>,
}

impl DpegPattern {
    fn classify_context(&self, policy: &DpegPolicy, now: DateTime<Utc>) -> ContextClassification {
        if !policy.toggles.contagion_detection {
            return ContextClassification::Isolated;
        }

        let tenant_id = policy.tenant_id.as_str();
        let mut systemic_markets = 0usize;
        for ((candidate_tenant, candidate_market), candidate_policy) in &self.policies {
            if candidate_tenant != tenant_id {
                continue;
            }
            if !candidate_market.to_ascii_uppercase().ends_with("/USD") {
                continue;
            }
            let Some(quotes) = self
                .quote_cache
                .get(&(candidate_tenant.clone(), candidate_market.clone()))
            else {
                continue;
            };
            let quote_values = quotes.values().cloned().collect::<Vec<_>>();
            if let Some(divergence_pct) = market_divergence_pct(candidate_policy, &quote_values, now) {
                if divergence_pct >= policy.systemic_floor_pct {
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

    async fn reload_config(
        &mut self,
        config_map: &HashMap<(String, String), Value>,
    ) -> Result<()> {
        let mut new_policies = HashMap::new();
        for ((tenant_id, pattern_id), config) in config_map {
            if pattern_id != PATTERN_ID {
                continue;
            }
            // The config JSONB is an array of DpegPolicy objects (one per market_key).
            let entries: Vec<DpegPolicy> = match serde_json::from_value(config.clone()) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(
                        tenant_id = %tenant_id,
                        error = ?err,
                        "failed to parse dpeg config — skipping tenant"
                    );
                    continue;
                }
            };
            for mut policy in entries {
                policy.tenant_id = tenant_id.clone();
                if let Err(err) = policy.validate() {
                    tracing::warn!(
                        tenant_id = %tenant_id,
                        market_key = %policy.market_key,
                        error = ?err,
                        "invalid dpeg policy — skipping market"
                    );
                    continue;
                }
                new_policies.insert((tenant_id.clone(), policy.market_key.clone()), policy);
            }
        }
        self.policies = new_policies;
        tracing::info!(policy_count = self.policies.len(), "dpeg policies reloaded");
        Ok(())
    }

    async fn process_event(
        &mut self,
        event: &UnifiedEvent,
        now: DateTime<Utc>,
        repo: &PostgresRepository,
    ) -> Result<Option<DetectionResult>> {
        // Only care about market price feed events.
        let (Some(market_key), Some(price)) = (event.market_key.as_deref(), event.price) else {
            return Ok(None);
        };
        if !(price.is_finite() && price > 0.0) {
            return Ok(None);
        }

        let policy_key = (event.tenant_id.clone(), market_key.to_string());
        let Some(policy) = self.policies.get(&policy_key).cloned() else {
            return Ok(None);
        };

        // Infer source_kind from source_type string.
        let source_kind = infer_source_kind(&format!("{:?}", event.source_type));

        // Update quote cache for this source.
        let market_quotes = self.quote_cache.entry(policy_key.clone()).or_default();
        market_quotes.insert(
            event.source_id.clone(),
            QuoteInput {
                source_id: event.source_id.clone(),
                source_kind,
                price,
                observed_at: event.timestamp,
            },
        );

        // Lazily load state from DB on first encounter.
        if !self.state_cache.contains_key(&policy_key) {
            let loaded = repo
                .load_pattern_state(&policy_key.0, PATTERN_ID, &policy_key.1)
                .await?
                .and_then(|v| serde_json::from_value::<DpegAlertState>(v).ok())
                .unwrap_or_default();
            self.state_cache.insert(policy_key.clone(), loaded);
        }

        let current_state = self.state_cache.get(&policy_key).cloned().unwrap_or_default();
        let quotes: Vec<QuoteInput> = market_quotes.values().cloned().collect();
        let classification = self.classify_context(&policy, now);
        let outcome = evaluate_policy(&policy, &quotes, &current_state, now, classification)?;

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
        });
        let severity_str = outcome
            .snapshot
            .severity
            .as_ref()
            .map(|s| format!("{:?}", s).to_lowercase());
        let _ = repo
            .insert_pattern_snapshot(
                &policy_key.0,
                PATTERN_ID,
                market_key,
                snapshot_data,
                Some(outcome.snapshot.divergence_pct),
                severity_str.as_deref(),
            )
            .await;

        // Persist updated alert state to DB.
        let state_value = serde_json::to_value(&outcome.next_state)?;
        let _ = repo
            .upsert_pattern_state(&policy_key.0, PATTERN_ID, &policy_key.1, state_value)
            .await;
        self.state_cache
            .insert(policy_key.clone(), outcome.next_state);

        // Emit detection if needed.
        if outcome.should_emit_alert {
            if let Some(severity) = outcome.emitted_severity.clone() {
                return Ok(Some(build_detection(
                    &policy,
                    &outcome.snapshot,
                    severity,
                    outcome.transition,
                    now,
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
    let threshold_breach = divergence_pct >= trigger_floor_pct && severity.is_some();
    let breach_active = quorum_met
        && threshold_breach
        && (!policy.toggles.oracle_confirmation || oracle_confirmed);

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
        let sustained = now
            .signed_duration_since(breach_started)
            .num_milliseconds()
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
            if let Some(curr_severity) = severity.clone() {
                should_emit_alert = true;
                transition = Some(IncidentTransition::Escalate);
                emitted_severity = Some(curr_severity.clone());
                next_state.last_alerted_at = Some(now);
                next_state.last_divergence_pct = Some(divergence_pct);
                next_state.last_severity = Some(format!("{:?}", curr_severity).to_lowercase());
                next_state.last_classification =
                    Some(context_classification_str(&classification).to_string());
                next_state.below_severity_blocks = 0;
                if next_state.trigger_floor_pct.is_none() {
                    next_state.trigger_floor_pct = Some(trigger_floor_pct);
                }
            }
        } else if current_rank < previous_rank {
            next_state.below_severity_blocks += 1;
            if next_state.below_severity_blocks >= policy.deescalation_blocks {
                if let Some(curr_severity) = severity.clone() {
                    should_emit_alert = true;
                    transition = Some(IncidentTransition::Deescalate);
                    emitted_severity = Some(curr_severity.clone());
                    next_state.last_alerted_at = Some(now);
                    next_state.last_divergence_pct = Some(divergence_pct);
                    next_state.last_severity = Some(format!("{:?}", curr_severity).to_lowercase());
                    next_state.last_classification =
                        Some(context_classification_str(&classification).to_string());
                    next_state.below_severity_blocks = 0;
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
            if divergence_pct < resolution_floor {
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
        },
        should_emit_alert,
        next_state,
        transition,
        emitted_severity,
    })
}

fn market_divergence_pct(policy: &DpegPolicy, quotes: &[QuoteInput], now: DateTime<Utc>) -> Option<f64> {
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
        if oracle_confirmed { 100.0 } else { 0.0 }
    } else {
        50.0
    };
    let volume_score = 50.0;
    let source_weight: f64 = 0.7;
    let oracle_weight: f64 = if policy.toggles.oracle_confirmation { 0.2 } else { 0.0 };
    let volume_weight: f64 = if policy.toggles.volume_confirmation { 0.1 } else { 0.0 };
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
    policy: &DpegPolicy,
    snapshot: &ConsensusSnapshot,
    severity: Severity,
    transition: Option<IncidentTransition>,
    now: DateTime<Utc>,
) -> DetectionResult {
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
        requires_confirmation: false,
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
        created_at: now,
    }
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
                critical: 0.5,
            }),
            isolated_floor_pct: 0.5,
            systemic_floor_pct: 0.01,
            deescalation_blocks: 5,
            resolution_blocks: 30,
            source_filter: DpegSourceFilter::default(),
            toggles: DpegToggles::default(),
            source_overrides: HashMap::new(),
        }
    }

    fn quote(source_id: &str, source_kind: &str, price: f64, observed_at: DateTime<Utc>) -> QuoteInput {
        QuoteInput {
            source_id: source_id.to_string(),
            source_kind: source_kind.to_string(),
            price,
            observed_at,
        }
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
    fn contagion_toggle_off_forces_isolated_classification() {
        let now = Utc::now();
        let mut pattern = DpegPattern::default();
        let mut policy = base_policy();
        policy.toggles.contagion_detection = false;
        pattern
            .policies
            .insert((policy.tenant_id.clone(), policy.market_key.clone()), policy.clone());

        let classification = pattern.classify_context(&policy, now);
        assert!(matches!(classification, ContextClassification::Isolated));
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
                assert!(matches!(outcome.transition, Some(IncidentTransition::Resolve)));
            }
            state = outcome.next_state;
        }
    }
}
