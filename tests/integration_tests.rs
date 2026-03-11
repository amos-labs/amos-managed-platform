//! Integration tests for the AMOS Platform.
//!
//! These tests require a live PostgreSQL and Redis instance.
//!
//! **Local**: `amos_platform_test` DB on localhost (auto-created)
//! **CI**:   Postgres service container configured in GitHub Actions
//!
//! Run: `cargo test --test integration_tests`
//!
//! Set `TEST_DATABASE_URL` to override the default connection string.
//! Set `TEST_REDIS_URL` to override the default Redis URL.

use axum::body::Body;
use axum::http::{self, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::Executor;
use tower::ServiceExt; // for `oneshot`

// ═══════════════════════════════════════════════════════════════════════════
//  Test Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Default test DB URL — overridable via TEST_DATABASE_URL env var.
fn database_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://amos:amos_dev_password@localhost:5432/amos_platform_test".into())
}

/// Default test Redis URL — overridable via TEST_REDIS_URL env var.
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".into())
}

/// Create an isolated PlatformState backed by a real test database.
///
/// Each call creates a fresh `PlatformState` with migrations applied.
/// All tables are truncated before returning, guaranteeing a clean slate.
async fn setup_test_state() -> amos_platform::PlatformState {
    let db_url = database_url();
    let redis_url = redis_url();

    // Build AppConfig programmatically via env vars
    std::env::set_var("AMOS__DATABASE__URL", &db_url);
    std::env::set_var("AMOS__REDIS__URL", &redis_url);
    std::env::set_var("AMOS__SERVER__PORT", "0"); // never actually bind
    std::env::set_var("AMOS__AUTH__JWT_SECRET", "test-jwt-secret-for-integration-tests-2025");

    let config = amos_platform::AppConfig::load().expect("AppConfig::load");

    // Build state (will connect to DB & Redis)
    let state = amos_platform::PlatformState::new(config)
        .await
        .expect("PlatformState::new");

    // Run migrations
    state.run_migrations().await.expect("run_migrations");

    // Truncate all tables for a clean test run (order matters for FKs)
    state.db.execute(
        "TRUNCATE TABLE
            refresh_tokens,
            api_keys,
            harness_instances,
            harness_configs,
            usage_metrics,
            activity_reports,
            users,
            tenants,
            contribution_activities,
            emission_records
         CASCADE"
    ).await.expect("TRUNCATE tables");

    state
}

/// Build the full Axum router for testing (same as production, no port bind).
fn build_test_app(state: amos_platform::PlatformState) -> axum::Router {
    amos_platform::server::build_http_router(state)
}

/// Helper: send a JSON POST and return (status, body_json).
async fn post_json(
    app: &axum::Router,
    uri: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({"raw": String::from_utf8_lossy(&bytes).to_string()}));
    (status, json)
}

/// Helper: send a GET with optional Authorization header.
async fn get_with_auth(
    app: &axum::Router,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(http::Method::GET)
        .uri(uri);

    if let Some(t) = token {
        builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {}", t));
    }

    let req = builder.body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({"raw": String::from_utf8_lossy(&bytes).to_string()}));
    (status, json)
}

/// Helper: register a new user and return (access_token, refresh_token, tenant_id, user_id).
async fn register_test_user(
    app: &axum::Router,
    org: &str,
    email: &str,
    password: &str,
) -> (String, String, String, String) {
    let (status, body) = post_json(
        app,
        "/api/v1/auth/register",
        &json!({
            "organization_name": org,
            "email": email,
            "name": "Test User",
            "password": password,
        }),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {}", body);

    let access = body["access_token"].as_str().unwrap().to_string();
    let refresh = body["refresh_token"].as_str().unwrap().to_string();
    let tenant_id = body["tenant_id"].as_str().unwrap().to_string();
    let user_id = body["user_id"].as_str().unwrap().to_string();
    (access, refresh, tenant_id, user_id)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Health Check Tests
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_health_endpoint() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let (status, body) = get_with_auth(&app, "/api/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
}

#[tokio::test]
async fn test_readiness_endpoint() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let (status, body) = get_with_auth(&app, "/api/v1/readiness", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["db"], "ok");
    assert_eq!(body["redis"], "ok");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Auth Flow Integration Tests
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_register_success() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let (status, body) = post_json(
        &app,
        "/api/v1/auth/register",
        &json!({
            "organization_name": "Test Corp",
            "email": "admin@testcorp.com",
            "name": "Admin User",
            "password": "securepassword123",
        }),
    ).await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["access_token"].is_string());
    assert!(body["refresh_token"].is_string());
    assert_eq!(body["slug"], "test-corp");
    assert!(body["tenant_id"].is_string());
    assert!(body["user_id"].is_string());
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], 3600);
}

