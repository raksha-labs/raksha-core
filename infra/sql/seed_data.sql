-- ============================================================================
-- Raksha - Seed Data
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
     '{"market_symbols": ["USDCUSDT", "USDTUSDC", "DAIUSDT"]}'::jsonb,
     'global', NULL, TRUE),
    ('coinbase-advanced', 'cex_websocket', 'coinbase',
     '{"ws_endpoint": "wss://advanced-trade-ws.coinbase.com"}'::jsonb,
     '{"market_symbols": ["USDC-USD", "USDT-USD", "DAI-USD"]}'::jsonb,
     'global', NULL, TRUE),
    ('kraken-spot', 'cex_websocket', 'kraken',
     '{"ws_endpoint": "wss://ws.kraken.com/v2"}'::jsonb,
     '{"market_symbols": ["USDC/USD", "USDT/USD", "DAI/USD"]}'::jsonb,
     'global', NULL, TRUE),
    ('okx-global', 'cex_websocket', 'okx',
     '{"ws_endpoint": "wss://ws.okx.com:8443/ws/v5/public"}'::jsonb,
     '{"market_symbols": ["USDC-USDT", "USDT-USDC", "DAI-USDT"]}'::jsonb,
     'global', NULL, TRUE),
    ('bybit-spot', 'cex_websocket', 'bybit',
     '{"ws_endpoint": "wss://stream.bybit.com/v5/public/spot"}'::jsonb,
     '{"market_symbols": ["USDCUSDT", "USDTUSDC", "DAIUSDT"]}'::jsonb,
     'global', NULL, TRUE),
    ('gemini-spot', 'cex_websocket', 'gemini',
     '{"ws_endpoint": "wss://api.gemini.com/v1/marketdata/{subscription_key}"}'::jsonb,
     '{"market_symbols": ["usdcusd", "usdtusd", "daiusd"]}'::jsonb,
     'global', NULL, TRUE),

    -- Oracle + DEX Log Sources (Ethereum mainnet)
    ('chainlink-eth-mainnet', 'oracle_api', 'chainlink',
     '{"rpc_url": "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}", "chain_id": 1, "chain_slug": "ethereum"}'::jsonb,
     '{"market_symbols": ["USDC/USD", "USDT/USD", "DAI/USD"]}'::jsonb,
     'global', NULL, TRUE),
    ('uniswap-v2-eth-mainnet', 'dex_api', 'uniswap-v2',
     '{"rpc_url": "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}", "chain_id": 1, "chain_slug": "ethereum"}'::jsonb,
     '{"market_symbols": ["USDC/USD", "USDT/USD", "DAI/USD"]}'::jsonb,
     'global', NULL, TRUE),
    ('uniswap-v3-eth-mainnet', 'dex_api', 'uniswap-v3',
     '{"rpc_url": "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}", "chain_id": 1, "chain_slug": "ethereum"}'::jsonb,
     '{"market_symbols": ["USDC/USD", "USDT/USD", "DAI/USD"]}'::jsonb,
     'global', NULL, TRUE),
    ('sushi-v2-eth-mainnet', 'dex_api', 'sushi-v2',
     '{"rpc_url": "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}", "chain_id": 1, "chain_slug": "ethereum"}'::jsonb,
     '{"market_symbols": ["USDC/USD", "USDT/USD", "DAI/USD"]}'::jsonb,
     'global', NULL, TRUE),

    -- EVM Chain Sources
    ('ethereum-mainnet', 'evm_chain', 'ethereum',
     '{"chain_id": 1, "chain_slug": "ethereum", "rpc_url": "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}"}'::jsonb,
     NULL,
     'global', NULL, TRUE),
    ('arbitrum-one', 'evm_chain', 'arbitrum',
     '{"chain_id": 42161, "chain_slug": "arbitrum", "rpc_url": "wss://arb-mainnet.g.alchemy.com/v2/{alchemy_api_key}"}'::jsonb,
     NULL,
     'global', NULL, TRUE)
ON CONFLICT (source_id) DO NOTHING;

-- ─── Default Tenant "glider" ─────────────────────────────────────────────────
-- Associates all default sources and patterns with the built-in tenant.
-- Adjust or remove as appropriate for your deployment.

