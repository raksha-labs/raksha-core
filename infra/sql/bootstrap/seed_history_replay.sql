-- Raksha bootstrap seed: history replay templates for Simlab
-- Seeds baseline historical cases + case_events + replay_catalog rows.
-- Safe to re-run: uses ON CONFLICT upserts.

-- ---------------------------------------------------------------------------
-- 1) Cases
-- ---------------------------------------------------------------------------

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
        'case_bzx_2020_02_14',
        'glider',
        'exploit',
        'flash_loan',
        'critical',
        'closed',
        'ethereum',
        'bzx',
        'bZx Flash Loan Exploit (2020-02-14)',
        'Historical exploit involving flash-loan-backed oracle/price manipulation and collateral extraction.',
        '2020-02-14T03:55:00Z'::timestamptz,
        '2020-02-14T04:20:00Z'::timestamptz,
        '645000',
        0.95,
        jsonb_build_object(
            'source', 'simlab_seed',
            'references', jsonb_build_array(
                'https://rekt.news/bzx-rekt/'
            )
        )
    ),
    (
        'case_usdc_depeg_2023_03_11',
        'glider',
        'market_stress',
        'depeg',
        'high',
        'closed',
        'ethereum',
        'stablecoin-market',
        'USDC Depeg Market Stress (2023-03-11)',
        'Historical depeg window with rapid cross-venue price dislocation and liquidity stress.',
        '2023-03-11T08:00:00Z'::timestamptz,
        '2023-03-11T10:30:00Z'::timestamptz,
        'n/a',
        0.98,
        jsonb_build_object(
            'source', 'simlab_seed',
            'references', jsonb_build_array(
                'https://www.circle.com/blog/an-update-on-usdc-and-silicon-valley-bank'
            )
        )
    )
ON CONFLICT (case_id) DO UPDATE
SET
    tenant_id = EXCLUDED.tenant_id,
    case_type = EXCLUDED.case_type,
    classification = EXCLUDED.classification,
    severity_peak = EXCLUDED.severity_peak,
    status = EXCLUDED.status,
    chain_slug = EXCLUDED.chain_slug,
    protocol = EXCLUDED.protocol,
    title = EXCLUDED.title,
    summary = EXCLUDED.summary,
    incident_start_at = EXCLUDED.incident_start_at,
    incident_end_at = EXCLUDED.incident_end_at,
    loss_usd_estimate = EXCLUDED.loss_usd_estimate,
    source_confidence = EXCLUDED.source_confidence,
    source_payload = EXCLUDED.source_payload,
    updated_at = NOW();

-- ---------------------------------------------------------------------------
-- 2) Case events (source_table + source_pk are globally unique)
-- ---------------------------------------------------------------------------

