-- Allow app-hosting tier plan values on tenants (tiered base + container usage;
-- see src/billing/app_hosting.rs). Existing values are preserved.

ALTER TABLE tenants DROP CONSTRAINT IF EXISTS tenants_plan_check;
ALTER TABLE tenants ADD CONSTRAINT tenants_plan_check
    CHECK (plan IN (
        'free', 'hosted', 'starter', 'growth', 'enterprise',
        'app_starter', 'app_pro', 'app_compliance'
    ));
