//! Release registration and listing API.
//!
//! CI registers new releases after building images. The dashboard and sync
//! endpoints read from the `releases` table to determine what's available.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::state::PlatformState;

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/releases", get(list_releases).post(register_release))
        .route("/releases/latest", get(latest_release))
}

// ── Register Release (called by CI) ────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RegisterReleaseRequest {
    version: String,
    commit_sha: String,
    harness_image: String,
    agent_image: Option<String>,
    release_notes: Option<String>,
}


/// `POST /releases` — Register a new release. Authenticated via `RELEASE_API_KEY` bearer token.
async fn register_release(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Json(body): Json<RegisterReleaseRequest>,
) -> impl IntoResponse {
    // Authenticate with RELEASE_API_KEY env var
    let expected_key = match std::env::var("RELEASE_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            warn!("RELEASE_API_KEY not configured — rejecting release registration");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Release registration not configured"})),
            )
                .into_response();
        }
    };

    let provided_key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if provided_key != expected_key {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid API key"})),
        )
            .into_response();
    }

    // Insert the release
    let result = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        INSERT INTO releases (version, commit_sha, harness_image, agent_image, release_notes)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (version) DO UPDATE SET
            harness_image = EXCLUDED.harness_image,
            agent_image = EXCLUDED.agent_image,
            release_notes = EXCLUDED.release_notes
        RETURNING id
        "#,
    )
    .bind(&body.version)
    .bind(&body.commit_sha)
    .bind(&body.harness_image)
    .bind(&body.agent_image)
    .bind(&body.release_notes)
    .fetch_one(&state.db)
    .await;

    match result {
        Ok(id) => {
            info!(version = %body.version, "Release registered");
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": id.to_string(),
                    "version": body.version,
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!("Failed to register release: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to register release"})),
            )
                .into_response()
        }
    }
}

// ── List Releases ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct ReleaseInfo {
    id: String,
    version: String,
    commit_sha: String,
    status: String,
    release_notes: Option<String>,
    created_at: String,
}

/// `GET /releases` — List recent available releases.
async fn list_releases(State(state): State<PlatformState>) -> impl IntoResponse {
    let rows = sqlx::query_as::<_, (uuid::Uuid, String, String, String, Option<String>, String)>(
        r#"
        SELECT id, version, commit_sha, status, release_notes, created_at::text
        FROM releases
        WHERE status = 'available'
        ORDER BY created_at DESC
        LIMIT 20
        "#,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let releases: Vec<ReleaseInfo> = rows
        .into_iter()
        .map(|(id, version, commit_sha, status, release_notes, created_at)| ReleaseInfo {
            id: id.to_string(),
            version,
            commit_sha,
            status,
            release_notes,
            created_at,
        })
        .collect();

    Json(serde_json::json!({ "releases": releases }))
}

// ── Latest Release ─────────────────────────────────────────────────────

/// `GET /releases/latest` — Get the latest available release version.
async fn latest_release(State(state): State<PlatformState>) -> impl IntoResponse {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT version, harness_image FROM releases WHERE status = 'available' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    match row {
        Some((version, harness_image)) => Json(serde_json::json!({
            "version": version,
            "harness_image": harness_image,
        }))
        .into_response(),
        None => Json(serde_json::json!({
            "version": null,
            "harness_image": null,
        }))
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_release_info_serialization() {
        let info = ReleaseInfo {
            id: "test-id".into(),
            version: "sha-abc123".into(),
            commit_sha: "abc123".into(),
            status: "available".into(),
            release_notes: Some("Test release".into()),
            created_at: "2026-03-27T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("sha-abc123"));
        assert!(json.contains("Test release"));
    }
}