INSERT INTO history.case_events (
    case_id,
    tenant_id,
    event_type,
    event_ts,
    source_table,
    source_pk,
    payload_json,
    is_simulated,
    simulation_run_id
)
VALUES
    (
        'case_bzx_2020_02_14',
        'glider',
        'flash_loan',
        '2020-02-14T03:56:10Z'::timestamptz,
        'raw.chain_events',
        'seed-bzx-2020-evt-001',
        jsonb_build_object(
            'tx_hash', '0xbzxseed0001',
            'block_number', 9484688,
            'asset', 'WETH',
            'amount', '10000',
            'note', 'flash loan opened'
        ),
        FALSE,
        NULL
    ),
    (
        'case_bzx_2020_02_14',
        'glider',
        'oracle_update',
        '2020-02-14T03:57:20Z'::timestamptz,
        'raw.oracle_ticks',
        'seed-bzx-2020-evt-002',
        jsonb_build_object(
            'tx_hash', '0xbzxseed0002',
            'market', 'sUSD/ETH',
            'price', 0.0071,
            'baseline_price', 0.0104,
            'deviation_pct', -31.7
        ),
        FALSE,
        NULL
    ),
    (
        'case_bzx_2020_02_14',
        'glider',
        'liquidation',
        '2020-02-14T03:59:04Z'::timestamptz,
        'raw.chain_events',
        'seed-bzx-2020-evt-003',
        jsonb_build_object(
            'tx_hash', '0xbzxseed0003',
            'borrower', '0xseedborrower',
            'profit_usd', 351000,
            'note', 'position liquidated after manipulated price move'
        ),
        FALSE,
        NULL
    ),
    (
        'case_usdc_depeg_2023_03_11',
        'glider',
        'oracle_update',
        '2023-03-11T08:03:10Z'::timestamptz,
        'raw.oracle_ticks',
        'seed-usdc-depeg-evt-001',
        jsonb_build_object(
            'market', 'USDC/USD',
            'price', 0.9820,
            'venue', 'chainlink',
            'note', 'first large deviation below peg'
        ),
        FALSE,
        NULL
    ),
    (
        'case_usdc_depeg_2023_03_11',
        'glider',
        'dex_swap',
        '2023-03-11T08:04:25Z'::timestamptz,
        'raw.dex_swaps',
        'seed-usdc-depeg-evt-002',
        jsonb_build_object(
            'venue', 'uniswap-v3',
            'pair', 'USDC/USDT',
            'execution_price', 0.9635,
            'slippage_bps', 280
        ),
        FALSE,
        NULL
    ),
    (
        'case_usdc_depeg_2023_03_11',
        'glider',
        'oracle_update',
        '2023-03-11T08:06:40Z'::timestamptz,
        'raw.oracle_ticks',
        'seed-usdc-depeg-evt-003',
        jsonb_build_object(
            'market', 'USDC/USD',
            'price', 0.9110,
            'venue', 'cex-aggregate',
            'note', 'depeg accelerated'
        ),
        FALSE,
        NULL
    ),
    (
        'case_usdc_depeg_2023_03_11',
        'glider',
        'recovery',
        '2023-03-11T08:10:20Z'::timestamptz,
        'raw.market_ticks',
        'seed-usdc-depeg-evt-004',
        jsonb_build_object(
            'market', 'USDC/USD',
            'price', 0.9380,
            'note', 'partial recovery after intervention headlines'
        ),
        FALSE,
        NULL
    )
ON CONFLICT (source_table, source_pk) DO UPDATE
SET
    case_id = EXCLUDED.case_id,
    tenant_id = EXCLUDED.tenant_id,
    event_type = EXCLUDED.event_type,
    event_ts = EXCLUDED.event_ts,
    payload_json = EXCLUDED.payload_json,
    is_simulated = EXCLUDED.is_simulated,
    simulation_run_id = EXCLUDED.simulation_run_id,
    ingested_at = NOW();

