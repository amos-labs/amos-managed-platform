-- Tenant, user, authentication, and harness instance tables
-- Supports multi-tenancy, JWT auth, API keys, and harness provisioning tracking

-- ── Tenants (organizations) ────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS tenants (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        VARCHAR(255) NOT NULL,
    slug        VARCHAR(63) NOT NULL UNIQUE,       -- used for subdomain routing: {slug}.amos.ai
    plan        VARCHAR(50) NOT NULL DEFAULT 'free' CHECK (plan IN ('free', 'starter', 'growth', 'enterprise')),
    -- Deployment mode
    deployment_mode VARCHAR(20) NOT NULL DEFAULT 'managed' CHECK (deployment_mode IN ('managed', 'self_hosted')),
    -- Subdomain: only set for managed deployments
    subdomain   VARCHAR(63) UNIQUE,                -- e.g. "acme" → acme.amos.ai
    -- Billing
    stripe_customer_id VARCHAR(255),
    -- Metadata
    settings    JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_tenants_slug ON tenants (slug);
CREATE INDEX IF NOT EXISTS idx_tenants_subdomain ON tenants (subdomain) WHERE subdomain IS NOT NULL;

COMMENT ON TABLE tenants IS 'Organizations/entities that own harness instances and users';
COMMENT ON COLUMN tenants.slug IS 'URL-safe identifier, also used as subdomain prefix for managed plan';
COMMENT ON COLUMN tenants.subdomain IS 'Custom subdomain for managed deployments (e.g. acme → acme.amos.ai)';

-- ── Users ──────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    email           VARCHAR(255) NOT NULL,
    name            VARCHAR(255),
    password_hash   TEXT NOT NULL,           -- argon2id hash
    role            VARCHAR(50) NOT NULL DEFAULT 'member' CHECK (role IN ('owner', 'admin', 'member', 'viewer')),
    -- Solana wallet (optional, for governance / staking)
    wallet_address  VARCHAR(64),
    -- Status
    email_verified  BOOLEAN NOT NULL DEFAULT FALSE,
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    -- Metadata
    last_login_at   TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Each email is unique within a tenant
    UNIQUE (tenant_id, email)
);

CREATE INDEX IF NOT EXISTS idx_users_tenant_id ON users (tenant_id);
CREATE INDEX IF NOT EXISTS idx_users_email ON users (email);

COMMENT ON TABLE users IS 'Platform users, each belonging to exactly one tenant';
COMMENT ON COLUMN users.role IS 'owner: full control, admin: manage users+settings, member: use harness, viewer: read-only';

-- ── Refresh Tokens (sessions) ──────────────────────────────────────────

CREATE TABLE IF NOT EXISTS refresh_tokens (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash      TEXT NOT NULL UNIQUE,     -- SHA-256 hash of the refresh token
    expires_at      TIMESTAMPTZ NOT NULL,
    revoked         BOOLEAN NOT NULL DEFAULT FALSE,
    user_agent      TEXT,
    ip_address      VARCHAR(45),             -- supports IPv6
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_refresh_tokens_user_id ON refresh_tokens (user_id);
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_expires_at ON refresh_tokens (expires_at);

COMMENT ON TABLE refresh_tokens IS 'JWT refresh tokens for session management. Token value is never stored, only its hash.';

-- ── API Keys ───────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS api_keys (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    created_by      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name            VARCHAR(255) NOT NULL,         -- human label, e.g. "Production API Key"
    key_prefix      VARCHAR(12) NOT NULL,          -- first 8 chars of key, for identification: "amos_k_ab"
    key_hash        TEXT NOT NULL UNIQUE,           -- SHA-256 hash of the full API key
    scopes          TEXT[] NOT NULL DEFAULT '{}',   -- e.g. {"billing:read", "harness:write"}
    expires_at      TIMESTAMPTZ,                   -- NULL = never expires
    last_used_at    TIMESTAMPTZ,
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_api_keys_tenant_id ON api_keys (tenant_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_key_prefix ON api_keys (key_prefix);

COMMENT ON TABLE api_keys IS 'API keys for programmatic access. The full key is shown once at creation, only its hash is stored.';
COMMENT ON COLUMN api_keys.key_prefix IS 'First 8 characters of key for display/identification (e.g. amos_k_ab)';

-- ── Harness Instances ──────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS harness_instances (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    -- Container info
    container_id    VARCHAR(128),            -- Docker container ID (NULL if not yet provisioned)
    container_name  VARCHAR(255),
    status          VARCHAR(30) NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'provisioning', 'running', 'stopped', 'error', 'deprovisioned')),
    -- Configuration
    region          VARCHAR(50) NOT NULL DEFAULT 'us-west-2',
    instance_size   VARCHAR(20) NOT NULL DEFAULT 'small' CHECK (instance_size IN ('small', 'medium', 'large')),
    environment     VARCHAR(30) NOT NULL DEFAULT 'production',
    -- Subdomain mapping: this harness is reachable at {subdomain}.amos.ai
    subdomain       VARCHAR(63) UNIQUE,
    internal_url    VARCHAR(512),            -- internal Docker network URL
    external_port   INTEGER,                 -- host port mapped to container
    -- Health
    harness_version VARCHAR(20),
    last_heartbeat  TIMESTAMPTZ,
    healthy         BOOLEAN NOT NULL DEFAULT FALSE,
    -- Lifecycle
    provisioned_at  TIMESTAMPTZ,
    started_at      TIMESTAMPTZ,
    stopped_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_harness_instances_tenant_id ON harness_instances (tenant_id);
CREATE INDEX IF NOT EXISTS idx_harness_instances_status ON harness_instances (status);
CREATE INDEX IF NOT EXISTS idx_harness_instances_subdomain ON harness_instances (subdomain) WHERE subdomain IS NOT NULL;

COMMENT ON TABLE harness_instances IS 'Tracks provisioned harness containers and their lifecycle state';
COMMENT ON COLUMN harness_instances.subdomain IS 'Subdomain for accessing this harness (e.g. acme → acme.amos.ai)';

-- ── Trigger: auto-update updated_at ────────────────────────────────────

CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ language 'plpgsql';

CREATE TRIGGER update_tenants_updated_at BEFORE UPDATE ON tenants
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER update_users_updated_at BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER update_harness_instances_updated_at BEFORE UPDATE ON harness_instances
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
