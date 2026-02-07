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
