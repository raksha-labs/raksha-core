-- Raksha bootstrap seed: patterns
-- ============================================================================
-- Raksha - Seed Data
-- ============================================================================
-- This file seeds the database with initial patterns, data sources, and
-- example tenant configuration for the default "glider" tenant.
--
-- Run this file after bootstrap/core_schema.sql to populate initial data.
-- All inserts use ON CONFLICT DO NOTHING for safe re-running.
-- ============================================================================

-- ─── Pattern Catalog ─────────────────────────────────────────────────────────

INSERT INTO pattern.patterns (pattern_id, pattern_name, description, enabled)
VALUES
    ('dpeg', 'Stablecoin Depeg Alert',
     'Detects sustained divergence of a pegged asset from its peg target using HTTP-polled CEX and oracle price feeds with configurable polling intervals.',
     TRUE),
    ('dpeg_rpc', 'Stablecoin Depeg Alert (WebSocket)',
     'Detects sustained divergence of a pegged asset from its peg target using real-time WebSocket feeds from exchanges and on-chain oracles.',
     TRUE),
    ('flash_loan', 'Flash Loan Attack', 
     'Detects flash loan attacks by monitoring EVM chain events for anomalous loan + extraction patterns.', 
     TRUE),
    ('utilization_high', 'Protocol High Utilization',
     'Detects sustained high utilization in lending protocols using protocol_state events and per-market or protocol thresholds.',
     TRUE)
ON CONFLICT (pattern_id) DO NOTHING;

-- ─── Pattern Default Configurations ─────────────────────────────────────────

