use std::collections::HashMap;

use chrono::Utc;
use event_schema::{Chain, EventStatus, LifecycleState, NormalizedEvent};
use ethers::types::Log;
use uuid::Uuid;

use crate::{decode_chainlink::extract_round_id, protocol_map::ProtocolBinding};

pub fn chain_from_slug(chain_slug: &str) -> Chain {
    match chain_slug {
        "ethereum" => Chain::Ethereum,
        "base" => Chain::Base,
        _ => Chain::Unknown,
    }
}

pub fn normalize_answer_updated_event(
    chain_slug: &str,
    chain_id: u64,
    confirmation_depth: u64,
    binding: &ProtocolBinding,
    log: &Log,
    price: f64,
    source: &str,
    binding_index: usize,
    binding_count: usize,
) -> NormalizedEvent {
    let tx_hash = log
        .transaction_hash
        .map(|hash| format!("{:?}", hash))
        .unwrap_or_else(|| "0x".to_string());
    let block_number = log.block_number.unwrap_or_default().as_u64();
    let log_index = log.log_index.unwrap_or_default().as_u64();

    let base_event_key = format!("{chain_slug}:{block_number}:{tx_hash}:{log_index}");
    let event_key = if binding_count > 1 {
        format!("{base_event_key}:{}", sanitize_protocol(&binding.protocol))
    } else {
        base_event_key
    };

    let mut metadata = HashMap::new();
    metadata.insert(
        "oracle_address".to_string(),
        serde_json::json!(format!("{:?}", log.address)),
    );
    metadata.insert("source".to_string(), serde_json::json!(source));
    metadata.insert(
        "binding_index".to_string(),
        serde_json::json!(binding_index),
    );
    if let Some(round_id) = extract_round_id(log) {
        metadata.insert(
            "round_id".to_string(),
            serde_json::json!(format!("{:?}", round_id)),
        );
    }

    NormalizedEvent {
        event_id: Uuid::new_v4(),
        event_key,
        tenant_id: None,
        chain: chain_from_slug(chain_slug),
        chain_slug: chain_slug.to_string(),
        protocol: binding.protocol.clone(),
        protocol_category: binding.protocol_category.clone(),
        chain_id: Some(chain_id),
        tx_hash,
        block_number,
        block_hash: log.block_hash.map(|hash| format!("{:?}", hash)),
        parent_hash: None,
        tx_index: log.transaction_index.map(|idx| idx.as_u64()),
        log_index: Some(log_index),
        status: EventStatus::Observed,
        lifecycle_state: LifecycleState::Provisional,
        requires_confirmation: true,
        confirmation_depth,
        ingest_latency_ms: None,
        observed_at: Utc::now(),
        oracle_price: Some(price),
        reference_price: None,
        metadata,
    }
}

pub fn sanitize_protocol(protocol: &str) -> String {
    protocol
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ethers::types::{Address, Log, H256, U256, U64};

    use super::*;

    #[test]
    fn appends_protocol_suffix_when_multiple_bindings_share_same_log() {
        let binding = ProtocolBinding {
            protocol: "aave-v3".to_string(),
            oracle_decimals: 8,
            protocol_category: event_schema::ProtocolCategory::Lending,
        };

        let mut log = Log::default();
        log.address = Address::zero();
        log.block_number = Some(U64::from(100));
        log.transaction_hash = Some(H256::zero());
        log.log_index = Some(U256::from(7_u64));
        log.topics = vec![H256::zero(), H256::zero(), H256::zero()];

        let event =
            normalize_answer_updated_event("ethereum", 1, 3, &binding, &log, 100.0, "test", 0, 2);

        assert!(event.event_key.ends_with(":aave-v3"));
        assert_eq!(event.chain_slug, "ethereum");
    }
}
