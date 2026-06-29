//! Authentication module: JWT tokens, password hashing, and session management.

use amos_core::AmosError;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// JWT claims embedded in access tokens.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    /// Subject: user ID (UUID).
    pub sub: String,
    /// Tenant ID.
    pub tenant_id: String,
    /// User role within the tenant.
    pub role: String,
    /// Tenant slug (for routing).
    pub tenant_slug: String,
    /// Issued at (Unix timestamp).
    pub iat: i64,
    /// Expiration (Unix timestamp).
    pub exp: i64,
    /// For API-key principals (the AI/machine axis): the key's explicit scope
    /// list, which narrows the creator's role scopes. `None` for JWT users
    /// (scopes derive from `role`). Not part of the JWT payload — populated at
    /// authenticate time for API keys — so it stays absent from issued tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

/// Token pair returned after login or refresh.
#[derive(Debug, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
}

// ── Password Hashing ────────────────────────────────────────────────────

/// Hash a plaintext password using Argon2id.
pub fn hash_password(password: &str) -> Result<String, AmosError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AmosError::Internal(format!("Password hashing failed: {}", e)))?;
    Ok(hash.to_string())
}

/// Verify a plaintext password against an Argon2id hash.
pub fn verify_password(password: &str, hash: &str) -> Result<bool, AmosError> {
    let parsed_hash = PasswordHash::new(hash)
        .map_err(|e| AmosError::Internal(format!("Invalid password hash format: {}", e)))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

// ── JWT Tokens ──────────────────────────────────────────────────────────

/// Create an access token (short-lived).
pub fn create_access_token(
    user_id: Uuid,
    tenant_id: Uuid,
    role: &str,
    tenant_slug: &str,
    jwt_secret: &str,
    expiry_secs: i64,
) -> Result<String, AmosError> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id.to_string(),
        tenant_id: tenant_id.to_string(),
        role: role.to_string(),
        tenant_slug: tenant_slug.to_string(),
        iat: now.timestamp(),
        exp: (now + Duration::seconds(expiry_secs)).timestamp(),
        // JWT users derive scopes from their role; only API keys carry an
        // explicit scope subset (set at authenticate time).
        scopes: None,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .map_err(|e| AmosError::Internal(format!("JWT encoding failed: {}", e)))
}

/// Create a refresh token (long-lived, opaque random string).
pub fn create_refresh_token() -> String {
    // Generate 32 random bytes, encode as URL-safe base64
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen::<u8>()).collect();
    base64_url_encode(&bytes)
}

/// Validate an access token and return claims.
pub fn validate_access_token(token: &str, jwt_secret: &str) -> Result<Claims, AmosError> {
    let mut validation = Validation::default();
    validation.leeway = 0; // Strict expiration checking, no grace period
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &validation,
    )
    .map_err(|e| AmosError::Unauthorized(format!("Invalid or expired token: {}", e)))?;

    Ok(token_data.claims)
}

/// Hash a refresh token for database storage (we never store the raw token).
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

// ── API Key Generation ──────────────────────────────────────────────────

/// Generate a new API key with prefix.
///
/// Returns `(full_key, prefix, key_hash)`.
/// The full key is shown once to the user; only the hash is stored.
pub fn generate_api_key() -> (String, String, String) {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    // Generate 32 random bytes
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen::<u8>()).collect();
    let encoded = base64_url_encode(&bytes);

    let full_key = format!("amos_k_{}", encoded);
    let prefix = full_key[..12].to_string();
    let key_hash = hash_token(&full_key);

    (full_key, prefix, key_hash)
}

/// Validate an API key by checking its hash against the database hash.
pub fn validate_api_key_hash(provided_key: &str, stored_hash: &str) -> bool {
    hash_token(provided_key) == stored_hash
}

// ── Password Reset Tokens ─────────────────────────────────────────────────

/// How long a password-reset token stays valid. Tokens are delivered out of
/// band (an admin hands the user the link), so the window is generous: 24h.
pub const RESET_TOKEN_TTL_SECS: i64 = 24 * 60 * 60;

