-- ============================================================================
-- DeFi Surveillance - Seed Data
-- ============================================================================
-- This file seeds the database with initial patterns, data sources, and
-- example tenant configuration for the default "glider" tenant.
--
-- Run this file after schema.sql to populate initial data.
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

-- ─── Data Sources ────────────────────────────────────────────────────────────
-- These are example sources. Real deployments should configure their own
-- via the admin API or by inserting rows directly with actual API keys.

INSERT INTO data_sources (
  source_id,
  source_type,
  source_name,
  connection_config,
  filters,
  scope,
  owner_tenant_id,
  enabled
)
VALUES
    -- CEX WebSocket Sources
    ('binance-global', 'cex_websocket', 'binance',
     '{"ws_endpoint": "wss://stream.binance.com:9443/ws"}'::jsonb,
     '{"market_symbols": ["USDCUSDT", "DAIUSDT", "USDTUSDT"]}'::jsonb,
     'global',
     NULL,
     TRUE),

    ('coinbase-advanced', 'cex_websocket', 'coinbase',
     '{"ws_endpoint": "wss://advanced-trade-ws.coinbase.com"}'::jsonb,
     '{"market_symbols": ["USDC-USD", "DAI-USD"]}'::jsonb,
     'global',
     NULL,
     TRUE),

    -- DEX API Sources
    ('uniswap-v3-eth', 'dex_api', 'uniswap-v3',
     '{"ws_endpoint": "wss://api.thegraph.com/subgraphs/name/uniswap/uniswap-v3"}'::jsonb,
     '{"market_symbols": ["USDC/ETH", "DAI/ETH"]}'::jsonb,
     'global',
     NULL,
     TRUE),

    -- Oracle API Sources
    ('chainlink-eth', 'oracle_api', 'chainlink',
     '{"ws_endpoint": "wss://rpc.ankr.com/eth/ws"}'::jsonb,
     '{"market_symbols": ["USDC-USD", "DAI-USD"]}'::jsonb,
     'global',
     NULL,
     TRUE),

    ('pyth-mainnet', 'oracle_api', 'pyth',
     '{"ws_endpoint": "wss://hermes.pyth.network/api/v1/streaming"}'::jsonb,
     '{"market_symbols": ["Crypto.USDC/USD", "Crypto.DAI/USD"]}'::jsonb,
     'global',
     NULL,
     TRUE),

    -- EVM Chain Sources
    -- NOTE: Replace INFURA_KEY and ALCHEMY_KEY with your actual API keys
    ('ethereum-mainnet', 'evm_chain', 'ethereum',
     '{"chain_id": 1, "chain_slug": "ethereum", "rpc_url": "wss://mainnet.infura.io/ws/v3/INFURA_KEY"}'::jsonb,
     NULL,
     'global',
     NULL,
     TRUE),

    ('arbitrum-one', 'evm_chain', 'arbitrum',
     '{"chain_id": 42161, "chain_slug": "arbitrum", "rpc_url": "wss://arb-mainnet.g.alchemy.com/v2/ALCHEMY_KEY"}'::jsonb,
     NULL,
     'global',
     NULL,
     TRUE)
ON CONFLICT (source_id) DO NOTHING;

-- ─── Default Tenant "glider" ─────────────────────────────────────────────────
-- Associates all default sources and patterns with the built-in tenant.
-- Adjust or remove as appropriate for your deployment.

INSERT INTO tenant_data_sources (tenant_id, source_id, enabled)
VALUES
    ('glider', 'binance-global', TRUE),
    ('glider', 'coinbase-advanced', TRUE),
    ('glider', 'uniswap-v3-eth', TRUE),
    ('glider', 'chainlink-eth', TRUE),
    ('glider', 'pyth-mainnet', TRUE),
    ('glider', 'ethereum-mainnet', TRUE),
    ('glider', 'arbitrum-one', TRUE)
ON CONFLICT (tenant_id, source_id) DO NOTHING;

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

    -- Flash Loan: legacy object + multi-rule model compatibility
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
        ],
        "min_loan_amount_usd": 100000,
        "profit_threshold_usd": 1000,
        "cooldown_sec": 300,
        "enabled": true
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

-- ============================================================================
-- Post-Seeding Notes
-- ============================================================================
-- After running this seed data:
-- 1. Update data source connection configs with real API keys
-- 2. Customize tenant pattern configs based on your monitoring requirements
-- 3. Add additional tenants via admin API or direct SQL inserts
-- 4. Configure notification channels in tenant_policies.route_overrides
-- ============================================================================
