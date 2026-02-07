use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use common_types::{EventStatus, LifecycleState, NormalizedEvent, ProtocolCategory};
use core_interfaces::ChainAdapter;
use uuid::Uuid;

use crate::normalize::chain_from_slug;

#[derive(Debug, Clone)]
pub struct MockProtocol {
    pub protocol: String,
    pub protocol_category: ProtocolCategory,
    pub oracle_price: f64,
    pub reference_price: f64,
}

pub struct EvmMockAdapter {
    chain_slug: String,
    chain_id: u64,
    confirmation_depth: u64,
    protocols: Vec<MockProtocol>,
    tick: u64,
}

impl EvmMockAdapter {
    pub fn new(chain_slug: impl Into<String>, chain_id: u64, protocols: Vec<MockProtocol>) -> Self {
        let chain_slug = chain_slug.into();
        Self {
            confirmation_depth: default_confirmation_depth_for_chain(&chain_slug),
            chain_slug,
            chain_id,
            protocols,
            tick: 0,
        }
    }

    pub fn with_confirmation_depth(mut self, confirmation_depth: u64) -> Self {
        self.confirmation_depth = confirmation_depth.max(1);
        self
    }

    pub fn single_protocol(
        chain_slug: impl Into<String>,
        chain_id: u64,
        protocol: impl Into<String>,
        protocol_category: ProtocolCategory,
        oracle_price: f64,
        reference_price: f64,
    ) -> Self {
        Self::new(
            chain_slug,
            chain_id,
            vec![MockProtocol {
                protocol: protocol.into(),
                protocol_category,
                oracle_price,
                reference_price,
            }],
        )
    }
}

#[async_trait]
impl ChainAdapter for EvmMockAdapter {
    async fn next_events(&mut self) -> Result<Vec<NormalizedEvent>> {
        let mut events = Vec::new();
        let tick = self.tick;
        self.tick = self.tick.saturating_add(1);

        for (idx, protocol) in self.protocols.iter().enumerate() {
            let mut metadata = HashMap::new();
            metadata.insert("oracle_pair".to_string(), serde_json::json!("ETH/USD"));
            metadata.insert(
                "source".to_string(),
                serde_json::json!("chain_adapter_evm_mock"),
            );
            metadata.insert("mock_tick".to_string(), serde_json::json!(tick));

            let block_number = 19_000_000 + tick + idx as u64;
            let tx_hash = format!("0x{}mocktx{:02}", self.chain_slug, idx);

            events.push(NormalizedEvent {
                event_id: Uuid::new_v4(),
                event_key: format!("{}:{}:{}:{}", self.chain_slug, block_number, tx_hash, tick),
                tenant_id: None,
                chain: chain_from_slug(&self.chain_slug),
                chain_slug: self.chain_slug.clone(),
                protocol: protocol.protocol.clone(),
                protocol_category: protocol.protocol_category.clone(),
                chain_id: Some(self.chain_id),
                tx_hash,
                block_number,
                block_hash: None,
                parent_hash: None,
                tx_index: Some(0),
                log_index: Some(0),
                status: EventStatus::Observed,
                lifecycle_state: LifecycleState::Provisional,
                requires_confirmation: true,
                confirmation_depth: self.confirmation_depth,
                ingest_latency_ms: Some(25),
                observed_at: Utc::now(),
                oracle_price: Some(protocol.oracle_price),
                reference_price: Some(protocol.reference_price),
                metadata,
            });
        }

        Ok(events)
    }

    fn chain_name(&self) -> &'static str {
        "evm"
    }
}

fn default_confirmation_depth_for_chain(chain_slug: &str) -> u64 {
    match chain_slug {
        "base" => 64,
        _ => 3,
    }
}
