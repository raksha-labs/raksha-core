//! TVL (Total Value Locked) drop detection pattern.
//!
//! Processes protocol TVL state samples (`UnifiedEvent` with `event_type` like
//! `protocol_state`) and optional pause/unpause signals (`protocol_pause`), then
//! emits incident transitions for sustained or abrupt TVL drawdowns.

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

pub const PATTERN_ID: &str = "tvl_drop";
const TVL_DROP_STATE_CACHE_TTL_HOURS: i64 = 24;
const TVL_DROP_STATE_CACHE_MAX_ENTRIES: usize = 4_096;

const DEFAULT_FAST_DROP_PCT: f64 = 20.0;
const DEFAULT_FAST_WINDOW_MINUTES: i64 = 10;
const DEFAULT_SLOW_DROP_PCT: f64 = 35.0;
const DEFAULT_SLOW_WINDOW_MINUTES: i64 = 60;
const DEFAULT_VELOCITY_CRITICAL_PCT: f64 = 15.0;
const DEFAULT_VELOCITY_WINDOW_MINUTES: i64 = 2;
const DEFAULT_CONCURRENT_WINDOW_MINUTES: i64 = 5;
const DEFAULT_MIN_TVL_FLOOR_USD: f64 = 1_000_000.0;
const FAST_CRITICAL_WINDOW_MINUTES: f64 = 5.0;
const CLIFF_WINDOW_MINUTES: f64 = 0.1;
const LINEAR_WINDOW_UPPER_BOUND_MINUTES: f64 = 8.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvlDropRule {
    pub rule_id: String,
    pub protocol_id: String,
    pub chain_slug: String,
    pub scope: String, // "protocol" | "market"
    pub market_id: Option<String>,
    pub fast_drop_pct: f64,
    pub fast_window_minutes: i64,
    pub slow_drop_pct: f64,
    pub slow_window_minutes: i64,
    pub velocity_critical_pct: f64,
    pub velocity_critical_minutes: i64,
    pub concurrent_window_minutes: i64,
    pub min_tvl_floor_usd: f64,
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub contagion_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl TvlDropRule {
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
        if self.fast_drop_pct <= 0.0
            || self.slow_drop_pct <= 0.0
            || self.velocity_critical_pct <= 0.0
        {
            return Err(anyhow!("drop thresholds must be > 0"));
        }
        if self.fast_window_minutes <= 0
            || self.slow_window_minutes <= 0
            || self.velocity_critical_minutes <= 0
            || self.concurrent_window_minutes <= 0
        {
            return Err(anyhow!("window minutes must be > 0"));
        }
        if self.min_tvl_floor_usd < 0.0 {
            return Err(anyhow!("min_tvl_floor_usd must be >= 0"));
        }
        if self.scope == "market" && self.market_id.as_deref().unwrap_or("").trim().is_empty() {
            return Err(anyhow!("market scope requires market_id"));
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
                .filter(|value| !value.is_empty())?
                .to_ascii_lowercase();
            let subject_key = format!("{protocol_key}:{market}");
            return Some(("market".to_string(), subject_key, protocol_key));
        }
        Some(("protocol".to_string(), protocol_key.clone(), protocol_key))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TvlSamplePoint {
    observed_at: DateTime<Utc>,
    tvl_usd: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TvlRuleState {
    samples: Vec<TvlSamplePoint>,
    last_severity: Option<String>,
    last_drop_pct: Option<f64>,
    active_since: Option<DateTime<Utc>>,
    last_breach_at: Option<DateTime<Utc>>,
    last_transition_at: Option<DateTime<Utc>>,
    last_context: Option<String>,
    last_pause_state: Option<bool>,
    protocol_chain_key: Option<String>,
    /// Whether the tenant has deposits on this protocol.
    /// `None` or stale → defaults to `true` (safe default: proceed with detection).
    tenant_has_deposit: Option<bool>,
    /// Whether the position data backing `tenant_has_deposit` is stale.
    position_data_stale: Option<bool>,
}

#[derive(Debug, Clone)]
struct TvlStateEvent {
    protocol_id: String,
    chain_slug: String,
    market_id: Option<String>,
    tvl_usd: f64,
    block_number: i64,
    tx_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct TvlPauseEvent {
    protocol_id: String,
    chain_slug: String,
    market_id: Option<String>,
    paused: bool,
    block_number: i64,
    tx_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct TvlEvaluation {
    fast_drop_pct: f64,
    slow_drop_pct: f64,
    velocity_drop_pct: f64,
    selected_drop_pct: f64,
    fast_window_reference_tvl_usd: Option<f64>,
    time_to_reach_current_drop_minutes: Option<f64>,
    drain_rate_usd_per_min: Option<f64>,
    estimated_time_to_empty_minutes: Option<f64>,
    velocity_pattern: Option<String>,
    base_severity: Option<Severity>,
    breached_branches: Vec<String>,
    /// `true` when detection was skipped because tenant has no deposit.
    deposit_gate_skipped: bool,
    /// `true` when position data was stale (defaulted to alerting).
    position_data_stale: bool,
}

#[derive(Debug, Clone)]
struct DetectionSubject<'a> {
    subject_type: &'a str,
    subject_key: &'a str,
}

#[derive(Debug, Clone)]
struct TvlDetectionContext<'a> {
    subject: DetectionSubject<'a>,
    severity: Severity,
    transition: IncidentTransition,
    classification: ContextClassification,
    now: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct PauseDetectionContext<'a> {
    subject: DetectionSubject<'a>,
    severity: Severity,
    transition: IncidentTransition,
    classification: ContextClassification,
    now: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
struct WindowAnalysis {
    reference_tvl_usd: Option<f64>,
    current_tvl_usd: Option<f64>,
    drop_pct: f64,
    time_to_reach_current_drop_minutes: Option<f64>,
}

#[derive(Default)]
pub struct TvlDropPattern {
    // tenant_id -> rules
    configs: HashMap<String, Vec<TvlDropRule>>,
    // `${tenant_id}:${rule_id}:${subject_key}` -> state
    state_cache: HashMap<String, TvlRuleState>,
    // tenant_id -> set of enabled source_ids (None = unrestricted)
    source_bindings: HashMap<String, HashSet<String>>,
}

impl TvlDropPattern {
    fn load_state_key(rule_id: &str, subject_key: &str) -> String {
        format!("{rule_id}:{subject_key}")
    }

    fn cache_key(tenant_id: &str, load_state_key: &str) -> String {
        format!("{tenant_id}:{load_state_key}")
    }

    async fn effective_rules(
        &mut self,
        tenant_id: &str,
        _repo: &PostgresRepository,
    ) -> Result<Option<Vec<TvlDropRule>>> {
        Ok(self.configs.get(tenant_id).cloned())
    }

    fn prune_state_cache(&mut self, now: DateTime<Utc>, current_cache_key: &str) {
        let cutoff = now - Duration::hours(TVL_DROP_STATE_CACHE_TTL_HOURS);
        self.state_cache.retain(|cache_key, state| {
            if cache_key == current_cache_key {
                return true;
            }
            tvl_state_last_activity(state)
                .map(|observed_at| observed_at >= cutoff)
                .unwrap_or(false)
        });

        if self.state_cache.len() <= TVL_DROP_STATE_CACHE_MAX_ENTRIES {
            return;
        }

        let mut oldest_keys = self
            .state_cache
            .iter()
            .filter(|(cache_key, _)| cache_key.as_str() != current_cache_key)
            .map(|(cache_key, state)| {
                (
                    cache_key.clone(),
                    tvl_state_last_activity(state).unwrap_or(DateTime::<Utc>::MIN_UTC),
                )
            })
            .collect::<Vec<_>>();
        oldest_keys.sort_by_key(|(_, observed_at)| *observed_at);

        let remove_count = self
            .state_cache
            .len()
            .saturating_sub(TVL_DROP_STATE_CACHE_MAX_ENTRIES);
        for (cache_key, _) in oldest_keys.into_iter().take(remove_count) {
            self.state_cache.remove(&cache_key);
        }
    }

    /// Return other monitored market subject-keys on the same protocol:chain
    /// for the given tenant.  Used for same-protocol contagion flagging
    /// (Spec Section 6 Step 1: "exploit may be in core contract, all markets
    /// at risk").
    fn find_at_risk_markets(
        &self,
        tenant_id: &str,
        protocol_id: &str,
        chain_slug: &str,
        current_market_id: Option<&str>,
    ) -> Vec<String> {
        let Some(rules) = self.configs.get(tenant_id) else {
            return Vec::new();
        };
        rules
            .iter()
            .filter(|r| {
                r.enabled
                    && r.protocol_id.eq_ignore_ascii_case(protocol_id)
                    && r.chain_slug.eq_ignore_ascii_case(chain_slug)
                    && r.normalized_scope() == "market"
            })
            .filter_map(|r| {
                let market = r.market_id.as_deref()?;
                if current_market_id.is_none_or(|curr| !market.eq_ignore_ascii_case(curr)) {
                    Some(format!(
                        "{}:{}:{}",
                        r.protocol_id.to_ascii_lowercase(),
                        r.chain_slug.to_ascii_lowercase(),
                        market.to_ascii_lowercase()
                    ))
                } else {
                    None
                }
            })
            .collect()
    }

    fn has_concurrent_active_drop(
        &self,
        tenant_id: &str,
        current_cache_key: &str,
        current_protocol_chain_key: &str,
        now: DateTime<Utc>,
        window_minutes: i64,
    ) -> bool {
        let cutoff = now - Duration::minutes(window_minutes.max(1));
        let mut protocols = HashSet::new();
        protocols.insert(current_protocol_chain_key.to_string());

        for (cache_key, state) in &self.state_cache {
            if cache_key == current_cache_key {
                continue;
            }
            if !cache_key.starts_with(&format!("{tenant_id}:")) {
                continue;
            }
            if state.last_severity.is_none() {
                continue;
            }
            let Some(last_breach_at) = state.last_breach_at else {
                continue;
            };
            if last_breach_at < cutoff {
                continue;
            }
            let Some(protocol_chain_key) = state.protocol_chain_key.as_deref() else {
                continue;
            };
            protocols.insert(protocol_chain_key.to_string());
            if protocols.len() >= 2 {
                return true;
            }
        }

        false
    }

    fn process_state_sample(
        &self,
        rule: &TvlDropRule,
        state: &mut TvlRuleState,
        sample: &TvlStateEvent,
        now: DateTime<Utc>,
    ) -> TvlEvaluation {
        state.samples.push(TvlSamplePoint {
            observed_at: now,
            tvl_usd: sample.tvl_usd,
        });
        state.samples.sort_by_key(|point| point.observed_at);

        let max_window = *[
            rule.fast_window_minutes,
            rule.slow_window_minutes,
            rule.velocity_critical_minutes,
            rule.concurrent_window_minutes,
        ]
        .iter()
        .max()
        .unwrap_or(&rule.slow_window_minutes);
        let cutoff = now - Duration::minutes(max_window.max(1));
        state.samples.retain(|point| point.observed_at >= cutoff);

        let fast_window = analyze_window(&state.samples, now, rule.fast_window_minutes);
        let slow_window = analyze_window(&state.samples, now, rule.slow_window_minutes);
        let fast_drop = fast_window.drop_pct;
        let slow_drop = slow_window.drop_pct;
        let velocity_drop = max_one_minute_drop_pct(&state.samples, now, rule.fast_window_minutes);
        let selected_drop_pct = fast_drop.max(slow_drop);
        let time_to_reach = fast_window.time_to_reach_current_drop_minutes;
        let velocity_pattern =
            classify_velocity_pattern(fast_drop, velocity_drop, time_to_reach).map(str::to_string);
        let drain_rate_usd_per_min = match (
            fast_window.reference_tvl_usd,
            fast_window.current_tvl_usd,
            time_to_reach,
        ) {
            (Some(reference), Some(current), Some(minutes))
                if reference > current && minutes.is_finite() && minutes > 0.0 =>
            {
                Some((reference - current) / minutes)
            }
            _ => None,
        };
        let estimated_time_to_empty_minutes =
            match (fast_window.current_tvl_usd, drain_rate_usd_per_min) {
                (Some(current), Some(rate)) if current > 0.0 && rate > 0.0 => Some(current / rate),
                _ => None,
            };

        // Gate 1: Tenant deposit check (Spec Section 5 Step 1, Section 4).
        // If position data is stale or unavailable, default to true (proceed).
        let has_deposit = match (state.tenant_has_deposit, state.position_data_stale) {
            (Some(false), Some(true)) => true, // stale data → safe default
            (Some(false), _) => false,         // confirmed no deposit → skip
            _ => true,                         // unknown / true → proceed
        };
        let deposit_gate_skipped = !has_deposit;
        let position_data_stale = state.position_data_stale.unwrap_or(false);

        let mut breached_branches = Vec::new();
        let mut base_severity = None;
        // Gate 2: TVL floor check (Spec Section 5 Step 1, Section 11).
        if has_deposit
            && fast_window.reference_tvl_usd.unwrap_or(sample.tvl_usd) >= rule.min_tvl_floor_usd
        {
            if fast_drop >= rule.velocity_critical_pct
                && time_to_reach
                    .map(|minutes| minutes < rule.velocity_critical_minutes as f64)
                    .unwrap_or(false)
            {
                base_severity = Some(Severity::Critical);
                breached_branches.push("velocity".to_string());
            } else if fast_drop >= rule.fast_drop_pct {
                breached_branches.push("fast".to_string());
                if time_to_reach
                    .map(|minutes| minutes < FAST_CRITICAL_WINDOW_MINUTES)
                    .unwrap_or(false)
                {
                    base_severity = Some(Severity::Critical);
                } else {
                    base_severity = Some(Severity::High);
                }
            } else if slow_drop >= rule.slow_drop_pct {
                breached_branches.push("slow".to_string());
                base_severity = Some(Severity::High);
            }
        }

        TvlEvaluation {
            fast_drop_pct: fast_drop,
            slow_drop_pct: slow_drop,
            velocity_drop_pct: velocity_drop,
            selected_drop_pct,
            fast_window_reference_tvl_usd: fast_window.reference_tvl_usd,
            time_to_reach_current_drop_minutes: time_to_reach,
            drain_rate_usd_per_min,
            estimated_time_to_empty_minutes,
            velocity_pattern,
            base_severity,
            breached_branches,
            deposit_gate_skipped,
            position_data_stale,
        }
    }

    fn build_tvl_detection(
        event: &UnifiedEvent,
        rule: &TvlDropRule,
        context: &TvlDetectionContext<'_>,
        evaluation: &TvlEvaluation,
        sample: &TvlStateEvent,
    ) -> DetectionResult {
        let (is_simulated, simulation_run_id) = simulation_metadata_from_event(event);
        let confidence_breakdown = HashMap::from([
            ("fast_drop_pct".to_string(), evaluation.fast_drop_pct),
            ("slow_drop_pct".to_string(), evaluation.slow_drop_pct),
            (
                "velocity_drop_pct".to_string(),
                evaluation.velocity_drop_pct,
            ),
        ]);
        let mut oracle_context = HashMap::new();
        oracle_context.insert("protocol_id".to_string(), json!(sample.protocol_id));
        oracle_context.insert("chain_slug".to_string(), json!(sample.chain_slug));
        oracle_context.insert("market_id".to_string(), json!(sample.market_id));
        oracle_context.insert("tvl_usd".to_string(), json!(sample.tvl_usd));
        oracle_context.insert("current_tvl_usd".to_string(), json!(sample.tvl_usd));
        oracle_context.insert(
            "fast_window_reference_tvl_usd".to_string(),
            json!(evaluation.fast_window_reference_tvl_usd),
        );
        oracle_context.insert(
            "window_start_tvl_usd".to_string(),
            json!(evaluation.fast_window_reference_tvl_usd),
        );
        oracle_context.insert("drop_pct".to_string(), json!(evaluation.selected_drop_pct));
        oracle_context.insert(
            "time_to_reach_current_drop_minutes".to_string(),
            json!(evaluation.time_to_reach_current_drop_minutes),
        );
        oracle_context.insert(
            "drain_rate_usd_per_min".to_string(),
            json!(evaluation.drain_rate_usd_per_min),
        );
        oracle_context.insert(
            "estimated_time_to_empty_minutes".to_string(),
            json!(evaluation.estimated_time_to_empty_minutes),
        );
        oracle_context.insert(
            "velocity_pattern".to_string(),
            json!(evaluation.velocity_pattern),
        );
        oracle_context.insert(
            "velocity_classification".to_string(),
            json!(evaluation.velocity_pattern),
        );
        oracle_context.insert(
            "breached_branches".to_string(),
            json!(evaluation.breached_branches),
        );
        let breached_threshold_branch = evaluation
            .breached_branches
            .first()
            .map(String::as_str)
            .unwrap_or("fast");
        let breached_threshold_pct = match breached_threshold_branch {
            "velocity" => rule.velocity_critical_pct,
            "slow" => rule.slow_drop_pct,
            _ => rule.fast_drop_pct,
        };
        oracle_context.insert(
            "breached_threshold_branch".to_string(),
            json!(breached_threshold_branch),
        );
        oracle_context.insert(
            "breached_threshold_pct".to_string(),
            json!(breached_threshold_pct),
        );
        oracle_context.insert(
            "transition".to_string(),
            json!(incident_transition_str(&context.transition)),
        );
        oracle_context.insert(
            "contagion_status".to_string(),
            json!(match context.classification {
                ContextClassification::Systemic => "CONCURRENT_DROPS",
                ContextClassification::Isolated | ContextClassification::None => "NONE",
            }),
        );
        if evaluation.position_data_stale {
            oracle_context.insert("position_data_stale".to_string(), json!(true));
        }

        let risk_score = match context.severity {
            Severity::Critical => 95.0,
            Severity::High => 80.0,
            Severity::Medium => 60.0,
            Severity::Low => 30.0,
            Severity::Info => 10.0,
        };

        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: PATTERN_ID.to_string(),
            event_key: Some(format!(
                "tvl_drop:{}:{}:{}",
                event.tenant_id, rule.rule_id, context.subject.subject_key
            )),
            subject_type: Some(context.subject.subject_type.to_string()),
            subject_key: Some(context.subject.subject_key.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain: chain_from_slug(&sample.chain_slug),
            chain_slug: sample.chain_slug.clone(),
            protocol: sample.protocol_id.clone(),
            lifecycle_state: LifecycleState::Confirmed,
            requires_confirmation: false,
            attack_family: AttackFamily::LiquidationCascade,
            severity: context.severity.clone(),
            description: Some(format!(
                "TVL drop rule '{}' triggered (fast={:.2}%, slow={:.2}%, velocity={:.2}%, current_tvl=${:.2}).",
                rule.rule_id,
                evaluation.fast_drop_pct,
                evaluation.slow_drop_pct,
                evaluation.velocity_drop_pct,
                sample.tvl_usd
            )),
            triggered_rule_ids: vec![format!("tvl_drop.{}", rule.rule_id)],
            tx_hash: sample
                .tx_hash
                .clone()
                .or_else(|| event.tx_hash.clone())
                .unwrap_or_else(|| format!("tvl-drop-{}", Uuid::new_v4())),
            block_number: if sample.block_number > 0 {
                sample.block_number
            } else {
                event.block_number.unwrap_or_default()
            },
            signals: vec![DetectionSignal {
                signal_type: if evaluation
                    .breached_branches
                    .iter()
                    .any(|branch| branch == "velocity")
                {
                    SignalType::TvlVelocityDrop
                } else {
                    SignalType::TvlDropDetected
                },
                value: evaluation.selected_drop_pct,
                label: Some(format!("{:.2}% drop", evaluation.selected_drop_pct)),
                source_id: Some(event.source_id.clone()),
            }],
            risk_score: RiskScore {
                score: risk_score,
                confidence: 0.78,
                rationale: vec![
                    format!(
                        "fast={:.2}, slow={:.2}, velocity={:.2}",
                        evaluation.fast_drop_pct,
                        evaluation.slow_drop_pct,
                        evaluation.velocity_drop_pct
                    ),
                    format!(
                        "classification={}",
                        context_classification_str(&context.classification)
                    ),
                ],
                attribution: Vec::new(),
            },
            incident_transition: Some(context.transition.clone()),
            context_classification: Some(context.classification.clone()),
            confidence_breakdown,
            oracle_context,
            actions_recommended: recommended_actions_for_severity(&context.severity),
            is_simulated,
            simulation_run_id,
            detected_at: context.now,
            created_at: context.now,
        }
    }

    fn build_pause_detection(
        event: &UnifiedEvent,
        rule: &TvlDropRule,
        context: &PauseDetectionContext<'_>,
        pause: &TvlPauseEvent,
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
                "tvl_drop:{}:{}:{}:pause",
                event.tenant_id, rule.rule_id, context.subject.subject_key
            )),
            subject_type: Some(context.subject.subject_type.to_string()),
            subject_key: Some(context.subject.subject_key.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain: chain_from_slug(&pause.chain_slug),
            chain_slug: pause.chain_slug.clone(),
            protocol: pause.protocol_id.clone(),
            lifecycle_state: LifecycleState::Confirmed,
            requires_confirmation: false,
            attack_family: AttackFamily::LiquidationCascade,
            severity: context.severity.clone(),
            description: Some(
                if matches!(context.transition, IncidentTransition::Trigger) {
                    format!(
                        "Protocol {} has been {} — PROTOCOL_PAUSED alert initiated.",
                        pause.protocol_id, state_label
                    )
                } else {
                    format!(
                        "Protocol {} is {} while a TVL-drop incident remains active.",
                        pause.protocol_id, state_label
                    )
                },
            ),
            triggered_rule_ids: vec![format!("tvl_drop.{}", rule.rule_id)],
            tx_hash: pause
                .tx_hash
                .clone()
                .or_else(|| event.tx_hash.clone())
                .unwrap_or_else(|| format!("tvl-drop-pause-{}", Uuid::new_v4())),
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
                confidence: 0.7,
                rationale: vec![format!(
                    "protocol marked {state_label} during active incident"
                )],
                attribution: Vec::new(),
            },
            incident_transition: Some(context.transition.clone()),
            context_classification: Some(context.classification.clone()),
            confidence_breakdown: HashMap::new(),
            oracle_context,
            actions_recommended: if matches!(context.transition, IncidentTransition::Trigger) {
                vec![
                    "Monitor protocol for unpause event.".to_string(),
                    "Review protocol status with protocol operators.".to_string(),
                ]
            } else {
                vec![
                    "Confirm protocol control-plane status with protocol operators.".to_string(),
                    "Keep incident open until manual resolution criteria are met.".to_string(),
                ]
            },
            is_simulated,
            simulation_run_id,
            detected_at: context.now,
            created_at: context.now,
        }
    }
}

#[async_trait]
impl DetectionPattern for TvlDropPattern {
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
            let rules = parse_tvl_drop_rules(detection_config, tenant_id);
            next.insert(tenant_id.clone(), rules);
            if let Some(bound) = super::extract_bound_source_ids(config) {
                next_bindings.insert(tenant_id.clone(), bound);
            }
        }
        self.configs = next;
        self.source_bindings = next_bindings;
        tracing::info!(
            tenant_count = self.configs.len(),
            "tvl_drop configs reloaded"
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

        let mut emitted: Option<DetectionResult> = None;
        if let Some(sample) = parse_tvl_state_event(event) {
            for rule in rules.iter().filter(|rule| {
                rule.enabled
                    && rule.matches(
                        &sample.protocol_id,
                        &sample.chain_slug,
                        sample.market_id.as_deref(),
                    )
            }) {
                let Some((subject_type, subject_key, protocol_chain_key)) =
                    rule.subject_for_event(sample.market_id.as_deref())
                else {
                    continue;
                };

                let state_key = Self::load_state_key(&rule.rule_id, &subject_key);
                let cache_key = Self::cache_key(&event.tenant_id, &state_key);
                let current_state = repo
                    .load_pattern_state(&event.tenant_id, PATTERN_ID, &state_key)
                    .await?
                    .and_then(|value| serde_json::from_value::<TvlRuleState>(value).ok())
                    .unwrap_or_default();
                self.state_cache
                    .insert(cache_key.clone(), current_state.clone());
                self.prune_state_cache(now, &cache_key);

                let mut state = current_state;
                state.protocol_chain_key = Some(protocol_chain_key.clone());

                // Populate deposit gate from event metadata (Spec Section 4).
                if let Some(has_deposit) = event
                    .payload
                    .get("tenant_has_deposit")
                    .and_then(Value::as_bool)
                {
                    state.tenant_has_deposit = Some(has_deposit);
                }
                if let Some(stale) = event
                    .payload
                    .get("position_data_stale")
                    .and_then(Value::as_bool)
                {
                    state.position_data_stale = Some(stale);
                }

                let previous_severity = severity_from_str(state.last_severity.as_deref());
                let previous_drop = state.last_drop_pct.unwrap_or(0.0);
                let previous_context = state
                    .last_context
                    .as_deref()
                    .unwrap_or("isolated")
                    .to_string();

                let evaluation = self.process_state_sample(rule, &mut state, &sample, now);

                let (severity, classification) =
                    if let Some(base_severity) = evaluation.base_severity.clone() {
                        let systemic = rule.contagion_enabled
                            && self.has_concurrent_active_drop(
                                &event.tenant_id,
                                &cache_key,
                                &protocol_chain_key,
                                now,
                                rule.concurrent_window_minutes,
                            );
                        let unclamped = if systemic {
                            (Severity::Critical, ContextClassification::Systemic)
                        } else {
                            (base_severity, ContextClassification::Isolated)
                        };
                        // Severity only goes up, never down (Spec Section 8).
                        let clamped = clamp_severity(unclamped.0, previous_severity.as_ref());
                        (clamped, unclamped.1)
                    } else {
                        (Severity::Info, ContextClassification::None)
                    };

                let mut transition = None;
                if evaluation.base_severity.is_some() {
                    let current_rank = severity_rank(Some(&severity));
                    let previous_rank = severity_rank(previous_severity.as_ref());
                    if previous_severity.is_none() {
                        transition = Some(IncidentTransition::Trigger);
                    } else if current_rank > previous_rank {
                        transition = Some(IncidentTransition::Escalate);
                    } else {
                        let context_changed =
                            previous_context != context_classification_str(&classification);
                        if evaluation.selected_drop_pct >= (previous_drop + 1.0) || context_changed
                        {
                            transition = Some(IncidentTransition::Update);
                        }
                    }
                }

                if evaluation.base_severity.is_some() {
                    if state.active_since.is_none() {
                        state.active_since = Some(now);
                    }
                    state.last_breach_at = Some(now);
                    state.last_severity = Some(format!("{severity:?}").to_ascii_lowercase());
                    state.last_drop_pct = Some(evaluation.selected_drop_pct);
                    state.last_context =
                        Some(context_classification_str(&classification).to_string());
                    if transition.is_some() {
                        state.last_transition_at = Some(now);
                    }
                } else {
                    state.last_drop_pct = Some(evaluation.selected_drop_pct);
                }

                let snapshot = json!({
                    "rule_id": rule.rule_id,
                    "subject_key": subject_key,
                    "tvl_usd": sample.tvl_usd,
                    "fast_drop_pct": evaluation.fast_drop_pct,
                    "slow_drop_pct": evaluation.slow_drop_pct,
                    "velocity_drop_pct": evaluation.velocity_drop_pct,
                    "selected_drop_pct": evaluation.selected_drop_pct,
                    "breached_branches": evaluation.breached_branches,
                    "base_severity": evaluation.base_severity.as_ref().map(|value| format!("{value:?}").to_ascii_lowercase()),
                    "final_severity": if evaluation.base_severity.is_some() { Some(format!("{severity:?}").to_ascii_lowercase()) } else { None },
                    "incident_transition": transition.as_ref().map(incident_transition_str),
                    "context_classification": context_classification_str(&classification),
                    "deposit_gate_skipped": evaluation.deposit_gate_skipped,
                    "min_tvl_floor_usd": rule.min_tvl_floor_usd,
                });
                let severity_str = if evaluation.base_severity.is_some() {
                    Some(format!("{severity:?}").to_ascii_lowercase())
                } else {
                    None
                };
                let _ = repo
                    .insert_pattern_snapshot(PatternSnapshotInsert {
                        tenant_id: &event.tenant_id,
                        pattern_id: PATTERN_ID,
                        snapshot_key: &state_key,
                        data: append_snapshot_meta(event, snapshot),
                        score: Some(evaluation.selected_drop_pct),
                        severity: severity_str.as_deref(),
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
                self.state_cache.insert(cache_key.clone(), state);
                self.prune_state_cache(now, &cache_key);

                if let Some(transition) = transition {
                    let detection_context = TvlDetectionContext {
                        subject: DetectionSubject {
                            subject_type: &subject_type,
                            subject_key: &subject_key,
                        },
                        severity,
                        transition,
                        classification,
                        now,
                    };
                    let mut detection = Self::build_tvl_detection(
                        event,
                        rule,
                        &detection_context,
                        &evaluation,
                        &sample,
                    );
                    // Same-protocol contagion: flag other monitored markets as
                    // at-risk (Spec Section 6 Step 1).
                    let at_risk = self.find_at_risk_markets(
                        &event.tenant_id,
                        &sample.protocol_id,
                        &sample.chain_slug,
                        sample.market_id.as_deref(),
                    );
                    if !at_risk.is_empty() {
                        detection
                            .oracle_context
                            .insert("at_risk_markets".to_string(), json!(at_risk));
                    }
                    emitted = pick_higher_severity(emitted, detection);
                }
            }
        } else if let Some(pause) = parse_tvl_pause_event(event) {
            for rule in rules.iter().filter(|rule| {
                rule.enabled
                    && rule.matches(
                        &pause.protocol_id,
                        &pause.chain_slug,
                        pause.market_id.as_deref(),
                    )
            }) {
                let Some((subject_type, subject_key, protocol_chain_key)) =
                    rule.subject_for_event(pause.market_id.as_deref())
                else {
                    continue;
                };

                let state_key = Self::load_state_key(&rule.rule_id, &subject_key);
                let cache_key = Self::cache_key(&event.tenant_id, &state_key);
                let current_state = repo
                    .load_pattern_state(&event.tenant_id, PATTERN_ID, &state_key)
                    .await?
                    .and_then(|value| serde_json::from_value::<TvlRuleState>(value).ok())
                    .unwrap_or_default();
                self.state_cache
                    .insert(cache_key.clone(), current_state.clone());
                self.prune_state_cache(now, &cache_key);
                let mut state = current_state;
                state.protocol_chain_key = Some(protocol_chain_key);

                let previous_severity = severity_from_str(state.last_severity.as_deref());

                // Determine pause alert parameters based on active incident state.
                let (pause_severity, pause_transition, classification) =
                    if let Some(prev) = &previous_severity {
                        // Active incident → annotate with pause state (Spec Section 11).
                        let classification = match state.last_context.as_deref() {
                            Some("systemic") => ContextClassification::Systemic,
                            Some("isolated") => ContextClassification::Isolated,
                            _ => ContextClassification::None,
                        };
                        (prev.clone(), IncidentTransition::Update, classification)
                    } else if pause.paused {
                        // No active incident + paused → standalone PROTOCOL_PAUSED
                        // alert (Spec Section 11 "Protocol Pause").
                        (
                            Severity::High,
                            IncidentTransition::Trigger,
                            ContextClassification::None,
                        )
                    } else {
                        // No active incident + unpause → nothing to do.
                        continue;
                    };

                state.last_pause_state = Some(pause.paused);
                state.last_transition_at = Some(now);

                let snapshot = json!({
                    "rule_id": rule.rule_id,
                    "subject_key": subject_key,
                    "pause_state": if pause.paused { "paused" } else { "unpaused" },
                    "incident_transition": incident_transition_str(&pause_transition),
                    "context_classification": context_classification_str(&classification),
                });
                let _ = repo
                    .insert_pattern_snapshot(PatternSnapshotInsert {
                        tenant_id: &event.tenant_id,
                        pattern_id: PATTERN_ID,
                        snapshot_key: &state_key,
                        data: append_snapshot_meta(event, snapshot),
                        score: state.last_drop_pct,
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
                self.state_cache.insert(cache_key.clone(), state);
                self.prune_state_cache(now, &cache_key);

                let detection_context = PauseDetectionContext {
                    subject: DetectionSubject {
                        subject_type: &subject_type,
                        subject_key: &subject_key,
                    },
                    severity: pause_severity,
                    transition: pause_transition,
                    classification,
                    now,
                };
                let detection =
                    Self::build_pause_detection(event, rule, &detection_context, &pause);
                emitted = pick_higher_severity(emitted, detection);
            }
        }

        Ok(emitted)
    }
}

fn analyze_window(
    samples: &[TvlSamplePoint],
    now: DateTime<Utc>,
    window_minutes: i64,
) -> WindowAnalysis {
    if samples.is_empty() {
        return WindowAnalysis::default();
    }
    let cutoff = now - Duration::minutes(window_minutes.max(1));
    let window_points: Vec<&TvlSamplePoint> = samples
        .iter()
        .filter(|point| point.observed_at >= cutoff)
        .collect();

    let Some(first) = window_points.first() else {
        return WindowAnalysis::default();
    };
    let Some(last) = window_points.last() else {
        return WindowAnalysis::default();
    };
    if !first.tvl_usd.is_finite() || first.tvl_usd <= 0.0 {
        return WindowAnalysis::default();
    }

    let drop_pct = (((first.tvl_usd - last.tvl_usd).max(0.0) / first.tvl_usd) * 100.0).max(0.0);
    let threshold_tvl = last.tvl_usd;
    let time_to_reach_current_drop_minutes = window_points
        .iter()
        .find(|point| point.tvl_usd <= threshold_tvl)
        .map(|point| (point.observed_at - first.observed_at).num_seconds() as f64 / 60.0);

    WindowAnalysis {
        reference_tvl_usd: Some(first.tvl_usd),
        current_tvl_usd: Some(last.tvl_usd),
        drop_pct,
        time_to_reach_current_drop_minutes,
    }
}

fn max_one_minute_drop_pct(
    samples: &[TvlSamplePoint],
    now: DateTime<Utc>,
    window_minutes: i64,
) -> f64 {
    let cutoff = now - Duration::minutes(window_minutes.max(1));
    let window_points: Vec<&TvlSamplePoint> = samples
        .iter()
        .filter(|point| point.observed_at >= cutoff)
        .collect();
    let mut max_drop = 0.0;

    for pair in window_points.windows(2) {
        let previous = pair[0];
        let current = pair[1];
        let elapsed_seconds = (current.observed_at - previous.observed_at).num_seconds();
        if elapsed_seconds <= 0 || elapsed_seconds > 60 || previous.tvl_usd <= 0.0 {
            continue;
        }

        let drop_pct =
            (((previous.tvl_usd - current.tvl_usd).max(0.0) / previous.tvl_usd) * 100.0).max(0.0);
        if drop_pct > max_drop {
            max_drop = drop_pct;
        }
    }

    max_drop
}

fn classify_velocity_pattern(
    fast_drop_pct: f64,
    max_one_minute_drop_pct: f64,
    time_to_reach_minutes: Option<f64>,
) -> Option<&'static str> {
    if fast_drop_pct <= 0.0 {
        return None;
    }

    let minutes = time_to_reach_minutes?;
    if minutes < CLIFF_WINDOW_MINUTES {
        return Some("CLIFF");
    }
    if max_one_minute_drop_pct > (fast_drop_pct / 2.0) {
        return Some("ACCELERATING");
    }
    if minutes <= LINEAR_WINDOW_UPPER_BOUND_MINUTES {
        return Some("LINEAR");
    }
    Some("DECELERATING")
}

fn parse_tvl_drop_rules(config: &Value, tenant_id: &str) -> Vec<TvlDropRule> {
    let mut parsed = Vec::new();
    let Some(items) = config.get("rules").and_then(Value::as_array) else {
        tracing::warn!(
            tenant_id = %tenant_id,
            "invalid tvl_drop config; missing rules array"
        );
        return vec![default_tvl_drop_rule("tvl-default")];
    };

    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            continue;
        };
        let scope_obj = object
            .get("scope")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let thresholds = object
            .get("thresholds")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let contagion = object
            .get("contagion")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let market_value = scope_obj
            .get("market_id")
            .and_then(Value::as_str)
            .or_else(|| object.get("market_id").and_then(Value::as_str))
            .map(str::trim)
            .map(ToString::to_string)
            .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("all"));
        let inferred_scope = object
            .get("scope")
            .and_then(Value::as_str)
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| value == "market" || value == "protocol")
            .unwrap_or_else(|| {
                if market_value.is_some() {
                    "market".to_string()
                } else {
                    "protocol".to_string()
                }
            });

        let mut rule = TvlDropRule {
            rule_id: value_string(object.get("rule_id"), &format!("tvl-rule-{}", index + 1)),
            protocol_id: value_string(
                scope_obj
                    .get("protocol_id")
                    .or_else(|| object.get("protocol_id")),
                "aave_v3",
            )
            .to_ascii_lowercase(),
            chain_slug: value_string(
                scope_obj
                    .get("chain_slug")
                    .or_else(|| object.get("chain_slug")),
                "base",
            )
            .to_ascii_lowercase(),
            scope: inferred_scope,
            market_id: market_value.map(|value| value.to_ascii_lowercase()),
            fast_drop_pct: value_f64(
                thresholds
                    .get("fast_drop_pct")
                    .or_else(|| object.get("fast_drop_pct")),
                DEFAULT_FAST_DROP_PCT,
            ),
            fast_window_minutes: value_window_minutes(
                thresholds
                    .get("fast_window_minutes")
                    .or_else(|| object.get("fast_window_minutes")),
                thresholds
                    .get("fast_window_sec")
                    .or_else(|| object.get("fast_window_sec")),
                DEFAULT_FAST_WINDOW_MINUTES,
            ),
            slow_drop_pct: value_f64(
                thresholds
                    .get("slow_drop_pct")
                    .or_else(|| object.get("slow_drop_pct")),
                DEFAULT_SLOW_DROP_PCT,
            ),
            slow_window_minutes: value_window_minutes(
                thresholds
                    .get("slow_window_minutes")
                    .or_else(|| object.get("slow_window_minutes")),
                thresholds
                    .get("slow_window_sec")
                    .or_else(|| object.get("slow_window_sec")),
                DEFAULT_SLOW_WINDOW_MINUTES,
            ),
            velocity_critical_pct: value_f64(
                thresholds
                    .get("velocity_critical_pct")
                    .or_else(|| object.get("velocity_critical_pct")),
                DEFAULT_VELOCITY_CRITICAL_PCT,
            ),
            velocity_critical_minutes: value_window_minutes(
                thresholds
                    .get("velocity_critical_minutes")
                    .or_else(|| object.get("velocity_critical_minutes")),
                thresholds
                    .get("velocity_window_sec")
                    .or_else(|| object.get("velocity_window_sec")),
                DEFAULT_VELOCITY_WINDOW_MINUTES,
            ),
            concurrent_window_minutes: value_window_minutes(
                object
                    .get("concurrent_window_minutes")
                    .or_else(|| thresholds.get("concurrent_window_minutes")),
                contagion
                    .get("overlap_window_sec")
                    .or_else(|| object.get("concurrent_window_sec")),
                DEFAULT_CONCURRENT_WINDOW_MINUTES,
            ),
            min_tvl_floor_usd: value_f64(
                object
                    .get("min_tvl_floor_usd")
                    .or_else(|| thresholds.get("min_tvl_floor_usd")),
                DEFAULT_MIN_TVL_FLOOR_USD,
            ),
            enabled: object
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            contagion_enabled: contagion
                .get("enabled")
                .and_then(Value::as_bool)
                .or_else(|| object.get("contagion_enabled").and_then(Value::as_bool))
                .unwrap_or(true),
        };

        if rule.scope == "protocol" {
            rule.market_id = None;
        }

        if let Err(error) = rule.validate() {
            common::log_error!(
                warn,
                error,
                "invalid tvl_drop rule; skipping",
                tenant_id = %tenant_id,
                rule_id = %rule.rule_id
            );
            continue;
        }
        parsed.push(rule);
    }

    if parsed.is_empty() {
        tracing::warn!(
            tenant_id = %tenant_id,
            "no valid tvl_drop rules found; falling back to defaults"
        );
        return vec![default_tvl_drop_rule("tvl-default")];
    }

    parsed
}

