-- Allow 'hosted' plan value and simplify plan options.
-- Existing 'starter', 'growth', 'enterprise' rows are kept for backwards compat
-- but all map to 'hosted' in application code.

ALTER TABLE tenants DROP CONSTRAINT IF EXISTS tenants_plan_check;
ALTER TABLE tenants ADD CONSTRAINT tenants_plan_check
    CHECK (plan IN ('free', 'hosted', 'starter', 'growth', 'enterprise'));
