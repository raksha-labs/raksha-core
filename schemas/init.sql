-- DeFi Surveillance Database Schema
-- Version: 0.1.0
-- Created: 2026-02-06

-- Enable UUID extension
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

-- Detections table: Stores all rule evaluation results
CREATE TABLE IF NOT EXISTS detections (
    detection_id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    chain VARCHAR(50) NOT NULL,
    protocol VARCHAR(100) NOT NULL,
    severity VARCHAR(20) NOT NULL,
    triggered_rule_ids TEXT[] NOT NULL,
    tx_hash VARCHAR(66) NOT NULL,
    block_number BIGINT NOT NULL,
    risk_score NUMERIC(5,2) NOT NULL,
    signals JSONB NOT NULL,
    oracle_context JSONB NOT NULL,
    actions_recommended TEXT[],
    created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
);

-- Indexing for common queries
CREATE INDEX IF NOT EXISTS idx_detections_chain ON detections (chain);
CREATE INDEX IF NOT EXISTS idx_detections_protocol ON detections (protocol);
CREATE INDEX IF NOT EXISTS idx_detections_severity ON detections (severity);
CREATE INDEX IF NOT EXISTS idx_detections_block ON detections (block_number);
CREATE INDEX IF NOT EXISTS idx_detections_created ON detections (created_at);
CREATE INDEX IF NOT EXISTS idx_detections_tx ON detections (tx_hash);

