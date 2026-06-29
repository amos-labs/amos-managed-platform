//! MCP OAuth 2.1 connector — lets Claude web / mobile / Desktop connect to the
//! AMOS MCP via the Connectors flow (OAuth discovery + PKCE) instead of a
//! pasted bearer token. Mirrors Nuvola's proven Doorkeeper-backed flow:
//!
//!   - RFC 8414  GET /.well-known/oauth-authorization-server   (AS metadata)
//!   - RFC 9728  GET /.well-known/oauth-protected-resource     (resource → AS)
//!   - RFC 7591  POST /oauth/register                          (dynamic client reg)
//!   -           GET  /oauth/authorize                         (login + scope consent)
//!   -           POST /oauth/authorize/consent                 (issue auth code)
//!   -           POST /oauth/token                             (code + refresh grants)
//!
//! Security: PKCE S256 mandatory; redirect_uri exact-match against the
//! registered client; short-lived access tokens (1h) + rotating refresh (30d);
//! codes single-use (10m); all secrets/codes/tokens stored sha256-hashed;
//! granted scopes capped to the consenting user's role (`rbac::scopes_for_role`).
//! Issued access tokens resolve to `Claims` in `mcp::authenticate`, exactly
//! like an api_key — so the whole verb surface works over an OAuth connection.

use axum::{
    extract::{Form, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{Duration, Utc};
use rand::RngCore;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::{self, Claims};
use crate::state::PlatformState;

const SESSION_COOKIE: &str = "amos_session";
const ACCESS_TTL_SECS: i64 = 3600; // 1h
const REFRESH_TTL_SECS: i64 = 60 * 60 * 24 * 30; // 30d
const CODE_TTL_SECS: i64 = 600; // 10m
/// Scopes pre-checked on the consent screen (least-privilege default).
const DEFAULT_SCOPES: &[&str] = &["app:read", "harness:read", "receipts:read", "billing:read"];

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/authorize/consent", post(authorize_consent))
        .route("/oauth/token", post(token))
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Public base URL of this MCP server (issuer). The MCP endpoint is `{base}/mcp`.
pub fn base_url() -> String {
    std::env::var("AMOS__PUBLIC__BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://platform.custom.amoslabs.com".to_string())
}

/// 32 random bytes, base64url (no pad) — used for codes, secrets, tokens.
fn random_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

fn sha256_b64url(input: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(input);
    URL_SAFE_NO_PAD.encode(h.finalize())
}

/// PKCE S256: the verifier hashes (sha256 → base64url-no-pad) to the challenge.
pub fn pkce_s256_verify(verifier: &str, challenge: &str) -> bool {
    !verifier.is_empty() && sha256_b64url(verifier.as_bytes()) == challenge
}

/// Cap requested scopes to what the user's role can grant + keep only known
/// scopes. Empty → the least-privilege default set.
pub fn cap_scopes(requested: &[String], role: &str) -> Vec<String> {
    let grantable = crate::rbac::scopes_for_role(role);
    let mut out: Vec<String> = requested
        .iter()
        .filter(|s| grantable.contains(s.as_str()))
        .cloned()
        .collect();
    if out.is_empty() {
        out = DEFAULT_SCOPES
            .iter()
            .filter(|s| grantable.contains(**s))
            .map(|s| s.to_string())
            .collect();
    }
    out.sort();
    out.dedup();
    out
}

/// Resolve the browser session (amos_session JWT cookie) → Claims, or None.
fn session_claims(state: &PlatformState, headers: &HeaderMap) -> Option<Claims> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let token = cookie_header
        .split(';')
        .map(|s| s.trim())
        .find_map(|s| s.strip_prefix(&format!("{SESSION_COOKIE}=")))?;
    if token.is_empty() {
        return None;
    }
    let secret = state.config.auth.jwt_secret.expose_secret();
    auth::validate_access_token(token, secret).ok()
}

// ── discovery metadata ───────────────────────────────────────────────────────

async fn authorization_server_metadata() -> Json<Value> {
    let base = base_url();
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "scopes_supported": crate::rbac::scope::ALL,
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_basic", "client_secret_post"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn protected_resource_metadata() -> Json<Value> {
    let base = base_url();
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "scopes_supported": crate::rbac::scope::ALL,
        "bearer_methods_supported": ["header"],
    }))
}

