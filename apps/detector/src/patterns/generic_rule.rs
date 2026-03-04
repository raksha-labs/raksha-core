//! Generic rule-engine runtime pattern.
//!
//! Executes published tenant-authored sandbox rules through the internal runtime
//! service and emits detections when runtime reports matched alerts.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use event_schema::{
    AttackFamily, Chain, ContextClassification, DetectionResult, DetectionSignal,
    IncidentTransition, LifecycleState, RiskScore, Severity, SignalType, UnifiedEvent,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use state_manager::PostgresRepository;
use uuid::Uuid;

use super::DetectionPattern;

pub const PATTERN_ID: &str = "generic_rule";

#[derive(Debug, Clone, Default)]
struct CompiledRuleRef {
    rule_id: String,
    version: Option<i64>,
    language: String,
    engine: String,
    compiled_artifact: Value,
    params: Value,
}

#[derive(Debug, Serialize)]
struct RuntimeExecuteRequest {
    tenant_id: String,
    pattern_id: String,
    rule_id: String,
    rule_version: Option<i64>,
    language: String,
    compiled_artifact: Value,
    event: Value,
    state: Value,
    params: Value,
    now: String,
}

#[derive(Debug, Deserialize)]
struct RuntimeExecuteResponse {
    ok: bool,
    runtime: RuntimeReport,
}

