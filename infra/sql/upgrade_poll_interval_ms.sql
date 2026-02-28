-- One-time upgrade for existing databases:
-- Adds source_stream_configs.poll_interval_ms and backfills polling connector defaults.

ALTER TABLE source_stream_configs
  ADD COLUMN IF NOT EXISTS poll_interval_ms INTEGER;

UPDATE source_stream_configs
SET poll_interval_ms = CASE connector_mode
  WHEN 'rpc_logs' THEN 2000
  WHEN 'http_poll' THEN 5000
  ELSE poll_interval_ms
END
WHERE poll_interval_ms IS NULL
  AND connector_mode IN ('rpc_logs', 'http_poll');

UPDATE source_stream_configs
SET poll_interval_ms = NULL
WHERE connector_mode = 'websocket'
  AND poll_interval_ms IS NOT NULL;

ALTER TABLE source_stream_configs
  DROP CONSTRAINT IF EXISTS source_stream_configs_poll_interval_ms_check;

ALTER TABLE source_stream_configs
  ADD CONSTRAINT source_stream_configs_poll_interval_ms_check
    CHECK (poll_interval_ms IS NULL OR poll_interval_ms BETWEEN 200 AND 60000);