// ── dynamic client registration (RFC 7591) ────────────────────────────────────

async fn register(State(state): State<PlatformState>, body: String) -> Response {
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let redirect_uris: Vec<String> = parsed
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if redirect_uris.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_redirect_uri"})),
        )
            .into_response();
    }
    let name = parsed
        .get("client_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("MCP client {}", Utc::now().timestamp()));
    // "none" => public client (PKCE, no secret). Anything else => confidential.
    let auth_method = parsed
        .get("token_endpoint_auth_method")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    let confidential = auth_method != "none";

    let client_id = format!("amc_{}", random_token());
    let (secret_plain, secret_hash) = if confidential {
        let s = random_token();
        (Some(s.clone()), Some(auth::hash_token(&s)))
    } else {
        (None, None)
    };

    let res = sqlx::query(
        "INSERT INTO oauth_clients (client_id, client_secret_hash, client_name, redirect_uris, confidential)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&client_id)
    .bind(&secret_hash)
    .bind(&name)
    .bind(&redirect_uris)
    .bind(confidential)
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        tracing::error!("oauth register failed: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "server_error"})),
        )
            .into_response();
    }

    let mut body = json!({
        "client_id": client_id,
        "redirect_uris": redirect_uris,
        "token_endpoint_auth_method": if confidential { "client_secret_basic" } else { "none" },
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
    });
    if let Some(s) = secret_plain {
        body["client_secret"] = json!(s);
    }
    (StatusCode::CREATED, Json(body)).into_response()
}

// ── authorize (login + consent) ───────────────────────────────────────────────

#[derive(Deserialize)]
struct AuthorizeQuery {
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    scope: String,
}

async fn authorize(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    // PKCE S256 is mandatory.
    if q.response_type != "code" {
        return bad_request("response_type must be 'code'");
    }
    if q.code_challenge.is_empty() || q.code_challenge_method.to_uppercase() != "S256" {
        return bad_request("PKCE required: code_challenge with code_challenge_method=S256");
    }
    // Validate client + redirect_uri (exact match) BEFORE anything else.
    let client = match load_client(&state, &q.client_id).await {
        Some(c) => c,
        None => return bad_request("unknown client_id"),
    };
    if !client.redirect_uris.iter().any(|u| u == &q.redirect_uri) {
        return bad_request("redirect_uri not registered for this client");
    }

    // Require an AMOS login session; else send to login and come back.
    let claims = match session_claims(&state, &headers) {
        Some(c) => c,
        None => {
            let here = format!(
                "/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&state={}&code_challenge={}&code_challenge_method=S256&scope={}",
                urlencoding::encode(&q.client_id),
                urlencoding::encode(&q.redirect_uri),
                urlencoding::encode(&q.state),
                urlencoding::encode(&q.code_challenge),
                urlencoding::encode(&q.scope),
            );
            return axum::response::Redirect::to(&format!(
                "/login?redirect={}",
                urlencoding::encode(&here)
            ))
            .into_response();
        }
    };

    // Consent screen — pick which scopes to grant (role-capped).
    Html(consent_html(&q, &claims)).into_response()
}

#[derive(Deserialize)]
struct ConsentForm {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    #[serde(default)]
    scopes: String, // CSV of checked scopes
}

