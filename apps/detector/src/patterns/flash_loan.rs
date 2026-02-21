//! Flash-loan detection pattern.
//!
//! Processes EVM chain events (`UnifiedEvent` with `source_type == EvmChain`) and
//! fires when a transaction exhibits known flash-loan characteristics embedded in the
//! event `payload` by the indexer.
//!
//! Configuration (per-tenant, from `tenant_pattern_configs`):
//! ```json
//! {
//!   "rules": [
//!     {
//!       "rule_id": "flash-default",
//!       "name": "Default Flash Loan Rule",
//!       "min_loan_amount_usd": 100000,
//!       "profit_threshold_usd": 1000,
//!       "cooldown_sec": 60,
//!       "enabled": true
//!     }
//!   ]
//! }
//! ```
//!
//! Legacy single-object configs are also accepted for backward compatibility.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use event_schema::{
    AttackFamily, Chain, DetectionResult, DetectionSignal, LifecycleState, RiskScore, Severity,
    SignalType, SourceType, UnifiedEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use state_manager::PostgresRepository;
use uuid::Uuid;

use super::DetectionPattern;

pub const PATTERN_ID: &str = "flash_loan";

/// Default minimum loan size that warrants a detection (USD).
const DEFAULT_MIN_LOAN_USD: f64 = 100_000.0;
/// Default minimum extracted profit to trigger an alert (USD).
const DEFAULT_PROFIT_THRESHOLD_USD: f64 = 1_000.0;
/// Default cooldown between alerts for the same attacker address.
const DEFAULT_COOLDOWN_SEC: i64 = 300;

// ─── Per-tenant config ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashLoanRule {
    pub rule_id: String,
    pub name: String,
    pub min_loan_amount_usd: f64,
    pub profit_threshold_usd: f64,
    pub cooldown_sec: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

// ─── Per-attacker-address cooldown state ──────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FlashLoanState {
    /// address → cooldown_until
    pub cooldowns: HashMap<String, DateTime<Utc>>,
}

// ─── Pattern impl ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FlashLoanPattern {
    /// tenant_id → flash-loan rule set
    configs: HashMap<String, Vec<FlashLoanRule>>,
    /// tenant_id → in-memory state (cooldowns)
    state_cache: HashMap<String, FlashLoanState>,
}

#[async_trait]
impl DetectionPattern for FlashLoanPattern {
    fn pattern_id(&self) -> &str {
        PATTERN_ID
    }

    async fn reload_config(
        &mut self,
        config_map: &HashMap<(String, String), Value>,
    ) -> Result<()> {
        let mut new_configs = HashMap::new();
        for ((tenant_id, pattern_id), config) in config_map {
            if pattern_id != PATTERN_ID {
                continue;
            }
            let rules = parse_flash_loan_rules(config, tenant_id);
            new_configs.insert(tenant_id.clone(), rules);
        }
        self.configs = new_configs;
        tracing::info!(config_count = self.configs.len(), "flash_loan configs reloaded");
        Ok(())
    }

