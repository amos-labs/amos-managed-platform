-- Subscription credit balance for shared-Bedrock allowance.
--
-- Each tenant on the $49.99/mo plan gets $5 of Haiku-only Bedrock credits
-- (50,000 microcents) granted at subscription creation and reset at
-- each invoice.paid. Microcents = hundredths of a cent ($1 = 10,000
-- microcents), matching the existing metered_billing convention.
-- Hard cutoff at zero: harness gates shared-Bedrock calls when balance
-- is insufficient. See docs/SUBSCRIPTION_AND_ONBOARDING.md in amos-automate.
--
-- Credits are not used for BYOK calls. Deduction happens when the harness
-- activity report lands and llm_usage_records are written for shared-Bedrock
-- usage.

ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS credit_balance_microcents BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS credits_granted_at TIMESTAMPTZ;
