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
use state_manager::PostgresRepository;
use uuid::Uuid;

use super::DetectionPattern;

pub const PATTERN_ID: &str = "tvl_drop";

const DEFAULT_FAST_DROP_PCT: f64 = 20.0;
const DEFAULT_FAST_WINDOW_MINUTES: i64 = 10;
const DEFAULT_SLOW_DROP_PCT: f64 = 35.0;
const DEFAULT_SLOW_WINDOW_MINUTES: i64 = 60;
const DEFAULT_VELOCITY_CRITICAL_PCT: f64 = 15.0;
const DEFAULT_VELOCITY_WINDOW_MINUTES: i64 = 2;
const DEFAULT_CONCURRENT_WINDOW_MINUTES: i64 = 5;
const DEFAULT_MIN_TVL_FLOOR_USD: f64 = 1_000_000.0;

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
    base_severity: Option<Severity>,
    breached_branches: Vec<String>,
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
    classification: ContextClassification,
    now: DateTime<Utc>,
}

#[derive(Default)]
pub struct TvlDropPattern {
    // tenant_id -> rules
    configs: HashMap<String, Vec<TvlDropRule>>,
    // `${tenant_id}:${rule_id}:${subject_key}` -> state
    state_cache: HashMap<String, TvlRuleState>,
}

impl TvlDropPattern {
    fn load_state_key(rule_id: &str, subject_key: &str) -> String {
        format!("{rule_id}:{subject_key}")
    }

    fn cache_key(tenant_id: &str, load_state_key: &str) -> String {
        format!("{tenant_id}:{load_state_key}")
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

        let fast_drop = window_drop_pct(&state.samples, now, rule.fast_window_minutes);
        let slow_drop = window_drop_pct(&state.samples, now, rule.slow_window_minutes);
        let velocity_drop = window_drop_pct(&state.samples, now, rule.velocity_critical_minutes);
        let selected_drop_pct = fast_drop.max(slow_drop).max(velocity_drop);

        let mut breached_branches = Vec::new();
        let mut base_severity = None;
        if sample.tvl_usd >= rule.min_tvl_floor_usd {
            if velocity_drop >= rule.velocity_critical_pct {
                base_severity = Some(Severity::Critical);
                breached_branches.push("velocity".to_string());
            } else {
                if fast_drop >= rule.fast_drop_pct {
                    breached_branches.push("fast".to_string());
                }
                if slow_drop >= rule.slow_drop_pct {
                    breached_branches.push("slow".to_string());
                }
                if !breached_branches.is_empty() {
                    base_severity = Some(Severity::High);
                }
            }
        }

        TvlEvaluation {
            fast_drop_pct: fast_drop,
            slow_drop_pct: slow_drop,
            velocity_drop_pct: velocity_drop,
            selected_drop_pct,
            base_severity,
            breached_branches,
        }
    }

