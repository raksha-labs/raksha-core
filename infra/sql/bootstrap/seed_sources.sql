-- Raksha bootstrap seed: sources
-- ─── Data Sources ────────────────────────────────────────────────────────────
-- These are example sources. Real deployments should configure their own
-- via the admin API or by inserting rows directly with actual API keys.

INSERT INTO catalog.data_sources (
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
     '{"market_symbols": ["USDCUSDT", "DAIUSDT"]}'::jsonb,
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
    ('binance-global-http', 'cex_websocket', 'binance-http',
     '{"http_url": "https://api.binance.com/api/v3/ticker/24hr?symbol={subscription_key}"}'::jsonb,
     '{"market_symbols": ["USDCUSDT", "DAIUSDT"]}'::jsonb,
     'global', NULL, TRUE),
    ('kraken-spot-http', 'cex_websocket', 'kraken-http',
     '{"http_url": "https://api.kraken.com/0/public/Ticker?pair={subscription_key}"}'::jsonb,
     '{"market_symbols": ["USDCUSD", "USDTUSD", "DAIUSD"]}'::jsonb,
     'global', NULL, TRUE),
    ('okx-global-http', 'cex_websocket', 'okx-http',
     '{"http_url": "https://www.okx.com/api/v5/market/ticker?instId={subscription_key}"}'::jsonb,
     '{"market_symbols": ["USDC-USDT", "USDT-USDC", "DAI-USDT"]}'::jsonb,
     'global', NULL, TRUE),
    ('bybit-spot-http', 'cex_websocket', 'bybit-http',
     '{"http_url": "https://api.bybit.com/v5/market/tickers?category=spot&symbol={subscription_key}"}'::jsonb,
     '{"market_symbols": ["USDCUSDT", "USDTUSDC", "DAIUSDT"]}'::jsonb,
     'global', NULL, TRUE),
    ('gemini-spot-http', 'cex_websocket', 'gemini-http',
     '{"http_url": "https://api.gemini.com/v1/pubticker/{subscription_key}"}'::jsonb,
     '{"market_symbols": ["usdcusd", "usdtusd", "daiusd"]}'::jsonb,
     'global', NULL, TRUE),
    ('gate-spot-http', 'cex_websocket', 'gate-http',
     '{"http_url": "https://api.gateio.ws/api/v4/spot/tickers?currency_pair={subscription_key}"}'::jsonb,
     '{"market_symbols": ["USDC_USDT", "DAI_USDT"]}'::jsonb,
     'global', NULL, TRUE),

    -- Oracle + DEX Log Sources (Ethereum mainnet)
    ('chainlink-eth-mainnet', 'oracle_api', 'chainlink',
     '{"rpc_url": "wss://eth-mainnet.g.alchemy.com/v2/{alchemy_api_key}", "chain_id": 1, "chain_slug": "ethereum"}'::jsonb,
     '{"market_symbols": ["USDC/USD", "USDT/USD", "DAI/USD"]}'::jsonb,
     'global', NULL, TRUE),
    ('chainlink-data-streams', 'oracle_api', 'chainlink-data-streams',
     '{"endpoint": "https://api.dataengine.chain.link", "ws_endpoint": "wss://ws.dataengine.chain.link"}'::jsonb,
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
     'global', NULL, TRUE),
    ('defillama-http', 'custom_api', 'defillama',
     '{"http_base_url": "https://api.llama.fi"}'::jsonb,
     NULL,
     'global', NULL, TRUE)
ON CONFLICT (source_id) DO NOTHING;

-- ─── Default Tenant "glider" ─────────────────────────────────────────────────
-- Associates all default sources and patterns with the built-in tenant.
-- Adjust or remove as appropriate for your deployment.

INSERT INTO catalog.tenant_data_sources (tenant_id, source_id, enabled, override_config)
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
    ('glider', 'binance-global-http', TRUE, '{}'::jsonb),
    ('glider', 'kraken-spot-http', TRUE, '{}'::jsonb),
    ('glider', 'okx-global-http', TRUE, '{}'::jsonb),
    ('glider', 'bybit-spot-http', TRUE, '{}'::jsonb),
    ('glider', 'gemini-spot-http', TRUE, '{}'::jsonb),
    ('glider', 'gate-spot-http', TRUE, '{}'::jsonb),
    ('glider', 'chainlink-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'chainlink-data-streams', TRUE, '{}'::jsonb),
    ('glider', 'uniswap-v2-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'uniswap-v3-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'sushi-v2-eth-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'ethereum-mainnet', TRUE, '{}'::jsonb),
    ('glider', 'arbitrum-one', TRUE, '{}'::jsonb),
    ('glider', 'defillama-http', TRUE, '{}'::jsonb)
ON CONFLICT (tenant_id, source_id) DO NOTHING;

-- ─── Default Stream Configs (created by glider) ────────────────────────────

WITH desired_stream_configs AS (
  SELECT *
  FROM (
    VALUES
      -- Binance (USDT quoted)
      ('binance-global','websocket','miniTicker','usdcusdt@miniTicker','quote','binance_miniticker_v1','USDC/USD','USDCUSDT','{"symbols":["USDCUSDT"]}'::jsonb,NULL::text,'{}'::jsonb,'$.E','ms',NULL,TRUE,'glider'),
      ('binance-global','websocket','miniTicker','daiusdt@miniTicker','quote','binance_miniticker_v1','DAI/USD','DAIUSDT','{"symbols":["DAIUSDT"]}'::jsonb,NULL::text,'{}'::jsonb,'$.E','ms',NULL,FALSE,'glider'),

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

      -- Public CEX HTTP polls
      ('binance-global-http','http_poll','ticker_24hr','USDCUSDT','quote','binance_miniticker_v1','USDC/USD','USDCUSDT','{}'::jsonb,NULL::text,'{}'::jsonb,'$.closeTime','ms',5000,TRUE,'glider'),
      ('binance-global-http','http_poll','ticker_24hr','DAIUSDT','quote','binance_miniticker_v1','DAI/USD','DAIUSDT','{}'::jsonb,NULL::text,'{}'::jsonb,'$.closeTime','ms',5000,TRUE,'glider'),
      ('kraken-spot-http','http_poll','ticker','USDCUSD','quote','kraken_ticker_v2','USDC/USD','USDC/USD','{}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',5000,TRUE,'glider'),
      ('kraken-spot-http','http_poll','ticker','USDTUSD','quote','kraken_ticker_v2','USDT/USD','USDT/USD','{}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',5000,TRUE,'glider'),
      ('kraken-spot-http','http_poll','ticker','DAIUSD','quote','kraken_ticker_v2','DAI/USD','DAI/USD','{}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',5000,TRUE,'glider'),
      ('okx-global-http','http_poll','ticker','USDC-USDT','quote','okx_tickers_v5','USDC/USD','USDC-USDT','{}'::jsonb,NULL::text,'{}'::jsonb,'$.data[0].ts','ms',5000,TRUE,'glider'),
      ('okx-global-http','http_poll','ticker','USDT-USDC','quote','okx_tickers_v5','USDT/USD','USDT-USDC','{}'::jsonb,NULL::text,'{}'::jsonb,'$.data[0].ts','ms',5000,TRUE,'glider'),
      ('okx-global-http','http_poll','ticker','DAI-USDT','quote','okx_tickers_v5','DAI/USD','DAI-USDT','{}'::jsonb,NULL::text,'{}'::jsonb,'$.data[0].ts','ms',5000,TRUE,'glider'),
      ('bybit-spot-http','http_poll','ticker','USDCUSDT','quote','bybit_tickers_v5','USDC/USD','USDCUSDT','{}'::jsonb,NULL::text,'{}'::jsonb,'$.time','ms',5000,TRUE,'glider'),
      ('bybit-spot-http','http_poll','ticker','USDTUSDC','quote','bybit_tickers_v5','USDT/USD','USDTUSDC','{}'::jsonb,NULL::text,'{}'::jsonb,'$.time','ms',5000,TRUE,'glider'),
      ('bybit-spot-http','http_poll','ticker','DAIUSDT','quote','bybit_tickers_v5','DAI/USD','DAIUSDT','{}'::jsonb,NULL::text,'{}'::jsonb,'$.time','ms',5000,TRUE,'glider'),
      ('gemini-spot-http','http_poll','pubticker','usdcusd','quote','gemini_marketdata_v1','USDC/USD','USDCUSD','{}'::jsonb,NULL::text,'{}'::jsonb,'$.volume.timestamp','ms',5000,TRUE,'glider'),
      ('gemini-spot-http','http_poll','pubticker','usdtusd','quote','gemini_marketdata_v1','USDT/USD','USDTUSD','{}'::jsonb,NULL::text,'{}'::jsonb,'$.volume.timestamp','ms',5000,TRUE,'glider'),
      ('gemini-spot-http','http_poll','pubticker','daiusd','quote','gemini_marketdata_v1','DAI/USD','DAIUSD','{}'::jsonb,NULL::text,'{}'::jsonb,'$.volume.timestamp','ms',5000,TRUE,'glider'),
      ('gate-spot-http','http_poll','tickers','USDC_USDT','quote','gate_ticker_v4','USDC/USD','USDC_USDT','{}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',5000,TRUE,'glider'),
      ('gate-spot-http','http_poll','tickers','DAI_USDT','quote','gate_ticker_v4','DAI/USD','DAI_USDT','{}'::jsonb,NULL::text,'{}'::jsonb,NULL,'ms',5000,TRUE,'glider'),

      -- Chainlink (Ethereum mainnet logs)
      ('chainlink-eth-mainnet','rpc_logs','logs','usdc-usd-feed','oracle_update','chainlink_answer_updated_v1','USDC/USD','USDCUSD','{"addresses":["0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6"],"topics":["0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f"],"decimals":8}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('chainlink-eth-mainnet','rpc_logs','logs','usdt-usd-feed','oracle_update','chainlink_answer_updated_v1','USDT/USD','USDTUSD','{"addresses":["0x3E7d1eAB13ad0104d2750B8863b489D65364e32D"],"topics":["0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f"],"decimals":8}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('chainlink-eth-mainnet','rpc_logs','logs','dai-usd-feed','oracle_update','chainlink_answer_updated_v1','DAI/USD','DAIUSD','{"addresses":["0xAed0c38402a5d19df6E4c03F4E2DceD6e29c1ee9"],"topics":["0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f"],"decimals":8}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),

      -- Chainlink Data Streams (HTTP poll via Chainlink Rust SDK)
      ('chainlink-data-streams','websocket','latest_report','usdc-usd-feed','oracle_update','chainlink_data_streams_v3','USDC/USD','USDCUSD','{"feed_id_env":"CHAINLINK_DATA_STREAMS_FEED_ID_USDC_USD","price_decimals":8}'::jsonb,NULL::text,'{"api_key_env":"CHAINLINK_DATA_STREAMS_API_KEY","user_secret_env":"CHAINLINK_DATA_STREAMS_USER_SECRET"}'::jsonb,NULL,'s',NULL,FALSE,'glider'),
      ('chainlink-data-streams','websocket','latest_report','usdt-usd-feed','oracle_update','chainlink_data_streams_v3','USDT/USD','USDTUSD','{"feed_id_env":"CHAINLINK_DATA_STREAMS_FEED_ID_USDT_USD","price_decimals":8}'::jsonb,NULL::text,'{"api_key_env":"CHAINLINK_DATA_STREAMS_API_KEY","user_secret_env":"CHAINLINK_DATA_STREAMS_USER_SECRET"}'::jsonb,NULL,'s',NULL,FALSE,'glider'),
      ('chainlink-data-streams','websocket','latest_report','dai-usd-feed','oracle_update','chainlink_data_streams_v3','DAI/USD','DAIUSD','{"feed_id_env":"CHAINLINK_DATA_STREAMS_FEED_ID_DAI_USD","price_decimals":8}'::jsonb,NULL::text,'{"api_key_env":"CHAINLINK_DATA_STREAMS_API_KEY","user_secret_env":"CHAINLINK_DATA_STREAMS_USER_SECRET"}'::jsonb,NULL,'s',NULL,FALSE,'glider'),

      -- Pyth Hermes (HTTP poll every 2 seconds)
      ('pyth-eth-mainnet','http_poll','latest_price','0xeaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a','oracle_update','pyth_hermes_v2','USDC/USD','USDCUSD','{"use_spot_price":false}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('pyth-eth-mainnet','http_poll','latest_price','0x2b89b9dc8fdf9f34709a5b106b472f0f39bb6ca9ce04b0fd7f2e971688e2e53b','oracle_update','pyth_hermes_v2','USDT/USD','USDTUSD','{"use_spot_price":false}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),
      ('pyth-eth-mainnet','http_poll','latest_price','0xb0948a5e5313200c632b51bb5ca32f6de0d36e9950a942d19751e833f70dabfd','oracle_update','pyth_hermes_v2','DAI/USD','DAIUSD','{"use_spot_price":false}'::jsonb,NULL::text,'{}'::jsonb,NULL,'s',2000,FALSE,'glider'),

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
INSERT INTO catalog.source_stream_configs (
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
  FROM catalog.data_sources src
  WHERE src.source_id = ds.source_id
)
AND NOT EXISTS (
  SELECT 1
  FROM catalog.source_stream_configs ssc
  WHERE ssc.source_id = ds.source_id
    AND ssc.stream_name = ds.stream_name
    AND COALESCE(ssc.asset_pair, '') = COALESCE(ds.asset_pair, '')
    AND COALESCE(ssc.subscription_key, '') = COALESCE(ds.subscription_key, '')
);

-- Grant the glider tenant access to the baseline live market/oracle streams
-- used by local development and demo scenarios. Replay test streams are
-- provisioned dynamically per tenant/run by workbench-services.

WITH desired_stream_refs AS (
  SELECT source_id, stream_name, subscription_key, asset_pair
  FROM (
    VALUES
      ('binance-global','miniTicker','usdcusdt@miniTicker','USDCUSDT'),
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
      ('binance-global-http','ticker_24hr','USDCUSDT','USDCUSDT'),
      ('binance-global-http','ticker_24hr','DAIUSDT','DAIUSDT'),
      ('kraken-spot-http','ticker','USDCUSD','USDC/USD'),
      ('kraken-spot-http','ticker','USDTUSD','USDT/USD'),
      ('kraken-spot-http','ticker','DAIUSD','DAI/USD'),
      ('okx-global-http','ticker','USDC-USDT','USDC-USDT'),
      ('okx-global-http','ticker','USDT-USDC','USDT-USDC'),
      ('okx-global-http','ticker','DAI-USDT','DAI-USDT'),
      ('bybit-spot-http','ticker','USDCUSDT','USDCUSDT'),
      ('bybit-spot-http','ticker','USDTUSDC','USDTUSDC'),
      ('bybit-spot-http','ticker','DAIUSDT','DAIUSDT'),
      ('gemini-spot-http','pubticker','usdcusd','USDCUSD'),
      ('gemini-spot-http','pubticker','usdtusd','USDTUSD'),
      ('gemini-spot-http','pubticker','daiusd','DAIUSD'),
      ('gate-spot-http','tickers','USDC_USDT','USDC_USDT'),
      ('gate-spot-http','tickers','DAI_USDT','DAI_USDT'),
      ('chainlink-eth-mainnet','logs','usdc-usd-feed','USDCUSD'),
      ('chainlink-eth-mainnet','logs','usdt-usd-feed','USDTUSD'),
      ('chainlink-eth-mainnet','logs','dai-usd-feed','DAIUSD'),
      ('chainlink-data-streams','latest_report','usdc-usd-feed','USDCUSD'),
      ('chainlink-data-streams','latest_report','usdt-usd-feed','USDTUSD'),
      ('chainlink-data-streams','latest_report','dai-usd-feed','DAIUSD'),
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
INSERT INTO catalog.source_stream_tenant_targets (
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
FROM catalog.source_stream_configs ssc
JOIN desired_stream_refs ds
  ON ds.source_id = ssc.source_id
 AND ds.stream_name = ssc.stream_name
 AND COALESCE(ds.subscription_key, '') = COALESCE(ssc.subscription_key, '')
 AND COALESCE(ds.asset_pair, '') = COALESCE(ssc.asset_pair, '')
ON CONFLICT (stream_config_id, tenant_id) DO NOTHING;

-- ─── DefiLlama HTTP TVL Streams ──────────────────────────────────────────────
WITH protocol_stream_configs AS (
  SELECT *
  FROM (
    VALUES
      ('defillama-http','http_poll','protocol_tvl','aave_v3::ethereum',    'tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"aave_v3","chain_slug":"ethereum"}'::jsonb,    NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider'),
      ('defillama-http','http_poll','protocol_tvl','aave_v3::base',        'tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"aave_v3","chain_slug":"base"}'::jsonb,        NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider'),
      ('defillama-http','http_poll','protocol_tvl','aave_v3::arbitrum',    'tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"aave_v3","chain_slug":"arbitrum"}'::jsonb,    NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider'),
      ('defillama-http','http_poll','protocol_tvl','morpho_blue::ethereum','tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"morpho_blue","chain_slug":"ethereum"}'::jsonb,NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider'),
      ('defillama-http','http_poll','protocol_tvl','compound_v3::ethereum','tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"compound_v3","chain_slug":"ethereum"}'::jsonb,NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider'),
      ('defillama-http','http_poll','protocol_tvl','curve::ethereum',      'tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"curve","chain_slug":"ethereum"}'::jsonb,      NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider'),
      ('defillama-http','http_poll','protocol_tvl','maker::ethereum',      'tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"maker","chain_slug":"ethereum"}'::jsonb,      NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider'),
      ('defillama-http','http_poll','protocol_tvl','uniswap_v3::ethereum', 'tvl_snapshot','protocol_tvl_v1',NULL,NULL,'{"protocol_id":"uniswap_v3","chain_slug":"ethereum"}'::jsonb, NULL,'{}'::jsonb,NULL,'s',60000,TRUE,'glider')
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
INSERT INTO catalog.source_stream_configs (
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
  psc.source_id,
  psc.connector_mode,
  psc.stream_name,
  psc.subscription_key,
  psc.event_type,
  psc.parser_name,
  psc.market_key,
  psc.asset_pair,
  psc.filter_config,
  psc.auth_secret_ref,
  psc.auth_config,
  psc.payload_ts_path,
  psc.payload_ts_unit,
  psc.poll_interval_ms,
  psc.enabled,
  psc.created_by
FROM protocol_stream_configs psc
WHERE EXISTS (
  SELECT 1 FROM catalog.data_sources src WHERE src.source_id = psc.source_id
)
AND NOT EXISTS (
  SELECT 1
  FROM catalog.source_stream_configs ssc
  WHERE ssc.source_id = psc.source_id
    AND ssc.stream_name = psc.stream_name
    AND COALESCE(ssc.subscription_key, '') = COALESCE(psc.subscription_key, '')
);

UPDATE catalog.source_stream_configs
SET connection_config_override = jsonb_build_object(
  'http_url',
  format('https://api.llama.fi/tvl/%s', REPLACE(filter_config->>'protocol_id', '_', '-'))
)
WHERE source_id = 'defillama-http'
  AND operating_mode_profile = 'live'
  AND (connection_config_override IS NULL OR connection_config_override->>'http_url' IS NULL);

INSERT INTO catalog.source_stream_tenant_targets (
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
FROM catalog.source_stream_configs ssc
WHERE ssc.source_id = 'defillama-http'
  AND ssc.operating_mode_profile = 'live'
ON CONFLICT (stream_config_id, tenant_id) DO NOTHING;

-- ─── Default Tenant Operating Mode (local/dev bootstrap) ───────────────────
-- Local bootstrap defaults Glider to TEST mode so replay workflows can start
-- without manual tenant mode changes. Replay test streams are still created
-- dynamically for simulation runs and should not exist as static seeds.
INSERT INTO catalog.tenant_operating_mode (
  tenant_id,
  mode,
  mode_note,
  requested_by,
  requested_at,
  updated_at
)
VALUES (
  'glider',
  'test',
  'Bootstrap default: local/dev uses replay test mode.',
  'bootstrap:test-mode',
  NOW(),
  NOW()
)
ON CONFLICT (tenant_id) DO NOTHING;