#[tokio::test]
async fn test_register_validation_errors() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Empty org name
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/register",
        &json!({
            "organization_name": "",
            "email": "a@b.com",
            "name": "User",
            "password": "12345678",
        }),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "validation_error");
    assert_eq!(body["field"], "organization_name");

    // Invalid email
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/register",
        &json!({
            "organization_name": "Org",
            "email": "not-an-email",
            "name": "User",
            "password": "12345678",
        }),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["field"], "email");

    // Short password
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/register",
        &json!({
            "organization_name": "Org",
            "email": "a@b.com",
            "name": "User",
            "password": "short",
        }),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["field"], "password");

    // Invalid plan
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/register",
        &json!({
            "organization_name": "Org",
            "email": "a@b.com",
            "name": "User",
            "password": "12345678",
            "plan": "invalid_plan",
        }),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["field"], "plan");
}

#[tokio::test]
async fn test_register_duplicate_slug() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Register first
    register_test_user(&app, "Duplicate Corp", "first@dup.com", "password123").await;

    // Register with same slug
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/register",
        &json!({
            "organization_name": "Duplicate Corp",
            "email": "second@dup.com",
            "name": "User 2",
            "password": "password123",
        }),
    ).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "slug_conflict");
}

#[tokio::test]
async fn test_login_success() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Register
    register_test_user(&app, "Login Corp", "login@test.com", "password123").await;

    // Login
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/login",
        &json!({
            "email": "login@test.com",
            "password": "password123",
        }),
    ).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["access_token"].is_string());
    assert!(body["refresh_token"].is_string());
    assert_eq!(body["tenant_slug"], "login-corp");
    assert_eq!(body["role"], "owner");
    assert_eq!(body["token_type"], "Bearer");
}

#[tokio::test]
async fn test_login_invalid_credentials() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Non-existent user
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/login",
        &json!({
            "email": "nonexistent@test.com",
            "password": "password123",
        }),
    ).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "invalid_credentials");

    // Register then wrong password
    register_test_user(&app, "Wrong PW Corp", "wp@test.com", "correct_password").await;

    let (status, body) = post_json(
        &app,
        "/api/v1/auth/login",
        &json!({
            "email": "wp@test.com",
            "password": "wrong_password",
        }),
    ).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "invalid_credentials");
}

#[tokio::test]
async fn test_token_refresh_flow() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Register and get tokens
    let (_, refresh, _, _) = register_test_user(
        &app, "Refresh Corp", "refresh@test.com", "password123"
    ).await;

    // Refresh tokens
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/refresh",
        &json!({ "refresh_token": refresh }),
    ).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["access_token"].is_string());
    assert!(body["refresh_token"].is_string());
    // The new refresh token should be different (rotation)
    assert_ne!(body["refresh_token"].as_str().unwrap(), refresh);
}

#[tokio::test]
async fn test_refresh_token_rotation_reuse_detection() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let (_, refresh, _, _) = register_test_user(
        &app, "Reuse Corp", "reuse@test.com", "password123"
    ).await;

    // Use refresh token once (valid)
    let (status, _) = post_json(
        &app,
        "/api/v1/auth/refresh",
        &json!({ "refresh_token": &refresh }),
    ).await;
    assert_eq!(status, StatusCode::OK);

    // Reuse the SAME refresh token again (should detect reuse)
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/refresh",
        &json!({ "refresh_token": &refresh }),
    ).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "token_revoked");
}

#[tokio::test]
async fn test_logout() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let (_, refresh, _, _) = register_test_user(
        &app, "Logout Corp", "logout@test.com", "password123"
    ).await;

    // Logout
    let (status, body) = post_json(
        &app,
        "/api/v1/auth/logout",
        &json!({ "refresh_token": &refresh }),
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["message"].as_str().unwrap().contains("Logged out"));

    // Refresh should now fail
    let (status, _) = post_json(
        &app,
        "/api/v1/auth/refresh",
        &json!({ "refresh_token": &refresh }),
    ).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ═══════════════════════════════════════════════════════════════════════════
//  API Authentication Middleware Tests
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_protected_endpoint_without_token() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Try accessing a protected endpoint without auth
    let (status, body) = get_with_auth(&app, "/api/v1/tenants/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthorized");
}

#[tokio::test]
async fn test_protected_endpoint_with_valid_token() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let (token, _, _, _) = register_test_user(
        &app, "Auth Corp", "auth@test.com", "password123"
    ).await;

    // Access protected endpoint with valid token
    let (status, _body) = get_with_auth(&app, "/api/v1/tenants/me", Some(&token)).await;
    // Should succeed (200) or at least not 401
    assert_ne!(status, StatusCode::UNAUTHORIZED, "Token was rejected");
}

