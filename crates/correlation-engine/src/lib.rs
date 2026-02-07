use anyhow::Result;
use async_trait::async_trait;
use common_types::{DependencyEdge, DetectionResult};
use core_interfaces::CorrelationEngine;

#[derive(Default)]
pub struct NoopCorrelationEngine;

#[async_trait]
impl CorrelationEngine for NoopCorrelationEngine {
    async fn correlate_detection(&self, detection: &DetectionResult) -> Result<Option<String>> {
        if detection.triggered_rule_ids.is_empty() {
            return Ok(None);
        }

        Ok(Some(format!(
            "incident:{}:{}:{}",
            format!("{:?}", detection.chain).to_lowercase(),
            detection.protocol,
            detection.block_number
        )))
    }

    async fn dependency_edges_for_protocol(&self, protocol: &str) -> Result<Vec<DependencyEdge>> {
        Ok(vec![DependencyEdge {
            source: "chainlink:eth-usd".to_string(),
            target: protocol.to_string(),
            relation: "price_dependency".to_string(),
            weight: Some(1.0),
        }])
    }
}
