use std::{collections::HashMap, fs, path::Path};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use event_schema::{
    AttackFamily, DetectionResult, DetectionSignal, NormalizedEvent, RiskScore, Severity,
    SignalType,
};
use common::RuleEvaluator;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDefinition {
    pub id: String,
    pub version: u32,
    pub enabled: bool,
    pub chain: String,
    pub protocol: String,
    pub signals: Vec<String>,
    pub conditions: HashMap<String, f64>,
    pub window_ms: u64,
    pub severity: String,
    pub cooldown_sec: u64,
    pub routing_tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RuleEngine {
    rules: Vec<RuleDefinition>,
}

impl RuleEngine {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_paths([path.as_ref().to_path_buf()])
    }

    pub fn from_paths<I, P>(paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut rules = Vec::new();

        for path in paths {
            let path_ref = path.as_ref();
            let raw = fs::read_to_string(path_ref)
                .with_context(|| format!("failed to read rules file: {}", path_ref.display()))?;
            let rule: RuleDefinition = serde_yaml::from_str(&raw)
                .with_context(|| format!("failed to parse rule yaml: {}", path_ref.display()))?;
            rules.push(rule);
        }

        if rules.is_empty() {
            bail!("no rules loaded");
        }

        Ok(Self { rules })
    }

    pub fn with_rules(rules: Vec<RuleDefinition>) -> Self {
        Self { rules }
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

#[async_trait]
impl RuleEvaluator for RuleEngine {
    async fn evaluate(&self, event: &NormalizedEvent) -> Result<DetectionResult> {
        let mut signals = Vec::new();
        let mut triggered_rules = Vec::new();
        let mut triggered_rule_severities = Vec::new();

        let divergence =
            if let (Some(oracle), Some(reference)) = (event.oracle_price, event.reference_price) {
                if reference > 0.0 {
                    ((oracle - reference).abs() / reference) * 100.0
                } else {
                    0.0
                }
            } else {
                0.0
            };

        let applicable_rules: Vec<&RuleDefinition> = self
            .rules
            .iter()
            .filter(|rule| {
                rule.enabled
                    && chain_matches(&rule.chain, &event.chain, &event.chain_slug)
                    && protocol_matches(&rule.protocol, &event.protocol)
            })
            .collect();

        let signal_threshold = applicable_rules
            .iter()
            .filter_map(|rule| rule.conditions.get("divergence_pct_gt").copied())
            .fold(5.0, f64::min);

        signals.push(DetectionSignal {
            signal_type: SignalType::OracleDivergence,
            triggered: divergence > signal_threshold,
            value: Some(divergence),
            detail: format!("divergence_pct={divergence:.2}"),
        });

        for rule in applicable_rules {
            let threshold = rule
                .conditions
                .get("divergence_pct_gt")
                .copied()
                .unwrap_or(5.0);
            if divergence > threshold {
                triggered_rules.push(rule.id.clone());
                triggered_rule_severities.push(parse_severity(&rule.severity));
            }
        }

        let severity = triggered_rule_severities
            .into_iter()
            .max_by_key(severity_rank)
            .unwrap_or(Severity::Low);

        let result = DetectionResult {
            detection_id: Uuid::new_v4(),
            event_key: Some(event.event_key.clone()),
            subject_type: None,
            subject_key: None,
            tenant_id: event.tenant_id.clone(),
            chain: event.chain.clone(),
            chain_slug: event.chain_slug.clone(),
            protocol: event.protocol.clone(),
            lifecycle_state: event.lifecycle_state.clone(),
            requires_confirmation: event.requires_confirmation,
            attack_family: AttackFamily::OracleManipulation,
            near_miss: false,
            confidence: 0.7,
            severity,
            triggered_rule_ids: triggered_rules,
            tx_hash: event.tx_hash.clone(),
            block_number: event.block_number,
            signals,
            risk_score: RiskScore {
                score: 0.0,
                confidence: 0.0,
                rationale: vec!["risk score pending risk-scorer stage".to_string()],
                attribution: Vec::new(),
            },
            attribution: Vec::new(),
            oracle_context: event.metadata.clone(),
            actions_recommended: vec![
                "Review affected protocol market and pause sensitive actions if needed".to_string(),
            ],
            created_at: Utc::now(),
        };

        Ok(result)
    }
}

fn chain_matches(
    rule_chain: &str,
    event_chain: &event_schema::Chain,
    event_chain_slug: &str,
) -> bool {
    if rule_chain.eq_ignore_ascii_case(event_chain_slug) {
        return true;
    }
    rule_chain.eq_ignore_ascii_case(&format!("{:?}", event_chain).to_lowercase())
}

fn protocol_matches(rule_protocol: &str, event_protocol: &str) -> bool {
    rule_protocol.eq_ignore_ascii_case(event_protocol)
}

fn parse_severity(raw: &str) -> Severity {
    match raw.to_ascii_lowercase().as_str() {
        "info" => Severity::Info,
        "low" => Severity::Low,
        "medium" => Severity::Medium,
        "high" => Severity::High,
        "critical" => Severity::Critical,
        _ => Severity::Low,
    }
}

fn severity_rank(severity: &Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Low => 1,
        Severity::Medium => 2,
        Severity::High => 3,
        Severity::Critical => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_schema::{Chain, EventStatus, EventType, NormalizedEvent, ProtocolCategory};
    use std::collections::HashMap;

    fn mk_event(
        chain: Chain,
        protocol: &str,
        oracle_price: f64,
        reference_price: f64,
    ) -> NormalizedEvent {
        NormalizedEvent {
            event_id: Uuid::new_v4(),
            event_key: "ethereum:1:0xtest:0".to_string(),
            event_type: EventType::OracleUpdate,
            tenant_id: None,
            chain,
            chain_slug: "ethereum".to_string(),
            protocol: protocol.to_string(),
            protocol_category: ProtocolCategory::Lending,
            chain_id: Some(1),
            tx_hash: "0xtest".to_string(),
            block_number: 1,
            block_hash: None,
            parent_hash: None,
            tx_index: Some(0),
            log_index: Some(0),
            status: EventStatus::Observed,
            lifecycle_state: event_schema::LifecycleState::Provisional,
            requires_confirmation: true,
            confirmation_depth: 3,
            ingest_latency_ms: Some(10),
            observed_at: Utc::now(),
            oracle_price: Some(oracle_price),
            reference_price: Some(reference_price),
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn applies_matching_rule_threshold_and_severity() {
        let engine = RuleEngine::with_rules(vec![RuleDefinition {
            id: "r1".to_string(),
            version: 1,
            enabled: true,
            chain: "ethereum".to_string(),
            protocol: "aave-v3".to_string(),
            signals: vec!["oracle_divergence".to_string()],
            conditions: HashMap::from([("divergence_pct_gt".to_string(), 4.0)]),
            window_ms: 60_000,
            severity: "high".to_string(),
            cooldown_sec: 300,
            routing_tags: vec!["mvp".to_string()],
        }]);

        let event = mk_event(Chain::Ethereum, "aave-v3", 105.0, 100.0);
        let result = engine
            .evaluate(&event)
            .await
            .expect("evaluation should succeed");

        assert_eq!(result.triggered_rule_ids, vec!["r1".to_string()]);
        assert_eq!(result.severity, Severity::High);
        assert!(result.signals.iter().any(|signal| signal.triggered));
    }

    #[tokio::test]
    async fn ignores_non_matching_protocol_rules() {
        let engine = RuleEngine::with_rules(vec![RuleDefinition {
            id: "r1".to_string(),
            version: 1,
            enabled: true,
            chain: "ethereum".to_string(),
            protocol: "aave-v3".to_string(),
            signals: vec!["oracle_divergence".to_string()],
            conditions: HashMap::from([("divergence_pct_gt".to_string(), 1.0)]),
            window_ms: 60_000,
            severity: "critical".to_string(),
            cooldown_sec: 300,
            routing_tags: vec!["mvp".to_string()],
        }]);

        let event = mk_event(Chain::Ethereum, "other-protocol", 120.0, 100.0);
        let result = engine
            .evaluate(&event)
            .await
            .expect("evaluation should succeed");

        assert!(result.triggered_rule_ids.is_empty());
        assert_eq!(result.severity, Severity::Low);
    }

    #[tokio::test]
    async fn matches_rule_by_chain_slug_when_chain_enum_is_unknown() {
        let engine = RuleEngine::with_rules(vec![RuleDefinition {
            id: "r-base".to_string(),
            version: 1,
            enabled: true,
            chain: "arbitrum".to_string(),
            protocol: "aave-v3".to_string(),
            signals: vec!["oracle_divergence".to_string()],
            conditions: HashMap::from([("divergence_pct_gt".to_string(), 3.0)]),
            window_ms: 60_000,
            severity: "high".to_string(),
            cooldown_sec: 300,
            routing_tags: vec!["mvp".to_string()],
        }]);

        let mut event = mk_event(Chain::Unknown, "aave-v3", 104.0, 100.0);
        event.chain_slug = "arbitrum".to_string();

        let result = engine
            .evaluate(&event)
            .await
            .expect("evaluation should succeed");

        assert_eq!(result.triggered_rule_ids, vec!["r-base".to_string()]);
        assert_eq!(result.severity, Severity::High);
    }
}