async fn authorize_consent(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Form(f): Form<ConsentForm>,
) -> Response {
    let claims = match session_claims(&state, &headers) {
        Some(c) => c,
        None => return (StatusCode::UNAUTHORIZED, "login required").into_response(),
    };
    let client = match load_client(&state, &f.client_id).await {
        Some(c) => c,
        None => return bad_request("unknown client_id"),
    };
    if !client.redirect_uris.iter().any(|u| u == &f.redirect_uri) {
        return bad_request("redirect_uri not registered for this client");
    }
    if f.code_challenge.is_empty() {
        return bad_request("missing code_challenge");
    }
    let requested: Vec<String> = f
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let granted = cap_scopes(&requested, &claims.role);

    let tenant_id = Uuid::parse_str(&claims.tenant_id).unwrap_or_default();
    let user_id = Uuid::parse_str(&claims.sub).unwrap_or_default();
    let code = random_token();
    let code_hash = auth::hash_token(&code);
    let expires = Utc::now() + Duration::seconds(CODE_TTL_SECS);

    let res = sqlx::query(
        "INSERT INTO oauth_auth_codes (code_hash, client_id, redirect_uri, code_challenge, scopes, user_id, tenant_id, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(&code_hash)
    .bind(&f.client_id)
    .bind(&f.redirect_uri)
    .bind(&f.code_challenge)
    .bind(&granted)
    .bind(user_id)
    .bind(tenant_id)
    .bind(expires)
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        tracing::error!("oauth code issue failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "server_error").into_response();
    }

    // Redirect back to the client with code + state.
    let sep = if f.redirect_uri.contains('?') { '&' } else { '?' };
    let loc = format!(
        "{}{}code={}&state={}",
        f.redirect_uri,
        sep,
        urlencoding::encode(&code),
        urlencoding::encode(&f.state)
    );
    axum::response::Redirect::to(&loc).into_response()
}

// ── token endpoint ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    refresh_token: String,
}

async fn token(State(state): State<PlatformState>, Form(f): Form<TokenForm>) -> Response {
    match f.grant_type.as_str() {
        "authorization_code" => token_from_code(&state, &f).await,
        "refresh_token" => token_from_refresh(&state, &f).await,
        other => token_err("unsupported_grant_type", &format!("grant_type '{other}'")),
    }
}

async fn token_from_code(state: &PlatformState, f: &TokenForm) -> Response {
    let code_hash = auth::hash_token(&f.code);
    // Fetch + validate the code (unconsumed, unexpired).
    let row = sqlx::query_as::<_, (String, String, Vec<String>, Uuid, Uuid, bool, chrono::DateTime<Utc>)>(
        "SELECT redirect_uri, code_challenge, scopes, user_id, tenant_id, consumed, expires_at
         FROM oauth_auth_codes WHERE code_hash = $1",
    )
    .bind(&code_hash)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let (redirect_uri, challenge, scopes, user_id, tenant_id, consumed, expires) = match row {
        Some(r) => r,
        None => return token_err("invalid_grant", "unknown or expired code"),
    };
    if consumed || expires < Utc::now() {
        return token_err("invalid_grant", "code expired or already used");
    }
    if redirect_uri != f.redirect_uri {
        return token_err("invalid_grant", "redirect_uri mismatch");
    }
    if !pkce_s256_verify(&f.code_verifier, &challenge) {
        return token_err("invalid_grant", "PKCE verification failed");
    }
    // Single-use: consume immediately.
    let _ = sqlx::query("UPDATE oauth_auth_codes SET consumed = TRUE WHERE code_hash = $1")
        .bind(&code_hash)
        .execute(&state.db)
        .await;

    issue_tokens(state, &f.client_id, user_id, tenant_id, &scopes).await
}

async fn token_from_refresh(state: &PlatformState, f: &TokenForm) -> Response {
    let rt_hash = auth::hash_token(&f.refresh_token);
    let row = sqlx::query_as::<_, (Uuid, String, Uuid, Uuid, Vec<String>)>(
        "SELECT id, client_id, user_id, tenant_id, scopes FROM oauth_tokens
         WHERE refresh_token_hash = $1 AND revoked = FALSE
         AND (refresh_expires_at IS NULL OR refresh_expires_at > NOW())",
    )
    .bind(&rt_hash)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let (old_id, client_id, user_id, tenant_id, scopes) = match row {
        Some(r) => r,
        None => return token_err("invalid_grant", "unknown or expired refresh_token"),
    };
    // Rotate: revoke the old token row, issue a fresh pair.
    let _ = sqlx::query("UPDATE oauth_tokens SET revoked = TRUE WHERE id = $1")
        .bind(old_id)
        .execute(&state.db)
        .await;
    issue_tokens(state, &client_id, user_id, tenant_id, &scopes).await
}

