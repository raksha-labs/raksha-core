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

INSERT INTO patterns (pattern_id, pattern_name, description, enabled)
VALUES
    ('dpeg', 'De-Peg Detection', 
     'Detects sustained divergence of a pegged asset from its peg target using a weighted price consensus across multiple market sources.', 
     TRUE),
    ('flash_loan', 'Flash Loan Attack', 
     'Detects flash loan attacks by monitoring EVM chain events for anomalous loan + extraction patterns.', 
     TRUE)
ON CONFLICT (pattern_id) DO NOTHING;

-- ─── Pattern Default Configurations ─────────────────────────────────────────

INSERT INTO pattern_configs (pattern_id, config)
VALUES
    ('dpeg', '{}'::jsonb),
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
    }'::jsonb)
ON CONFLICT (pattern_id) DO NOTHING;

-- ─── Tenant Pattern Configurations ──────────────────────────────────────────

INSERT INTO tenant_pattern_configs (tenant_id, pattern_id, enabled, config)
VALUES
    -- DPEG: Example policy array monitoring USDC and DAI
    ('glider', 'dpeg', TRUE, '[
        {
            "market_key": "USDC/USD",
            "peg_target": 1.0,
            "min_sources": 3,
            "quorum_pct": 0.6,
            "sustained_window_ms": 60000,
            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 3.0}
        },
        {
            "market_key": "DAI/USD",
            "peg_target": 1.0,
            "min_sources": 2,
            "quorum_pct": 0.5,
            "sustained_window_ms": 60000,
            "cooldown_sec": 300,
            "stale_timeout_ms": 30000,
            "severity_bands": {"medium": 0.5, "high": 1.0, "critical": 3.0}
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
    }'::jsonb)
ON CONFLICT (tenant_id, pattern_id) DO NOTHING;

-- ─── Default Tenant Policy ───────────────────────────────────────────────────

INSERT INTO tenant_policies (tenant_id, severity_threshold, cooldown_sec, default_channels, protocol_watchlist)
VALUES
    ('glider', 'medium', 300, '{webhook}', '{}')
ON CONFLICT (tenant_id) DO NOTHING;

-- ─── Pattern Ingestion Bindings (backfill from tenant_data_sources) ─────────

INSERT INTO tenant_pattern_source_bindings (tenant_id, pattern_id, source_id, enabled, binding_config)
SELECT
  tpc.tenant_id,
  tpc.pattern_id,
  tds.source_id,
  tds.enabled,
  '{}'::jsonb
FROM tenant_pattern_configs tpc
JOIN tenant_data_sources tds
  ON tds.tenant_id = tpc.tenant_id
WHERE tpc.enabled = TRUE
ON CONFLICT (tenant_id, pattern_id, source_id) DO NOTHING;

-- ─── Pattern Alerting Policies (backfill from tenant_policies) ──────────────

INSERT INTO tenant_pattern_alert_policies (
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
FROM tenant_pattern_configs tpc
JOIN tenant_policies tp
  ON tp.tenant_id = tpc.tenant_id
WHERE tpc.enabled = TRUE
ON CONFLICT (tenant_id, pattern_id) DO NOTHING;

-- ─── Pattern Notification Channel Overrides (default inherit) ────────────────

INSERT INTO tenant_pattern_notification_channels (
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
FROM tenant_pattern_configs tpc
CROSS JOIN (
  VALUES ('webhook'), ('slack'), ('telegram'), ('discord')
) AS channel_value(channel)
ON CONFLICT (tenant_id, pattern_id, channel) DO NOTHING;
