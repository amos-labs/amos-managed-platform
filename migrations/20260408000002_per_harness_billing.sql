-- Per-harness billing: each harness instance is a billable unit.
-- Billing moves from "tenant has a plan" to "each harness has a price".
-- The tenant account is free; you pay for compute (running harnesses).

-- Add billing columns to harness_instances
ALTER TABLE harness_instances
    ADD COLUMN IF NOT EXISTS billing_status VARCHAR(30) NOT NULL DEFAULT 'none'
        CHECK (billing_status IN ('none', 'active', 'past_due', 'canceled', 'free')),
    ADD COLUMN IF NOT EXISTS stripe_subscription_item_id VARCHAR(255),
    ADD COLUMN IF NOT EXISTS monthly_price_cents INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS price_id VARCHAR(255);

CREATE INDEX IF NOT EXISTS idx_harness_billing_status
    ON harness_instances (billing_status) WHERE billing_status != 'none';

CREATE INDEX IF NOT EXISTS idx_harness_stripe_sub_item
    ON harness_instances (stripe_subscription_item_id) WHERE stripe_subscription_item_id IS NOT NULL;

-- Add storage_gb to harness_instances for tracking per-instance storage
ALTER TABLE harness_instances
    ADD COLUMN IF NOT EXISTS storage_gb INTEGER NOT NULL DEFAULT 10;

-- Track the total harness count limit per tenant (default 1 for free, unlimited for paid)
ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS max_harnesses INTEGER NOT NULL DEFAULT 1;

COMMENT ON COLUMN harness_instances.billing_status IS 'Billing state: none=self-hosted/free, active=paying, past_due=payment failed, canceled=subscription ended, free=included free tier';
COMMENT ON COLUMN harness_instances.stripe_subscription_item_id IS 'Stripe subscription item ID linking this harness to a line item on the customer subscription';
COMMENT ON COLUMN harness_instances.monthly_price_cents IS 'Monthly price for this harness in cents (4500=small, 9500=medium, 19500=large)';
COMMENT ON COLUMN harness_instances.price_id IS 'Stripe Price ID used for this harness instance';
COMMENT ON COLUMN tenants.max_harnesses IS 'Maximum harnesses this tenant can provision (1=free tier default, higher for paid)';
