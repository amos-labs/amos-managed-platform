-- Add Stripe billing columns to tenants + webhook event log.

ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS stripe_subscription_id VARCHAR(255),
    ADD COLUMN IF NOT EXISTS stripe_subscription_status VARCHAR(30)
        DEFAULT 'none'
        CHECK (stripe_subscription_status IN (
            'none', 'active', 'past_due', 'canceled', 'incomplete', 'trialing', 'unpaid'
        )),
    ADD COLUMN IF NOT EXISTS subscription_current_period_end TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_tenants_stripe_customer_id
    ON tenants (stripe_customer_id) WHERE stripe_customer_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS stripe_webhook_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stripe_event_id VARCHAR(255) NOT NULL UNIQUE,
    event_type      VARCHAR(100) NOT NULL,
    processed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    payload         JSONB NOT NULL DEFAULT '{}'
);
