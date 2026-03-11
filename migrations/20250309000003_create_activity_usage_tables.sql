-- Activity reports and usage metrics tables for billing
-- Supports harness activity ingestion and usage tracking

-- ── Activity Reports ────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS activity_reports (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    period_start    TIMESTAMPTZ NOT NULL,
    period_end      TIMESTAMPTZ NOT NULL,
    conversations   BIGINT NOT NULL DEFAULT 0,
    messages        BIGINT NOT NULL DEFAULT 0,
    tokens_input    BIGINT NOT NULL DEFAULT 0,
    tokens_output   BIGINT NOT NULL DEFAULT 0,
    tools_executed  BIGINT NOT NULL DEFAULT 0,
    models_used     TEXT[] NOT NULL DEFAULT '{}',
    received_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_activity_reports_tenant_id ON activity_reports (tenant_id);
CREATE INDEX IF NOT EXISTS idx_activity_reports_period ON activity_reports (period_start, period_end);
CREATE INDEX IF NOT EXISTS idx_activity_reports_received_at ON activity_reports (received_at);

COMMENT ON TABLE activity_reports IS 'Raw activity reports received from harness instances';
COMMENT ON COLUMN activity_reports.period_start IS 'Start of the activity period being reported';
COMMENT ON COLUMN activity_reports.period_end IS 'End of the activity period being reported';

-- ── Usage Metrics (aggregated) ─────────────────────────────────────

CREATE TABLE IF NOT EXISTS usage_metrics (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    period_start    TIMESTAMPTZ NOT NULL,
    conversations   BIGINT NOT NULL DEFAULT 0,
    messages        BIGINT NOT NULL DEFAULT 0,
    tokens_input    BIGINT NOT NULL DEFAULT 0,
    tokens_output   BIGINT NOT NULL DEFAULT 0,
    tools_executed  BIGINT NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Each tenant has one usage metric row per period
    UNIQUE (tenant_id, period_start)
);

CREATE INDEX IF NOT EXISTS idx_usage_metrics_tenant_id ON usage_metrics (tenant_id);
CREATE INDEX IF NOT EXISTS idx_usage_metrics_period ON usage_metrics (period_start);

COMMENT ON TABLE usage_metrics IS 'Aggregated usage metrics per tenant per period (typically monthly)';
COMMENT ON COLUMN usage_metrics.period_start IS 'Start of the billing period (e.g., 2025-03-01 00:00:00)';

-- ── Harness Configs ─────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS harness_configs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE UNIQUE,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    feature_flags   JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_harness_configs_tenant_id ON harness_configs (tenant_id);

COMMENT ON TABLE harness_configs IS 'Harness-specific configuration distributed via sync API';
COMMENT ON COLUMN harness_configs.feature_flags IS 'Feature flags as JSON object, e.g. {"sovereign_ai": true}';

-- Trigger to auto-update updated_at
CREATE TRIGGER update_usage_metrics_updated_at BEFORE UPDATE ON usage_metrics
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER update_harness_configs_updated_at BEFORE UPDATE ON harness_configs
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