-- ---------------------------------------------------------------------------
-- 3) Replay catalog rows (drives Simlab "History Templates")
-- ---------------------------------------------------------------------------

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
        'hist_bzx_2020_flash_loan_v1',
        'glider',
        'case_bzx_2020_02_14',
        'bzx-2020-flash-loan',
        'bZx 2020 Flash Loan',
        'exploit',
        ARRAY['flash-loan', 'oracle-manipulation', 'historical'],
        'flash_loan',
        'ethereum',
        'bzx',
        'lending',
        'Replay template for bZx flash-loan exploitation sequence.',
        'Rapid collateral value distortion and liquidation path abuse.',
        '645000',
        'flash-loan-oracle-manipulation',
        'flash_loan',
        '2020-02-14T03:55:00Z'::timestamptz,
        '2020-02-14T04:00:00Z'::timestamptz,
        12,
        ARRAY['flash_loan', 'dpeg'],
        ARRAY['tx_hash', 'price', 'amount', 'slippage_bps'],
        2,
        '[{"pattern_id":"flash_loan","severity":"high"},{"pattern_id":"dpeg","severity":"medium"}]'::jsonb,
        '[{"ts":"2020-02-14T03:56:10Z","label":"flash loan opened"},{"ts":"2020-02-14T03:57:20Z","label":"oracle moved"},{"ts":"2020-02-14T03:59:04Z","label":"liquidation"}]'::jsonb,
        '[{"label":"Rekt","url":"https://rekt.news/bzx-rekt/"}]'::jsonb,
        '[{"source_id":"chainlink-eth-mainnet"},{"source_id":"uniswap-v2-eth-mainnet"}]'::jsonb,
        ARRAY['Use 300s window for baseline replay.', 'Override oracle prices to test detector sensitivity.'],
        'v1',
        'history/replay/bzx_2020_flash_loan/v1',
        '6f3c9a8deec7e5b8c5a78d8f8b2c001f',
        NULL,
        TRUE
    ),
    (
        'hist_usdc_depeg_2023_v1',
        'glider',
        'case_usdc_depeg_2023_03_11',
        'usdc-depeg-2023',
        'USDC Depeg 2023',
        'market_stress',
        ARRAY['depeg', 'stablecoin', 'historical'],
        'depeg',
        'ethereum',
        'stablecoin-market',
        'stablecoin',
        'Replay template for USDC depeg stress and cross-venue dislocation.',
        'Peg break with rapid spread widening and partial recovery.',
        'n/a',
        'cross-venue-liquidity-shock',
        'dpeg',
        '2023-03-11T08:02:00Z'::timestamptz,
        '2023-03-11T08:11:00Z'::timestamptz,
        8,
        ARRAY['dpeg', 'tvl_drop'],
        ARRAY['price', 'slippage_bps', 'market'],
        1,
        '[{"pattern_id":"dpeg","severity":"critical"}]'::jsonb,
        '[{"ts":"2023-03-11T08:03:10Z","label":"peg deviation"},{"ts":"2023-03-11T08:06:40Z","label":"depeg accelerated"},{"ts":"2023-03-11T08:10:20Z","label":"partial recovery"}]'::jsonb,
        '[{"label":"Circle update","url":"https://www.circle.com/blog/an-update-on-usdc-and-silicon-valley-bank"}]'::jsonb,
        '[{"source_id":"binance-global"},{"source_id":"coinbase-advanced"},{"source_id":"uniswap-v3-eth-mainnet"}]'::jsonb,
        ARRAY['Use short windows (60-180s) to stress test threshold behavior.', 'Try payload overrides on oracle_update events.'],
        'v1',
        'history/replay/usdc_depeg_2023/v1',
        '118aa0fe4cf8f5d36f99f618e08f3319',
        NULL,
        TRUE
    )
ON CONFLICT (scenario_id) DO UPDATE
SET
    tenant_id = EXCLUDED.tenant_id,
    case_id = EXCLUDED.case_id,
    slug = EXCLUDED.slug,
    title = EXCLUDED.title,
    category = EXCLUDED.category,
    tags = EXCLUDED.tags,
    incident_class = EXCLUDED.incident_class,
    chain = EXCLUDED.chain,
    protocol = EXCLUDED.protocol,
    protocol_category = EXCLUDED.protocol_category,
    description = EXCLUDED.description,
    impact_summary = EXCLUDED.impact_summary,
    losses_usd_estimate = EXCLUDED.losses_usd_estimate,
    attack_vector = EXCLUDED.attack_vector,
    detection_focus = EXCLUDED.detection_focus,
    default_time_window_start = EXCLUDED.default_time_window_start,
    default_time_window_end = EXCLUDED.default_time_window_end,
    default_speed = EXCLUDED.default_speed,
    supported_patterns = EXCLUDED.supported_patterns,
    supported_override_keys = EXCLUDED.supported_override_keys,
    baseline_expected_alerts = EXCLUDED.baseline_expected_alerts,
    expected_alerts_json = EXCLUDED.expected_alerts_json,
    timeline_json = EXCLUDED.timeline_json,
    references_json = EXCLUDED.references_json,
    source_feeds_json = EXCLUDED.source_feeds_json,
    runbook_notes = EXCLUDED.runbook_notes,
    dataset_version = EXCLUDED.dataset_version,
    object_prefix = EXCLUDED.object_prefix,
    checksum = EXCLUDED.checksum,
    simlab_scenario_id = EXCLUDED.simlab_scenario_id,
    is_active = EXCLUDED.is_active,
    updated_at = NOW();