    async fn process_event(
        &mut self,
        event: &UnifiedEvent,
        now: DateTime<Utc>,
        repo: &PostgresRepository,
    ) -> Result<Option<DetectionResult>> {
        // Only process EVM chain events.
        if !matches!(event.source_type, SourceType::EvmChain) {
            return Ok(None);
        }

        // Look up config set for this tenant.
        let rules = match self.configs.get(&event.tenant_id).cloned() {
            Some(configs) if !configs.is_empty() => configs,
            _ => return Ok(None),
        };

        // Parse flash-loan indicators out of the event payload.
        let Some(fl) = extract_flash_loan_indicators(&event.payload) else {
            return Ok(None);
        };

        // Load state once per tenant.
        if !self.state_cache.contains_key(&event.tenant_id) {
            let loaded = repo
                .load_pattern_state(&event.tenant_id, PATTERN_ID, "cooldowns")
                .await?
                .and_then(|v| serde_json::from_value::<FlashLoanState>(v).ok())
                .unwrap_or_default();
            self.state_cache.insert(event.tenant_id.clone(), loaded);
        }

        let state = self.state_cache.entry(event.tenant_id.clone()).or_default();

        for rule in rules {
            if !rule.enabled {
                continue;
            }

            if fl.loan_amount_usd < rule.min_loan_amount_usd {
                continue;
            }
            if fl.profit_usd < rule.profit_threshold_usd {
                continue;
            }

            let cooldown_key = format!("{}:{}", rule.rule_id, fl.attacker_address);
            if let Some(cooldown_until) = state.cooldowns.get(&cooldown_key) {
                if *cooldown_until > now {
                    continue;
                }
            }

            // Apply cooldown.
            state
                .cooldowns
                .insert(cooldown_key.clone(), now + Duration::seconds(rule.cooldown_sec));

            // Persist updated state.
            let state_value = serde_json::to_value(state.clone())?;
            let _ = repo
                .upsert_pattern_state(&event.tenant_id, PATTERN_ID, "cooldowns", state_value)
                .await;

            // Persist snapshot.
            let snapshot_data = serde_json::json!({
                "rule_id": rule.rule_id,
                "rule_name": rule.name,
                "loan_amount_usd": fl.loan_amount_usd,
                "profit_usd": fl.profit_usd,
                "attacker_address": fl.attacker_address,
                "protocol": fl.protocol,
                "tx_hash": event.tx_hash,
            });
            let _ = repo
                .insert_pattern_snapshot(
                    &event.tenant_id,
                    PATTERN_ID,
                    &cooldown_key,
                    snapshot_data,
                    Some(fl.profit_usd),
                    Some("high"),
                )
                .await;

            return Ok(Some(build_detection(event, &fl, now, &rule)));
        }

        Ok(None)
    }
}

fn parse_flash_loan_rules(config: &Value, tenant_id: &str) -> Vec<FlashLoanRule> {
    let mut parsed: Vec<FlashLoanRule> = Vec::new();
    if let Some(items) = config.get("rules").and_then(|value| value.as_array()) {
        for (index, item) in items.iter().enumerate() {
            if let Some(rule) = parse_rule_item(item, index) {
                parsed.push(rule);
            }
        }
    }

    if !parsed.is_empty() {
        return parsed;
    }

    if let Some(rule) = parse_legacy_rule(config) {
        return vec![rule];
    }

    tracing::warn!(tenant_id = %tenant_id, "invalid flash_loan config; falling back to defaults");
    vec![default_flash_rule("flash-default", "Default Flash Loan Rule")]
}

