-- Proof-carrying operations: an auditable receipt per consequential platform
-- operation (deploy_app, build_image, …). The runtime analog of the relay's
-- bounty proof receipt and the proofgate code-PR receipt — see
-- src/proof/mod.rs. The full OperationReceipt is stored as JSONB so it stays
-- self-describing and Oracle-reviewable without a per-field migration.

CREATE TABLE IF NOT EXISTS operation_receipts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    -- The operation performed, e.g. 'deploy_app', 'build_image'.
    operation   VARCHAR(64) NOT NULL,
    -- Authenticated subject (user/agent id) that initiated the operation.
    actor       VARCHAR(128) NOT NULL,
    -- Did every recorded verification check pass (no failures)?
    verified    BOOLEAN NOT NULL DEFAULT FALSE,
    -- The full OperationReceipt (intent, policy, inputs, validation, outputs).
    receipt     JSONB NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_operation_receipts_tenant_id
    ON operation_receipts (tenant_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_operation_receipts_operation
    ON operation_receipts (operation);

COMMENT ON TABLE operation_receipts IS
    'Proof-carrying record of each consequential platform operation (see src/proof/mod.rs).';
