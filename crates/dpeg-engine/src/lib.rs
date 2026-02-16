use std::collections::HashMap;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use event_schema::Severity;
use serde::{Deserialize, Serialize};

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
    pub severity_bands: DpegSeverityBands,
    #[serde(default)]
    pub source_overrides: HashMap<String, DpegSourceOverride>,
}

impl DpegPolicy {
    pub fn validate(&self) -> Result<()> {
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
        if !(self.severity_bands.medium < self.severity_bands.high
            && self.severity_bands.high < self.severity_bands.critical)
        {
            return Err(anyhow!(
                "severity bands must satisfy medium < high < critical"
            ));
        }

        Ok(())
    }

    fn source_enabled(&self, source_id: &str) -> bool {
        self.source_overrides
            .get(source_id)
            .map(|value| value.enabled)
            .unwrap_or(true)
    }

    fn source_weight(&self, source_id: &str) -> f64 {
        self.source_overrides
            .get(source_id)
            .map(|value| value.weight)
            .unwrap_or(1.0)
            .max(0.0)
    }

    fn source_stale_timeout_ms(&self, source_id: &str) -> i64 {
        self.source_overrides
            .get(source_id)
            .and_then(|value| value.stale_timeout_ms)
            .unwrap_or(self.stale_timeout_ms)
            .max(1)
    }

    fn enabled_source_count(&self) -> usize {
        let configured_enabled = self
            .source_overrides
            .values()
            .filter(|value| value.enabled)
            .count();
        if configured_enabled == 0 {
            self.min_sources
        } else {
            configured_enabled
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuoteInput {
    pub source_id: String,
    pub price: f64,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct DpegAlertState {
    pub breach_started_at: Option<DateTime<Utc>>,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_alerted_at: Option<DateTime<Utc>>,
    pub last_divergence_pct: Option<f64>,
    pub last_severity: Option<Severity>,
}

#[derive(Debug, Clone)]
pub struct ConsensusSnapshot {
    pub weighted_median_price: f64,
    pub divergence_pct: f64,
    pub source_count: usize,
    pub quorum_met: bool,
    pub breach_active: bool,
    pub severity: Option<Severity>,
}

#[derive(Debug, Clone)]
pub struct EvaluationOutcome {
    pub snapshot: ConsensusSnapshot,
    pub should_emit_alert: bool,
    pub next_state: DpegAlertState,
}

pub fn evaluate_policy(
    policy: &DpegPolicy,
    quotes: &[QuoteInput],
    current_state: &DpegAlertState,
    now: DateTime<Utc>,
) -> Result<EvaluationOutcome> {
    policy.validate()?;

    let mut weighted_points = Vec::<(f64, f64)>::new();
    for quote in quotes {
        if !policy.source_enabled(&quote.source_id) {
            continue;
        }
        if !(quote.price.is_finite() && quote.price > 0.0) {
            continue;
        }

        let stale_ms = policy.source_stale_timeout_ms(&quote.source_id);
        let age_ms = now.signed_duration_since(quote.observed_at).num_milliseconds();
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
                quorum_met: false,
                breach_active: false,
                severity: None,
            },
            should_emit_alert: false,
            next_state: DpegAlertState::default(),
        });
    }

    let Some(weighted_median_price) = weighted_median(&weighted_points) else {
        return Err(anyhow!("failed to calculate weighted median"));
    };
    let divergence_pct = ((weighted_median_price - policy.peg_target).abs() / policy.peg_target) * 100.0;
    let severity = severity_for_divergence(divergence_pct, &policy.severity_bands);

    let source_count = weighted_points.len();
    let enabled_source_count = policy.enabled_source_count().max(1);
    let source_ratio = source_count as f64 / enabled_source_count as f64;
    let quorum_met = source_count >= policy.min_sources && source_ratio >= policy.quorum_pct;
    let breach_active = quorum_met && severity.is_some();

    let mut next_state = current_state.clone();
    let mut should_emit_alert = false;

    if breach_active {
        if next_state.breach_started_at.is_none() {
            next_state.breach_started_at = Some(now);
        }

        let breach_started_at = next_state.breach_started_at.expect("breach_started_at just set");
        let sustained = now.signed_duration_since(breach_started_at).num_milliseconds()
            >= policy.sustained_window_ms;

        let cooldown_active = next_state
            .cooldown_until
            .map(|until| until > now)
            .unwrap_or(false);

        let current_rank = severity_rank(severity.as_ref());
        let previous_rank = severity_rank(next_state.last_severity.as_ref());
        let severity_escalated = current_rank > previous_rank;

        if sustained && (!cooldown_active || severity_escalated) {
            should_emit_alert = true;
            next_state.last_alerted_at = Some(now);
            next_state.last_divergence_pct = Some(divergence_pct);
            next_state.last_severity = severity.clone();
            next_state.cooldown_until = Some(now + Duration::seconds(policy.cooldown_sec));
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
            quorum_met,
            breach_active,
            severity,
        },
        should_emit_alert,
        next_state,
    })
}

