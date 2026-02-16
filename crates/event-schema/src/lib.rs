use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Chain {
    Ethereum,
    Base,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Observed,
    Confirmed,
    Retracted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Provisional,
    Confirmed,
    Retracted,
    Suppressed,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AttackFamily {
    OracleManipulation,
    FlashLoanManipulation,
    CrossChainMismatch,
    StalePriceExploitation,
    OracleUpgradeRisk,
    DecimalPrecisionRisk,
    LiquidationCascade,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolCategory {
    Lending,
    PerpDex,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    OracleDivergence,
    FlashLoanDetected,
    OracleReadDetected,
    LargeSwapDetected,
    CrossChainMismatch,
    OracleUpgradeDetected,
    StalePriceDetected,
    DecimalPrecisionMismatch,
    NearMissPattern,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    OracleUpdate,
    FlashLoanCandidate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub event_id: Uuid,
    pub event_key: String,
    pub event_type: EventType,
    pub tenant_id: Option<String>,
    pub chain: Chain,
    pub chain_slug: String,
    pub protocol: String,
    pub protocol_category: ProtocolCategory,
    pub chain_id: Option<u64>,
    pub tx_hash: String,
    pub block_number: u64,
    pub block_hash: Option<String>,
    pub parent_hash: Option<String>,
    pub tx_index: Option<u64>,
    pub log_index: Option<u64>,
    pub status: EventStatus,
    pub lifecycle_state: LifecycleState,
    pub requires_confirmation: bool,
    pub confirmation_depth: u64,
    pub ingest_latency_ms: Option<u64>,
    pub observed_at: DateTime<Utc>,
    pub oracle_price: Option<f64>,
    pub reference_price: Option<f64>,
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionSignal {
    pub signal_type: SignalType,
    pub triggered: bool,
    pub value: Option<f64>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attribution {
    pub feature: String,
    pub contribution: f64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskScore {
    pub score: f64,
    pub confidence: f64,
    pub rationale: Vec<String>,
    pub attribution: Vec<Attribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionResult {
    pub detection_id: Uuid,
    pub event_key: Option<String>,
    pub tenant_id: Option<String>,
    pub chain: Chain,
    pub chain_slug: String,
    pub protocol: String,
    pub lifecycle_state: LifecycleState,
    pub requires_confirmation: bool,
    pub attack_family: AttackFamily,
    pub near_miss: bool,
    pub confidence: f64,
    pub severity: Severity,
    pub triggered_rule_ids: Vec<String>,
    pub tx_hash: String,
    pub block_number: u64,
    pub signals: Vec<DetectionSignal>,
    pub risk_score: RiskScore,
    pub attribution: Vec<Attribution>,
    pub oracle_context: HashMap<String, serde_json::Value>,
    pub actions_recommended: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertEvent {
    pub alert_id: Uuid,
    pub incident_id: Option<String>,
    pub event_key: Option<String>,
    pub tenant_id: Option<String>,
    pub chain: Chain,
    pub chain_slug: String,
    pub protocol: String,
    pub lifecycle_state: LifecycleState,
    pub severity: Severity,
    pub risk_score: f64,
    pub confidence: f64,
    pub rule_ids: Vec<String>,
    pub channel_routes: Vec<String>,
    pub dedup_key: Option<String>,
    pub attribution: Vec<Attribution>,
    pub blast_radius: Vec<String>,
    pub tx_hash: String,
    pub block_number: u64,
    pub oracle_context: HashMap<String, serde_json::Value>,
    pub actions_recommended: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalityUpdate {
    pub chain: Chain,
    pub event_key: String,
    pub block_number: u64,
    pub block_hash: Option<String>,
    pub lifecycle_state: LifecycleState,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReorgNotice {
    pub chain: Chain,
    pub orphaned_from_block: u64,
    pub common_ancestor_block: u64,
    pub affected_event_keys: Vec<String>,
    pub noticed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub weight: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureVector {
    pub feature_set_version: String,
    pub values: HashMap<String, f64>,
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSimulationReport {
    pub simulation_id: String,
    pub triggered: bool,
    pub risk_score: f64,
    pub notes: Vec<String>,
    pub fixture_name: Option<String>,
}
