-- MCP OAuth connector (OAuth 2.1 + PKCE) so Claude web/mobile/Desktop can
-- connect via the Connectors flow (OAuth discovery) instead of a pasted bearer.
-- Mirrors Nuvola's Doorkeeper-backed flow. All tokens/codes/secrets stored
-- HASHED (sha256 hex), never plaintext — same convention as api_keys.key_hash.

CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id          TEXT PRIMARY KEY,                 -- public identifier (random)
    client_secret_hash TEXT,                             -- sha256 of secret; NULL = public (PKCE) client
    client_name        TEXT NOT NULL DEFAULT 'MCP client',
    redirect_uris      TEXT[] NOT NULL,                  -- exact-match allowlist
    confidential       BOOLEAN NOT NULL DEFAULT FALSE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Short-lived authorization codes (single-use). PKCE challenge bound here,
-- verifier checked at the token endpoint.
CREATE TABLE IF NOT EXISTS oauth_auth_codes (
    code_hash             TEXT PRIMARY KEY,              -- sha256 of the issued code
    client_id             TEXT NOT NULL REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    redirect_uri          TEXT NOT NULL,
    code_challenge        TEXT NOT NULL,                 -- PKCE S256 challenge
    code_challenge_method TEXT NOT NULL DEFAULT 'S256',
    scopes                TEXT[] NOT NULL DEFAULT '{}',  -- scopes the user consented to (role-capped)
    user_id               UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id             UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    consumed              BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at            TIMESTAMPTZ NOT NULL,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Issued access + refresh tokens. Access token resolves to Claims at the MCP
-- endpoint exactly like an api_key (tenant + role-capped scopes).
CREATE TABLE IF NOT EXISTS oauth_tokens (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    access_token_hash   TEXT NOT NULL UNIQUE,            -- sha256 of access token
    refresh_token_hash  TEXT UNIQUE,                     -- sha256 of refresh token
    client_id           TEXT NOT NULL REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    user_id             UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id           UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    scopes              TEXT[] NOT NULL DEFAULT '{}',
    access_expires_at   TIMESTAMPTZ NOT NULL,
    refresh_expires_at  TIMESTAMPTZ,
    revoked             BOOLEAN NOT NULL DEFAULT FALSE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at        TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_oauth_tokens_access ON oauth_tokens (access_token_hash) WHERE revoked = FALSE;
CREATE INDEX IF NOT EXISTS idx_oauth_tokens_refresh ON oauth_tokens (refresh_token_hash) WHERE revoked = FALSE;
CREATE INDEX IF NOT EXISTS idx_oauth_auth_codes_expires ON oauth_auth_codes (expires_at);
