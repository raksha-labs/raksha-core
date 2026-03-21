use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
};

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use common::FinalityEngine;
use event_schema::{Chain, FinalityUpdate, LifecycleState, NormalizedEvent, ReorgNotice};

#[derive(Clone, Default)]
pub struct InMemoryFinalityEngine {
    states: Arc<RwLock<HashMap<String, LifecycleState>>>,
}

impl InMemoryFinalityEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_state(&self, event_key: &str) -> Option<LifecycleState> {
        self.states
            .read()
            .ok()
            .and_then(|guard| guard.get(event_key).cloned())
    }
}

#[async_trait]
impl FinalityEngine for InMemoryFinalityEngine {
    async fn apply_update(&self, update: &FinalityUpdate) -> Result<()> {
        if let Ok(mut guard) = self.states.write() {
            guard.insert(update.event_key.clone(), update.lifecycle_state.clone());
        }
        Ok(())
    }

    async fn handle_reorg_notice(&self, notice: &ReorgNotice) -> Result<()> {
        if let Ok(mut guard) = self.states.write() {
            for key in &notice.affected_event_keys {
                guard.insert(key.clone(), LifecycleState::Retracted);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct FinalityBatch {
    pub updates: Vec<FinalityUpdate>,
    pub reorg_notice: Option<ReorgNotice>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BlockEntry {
    block_hash: Option<String>,
    event_keys: Vec<String>,
}

pub struct ChainFinalityTracker {
    chain: Chain,
    default_confirmation_depth: u64,
    head_block: u64,
    blocks: BTreeMap<u64, BlockEntry>,
    states: HashMap<String, LifecycleState>,
}

impl ChainFinalityTracker {
    pub fn new(chain: Chain, default_confirmation_depth: u64) -> Self {
        Self {
            chain,
            default_confirmation_depth: default_confirmation_depth.max(1),
            head_block: 0,
            blocks: BTreeMap::new(),
            states: HashMap::new(),
        }
    }

    pub fn observe_event(&mut self, event: &NormalizedEvent) -> FinalityBatch {
        if event.chain != self.chain {
            return FinalityBatch::default();
        }

        let mut batch = FinalityBatch::default();
        let event_block = event.block_number;

        if let Some(conflict_hash) =
            self.detect_conflicting_hash(event_block, event.block_hash.as_deref())
        {
            let affected = self.collect_affected_event_keys(event_block);
            self.rollback_from_block(event_block);

            if !affected.is_empty() {
                batch.reorg_notice = Some(ReorgNotice {
                    chain: self.chain.clone(),
                    orphaned_from_block: event_block,
                    common_ancestor_block: event_block.saturating_sub(1),
                    affected_event_keys: affected.clone(),
                    noticed_at: Utc::now(),
                });

                for event_key in affected {
                    if self
                        .states
                        .insert(event_key.clone(), LifecycleState::Retracted)
                        != Some(LifecycleState::Retracted)
                    {
                        batch.updates.push(FinalityUpdate {
                            chain: self.chain.clone(),
                            event_key,
                            block_number: event_block,
                            block_hash: Some(conflict_hash.clone()),
                            lifecycle_state: LifecycleState::Retracted,
                            updated_at: Utc::now(),
                        });
                    }
                }
            }
        }

        let entry = self
            .blocks
            .entry(event_block)
            .or_insert_with(|| BlockEntry {
                block_hash: event.block_hash.clone(),
                event_keys: Vec::new(),
            });
        if entry.block_hash.is_none() {
            entry.block_hash = event.block_hash.clone();
        }
        if !entry.event_keys.iter().any(|key| key == &event.event_key) {
            entry.event_keys.push(event.event_key.clone());
        }

        self.head_block = self.head_block.max(event_block);

        if !self.states.contains_key(&event.event_key) {
            self.states
                .insert(event.event_key.clone(), LifecycleState::Provisional);
            batch.updates.push(FinalityUpdate {
                chain: self.chain.clone(),
                event_key: event.event_key.clone(),
                block_number: event.block_number,
                block_hash: event.block_hash.clone(),
                lifecycle_state: LifecycleState::Provisional,
                updated_at: Utc::now(),
            });
        }

        let confirmation_depth = if event.requires_confirmation {
            event.confirmation_depth.max(1)
        } else {
            self.default_confirmation_depth
        };
        let finalized_height = self.head_block.saturating_sub(confirmation_depth);
        for (block_number, block_entry) in self.blocks.iter() {
            if *block_number > finalized_height {
                break;
            }

            for event_key in &block_entry.event_keys {
                if self.states.get(event_key) == Some(&LifecycleState::Provisional) {
                    self.states
                        .insert(event_key.clone(), LifecycleState::Confirmed);
                    batch.updates.push(FinalityUpdate {
                        chain: self.chain.clone(),
                        event_key: event_key.clone(),
                        block_number: *block_number,
                        block_hash: block_entry.block_hash.clone(),
                        lifecycle_state: LifecycleState::Confirmed,
                        updated_at: Utc::now(),
                    });
                }
            }
        }

        batch
    }

    pub fn apply_reorg_notice(&mut self, notice: &ReorgNotice) -> Vec<FinalityUpdate> {
        if notice.chain != self.chain {
            return Vec::new();
        }

        self.rollback_from_block(notice.orphaned_from_block);
        notice
            .affected_event_keys
            .iter()
            .filter_map(|event_key| {
                if self
                    .states
                    .insert(event_key.clone(), LifecycleState::Retracted)
                    == Some(LifecycleState::Retracted)
                {
                    return None;
                }
                Some(FinalityUpdate {
                    chain: self.chain.clone(),
                    event_key: event_key.clone(),
                    block_number: notice.orphaned_from_block,
                    block_hash: None,
                    lifecycle_state: LifecycleState::Retracted,
                    updated_at: Utc::now(),
                })
            })
            .collect()
    }

    fn detect_conflicting_hash(
        &self,
        block_number: u64,
        incoming_hash: Option<&str>,
    ) -> Option<String> {
        let existing = self.blocks.get(&block_number)?;
        let existing_hash = existing.block_hash.as_deref()?;
        let incoming_hash = incoming_hash?;

        if existing_hash != incoming_hash {
            Some(incoming_hash.to_string())
        } else {
            None
        }
    }

    fn collect_affected_event_keys(&self, orphaned_from_block: u64) -> Vec<String> {
        self.blocks
            .range(orphaned_from_block..)
            .flat_map(|(_, entry)| entry.event_keys.iter().cloned())
            .collect()
    }

    fn rollback_from_block(&mut self, orphaned_from_block: u64) {
        self.blocks.split_off(&orphaned_from_block);
        self.head_block = self.blocks.keys().next_back().copied().unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use event_schema::{EventStatus, EventType, ProtocolCategory};
    use uuid::Uuid;

    use super::*;

    fn mk_event(event_key: &str, block_number: u64, block_hash: &str) -> NormalizedEvent {
        NormalizedEvent {
            event_id: Uuid::new_v4(),
            event_key: event_key.to_string(),
            event_type: EventType::OracleUpdate,
            tenant_id: None,
            chain: Chain::Ethereum,
            chain_slug: "ethereum".to_string(),
            protocol: "aave-v3".to_string(),
            protocol_category: ProtocolCategory::Lending,
            chain_id: Some(1),
            tx_hash: format!("0x{event_key}"),
            block_number,
            block_hash: Some(block_hash.to_string()),
            parent_hash: None,
            tx_index: Some(0),
            log_index: Some(0),
            status: EventStatus::Observed,
            lifecycle_state: LifecycleState::Provisional,
            requires_confirmation: true,
            confirmation_depth: 2,
            ingest_latency_ms: Some(10),
            observed_at: Utc::now(),
            oracle_price: Some(100.0),
            reference_price: Some(99.0),
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn confirms_events_after_confirmation_depth() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        let first = tracker.observe_event(&mk_event("ev-1", 100, "0xaaa"));
        assert!(first.updates.iter().any(|update| update.event_key == "ev-1"
            && update.lifecycle_state == LifecycleState::Provisional));
        assert!(!first
            .updates
            .iter()
            .any(|update| update.lifecycle_state == LifecycleState::Confirmed));

        let second = tracker.observe_event(&mk_event("ev-2", 101, "0xbbb"));
        assert!(!second
            .updates
            .iter()
            .any(|update| update.lifecycle_state == LifecycleState::Confirmed));

        let third = tracker.observe_event(&mk_event("ev-3", 102, "0xccc"));
        assert!(third.updates.iter().any(|update| {
            update.event_key == "ev-1" && update.lifecycle_state == LifecycleState::Confirmed
        }));
    }

    #[test]
    fn emits_retraction_on_conflicting_hash_for_same_block() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("ev-1", 100, "0xaaa"));
        tracker.observe_event(&mk_event("ev-2", 101, "0xbbb"));
        let reorg = tracker.observe_event(&mk_event("ev-3", 101, "0xccc"));

        assert!(reorg.reorg_notice.is_some());
        let notice = reorg.reorg_notice.expect("notice must exist");
        assert_eq!(notice.orphaned_from_block, 101);
        assert!(notice.affected_event_keys.iter().any(|key| key == "ev-2"));
        assert!(reorg.updates.iter().any(|update| {
            update.event_key == "ev-2" && update.lifecycle_state == LifecycleState::Retracted
        }));
        assert!(reorg.updates.iter().any(|update| {
            update.event_key == "ev-3" && update.lifecycle_state == LifecycleState::Provisional
        }));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // BA/QA Test Suite — Finality & Reorg (Test_Cases_DepegV1_0)
    // ═══════════════════════════════════════════════════════════════════════

    // ─── 10. Finality Lifecycle (TC-D-1000 to TC-D-1004) ──────────────────
    // NOTE: Current code uses Provisional→Confirmed (not TENTATIVE→SAFE→FINALIZED).
    // The orchestrator maps: Provisional→Tentative, Confirmed→Finalized.
    // There is no SAFE intermediate state in the current implementation.
    // Tests validate the core finality tracker behaviour which underpins the spec.

    /// TC-D-1000: PROVISIONAL → CONFIRMED finality progression
    /// Spec: TENTATIVE → SAFE → FINALIZED (maps to Provisional → Confirmed in code)
    #[test]
    fn tc_d_1000_finality_provisional_to_confirmed() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        // Block 100: event enters as Provisional
        let batch1 = tracker.observe_event(&mk_event("depeg-alert-1", 100, "0xaaa"));
        assert!(batch1.updates.iter().any(|u| {
            u.event_key == "depeg-alert-1" && u.lifecycle_state == LifecycleState::Provisional
        }));

        // Block 101: still provisional (depth=2, need 2 more blocks)
        let batch2 = tracker.observe_event(&mk_event("ev-filler-101", 101, "0xbbb"));
        assert!(!batch2
            .updates
            .iter()
            .any(|u| u.event_key == "depeg-alert-1"
                && u.lifecycle_state == LifecycleState::Confirmed));

        // Block 102: confirmation depth reached → Confirmed
        let batch3 = tracker.observe_event(&mk_event("ev-filler-102", 102, "0xccc"));
        assert!(batch3.updates.iter().any(|u| {
            u.event_key == "depeg-alert-1" && u.lifecycle_state == LifecycleState::Confirmed
        }));
    }

    /// TC-D-1001: Multiple events confirmed in order
    #[test]
    fn tc_d_1001_multiple_events_confirmed_in_sequence() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("depeg-1", 100, "0xaaa"));
        tracker.observe_event(&mk_event("depeg-2", 101, "0xbbb"));
        let batch = tracker.observe_event(&mk_event("ev-103", 103, "0xddd"));

        // Block 100 event should be confirmed (103 - 2 = 101, so block 100 and 101 confirmed)
        assert!(batch
            .updates
            .iter()
            .any(|u| u.event_key == "depeg-1" && u.lifecycle_state == LifecycleState::Confirmed));
        assert!(batch
            .updates
            .iter()
            .any(|u| u.event_key == "depeg-2" && u.lifecycle_state == LifecycleState::Confirmed));
    }

    /// TC-D-1002: No finality confirmation for retracted (invalidated) event
    #[test]
    fn tc_d_1002_no_finality_for_retracted_event() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        // Block 100: event enters
        tracker.observe_event(&mk_event("depeg-1", 100, "0xaaa"));
        // Block 101: another event
        tracker.observe_event(&mk_event("depeg-2", 101, "0xbbb"));
        // Reorg at block 101: depeg-2 retracted
        let reorg_batch = tracker.observe_event(&mk_event("depeg-2-canonical", 101, "0xfff"));
        assert!(reorg_batch.reorg_notice.is_some());

        // Block 102 and 103: advance chain
        tracker.observe_event(&mk_event("ev-102", 102, "0xeee"));
        let batch = tracker.observe_event(&mk_event("ev-103", 103, "0xggg"));

        // depeg-2 was retracted and should NOT appear as Confirmed
        assert!(!batch
            .updates
            .iter()
            .any(|u| u.event_key == "depeg-2" && u.lifecycle_state == LifecycleState::Confirmed));
    }

    /// Helper: create event that uses tracker's default confirmation depth.
    fn mk_event_default_depth(
        event_key: &str,
        block_number: u64,
        block_hash: &str,
    ) -> NormalizedEvent {
        let mut event = mk_event(event_key, block_number, block_hash);
        event.requires_confirmation = false; // Use tracker's default depth
        event
    }

    /// TC-D-1003: Finality depth of 1 confirms immediately on next block
    #[test]
    fn tc_d_1003_finality_depth_one() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 1);

        tracker.observe_event(&mk_event_default_depth("depeg-1", 100, "0xaaa"));
        let batch = tracker.observe_event(&mk_event_default_depth("ev-101", 101, "0xbbb"));

        // With default depth=1: finalized_height = 101-1 = 100, so block 100 confirmed
        assert!(batch
            .updates
            .iter()
            .any(|u| u.event_key == "depeg-1" && u.lifecycle_state == LifecycleState::Confirmed));
    }