INSERT INTO tenant_data_sources (tenant_id, source_id, enabled, override_config)
VALUES
    ('glider', 'binance-global', TRUE, '{
      "pair_mappings": [
        {
          "market_key": "USDC/USD",
          "source_symbol": "USDCUSDT",
          "enabled": true
        }
      ]
    }'::jsonb),
    ('glider', 'coinbase-advanced', TRUE, '{}'::jsonb),
    ('glider', 'kraken-spot', TRUE, '{}'::jsonb),
    ('glider', 'okx-global', TRUE, '{}'::jsonb),
    ('glider', 'bybit-spot', TRUE, '{}'::jsonb),
    ('glider', 'gemini-spot', TRUE, '{}'::jsonb),
    ('glider', 'chainlink-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'uniswap-v2-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'uniswap-v3-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'sushi-v2-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'ethereum-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'arbitrum-one', TRUE, '{}'::jsonb)
ON CONFLICT (tenant_id, source_id) DO NOTHING;

-- ─── Default Stream Configs (created by glider) ────────────────────────────

WITH desired_stream_configs AS (
  SELECT *
  FROM (
    VALUES
      -- Binance (USDT quoted)
      ('binance-global','websocket','miniTicker','usdcusdt@miniTicker','quote','binance_miniticker_v1','USDC/USD','USDCUSDT','{"symbols":["USDCUSDT"]}'::jsonb,NULL::text,'{}'::jsonb,'$.E','ms',NULL,TRUE,'glider'),
      ('binance-global','websocket','miniTicker','usdtusdc@miniTicker','quote','binance_miniticker_v1','USDT/USD','USDTUSDC','{"symbols":["USDTUSDC"]}'::jsonb,NULL::text,'{}'::jsonb,'$.E','ms',NULL,TRUE,'glider'),
      ('binance-global','websocket','miniTicker','daiusdt@miniTicker','quote','binance_miniticker_v1','DAI/USD','DAIUSDT','{"symbols":["DAIUSDT"]}'::jsonb,NULL::text,'{}'::jsonb,'$.E','ms',NULL,TRUE,'glider'),

      -- Coinbase (USD direct)
      ('coinbase-advanced','websocket','ticker','USDC-USD','quote','coinbase_ticker_v1','USDC/USD','USDC-USD','{"subscribe_message":{"type":"subscribe","channel":"ticker","product_ids":["USDC-USD"]}}'::jsonb,NULL::text,'{}'::jsonb,'$.timestamp','iso8601',NULL,TRUE,'glider'),
      ('coinbase-advanced','websocket','ticker','USDT-USD','quote','coinbase_ticker_v1','USDT/USD','USDT-USD','{"subscribe_message":{"type":"subscribe","channel":"ticker","product_ids":["USDT-USD"]}}'::jsonb,NULL::text,'{}'::jsonb,'$.timestamp','iso8601',NULL,TRUE,'glider'),
      ('coinbase-advanced','websocket','ticker','DAI-USD','quote','coinbase_ticker_v1','DAI/USD','DAI-USD','{"subscribe_message":{"type":"subscribe","channel":"ticker","product_ids":["DAI-USD"]}}'::jsonb,NULL::text,'{}'::jsonb,'$.timestamp','iso8601',NULL,TRUE,'glider'),

      -- Kraken (USD direct)
      ('kraken-spot','websocket','ticker','USDC/USD','quote','kraken_ticker_v2','USDC/USD','USDC/USD','{"subscribe_message":{"method":"subscribe","params":{"channel":"ticker","symbol":["USDC/USD"]},"req_id":1}}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',NULL,TRUE,'glider'),
      ('kraken-spot','websocket','ticker','USDT/USD','quote','kraken_ticker_v2','USDT/USD','USDT/USD','{"subscribe_message":{"method":"subscribe","params":{"channel":"ticker","symbol":["USDT/USD"]},"req_id":1}}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',NULL,TRUE,'glider'),
      ('kraken-spot','websocket','ticker','DAI/USD','quote','kraken_ticker_v2','DAI/USD','DAI/USD','{"subscribe_message":{"method":"subscribe","params":{"channel":"ticker","symbol":["DAI/USD"]},"req_id":1}}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',NULL,TRUE,'glider'),

      -- OKX (USDT quoted)
      ('okx-global','websocket','tickers','USDC-USDT','quote','okx_tickers_v5','USDC/USD','USDC-USDT','{"subscribe_message":{"op":"subscribe","args":[{"channel":"tickers","instId":"USDC-USDT"}]}}'::jsonb,NULL::text,'{}'::jsonb,'$.data[0].ts','ms',NULL,TRUE,'glider'),
      ('okx-global','websocket','tickers','USDT-USDC','quote','okx_tickers_v5','USDT/USD','USDT-USDC','{"subscribe_message":{"op":"subscribe","args":[{"channel":"tickers","instId":"USDT-USDC"}]}}'::jsonb,NULL::text,'{}'::jsonb,'$.data[0].ts','ms',NULL,TRUE,'glider'),
      ('okx-global','websocket','tickers','DAI-USDT','quote','okx_tickers_v5','DAI/USD','DAI-USDT','{"subscribe_message":{"op":"subscribe","args":[{"channel":"tickers","instId":"DAI-USDT"}]}}'::jsonb,NULL::text,'{}'::jsonb,'$.data[0].ts','ms',NULL,TRUE,'glider'),

      -- Bybit (USDT quoted)
      ('bybit-spot','websocket','tickers','USDCUSDT','quote','bybit_tickers_v5','USDC/USD','USDCUSDT','{"subscribe_message":{"op":"subscribe","args":["tickers.USDCUSDT"]}}'::jsonb,NULL::text,'{}'::jsonb,'$.ts','ms',NULL,TRUE,'glider'),
      ('bybit-spot','websocket','tickers','USDTUSDC','quote','bybit_tickers_v5','USDT/USD','USDTUSDC','{"subscribe_message":{"op":"subscribe","args":["tickers.USDTUSDC"]}}'::jsonb,NULL::text,'{}'::jsonb,'$.ts','ms',NULL,TRUE,'glider'),
      ('bybit-spot','websocket','tickers','DAIUSDT','quote','bybit_tickers_v5','DAI/USD','DAIUSDT','{"subscribe_message":{"op":"subscribe","args":["tickers.DAIUSDT"]}}'::jsonb,NULL::text,'{}'::jsonb,'$.ts','ms',NULL,TRUE,'glider'),

      -- Gemini (USD direct, endpoint template uses subscription_key)
      ('gemini-spot','websocket','marketdata','usdcusd','quote','gemini_marketdata_v1','USDC/USD','USDCUSD','{}'::jsonb,NULL::text,'{}'::jsonb,'$.timestampms','ms',NULL,TRUE,'glider'),
      ('gemini-spot','websocket','marketdata','usdtusd','quote','gemini_marketdata_v1','USDT/USD','USDTUSD','{}'::jsonb,NULL::text,'{}'::jsonb,'$.timestampms','ms',NULL,TRUE,'glider'),
      ('gemini-spot','websocket','marketdata','daiusd','quote','gemini_marketdata_v1','DAI/USD','DAIUSD','{}'::jsonb,NULL::text,'{}'::jsonb,'$.timestampms','ms',NULL,TRUE,'glider'),

      -- Chainlink (Ethereum mainnet logs)
      ('chainlink-eth-mainnet','rpc_logs','logs','usdc-usd-feed','oracle_update','chainlink_answer_updated_v1','USDC/USD','USDCUSD','{"addresses":["0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6"],"topics":["0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f"],"decimals":8}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('chainlink-eth-mainnet','rpc_logs','logs','usdt-usd-feed','oracle_update','chainlink_answer_updated_v1','USDT/USD','USDTUSD','{"addresses":["0x3E7d1eAB13ad0104d2750B8863b489D65364e32D"],"topics":["0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f"],"decimals":8}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('chainlink-eth-mainnet','rpc_logs','logs','dai-usd-feed','oracle_update','chainlink_answer_updated_v1','DAI/USD','DAIUSD','{"addresses":["0xAed0c38402a5d19df6E4c03F4E2DceD6e29c1ee9"],"topics":["0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f"],"decimals":8}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),

      -- Uniswap V2 (Ethereum mainnet logs)
      ('uniswap-v2-eth-mainnet','rpc_logs','logs','uni-v2-usdc-usdt','swap','uniswap_v2_swap_price_v1','USDC/USD','USDCUSDT','{"addresses":["0x3041CbD36888bECc7bbCBc0045E3B1f144466f5f"],"topics":["0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"],"token0_symbol":"USDC","token1_symbol":"USDT","token0_decimals":6,"token1_decimals":6,"base_symbol":"USDC"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('uniswap-v2-eth-mainnet','rpc_logs','logs','uni-v2-usdt-usdc','swap','uniswap_v2_swap_price_v1','USDT/USD','USDTUSDC','{"addresses":["0x3041CbD36888bECc7bbCBc0045E3B1f144466f5f"],"topics":["0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"],"token0_symbol":"USDC","token1_symbol":"USDT","token0_decimals":6,"token1_decimals":6,"base_symbol":"USDT"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('uniswap-v2-eth-mainnet','rpc_logs','logs','uni-v2-dai-usdt','swap','uniswap_v2_swap_price_v1','DAI/USD','DAIUSDT','{"addresses":["0x1f98A4a54f8D9f3b9B6Da3f68A2B4E8C8D718a51"],"topics":["0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"],"token0_symbol":"DAI","token1_symbol":"USDT","token0_decimals":18,"token1_decimals":6,"base_symbol":"DAI"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),

      -- Uniswap V3 (Ethereum mainnet logs)
      ('uniswap-v3-eth-mainnet','rpc_logs','logs','uni-v3-usdc-usdt','swap','uniswap_v3_swap_price_v1','USDC/USD','USDCUSDT','{"addresses":["0x3416cF6C708Da44DB2624D63ea0AAef7113527C6"],"topics":["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"],"token0_symbol":"USDC","token1_symbol":"USDT","token0_decimals":6,"token1_decimals":6,"base_symbol":"USDC"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('uniswap-v3-eth-mainnet','rpc_logs','logs','uni-v3-usdt-usdc','swap','uniswap_v3_swap_price_v1','USDT/USD','USDTUSDC','{"addresses":["0x3416cF6C708Da44DB2624D63ea0AAef7113527C6"],"topics":["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"],"token0_symbol":"USDC","token1_symbol":"USDT","token0_decimals":6,"token1_decimals":6,"base_symbol":"USDT"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('uniswap-v3-eth-mainnet','rpc_logs','logs','uni-v3-dai-usdt','swap','uniswap_v3_swap_price_v1','DAI/USD','DAIUSDT','{"addresses":["0x48DA0965ab2d2cbf1c17c09cfb5cbe67ad5b1406"],"topics":["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"],"token0_symbol":"DAI","token1_symbol":"USDT","token0_decimals":18,"token1_decimals":6,"base_symbol":"DAI"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),

      -- Sushi V2 (same event shape as UniV2)
      ('sushi-v2-eth-mainnet','rpc_logs','logs','sushi-v2-usdc-usdt','swap','uniswap_v2_swap_price_v1','USDC/USD','USDCUSDT','{"addresses":["0x397FF1542f962076d0BFE58eA045FfA2d347ACa0"],"topics":["0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"],"token0_symbol":"USDC","token1_symbol":"USDT","token0_decimals":6,"token1_decimals":6,"base_symbol":"USDC"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('sushi-v2-eth-mainnet','rpc_logs','logs','sushi-v2-usdt-usdc','swap','uniswap_v2_swap_price_v1','USDT/USD','USDTUSDC','{"addresses":["0x397FF1542f962076d0BFE58eA045FfA2d347ACa0"],"topics":["0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"],"token0_symbol":"USDC","token1_symbol":"USDT","token0_decimals":6,"token1_decimals":6,"base_symbol":"USDT"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('sushi-v2-eth-mainnet','rpc_logs','logs','sushi-v2-dai-usdt','swap','uniswap_v2_swap_price_v1','DAI/USD','DAIUSDT','{"addresses":["0xC3D03e4f041FdA8Ff4F9fB6A90f0A6f2fA2f6C9A"],"topics":["0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"],"token0_symbol":"DAI","token1_symbol":"USDT","token0_decimals":18,"token1_decimals":6,"base_symbol":"DAI"}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider')
  ) AS t(
    source_id,
    connector_mode,
    stream_name,
    subscription_key,
    event_type,
    parser_name,
    market_key,
    asset_pair,
    filter_config,
    auth_secret_ref,
    auth_config,
    payload_ts_path,
    payload_ts_unit,
    poll_interval_ms,
    enabled,
    created_by
  )
)
INSERT INTO source_stream_configs (
  source_id,
  connector_mode,
  stream_name,
  subscription_key,
  event_type,
  parser_name,
  market_key,
  asset_pair,
  filter_config,
  auth_secret_ref,
  auth_config,
  payload_ts_path,
  payload_ts_unit,
  poll_interval_ms,
  enabled,
  created_by
)
SELECT
  ds.source_id,
  ds.connector_mode,
  ds.stream_name,
  ds.subscription_key,
  ds.event_type,
  ds.parser_name,
  ds.market_key,
  ds.asset_pair,
  ds.filter_config,
  ds.auth_secret_ref,
  ds.auth_config,
  ds.payload_ts_path,
  ds.payload_ts_unit,
  ds.poll_interval_ms,
  ds.enabled,
  ds.created_by
FROM desired_stream_configs ds
WHERE EXISTS (
  SELECT 1
  FROM data_sources src
  WHERE src.source_id = ds.source_id
)
AND NOT EXISTS (
  SELECT 1
  FROM source_stream_configs ssc
  WHERE ssc.source_id = ds.source_id
    AND ssc.stream_name = ds.stream_name
    AND COALESCE(ssc.asset_pair, '') = COALESCE(ds.asset_pair, '')
    AND COALESCE(ssc.subscription_key, '') = COALESCE(ds.subscription_key, '')
);

WITH desired_stream_refs AS (
  SELECT source_id, stream_name, subscription_key, asset_pair
  FROM (
    VALUES
      ('binance-global','miniTicker','usdcusdt@miniTicker','USDCUSDT'),
      ('binance-global','miniTicker','usdtusdc@miniTicker','USDTUSDC'),
      ('binance-global','miniTicker','daiusdt@miniTicker','DAIUSDT'),
      ('coinbase-advanced','ticker','USDC-USD','USDC-USD'),
      ('coinbase-advanced','ticker','USDT-USD','USDT-USD'),
      ('coinbase-advanced','ticker','DAI-USD','DAI-USD'),
      ('kraken-spot','ticker','USDC/USD','USDC/USD'),
      ('kraken-spot','ticker','USDT/USD','USDT/USD'),
      ('kraken-spot','ticker','DAI/USD','DAI/USD'),
      ('okx-global','tickers','USDC-USDT','USDC-USDT'),
      ('okx-global','tickers','USDT-USDC','USDT-USDC'),
      ('okx-global','tickers','DAI-USDT','DAI-USDT'),
      ('bybit-spot','tickers','USDCUSDT','USDCUSDT'),
      ('bybit-spot','tickers','USDTUSDC','USDTUSDC'),
      ('bybit-spot','tickers','DAIUSDT','DAIUSDT'),
      ('gemini-spot','marketdata','usdcusd','USDCUSD'),
      ('gemini-spot','marketdata','usdtusd','USDTUSD'),
      ('gemini-spot','marketdata','daiusd','DAIUSD'),
      ('chainlink-eth-mainnet','logs','usdc-usd-feed','USDCUSD'),
      ('chainlink-eth-mainnet','logs','usdt-usd-feed','USDTUSD'),
      ('chainlink-eth-mainnet','logs','dai-usd-feed','DAIUSD'),
      ('uniswap-v2-eth-mainnet','logs','uni-v2-usdc-usdt','USDCUSDT'),
      ('uniswap-v2-eth-mainnet','logs','uni-v2-usdt-usdc','USDTUSDC'),
      ('uniswap-v2-eth-mainnet','logs','uni-v2-dai-usdt','DAIUSDT'),
      ('uniswap-v3-eth-mainnet','logs','uni-v3-usdc-usdt','USDCUSDT'),
      ('uniswap-v3-eth-mainnet','logs','uni-v3-usdt-usdc','USDTUSDC'),
      ('uniswap-v3-eth-mainnet','logs','uni-v3-dai-usdt','DAIUSDT'),
      ('sushi-v2-eth-mainnet','logs','sushi-v2-usdc-usdt','USDCUSDT'),
      ('sushi-v2-eth-mainnet','logs','sushi-v2-usdt-usdc','USDTUSDC'),
      ('sushi-v2-eth-mainnet','logs','sushi-v2-dai-usdt','DAIUSDT')
  ) AS t(source_id, stream_name, subscription_key, asset_pair)
)
INSERT INTO source_stream_tenant_targets (
  stream_config_id,
  tenant_id,
  enabled,
  created_by
)
SELECT
  ssc.stream_config_id,
  'glider',
  TRUE,
  'glider'
FROM source_stream_configs ssc
JOIN desired_stream_refs ds
  ON ds.source_id = ssc.source_id
 AND ds.stream_name = ssc.stream_name
 AND COALESCE(ds.subscription_key, '') = COALESCE(ssc.subscription_key, '')
 AND COALESCE(ds.asset_pair, '') = COALESCE(ssc.asset_pair, '')
ON CONFLICT (stream_config_id, tenant_id) DO NOTHING;

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

-- ─── History Intelligence Seed (Replay Catalog + Cases) ─────────────────────

INSERT INTO history.cases (
  case_id,
  tenant_id,
  case_type,
  classification,
  severity_peak,
  status,
  chain_slug,
  protocol,
  title,
  summary,
  incident_start_at,
  incident_end_at,
  loss_usd_estimate,
  source_confidence,
  source_payload
)
VALUES
  (
    'case-usdc-depeg-svb-2023',
    'glider',
    'market_stress',
    'stablecoin_depeg',
    'critical',
    'closed',
    'ethereum',
    'circle-usdc',
    'USDC Depeg During SVB (Mar 2023)',
    'USDC deviated sharply from peg after SVB exposure disclosures before recovering.',
    '2023-03-10T00:00:00Z',
    '2023-03-13T23:59:59Z',
    'Market dislocation event; no direct protocol exploit loss estimate',
    0.97,
    '{"source":"seed"}'::jsonb
  ),
  (
    'case-ust-collapse-2022',
    'glider',
    'market_stress',
    'stablecoin_depeg',
    'critical',
    'closed',
    'terra',
    'terra-ust',
    'UST Collapse (May 2022)',
    'Algorithmic stablecoin UST entered a sustained depeg spiral with cascading liquidity collapse.',
    '2022-05-07T00:00:00Z',
    '2022-05-14T23:59:59Z',
    '~40B ecosystem value drawdown',
    0.95,
    '{"source":"seed"}'::jsonb
  ),
  (
    'case-mango-oracle-manip-2022',
    'glider',
    'exploit',
    'oracle_manipulation',
    'critical',
    'closed',
    'solana',
    'mango-markets',
    'Mango Markets Oracle Manipulation (Oct 2022)',
    'Attacker manipulated oracle-linked pricing and extracted protocol value.',
    '2022-10-11T00:00:00Z',
    '2022-10-12T23:59:59Z',
    '~114M',
    0.94,
    '{"source":"seed"}'::jsonb
  ),
  (
    'case-euler-exploit-2023',
    'glider',
    'exploit',
    'flash_loan_manipulation',
    'critical',
    'closed',
    'ethereum',
    'euler',
    'Euler Finance Exploit (Mar 2023)',
    'Flash-loan-assisted exploit sequence affecting Euler lending markets.',
    '2023-03-13T00:00:00Z',
    '2023-03-18T23:59:59Z',
    '~197M',
    0.96,
    '{"source":"seed"}'::jsonb
  )
ON CONFLICT (case_id) DO NOTHING;

INSERT INTO history.replay_catalog (
  scenario_id,
  tenant_id,
  case_id,
  slug,
  title,
  category,
  tags,
  incident_class,
  chain,
  protocol,
  protocol_category,
  description,
  impact_summary,
  losses_usd_estimate,
  attack_vector,
  detection_focus,
  default_time_window_start,
  default_time_window_end,
  default_speed,
  supported_patterns,
  supported_override_keys,
  baseline_expected_alerts,
  expected_alerts_json,
  timeline_json,
  references_json,
  source_feeds_json,
  runbook_notes,
  dataset_version,
  object_prefix,
  checksum,
  simlab_scenario_id,
  is_active
)
VALUES
  (
    'scn-usdc-depeg-svb-2023',
    'glider',
    'case-usdc-depeg-svb-2023',
    'usdc-depeg-svb-2023',
    'USDC Depeg During SVB (Mar 2023)',
    'market_stress',
    ARRAY['stablecoin', 'depeg', 'cex', 'dex', 'contagion'],
    'stablecoin_depeg',
    'ethereum',
    'circle-usdc',
    'stablecoin',
    'USDC deviated sharply from peg after SVB exposure disclosures before recovering after policy intervention.',
    'Cross-venue dislocation and temporary confidence shock.',
    'Market dislocation event; no direct protocol exploit loss estimate',
    'Liquidity shock and bank-run dynamics.',
    'Consensus divergence and sustained peg violations.',
    '2023-03-10T00:00:00Z',
    '2023-03-13T23:59:59Z',
    30,
    ARRAY['dpeg'],
    ARRAY['ingestion.source_filter', 'detection.thresholds', 'alerting.policy'],
    18,
    '[{"pattern_id":"dpeg","severity":"critical","title":"USDC sustained peg break","rationale":"Consensus median deviation remains above threshold","delivery_status":"blocked_simulation"}]'::jsonb,
    '[{"at":"2023-03-10T21:00:00Z","label":"Exposure concerns rise","detail":"Market reacts to reserve exposure uncertainty"},{"at":"2023-03-11T03:00:00Z","label":"Deep discount phase","detail":"USDC trades materially below peg"}]'::jsonb,
    '[{"title":"Circle update on reserve exposure","url":"https://www.circle.com/blog/an-update-on-usdc-and-silicon-valley-bank","published_at":"2023-03-11"}]'::jsonb,
    '[{"source_id":"cex.binance.usdcusdt","label":"Binance USDC/USDT","kind":"cex"},{"source_id":"dex.uniswapv3.usdcusdt","label":"Uniswap V3 USDC/USDT","kind":"dex"}]'::jsonb,
    ARRAY['Verify severity bands and cooldown policy before sharing to BA users.'],
    'v1',
    'simulation/scenarios/usdc-depeg-svb-2023/v1',
    'sha256:7eb36d9c9b241dbd0ea2ccf7ac514f3ab5fb2f31ef8cd0708f2f78411fed7d57',
    NULL,
    TRUE
  ),
  (
    'scn-ust-collapse-2022',
    'glider',
    'case-ust-collapse-2022',
    'ust-collapse-2022',
    'UST Collapse (May 2022)',
    'market_stress',
    ARRAY['stablecoin', 'depeg', 'reflexive', 'contagion'],
    'stablecoin_depeg',
    'terra',
    'terra-ust',
    'stablecoin',
    'Algorithmic stablecoin UST entered a sustained depeg spiral with cascading liquidity collapse.',
    'Systemic ecosystem collapse and prolonged deviation.',
    '~40B ecosystem value drawdown',
    'Reflexive design failure under liquidity stress.',
    'Rapid peg divergence acceleration and prolonged non-recovery windows.',
    '2022-05-07T00:00:00Z',
    '2022-05-14T23:59:59Z',
    45,
    ARRAY['dpeg'],
    ARRAY['ingestion.source_filter', 'detection.thresholds', 'alerting.policy'],
    34,
    '[{"pattern_id":"dpeg","severity":"critical","title":"UST terminal depeg pattern","rationale":"Deviation persists and worsens after sustained windows","delivery_status":"blocked_simulation"}]'::jsonb,
    '[{"at":"2022-05-08T04:00:00Z","label":"Initial break","detail":"UST slips below peg and does not mean-revert"},{"at":"2022-05-12T00:00:00Z","label":"Severe dislocation","detail":"Market pricing reflects failure scenario"}]'::jsonb,
    '[{"title":"Nansen UST depeg analysis","url":"https://www.nansen.ai/research/on-chain-forensics-demystifying-terrausd-depeg","published_at":"2022-06-15"}]'::jsonb,
    '[{"source_id":"cex.binance.ustusdt","label":"Binance UST/USDT","kind":"cex"},{"source_id":"dex.curve.ust3pool","label":"Curve UST-3Pool","kind":"dex"}]'::jsonb,
    ARRAY['Stress test cooldown settings against rapid alert cascades.'],
    'v1',
    'simulation/scenarios/ust-collapse-2022/v1',
    'sha256:26f5429974bcf6f32f29915a48dd0720e17ac6738ce951cae1c79ea5d33d813d',
    NULL,
    TRUE
  ),
  (
    'scn-mango-oracle-manip-2022',
    'glider',
    'case-mango-oracle-manip-2022',
    'mango-oracle-manipulation-2022',
    'Mango Markets Oracle Manipulation (Oct 2022)',
    'exploit',
    ARRAY['oracle', 'manipulation', 'solana', 'derivatives'],
    'oracle_manipulation',
    'solana',
    'mango-markets',
    'derivatives',
    'Attacker manipulated oracle-linked pricing and extracted protocol value.',
    'Oracle trust failure and insolvency-triggering collateral distortion.',
    '~114M',
    'Price manipulation with leveraged position construction.',
    'Oracle/reference divergence and abrupt collateral valuation jumps.',
    '2022-10-11T00:00:00Z',
    '2022-10-12T23:59:59Z',
    25,
    ARRAY['oracle_manipulation'],
    ARRAY['ingestion.source_filter', 'detection.rules', 'alerting.policy'],
    9,
    '[{"pattern_id":"oracle_manipulation","severity":"critical","title":"Oracle/reference divergence exceeds hard bounds","rationale":"Observed price path diverges from independent sources","delivery_status":"blocked_simulation"}]'::jsonb,
    '[{"at":"2022-10-11T20:15:00Z","label":"Manipulation leg","detail":"Aggressive buys force valuation jump"},{"at":"2022-10-11T21:10:00Z","label":"Extraction leg","detail":"Borrowing capacity rises followed by rapid outflow"}]'::jsonb,
    '[{"title":"Mango post-incident writeup","url":"https://mango.markets/blog/mango-markets-exploit-analysis"}]'::jsonb,
    '[{"source_id":"oracle.pyth.mngo","label":"Pyth MNGO","kind":"oracle"},{"source_id":"dex.serum.mngo","label":"Serum MNGO","kind":"dex"}]'::jsonb,
    ARRAY['Inspect oracle divergence residuals and confidence drops.'],
    'v1',
    'simulation/scenarios/mango-oracle-manipulation-2022/v1',
    'sha256:cbcb91714e3f0fd4f522f31f20e6688f087ca6ca2836f0f35d019102f6f2af5a',
    NULL,
    TRUE
  ),
  (
    'scn-euler-exploit-2023',
    'glider',
    'case-euler-exploit-2023',
    'euler-exploit-2023',
    'Euler Finance Exploit (Mar 2023)',
    'exploit',
    ARRAY['exploit', 'flash-loan', 'lending', 'ethereum'],
    'flash_loan_manipulation',
    'ethereum',
    'euler',
    'lending',
    'Flash-loan-assisted exploit sequence affecting Euler lending markets.',
    'Rapid loss event with follow-on remediation and fund recovery activity.',
    '~197M',
    'Exploit chain leveraging liquidity and protocol logic interactions.',
    'Flash loan outlier detection and correlated anomaly triggers.',
    '2023-03-13T00:00:00Z',
    '2023-03-18T23:59:59Z',
    20,
    ARRAY['flash_loan'],
    ARRAY['ingestion.source_filter', 'detection.rules', 'alerting.policy'],
    11,
    '[{"pattern_id":"flash_loan","severity":"critical","title":"Abnormal flash loan with exploit signature","rationale":"Loan size and state transitions match risk profile","delivery_status":"blocked_simulation"}]'::jsonb,
    '[{"at":"2023-03-13T09:30:00Z","label":"Primary exploit tx","detail":"Core exploit transactions executed"},{"at":"2023-03-13T10:00:00Z","label":"Secondary movements","detail":"Funds moved across venues"}]'::jsonb,
    '[{"title":"Euler recovery background","url":"https://www.euler.finance/blog/war-peace-behind-the-scenes-of-eulers-240m-exploit-recovery","published_at":"2023-04-04"}]'::jsonb,
    '[{"source_id":"onchain.ethereum.euler","label":"Euler On-chain Tx Stream","kind":"onchain"},{"source_id":"oracle.chainlink.ethusd","label":"Chainlink ETH/USD","kind":"oracle"}]'::jsonb,
    ARRAY['Ensure flash-loan thresholds trigger before downstream confirmation delays.'],
    'v1',
    'simulation/scenarios/euler-exploit-2023/v1',
    'sha256:a52df1734593a33db7b9fcd5879f7c6ecf8f6f9dedfbb77ccefa428f4c8a6cda',
    'euler_2023_flash_loan',
    TRUE
  )
ON CONFLICT (scenario_id) DO NOTHING;

-- ============================================================================
-- Post-Seeding Notes
-- ============================================================================
-- After running this seed data:
-- 1. Update data source connection configs with real API keys
-- 2. Customize tenant pattern configs based on your monitoring requirements
-- 3. Add additional tenants via admin API or direct SQL inserts
-- 4. Configure notification channels in tenant_policies.route_overrides
-- ============================================================================
