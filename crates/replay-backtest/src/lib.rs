use anyhow::Result;
use common_types::DetectionResult;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayExpectation {
    pub fixture_name: String,
    pub min_risk_score: f64,
    pub expected_rule_ids: Vec<String>,
}

pub fn assert_detection_against_expectation(
    detection: &DetectionResult,
    expectation: &ReplayExpectation,
) -> Result<bool> {
    if detection.risk_score.score < expectation.min_risk_score {
        return Ok(false);
    }

    for rule_id in &expectation.expected_rule_ids {
        if !detection.triggered_rule_ids.contains(rule_id) {
            return Ok(false);
        }
    }

    Ok(true)
}
