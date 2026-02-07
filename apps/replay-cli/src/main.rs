use anyhow::Result;
use chrono::Utc;
use common_types::{AttackFamily, Chain, DetectionResult, LifecycleState, RiskScore, Severity};
use replay_backtest::{assert_detection_against_expectation, ReplayExpectation};
use std::collections::HashMap;
use uuid::Uuid;

fn main() -> Result<()> {
    let detection = DetectionResult {
        detection_id: Uuid::new_v4(),
        event_key: Some("ethereum:19123456:0xreplaytx:0".to_string()),
        tenant_id: Some("default".to_string()),
        chain: Chain::Ethereum,
        chain_slug: "ethereum".to_string(),
        protocol: "aave-v3".to_string(),
        lifecycle_state: LifecycleState::Confirmed,
        requires_confirmation: false,
        attack_family: AttackFamily::OracleManipulation,
        near_miss: false,
        confidence: 0.9,
        severity: Severity::High,
        triggered_rule_ids: vec!["oracle-divergence-eth-lending".to_string()],
        tx_hash: "0xreplaytx".to_string(),
        block_number: 19_123_456,
        signals: vec![],
        risk_score: RiskScore {
            score: 82.0,
            confidence: 0.9,
            rationale: vec!["fixture replay".to_string()],
            attribution: vec![],
        },
        attribution: vec![],
        oracle_context: HashMap::new(),
        actions_recommended: vec!["pause risky actions".to_string()],
        created_at: Utc::now(),
    };

    let expectation = ReplayExpectation {
        fixture_name: "euler_like_fixture".to_string(),
        min_risk_score: 70.0,
        expected_rule_ids: vec!["oracle-divergence-eth-lending".to_string()],
    };

    let ok = assert_detection_against_expectation(&detection, &expectation)?;
    println!("replay check passed: {ok}");

    Ok(())
}