async fn issue_tokens(
    state: &PlatformState,
    client_id: &str,
    user_id: Uuid,
    tenant_id: Uuid,
    scopes: &[String],
) -> Response {
    let access = random_token();
    let refresh = random_token();
    let access_exp = Utc::now() + Duration::seconds(ACCESS_TTL_SECS);
    let refresh_exp = Utc::now() + Duration::seconds(REFRESH_TTL_SECS);
    let res = sqlx::query(
        "INSERT INTO oauth_tokens (access_token_hash, refresh_token_hash, client_id, user_id, tenant_id, scopes, access_expires_at, refresh_expires_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(auth::hash_token(&access))
    .bind(auth::hash_token(&refresh))
    .bind(client_id)
    .bind(user_id)
    .bind(tenant_id)
    .bind(scopes)
    .bind(access_exp)
    .bind(refresh_exp)
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        tracing::error!("oauth token issue failed: {e}");
        return token_err("server_error", "could not issue token");
    }
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS,
            "refresh_token": refresh,
            "scope": scopes.join(" "),
        })),
    )
        .into_response()
}

// ── MCP-side: resolve an OAuth access token → Claims (called from mcp::authenticate) ──

/// Resolve an OAuth access token to Claims (tenant + role-capped scopes), or
/// None. Mirrors the api_key path. Updates last_used_at on success.
pub(crate) async fn claims_for_access_token(
    state: &PlatformState,
    token: &str,
) -> Option<Claims> {
    let hash = auth::hash_token(token);
    let (user_id, tenant_id, scopes) = sqlx::query_as::<_, (Uuid, Uuid, Vec<String>)>(
        "SELECT user_id, tenant_id, scopes FROM oauth_tokens
         WHERE access_token_hash = $1 AND revoked = FALSE AND access_expires_at > NOW()",
    )
    .bind(&hash)
    .fetch_optional(&state.db)
    .await
    .ok()??;

    let (role, tenant_slug) = sqlx::query_as::<_, (String, String)>(
        "SELECT u.role, t.slug FROM users u JOIN tenants t ON u.tenant_id = t.id WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .ok()??;

    let db = state.db.clone();
    let h = hash.clone();
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE oauth_tokens SET last_used_at = NOW() WHERE access_token_hash = $1")
            .bind(&h)
            .execute(&db)
            .await;
    });

    Some(Claims {
        sub: user_id.to_string(),
        tenant_id: tenant_id.to_string(),
        role,
        tenant_slug,
        iat: Utc::now().timestamp(),
        exp: Utc::now().timestamp() + ACCESS_TTL_SECS,
        scopes: Some(scopes),
    })
}

// ── small helpers ────────────────────────────────────────────────────────────

struct ClientRow {
    redirect_uris: Vec<String>,
}

async fn load_client(state: &PlatformState, client_id: &str) -> Option<ClientRow> {
    let (redirect_uris,) =
        sqlx::query_as::<_, (Vec<String>,)>("SELECT redirect_uris FROM oauth_clients WHERE client_id = $1")
            .bind(client_id)
            .fetch_optional(&state.db)
            .await
            .ok()??;
    Some(ClientRow { redirect_uris })
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_request", "error_description": msg})))
        .into_response()
}

fn token_err(code: &str, desc: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error": code, "error_description": desc})),
    )
        .into_response()
}

