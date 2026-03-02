use std::collections::HashMap;

use chrono::Utc;
use event_schema::{Chain, EventStatus, EventType, LifecycleState, NormalizedEvent, ProtocolCategory};
use ethers::types::Log;
use uuid::Uuid;

use crate::{
    decode_chainlink::extract_round_id, decode_flashloan::DecodedFlashLoanLog,
    protocol_map::ProtocolBinding,
};

pub fn chain_from_slug(chain_slug: &str) -> Chain {
    match chain_slug {
        "ethereum" => Chain::Ethereum,
        "base" => Chain::Base,
        "offchain" => Chain::Offchain,
        _ => Chain::Unknown,
    }
}

#[allow(clippy::too_many_arguments)]
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
        event_type: EventType::OracleUpdate,
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

#[allow(clippy::too_many_arguments)]
pub fn normalize_flash_loan_candidate_event(
    chain_slug: &str,
    chain_id: u64,
    confirmation_depth: u64,
    source_id: &str,
    protocol: &str,
    protocol_category: ProtocolCategory,
    asset_pair_hint: Option<&str>,
    log: &Log,
    decoded: &DecodedFlashLoanLog,
    source: &str,
) -> NormalizedEvent {
    let tx_hash = log
        .transaction_hash
        .map(|hash| format!("{:?}", hash))
        .unwrap_or_else(|| "0x".to_string());
    let block_number = log.block_number.unwrap_or_default().as_u64();
    let log_index = log.log_index.unwrap_or_default().as_u64();
    let event_key = format!(
        "{chain_slug}:{block_number}:{tx_hash}:{log_index}:{}:{}",
        sanitize_protocol(protocol),
        sanitize_protocol(source_id)
    );

    let mut metadata = HashMap::new();
    metadata.insert("flash_loan.detected".to_string(), serde_json::json!(true));
    metadata.insert(
        "flash_loan.source_id".to_string(),
        serde_json::json!(source_id),
    );
    metadata.insert(
        "flash_loan.protocol".to_string(),
        serde_json::json!(protocol),
    );
    metadata.insert(
        "flash_loan.asset_address".to_string(),
        serde_json::json!(
            decoded
                .asset_address
                .map(|asset| format!("{asset:?}"))
                .unwrap_or_default()
        ),
    );
    metadata.insert(
        "flash_loan.loan_amount_raw".to_string(),
        serde_json::json!(decoded.loan_amount_raw.to_string()),
    );
    metadata.insert(
        "flash_loan.fee_raw".to_string(),
        serde_json::json!(decoded.fee_raw.map(|fee| fee.to_string())),
    );
    metadata.insert(
        "flash_loan.borrower".to_string(),
        serde_json::json!(decoded.borrower.map(|borrower| format!("{borrower:?}"))),
    );
    metadata.insert(
        "flash_loan.initiator".to_string(),
        serde_json::json!(decoded.initiator.map(|initiator| format!("{initiator:?}"))),
    );
    metadata.insert(
        "flash_loan.detection_method".to_string(),
        serde_json::json!("event_signature"),
    );
    metadata.insert(
        "flash_loan.event_signature_source".to_string(),
        serde_json::json!(source),
    );
    if let Some(hint) = asset_pair_hint {
        metadata.insert(
            "flash_loan.asset_pair_hint".to_string(),
            serde_json::json!(hint),
        );
    }

    NormalizedEvent {
        event_id: Uuid::new_v4(),
        event_key,
        event_type: EventType::FlashLoanCandidate,
        tenant_id: None,
        chain: chain_from_slug(chain_slug),
        chain_slug: chain_slug.to_string(),
        protocol: protocol.to_string(),
        protocol_category,
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
        oracle_price: None,
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
        assert_eq!(event.event_type, event_schema::EventType::OracleUpdate);
        assert_eq!(event.chain_slug, "ethereum");
    }
}
