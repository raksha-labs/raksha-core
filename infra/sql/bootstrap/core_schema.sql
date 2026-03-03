-- ============================================================================
-- Raksha - Clean Bootstrap Schema (Create-Only)
-- ============================================================================
-- Fresh-install schema for local/dev bootstrap.
-- No migration-style ALTER/UPDATE blocks are included here.
--
-- Pipeline order:
--   1) Source catalog + stream configs
--   2) Operational ingestion event bus
--   3) Pattern catalog/config + runtime state
--   4) Detections -> Alerts -> Alert lifecycle
--   5) Finality + analytics/support tables
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ─────────────────────────────────────────────────────────────────────────────
-- 1) Source Catalog + Stream Configuration
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS data_sources (
    source_id         TEXT PRIMARY KEY,
    source_type       TEXT NOT NULL,        -- evm_chain | cex_websocket | dex_api | oracle_api | custom_api
    source_name       TEXT NOT NULL,
    connection_config JSONB NOT NULL DEFAULT '{}'::jsonb,
    filters           JSONB,
    scope             TEXT NOT NULL DEFAULT 'global'
        CHECK (scope IN ('global', 'tenant')),
    owner_tenant_id   TEXT,
    promoted_at       TIMESTAMPTZ,
    promoted_from_tenant_id TEXT,
    active_sharing_offer_id UUID,
    enabled           BOOLEAN NOT NULL DEFAULT TRUE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (
        (scope = 'global' AND owner_tenant_id IS NULL)
        OR (scope = 'tenant' AND owner_tenant_id IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_data_sources_scope_owner
    ON data_sources (scope, owner_tenant_id);

CREATE TABLE IF NOT EXISTS tenant_data_sources (
    tenant_id       TEXT NOT NULL,
    source_id       TEXT NOT NULL REFERENCES data_sources(source_id) ON DELETE CASCADE,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    override_config JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, source_id)
);

CREATE INDEX IF NOT EXISTS idx_tenant_data_sources_tenant
    ON tenant_data_sources (tenant_id);

CREATE TABLE IF NOT EXISTS source_stream_configs (
    stream_config_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id        TEXT NOT NULL REFERENCES data_sources(source_id) ON DELETE CASCADE,
    connector_mode   TEXT NOT NULL CHECK (connector_mode IN ('websocket', 'rpc_logs', 'http_poll')),
    stream_name      TEXT NOT NULL,
    subscription_key TEXT,
    event_type       TEXT NOT NULL,
    parser_name      TEXT NOT NULL,
    market_key       TEXT,
    asset_pair       TEXT,
    filter_config    JSONB NOT NULL DEFAULT '{}'::jsonb,
    auth_secret_ref  TEXT,
    auth_config      JSONB NOT NULL DEFAULT '{}'::jsonb,
    payload_ts_path  TEXT,
    payload_ts_unit  TEXT NOT NULL DEFAULT 'ms' CHECK (payload_ts_unit IN ('ms', 's', 'iso8601')),
    poll_interval_ms INTEGER,
    enabled          BOOLEAN NOT NULL DEFAULT TRUE,
    created_by       TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by       TEXT,
    updated_at       TIMESTAMPTZ,
    CONSTRAINT source_stream_configs_poll_interval_ms_check
        CHECK (poll_interval_ms IS NULL OR poll_interval_ms BETWEEN 200 AND 60000)
);

CREATE INDEX IF NOT EXISTS idx_source_stream_configs_source_enabled
    ON source_stream_configs (source_id, enabled);
CREATE INDEX IF NOT EXISTS idx_source_stream_configs_event_type
    ON source_stream_configs (event_type);
CREATE UNIQUE INDEX IF NOT EXISTS uq_source_stream_configs_natural
    ON source_stream_configs (source_id, stream_name, COALESCE(asset_pair, ''), COALESCE(subscription_key, ''));

CREATE TABLE IF NOT EXISTS source_stream_tenant_targets (
    stream_config_id UUID NOT NULL REFERENCES source_stream_configs(stream_config_id) ON DELETE CASCADE,
    tenant_id        TEXT NOT NULL,
    enabled          BOOLEAN NOT NULL DEFAULT TRUE,
    created_by       TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by       TEXT,
    updated_at       TIMESTAMPTZ,
    PRIMARY KEY (stream_config_id, tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_source_stream_tenant_targets_tenant_enabled
    ON source_stream_tenant_targets (tenant_id, enabled);

CREATE TABLE IF NOT EXISTS source_requests (
    request_id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    requesting_tenant_id  TEXT NOT NULL,
    requested_source_type TEXT NOT NULL,
    requested_stream_spec JSONB NOT NULL DEFAULT '{}'::jsonb,
    justification         TEXT,
    status                TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved_private', 'approved_global', 'rejected', 'cancelled')),
    resolved_source_id    TEXT REFERENCES data_sources(source_id) ON DELETE SET NULL,
    review_notes          TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at           TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_source_requests_tenant_status
    ON source_requests (requesting_tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_source_requests_status_created
    ON source_requests (status, created_at DESC);

CREATE TABLE IF NOT EXISTS source_sharing_offers (
    offer_id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id                      TEXT NOT NULL REFERENCES data_sources(source_id) ON DELETE CASCADE,
    offering_tenant_id             TEXT NOT NULL,
    status                         TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'active', 'paused', 'closed', 'promoted', 'cancelled')),
    title                          TEXT NOT NULL,
    description                    TEXT,
    auto_promote_at_subscribers    INTEGER CHECK (auto_promote_at_subscribers IS NULL OR auto_promote_at_subscribers > 0),
    published_at                   TIMESTAMPTZ,
    closed_at                      TIMESTAMPTZ,
    created_at                     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_source_sharing_offers_source
    ON source_sharing_offers (source_id);
CREATE INDEX IF NOT EXISTS idx_source_sharing_offers_status_published
    ON source_sharing_offers (status, published_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS uq_source_sharing_offers_active_source
    ON source_sharing_offers (source_id)
    WHERE status = 'active';

CREATE TABLE IF NOT EXISTS source_sharing_consents (
    consent_id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    offer_id                UUID NOT NULL REFERENCES source_sharing_offers(offer_id) ON DELETE CASCADE,
    subscribing_tenant_id   TEXT NOT NULL,
    status                  TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'revoked', 'blocked')),
    accepted_at             TIMESTAMPTZ,
    revoked_at              TIMESTAMPTZ,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (offer_id, subscribing_tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_source_sharing_consents_tenant_status
    ON source_sharing_consents (subscribing_tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_source_sharing_consents_offer_status
    ON source_sharing_consents (offer_id, status);

CREATE TABLE IF NOT EXISTS source_sharing_offer_audit_log (
    audit_id          BIGSERIAL PRIMARY KEY,
    entity_type       TEXT NOT NULL CHECK (entity_type IN ('offer', 'consent', 'request', 'promotion')),
    entity_id         TEXT NOT NULL,
    from_status       TEXT,
    to_status         TEXT,
    actor_tenant_id   TEXT,
    actor_user_id     TEXT,
    reason            TEXT,
    metadata          JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_source_sharing_offer_audit_entity
    ON source_sharing_offer_audit_log (entity_type, entity_id, created_at DESC);

CREATE TABLE IF NOT EXISTS source_promotion_outbox (
    event_id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    offer_id              UUID NOT NULL REFERENCES source_sharing_offers(offer_id) ON DELETE CASCADE,
    source_id             TEXT NOT NULL REFERENCES data_sources(source_id) ON DELETE CASCADE,
    old_owner_tenant_id   TEXT,
    event_type            TEXT NOT NULL DEFAULT 'source_promoted'
        CHECK (event_type IN ('source_promoted')),
    status                TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'processing', 'done', 'failed')),
    attempts              INTEGER NOT NULL DEFAULT 0,
    last_error            TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (offer_id, event_type)
);

CREATE INDEX IF NOT EXISTS idx_source_promotion_outbox_status_created
    ON source_promotion_outbox (status, created_at);

-- Idempotent column migration: add active_sharing_offer_id to data_sources if
-- the table was created before this column was introduced.
ALTER TABLE data_sources
    ADD COLUMN IF NOT EXISTS active_sharing_offer_id UUID;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'fk_data_sources_active_sharing_offer'
    ) THEN
        ALTER TABLE data_sources
            ADD CONSTRAINT fk_data_sources_active_sharing_offer
            FOREIGN KEY (active_sharing_offer_id)
            REFERENCES source_sharing_offers(offer_id)
            ON DELETE SET NULL;
    END IF;
END $$;

CREATE OR REPLACE FUNCTION fn_source_sharing_set_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at := NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_validate_source_sharing_offer_owner()
RETURNS TRIGGER AS $$
DECLARE
    source_owner TEXT;
    source_scope TEXT;
BEGIN
    IF NEW.status IN ('draft', 'active', 'paused') THEN
        SELECT owner_tenant_id, scope
        INTO source_owner, source_scope
        FROM data_sources
        WHERE source_id = NEW.source_id;

        IF source_scope IS NULL THEN
            RAISE EXCEPTION 'source_not_found';
        END IF;
        IF source_scope <> 'tenant' THEN
            RAISE EXCEPTION 'source_not_tenant_owned';
        END IF;
        IF source_owner IS DISTINCT FROM NEW.offering_tenant_id THEN
            RAISE EXCEPTION 'source_not_owned_by_offering_tenant';
        END IF;
    END IF;

    IF NEW.status = 'active' AND NEW.published_at IS NULL THEN
        NEW.published_at := NOW();
    END IF;
    IF NEW.status IN ('closed', 'promoted', 'cancelled') AND NEW.closed_at IS NULL THEN
        NEW.closed_at := NOW();
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_sync_data_source_active_offer_pointer()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.status = 'active' THEN
        UPDATE data_sources
        SET active_sharing_offer_id = NEW.offer_id
        WHERE source_id = NEW.source_id;
    ELSIF TG_OP = 'UPDATE' AND OLD.status = 'active' AND NEW.status <> 'active' THEN
        UPDATE data_sources
        SET active_sharing_offer_id = NULL
        WHERE source_id = NEW.source_id
          AND active_sharing_offer_id = NEW.offer_id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_log_source_sharing_offer_status_change()
RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'INSERT' OR OLD.status IS DISTINCT FROM NEW.status THEN
        INSERT INTO source_sharing_offer_audit_log (
            entity_type,
            entity_id,
            from_status,
            to_status,
            actor_tenant_id,
            metadata
        )
        VALUES (
            'offer',
            NEW.offer_id::text,
            CASE WHEN TG_OP = 'INSERT' THEN NULL ELSE OLD.status END,
            NEW.status,
            NEW.offering_tenant_id,
            jsonb_build_object('source_id', NEW.source_id)
        );
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_validate_source_sharing_consent()
RETURNS TRIGGER AS $$
DECLARE
    offer_owner TEXT;
    offer_status TEXT;
BEGIN
    SELECT offering_tenant_id, status
    INTO offer_owner, offer_status
    FROM source_sharing_offers
    WHERE offer_id = NEW.offer_id;

    IF offer_owner IS NULL THEN
        RAISE EXCEPTION 'sharing_offer_not_found';
    END IF;
    IF NEW.subscribing_tenant_id = offer_owner THEN
        RAISE EXCEPTION 'owner_cannot_subscribe_own_offer';
    END IF;
    IF NEW.status = 'active' AND offer_status NOT IN ('active', 'promoted') THEN
        RAISE EXCEPTION 'offer_not_subscribable';
    END IF;

    IF NEW.status = 'active' AND NEW.accepted_at IS NULL THEN
        NEW.accepted_at := NOW();
    END IF;
    IF NEW.status = 'revoked' AND NEW.revoked_at IS NULL THEN
        NEW.revoked_at := NOW();
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_log_source_sharing_consent_status_change()
RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'INSERT' OR OLD.status IS DISTINCT FROM NEW.status THEN
        INSERT INTO source_sharing_offer_audit_log (
            entity_type,
            entity_id,
            from_status,
            to_status,
            actor_tenant_id,
            metadata
        )
        VALUES (
            'consent',
            NEW.consent_id::text,
            CASE WHEN TG_OP = 'INSERT' THEN NULL ELSE OLD.status END,
            NEW.status,
            NEW.subscribing_tenant_id,
            jsonb_build_object('offer_id', NEW.offer_id)
        );
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_promote_source_to_global(p_offer_id UUID)
RETURNS BOOLEAN AS $$
DECLARE
    v_offer source_sharing_offers%ROWTYPE;
    v_old_owner TEXT;
BEGIN
    SELECT *
    INTO v_offer
    FROM source_sharing_offers
    WHERE offer_id = p_offer_id
    FOR UPDATE;

    IF v_offer.offer_id IS NULL THEN
        RETURN FALSE;
    END IF;
    IF v_offer.status = 'promoted' THEN
        RETURN TRUE;
    END IF;
    IF v_offer.status NOT IN ('active', 'paused') THEN
        RETURN FALSE;
    END IF;

    SELECT owner_tenant_id
    INTO v_old_owner
    FROM data_sources
    WHERE source_id = v_offer.source_id
    FOR UPDATE;

    UPDATE data_sources
    SET scope = 'global',
        owner_tenant_id = NULL,
        promoted_at = NOW(),
        promoted_from_tenant_id = COALESCE(promoted_from_tenant_id, v_old_owner),
        active_sharing_offer_id = NULL
    WHERE source_id = v_offer.source_id;

    UPDATE source_sharing_offers
    SET status = 'promoted',
        closed_at = NOW(),
        updated_at = NOW()
    WHERE offer_id = p_offer_id;

    INSERT INTO source_promotion_outbox (
        offer_id,
        source_id,
        old_owner_tenant_id,
        event_type,
        status
    )
    VALUES (
        p_offer_id,
        v_offer.source_id,
        v_old_owner,
        'source_promoted',
        'pending'
    )
    ON CONFLICT (offer_id, event_type) DO NOTHING;

    INSERT INTO source_sharing_offer_audit_log (
        entity_type,
        entity_id,
        from_status,
        to_status,
        actor_tenant_id,
        metadata
    )
    VALUES (
        'promotion',
        p_offer_id::text,
        v_offer.status,
        'promoted',
        v_old_owner,
        jsonb_build_object('source_id', v_offer.source_id)
    );

    RETURN TRUE;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_sync_tenant_subscription_from_consent()
RETURNS TRIGGER AS $$
DECLARE
    v_source_id TEXT;
    v_threshold INTEGER;
    v_active_count INTEGER;
BEGIN
    SELECT source_id, auto_promote_at_subscribers
    INTO v_source_id, v_threshold
    FROM source_sharing_offers
    WHERE offer_id = NEW.offer_id;

    IF v_source_id IS NULL THEN
        RETURN NEW;
    END IF;

    IF NEW.status = 'active' THEN
        INSERT INTO tenant_data_sources (tenant_id, source_id, enabled, override_config, created_at)
        VALUES (NEW.subscribing_tenant_id, v_source_id, TRUE, '{}'::jsonb, NOW())
        ON CONFLICT (tenant_id, source_id) DO UPDATE SET
            enabled = TRUE;

        INSERT INTO source_stream_tenant_targets (
            stream_config_id,
            tenant_id,
            enabled,
            created_by,
            created_at
        )
        SELECT
            ssc.stream_config_id,
            NEW.subscribing_tenant_id,
            TRUE,
            'source-sharing',
            NOW()
        FROM source_stream_configs ssc
        WHERE ssc.source_id = v_source_id
        ON CONFLICT (stream_config_id, tenant_id) DO UPDATE SET
            enabled = TRUE,
            updated_by = 'source-sharing',
            updated_at = NOW();

        IF v_threshold IS NOT NULL THEN
            SELECT COUNT(*)::INTEGER
            INTO v_active_count
            FROM source_sharing_consents
            WHERE offer_id = NEW.offer_id
              AND status = 'active';

            IF v_active_count >= v_threshold THEN
                PERFORM fn_promote_source_to_global(NEW.offer_id);
            END IF;
        END IF;
    ELSIF TG_OP = 'UPDATE' AND OLD.status = 'active' AND NEW.status <> 'active' THEN
        UPDATE tenant_data_sources
        SET enabled = FALSE
        WHERE tenant_id = NEW.subscribing_tenant_id
          AND source_id = v_source_id;

        UPDATE source_stream_tenant_targets stt
        SET enabled = FALSE,
            updated_by = 'source-sharing',
            updated_at = NOW()
        FROM source_stream_configs ssc
        WHERE ssc.stream_config_id = stt.stream_config_id
          AND ssc.source_id = v_source_id
          AND stt.tenant_id = NEW.subscribing_tenant_id;
    END IF;

    PERFORM pg_notify('source_stream_config_changed', 'source-sharing');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_source_sharing_block_audit_mutation()
RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'source_sharing_offer_audit_log is append-only';
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_source_requests_set_updated_at ON source_requests;
CREATE TRIGGER trg_source_requests_set_updated_at
    BEFORE UPDATE ON source_requests
    FOR EACH ROW
    EXECUTE FUNCTION fn_source_sharing_set_updated_at();

DROP TRIGGER IF EXISTS trg_source_sharing_offers_set_updated_at ON source_sharing_offers;
CREATE TRIGGER trg_source_sharing_offers_set_updated_at
    BEFORE UPDATE ON source_sharing_offers
    FOR EACH ROW
    EXECUTE FUNCTION fn_source_sharing_set_updated_at();

DROP TRIGGER IF EXISTS trg_source_sharing_offers_validate_owner ON source_sharing_offers;
CREATE TRIGGER trg_source_sharing_offers_validate_owner
    BEFORE INSERT OR UPDATE ON source_sharing_offers
    FOR EACH ROW
    EXECUTE FUNCTION fn_validate_source_sharing_offer_owner();

DROP TRIGGER IF EXISTS trg_source_sharing_offers_sync_pointer ON source_sharing_offers;
CREATE TRIGGER trg_source_sharing_offers_sync_pointer
    AFTER INSERT OR UPDATE ON source_sharing_offers
    FOR EACH ROW
    EXECUTE FUNCTION fn_sync_data_source_active_offer_pointer();

DROP TRIGGER IF EXISTS trg_source_sharing_offers_audit ON source_sharing_offers;
CREATE TRIGGER trg_source_sharing_offers_audit
    AFTER INSERT OR UPDATE ON source_sharing_offers
    FOR EACH ROW
    EXECUTE FUNCTION fn_log_source_sharing_offer_status_change();

DROP TRIGGER IF EXISTS trg_source_sharing_consents_set_updated_at ON source_sharing_consents;
CREATE TRIGGER trg_source_sharing_consents_set_updated_at
    BEFORE UPDATE ON source_sharing_consents
    FOR EACH ROW
    EXECUTE FUNCTION fn_source_sharing_set_updated_at();

DROP TRIGGER IF EXISTS trg_source_sharing_consents_validate ON source_sharing_consents;
CREATE TRIGGER trg_source_sharing_consents_validate
    BEFORE INSERT OR UPDATE ON source_sharing_consents
    FOR EACH ROW
    EXECUTE FUNCTION fn_validate_source_sharing_consent();

DROP TRIGGER IF EXISTS trg_source_sharing_consents_audit ON source_sharing_consents;
CREATE TRIGGER trg_source_sharing_consents_audit
    AFTER INSERT OR UPDATE ON source_sharing_consents
    FOR EACH ROW
    EXECUTE FUNCTION fn_log_source_sharing_consent_status_change();

DROP TRIGGER IF EXISTS trg_source_sharing_consents_sync ON source_sharing_consents;
CREATE TRIGGER trg_source_sharing_consents_sync
    AFTER INSERT OR UPDATE OF status ON source_sharing_consents
    FOR EACH ROW
    EXECUTE FUNCTION fn_sync_tenant_subscription_from_consent();

DROP TRIGGER IF EXISTS trg_source_sharing_audit_block_update ON source_sharing_offer_audit_log;
CREATE TRIGGER trg_source_sharing_audit_block_update
    BEFORE UPDATE OR DELETE ON source_sharing_offer_audit_log
    FOR EACH ROW
    EXECUTE FUNCTION fn_source_sharing_block_audit_mutation();

CREATE TABLE IF NOT EXISTS source_required_pairs (
    source_id             TEXT        NOT NULL,
    market_key            TEXT        NOT NULL,
    source_symbol         TEXT        NOT NULL,
    required_tenant_count INTEGER     NOT NULL DEFAULT 0,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (source_id, market_key, source_symbol)
);

CREATE INDEX IF NOT EXISTS idx_source_required_pairs_source_market
    ON source_required_pairs (source_id, market_key);

CREATE TABLE IF NOT EXISTS data_source_health (
    tenant_id       TEXT        NOT NULL,
    source_id       TEXT        NOT NULL,
    healthy         BOOLEAN     NOT NULL,
    last_message_at TIMESTAMPTZ,
    last_error      TEXT,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, source_id)
);

-- Ingestion operational bus (lean envelope + raw pointers)
CREATE TABLE IF NOT EXISTS ingest_operational_events (
    ingest_event_id     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id           UUID,
    source_id           TEXT        NOT NULL,
    source_type         TEXT        NOT NULL,
    tenant_id           TEXT,
    event_type          TEXT        NOT NULL,
    event_id            TEXT,
    market_key          TEXT,
    asset_pair          TEXT,
    chain_id            BIGINT,
    block_number        BIGINT,
    tx_hash             TEXT,
    log_index           BIGINT,
    topic0              TEXT,
    price               DOUBLE PRECISION,
    payload_event_ts    TIMESTAMPTZ,
    observed_at         TIMESTAMPTZ NOT NULL,
    parse_status        TEXT        NOT NULL CHECK (parse_status IN ('parsed', 'partial', 'raw_only', 'error')),
    parse_error         TEXT,
    payload             JSONB       NOT NULL,
    normalized_fields   JSONB       NOT NULL DEFAULT '{}'::jsonb,
    dedup_key           TEXT,
    raw_ref_type        TEXT,
    raw_ref_id          TEXT,
    raw_s3_uri          TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_ingest_operational_source_observed
    ON ingest_operational_events (source_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_ingest_operational_market_observed
    ON ingest_operational_events (market_key, observed_at DESC)
    WHERE market_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ingest_operational_event_type_observed
    ON ingest_operational_events (event_type, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_ingest_operational_stream_observed
    ON ingest_operational_events (stream_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_ingest_operational_tx
    ON ingest_operational_events (tx_hash, log_index)
    WHERE tx_hash IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_ingest_operational_event_id
    ON ingest_operational_events (event_id)
    WHERE event_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_ingest_operational_dedup
    ON ingest_operational_events (dedup_key)
    WHERE dedup_key IS NOT NULL;

CREATE OR REPLACE VIEW source_registry AS
SELECT
    ds.source_id,
    ds.source_type,
    ds.source_name,
    ds.connection_config,
    ds.filters,
    ds.scope,
    ds.owner_tenant_id,
    ds.enabled,
    ds.created_at
FROM data_sources ds;

CREATE OR REPLACE VIEW source_stream_registry AS
SELECT
    ssc.stream_config_id AS stream_id,
    ssc.source_id,
    ssc.connector_mode,
    ssc.stream_name,
    ssc.subscription_key,
    ssc.event_type,
    ssc.parser_name,
    ssc.market_key,
    ssc.asset_pair,
    ssc.filter_config,
    ssc.auth_secret_ref,
    ssc.auth_config,
    ssc.payload_ts_path,
    ssc.payload_ts_unit,
    ssc.poll_interval_ms,
    ssc.enabled,
    ssc.created_by,
    ssc.created_at,
    ssc.updated_by,
    ssc.updated_at
FROM source_stream_configs ssc;

CREATE OR REPLACE VIEW tenant_stream_targets AS
SELECT
    stt.stream_config_id AS stream_id,
    stt.tenant_id,
    stt.enabled,
    stt.created_by,
    stt.created_at,
    stt.updated_by,
    stt.updated_at
FROM source_stream_tenant_targets stt;

CREATE OR REPLACE VIEW ingest_activity_1m AS
SELECT
    ioe.source_id,
    ioe.market_key,
    ioe.event_type,
    date_trunc('minute', ioe.observed_at) AS bucket_1m,
    COUNT(*)::bigint AS events_count,
    MAX(ioe.observed_at) AS last_observed_at
FROM ingest_operational_events ioe
GROUP BY ioe.source_id, ioe.market_key, ioe.event_type, date_trunc('minute', ioe.observed_at);

CREATE OR REPLACE VIEW ingest_latest_samples AS
SELECT DISTINCT ON (ioe.source_id, COALESCE(ioe.market_key, ''), ioe.event_type)
    ioe.source_id,
    ioe.market_key,
    ioe.event_type,
    ioe.event_id,
    ioe.price,
    ioe.observed_at,
    ioe.payload,
    ioe.raw_ref_type,
    ioe.raw_ref_id,
    ioe.raw_s3_uri
FROM ingest_operational_events ioe
ORDER BY ioe.source_id, COALESCE(ioe.market_key, ''), ioe.event_type, ioe.observed_at DESC;

-- ─────────────────────────────────────────────────────────────────────────────
-- 3) Pattern Catalog, Tenant Bindings, and Runtime State
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS patterns (
    pattern_id   TEXT PRIMARY KEY,
    pattern_name TEXT NOT NULL,
    description  TEXT,
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pattern_configs (
    pattern_id TEXT PRIMARY KEY REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    config     JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS tenant_pattern_configs (
    tenant_id  TEXT NOT NULL,
    pattern_id TEXT NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    config     JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, pattern_id)
);

CREATE INDEX IF NOT EXISTS idx_tenant_pattern_configs_tenant
    ON tenant_pattern_configs (tenant_id);

CREATE TABLE IF NOT EXISTS tenant_pattern_source_bindings (
    tenant_id      TEXT        NOT NULL,
    pattern_id     TEXT        NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    source_id      TEXT        NOT NULL,
    enabled        BOOLEAN     NOT NULL DEFAULT TRUE,
    binding_config JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, pattern_id, source_id),
    FOREIGN KEY (tenant_id, source_id)
      REFERENCES tenant_data_sources(tenant_id, source_id)
      ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_tenant_pattern_source_bindings_tenant_pattern
    ON tenant_pattern_source_bindings (tenant_id, pattern_id);

CREATE TABLE IF NOT EXISTS tenant_pattern_required_assets (
    tenant_id  TEXT        NOT NULL,
    pattern_id TEXT        NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    market_key TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, pattern_id, market_key)
);

CREATE INDEX IF NOT EXISTS idx_tenant_pattern_required_assets_tenant_pattern
    ON tenant_pattern_required_assets (tenant_id, pattern_id);

CREATE TABLE IF NOT EXISTS tenant_pattern_alert_policies (
    tenant_id          TEXT        NOT NULL,
    pattern_id         TEXT        NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    severity_threshold TEXT        NOT NULL DEFAULT 'medium',
    cooldown_sec       INTEGER     NOT NULL DEFAULT 300,
    default_channels   TEXT[]      NOT NULL DEFAULT '{webhook}',
    route_overrides    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, pattern_id)
);

CREATE INDEX IF NOT EXISTS idx_tenant_pattern_alert_policies_tenant
    ON tenant_pattern_alert_policies (tenant_id);

CREATE TABLE IF NOT EXISTS tenant_pattern_notification_channels (
    tenant_id          TEXT        NOT NULL,
    pattern_id         TEXT        NOT NULL REFERENCES patterns(pattern_id) ON DELETE CASCADE,
    channel            TEXT        NOT NULL,
    enabled            BOOLEAN     NOT NULL DEFAULT FALSE,
    config_json        JSONB       NOT NULL DEFAULT '{}'::jsonb,
    use_tenant_default BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, pattern_id, channel),
    CHECK (channel IN ('webhook', 'slack', 'telegram', 'discord'))
);

CREATE INDEX IF NOT EXISTS idx_tenant_pattern_notification_channels_tenant
    ON tenant_pattern_notification_channels (tenant_id, pattern_id);

CREATE TABLE IF NOT EXISTS pattern_state (
    tenant_id  TEXT        NOT NULL,
    pattern_id TEXT        NOT NULL,
    state_key  TEXT        NOT NULL,
    data       JSONB       NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, pattern_id, state_key)
);

CREATE INDEX IF NOT EXISTS idx_pattern_state_tenant_pattern
    ON pattern_state (tenant_id, pattern_id);

CREATE TABLE IF NOT EXISTS pattern_snapshots (
    id           BIGSERIAL   PRIMARY KEY,
    tenant_id    TEXT        NOT NULL,
    pattern_id   TEXT        NOT NULL,
    snapshot_key TEXT        NOT NULL,
    data         JSONB       NOT NULL,
    score        DOUBLE PRECISION,
    severity     TEXT,
    observed_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_pattern_snapshots_tenant_pattern_observed
    ON pattern_snapshots (tenant_id, pattern_id, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_pattern_snapshots_tenant_key_observed
    ON pattern_snapshots (tenant_id, snapshot_key, observed_at DESC);

CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant_id          TEXT PRIMARY KEY,
    severity_threshold TEXT    NOT NULL DEFAULT 'medium',
    cooldown_sec       INTEGER NOT NULL DEFAULT 300,
    default_channels   TEXT[]  NOT NULL DEFAULT '{webhook}',
    protocol_watchlist TEXT[]  NOT NULL DEFAULT '{}',
    route_overrides    JSONB   NOT NULL DEFAULT '{}'::jsonb,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─────────────────────────────────────────────────────────────────────────────
-- 4) Detection and Alert Pipeline
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS detections (
    id           TEXT PRIMARY KEY,
    tx_hash      TEXT NOT NULL,
    chain        TEXT NOT NULL,
    protocol     TEXT NOT NULL,
    subject_type TEXT,
    subject_key  TEXT,
    tenant_id    TEXT,
    pattern_id   TEXT,
    severity     TEXT NOT NULL,
    risk_score   DOUBLE PRECISION NOT NULL,
    payload      JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_detections_chain ON detections(chain);
CREATE INDEX IF NOT EXISTS idx_detections_protocol ON detections(protocol);
CREATE INDEX IF NOT EXISTS idx_detections_tx_hash ON detections(tx_hash);
CREATE INDEX IF NOT EXISTS idx_detections_created_at ON detections(created_at);
CREATE INDEX IF NOT EXISTS idx_detections_subject_created
    ON detections (subject_type, subject_key, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_detections_tenant_created
    ON detections (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_detections_tenant_pattern_created
    ON detections (tenant_id, pattern_id, created_at DESC);

CREATE TABLE IF NOT EXISTS alerts (
    id              TEXT PRIMARY KEY,
    incident_id     TEXT,
    tx_hash         TEXT NOT NULL,
    chain           TEXT NOT NULL,
    chain_slug      TEXT NOT NULL DEFAULT 'unknown',
    protocol        TEXT NOT NULL,
    block_number    BIGINT,
    subject_type    TEXT,
    subject_key     TEXT,
    tenant_id       TEXT,
    pattern_id      TEXT,
    lifecycle_state TEXT NOT NULL DEFAULT 'confirmed',
    severity        TEXT NOT NULL,
    risk_score      DOUBLE PRECISION NOT NULL,
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_alerts_chain ON alerts(chain);
CREATE INDEX IF NOT EXISTS idx_alerts_protocol ON alerts(protocol);
CREATE INDEX IF NOT EXISTS idx_alerts_tx_hash ON alerts(tx_hash);
CREATE INDEX IF NOT EXISTS idx_alerts_incident_id ON alerts(incident_id);
CREATE INDEX IF NOT EXISTS idx_alerts_created_at ON alerts(created_at);
CREATE INDEX IF NOT EXISTS idx_alerts_subject_created
    ON alerts (subject_type, subject_key, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alerts_tenant_created
    ON alerts (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alerts_tenant_pattern_created
    ON alerts (tenant_id, pattern_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alerts_tenant_lifecycle
    ON alerts (tenant_id, lifecycle_state, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alerts_tenant_severity
    ON alerts (tenant_id, severity, created_at DESC);

CREATE TABLE IF NOT EXISTS alert_lifecycle_events (
    id              BIGSERIAL PRIMARY KEY,
    alert_id        TEXT NOT NULL,
    incident_id     TEXT,
    event_key       TEXT,
    tx_hash         TEXT NOT NULL DEFAULT '',
    block_number    BIGINT NOT NULL DEFAULT 0,
    lifecycle_state TEXT NOT NULL DEFAULT 'confirmed',
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_alert_lifecycle_alert_id
    ON alert_lifecycle_events (alert_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alert_lifecycle_incident_id
    ON alert_lifecycle_events (incident_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alert_lifecycle_events_event_key
    ON alert_lifecycle_events (event_key);

CREATE TABLE IF NOT EXISTS incidents (
    incident_id      TEXT PRIMARY KEY,
    tenant_id        TEXT NOT NULL,
    pattern_id       TEXT NOT NULL,
    subject_type     TEXT,
    subject_key      TEXT,
    chain_slug       TEXT NOT NULL DEFAULT 'unknown',
    status           TEXT NOT NULL DEFAULT 'triggered',
    current_severity TEXT NOT NULL DEFAULT 'medium',
    opened_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at        TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_incidents_tenant_status_updated
    ON incidents (tenant_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_incidents_tenant_pattern_subject
    ON incidents (tenant_id, pattern_id, subject_key, chain_slug);
CREATE INDEX IF NOT EXISTS idx_incidents_chain_pattern_status
    ON incidents (chain_slug, pattern_id, status, updated_at DESC);

CREATE TABLE IF NOT EXISTS incident_events (
    id               BIGSERIAL PRIMARY KEY,
    incident_id      TEXT NOT NULL REFERENCES incidents(incident_id) ON DELETE CASCADE,
    transition_type  TEXT NOT NULL,
    from_state       TEXT,
    to_state         TEXT,
    reason           TEXT,
    payload          JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_incident_events_incident_created
    ON incident_events (incident_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_incident_events_transition_created
    ON incident_events (transition_type, created_at DESC);

CREATE TABLE IF NOT EXISTS incident_context_snapshots (
    id             BIGSERIAL PRIMARY KEY,
    incident_id    TEXT NOT NULL REFERENCES incidents(incident_id) ON DELETE CASCADE,
    classification TEXT,
    score          DOUBLE PRECISION,
    confidence     DOUBLE PRECISION,
    payload        JSONB NOT NULL DEFAULT '{}'::jsonb,
    observed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_incident_context_snapshots_incident_observed
    ON incident_context_snapshots (incident_id, observed_at DESC);

CREATE TABLE IF NOT EXISTS alert_delivery_attempts (
    id          BIGSERIAL PRIMARY KEY,
    alert_id    TEXT NOT NULL,
    tenant_id   TEXT NOT NULL,
    channel     TEXT NOT NULL,
    delivered   BOOLEAN NOT NULL,
    reason      TEXT,
    status_code INTEGER,
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_alert_delivery_attempts_alert
    ON alert_delivery_attempts (alert_id, attempted_at DESC);
CREATE INDEX IF NOT EXISTS idx_alert_delivery_attempts_tenant
    ON alert_delivery_attempts (tenant_id, attempted_at DESC);

CREATE TABLE IF NOT EXISTS usage_events (
    id          BIGSERIAL PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    alert_type  TEXT NOT NULL,
    chain_id    BIGINT,
    quantity    INTEGER NOT NULL DEFAULT 1,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_usage_events_tenant_recorded
    ON usage_events (tenant_id, recorded_at DESC);

-- ─────────────────────────────────────────────────────────────────────────────
-- 5) Indexer Durable State + Dead Letter Queue
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS indexer_state (
    chain                  TEXT PRIMARY KEY,
    last_indexed_block     BIGINT NOT NULL,
    last_block_hash        TEXT NOT NULL,
    last_block_timestamp   TIMESTAMPTZ,
    processed_events_count BIGINT NOT NULL DEFAULT 0,
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS processed_events (
    event_id      UUID PRIMARY KEY,
    tx_hash       TEXT NOT NULL,
    block_number  BIGINT NOT NULL,
    block_hash    TEXT NOT NULL,
    chain         TEXT NOT NULL,
    event_key     TEXT,
    processed_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    reverted      BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE (tx_hash, block_number, chain)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_processed_events_event_key
    ON processed_events (event_key);
CREATE INDEX IF NOT EXISTS idx_processed_events_chain_block
    ON processed_events (chain, block_number);

CREATE TABLE IF NOT EXISTS normalized_events (
    event_key              TEXT PRIMARY KEY,
    event_id               UUID NOT NULL,
    chain                  TEXT NOT NULL,
    chain_slug             TEXT NOT NULL,
    chain_id               BIGINT,
    protocol               TEXT NOT NULL,
    protocol_category      TEXT NOT NULL,
    event_type             TEXT NOT NULL CHECK (event_type IN ('oracle_update', 'flash_loan_candidate')),
    tx_hash                TEXT NOT NULL,
    block_number           BIGINT NOT NULL,
    block_hash             TEXT,
    tx_index               BIGINT,
    log_index              BIGINT,
    status                 TEXT NOT NULL,
    lifecycle_state        TEXT NOT NULL,
    requires_confirmation  BOOLEAN NOT NULL,
    confirmation_depth     BIGINT NOT NULL,
    observed_at            TIMESTAMPTZ NOT NULL,
    reverted               BOOLEAN NOT NULL DEFAULT FALSE,
    payload                JSONB NOT NULL,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_normalized_events_chain_block
    ON normalized_events (chain_slug, block_number);
CREATE INDEX IF NOT EXISTS idx_normalized_events_tx_hash
    ON normalized_events (tx_hash);
CREATE INDEX IF NOT EXISTS idx_normalized_events_observed_at
    ON normalized_events (observed_at);
CREATE INDEX IF NOT EXISTS idx_normalized_events_reverted
    ON normalized_events (reverted);

CREATE TABLE IF NOT EXISTS dead_letter_queue (
    id               TEXT PRIMARY KEY,
    stream_name      TEXT NOT NULL,
    entry_id         TEXT NOT NULL,
    payload          JSONB NOT NULL,
    error_message    TEXT NOT NULL,
    retry_count      INTEGER NOT NULL DEFAULT 0,
    first_failure_at TIMESTAMPTZ NOT NULL,
    last_failure_at  TIMESTAMPTZ NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_dead_letter_queue_stream_name
    ON dead_letter_queue (stream_name);
CREATE INDEX IF NOT EXISTS idx_dead_letter_queue_created_at
    ON dead_letter_queue (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_dead_letter_queue_retry_count
    ON dead_letter_queue (retry_count DESC);

-- ─────────────────────────────────────────────────────────────────────────────
-- 6) Finality, Dependency Graph, and Feature Storage
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS finality_state (
    chain              TEXT PRIMARY KEY,
    head_block         BIGINT NOT NULL,
    confirmation_depth INTEGER NOT NULL,
    blocks             JSONB NOT NULL,
    states             JSONB NOT NULL,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS dependency_edges (
    id         BIGSERIAL PRIMARY KEY,
    chain      TEXT NOT NULL,
    protocol   TEXT NOT NULL,
    source     TEXT NOT NULL,
    target     TEXT NOT NULL,
    relation   TEXT NOT NULL,
    weight     DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_dependency_edges_protocol
    ON dependency_edges (chain, protocol);

CREATE TABLE IF NOT EXISTS feature_vectors (
    id                  BIGSERIAL PRIMARY KEY,
    detection_id        TEXT NOT NULL,
    tenant_id           TEXT,
    feature_set_version TEXT NOT NULL,
    values              JSONB NOT NULL,
    labels              JSONB NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_feature_vectors_detection
    ON feature_vectors (detection_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- 7) History Intelligence Layer (Replay + Case Intelligence + ML Registry)
-- ─────────────────────────────────────────────────────────────────────────────

CREATE SCHEMA IF NOT EXISTS history;

CREATE TABLE IF NOT EXISTS history.cases (
    case_id            TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL,
    case_type          TEXT NOT NULL CHECK (case_type IN ('exploit', 'market_stress', 'anomaly')),
    classification     TEXT NOT NULL,
    severity_peak      TEXT NOT NULL DEFAULT 'medium',
    status             TEXT NOT NULL DEFAULT 'open',
    chain_slug         TEXT,
    protocol           TEXT,
    title              TEXT NOT NULL,
    summary            TEXT,
    incident_start_at  TIMESTAMPTZ,
    incident_end_at    TIMESTAMPTZ,
    loss_usd_estimate  TEXT,
    source_confidence  DOUBLE PRECISION,
    source_payload     JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_cases_tenant_start
    ON history.cases (tenant_id, incident_start_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_cases_tenant_class_start
    ON history.cases (tenant_id, classification, incident_start_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_cases_chain_protocol_start
    ON history.cases (chain_slug, protocol, incident_start_at DESC);

CREATE TABLE IF NOT EXISTS history.case_events (
    event_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    case_id        TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    tenant_id      TEXT NOT NULL,
    event_type     TEXT NOT NULL,
    event_ts       TIMESTAMPTZ NOT NULL,
    source_table   TEXT NOT NULL,
    source_pk      TEXT NOT NULL,
    payload_json   JSONB NOT NULL DEFAULT '{}'::jsonb,
    ingested_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (source_table, source_pk)
);

CREATE INDEX IF NOT EXISTS idx_history_case_events_case_ts
    ON history.case_events (case_id, event_ts);
CREATE INDEX IF NOT EXISTS idx_history_case_events_tenant_ts
    ON history.case_events (tenant_id, event_ts DESC);

CREATE TABLE IF NOT EXISTS history.case_alert_links (
    id              BIGSERIAL PRIMARY KEY,
    case_id         TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    tenant_id       TEXT NOT NULL,
    alert_id        TEXT NOT NULL,
    incident_id     TEXT,
    pattern_id      TEXT,
    severity        TEXT,
    delivery_status TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (case_id, alert_id)
);

CREATE INDEX IF NOT EXISTS idx_history_case_alert_links_case_created
    ON history.case_alert_links (case_id, created_at);
CREATE INDEX IF NOT EXISTS idx_history_case_alert_links_tenant_pattern_created
    ON history.case_alert_links (tenant_id, pattern_id, created_at DESC);

CREATE TABLE IF NOT EXISTS history.replay_catalog (
    scenario_id                  TEXT PRIMARY KEY,
    tenant_id                    TEXT NOT NULL,
    case_id                      TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    slug                         TEXT NOT NULL,
    title                        TEXT NOT NULL,
    category                     TEXT NOT NULL CHECK (category IN ('market_stress', 'exploit', 'anomaly')),
    tags                         TEXT[] NOT NULL DEFAULT '{}',
    incident_class               TEXT,
    chain                        TEXT,
    protocol                     TEXT,
    protocol_category            TEXT,
    description                  TEXT,
    impact_summary               TEXT,
    losses_usd_estimate          TEXT,
    attack_vector                TEXT,
    detection_focus              TEXT,
    default_time_window_start    TIMESTAMPTZ NOT NULL,
    default_time_window_end      TIMESTAMPTZ NOT NULL,
    default_speed                INTEGER NOT NULL DEFAULT 10,
    supported_patterns           TEXT[] NOT NULL DEFAULT '{}',
    supported_override_keys      TEXT[] NOT NULL DEFAULT '{}',
    baseline_expected_alerts     INTEGER NOT NULL DEFAULT 0,
    expected_alerts_json         JSONB NOT NULL DEFAULT '[]'::jsonb,
    timeline_json                JSONB NOT NULL DEFAULT '[]'::jsonb,
    references_json              JSONB NOT NULL DEFAULT '[]'::jsonb,
    source_feeds_json            JSONB NOT NULL DEFAULT '[]'::jsonb,
    runbook_notes                TEXT[] NOT NULL DEFAULT '{}',
    dataset_version              TEXT NOT NULL DEFAULT 'v1',
    object_prefix                TEXT NOT NULL,
    checksum                     TEXT NOT NULL,
    simlab_scenario_id           TEXT,
    is_active                    BOOLEAN NOT NULL DEFAULT TRUE,
    created_at                   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (tenant_id, slug, dataset_version)
);

CREATE INDEX IF NOT EXISTS idx_history_replay_catalog_tenant_active
    ON history.replay_catalog (tenant_id, is_active, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_replay_catalog_case
    ON history.replay_catalog (case_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS history.ml_feature_registry (
    feature_slice_id      TEXT PRIMARY KEY,
    tenant_id             TEXT NOT NULL,
    case_id               TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    feature_set_version   TEXT NOT NULL,
    label_set_version     TEXT NOT NULL,
    s3_uri                TEXT NOT NULL,
    row_count             BIGINT NOT NULL DEFAULT 0,
    metadata_json         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_ml_feature_registry_case
    ON history.ml_feature_registry (case_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_ml_feature_registry_tenant_feature
    ON history.ml_feature_registry (tenant_id, feature_set_version, created_at DESC);

CREATE TABLE IF NOT EXISTS history.dataset_manifests (
    manifest_id        TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL,
    case_id            TEXT REFERENCES history.cases(case_id) ON DELETE SET NULL,
    dataset_version    TEXT NOT NULL,
    checksum           TEXT NOT NULL,
    source_export_refs JSONB NOT NULL DEFAULT '[]'::jsonb,
    payload            JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_dataset_manifests_tenant_version
    ON history.dataset_manifests (tenant_id, dataset_version, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_dataset_manifests_case
    ON history.dataset_manifests (case_id, created_at DESC);

CREATE TABLE IF NOT EXISTS history.case_data_provenance (
    provenance_id   TEXT PRIMARY KEY,
    case_id         TEXT NOT NULL REFERENCES history.cases(case_id) ON DELETE CASCADE,
    tenant_id       TEXT NOT NULL,
    provider        TEXT NOT NULL,
    query_window    JSONB NOT NULL DEFAULT '{}'::jsonb,
    hash_chain      JSONB NOT NULL DEFAULT '[]'::jsonb,
    source_manifest TEXT,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_history_case_data_provenance_case
    ON history.case_data_provenance (case_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_case_data_provenance_tenant_provider
    ON history.case_data_provenance (tenant_id, provider, created_at DESC);

CREATE TABLE IF NOT EXISTS history.ingest_offsets (
    source_name   TEXT PRIMARY KEY,
    last_seen_ts  TIMESTAMPTZ NOT NULL DEFAULT to_timestamp(0),
    last_seen_id  TEXT NOT NULL DEFAULT '',
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ============================================================================
-- Notes
-- ============================================================================
-- For high-volume production workloads, consider:
-- 1. Time-based partitioning on ingest_operational_events, detections, alerts
-- 2. Retention/archive jobs for historical rows
-- 3. Connection pooling (PgBouncer)
-- 4. Read replicas for analytics
-- ============================================================================