fn default_tvl_drop_rule(rule_id: &str) -> TvlDropRule {
    TvlDropRule {
        rule_id: rule_id.to_string(),
        protocol_id: "aave_v3".to_string(),
        chain_slug: "base".to_string(),
        scope: "protocol".to_string(),
        market_id: None,
        fast_drop_pct: DEFAULT_FAST_DROP_PCT,
        fast_window_minutes: DEFAULT_FAST_WINDOW_MINUTES,
        slow_drop_pct: DEFAULT_SLOW_DROP_PCT,
        slow_window_minutes: DEFAULT_SLOW_WINDOW_MINUTES,
        velocity_critical_pct: DEFAULT_VELOCITY_CRITICAL_PCT,
        velocity_critical_minutes: DEFAULT_VELOCITY_WINDOW_MINUTES,
        concurrent_window_minutes: DEFAULT_CONCURRENT_WINDOW_MINUTES,
        min_tvl_floor_usd: DEFAULT_MIN_TVL_FLOOR_USD,
        enabled: true,
        contagion_enabled: true,
    }
}

fn parse_tvl_state_event(event: &UnifiedEvent) -> Option<TvlStateEvent> {
    match event.event_type.as_str() {
        "protocol_state" | "protocol_tvl" => {}
        _ => return None,
    }

    let payload = event.payload.as_object()?;
    let protocol_id = payload
        .get("protocol_id")
        .or_else(|| payload.get("protocol"))
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_lowercase();
    let chain_slug = payload
        .get("chain_slug")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .or_else(|| chain_slug_from_chain_id(event.chain_id))
        .unwrap_or_else(|| "unknown".to_string());
    let market_id = payload
        .get("market_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("all"))
        .map(|value| value.to_ascii_lowercase());
    let tvl_usd = payload
        .get("tvl_usd")
        .or_else(|| payload.get("tvl"))
        .and_then(value_to_f64)?;
    if !(tvl_usd.is_finite() && tvl_usd > 0.0) {
        return None;
    }
    let block_number = event
        .block_number
        .or_else(|| payload.get("block_number").and_then(Value::as_i64))
        .unwrap_or(0);
    let tx_hash = event.tx_hash.clone().or_else(|| {
        payload
            .get("tx_hash")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    });

    Some(TvlStateEvent {
        protocol_id,
        chain_slug,
        market_id,
        tvl_usd,
        block_number,
        tx_hash,
    })
}

fn parse_tvl_pause_event(event: &UnifiedEvent) -> Option<TvlPauseEvent> {
    let inferred_pause_state = match event.event_type.as_str() {
        "protocol_pause" | "protocol_paused" => Some(true),
        "protocol_unpause" | "protocol_unpaused" => Some(false),
        _ => None,
    };
    let payload = event.payload.as_object()?;
    let protocol_id = payload
        .get("protocol_id")
        .or_else(|| payload.get("protocol"))
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_lowercase();
    let chain_slug = payload
        .get("chain_slug")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .or_else(|| chain_slug_from_chain_id(event.chain_id))
        .unwrap_or_else(|| "unknown".to_string());
    let market_id = payload
        .get("market_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("all"))
        .map(|value| value.to_ascii_lowercase());
    let paused = inferred_pause_state
        .or_else(|| payload.get("paused").and_then(Value::as_bool))
        .or_else(|| payload.get("is_paused").and_then(Value::as_bool))?;
    let block_number = event
        .block_number
        .or_else(|| payload.get("block_number").and_then(Value::as_i64))
        .unwrap_or(0);
    let tx_hash = event.tx_hash.clone().or_else(|| {
        payload
            .get("tx_hash")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    });

    Some(TvlPauseEvent {
        protocol_id,
        chain_slug,
        market_id,
        paused,
        block_number,
        tx_hash,
    })
}

fn chain_slug_from_chain_id(chain_id: Option<i64>) -> Option<String> {
    match chain_id {
        Some(1) => Some("ethereum".to_string()),
        Some(42161) => Some("arbitrum".to_string()),
        Some(10) => Some("optimism".to_string()),
        Some(8453) => Some("base".to_string()),
        Some(137) => Some("polygon".to_string()),
        Some(43114) => Some("avalanche".to_string()),
        Some(56) => Some("bsc".to_string()),
        _ => None,
    }
}

fn chain_from_slug(slug: &str) -> Chain {
    match slug {
        "ethereum" | "mainnet" => Chain::Ethereum,
        "arbitrum" => Chain::Arbitrum,
        "optimism" => Chain::Optimism,
        "base" => Chain::Base,
        "polygon" => Chain::Polygon,
        "avalanche" => Chain::Avalanche,
        "bsc" | "bnb" => Chain::BSC,
        _ => Chain::Unknown,
    }
}

fn recommended_actions_for_severity(severity: &Severity) -> Vec<String> {
    match severity {
        Severity::Critical => vec![
            "Activate incident playbook and halt high-risk protocol interactions.".to_string(),
            "Escalate to on-call and communicate exposure impact immediately.".to_string(),
            "Verify protocol pause controls and liquidity withdrawal options.".to_string(),
        ],
        Severity::High => vec![
            "Increase monitoring cadence and review wallet/protocol exposure.".to_string(),
            "Prepare partial de-risk actions for affected markets.".to_string(),
        ],
        _ => vec!["Monitor trend and keep incident timeline updated.".to_string()],
    }
}

fn severity_from_str(value: Option<&str>) -> Option<Severity> {
    match value {
        Some(value) if value.eq_ignore_ascii_case("critical") => Some(Severity::Critical),
        Some(value) if value.eq_ignore_ascii_case("high") => Some(Severity::High),
        Some(value) if value.eq_ignore_ascii_case("medium") => Some(Severity::Medium),
        Some(value) if value.eq_ignore_ascii_case("low") => Some(Severity::Low),
        Some(value) if value.eq_ignore_ascii_case("info") => Some(Severity::Info),
        _ => None,
    }
}

/// Clamp severity so it never drops below the previous level.
/// Spec Section 8 / Alerting Spec Section 3.5: "Severity only goes up, never down."
fn clamp_severity(current: Severity, previous: Option<&Severity>) -> Severity {
    match previous {
        Some(prev) if severity_rank(Some(&current)) < severity_rank(Some(prev)) => prev.clone(),
        _ => current,
    }
}

fn severity_rank(value: Option<&Severity>) -> u8 {
    match value {
        Some(Severity::Critical) => 5,
        Some(Severity::High) => 4,
        Some(Severity::Medium) => 3,
        Some(Severity::Low) => 2,
        Some(Severity::Info) => 1,
        None => 0,
    }
}

fn incident_transition_str(value: &IncidentTransition) -> &'static str {
    match value {
        IncidentTransition::Trigger => "trigger",
        IncidentTransition::Escalate => "escalate",
        IncidentTransition::Deescalate => "deescalate",
        IncidentTransition::Resolve => "resolve",
        IncidentTransition::Retract => "retract",
        IncidentTransition::Update => "update",
    }
}

fn context_classification_str(value: &ContextClassification) -> &'static str {
    match value {
        ContextClassification::Isolated => "isolated",
        ContextClassification::Systemic => "systemic",
        ContextClassification::None => "none",
    }
}

