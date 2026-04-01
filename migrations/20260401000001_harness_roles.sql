-- Multi-harness orchestration: add role, packages, and parent relationship to harness_instances.
-- Enables primary/specialist/worker harness roles with package-aware provisioning.

ALTER TABLE harness_instances
  ADD COLUMN harness_role VARCHAR(50) NOT NULL DEFAULT 'primary',
  ADD COLUMN packages JSONB NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN parent_harness_id UUID REFERENCES harness_instances(id);

-- Only one primary harness per tenant (non-deprovisioned)
CREATE UNIQUE INDEX idx_one_primary_per_tenant
  ON harness_instances (tenant_id)
  WHERE harness_role = 'primary' AND status != 'deprovisioned';

-- Index for looking up siblings via parent
CREATE INDEX idx_harness_parent ON harness_instances (parent_harness_id)
  WHERE parent_harness_id IS NOT NULL;

-- Index for looking up harnesses by tenant + role
CREATE INDEX idx_harness_role ON harness_instances (tenant_id, harness_role);
