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
    AttackFamily, Chain, DetectionResult, DetectionSignal, LifecycleState, RiskScore, Severity,
    SignalType, UnifiedEvent,
};
use serde::{Deserialize, Serialize};
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
    pub source_filter: DpegSourceFilter,
    #[serde(default)]
    pub toggles: DpegToggles,
    #[serde(default)]
    pub source_overrides: HashMap<String, DpegSourceOverride>,
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
        let outcome = evaluate_policy(&policy, &quotes, &current_state, now)?;

        // Persist snapshot regardless of whether an alert fired.
        let snapshot_data = serde_json::json!({
            "weighted_median_price": outcome.snapshot.weighted_median_price,
            "divergence_pct": outcome.snapshot.divergence_pct,
            "source_count": outcome.snapshot.source_count,
            "quorum_met": outcome.snapshot.quorum_met,
            "breach_active": outcome.snapshot.breach_active,
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
            if let Some(severity) = outcome.snapshot.severity.clone() {
                return Ok(Some(build_detection(&policy, &outcome.snapshot, severity, now)));
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
    severity: Option<Severity>,
}

struct EvaluationOutcome {
    snapshot: ConsensusSnapshot,
    should_emit_alert: bool,
    next_state: DpegAlertState,
}

fn evaluate_policy(
    policy: &DpegPolicy,
    quotes: &[QuoteInput],
    current_state: &DpegAlertState,
    now: DateTime<Utc>,
) -> Result<EvaluationOutcome> {
    let mut weighted_points = Vec::<(f64, f64)>::new();
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
    }

    if weighted_points.is_empty() {
        return Ok(EvaluationOutcome {
            snapshot: ConsensusSnapshot {
                weighted_median_price: policy.peg_target,
                divergence_pct: 0.0,
                source_count: 0,
                eligible_source_count: 0,
                quorum_met: false,
                breach_active: false,
                severity: None,
            },
            should_emit_alert: false,
            next_state: DpegAlertState::default(),
        });
    }

    let weighted_median_price =
        weighted_median(&weighted_points).ok_or_else(|| anyhow!("weighted median failed"))?;
    let divergence_pct =
        ((weighted_median_price - policy.peg_target).abs() / policy.peg_target) * 100.0;
    let severity = severity_for_divergence(divergence_pct, &policy.severity_bands);

    let source_count = weighted_points.len();
    let enabled_source_count = policy.enabled_source_count().max(1);
    let min_healthy = policy
        .source_filter
        .min_healthy_sources
        .max(policy.min_sources)
        .max(1);
    let source_ratio = source_count as f64 / enabled_source_count as f64;
    let quorum_met = source_count >= min_healthy && source_ratio >= policy.quorum_pct;
    let breach_active = quorum_met && severity.is_some();

    let mut next_state = current_state.clone();
    let mut should_emit_alert = false;

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
        let previous_rank = severity_rank_str(next_state.last_severity.as_deref());
        let severity_escalated = current_rank > previous_rank;

        if sustained && (!cooldown_active || severity_escalated) {
            should_emit_alert = true;
            next_state.last_alerted_at = Some(now);
            next_state.last_divergence_pct = Some(divergence_pct);
            next_state.last_severity = severity
                .as_ref()
                .map(|s| format!("{:?}", s).to_lowercase());
            next_state.cooldown_until =
                Some(now + Duration::seconds(policy.cooldown_sec));
        }
    } else {
        next_state.breach_started_at = None;
        next_state.last_divergence_pct = Some(divergence_pct);
    }

    Ok(EvaluationOutcome {
        snapshot: ConsensusSnapshot {
            weighted_median_price,
            divergence_pct,
            source_count,
            eligible_source_count: enabled_source_count,
            quorum_met,
            breach_active,
            severity,
        },
        should_emit_alert,
        next_state,
    })
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

fn severity_rank_str(s: Option<&str>) -> u8 {
    match s {
        Some("critical") => 5,
        Some("high") => 4,
        Some("medium") => 3,
        Some("low") => 2,
        Some("info") => 1,
        _ => 0,
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
        risk_score: RiskScore::default(),
        oracle_context: std::collections::HashMap::new(),
        actions_recommended: vec![],
        created_at: now,
    }
}