    /// TC-D-1004: Confirmation depth of 0 is clamped to 1
    #[test]
    fn tc_d_1004_confirmation_depth_clamped_to_one() {
        // Constructor clamps to max(1)
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 0);

        tracker.observe_event(&mk_event_default_depth("depeg-1", 100, "0xaaa"));
        let batch = tracker.observe_event(&mk_event_default_depth("ev-101", 101, "0xbbb"));

        // depth=0 clamped to 1, so confirmed on next block
        assert!(batch
            .updates
            .iter()
            .any(|u| u.event_key == "depeg-1" && u.lifecycle_state == LifecycleState::Confirmed));
    }

    // ─── 11. Reorg Corrections (TC-D-1100 to TC-D-1105) ──────────────────

    /// TC-D-1100: Retraction — event only on orphaned fork
    #[test]
    fn tc_d_1100_retraction_orphaned_fork_event() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("depeg-alert", 100, "0xaaa"));
        tracker.observe_event(&mk_event("ev-101", 101, "0xbbb"));

        // Reorg at block 100 — depeg-alert was on orphaned chain
        let reorg = tracker.observe_event(&mk_event("canonical-100", 100, "0xzzz"));

        assert!(reorg.reorg_notice.is_some());
        let notice = reorg.reorg_notice.as_ref().unwrap();
        assert_eq!(notice.orphaned_from_block, 100);
        assert!(notice
            .affected_event_keys
            .contains(&"depeg-alert".to_string()));
        assert!(notice.affected_event_keys.contains(&"ev-101".to_string()));

        // Both events retracted
        assert!(reorg.updates.iter().any(|u| {
            u.event_key == "depeg-alert" && u.lifecycle_state == LifecycleState::Retracted
        }));
        assert!(reorg.updates.iter().any(|u| {
            u.event_key == "ev-101" && u.lifecycle_state == LifecycleState::Retracted
        }));
    }

    /// TC-D-1101: apply_reorg_notice retracts specified events
    #[test]
    fn tc_d_1101_apply_reorg_notice_retracts_events() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("depeg-1", 100, "0xaaa"));
        tracker.observe_event(&mk_event("depeg-2", 101, "0xbbb"));

        let notice = ReorgNotice {
            chain: Chain::Ethereum,
            orphaned_from_block: 100,
            common_ancestor_block: 99,
            affected_event_keys: vec!["depeg-1".to_string(), "depeg-2".to_string()],
            noticed_at: Utc::now(),
        };

        let updates = tracker.apply_reorg_notice(&notice);
        assert_eq!(updates.len(), 2);
        assert!(updates
            .iter()
            .all(|u| u.lifecycle_state == LifecycleState::Retracted));
    }

    /// TC-D-1102: Reorg on different chain is ignored
    #[test]
    fn tc_d_1102_reorg_different_chain_ignored() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("depeg-1", 100, "0xaaa"));

        let notice = ReorgNotice {
            chain: Chain::Base, // Different chain
            orphaned_from_block: 100,
            common_ancestor_block: 99,
            affected_event_keys: vec!["depeg-1".to_string()],
            noticed_at: Utc::now(),
        };

        let updates = tracker.apply_reorg_notice(&notice);
        assert!(
            updates.is_empty(),
            "Reorg for different chain should be ignored"
        );
    }

    /// TC-D-1103: Reorg does not double-retract already retracted events
    #[test]
    fn tc_d_1103_no_double_retraction() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("depeg-1", 100, "0xaaa"));
        tracker.observe_event(&mk_event("ev-101", 101, "0xbbb"));

        // First reorg
        let reorg1 = tracker.observe_event(&mk_event("canonical-101", 101, "0xyyy"));
        assert!(reorg1
            .updates
            .iter()
            .any(|u| u.event_key == "ev-101" && u.lifecycle_state == LifecycleState::Retracted));

        // Apply notice again for same events — should not emit duplicate retractions
        let notice = ReorgNotice {
            chain: Chain::Ethereum,
            orphaned_from_block: 101,
            common_ancestor_block: 100,
            affected_event_keys: vec!["ev-101".to_string()],
            noticed_at: Utc::now(),
        };
        let updates = tracker.apply_reorg_notice(&notice);
        // Already retracted → no new updates
        assert!(
            updates.is_empty(),
            "Already retracted events should not produce duplicate updates"
        );
    }

    /// TC-D-1104: Events before reorg point are unaffected
    #[test]
    fn tc_d_1104_events_before_reorg_unaffected() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("depeg-block-99", 99, "0x999"));
        tracker.observe_event(&mk_event("depeg-block-100", 100, "0xaaa"));
        tracker.observe_event(&mk_event("ev-101", 101, "0xbbb"));

        // Reorg at block 101 only
        let reorg = tracker.observe_event(&mk_event("canonical-101", 101, "0xyyy"));

        let notice = reorg.reorg_notice.as_ref().unwrap();
        // Block 99 and 100 events should NOT be affected
        assert!(!notice
            .affected_event_keys
            .contains(&"depeg-block-99".to_string()));
        assert!(!notice
            .affected_event_keys
            .contains(&"depeg-block-100".to_string()));
        // Only block 101 event affected
        assert!(notice.affected_event_keys.contains(&"ev-101".to_string()));
    }

    /// TC-D-1105: Serialization round-trip preserves state
    #[test]
    fn tc_d_1105_serialization_roundtrip() {
        let mut tracker = ChainFinalityTracker::new(Chain::Ethereum, 2);

        tracker.observe_event(&mk_event("depeg-1", 100, "0xaaa"));
        tracker.observe_event(&mk_event("depeg-2", 101, "0xbbb"));
        tracker.observe_event(&mk_event("depeg-3", 102, "0xccc"));

        let json = tracker.to_json().expect("serialize");
        let restored =
            ChainFinalityTracker::from_json(Chain::Ethereum, 2, json).expect("deserialize");

        // Restored tracker should produce same confirmation on next block
        // (depeg-1 at block 100 should be confirmed since head=102, depth=2)
        // The state should already show depeg-1 as Confirmed after the observe_event calls
        let json2 = restored.to_json().expect("serialize restored");
        assert_eq!(
            tracker.to_json().expect("original json"),
            json2,
            "Round-trip should preserve state"
        );
    }
}

// Persistence support for ChainFinalityTracker
impl ChainFinalityTracker {
    /// Serialize the tracker state to JSON for persistence
    pub fn to_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(&FinalityTrackerState {
            head_block: self.head_block,
            blocks: self.blocks.clone(),
            states: self.states.clone(),
        })
    }

    /// Restore tracker from persisted JSON state
    pub fn from_json(
        chain: Chain,
        confirmation_depth: u64,
        json: serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        let state: FinalityTrackerState = serde_json::from_value(json)?;
        Ok(Self {
            chain,
            default_confirmation_depth: confirmation_depth,
            head_block: state.head_block,
            blocks: state.blocks,
            states: state.states,
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FinalityTrackerState {
    head_block: u64,
    blocks: BTreeMap<u64, BlockEntry>,
    states: HashMap<String, LifecycleState>,
}