-- Alerts table: Stores alerts sent to external systems
CREATE TABLE IF NOT EXISTS alerts (
    alert_id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    detection_id UUID REFERENCES detections(detection_id),
    chain VARCHAR(50) NOT NULL,
    protocol VARCHAR(100) NOT NULL,
    severity VARCHAR(20) NOT NULL,
    risk_score NUMERIC(5,2) NOT NULL,
    rule_ids TEXT[] NOT NULL,
    tx_hash VARCHAR(66) NOT NULL,
    block_number BIGINT NOT NULL,
    oracle_context JSONB NOT NULL,
    actions_recommended TEXT[],
    
    -- Alert delivery tracking
    delivered_to TEXT[] DEFAULT '{}',
    delivery_status VARCHAR(20) DEFAULT 'pending',
    delivery_attempts INT DEFAULT 0,
    last_delivery_attempt TIMESTAMP WITH TIME ZONE,
    
    -- Deduplication
    dedup_key VARCHAR(255),
    
    created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_alerts_chain ON alerts (chain);
CREATE INDEX IF NOT EXISTS idx_alerts_severity ON alerts (severity);
CREATE INDEX IF NOT EXISTS idx_alerts_status ON alerts (delivery_status);
CREATE INDEX IF NOT EXISTS idx_alerts_created ON alerts (created_at);
CREATE INDEX IF NOT EXISTS idx_alerts_dedup ON alerts (dedup_key);

-- Incidents table: Groups related alerts into incidents
CREATE TABLE IF NOT EXISTS incidents (
    incident_id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    title VARCHAR(255) NOT NULL,
    description TEXT,
    chain VARCHAR(50) NOT NULL,
    protocol VARCHAR(100) NOT NULL,
    severity VARCHAR(20) NOT NULL,
    status VARCHAR(20) DEFAULT 'open',
    
    -- Related alerts
    alert_ids UUID[] DEFAULT '{}',
    
    -- Impact assessment
    estimated_impact_usd NUMERIC(20,2),
    affected_users INT,
    
    -- Timeline
    first_detected_at TIMESTAMP WITH TIME ZONE NOT NULL,
    resolved_at TIMESTAMP WITH TIME ZONE,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_incidents_status ON incidents (status);
CREATE INDEX IF NOT EXISTS idx_incidents_chain ON incidents (chain);
CREATE INDEX IF NOT EXISTS idx_incidents_severity ON incidents (severity);
CREATE INDEX IF NOT EXISTS idx_incidents_created ON incidents (created_at);

-- Indexer state: Track processing progress per chain
CREATE TABLE IF NOT EXISTS indexer_state (
    chain VARCHAR(50) PRIMARY KEY,
    last_indexed_block BIGINT NOT NULL,
    last_block_hash VARCHAR(66) NOT NULL,
    last_block_timestamp TIMESTAMP WITH TIME ZONE,
    processed_events_count BIGINT DEFAULT 0,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
);

-- Processed events: Deduplication and reorg tracking
CREATE TABLE IF NOT EXISTS processed_events (
    event_id UUID PRIMARY KEY,
    tx_hash VARCHAR(66) NOT NULL,
    block_number BIGINT NOT NULL,
    block_hash VARCHAR(66) NOT NULL,
    chain VARCHAR(50) NOT NULL,
    processed_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
    reverted BOOLEAN DEFAULT FALSE,
    UNIQUE (tx_hash, block_number, chain)
);

CREATE INDEX IF NOT EXISTS idx_processed_tx ON processed_events (tx_hash);
CREATE INDEX IF NOT EXISTS idx_processed_block ON processed_events (block_number);

-- Normalized event store: Canonical ingestion payload persistence
CREATE TABLE IF NOT EXISTS normalized_events (
    event_key TEXT PRIMARY KEY,
    event_id UUID NOT NULL,
    chain TEXT NOT NULL,
    chain_slug TEXT NOT NULL,
    chain_id BIGINT NULL,
    protocol TEXT NOT NULL,
    protocol_category TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK (event_type IN ('oracle_update', 'flash_loan_candidate')),
    tx_hash TEXT NOT NULL,
    block_number BIGINT NOT NULL,
    block_hash TEXT NULL,
    tx_index BIGINT NULL,
    log_index BIGINT NULL,
    status TEXT NOT NULL,
    lifecycle_state TEXT NOT NULL,
    requires_confirmation BOOLEAN NOT NULL,
    confirmation_depth BIGINT NOT NULL,
    observed_at TIMESTAMP WITH TIME ZONE NOT NULL,
    reverted BOOLEAN NOT NULL DEFAULT FALSE,
    payload JSONB NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_normalized_events_chain_block
    ON normalized_events (chain_slug, block_number);
CREATE INDEX IF NOT EXISTS idx_normalized_events_tx_hash
    ON normalized_events (tx_hash);
CREATE INDEX IF NOT EXISTS idx_normalized_events_observed_at
    ON normalized_events (observed_at);
CREATE INDEX IF NOT EXISTS idx_normalized_events_reverted
    ON normalized_events (reverted);

-- Alert delivery log: Track all delivery attempts
CREATE TABLE IF NOT EXISTS alert_delivery_log (
    id BIGSERIAL PRIMARY KEY,
    alert_id UUID REFERENCES alerts(alert_id),
    sink_name VARCHAR(50) NOT NULL,
    status VARCHAR(20) NOT NULL,
    response_code INT,
    response_body TEXT,
    error_message TEXT,
    delivered_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_delivery_alert ON alert_delivery_log (alert_id);
CREATE INDEX IF NOT EXISTS idx_delivery_status ON alert_delivery_log (status);

-- Rule execution metrics: Performance tracking
CREATE TABLE IF NOT EXISTS rule_execution_metrics (
    id BIGSERIAL PRIMARY KEY,
    rule_id VARCHAR(100) NOT NULL,
    chain VARCHAR(50) NOT NULL,
    protocol VARCHAR(100) NOT NULL,
    execution_time_ms INT NOT NULL,
    triggered BOOLEAN NOT NULL,
    event_count INT DEFAULT 1,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_metrics_rule ON rule_execution_metrics (rule_id);
CREATE INDEX IF NOT EXISTS idx_metrics_created ON rule_execution_metrics (created_at);

-- Insert initial indexer state
INSERT INTO indexer_state (chain, last_indexed_block, last_block_hash)
VALUES 
    ('ethereum', 0, '0x0000000000000000000000000000000000000000000000000000000000000000'),
    ('base', 0, '0x0000000000000000000000000000000000000000000000000000000000000000')
ON CONFLICT (chain) DO NOTHING;

-- Create function to update updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Add triggers for updated_at
DROP TRIGGER IF EXISTS update_alerts_updated_at ON alerts;
CREATE TRIGGER update_alerts_updated_at BEFORE UPDATE ON alerts
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

DROP TRIGGER IF EXISTS update_incidents_updated_at ON incidents;
CREATE TRIGGER update_incidents_updated_at BEFORE UPDATE ON incidents
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

DROP TRIGGER IF EXISTS update_indexer_state_updated_at ON indexer_state;
CREATE TRIGGER update_indexer_state_updated_at BEFORE UPDATE ON indexer_state
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Create views for common queries

-- Active alerts view
CREATE OR REPLACE VIEW active_alerts AS
SELECT 
    a.*,
    d.signals,
    i.incident_id,
    i.status as incident_status
FROM alerts a
LEFT JOIN detections d ON a.detection_id = d.detection_id
LEFT JOIN incidents i ON a.alert_id = ANY(i.alert_ids)
WHERE a.delivery_status IN ('pending', 'delivered')
    AND a.created_at > NOW() - INTERVAL '24 hours'
ORDER BY a.created_at DESC;

-- Protocol health summary
CREATE OR REPLACE VIEW protocol_health_summary AS
SELECT 
    chain,
    protocol,
    COUNT(*) as alert_count_24h,
    MAX(risk_score) as max_risk_score,
    AVG(risk_score) as avg_risk_score,
    COUNT(DISTINCT tx_hash) as affected_txs,
    MAX(created_at) as last_alert_at
FROM alerts
WHERE created_at > NOW() - INTERVAL '24 hours'
GROUP BY chain, protocol
ORDER BY alert_count_24h DESC;

-- System performance metrics
CREATE OR REPLACE VIEW system_performance AS
SELECT 
    rule_id,
    chain,
    COUNT(*) as execution_count,
    AVG(execution_time_ms) as avg_execution_ms,
    MAX(execution_time_ms) as max_execution_ms,
    SUM(CASE WHEN triggered THEN 1 ELSE 0 END) as trigger_count,
    ROUND(100.0 * SUM(CASE WHEN triggered THEN 1 ELSE 0 END) / COUNT(*), 2) as trigger_rate_pct
FROM rule_execution_metrics
WHERE created_at > NOW() - INTERVAL '1 hour'
GROUP BY rule_id, chain
ORDER BY execution_count DESC;

-- Grant permissions (adjust as needed)
-- GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO raksha_user;
-- GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public TO raksha_user;