fn parse_rule_item(value: &Value, index: usize) -> Option<FlashLoanRule> {
    let object = value.as_object()?;
    let rule_id = object
        .get("rule_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("flash-rule-{}", index + 1));

    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| rule_id.clone());

    Some(FlashLoanRule {
        rule_id,
        name,
        min_loan_amount_usd: parse_f64(object.get("min_loan_amount_usd"), DEFAULT_MIN_LOAN_USD),
        profit_threshold_usd: parse_f64(
            object.get("profit_threshold_usd"),
            DEFAULT_PROFIT_THRESHOLD_USD,
        ),
        cooldown_sec: parse_i64(object.get("cooldown_sec"), DEFAULT_COOLDOWN_SEC),
        enabled: object
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

fn parse_legacy_rule(value: &Value) -> Option<FlashLoanRule> {
    let object = value.as_object()?;
    Some(FlashLoanRule {
        rule_id: "flash-default".to_string(),
        name: "Default Flash Loan Rule".to_string(),
        min_loan_amount_usd: parse_f64(object.get("min_loan_amount_usd"), DEFAULT_MIN_LOAN_USD),
        profit_threshold_usd: parse_f64(
            object.get("profit_threshold_usd"),
            DEFAULT_PROFIT_THRESHOLD_USD,
        ),
        cooldown_sec: parse_i64(object.get("cooldown_sec"), DEFAULT_COOLDOWN_SEC),
        enabled: object
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

fn parse_f64(value: Option<&Value>, fallback: f64) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(fallback)
}

fn parse_i64(value: Option<&Value>, fallback: i64) -> i64 {
    if let Some(number) = value.and_then(Value::as_i64) {
        return number;
    }
    if let Some(number) = value.and_then(Value::as_u64) {
        return number as i64;
    }
    fallback
}

fn default_flash_rule(rule_id: &str, name: &str) -> FlashLoanRule {
    FlashLoanRule {
        rule_id: rule_id.to_string(),
        name: name.to_string(),
        min_loan_amount_usd: DEFAULT_MIN_LOAN_USD,
        profit_threshold_usd: DEFAULT_PROFIT_THRESHOLD_USD,
        cooldown_sec: DEFAULT_COOLDOWN_SEC,
        enabled: true,
    }
}

// ─── Payload parsing ──────────────────────────────────────────────────────────

struct FlashLoanIndicators {
    loan_amount_usd: f64,
    profit_usd: f64,
    attacker_address: String,
    protocol: String,
    chain: Chain,
    chain_slug: String,
}

/// Attempts to extract flash-loan indicators from a normalized event payload.
///
/// The indexer is expected to set `event_type = "flash_loan"` and include at least:
/// - `flash_loan.loan_amount_usd`
/// - `flash_loan.profit_usd`
/// - `flash_loan.attacker_address` (or `from`)
/// - `flash_loan.protocol` (or `protocol`)
fn extract_flash_loan_indicators(payload: &Value) -> Option<FlashLoanIndicators> {
    // Accept events where event_type was annotated by the indexer or the payload
    // contains a `flash_loan` sub-object.
    let fl = payload.get("flash_loan").or(Some(payload))?;

    let loan_amount_usd = fl.get("loan_amount_usd")?.as_f64()?;
    let profit_usd = fl.get("profit_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let attacker_address = fl
        .get("attacker_address")
        .or_else(|| payload.get("from"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let protocol = fl
        .get("protocol")
        .or_else(|| payload.get("protocol"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let chain_slug = payload
        .get("chain_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let chain = chain_from_slug(&chain_slug);

    Some(FlashLoanIndicators {
        loan_amount_usd,
        profit_usd,
        attacker_address,
        protocol,
        chain,
        chain_slug,
    })
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

fn build_detection(
    event: &UnifiedEvent,
    fl: &FlashLoanIndicators,
    now: DateTime<Utc>,
    rule: &FlashLoanRule,
) -> DetectionResult {
    let tx_hash = event.tx_hash.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let description = format!(
        "Flash loan rule '{}' triggered: ${:.0} borrowed from {}, ${:.0} profit extracted by {} on {}.",
        rule.name,
        fl.loan_amount_usd,
        fl.protocol,
        fl.profit_usd,
        fl.attacker_address,
        fl.chain_slug,
    );

        DetectionResult {
        detection_id: Uuid::new_v4(),
        pattern_id: PATTERN_ID.to_string(),
        event_key: Some(format!("flash_loan:{}:{}:{}", event.tenant_id, rule.rule_id, tx_hash)),
        subject_type: Some("address".to_string()),
        subject_key: Some(fl.attacker_address.clone()),
        tenant_id: Some(event.tenant_id.clone()),
        chain: fl.chain.clone(),
        chain_slug: fl.chain_slug.clone(),
        protocol: fl.protocol.clone(),
        lifecycle_state: LifecycleState::Confirmed,
        requires_confirmation: false,
        attack_family: AttackFamily::FlashLoan,
        severity: Severity::High,
        tx_hash: tx_hash.clone(),
        block_number: event.block_number.unwrap_or(0),
        triggered_rule_ids: vec![format!("flash_loan.{}", rule.rule_id)],
        description: Some(description),
        signals: vec![
            DetectionSignal {
                signal_type: SignalType::LoanVolumeSpike,
                value: fl.loan_amount_usd,
                label: Some(format!("loan ${:.0}", fl.loan_amount_usd)),
                source_id: None,
            },
            DetectionSignal {
                signal_type: SignalType::ProfitExtracted,
                value: fl.profit_usd,
                label: Some(format!("profit ${:.0}", fl.profit_usd)),
                source_id: None,
            },
        ],
        risk_score: RiskScore::default(),
        oracle_context: std::collections::HashMap::new(),
        actions_recommended: vec![
            "Pause affected protocol pools immediately.".to_string(),
            "Notify protocol team and security researchers.".to_string(),
        ],
        created_at: now,
    }
}
