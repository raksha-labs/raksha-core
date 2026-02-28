use anyhow::Result;
use async_trait::async_trait;
use event_schema::{Attribution, DetectionResult, RiskScore, Severity, SignalType};
use common::RiskScorer;

pub struct DeterministicRiskScorer;

#[async_trait]
impl RiskScorer for DeterministicRiskScorer {
    async fn score(&self, result: &DetectionResult) -> Result<RiskScore> {
        let mut score: f64 = 0.0;
        let mut rationale = Vec::new();
        let mut attribution = Vec::new();

        match result.severity {
            Severity::Critical => {
                score += 40.0;
                rationale.push("Critical severity baseline +40".to_string());
                attribution.push(Attribution {
                    feature: "severity_critical".to_string(),
                    contribution: 40.0,
                    detail: "Critical severity baseline".to_string(),
                });
            }
            Severity::High => {
                score += 30.0;
                rationale.push("High severity baseline +30".to_string());
                attribution.push(Attribution {
                    feature: "severity_high".to_string(),
                    contribution: 30.0,
                    detail: "High severity baseline".to_string(),
                });
            }
            Severity::Medium => {
                score += 20.0;
                rationale.push("Medium severity baseline +20".to_string());
                attribution.push(Attribution {
                    feature: "severity_medium".to_string(),
                    contribution: 20.0,
                    detail: "Medium severity baseline".to_string(),
                });
            }
            Severity::Low => {
                score += 10.0;
                rationale.push("Low severity baseline +10".to_string());
                attribution.push(Attribution {
                    feature: "severity_low".to_string(),
                    contribution: 10.0,
                    detail: "Low severity baseline".to_string(),
                });
            }
            Severity::Info => {}
        }

        if !result.triggered_rule_ids.is_empty() {
            score += 20.0;
            rationale.push("Triggered rules present +20".to_string());
            attribution.push(Attribution {
                feature: "triggered_rules".to_string(),
                contribution: 20.0,
                detail: format!("{} rule(s) triggered", result.triggered_rule_ids.len()),
            });
        }

        for signal in &result.signals {
            if signal.signal_type == SignalType::OracleDivergence {
                score += 25.0;
                rationale.push("Oracle divergence triggered +25".to_string());
                attribution.push(Attribution {
                    feature: "oracle_divergence".to_string(),
                    contribution: 25.0,
                    detail: signal.label.clone().unwrap_or_else(|| format!("divergence value: {:.4}", signal.value)),
                });
            }
        }

        if result
            .oracle_context
            .get("trace_summary")
            .and_then(|value| value.get("has_flash_loan"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            score += 15.0;
            rationale.push("Flash loan in trace summary +15".to_string());
            attribution.push(Attribution {
                feature: "trace_flash_loan".to_string(),
                contribution: 15.0,
                detail: "trace enrichment indicates flash loan".to_string(),
            });
        }

        let bounded_score = score.min(100.0);
        Ok(RiskScore {
            score: bounded_score,
            confidence: (0.6 + (bounded_score / 250.0)).min(0.99),
            rationale,
            attribution,
        })
    }
}