INSERT INTO pattern.pattern_configs (pattern_id, config)
VALUES
    ('dpeg', '{
        "poll_interval_ms": 5000,
        "policies": [
          {
            "market_key": "USDT/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          },
          {
            "market_key": "USDC/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          },
          {
            "market_key": "DAI/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          }
        ]
    }'::jsonb),
    ('dpeg_rpc', '{
        "policies": [
          {
            "market_key": "USDT/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          },
          {
            "market_key": "USDC/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          },
          {
            "market_key": "DAI/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          }
        ]
    }'::jsonb),
    ('flash_loan', '{
        "rules": [
          {
            "rule_id": "flash-default",
            "name": "Default Flash Loan Rule",
            "enabled": true,
            "min_loan_amount_usd": 100000,
            "profit_threshold_usd": 1000,
            "cooldown_sec": 300
          }
        ]
    }'::jsonb),
    ('utilization_high', '{
        "rules": [
          {
            "rule_id": "utilization-default",
            "protocol_id": "aave_v3",
            "chain_slug": "base",
            "scope": "protocol",
            "market_id": null,
            "medium_threshold_pct": 90,
            "high_threshold_pct": 95,
            "critical_threshold_pct": 99,
            "resolution_medium_pct": 85,
            "resolution_high_pct": 88,
            "resolution_critical_pct": 90,
            "resolution_confirmation_blocks": 10,
            "min_tvl_floor_usd": 500000,
            "enabled": true
          }
        ]
    }'::jsonb)
ON CONFLICT (pattern_id) DO NOTHING;

-- ─── Tenant Pattern Configurations ──────────────────────────────────────────

INSERT INTO pattern.tenant_pattern_configs (tenant_id, pattern_id, enabled, config)
VALUES
    -- DPEG (HTTP): Primary — HTTP-polled CEX + oracle price feeds
    ('glider', 'dpeg', TRUE, '{
        "poll_interval_ms": 5000,
        "policies": [
          {
            "market_key": "USDT/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          },
          {
            "market_key": "USDC/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          },
          {
            "market_key": "DAI/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
          }
        ]
    }'::jsonb),

    -- DPEG (WebSocket): Real-time WebSocket feeds from exchanges and on-chain oracles
    ('glider', 'dpeg_rpc', TRUE, '[
        {
            "market_key": "USDT/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
        },
        {
            "market_key": "USDC/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
        },
        {
            "market_key": "DAI/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.0,

            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 5.0},
            "severity_bands_systemic": {"medium": 0.01, "high": 0.25, "critical": 0.25},
            "toggles": {"oracle_confirmation": true, "contagion_detection": true}
        }
    ]'::jsonb),

    -- Flash Loan: rule-based configuration
    ('glider', 'flash_loan', TRUE, '{
        "rules": [
          {
            "rule_id": "flash-default",
            "name": "Default Flash Loan Rule",
            "enabled": true,
            "min_loan_amount_usd": 100000,
            "profit_threshold_usd": 1000,
            "cooldown_sec": 300
          }
        ]
    }'::jsonb),

    -- High Utilization: default protocol-level rule
    ('glider', 'utilization_high', TRUE, '{
        "rules": [
          {
            "rule_id": "utilization-default",
            "protocol_id": "aave_v3",
            "chain_slug": "base",
            "scope": "protocol",
            "market_id": null,
            "medium_threshold_pct": 90,
            "high_threshold_pct": 95,
            "critical_threshold_pct": 99,
            "resolution_medium_pct": 85,
            "resolution_high_pct": 88,
            "resolution_critical_pct": 90,
            "resolution_confirmation_blocks": 10,
            "min_tvl_floor_usd": 500000,
            "enabled": true
          }
        ]
    }'::jsonb)
ON CONFLICT (tenant_id, pattern_id) DO NOTHING;

-- ─── Default Tenant Policy ───────────────────────────────────────────────────

INSERT INTO pattern.tenant_policies (
  tenant_id,
  severity_threshold,
  cooldown_sec,
  default_channels,
  protocol_watchlist,
  route_overrides
)
VALUES
    (
      'glider',
      'medium',
      300,
      '{webhook}'::text[],
      '{}'::text[],
      '{
        "severity:medium": ["webhook"],
        "severity:high": ["webhook", "email"],
        "severity:critical": ["webhook", "slack", "telegram", "discord", "email"]
      }'::jsonb
    )
ON CONFLICT (tenant_id) DO NOTHING;

-- ─── Pattern Ingestion Bindings (backfill from tenant_data_sources) ─────────

INSERT INTO pattern.tenant_pattern_source_bindings (tenant_id, pattern_id, source_id, enabled, binding_config)
SELECT
  tpc.tenant_id,
  tpc.pattern_id,
  tds.source_id,
  tds.enabled,
  '{}'::jsonb
FROM pattern.tenant_pattern_configs tpc
JOIN catalog.tenant_data_sources tds
  ON tds.tenant_id = tpc.tenant_id
JOIN catalog.data_sources ds
  ON ds.source_id = tds.source_id
WHERE tpc.enabled = TRUE
  AND NOT (tpc.pattern_id IN ('dpeg', 'dpeg_rpc') AND ds.source_type = 'dex_api')
ON CONFLICT (tenant_id, pattern_id, source_id) DO NOTHING;

DELETE FROM pattern.tenant_pattern_source_bindings tpsb
USING catalog.data_sources ds
WHERE tpsb.pattern_id IN ('dpeg', 'dpeg_rpc')
  AND tpsb.source_id = ds.source_id
  AND ds.source_type = 'dex_api';

-- ─── Pattern Alerting Policies (backfill from tenant_policies) ──────────────

INSERT INTO pattern.tenant_pattern_alert_policies (
  tenant_id,
  pattern_id,
  severity_threshold,
  cooldown_sec,
  default_channels,
  route_overrides
)
SELECT
  tpc.tenant_id,
  tpc.pattern_id,
  tp.severity_threshold,
  tp.cooldown_sec,
  tp.default_channels,
  tp.route_overrides
FROM pattern.tenant_pattern_configs tpc
JOIN pattern.tenant_policies tp
  ON tp.tenant_id = tpc.tenant_id
WHERE tpc.enabled = TRUE
ON CONFLICT (tenant_id, pattern_id) DO NOTHING;

-- ─── Pattern Notification Channel Overrides (default inherit) ────────────────

INSERT INTO pattern.tenant_pattern_notification_channels (
  tenant_id,
  pattern_id,
  channel,
  enabled,
  config_json,
  use_tenant_default
)
SELECT
  tpc.tenant_id,
  tpc.pattern_id,
  channel_value.channel,
  FALSE,
  '{}'::jsonb,
  TRUE
FROM pattern.tenant_pattern_configs tpc
CROSS JOIN (
  VALUES ('webhook'), ('slack'), ('telegram'), ('discord')
) AS channel_value(channel)
ON CONFLICT (tenant_id, pattern_id, channel) DO NOTHING;