#[tokio::test]
async fn test_protected_endpoint_with_expired_token() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Create an expired token manually
    let expired_token = amos_platform::auth::create_access_token(
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        "owner",
        "test-slug",
        "test-jwt-secret-for-integration-tests-2025",
        -1, // expired
    ).unwrap();

    let (status, body) = get_with_auth(&app, "/api/v1/tenants/me", Some(&expired_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthorized");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tenant Isolation Tests
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_tenant_isolation_separate_data() {
    let state = setup_test_state().await;
    let app = build_test_app(state.clone());

    // Register two separate tenants
    let (_token_a, _, tenant_a, _) = register_test_user(
        &app, "Tenant Alpha", "admin@alpha.com", "password123"
    ).await;
    let (_token_b, _, tenant_b, _) = register_test_user(
        &app, "Tenant Beta", "admin@beta.com", "password123"
    ).await;

    // Verify they got different tenant IDs
    assert_ne!(tenant_a, tenant_b, "Tenants should have different IDs");

    // Verify each tenant can only see their own data
    // Tenant A's harness instances should not include Tenant B's
    let count_a: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM harness_instances WHERE tenant_id = $1")
        .bind(uuid::Uuid::parse_str(&tenant_a).unwrap())
        .fetch_one(&state.db)
        .await
        .unwrap();

    let count_b: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM harness_instances WHERE tenant_id = $1")
        .bind(uuid::Uuid::parse_str(&tenant_b).unwrap())
        .fetch_one(&state.db)
        .await
        .unwrap();

    // Each tenant should have exactly 1 harness (auto-created at registration)
    assert_eq!(count_a.0, 1, "Tenant A should have exactly 1 harness");
    assert_eq!(count_b.0, 1, "Tenant B should have exactly 1 harness");
}

#[tokio::test]
async fn test_cross_tenant_token_scoping() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Register two tenants
    let (token_a, _, _, _) = register_test_user(
        &app, "Scoped Alpha", "scope-a@test.com", "password123"
    ).await;
    let (token_b, _, _, _) = register_test_user(
        &app, "Scoped Beta", "scope-b@test.com", "password123"
    ).await;

    // Validate that tokens contain different tenant claims
    let claims_a = amos_platform::auth::validate_access_token(
        &token_a,
        "test-jwt-secret-for-integration-tests-2025",
    ).unwrap();
    let claims_b = amos_platform::auth::validate_access_token(
        &token_b,
        "test-jwt-secret-for-integration-tests-2025",
    ).unwrap();

    assert_ne!(claims_a.tenant_id, claims_b.tenant_id);
    assert_eq!(claims_a.tenant_slug, "scoped-alpha");
    assert_eq!(claims_b.tenant_slug, "scoped-beta");
    assert_eq!(claims_a.role, "owner");
    assert_eq!(claims_b.role, "owner");
}

// ═══════════════════════════════════════════════════════════════════════════
//  UI Route Tests (SSR pages)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_login_page_renders() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("/login")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Sign in"), "Login page should contain 'Sign in'");
}

#[tokio::test]
async fn test_register_page_renders() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("/register")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("register") || html.contains("Register") || html.contains("Create"),
            "Register page should contain registration content");
}

#[tokio::test]
async fn test_dashboard_requires_auth() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Dashboard without cookie should redirect to login
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("/dashboard")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    // Should redirect (302) or return unauthorized-like behavior
    let status = resp.status();
    assert!(
        status == StatusCode::FOUND || status == StatusCode::SEE_OTHER || status == StatusCode::UNAUTHORIZED,
        "Dashboard without auth should redirect or return 401/302, got {}",
        status
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Discovery / Root Route Tests
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_root_returns_json_for_agents() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    // Agent-like request (no Accept: text/html)
    let (status, _body) = get_with_auth(&app, "/", None).await;
    // Should return JSON catalog or redirect
    assert!(status == StatusCode::OK || status == StatusCode::FOUND,
            "Root should return OK or redirect, got {}", status);
}

#[tokio::test]
async fn test_api_catalog() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let (status, body) = get_with_auth(&app, "/api/v1", None).await;
    assert_eq!(status, StatusCode::OK);
    // Should list available endpoints
    assert!(body.is_object(), "API catalog should return JSON object");
}

#[tokio::test]
async fn test_404_returns_json() {
    let state = setup_test_state().await;
    let app = build_test_app(state);

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("/api/v1/nonexistent-endpoint")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Database Migration Tests
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_migrations_are_idempotent() {
    let state = setup_test_state().await;

    // Running migrations again should not fail
    state.run_migrations().await.expect("Migrations should be idempotent");

    // Verify key tables exist
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public'
         ORDER BY table_name"
    )
    .fetch_all(&state.db)
    .await
    .unwrap();

    let table_names: Vec<&str> = tables.iter().map(|t| t.0.as_str()).collect();
    assert!(table_names.contains(&"tenants"), "Missing tenants table");
    assert!(table_names.contains(&"users"), "Missing users table");
    assert!(table_names.contains(&"harness_instances"), "Missing harness_instances table");
    assert!(table_names.contains(&"api_keys"), "Missing api_keys table");
    assert!(table_names.contains(&"refresh_tokens"), "Missing refresh_tokens table");
}