pub fn weighted_median(points: &[(f64, f64)]) -> Option<f64> {
    if points.is_empty() {
        return None;
    }

    let mut sorted = points.to_vec();
    sorted.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let total_weight: f64 = sorted.iter().map(|(_, weight)| *weight).sum();
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

fn severity_for_divergence(divergence_pct: f64, bands: &DpegSeverityBands) -> Option<Severity> {
    if divergence_pct >= bands.critical {
        return Some(Severity::Critical);
    }
    if divergence_pct >= bands.high {
        return Some(Severity::High);
    }
    if divergence_pct >= bands.medium {
        return Some(Severity::Medium);
    }
    None
}

fn severity_rank(severity: Option<&Severity>) -> u8 {
    match severity {
        Some(Severity::Info) => 1,
        Some(Severity::Low) => 2,
        Some(Severity::Medium) => 3,
        Some(Severity::High) => 4,
        Some(Severity::Critical) => 5,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> DpegPolicy {
        DpegPolicy {
            tenant_id: "tenant-a".to_string(),
            market_key: "USDC/USD".to_string(),
            peg_target: 1.0,
            min_sources: 2,
            quorum_pct: 0.5,
            sustained_window_ms: 20_000,
            cooldown_sec: 60,
            stale_timeout_ms: 15_000,
            severity_bands: DpegSeverityBands::default(),
            source_overrides: HashMap::new(),
        }
    }

    #[test]
    fn weighted_median_prefers_weighted_center() {
        let median = weighted_median(&[(1.0, 1.0), (0.95, 5.0), (1.05, 1.0)]).expect("median");
        assert_eq!(median, 0.95);
    }

    #[test]
    fn staleness_excludes_old_source() {
        let policy = default_policy();
        let now = Utc::now();
        let quotes = vec![
            QuoteInput {
                source_id: "s1".to_string(),
                price: 0.97,
                observed_at: now - Duration::seconds(2),
            },
            QuoteInput {
                source_id: "s2".to_string(),
                price: 0.98,
                observed_at: now - Duration::seconds(45),
            },
        ];

        let outcome = evaluate_policy(&policy, &quotes, &DpegAlertState::default(), now)
            .expect("evaluation");
        assert_eq!(outcome.snapshot.source_count, 1);
        assert!(!outcome.snapshot.quorum_met);
    }

    #[test]
    fn requires_sustained_window_before_emit() {
        let policy = default_policy();
        let now = Utc::now();
        let quotes = vec![
            QuoteInput {
                source_id: "s1".to_string(),
                price: 0.94,
                observed_at: now,
            },
            QuoteInput {
                source_id: "s2".to_string(),
                price: 0.95,
                observed_at: now,
            },
        ];

        let first = evaluate_policy(&policy, &quotes, &DpegAlertState::default(), now)
            .expect("first evaluation");
        assert!(!first.should_emit_alert);

        let later = now + Duration::seconds(25);
        let later_quotes = vec![
            QuoteInput {
                source_id: "s1".to_string(),
                price: 0.94,
                observed_at: later,
            },
            QuoteInput {
                source_id: "s2".to_string(),
                price: 0.95,
                observed_at: later,
            },
        ];
        let second = evaluate_policy(&policy, &later_quotes, &first.next_state, later)
            .expect("second evaluation");
        assert!(second.should_emit_alert);
    }

    #[test]
    fn cooldown_suppresses_repeat_emit_until_escalation() {
        let policy = default_policy();
        let now = Utc::now();
        let quotes_medium = vec![
            QuoteInput {
                source_id: "s1".to_string(),
                price: 0.98,
                observed_at: now,
            },
            QuoteInput {
                source_id: "s2".to_string(),
                price: 0.98,
                observed_at: now,
            },
        ];
        let mut state = DpegAlertState {
            breach_started_at: Some(now - Duration::seconds(30)),
            ..Default::default()
        };

        let emitted = evaluate_policy(&policy, &quotes_medium, &state, now).expect("emit");
        assert!(emitted.should_emit_alert);
        state = emitted.next_state;

        let within_cooldown = evaluate_policy(
            &policy,
            &quotes_medium,
            &state,
            now + Duration::seconds(10),
        )
        .expect("within cooldown");
        assert!(!within_cooldown.should_emit_alert);

        let escalation_time = now + Duration::seconds(20);
        let quotes_critical_fresh = vec![
            QuoteInput {
                source_id: "s1".to_string(),
                price: 0.90,
                observed_at: escalation_time,
            },
            QuoteInput {
                source_id: "s2".to_string(),
                price: 0.90,
                observed_at: escalation_time,
            },
        ];
        let escalated = evaluate_policy(
            &policy,
            &quotes_critical_fresh,
            &within_cooldown.next_state,
            escalation_time,
        )
        .expect("escalation");
        assert!(escalated.should_emit_alert);
        assert_eq!(escalated.snapshot.severity, Some(Severity::Critical));
    }
}
