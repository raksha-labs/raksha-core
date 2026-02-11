CREATE TABLE IF NOT EXISTS detections (
    id TEXT PRIMARY KEY,
    tx_hash TEXT NOT NULL,
    chain TEXT NOT NULL,
    protocol TEXT NOT NULL,
    severity TEXT NOT NULL,
    risk_score DOUBLE PRECISION NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS alerts (
    id TEXT PRIMARY KEY,
    tx_hash TEXT NOT NULL,
    chain TEXT NOT NULL,
    protocol TEXT NOT NULL,
    severity TEXT NOT NULL,
    risk_score DOUBLE PRECISION NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Finality tracker state persistence for fault tolerance
CREATE TABLE IF NOT EXISTS finality_state (
    chain TEXT PRIMARY KEY,
    head_block BIGINT NOT NULL,
    confirmation_depth INT NOT NULL,
    blocks JSONB NOT NULL,  -- Serialized BTreeMap<u64, BlockEntry>
    states JSONB NOT NULL,  -- Serialized HashMap<String, LifecycleState>
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for performance
CREATE INDEX IF NOT EXISTS idx_detections_chain ON detections(chain);
CREATE INDEX IF NOT EXISTS idx_detections_protocol ON detections(protocol);
CREATE INDEX IF NOT EXISTS idx_detections_tx_hash ON detections(tx_hash);
CREATE INDEX IF NOT EXISTS idx_detections_created_at ON detections(created_at);

CREATE INDEX IF NOT EXISTS idx_alerts_chain ON alerts(chain);
CREATE INDEX IF NOT EXISTS idx_alerts_protocol ON alerts(protocol);
CREATE INDEX IF NOT EXISTS idx_alerts_tx_hash ON alerts(tx_hash);
CREATE INDEX IF NOT EXISTS idx_alerts_created_at ON alerts(created_at);

-- NOTE: For high-volume production workloads, consider:
-- 1. Time-based partitioning for detections and alerts tables
--    ALTER TABLE detections PARTITION BY RANGE (created_at);
-- 2. Separate partitions for each chain
--    ALTER TABLE detections PARTITION BY LIST (chain);
-- 3. Retention policies to archive old data
--    pg_cron or application-level archival jobs