#[derive(Debug, Deserialize)]
struct RuntimeReport {
    ok: bool,
    matched_alerts: Vec<RuntimeAlert>,
    state: Value,
    trace: Vec<String>,
    metrics: RuntimeMetrics,
    error_code: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeMetrics {
    execution_ms: i64,
    ops_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeAlert {
    severity: String,
    title: String,
    reason: String,
    payload: Value,
}

/// Pattern ID -> tenant ID -> compiled rules.
pub struct GenericRulePattern {
    compiled_rules: HashMap<String, HashMap<String, Vec<CompiledRuleRef>>>,
    runtime_url: Option<String>,
    runtime_token: Option<String>,
    runtime_degraded: HashMap<String, bool>,
    client: Client,
}

impl Default for GenericRulePattern {
    fn default() -> Self {
        let runtime_base = std::env::var("RULE_RUNTIME_SERVICE_URL")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        let runtime_token = std::env::var("RULE_RUNTIME_INTERNAL_SERVICE_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty());

        Self {
            compiled_rules: HashMap::new(),
            runtime_url: runtime_base,
            runtime_token,
            runtime_degraded: HashMap::new(),
            client: Client::new(),
        }
    }
}

impl GenericRulePattern {
    fn state_key(pattern_id: &str, rule_id: &str) -> String {
        format!("runtime:{pattern_id}:{rule_id}")
    }

    fn health_key(tenant_id: &str, pattern_id: &str, rule_id: &str) -> String {
        format!("{tenant_id}:{pattern_id}:{rule_id}")
    }

    fn severity_from_runtime(value: &str) -> Severity {
        match value.to_ascii_lowercase().as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "medium" => Severity::Medium,
            "low" => Severity::Low,
            "info" => Severity::Info,
            _ => Severity::Medium,
        }
    }

    fn risk_score_for_severity(severity: &Severity) -> f64 {
        match severity {
            Severity::Critical => 95.0,
            Severity::High => 80.0,
            Severity::Medium => 60.0,
            Severity::Low => 35.0,
            Severity::Info => 10.0,
        }
    }

    fn signal_type_for_pattern(pattern_id: &str) -> SignalType {
        if pattern_id == "dpeg" {
            SignalType::PegDeviation
        } else if pattern_id == "tvl_drop" {
            SignalType::PriceDeviation
        } else {
            SignalType::NearMissPattern
        }
    }

    fn attack_family_for_pattern(pattern_id: &str) -> AttackFamily {
        if pattern_id == "dpeg" {
            AttackFamily::PegDeviation
        } else if pattern_id == "tvl_drop" {
            AttackFamily::LiquidationCascade
        } else if pattern_id == "flash_loan" {
            AttackFamily::FlashLoan
        } else {
            AttackFamily::Unknown
        }
    }

    fn chain_from_event(event: &UnifiedEvent) -> Chain {
        match event.chain_id.unwrap_or_default() {
            1 => Chain::Ethereum,
            42161 => Chain::Arbitrum,
            10 => Chain::Optimism,
            8453 => Chain::Base,
            137 => Chain::Polygon,
            43114 => Chain::Avalanche,
            56 => Chain::BSC,
            _ => Chain::Unknown,
        }
    }

    fn chain_slug(chain: &Chain) -> String {
        match chain {
            Chain::Ethereum => "ethereum",
            Chain::Arbitrum => "arbitrum",
            Chain::Optimism => "optimism",
            Chain::Base => "base",
            Chain::Polygon => "polygon",
            Chain::Avalanche => "avalanche",
            Chain::BSC => "bsc",
            Chain::Offchain => "offchain",
            Chain::Unknown => "unknown",
        }
        .to_string()
    }

    fn detection_from_alert(
        pattern_id: &str,
        rule_id: &str,
        event: &UnifiedEvent,
        alert: &RuntimeAlert,
        runtime_trace: &[String],
    ) -> DetectionResult {
        let severity = Self::severity_from_runtime(&alert.severity);
        let score = Self::risk_score_for_severity(&severity);
        let chain = Self::chain_from_event(event);
        let chain_slug = Self::chain_slug(&chain);
        let mut oracle_context = HashMap::new();
        oracle_context.insert("runtime_payload".to_string(), alert.payload.clone());
        oracle_context.insert(
            "runtime_trace".to_string(),
            Value::Array(
                runtime_trace
                    .iter()
                    .map(|item| Value::String(item.clone()))
                    .collect(),
            ),
        );
        oracle_context.insert(
            "source_id".to_string(),
            Value::String(event.source_id.clone()),
        );

        DetectionResult {
            detection_id: Uuid::new_v4(),
            pattern_id: pattern_id.to_string(),
            event_key: Some(event.event_id.clone()),
            subject_type: Some("rule".to_string()),
            subject_key: Some(rule_id.to_string()),
            tenant_id: Some(event.tenant_id.clone()),
            chain,
            chain_slug,
            protocol: pattern_id.to_string(),
            lifecycle_state: LifecycleState::Provisional,
            requires_confirmation: false,
            attack_family: Self::attack_family_for_pattern(pattern_id),
            severity: severity.clone(),
            description: Some(alert.reason.clone()),
            triggered_rule_ids: vec![rule_id.to_string()],
            tx_hash: event
                .tx_hash
                .clone()
                .unwrap_or_else(|| event.event_id.clone()),
            block_number: event.block_number.unwrap_or_default(),
            signals: vec![DetectionSignal {
                signal_type: Self::signal_type_for_pattern(pattern_id),
                value: score,
                label: Some(alert.title.clone()),
                source_id: Some(event.source_id.clone()),
            }],
            risk_score: RiskScore {
                score,
                confidence: 0.72,
                rationale: vec![alert.reason.clone()],
                attribution: vec![],
            },
            incident_transition: Some(IncidentTransition::Trigger),
            context_classification: Some(ContextClassification::None),
            confidence_breakdown: HashMap::new(),
            oracle_context,
            actions_recommended: vec![
                "Review runtime-generated alert payload".to_string(),
                "Confirm source freshness and quorum".to_string(),
            ],
            created_at: Utc::now(),
        }
    }

    async fn execute_runtime(
        &self,
        pattern_id: &str,
        rule: &CompiledRuleRef,
        event: &UnifiedEvent,
        state: Value,
    ) -> Result<RuntimeReport> {
        let runtime_url = match &self.runtime_url {
            Some(value) => value,
            None => {
                return Ok(RuntimeReport {
                    ok: false,
                    matched_alerts: vec![],
                    state,
                    trace: vec!["runtime_url_not_configured".to_string()],
                    metrics: RuntimeMetrics {
                        execution_ms: 0,
                        ops_count: 0,
                    },
                    error_code: Some("runtime_url_not_configured".to_string()),
                });
            }
        };

        let mut event_payload = event.payload.clone();
        if let Some(object) = event_payload.as_object_mut() {
            object.insert(
                "event_id".to_string(),
                Value::String(event.event_id.clone()),
            );
            object.insert(
                "event_type".to_string(),
                Value::String(event.event_type.clone()),
            );
            object.insert(
                "source_id".to_string(),
                Value::String(event.source_id.clone()),
            );
            if let Some(market_key) = &event.market_key {
                object.insert("market_key".to_string(), Value::String(market_key.clone()));
            }
            if let Some(price) = event.price {
                object.insert("price".to_string(), json!(price));
            }
            if let Some(block_number) = event.block_number {
                object.insert("block_number".to_string(), json!(block_number));
            }
            if let Some(tx_hash) = &event.tx_hash {
                object.insert("tx_hash".to_string(), Value::String(tx_hash.clone()));
            }
        }

        let request = RuntimeExecuteRequest {
            tenant_id: event.tenant_id.clone(),
            pattern_id: pattern_id.to_string(),
            rule_id: rule.rule_id.clone(),
            rule_version: rule.version,
            language: rule.language.clone(),
            compiled_artifact: rule.compiled_artifact.clone(),
            event: event_payload,
            state,
            params: rule.params.clone(),
            now: Utc::now().to_rfc3339(),
        };

        let mut request_builder = self
            .client
            .post(format!("{runtime_url}/execute"))
            .json(&request);
        if let Some(token) = &self.runtime_token {
            request_builder = request_builder.header("x-service-token", token);
        }

        let response = request_builder.send().await?;
        if !response.status().is_success() {
            return Ok(RuntimeReport {
                ok: false,
                matched_alerts: vec![],
                state: json!({}),
                trace: vec![format!("http_status={}", response.status().as_u16())],
                metrics: RuntimeMetrics {
                    execution_ms: 0,
                    ops_count: 0,
                },
                error_code: Some("runtime_http_error".to_string()),
            });
        }

        let payload = response.json::<RuntimeExecuteResponse>().await?;
        if !payload.ok {
            return Ok(RuntimeReport {
                ok: false,
                matched_alerts: vec![],
                state: json!({}),
                trace: vec!["runtime_response_not_ok".to_string()],
                metrics: RuntimeMetrics {
                    execution_ms: 0,
                    ops_count: 0,
                },
                error_code: Some("runtime_response_not_ok".to_string()),
            });
        }

        Ok(payload.runtime)
    }
}

#[async_trait]
impl DetectionPattern for GenericRulePattern {
    fn pattern_id(&self) -> &str {
        PATTERN_ID
    }

    async fn reload_config(&mut self, config_map: &HashMap<(String, String), Value>) -> Result<()> {
        let mut next: HashMap<String, HashMap<String, Vec<CompiledRuleRef>>> = HashMap::new();

        for ((tenant_id, pattern_id), config) in config_map {
            if pattern_id == "dpeg" || pattern_id == "flash_loan" || pattern_id == "tvl_drop" {
                continue;
            }

            let mut rules: Vec<CompiledRuleRef> = Vec::new();
            if let Some(items) = config.get("rules").and_then(Value::as_array) {
                for item in items {
                    let object = match item.as_object() {
                        Some(object) => object,
                        None => continue,
                    };

                    let rule_id = object
                        .get("rule_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if rule_id.is_empty() {
                        continue;
                    }

                    let version = object.get("rule_version").and_then(Value::as_i64);
                    let language = object
                        .get("language")
                        .and_then(Value::as_str)
                        .unwrap_or("cel")
                        .to_string();
                    let compiled = object
                        .get("compiled_artifact")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let engine = compiled
                        .get("engine")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();

                    if engine != "python-sandbox-v1" && engine != "js-sandbox-v1" {
                        continue;
                    }

                    let params = object
                        .get("authoring_model")
                        .cloned()
                        .unwrap_or_else(|| json!({}));

                    rules.push(CompiledRuleRef {
                        rule_id,
                        version,
                        language,
                        engine,
                        compiled_artifact: compiled,
                        params,
                    });
                }
            }

            if !rules.is_empty() {
                next.entry(pattern_id.clone())
                    .or_default()
                    .insert(tenant_id.clone(), rules);
            }
        }

        self.compiled_rules = next;
        let loaded_patterns = self.compiled_rules.len();
        let loaded_rules: usize = self
            .compiled_rules
            .values()
            .map(|per_tenant| per_tenant.values().map(Vec::len).sum::<usize>())
            .sum();
        let python_rule_count: usize = self
            .compiled_rules
            .values()
            .flat_map(|per_tenant| per_tenant.values())
            .flatten()
            .filter(|rule| rule.engine == "python-sandbox-v1")
            .count();
        let js_rule_count: usize = self
            .compiled_rules
            .values()
            .flat_map(|per_tenant| per_tenant.values())
            .flatten()
            .filter(|rule| rule.engine == "js-sandbox-v1")
            .count();

        tracing::info!(
            loaded_patterns,
            loaded_rules,
            python_rule_count,
            js_rule_count,
            runtime_url_configured = self.runtime_url.is_some(),
            "generic_rule sandbox artifacts cached"
        );

        Ok(())
    }

    async fn process_event(
        &mut self,
        event: &UnifiedEvent,
        _now: DateTime<Utc>,
        repo: &PostgresRepository,
    ) -> Result<Option<DetectionResult>> {
        if self.runtime_url.is_none() {
            return Ok(None);
        }

        let mut candidates: Vec<(String, CompiledRuleRef)> = Vec::new();
        for (pattern_id, per_tenant) in &self.compiled_rules {
            if let Some(rules) = per_tenant.get(&event.tenant_id) {
                for rule in rules {
                    candidates.push((pattern_id.clone(), rule.clone()));
                }
            }
        }

        for (pattern_id, rule) in candidates {
            let state_key = Self::state_key(&pattern_id, &rule.rule_id);
            let runtime_state = repo
                .load_pattern_state(&event.tenant_id, &pattern_id, &state_key)
                .await?
                .unwrap_or_else(|| json!({}));

            let runtime = self
                .execute_runtime(&pattern_id, &rule, event, runtime_state)
                .await?;

            let health_key = Self::health_key(&event.tenant_id, &pattern_id, &rule.rule_id);
            if !runtime.ok {
                let already_degraded = self
                    .runtime_degraded
                    .get(&health_key)
                    .copied()
                    .unwrap_or(false);
                if !already_degraded {
                    tracing::warn!(
                        tenant_id = %event.tenant_id,
                        pattern_id = %pattern_id,
                        rule_id = %rule.rule_id,
                        error_code = ?runtime.error_code,
                        "sandbox runtime degraded for rule"
                    );
                }
                self.runtime_degraded.insert(health_key, true);
                continue;
            }
            self.runtime_degraded.insert(health_key, false);

            if let Err(error) = repo
                .upsert_pattern_state(
                    &event.tenant_id,
                    &pattern_id,
                    &state_key,
                    runtime.state.clone(),
                )
                .await
            {
                tracing::warn!(
                    tenant_id = %event.tenant_id,
                    pattern_id = %pattern_id,
                    rule_id = %rule.rule_id,
                    error = ?error,
                    "failed to persist runtime state"
                );
            }

            if runtime.matched_alerts.is_empty() {
                continue;
            }

            let first = &runtime.matched_alerts[0];
            let detection = Self::detection_from_alert(
                &pattern_id,
                &rule.rule_id,
                event,
                first,
                &runtime.trace,
            );

            let snapshot = json!({
                "rule_id": rule.rule_id,
                "engine": rule.engine,
                "language": rule.language,
                "runtime_trace": runtime.trace,
                "runtime_metrics": {
                    "execution_ms": runtime.metrics.execution_ms,
                    "ops_count": runtime.metrics.ops_count,
                },
                "matched_alerts": runtime.matched_alerts,
            });
            let severity = first.severity.to_ascii_lowercase();
            if let Err(error) = repo
                .insert_pattern_snapshot(
                    &event.tenant_id,
                    &pattern_id,
                    &format!("{}:{}", rule.rule_id, event.event_id),
                    snapshot,
                    Some(Self::risk_score_for_severity(&detection.severity)),
                    Some(severity.as_str()),
                )
                .await
            {
                tracing::warn!(
                    tenant_id = %event.tenant_id,
                    pattern_id = %pattern_id,
                    rule_id = %rule.rule_id,
                    error = ?error,
                    "failed to persist runtime snapshot"
                );
            }

            return Ok(Some(detection));
        }

        Ok(None)
    }
}