fn tvl_state_last_activity(state: &TvlRuleState) -> Option<DateTime<Utc>> {
    state
        .samples
        .last()
        .map(|sample| sample.observed_at)
        .into_iter()
        .chain(state.last_transition_at)
        .chain(state.last_breach_at)
        .chain(state.active_since)
        .max()
}

fn pick_higher_severity(
    current: Option<DetectionResult>,
    candidate: DetectionResult,
) -> Option<DetectionResult> {
    match current {
        Some(existing) => {
            let existing_rank = severity_rank(Some(&existing.severity));
            let candidate_rank = severity_rank(Some(&candidate.severity));
            if candidate_rank > existing_rank {
                Some(candidate)
            } else {
                Some(existing)
            }
        }
        None => Some(candidate),
    }
}

fn value_string(value: Option<&Value>, fallback: &str) -> String {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn value_to_f64(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value
        .as_str()
        .and_then(|text| text.trim().parse::<f64>().ok())
}

fn value_f64(value: Option<&Value>, fallback: f64) -> f64 {
    value.and_then(value_to_f64).unwrap_or(fallback)
}

fn value_i64(value: Option<&Value>) -> Option<i64> {
    if let Some(number) = value.and_then(Value::as_i64) {
        return Some(number);
    }
    if let Some(number) = value.and_then(Value::as_u64) {
        return Some(number as i64);
    }
    value
        .and_then(Value::as_str)
        .and_then(|text| text.trim().parse::<i64>().ok())
}

fn value_window_minutes(minutes: Option<&Value>, seconds: Option<&Value>, fallback: i64) -> i64 {
    if let Some(value) = value_i64(minutes) {
        return value.max(1);
    }
    if let Some(value) = value_i64(seconds) {
        return ((value + 59) / 60).max(1);
    }
    fallback.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_schema::SourceType;
    use serde_json::json;

    fn base_rule() -> TvlDropRule {
        TvlDropRule {
            rule_id: "tvl-default".to_string(),
            protocol_id: "aave_v3".to_string(),
            chain_slug: "base".to_string(),
            scope: "protocol".to_string(),
            market_id: None,
            fast_drop_pct: 20.0,
            fast_window_minutes: 10,
            slow_drop_pct: 35.0,
            slow_window_minutes: 60,
            velocity_critical_pct: 15.0,
            velocity_critical_minutes: 2,
            concurrent_window_minutes: 5,
            min_tvl_floor_usd: 1_000_000.0,
            enabled: true,
            contagion_enabled: true,
        }
    }

    fn sample(ts: DateTime<Utc>, tvl_usd: f64) -> TvlSamplePoint {
        TvlSamplePoint {
            observed_at: ts,
            tvl_usd,
        }
    }

    fn state_event(
        protocol_id: &str,
        chain_slug: &str,
        market_id: Option<&str>,
        tvl_usd: f64,
    ) -> TvlStateEvent {
        TvlStateEvent {
            protocol_id: protocol_id.to_string(),
            chain_slug: chain_slug.to_string(),
            market_id: market_id.map(ToString::to_string),
            tvl_usd,
            block_number: 1,
            tx_hash: Some("0xtest".to_string()),
        }
    }

    fn unified_tvl_event() -> UnifiedEvent {
        UnifiedEvent {
            event_id: "evt-1".to_string(),
            tenant_id: "tenant-a".to_string(),
            source_id: "aave-v3-base-protocol-state".to_string(),
            source_type: SourceType::EvmChain,
            event_type: "protocol_state".to_string(),
            timestamp: Utc::now(),
            payload: json!({
                "protocol_id": "aave_v3",
                "chain_slug": "base",
                "tvl_usd": 375_000_000.0
            }),
            chain_id: Some(8453),
            block_number: Some(1),
            tx_hash: Some("0xtest".to_string()),
            market_key: None,
            price: None,
        }
    }

    #[test]
    fn velocity_branch_sets_critical() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(1), 100_000_000.0));
        let event = state_event("aave_v3", "base", None, 80_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(matches!(eval.base_severity, Some(Severity::Critical)));
        assert_eq!(eval.velocity_pattern.as_deref(), Some("ACCELERATING"));
        assert_eq!(eval.breached_branches, vec!["velocity".to_string()]);
    }

    #[test]
    fn floor_gate_blocks_detection() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let mut rule = base_rule();
        rule.min_tvl_floor_usd = 5_000_000.0;
        state
            .samples
            .push(sample(now - Duration::minutes(3), 4_500_000.0));
        let event = state_event("aave_v3", "base", None, 3_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(eval.base_severity.is_none());
    }

    #[test]
    fn floor_gate_uses_fast_window_reference_tvl() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(4), 2_000_000.0));
        state
            .samples
            .push(sample(now - Duration::minutes(2), 1_600_000.0));
        let event = state_event("aave_v3", "base", None, 800_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        assert!(matches!(eval.base_severity, Some(Severity::Critical)));
        assert_eq!(eval.fast_window_reference_tvl_usd, Some(2_000_000.0));
    }

    #[test]
    fn exactly_at_floor_still_allows_detection() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(4), 1_000_000.0));
        let event = state_event("aave_v3", "base", None, 750_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        assert!(matches!(eval.base_severity, Some(Severity::Critical)));
    }

    #[test]
    fn fast_drop_under_five_minutes_is_critical() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(4), 100_000_000.0));
        let event = state_event("aave_v3", "base", None, 78_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        assert!(matches!(eval.base_severity, Some(Severity::Critical)));
        assert_eq!(eval.time_to_reach_current_drop_minutes, Some(4.0));
        assert_eq!(eval.breached_branches, vec!["fast".to_string()]);
    }

    #[test]
    fn fast_drop_at_exactly_five_minutes_is_high() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(5), 100_000_000.0));
        let event = state_event("aave_v3", "base", None, 80_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        assert!(matches!(eval.base_severity, Some(Severity::High)));
        assert_eq!(eval.time_to_reach_current_drop_minutes, Some(5.0));
    }

    #[test]
    fn fast_drop_at_exactly_two_minutes_uses_fast_branch_critical() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(2), 100_000_000.0));
        let event = state_event("aave_v3", "base", None, 80_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        assert!(matches!(eval.base_severity, Some(Severity::Critical)));
        assert_eq!(eval.time_to_reach_current_drop_minutes, Some(2.0));
        assert_eq!(eval.breached_branches, vec!["fast".to_string()]);
    }

    #[test]
    fn slow_drain_fires_when_fast_window_misses() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(60), 500_000_000.0));
        state
            .samples
            .push(sample(now - Duration::minutes(10), 340_000_000.0));
        let event = state_event("aave_v3", "base", None, 310_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        assert!(matches!(eval.base_severity, Some(Severity::High)));
        assert!(eval.fast_drop_pct < rule.fast_drop_pct);
        assert!(eval.slow_drop_pct >= rule.slow_drop_pct);
        assert_eq!(eval.breached_branches, vec!["slow".to_string()]);
    }

    #[test]
    fn tvl_increase_does_not_alert() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(10), 500_000_000.0));
        let event = state_event("aave_v3", "base", None, 550_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        assert!(eval.base_severity.is_none());
        assert_eq!(eval.fast_drop_pct, 0.0);
        assert_eq!(eval.slow_drop_pct, 0.0);
    }

    #[test]
    fn velocity_classification_matches_pdf_examples() {
        assert_eq!(
            classify_velocity_pattern(30.0, 10.0, Some(0.05)),
            Some("CLIFF")
        );
        assert_eq!(
            classify_velocity_pattern(25.0, 15.0, Some(4.0)),
            Some("ACCELERATING")
        );
        assert_eq!(
            classify_velocity_pattern(20.0, 5.0, Some(5.0)),
            Some("LINEAR")
        );
        assert_eq!(
            classify_velocity_pattern(20.0, 5.0, Some(9.0)),
            Some("DECELERATING")
        );
    }

    #[test]
    fn parser_supports_threshold_and_scope_variants() {
        let config = json!({
            "rules": [{
                "rule_id": "r1",
                "scope": {
                    "chain_slug": "base",
                    "protocol_id": "morpho_blue",
                    "market_id": "usdc"
                },
                "thresholds": {
                    "fast_drop_pct": 25,
                    "fast_window_sec": 300,
                    "slow_drop_pct": 40,
                    "slow_window_sec": 3600,
                    "velocity_critical_pct": 18,
                    "velocity_window_sec": 120
                },
                "contagion": {
                    "enabled": true,
                    "overlap_window_sec": 600
                },
                "min_tvl_floor_usd": 2500000,
                "enabled": true
            }]
        });

        let rules = parse_tvl_drop_rules(&config, "tenant-a");
        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        assert_eq!(rule.protocol_id, "morpho_blue");
        assert_eq!(rule.chain_slug, "base");
        assert_eq!(rule.scope, "market");
        assert_eq!(rule.market_id.as_deref(), Some("usdc"));
        assert_eq!(rule.fast_window_minutes, 5);
        assert_eq!(rule.velocity_critical_minutes, 2);
        assert_eq!(rule.concurrent_window_minutes, 10);
    }

    #[tokio::test]
    async fn reload_config_keeps_tenant_rules_isolated_for_same_subject() {
        let mut pattern = TvlDropPattern::default();
        let mut config_map = HashMap::new();

        config_map.insert(
            ("tenant-a".to_string(), PATTERN_ID.to_string()),
            json!({
                "rules": [{
                    "rule_id": "tvl-a",
                    "scope": {
                        "chain_slug": "base",
                        "protocol_id": "aave_v3",
                        "market_id": "all"
                    },
                    "thresholds": {
                        "fast_drop_pct": 10,
                        "fast_window_sec": 300,
                        "slow_drop_pct": 20,
                        "slow_window_sec": 3600,
                        "velocity_critical_pct": 5,
                        "velocity_window_sec": 120
                    },
                    "contagion": {
                        "enabled": true,
                        "overlap_window_sec": 300
                    },
                    "min_tvl_floor_usd": 1000000,
                    "enabled": true
                }]
            }),
        );
        config_map.insert(
            ("tenant-b".to_string(), PATTERN_ID.to_string()),
            json!({
                "rules": [{
                    "rule_id": "tvl-b",
                    "scope": {
                        "chain_slug": "base",
                        "protocol_id": "aave_v3",
                        "market_id": "all"
                    },
                    "thresholds": {
                        "fast_drop_pct": 35,
                        "fast_window_sec": 300,
                        "slow_drop_pct": 50,
                        "slow_window_sec": 3600,
                        "velocity_critical_pct": 25,
                        "velocity_window_sec": 120
                    },
                    "contagion": {
                        "enabled": false,
                        "overlap_window_sec": 900
                    },
                    "min_tvl_floor_usd": 2000000,
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

        assert_eq!(tenant_a.len(), 1);
        assert_eq!(tenant_b.len(), 1);
        assert_eq!(tenant_a[0].fast_drop_pct, 10.0);
        assert_eq!(tenant_b[0].fast_drop_pct, 35.0);
        assert!(tenant_a[0].contagion_enabled);
        assert!(!tenant_b[0].contagion_enabled);
    }

    #[test]
    fn process_state_sample_uses_tenant_specific_thresholds_independently() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        state
            .samples
            .push(sample(now - Duration::minutes(4), 100_000_000.0));
        let mut tenant_a_state = state.clone();
        let mut tenant_b_state = state;

        let event = state_event("aave_v3", "base", None, 80_000_000.0);

        let mut tenant_a_rule = base_rule();
        tenant_a_rule.fast_drop_pct = 10.0;
        tenant_a_rule.slow_drop_pct = 20.0;
        tenant_a_rule.velocity_critical_pct = 25.0;

        let mut tenant_b_rule = base_rule();
        tenant_b_rule.fast_drop_pct = 30.0;
        tenant_b_rule.slow_drop_pct = 45.0;
        tenant_b_rule.velocity_critical_pct = 25.0;

        let pattern = TvlDropPattern::default();
        let tenant_a =
            pattern.process_state_sample(&tenant_a_rule, &mut tenant_a_state, &event, now);
        let tenant_b =
            pattern.process_state_sample(&tenant_b_rule, &mut tenant_b_state, &event, now);

        assert!(tenant_a.base_severity.is_some());
        assert!(tenant_a.selected_drop_pct >= tenant_a_rule.fast_drop_pct);
        assert!(tenant_b.base_severity.is_none());
        assert!(tenant_b.selected_drop_pct < tenant_b_rule.fast_drop_pct);
    }

    #[test]
    fn payload_includes_velocity_drain_and_contagion_metadata() {
        let event = unified_tvl_event();
        let rule = base_rule();
        let evaluation = TvlEvaluation {
            fast_drop_pct: 25.0,
            slow_drop_pct: 25.0,
            velocity_drop_pct: 15.0,
            selected_drop_pct: 25.0,
            fast_window_reference_tvl_usd: Some(500_000_000.0),
            time_to_reach_current_drop_minutes: Some(5.0),
            drain_rate_usd_per_min: Some(25_000_000.0),
            estimated_time_to_empty_minutes: Some(15.0),
            velocity_pattern: Some("CLIFF".to_string()),
            base_severity: Some(Severity::Critical),
            breached_branches: vec!["velocity".to_string()],
            deposit_gate_skipped: false,
            position_data_stale: false,
        };
        let context = TvlDetectionContext {
            subject: DetectionSubject {
                subject_type: "protocol",
                subject_key: "aave_v3:base",
            },
            severity: Severity::Critical,
            transition: IncidentTransition::Trigger,
            classification: ContextClassification::Systemic,
            now: Utc::now(),
        };
        let sample = state_event("aave_v3", "base", None, 375_000_000.0);

        let detection =
            TvlDropPattern::build_tvl_detection(&event, &rule, &context, &evaluation, &sample);
        let oracle = detection.oracle_context;

        assert_eq!(
            oracle.get("velocity_pattern"),
            Some(&json!(Some("CLIFF".to_string())))
        );
        assert_eq!(
            oracle.get("drain_rate_usd_per_min"),
            Some(&json!(Some(25_000_000.0)))
        );
        assert_eq!(
            oracle.get("estimated_time_to_empty_minutes"),
            Some(&json!(Some(15.0)))
        );
        assert_eq!(
            oracle.get("contagion_status"),
            Some(&json!("CONCURRENT_DROPS"))
        );
    }

    #[test]
    fn concurrent_active_drop_detection_is_tenant_isolated() {
        let now = Utc::now();
        let mut pattern = TvlDropPattern::default();
        pattern.state_cache.insert(
            "tenant-a:rule-a:aave_v3:base".to_string(),
            TvlRuleState {
                last_severity: Some("high".to_string()),
                last_breach_at: Some(now),
                protocol_chain_key: Some("aave_v3:base".to_string()),
                ..Default::default()
            },
        );
        pattern.state_cache.insert(
            "tenant-a:rule-b:euler_v2:base".to_string(),
            TvlRuleState {
                last_severity: Some("high".to_string()),
                last_breach_at: Some(now),
                protocol_chain_key: Some("euler_v2:base".to_string()),
                ..Default::default()
            },
        );
        pattern.state_cache.insert(
            "tenant-b:rule-c:euler_v2:base".to_string(),
            TvlRuleState {
                last_severity: Some("high".to_string()),
                last_breach_at: Some(now),
                protocol_chain_key: Some("euler_v2:base".to_string()),
                ..Default::default()
            },
        );

        assert!(pattern.has_concurrent_active_drop(
            "tenant-a",
            "tenant-a:rule-a:aave_v3:base",
            "aave_v3:base",
            now,
            5
        ));
        assert!(!pattern.has_concurrent_active_drop(
            "tenant-b",
            "tenant-b:rule-c:euler_v2:base",
            "euler_v2:base",
            now,
            5
        ));
    }

    // ───────────────────────────────────────────────────────────────────────
    // Test-plan helpers
    // ───────────────────────────────────────────────────────────────────────

    /// Determine the incident transition for a TVL-drop evaluation.
    /// Returns `None` when no transition is warranted (duplicate signal with
    /// unchanged severity / drop / context → Spec Section 7 deduplication).
    fn determine_transition(
        base_severity: Option<&Severity>,
        final_severity: &Severity,
        previous_severity: Option<&Severity>,
        selected_drop_pct: f64,
        previous_drop_pct: f64,
        previous_context: &str,
        current_context: &str,
    ) -> Option<IncidentTransition> {
        base_severity?;
        let current_rank = severity_rank(Some(final_severity));
        let previous_rank = severity_rank(previous_severity);
        if previous_severity.is_none() {
            Some(IncidentTransition::Trigger)
        } else if current_rank > previous_rank {
            Some(IncidentTransition::Escalate)
        } else {
            let context_changed = previous_context != current_context;
            if selected_drop_pct >= (previous_drop_pct + 1.0) || context_changed {
                Some(IncidentTransition::Update)
            } else {
                None
            }
        }
    }

    /// Compare orphaned-fork and canonical-chain evaluations to determine the
    /// reorg correction type (Spec Section 11 Reorg Handling).
    fn determine_reorg_correction(
        orphaned_severity: Option<&Severity>,
        canonical_severity: Option<&Severity>,
    ) -> Option<&'static str> {
        match (orphaned_severity, canonical_severity) {
            (Some(_), None) => Some("RETRACTION"),
            (Some(o), Some(c)) if severity_rank(Some(c)) != severity_rank(Some(o)) => {
                Some("UPDATE")
            }
            (None, Some(_)) => Some("LATE_ALERT"),
            _ => None,
        }
    }

    fn pause_event_data(protocol_id: &str, chain_slug: &str, paused: bool) -> TvlPauseEvent {
        TvlPauseEvent {
            protocol_id: protocol_id.to_string(),
            chain_slug: chain_slug.to_string(),
            market_id: None,
            paused,
            block_number: 1,
            tx_hash: Some("0xtest-pause".to_string()),
        }
    }

    fn unified_pause_event(protocol_id: &str, paused: bool) -> UnifiedEvent {
        UnifiedEvent {
            event_id: "evt-pause-1".to_string(),
            tenant_id: "tenant-a".to_string(),
            source_id: format!("{protocol_id}-base-pause"),
            source_type: SourceType::EvmChain,
            event_type: if paused {
                "protocol_pause".to_string()
            } else {
                "protocol_unpause".to_string()
            },
            timestamp: Utc::now(),
            payload: json!({
                "protocol_id": protocol_id,
                "chain_slug": "base",
                "paused": paused,
            }),
            chain_id: Some(8453),
            block_number: Some(1),
            tx_hash: Some("0xtest-pause".to_string()),
            market_key: None,
            price: None,
        }
    }

    // ───────────────────────────────────────────────────────────────────────
    // 2. Gate Checks — TC-GATE-01, TC-GATE-04
    // ───────────────────────────────────────────────────────────────────────

    /// TC-GATE-01: tenant has NO deposits → skip detection even for severe drop.
    #[test]
    fn no_deposit_skips_detection() {
        let now = Utc::now();
        let mut state = TvlRuleState {
            tenant_has_deposit: Some(false),
            ..Default::default()
        };
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(3), 500_000_000.0));
        // 50% drop — would be CRITICAL if evaluated.
        let event = state_event("aave_v3", "base", None, 250_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(eval.base_severity.is_none());
        assert!(eval.deposit_gate_skipped);
    }

    /// TC-GATE-04: position data stale → default to alerting (tenant_has_deposit = true).
    #[test]
    fn stale_position_data_defaults_to_alerting() {
        let now = Utc::now();
        let mut state = TvlRuleState {
            tenant_has_deposit: Some(false), // would normally skip
            position_data_stale: Some(true), // stale overrides → proceed
            ..Default::default()
        };
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(3), 500_000_000.0));
        let event = state_event("aave_v3", "base", None, 375_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(matches!(eval.base_severity, Some(Severity::Critical)));
        assert!(!eval.deposit_gate_skipped);
        assert!(eval.position_data_stale);
    }

    // ───────────────────────────────────────────────────────────────────────
    // 3. Severity — Fast Drop Window — TC-SEV-03, TC-SEV-04, TC-SEV-07
    // ───────────────────────────────────────────────────────────────────────

    /// TC-SEV-03: 22% drop over 7 min → HIGH (moderate velocity, >= 5 min).
    #[test]
    fn moderate_fast_drop_over_five_min_is_high() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(7), 100_000_000.0));
        let event = state_event("aave_v3", "base", None, 78_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(matches!(eval.base_severity, Some(Severity::High)));
        assert_eq!(eval.breached_branches, vec!["fast".to_string()]);
        assert_eq!(eval.time_to_reach_current_drop_minutes, Some(7.0));
    }

    /// TC-SEV-04: fast=18% (< 20%), slow=12% (< 35%) → no alert.
    #[test]
    fn below_both_thresholds_no_alert() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule(); // fast_drop_pct=20, slow_drop_pct=35
        state
            .samples
            .push(sample(now - Duration::minutes(60), 100_000_000.0));
        state
            .samples
            .push(sample(now - Duration::minutes(10), 100_000_000.0));
        // fast: (100M - 82M)/100M = 18%, slow: (100M - 82M)/100M = 18%
        let event = state_event("aave_v3", "base", None, 82_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(eval.base_severity.is_none());
        assert!(eval.breached_branches.is_empty());
    }

    /// TC-SEV-07: severity is NEVER MEDIUM for TVL drops (exhaustive).
    #[test]
    fn severity_is_never_medium_for_tvl_drops() {
        let now = Utc::now();
        let pattern = TvlDropPattern::default();
        let rule = base_rule();

        for fast_pct in [5, 10, 15, 20, 25, 30, 50, 80] {
            for time_min in [1i64, 2, 3, 5, 7, 10] {
                let start_tvl = 100_000_000.0;
                let end_tvl = start_tvl * (1.0 - fast_pct as f64 / 100.0);
                let mut state = TvlRuleState::default();
                state
                    .samples
                    .push(sample(now - Duration::minutes(time_min), start_tvl));
                let event = state_event("aave_v3", "base", None, end_tvl);
                let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
                if let Some(ref sev) = eval.base_severity {
                    assert!(
                        !matches!(sev, Severity::Medium),
                        "got MEDIUM for fast_pct={fast_pct}%, time={time_min}min"
                    );
                }
            }
        }
    }

    // ───────────────────────────────────────────────────────────────────────
    // 4. Severity — Slow Drop Window — TC-SLOW-01, TC-SLOW-02
    // ───────────────────────────────────────────────────────────────────────

    /// TC-SLOW-01: fast=8.6% (< 20%), slow=36% (>= 35%) → HIGH.
    #[test]
    fn slow_drain_above_threshold_standalone_high() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        // 60-min TVL $500M, 10-min TVL $350M, current $320M.
        // fast = (350 - 320)/350 = 8.6%, slow = (500 - 320)/500 = 36%.
        state
            .samples
            .push(sample(now - Duration::minutes(60), 500_000_000.0));
        state
            .samples
            .push(sample(now - Duration::minutes(10), 350_000_000.0));
        let event = state_event("aave_v3", "base", None, 320_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(matches!(eval.base_severity, Some(Severity::High)));
        assert!(eval.fast_drop_pct < rule.fast_drop_pct);
        assert!(eval.slow_drop_pct >= rule.slow_drop_pct);
        assert_eq!(eval.breached_branches, vec!["slow".to_string()]);
    }

    /// TC-SLOW-02: fast=2.2% (< 20%), slow=12% (< 35%) → no alert.
    #[test]
    fn slow_drain_below_threshold_no_alert() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(60), 100_000_000.0));
        state
            .samples
            .push(sample(now - Duration::minutes(10), 90_000_000.0));
        let event = state_event("aave_v3", "base", None, 88_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(eval.base_severity.is_none());
    }

    // ───────────────────────────────────────────────────────────────────────
    // 7. Contagion — TC-CONT-02, TC-CONT-03
    // ───────────────────────────────────────────────────────────────────────

    /// TC-CONT-02: cross-protocol concurrent drops → systemic flag detected.
    /// When two different protocols have active breaches for the same tenant,
    /// `has_concurrent_active_drop` returns true (systemic → elevate to CRITICAL).
    #[test]
    fn cross_protocol_concurrent_drops_elevates_to_critical() {
        let now = Utc::now();
        let mut pattern = TvlDropPattern::default();
        // Aave V3 active breach
        pattern.state_cache.insert(
            "tenant-a:rule-a:aave_v3:base".to_string(),
            TvlRuleState {
                last_severity: Some("high".to_string()),
                last_breach_at: Some(now),
                protocol_chain_key: Some("aave_v3:base".to_string()),
                ..Default::default()
            },
        );
        // Euler V2 active breach (different protocol)
        pattern.state_cache.insert(
            "tenant-a:rule-b:euler_v2:base".to_string(),
            TvlRuleState {
                last_severity: Some("high".to_string()),
                last_breach_at: Some(now),
                protocol_chain_key: Some("euler_v2:base".to_string()),
                ..Default::default()
            },
        );

        // From aave_v3's perspective: euler_v2 is also breaching → concurrent
        assert!(pattern.has_concurrent_active_drop(
            "tenant-a",
            "tenant-a:rule-a:aave_v3:base",
            "aave_v3:base",
            now,
            5
        ));
        // From euler_v2's perspective: aave_v3 is also breaching → concurrent
        assert!(pattern.has_concurrent_active_drop(
            "tenant-a",
            "tenant-a:rule-b:euler_v2:base",
            "euler_v2:base",
            now,
            5
        ));
    }

    /// TC-CONT-03: single protocol dropping → no cross-protocol contagion.
    #[test]
    fn single_protocol_no_cross_contagion() {
        let now = Utc::now();
        let mut pattern = TvlDropPattern::default();
        pattern.state_cache.insert(
            "tenant-a:rule-a:aave_v3:base".to_string(),
            TvlRuleState {
                last_severity: Some("high".to_string()),
                last_breach_at: Some(now),
                protocol_chain_key: Some("aave_v3:base".to_string()),
                ..Default::default()
            },
        );
        // No other protocols active for tenant-a
        assert!(!pattern.has_concurrent_active_drop(
            "tenant-a",
            "tenant-a:rule-a:aave_v3:base",
            "aave_v3:base",
            now,
            5
        ));
    }

    // ───────────────────────────────────────────────────────────────────────
    // 9. Escalation — TC-ESC-01, TC-ESC-02
    // ───────────────────────────────────────────────────────────────────────

    /// TC-ESC-01: severity escalates from HIGH to CRITICAL on worsening drop.
    #[test]
    fn severity_escalates_high_to_critical() {
        let now = Utc::now();
        let pattern = TvlDropPattern::default();
        let rule = base_rule();

        // First evaluation: 20% drop over 7 min → HIGH
        let mut state = TvlRuleState::default();
        state
            .samples
            .push(sample(now - Duration::minutes(7), 100_000_000.0));
        let event1 = state_event("aave_v3", "base", None, 80_000_000.0);
        let eval1 = pattern.process_state_sample(&rule, &mut state, &event1, now);
        assert!(matches!(eval1.base_severity, Some(Severity::High)));

        // Simulate state update (process_event would do this)
        state.last_severity = Some("high".to_string());
        state.last_drop_pct = Some(eval1.selected_drop_pct);

        // Second evaluation 4 minutes later: the original sample (now-7min) ages
        // out of the 10-min window relative to now2 (now-7min = now2-11min).
        // Remaining window: [now: 80M, now2: 50M]  →  37.5% drop in 4 min → CRITICAL.
        let now2 = now + Duration::minutes(4);
        let event2 = state_event("aave_v3", "base", None, 50_000_000.0);
        let eval2 = pattern.process_state_sample(&rule, &mut state, &event2, now2);
        assert!(matches!(eval2.base_severity, Some(Severity::Critical)));

        // clamp_severity should allow the escalation
        let clamped = clamp_severity(Severity::Critical, Some(&Severity::High));
        assert!(matches!(clamped, Severity::Critical));
    }

    /// TC-ESC-02: severity never de-escalates (clamp_severity).
    #[test]
    fn severity_never_deescalates() {
        // HIGH when previous was CRITICAL → stays CRITICAL
        assert!(matches!(
            clamp_severity(Severity::High, Some(&Severity::Critical)),
            Severity::Critical
        ));
        // CRITICAL stays CRITICAL
        assert!(matches!(
            clamp_severity(Severity::Critical, Some(&Severity::Critical)),
            Severity::Critical
        ));
        // No previous → use current
        assert!(matches!(
            clamp_severity(Severity::High, None),
            Severity::High
        ));
        // Escalation works normally
        assert!(matches!(
            clamp_severity(Severity::Critical, Some(&Severity::High)),
            Severity::Critical
        ));
    }

    // ───────────────────────────────────────────────────────────────────────
    // 11. Special Cases — TC-SPEC-01, TC-SPEC-02
    // ───────────────────────────────────────────────────────────────────────

    /// TC-SPEC-01: protocol pause WITHOUT TVL drop → standalone PROTOCOL_PAUSED alert.
    #[test]
    fn pause_without_tvl_drop_fires_protocol_paused() {
        let event = unified_pause_event("euler_v2", true);
        let mut rule = base_rule();
        rule.protocol_id = "euler_v2".to_string();

        let pause = pause_event_data("euler_v2", "base", true);
        let context = PauseDetectionContext {
            subject: DetectionSubject {
                subject_type: "protocol",
                subject_key: "euler_v2:base",
            },
            severity: Severity::High,
            transition: IncidentTransition::Trigger,
            classification: ContextClassification::None,
            now: Utc::now(),
        };

        let detection = TvlDropPattern::build_pause_detection(&event, &rule, &context, &pause);
        assert!(matches!(detection.severity, Severity::High));
        assert!(matches!(
            detection.incident_transition,
            Some(IncidentTransition::Trigger)
        ));
        assert_eq!(
            detection.signals[0].signal_type,
            SignalType::ProtocolPauseState
        );
        assert_eq!(detection.signals[0].value, 1.0); // paused
        assert!(detection.description.as_deref().unwrap().contains("paused"));
    }

    /// TC-SPEC-02: protocol pause DURING active TVL drop → annotate incident.
    #[test]
    fn pause_during_active_tvl_drop_annotates_incident() {
        let event = unified_pause_event("euler_v2", true);
        let mut rule = base_rule();
        rule.protocol_id = "euler_v2".to_string();

        let pause = pause_event_data("euler_v2", "base", true);
        let context = PauseDetectionContext {
            subject: DetectionSubject {
                subject_type: "protocol",
                subject_key: "euler_v2:base",
            },
            severity: Severity::Critical, // previous severity of active incident
            transition: IncidentTransition::Update,
            classification: ContextClassification::Isolated,
            now: Utc::now(),
        };

        let detection = TvlDropPattern::build_pause_detection(&event, &rule, &context, &pause);
        assert!(matches!(detection.severity, Severity::Critical));
        assert!(matches!(
            detection.incident_transition,
            Some(IncidentTransition::Update)
        ));
        assert_eq!(
            detection.oracle_context.get("pause_state"),
            Some(&json!("paused"))
        );
        assert!(detection
            .description
            .as_deref()
            .unwrap()
            .contains("TVL-drop incident remains active"));
    }

    // ───────────────────────────────────────────────────────────────────────
    // 14. No-Alert Scenarios — TC-NONE-02, TC-NONE-03
    // ───────────────────────────────────────────────────────────────────────

    /// TC-NONE-02: flash loan noise — end-of-block TVL unchanged → no alert.
    #[test]
    fn tvl_unchanged_no_alert() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(10), 300_000_000.0));
        let event = state_event("aave_v3", "base", None, 300_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(eval.base_severity.is_none());
        assert_eq!(eval.fast_drop_pct, 0.0);
    }

    /// TC-NONE-03: small organic withdrawal (3%) — below all thresholds.
    #[test]
    fn small_organic_withdrawal_no_alert() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(10), 100_000_000.0));
        let event = state_event("aave_v3", "base", None, 97_000_000.0);

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(eval.base_severity.is_none());
        assert!(eval.fast_drop_pct < rule.fast_drop_pct);
    }

    // ───────────────────────────────────────────────────────────────────────
    // 8. Incident Creation & Deduplication — TC-INC-01, TC-INC-02, TC-INC-03
    // ───────────────────────────────────────────────────────────────────────

    /// TC-INC-01: duplicate detection signal — no new incident.
    /// When an active incident exists (previous severity set) and the new
    /// evaluation has the same severity + similar drop %, no transition is
    /// produced and therefore no duplicate incident is created.
    #[test]
    fn duplicate_signal_same_severity_no_new_trigger() {
        // Same severity (High), similar drop (20% vs 20%), same context → None.
        let t = determine_transition(
            Some(&Severity::High),
            &Severity::High,
            Some(&Severity::High),
            20.0,
            20.0,
            "isolated",
            "isolated",
        );
        assert!(t.is_none(), "duplicate should produce no transition");

        // Drop increased by <1% → still no transition.
        let t2 = determine_transition(
            Some(&Severity::High),
            &Severity::High,
            Some(&Severity::High),
            20.5,
            20.0,
            "isolated",
            "isolated",
        );
        assert!(t2.is_none(), "<1% increase should not trigger update");

        // Drop increased by >=1% → Update (not a new Trigger).
        let t3 = determine_transition(
            Some(&Severity::High),
            &Severity::High,
            Some(&Severity::High),
            22.0,
            20.0,
            "isolated",
            "isolated",
        );
        assert!(matches!(t3, Some(IncidentTransition::Update)));
    }

    /// TC-INC-02: different protocol → new incident created (different subject).
    #[test]
    fn different_protocol_produces_different_subject() {
        let rule_aave = base_rule();
        let mut rule_euler = base_rule();
        rule_euler.protocol_id = "euler_v2".to_string();

        let (_, key_aave, _) = rule_aave.subject_for_event(None).unwrap();
        let (_, key_euler, _) = rule_euler.subject_for_event(None).unwrap();

        assert_ne!(key_aave, key_euler);
        assert_eq!(key_aave, "aave_v3:base");
        assert_eq!(key_euler, "euler_v2:base");
    }

    /// TC-INC-03: incident fields set correctly at creation.
    #[test]
    fn detection_fields_correct_at_creation() {
        let event = unified_tvl_event();
        let rule = base_rule();
        let evaluation = TvlEvaluation {
            fast_drop_pct: 25.0,
            slow_drop_pct: 10.0,
            velocity_drop_pct: 12.0,
            selected_drop_pct: 25.0,
            fast_window_reference_tvl_usd: Some(500_000_000.0),
            time_to_reach_current_drop_minutes: Some(4.0),
            drain_rate_usd_per_min: Some(31_250_000.0),
            estimated_time_to_empty_minutes: Some(12.0),
            velocity_pattern: Some("ACCELERATING".to_string()),
            base_severity: Some(Severity::Critical),
            breached_branches: vec!["fast".to_string()],
            deposit_gate_skipped: false,
            position_data_stale: false,
        };
        let sample_evt = state_event("aave_v3", "base", None, 375_000_000.0);
        let context = TvlDetectionContext {
            subject: DetectionSubject {
                subject_type: "protocol",
                subject_key: "aave_v3:base",
            },
            severity: Severity::Critical,
            transition: IncidentTransition::Trigger,
            classification: ContextClassification::Isolated,
            now: Utc::now(),
        };

        let det =
            TvlDropPattern::build_tvl_detection(&event, &rule, &context, &evaluation, &sample_evt);
        // Spec Section 7 Stage 1 — required fields.
        assert_eq!(det.pattern_id, "tvl_drop");
        assert!(matches!(det.severity, Severity::Critical));
        assert!(matches!(det.lifecycle_state, LifecycleState::Confirmed));
        assert!(matches!(
            det.incident_transition,
            Some(IncidentTransition::Trigger)
        ));
        assert!(matches!(
            det.attack_family,
            AttackFamily::LiquidationCascade
        ));
        assert_eq!(det.protocol, "aave_v3");
        assert_eq!(det.subject_key.as_deref(), Some("aave_v3:base"));
        assert_eq!(det.tenant_id.as_deref(), Some("tenant-a"));
    }

    // ───────────────────────────────────────────────────────────────────────
    // 9. Escalation — TC-ESC-03
    // ───────────────────────────────────────────────────────────────────────

    /// TC-ESC-03: new markets dropping → separate detection produced.
    /// Each market-scoped rule produces an independent detection with its
    /// own subject_key.  Scope expansion to a shared incident is handled
    /// downstream by the orchestrator.
    #[test]
    fn new_market_dropping_produces_independent_detection() {
        let now = Utc::now();
        let pattern = TvlDropPattern::default();
        let mut rule_usdc = base_rule();
        rule_usdc.scope = "market".to_string();
        rule_usdc.market_id = Some("usdc".to_string());
        let mut rule_weth = base_rule();
        rule_weth.scope = "market".to_string();
        rule_weth.market_id = Some("weth".to_string());

        let (_, key_usdc, _) = rule_usdc.subject_for_event(Some("usdc")).unwrap();
        let (_, key_weth, _) = rule_weth.subject_for_event(Some("weth")).unwrap();
        assert_ne!(key_usdc, key_weth);

        // Both markets drop → independent evaluations.
        let mut state_usdc = TvlRuleState::default();
        state_usdc
            .samples
            .push(sample(now - Duration::minutes(4), 200_000_000.0));
        let evt_usdc = state_event("aave_v3", "base", Some("usdc"), 150_000_000.0);
        let eval_usdc = pattern.process_state_sample(&rule_usdc, &mut state_usdc, &evt_usdc, now);

        let mut state_weth = TvlRuleState::default();
        state_weth
            .samples
            .push(sample(now - Duration::minutes(4), 100_000_000.0));
        let evt_weth = state_event("aave_v3", "base", Some("weth"), 75_000_000.0);
        let eval_weth = pattern.process_state_sample(&rule_weth, &mut state_weth, &evt_weth, now);

        assert!(eval_usdc.base_severity.is_some());
        assert!(eval_weth.base_severity.is_some());
    }

    // ───────────────────────────────────────────────────────────────────────
    // 10. Resolution — TC-RES-01, TC-RES-06
    // ───────────────────────────────────────────────────────────────────────

    /// TC-RES-01: no automatic resolution for TVL drops.
    /// When TVL fully recovers, the detector produces no transition — the
    /// incident remains ACTIVE and can only be resolved by an operator.
    #[test]
    fn no_automatic_resolution_on_tvl_recovery() {
        let now = Utc::now();
        let pattern = TvlDropPattern::default();
        let rule = base_rule();

        // Active incident at CRITICAL after 25% drop.
        let mut state = TvlRuleState {
            last_severity: Some("critical".to_string()),
            last_drop_pct: Some(25.0),
            ..Default::default()
        };
        state
            .samples
            .push(sample(now - Duration::minutes(10), 500_000_000.0));
        state
            .samples
            .push(sample(now - Duration::minutes(5), 375_000_000.0));

        // TVL recovers to pre-drop levels.
        let event = state_event("aave_v3", "base", None, 500_000_000.0);
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);

        // base_severity is None → no transition → no auto-resolution.
        assert!(eval.base_severity.is_none());
        let t = determine_transition(
            eval.base_severity.as_ref(),
            &Severity::Info,
            Some(&Severity::Critical),
            eval.selected_drop_pct,
            25.0,
            "isolated",
            "none",
        );
        assert!(t.is_none(), "must NOT auto-resolve");
    }

    /// TC-RES-06: protocol unpause → notification but no auto-resolve.
    #[test]
    fn unpause_notification_no_auto_resolve() {
        let event = unified_pause_event("euler_v2", false); // unpause
        let mut rule = base_rule();
        rule.protocol_id = "euler_v2".to_string();

        let pause = pause_event_data("euler_v2", "base", false);
        // Previous incident at CRITICAL (PAUSED state).
        let context = PauseDetectionContext {
            subject: DetectionSubject {
                subject_type: "protocol",
                subject_key: "euler_v2:base",
            },
            severity: Severity::Critical,
            transition: IncidentTransition::Update, // NOT Resolve
            classification: ContextClassification::Isolated,
            now: Utc::now(),
        };

        let det = TvlDropPattern::build_pause_detection(&event, &rule, &context, &pause);
        // Must be Update, NOT Resolve.
        assert!(matches!(
            det.incident_transition,
            Some(IncidentTransition::Update)
        ));
        assert_eq!(
            det.oracle_context.get("pause_state"),
            Some(&json!("unpaused"))
        );
        // Severity remains from previous incident, not auto-resolved.
        assert!(matches!(det.severity, Severity::Critical));
    }

    // ───────────────────────────────────────────────────────────────────────
    // 7. Contagion — TC-CONT-01
    // ───────────────────────────────────────────────────────────────────────

    /// TC-CONT-01: same-protocol contagion flags all markets.
    /// When a market-scoped rule fires on one market, other monitored
    /// markets on the same protocol are listed as at-risk.
    #[test]
    fn same_protocol_contagion_flags_at_risk_markets() {
        let mut pattern = TvlDropPattern::default();
        let mut rule_usdc = base_rule();
        rule_usdc.rule_id = "tvl-usdc".to_string();
        rule_usdc.scope = "market".to_string();
        rule_usdc.market_id = Some("usdc".to_string());
        let mut rule_weth = base_rule();
        rule_weth.rule_id = "tvl-weth".to_string();
        rule_weth.scope = "market".to_string();
        rule_weth.market_id = Some("weth".to_string());

        pattern
            .configs
            .insert("tenant-a".to_string(), vec![rule_usdc, rule_weth]);

        // USDC drops → WETH should be listed as at-risk.
        let at_risk = pattern.find_at_risk_markets("tenant-a", "aave_v3", "base", Some("usdc"));
        assert_eq!(at_risk, vec!["aave_v3:base:weth"]);

        // From WETH's perspective → USDC is at-risk.
        let at_risk2 = pattern.find_at_risk_markets("tenant-a", "aave_v3", "base", Some("weth"));
        assert_eq!(at_risk2, vec!["aave_v3:base:usdc"]);
    }

    // ───────────────────────────────────────────────────────────────────────
    // 6. Tenant Isolation — TC-TENANT-02
    // ───────────────────────────────────────────────────────────────────────

    /// TC-TENANT-02: same TVL data, independent evaluation per tenant.
    /// Two tenants with different thresholds receive the same TVL sample
    /// but evaluate independently (one alerts, one doesn't).
    #[test]
    fn same_tvl_data_evaluated_independently_per_tenant() {
        let now = Utc::now();
        let pattern = TvlDropPattern::default();
        let event = state_event("aave_v3", "base", None, 80_000_000.0);

        // Tenant A: conservative (15% threshold) → 20% drop → fires.
        let mut rule_a = base_rule();
        rule_a.fast_drop_pct = 15.0;
        let mut state_a = TvlRuleState::default();
        state_a
            .samples
            .push(sample(now - Duration::minutes(5), 100_000_000.0));

        // Tenant B: tolerant (25% threshold) → 20% drop → doesn't fire.
        let mut rule_b = base_rule();
        rule_b.fast_drop_pct = 25.0;
        let mut state_b = TvlRuleState::default();
        state_b
            .samples
            .push(sample(now - Duration::minutes(5), 100_000_000.0));

        let eval_a = pattern.process_state_sample(&rule_a, &mut state_a, &event, now);
        let eval_b = pattern.process_state_sample(&rule_b, &mut state_b, &event, now);

        assert!(eval_a.base_severity.is_some(), "Tenant A should fire");
        assert!(eval_b.base_severity.is_none(), "Tenant B should not fire");
        // Tenant A's result has no effect on Tenant B's state.
        assert!(state_b.last_severity.is_none());
    }

    // ───────────────────────────────────────────────────────────────────────
    // 12. Reorg Handling — TC-REORG-01 to TC-REORG-05
    // ───────────────────────────────────────────────────────────────────────

    /// TC-REORG-01: retraction — TVL drop only on orphaned fork.
    /// Orphaned fork showed 30% drop → CRITICAL, canonical chain shows no drop.
    /// Correction type = RETRACTION.
    #[test]
    fn reorg_retraction_no_drop_on_canonical() {
        let now = Utc::now();
        let pattern = TvlDropPattern::default();
        let rule = base_rule();

        // Orphaned fork evaluation: 30% drop → CRITICAL.
        let mut state_orphaned = TvlRuleState::default();
        state_orphaned
            .samples
            .push(sample(now - Duration::minutes(3), 500_000_000.0));
        let orphaned_evt = state_event("aave_v3", "base", None, 350_000_000.0);
        let eval_orphaned =
            pattern.process_state_sample(&rule, &mut state_orphaned, &orphaned_evt, now);
        assert!(matches!(
            eval_orphaned.base_severity,
            Some(Severity::Critical)
        ));

        // Canonical chain: TVL unchanged.
        let mut state_canonical = TvlRuleState::default();
        state_canonical
            .samples
            .push(sample(now - Duration::minutes(3), 500_000_000.0));
        let canonical_evt = state_event("aave_v3", "base", None, 500_000_000.0);
        let eval_canonical =
            pattern.process_state_sample(&rule, &mut state_canonical, &canonical_evt, now);
        assert!(eval_canonical.base_severity.is_none());

        let correction = determine_reorg_correction(
            eval_orphaned.base_severity.as_ref(),
            eval_canonical.base_severity.as_ref(),
        );
        assert_eq!(correction, Some("RETRACTION"));
    }

    /// TC-REORG-02: severity update — different data on canonical chain.
    /// Orphaned: 20% drop → HIGH.  Canonical: 30% drop → CRITICAL.
    #[test]
    fn reorg_severity_update_canonical_different() {
        let now = Utc::now();
        let pattern = TvlDropPattern::default();
        let rule = base_rule();

        // Orphaned: 20% drop, 7 min → HIGH.
        let mut s1 = TvlRuleState::default();
        s1.samples
            .push(sample(now - Duration::minutes(7), 100_000_000.0));
        let e1 = state_event("aave_v3", "base", None, 80_000_000.0);
        let ev1 = pattern.process_state_sample(&rule, &mut s1, &e1, now);
        assert!(matches!(ev1.base_severity, Some(Severity::High)));

        // Canonical: 30% drop, 3 min → CRITICAL.
        let mut s2 = TvlRuleState::default();
        s2.samples
            .push(sample(now - Duration::minutes(3), 100_000_000.0));
        let e2 = state_event("aave_v3", "base", None, 70_000_000.0);
        let ev2 = pattern.process_state_sample(&rule, &mut s2, &e2, now);
        assert!(matches!(ev2.base_severity, Some(Severity::Critical)));

        let correction =
            determine_reorg_correction(ev1.base_severity.as_ref(), ev2.base_severity.as_ref());
        assert_eq!(correction, Some("UPDATE"));
    }

    /// TC-REORG-03: late alert — drop only exists on canonical chain.
    /// No detection on orphaned fork; canonical block shows 25% TVL drop.
    #[test]
    fn reorg_late_alert_drop_only_on_canonical() {
        let correction = determine_reorg_correction(
            None,                      // orphaned: no detection
            Some(&Severity::Critical), // canonical: CRITICAL
        );
        assert_eq!(correction, Some("LATE_ALERT"));
    }

    /// TC-REORG-04: no correction — same severity on both chains.
    #[test]
    fn reorg_no_correction_same_severity() {
        let correction =
            determine_reorg_correction(Some(&Severity::Critical), Some(&Severity::Critical));
        assert!(correction.is_none(), "same severity → no correction");

        // Both None → also no correction.
        let correction2 = determine_reorg_correction(None, None);
        assert!(correction2.is_none());
    }

    /// TC-REORG-05: alert type is PROTOCOL_EXPLOIT (AttackFamily::LiquidationCascade),
    /// not STABLECOIN_DEPEG (AttackFamily::PegDeviation).
    #[test]
    fn reorg_uses_protocol_exploit_not_depeg() {
        let event = unified_tvl_event();
        let rule = base_rule();
        let evaluation = TvlEvaluation {
            fast_drop_pct: 25.0,
            slow_drop_pct: 0.0,
            velocity_drop_pct: 0.0,
            selected_drop_pct: 25.0,
            fast_window_reference_tvl_usd: Some(500_000_000.0),
            time_to_reach_current_drop_minutes: Some(4.0),
            drain_rate_usd_per_min: None,
            estimated_time_to_empty_minutes: None,
            velocity_pattern: None,
            base_severity: Some(Severity::Critical),
            breached_branches: vec!["fast".to_string()],
            deposit_gate_skipped: false,
            position_data_stale: false,
        };
        let sample_evt = state_event("aave_v3", "base", None, 375_000_000.0);
        let context = TvlDetectionContext {
            subject: DetectionSubject {
                subject_type: "protocol",
                subject_key: "aave_v3:base",
            },
            severity: Severity::Critical,
            transition: IncidentTransition::Trigger,
            classification: ContextClassification::Isolated,
            now: Utc::now(),
        };

        let det =
            TvlDropPattern::build_tvl_detection(&event, &rule, &context, &evaluation, &sample_evt);
        // TVL drops use LiquidationCascade (PROTOCOL_EXPLOIT), not PegDeviation.
        assert!(matches!(
            det.attack_family,
            AttackFamily::LiquidationCascade
        ));
        assert!(!matches!(det.attack_family, AttackFamily::PegDeviation));
    }
}
