-- Releases table: tracks built images registered by CI.
-- Each row represents a deployable version that users can choose to update to.

CREATE TABLE IF NOT EXISTS releases (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    version         VARCHAR(80) NOT NULL UNIQUE,   -- e.g. "sha-abc123def"
    commit_sha      VARCHAR(40) NOT NULL,
    harness_image   VARCHAR(512) NOT NULL,          -- full ECR URI
    agent_image     VARCHAR(512),                   -- full ECR URI (nullable)
    status          VARCHAR(20) NOT NULL DEFAULT 'available'
                    CHECK (status IN ('available', 'deprecated', 'yanked')),
    release_notes   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_releases_created_at ON releases (created_at DESC);

-- Track which image tag each harness is running, and what it ran before (for rollback).
ALTER TABLE harness_instances
    ADD COLUMN IF NOT EXISTS image_tag VARCHAR(80),
    ADD COLUMN IF NOT EXISTS previous_image_tag VARCHAR(80);