/// The consent screen — scope picker, role-capped, brand-aligned.
fn consent_html(q: &AuthorizeQuery, claims: &Claims) -> String {
    use std::fmt::Write;
    let grantable = crate::rbac::scopes_for_role(&claims.role);
    let mut scopes: Vec<&str> = crate::rbac::scope::ALL
        .iter()
        .copied()
        .filter(|s| grantable.contains(s))
        .collect();
    scopes.sort();
    let mut boxes = String::new();
    for s in &scopes {
        let checked = if DEFAULT_SCOPES.contains(s) { "checked" } else { "" };
        let _ = write!(
            boxes,
            r#"<label style="display:flex;gap:8px;align-items:center;padding:6px 0"><input type="checkbox" class="sc" value="{s}" {checked}><code style="font-size:13px;color:#3f6fe6">{s}</code></label>"#,
        );
    }
    format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Connect to AMOS</title>
<body style="font-family:-apple-system,Segoe UI,Roboto,sans-serif;background:#f6f7f9;margin:0;padding:40px 16px;color:#1f2733">
<div style="max-width:460px;margin:0 auto;background:#fff;border:1px solid #e6e8ee;border-radius:14px;padding:26px">
  <div style="font-weight:800;letter-spacing:.04em;color:#0E1420">AMOS</div>
  <h2 style="margin:14px 0 4px;color:#0E1420">Authorize this AI client</h2>
  <p style="color:#69707d;font-size:14px;margin:0 0 18px">Signed in as <b>{email_sub}</b> ({role}). Choose exactly what this client may do in your environment — it can never exceed your role.</p>
  <form method="post" action="/oauth/authorize/consent" onsubmit="document.getElementById('scopes').value=Array.from(document.querySelectorAll('.sc:checked')).map(e=>e.value).join(',')">
    <input type="hidden" name="client_id" value="{cid}">
    <input type="hidden" name="redirect_uri" value="{ruri}">
    <input type="hidden" name="state" value="{state}">
    <input type="hidden" name="code_challenge" value="{chal}">
    <input type="hidden" id="scopes" name="scopes" value="">
    <div style="border-top:1px solid #e6e8ee;border-bottom:1px solid #e6e8ee;padding:10px 0;margin-bottom:18px">{boxes}</div>
    <button type="submit" style="width:100%;background:#0E1420;color:#fff;border:0;border-radius:9px;padding:11px;font-size:15px;font-weight:600;cursor:pointer">Authorize</button>
  </form>
</div></body>"#,
        email_sub = html_escape(&claims.sub),
        role = html_escape(&claims.role),
        cid = html_escape(&q.client_id),
        ruri = html_escape(&q.redirect_uri),
        state = html_escape(&q.state),
        chal = html_escape(&q.code_challenge),
        boxes = boxes,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_roundtrip() {
        // verifier → S256 challenge (RFC 7636 example-style)
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = sha256_b64url(verifier.as_bytes());
        assert!(pkce_s256_verify(verifier, &challenge));
        assert!(!pkce_s256_verify("wrong-verifier", &challenge));
        assert!(!pkce_s256_verify("", &challenge)); // empty never verifies
    }

    #[test]
    fn scopes_capped_to_role_and_known() {
        // member can't escalate to billing:manage; unknown scopes dropped.
        let got = cap_scopes(
            &[
                "app:deploy".into(),
                "billing:manage".into(),
                "totally:made-up".into(),
            ],
            "member",
        );
        assert!(got.contains(&"app:deploy".to_string()));
        assert!(!got.iter().any(|s| s == "billing:manage"));
        assert!(!got.iter().any(|s| s == "totally:made-up"));
    }

    #[test]
    fn empty_request_falls_back_to_readonly_default() {
        let got = cap_scopes(&[], "owner");
        assert!(got.contains(&"app:read".to_string()));
        assert!(!got.iter().any(|s| s == "app:deploy"));
    }

    #[test]
    fn metadata_scope_list_is_the_rbac_catalog() {
        // The advertised scopes_supported must reflect the real catalog.
        assert!(crate::rbac::scope::ALL.contains(&"finance:read"));
    }
}
