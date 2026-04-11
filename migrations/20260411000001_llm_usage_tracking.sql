-- Per-model LLM token usage tracking for metered billing.
-- Records are created from harness activity reports and billed via Stripe.
CREATE TABLE IF NOT EXISTS llm_usage_records (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    harness_id      UUID NOT NULL REFERENCES harness_instances(id) ON DELETE CASCADE,
    model_id        VARCHAR(200) NOT NULL,
    tokens_input    BIGINT NOT NULL DEFAULT 0,
    tokens_output   BIGINT NOT NULL DEFAULT 0,
    -- Cost stored in hundredths of a cent (1 = $0.0001) for precision
    cost_microcents BIGINT NOT NULL DEFAULT 0,
    period_start    TIMESTAMPTZ NOT NULL,
    period_end      TIMESTAMPTZ NOT NULL,
    reported_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    billed          BOOLEAN NOT NULL DEFAULT FALSE,
    stripe_usage_record_id VARCHAR(255)
);

CREATE INDEX IF NOT EXISTS idx_llm_usage_tenant_period
    ON llm_usage_records (tenant_id, period_start);
CREATE INDEX IF NOT EXISTS idx_llm_usage_unbilled
    ON llm_usage_records (billed) WHERE billed = FALSE;
CREATE INDEX IF NOT EXISTS idx_llm_usage_harness
    ON llm_usage_records (harness_id, period_start);

-- Add LLM provider mode tracking to harness instances
ALTER TABLE harness_instances
    ADD COLUMN IF NOT EXISTS llm_provider_mode VARCHAR(30) NOT NULL DEFAULT 'shared_bedrock',
    ADD COLUMN IF NOT EXISTS stripe_metered_item_id VARCHAR(255);
