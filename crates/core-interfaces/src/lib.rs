use anyhow::Result;
use async_trait::async_trait;
use common_types::{
    AlertEvent, DependencyEdge, DetectionResult, FeatureVector, FinalityUpdate, NormalizedEvent,
    ReorgNotice, RiskScore, RuleSimulationReport,
};
use serde_json::Value;

#[async_trait]
pub trait ChainAdapter: Send + Sync {
    async fn next_events(&mut self) -> Result<Vec<NormalizedEvent>>;
    fn chain_name(&self) -> &'static str;

    async fn subscribe_heads(&mut self) -> Result<()> {
        Ok(())
    }

    async fn subscribe_logs(&mut self) -> Result<()> {
        Ok(())
    }

    async fn backfill_range(
        &mut self,
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<NormalizedEvent>> {
        Ok(Vec::new())
    }

    async fn latest_block_number(&mut self) -> Result<Option<u64>> {
        Ok(None)
    }

    fn chain_id(&self) -> Option<u64> {
        None
    }
}

#[async_trait]
pub trait RuleEvaluator: Send + Sync {
    async fn evaluate(&self, event: &NormalizedEvent) -> Result<DetectionResult>;

    async fn evaluate_window(
        &self,
        events: &[NormalizedEvent],
        _window_ms: u64,
    ) -> Result<Vec<DetectionResult>> {
        let mut out = Vec::new();
        for event in events {
            out.push(self.evaluate(event).await?);
        }
        Ok(out)
    }
}

#[async_trait]
pub trait RiskScorer: Send + Sync {
    async fn score(&self, result: &DetectionResult) -> Result<RiskScore>;
}

#[async_trait]
pub trait AlertSink: Send + Sync {
    async fn send(&self, alert: &AlertEvent) -> Result<()>;
    fn sink_name(&self) -> &'static str;
}

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish_json(&self, stream: &str, payload: &Value) -> Result<()>;
    async fn healthcheck(&self) -> Result<()>;
}

#[async_trait]
pub trait FinalityEngine: Send + Sync {
    async fn apply_update(&self, update: &FinalityUpdate) -> Result<()>;
    async fn handle_reorg_notice(&self, notice: &ReorgNotice) -> Result<()>;
}

#[async_trait]
pub trait CorrelationEngine: Send + Sync {
    async fn correlate_detection(&self, detection: &DetectionResult) -> Result<Option<String>>;
    async fn dependency_edges_for_protocol(&self, protocol: &str) -> Result<Vec<DependencyEdge>>;
}

#[async_trait]
pub trait TenantPolicyResolver: Send + Sync {
    async fn resolve_alert_routes(
        &self,
        tenant_id: Option<&str>,
        detection: &DetectionResult,
    ) -> Result<Vec<String>>;
}

#[async_trait]
pub trait FeatureExtractor: Send + Sync {
    async fn extract(&self, detection: &DetectionResult) -> Result<FeatureVector>;
}

#[async_trait]
pub trait RuleSimulator: Send + Sync {
    async fn simulate(
        &self,
        tenant_id: Option<&str>,
        rule_id: &str,
        fixture_name: &str,
    ) -> Result<RuleSimulationReport>;
}