/// Generate a password-reset token. Returns `(raw, hash)`: the raw token goes
/// in the reset URL (shown once), only the hash is stored in the database — so
/// a leaked database row can't be turned back into a working link.
pub fn generate_reset_token() -> (String, String) {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen::<u8>()).collect();
    let raw = base64_url_encode(&bytes);
    let hash = hash_token(&raw);
    (raw, hash)
}

/// Whether a reset-token row is currently usable: not yet consumed and not
/// expired. (DB queries enforce this too; this keeps the rule unit-testable.)
pub fn reset_token_is_usable(
    used_at: Option<chrono::DateTime<Utc>>,
    expires_at: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
) -> bool {
    used_at.is_none() && expires_at > now
}

/// Validate a chosen new password: minimum length and confirmation match.
/// Returns the reason on failure so callers can surface it directly.
pub fn validate_new_password(new_password: &str, confirm: &str) -> Result<(), &'static str> {
    if new_password.len() < 8 {
        return Err("Password must be at least 8 characters.");
    }
    if new_password != confirm {
        return Err("Passwords do not match.");
    }
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a URL-safe slug from a name.
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password_hash_and_verify() {
        let password = "test-password-123!";
        let hash = hash_password(password).unwrap();
        assert!(verify_password(password, &hash).unwrap());
        assert!(!verify_password("wrong-password", &hash).unwrap());
    }

    #[test]
    fn reset_token_raw_hashes_to_stored() {
        let (raw, hash) = generate_reset_token();
        // The lookup recomputes the hash from the raw token in the URL.
        assert_eq!(hash_token(&raw), hash);
        // Two tokens never collide.
        let (raw2, hash2) = generate_reset_token();
        assert_ne!(raw, raw2);
        assert_ne!(hash, hash2);
    }

    #[test]
    fn reset_token_usability_respects_expiry_and_use() {
        let now = Utc::now();
        let future = now + Duration::seconds(60);
        let past = now - Duration::seconds(60);
        // Fresh, unexpired, unused → usable.
        assert!(reset_token_is_usable(None, future, now));
        // Expired → not usable.
        assert!(!reset_token_is_usable(None, past, now));
        // Already used → not usable (single-use).
        assert!(!reset_token_is_usable(Some(now), future, now));
    }

    #[test]
    fn new_password_validation() {
        assert!(validate_new_password("longenough", "longenough").is_ok());
        // Too short.
        assert!(validate_new_password("short", "short").is_err());
        // Mismatch.
        assert!(validate_new_password("longenough", "different1").is_err());
    }

    #[test]
    fn test_jwt_roundtrip() {
        let secret = "test-secret-key-for-jwt-signing";
        let user_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();

        let token =
            create_access_token(user_id, tenant_id, "admin", "test-org", secret, 3600).unwrap();

        let claims = validate_access_token(&token, secret).unwrap();
        assert_eq!(claims.sub, user_id.to_string());
        assert_eq!(claims.tenant_id, tenant_id.to_string());
        assert_eq!(claims.role, "admin");
        assert_eq!(claims.tenant_slug, "test-org");
    }

    #[test]
    fn test_jwt_expired() {
        let secret = "test-secret-key-for-jwt-signing";
        let user_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();

        // Create token that expired 1 second ago
        let token =
            create_access_token(user_id, tenant_id, "member", "expired-org", secret, -1).unwrap();

        let result = validate_access_token(&token, secret);
        assert!(result.is_err());
    }

    #[test]
    fn test_api_key_generation() {
        let (full_key, prefix, hash) = generate_api_key();
        assert!(full_key.starts_with("amos_k_"));
        assert_eq!(prefix.len(), 12);
        assert!(validate_api_key_hash(&full_key, &hash));
        assert!(!validate_api_key_hash("wrong-key", &hash));
    }

    #[test]
    fn test_refresh_token_hashing() {
        let token = create_refresh_token();
        let hash1 = hash_token(&token);
        let hash2 = hash_token(&token);
        assert_eq!(hash1, hash2); // deterministic
        assert_ne!(hash_token("other-token"), hash1);
    }

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Acme Corp"), "acme-corp");
        assert_eq!(slugify("My  Cool  Company!!!"), "my-cool-company");
        assert_eq!(slugify("hello-world"), "hello-world");
        assert_eq!(slugify("  spaces  "), "spaces");
    }
}