    fn build_tvl_detection(
        event: &UnifiedEvent,
        rule: &TvlDropRule,
        context: &TvlDetectionContext<'_>,
        evaluation: &TvlEvaluation,
        sample: &TvlStateEvent,
    ) -> DetectionResult {
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
        oracle_context.insert(
            "breached_branches".to_string(),
            json!(evaluation.breached_branches),
        );
        oracle_context.insert(
            "transition".to_string(),
            json!(incident_transition_str(&context.transition)),
        );

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
            created_at: context.now,
        }
    }

    fn build_pause_detection(
        event: &UnifiedEvent,
        rule: &TvlDropRule,
        context: &PauseDetectionContext<'_>,
        pause: &TvlPauseEvent,
    ) -> DetectionResult {
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
            description: Some(format!(
                "Protocol {} is {} while a TVL-drop incident remains active.",
                pause.protocol_id, state_label
            )),
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
            incident_transition: Some(IncidentTransition::Update),
            context_classification: Some(context.classification.clone()),
            confidence_breakdown: HashMap::new(),
            oracle_context,
            actions_recommended: vec![
                "Confirm protocol control-plane status with protocol operators.".to_string(),
                "Keep incident open until manual resolution criteria are met.".to_string(),
            ],
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
        for ((tenant_id, pattern_id), config) in config_map {
            if pattern_id != PATTERN_ID {
                continue;
            }
            let rules = parse_tvl_drop_rules(config, tenant_id);
            next.insert(tenant_id.clone(), rules);
        }
        self.configs = next;
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
        let Some(rules) = self.configs.get(&event.tenant_id).cloned() else {
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
                if !self.state_cache.contains_key(&cache_key) {
                    let loaded = repo
                        .load_pattern_state(&event.tenant_id, PATTERN_ID, &state_key)
                        .await?
                        .and_then(|value| serde_json::from_value::<TvlRuleState>(value).ok())
                        .unwrap_or_default();
                    self.state_cache.insert(cache_key.clone(), loaded);
                }

                let mut state = self
                    .state_cache
                    .get(&cache_key)
                    .cloned()
                    .unwrap_or_default();
                state.protocol_chain_key = Some(protocol_chain_key.clone());
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
                        if systemic {
                            (Severity::Critical, ContextClassification::Systemic)
                        } else {
                            (base_severity, ContextClassification::Isolated)
                        }
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
                    "min_tvl_floor_usd": rule.min_tvl_floor_usd,
                });
                let severity_str = if evaluation.base_severity.is_some() {
                    Some(format!("{severity:?}").to_ascii_lowercase())
                } else {
                    None
                };
                let _ = repo
                    .insert_pattern_snapshot(
                        &event.tenant_id,
                        PATTERN_ID,
                        &state_key,
                        snapshot,
                        Some(evaluation.selected_drop_pct),
                        severity_str.as_deref(),
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
                self.state_cache.insert(cache_key.clone(), state);

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
                    let detection = Self::build_tvl_detection(
                        event,
                        rule,
                        &detection_context,
                        &evaluation,
                        &sample,
                    );
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
                if !self.state_cache.contains_key(&cache_key) {
                    let loaded = repo
                        .load_pattern_state(&event.tenant_id, PATTERN_ID, &state_key)
                        .await?
                        .and_then(|value| serde_json::from_value::<TvlRuleState>(value).ok())
                        .unwrap_or_default();
                    self.state_cache.insert(cache_key.clone(), loaded);
                }
                let mut state = self
                    .state_cache
                    .get(&cache_key)
                    .cloned()
                    .unwrap_or_default();
                state.protocol_chain_key = Some(protocol_chain_key);

                let Some(previous_severity) = severity_from_str(state.last_severity.as_deref())
                else {
                    continue;
                };

                let classification = match state.last_context.as_deref() {
                    Some("systemic") => ContextClassification::Systemic,
                    Some("isolated") => ContextClassification::Isolated,
                    _ => ContextClassification::None,
                };
                state.last_pause_state = Some(pause.paused);
                state.last_transition_at = Some(now);

                let snapshot = json!({
                    "rule_id": rule.rule_id,
                    "subject_key": subject_key,
                    "pause_state": if pause.paused { "paused" } else { "unpaused" },
                    "incident_transition": "update",
                    "context_classification": context_classification_str(&classification),
                });
                let _ = repo
                    .insert_pattern_snapshot(
                        &event.tenant_id,
                        PATTERN_ID,
                        &state_key,
                        snapshot,
                        state.last_drop_pct,
                        state.last_severity.as_deref(),
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

                let detection_context = PauseDetectionContext {
                    subject: DetectionSubject {
                        subject_type: &subject_type,
                        subject_key: &subject_key,
                    },
                    severity: previous_severity,
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

fn window_drop_pct(samples: &[TvlSamplePoint], now: DateTime<Utc>, window_minutes: i64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let cutoff = now - Duration::minutes(window_minutes.max(1));
    let mut max_tvl = None::<f64>;
    let mut current_tvl = None::<f64>;
    for point in samples.iter().filter(|point| point.observed_at >= cutoff) {
        max_tvl = Some(max_tvl.unwrap_or(point.tvl_usd).max(point.tvl_usd));
        current_tvl = Some(point.tvl_usd);
    }

    let Some(window_peak) = max_tvl else {
        return 0.0;
    };
    let Some(window_current) = current_tvl else {
        return 0.0;
    };
    if !window_peak.is_finite() || window_peak <= 0.0 {
        return 0.0;
    }
    (((window_peak - window_current).max(0.0) / window_peak) * 100.0).max(0.0)
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
            tracing::warn!(
                tenant_id = %tenant_id,
                rule_id = %rule.rule_id,
                error = ?error,
                "invalid tvl_drop rule; skipping"
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

    #[test]
    fn velocity_branch_sets_critical() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let rule = base_rule();
        state
            .samples
            .push(sample(now - Duration::minutes(1), 100_000_000.0));
        let event = TvlStateEvent {
            protocol_id: "aave_v3".to_string(),
            chain_slug: "base".to_string(),
            market_id: None,
            tvl_usd: 80_000_000.0,
            block_number: 1,
            tx_hash: None,
        };

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(matches!(eval.base_severity, Some(Severity::Critical)));
        assert!(eval.velocity_drop_pct >= rule.velocity_critical_pct);
    }

    #[test]
    fn floor_gate_blocks_detection() {
        let now = Utc::now();
        let mut state = TvlRuleState::default();
        let mut rule = base_rule();
        rule.min_tvl_floor_usd = 5_000_000.0;
        state
            .samples
            .push(sample(now - Duration::minutes(3), 6_000_000.0));
        let event = TvlStateEvent {
            protocol_id: "aave_v3".to_string(),
            chain_slug: "base".to_string(),
            market_id: None,
            tvl_usd: 4_000_000.0,
            block_number: 1,
            tx_hash: None,
        };

        let pattern = TvlDropPattern::default();
        let eval = pattern.process_state_sample(&rule, &mut state, &event, now);
        assert!(eval.base_severity.is_none());
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
}
