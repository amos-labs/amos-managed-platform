//! Sync API endpoints for harness↔platform communication.
//!
//! These endpoints are called by the PlatformSyncClient running inside each
//! harness container (both managed and self-hosted deployments).

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::state::PlatformState;

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/sync/heartbeat", post(receive_heartbeat))
        .route("/sync/config", get(get_config))
        .route("/sync/activity", post(receive_activity))
        .route("/sync/version", get(get_latest_version))
        .route("/sync/siblings", get(get_siblings))
        .route("/sync/update-self", post(update_self))
}

// ── Self-update ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct UpdateSelfPayload {
    /// UUID of the harness instance requesting the update.
    harness_id: String,
}

#[derive(Serialize)]
struct UpdateSelfResponse {
    success: bool,
    new_version: Option<String>,
    error: Option<String>,
}

/// `POST /api/v1/sync/update-self` — the harness's own backend calls this
/// when the user clicks the in-harness update banner. Saves the customer
/// the round-trip to the platform dashboard.
///
/// Auth: the calling harness's current container_id must match the
/// container_id recorded for that harness_id. That's a weak check, but
/// the 169.254.170.2 metadata endpoint + the harness's own ECS task role
/// are already well-isolated per customer. Full mTLS / signed tokens
/// between harness and platform are a follow-up.
async fn update_self(
    State(state): State<PlatformState>,
    Json(payload): Json<UpdateSelfPayload>,
) -> impl IntoResponse {
    let harness_id = match uuid::Uuid::parse_str(&payload.harness_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(UpdateSelfResponse {
                    success: false,
                    new_version: None,
                    error: Some("Invalid harness_id UUID".to_string()),
                }),
            );
        }
    };

    let container_id: Option<String> =
        sqlx::query_scalar("SELECT container_id FROM harness_instances WHERE id = $1")
            .bind(harness_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    match crate::routes::ui::perform_harness_update(&state, harness_id, container_id.as_deref())
        .await
    {
        Ok(new_version) => {
            info!(
                harness_id = %harness_id,
                new_version = %new_version,
                "Harness self-update kicked off from in-harness banner"
            );
            (
                StatusCode::OK,
                Json(UpdateSelfResponse {
                    success: true,
                    new_version: Some(new_version),
                    error: None,
                }),
            )
        }
        Err(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(UpdateSelfResponse {
                success: false,
                new_version: None,
                error: Some(msg),
            }),
        ),
    }
}

// ── Heartbeat ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct HeartbeatPayload {
    harness_version: String,
    deployment_mode: String,
    uptime_secs: u64,
    healthy: bool,
    timestamp: String,
    /// Optional tenant_id for identifying which harness is sending
    tenant_id: Option<String>,
    /// Optional harness_id for per-harness heartbeat (multi-harness mode)
    harness_id: Option<String>,
    /// Harness role: primary, specialist, or worker
    harness_role: Option<String>,
    /// Enabled packages on this harness
    packages: Option<Vec<String>>,
}

#[derive(Serialize)]
struct HeartbeatResponse {
    acknowledged: bool,
    server_time: DateTime<Utc>,
}

async fn receive_heartbeat(
    State(state): State<PlatformState>,
    Json(payload): Json<HeartbeatPayload>,
) -> impl IntoResponse {
    debug!(
        "Heartbeat received: version={}, mode={}, uptime={}s, healthy={}, tenant_id={:?}",
        payload.harness_version,
        payload.deployment_mode,
        payload.uptime_secs,
        payload.healthy,
        payload.tenant_id,
    );

    // Per-harness heartbeat: if harness_id is provided, update only that instance.
    // Fallback: update by tenant_id for backward compatibility.
    if let Some(harness_id_str) = &payload.harness_id {
        if let Ok(harness_id) = uuid::Uuid::parse_str(harness_id_str) {
            let result = sqlx::query(
                r#"
                UPDATE harness_instances
                SET last_heartbeat = NOW(),
                    harness_version = $1,
                    healthy = $2
                WHERE id = $3 AND status != 'deprovisioned'
                "#,
            )
            .bind(&payload.harness_version)
            .bind(payload.healthy)
            .bind(harness_id)
            .execute(&state.db)
            .await;

            match result {
                Ok(result) => {
                    if result.rows_affected() > 0 {
                        debug!("Updated harness status for harness {}", harness_id);
                    } else {
                        debug!(
                            "No harness instance found for id {} (may not be provisioned yet)",
                            harness_id
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to update harness status in database: {}", e);
                }
            }
        } else {
            debug!("Invalid harness_id format: {}", harness_id_str);
        }
    } else if let Some(tenant_id_str) = &payload.tenant_id {
        // Backward compatible: update all harnesses for this tenant
        if let Ok(tenant_id) = uuid::Uuid::parse_str(tenant_id_str) {
            let result = sqlx::query(
                r#"
                UPDATE harness_instances
                SET last_heartbeat = NOW(),
                    harness_version = $1,
                    healthy = $2
                WHERE tenant_id = $3 AND status != 'deprovisioned'
                "#,
            )
            .bind(&payload.harness_version)
            .bind(payload.healthy)
            .bind(tenant_id)
            .execute(&state.db)
            .await;

            match result {
                Ok(result) => {
                    if result.rows_affected() > 0 {
                        debug!("Updated harness status for tenant {}", tenant_id);
                    } else {
                        debug!(
                            "No harness instance found for tenant {} (may not be provisioned yet)",
                            tenant_id
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to update harness status in database: {}", e);
                }
            }
        } else {
            debug!("Invalid tenant_id format: {}", tenant_id_str);
        }
    } else {
        debug!("No tenant_id or harness_id provided in heartbeat (backwards compatibility mode)");
    }

    Json(HeartbeatResponse {
        acknowledged: true,
        server_time: Utc::now(),
    })
}

// ── Config Distribution ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ConfigQuery {
    version: Option<String>,
    tenant_id: Option<String>,
}

#[derive(Serialize)]
struct RemoteConfig {
    /// Latest available harness version.
    latest_version: Option<String>,
    /// Whether this harness instance is enabled.
    enabled: bool,
    /// Model overrides (empty for now).
    model_overrides: Vec<ModelOverride>,
    /// Feature flags.
    feature_flags: std::collections::HashMap<String, bool>,
    /// Sync timestamp.
    synced_at: String,
}

#[derive(Serialize)]
struct ModelOverride {
    name: String,
    model_id: String,
    tier: u8,
}

async fn get_config(
    State(state): State<PlatformState>,
    axum::extract::Query(query): axum::extract::Query<ConfigQuery>,
) -> impl IntoResponse {
    debug!(
        "Config request: version={:?}, tenant_id={:?}",
        query.version, query.tenant_id
    );

    // Default config values
    let mut enabled = true;
    let mut feature_flags = std::collections::HashMap::new();
    feature_flags.insert("sovereign_ai".to_string(), true);
    feature_flags.insert("custom_models".to_string(), true);

    // Try to look up harness-specific config from database
    if let Some(tenant_id_str) = &query.tenant_id {
        if let Ok(tenant_id) = uuid::Uuid::parse_str(tenant_id_str) {
            let result = sqlx::query_as::<_, (bool, serde_json::Value)>(
                r#"
                SELECT enabled, feature_flags
                FROM harness_configs
                WHERE tenant_id = $1
                "#,
            )
            .bind(tenant_id)
            .fetch_optional(&state.db)
            .await;

            match result {
                Ok(Some((db_enabled, db_flags))) => {
                    enabled = db_enabled;
                    // Merge database feature flags with defaults
                    if let Some(flags_obj) = db_flags.as_object() {
                        for (key, value) in flags_obj {
                            if let Some(bool_val) = value.as_bool() {
                                feature_flags.insert(key.clone(), bool_val);
                            }
                        }
                    }
                    debug!("Loaded config from database for tenant {}", tenant_id);
                }
                Ok(None) => {
                    debug!(
                        "No config found in database for tenant {}, using defaults",
                        tenant_id
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to fetch config from database: {}, using defaults",
                        e
                    );
                }
            }
        }
    }

    // Look up the latest available release from the database; fall back to compile-time version.
    let latest_version: Option<String> = sqlx::query_scalar(
        "SELECT version FROM releases WHERE status = 'available' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .or_else(|| Some(env!("CARGO_PKG_VERSION").to_string()));

    Json(RemoteConfig {
        latest_version,
        enabled,
        model_overrides: vec![],
        feature_flags,
        synced_at: Utc::now().to_rfc3339(),
    })
}

// ── Activity Ingest ─────────────────────────────────────────────────────

/// Per-model token usage entry from harness activity reports.
#[derive(Debug, Deserialize)]
struct ModelUsageEntry {
    model_id: String,
    tokens_input: u64,
    tokens_output: u64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ActivityReport {
    period_start: String,
    period_end: String,
    conversations: u64,
    messages: u64,
    tokens_input: u64,
    tokens_output: u64,
    tools_executed: u64,
    models_used: Vec<String>,
    /// Per-model token breakdown for metered billing.
    #[serde(default)]
    model_usage: Vec<ModelUsageEntry>,
    timestamp: String,
    /// Optional tenant_id for identifying which harness is reporting
    tenant_id: Option<String>,
    /// Optional harness_id for per-harness billing
    harness_id: Option<String>,
}

#[derive(Serialize)]
struct ActivityResponse {
    accepted: bool,
    server_time: DateTime<Utc>,
}

async fn receive_activity(
    State(state): State<PlatformState>,
    Json(report): Json<ActivityReport>,
) -> impl IntoResponse {
    info!(
        "Activity report: {} convs, {} msgs, {} input tokens, {} output tokens over {}..{}, tenant_id={:?}",
        report.conversations, report.messages,
        report.tokens_input, report.tokens_output,
        report.period_start, report.period_end, report.tenant_id,
    );

    // Store activity in database if tenant_id is provided
    if let Some(tenant_id_str) = &report.tenant_id {
        if let Ok(tenant_id) = uuid::Uuid::parse_str(tenant_id_str) {
            // Parse period timestamps
            let period_start_result = chrono::DateTime::parse_from_rfc3339(&report.period_start);
            let period_end_result = chrono::DateTime::parse_from_rfc3339(&report.period_end);

            if let (Ok(period_start), Ok(period_end)) = (period_start_result, period_end_result) {
                let period_start_utc = period_start.with_timezone(&Utc);
                let period_end_utc = period_end.with_timezone(&Utc);

                // Insert activity report
                let insert_result = sqlx::query(
                    r#"
                    INSERT INTO activity_reports
                    (tenant_id, period_start, period_end, conversations, messages,
                     tokens_input, tokens_output, tools_executed, models_used, received_at)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
                    "#,
                )
                .bind(tenant_id)
                .bind(period_start_utc)
                .bind(period_end_utc)
                .bind(report.conversations as i64)
                .bind(report.messages as i64)
                .bind(report.tokens_input as i64)
                .bind(report.tokens_output as i64)
                .bind(report.tools_executed as i64)
                .bind(&report.models_used)
                .execute(&state.db)
                .await;

                match insert_result {
                    Ok(_) => {
                        debug!("Stored activity report for tenant {}", tenant_id);

                        // Update usage metrics aggregates
                        let aggregate_result = sqlx::query(
                            r#"
                            INSERT INTO usage_metrics
                            (tenant_id, period_start, conversations, messages, tokens_input, tokens_output, tools_executed)
                            VALUES ($1, $2, $3, $4, $5, $6, $7)
                            ON CONFLICT (tenant_id, period_start)
                            DO UPDATE SET
                                conversations = usage_metrics.conversations + EXCLUDED.conversations,
                                messages = usage_metrics.messages + EXCLUDED.messages,
                                tokens_input = usage_metrics.tokens_input + EXCLUDED.tokens_input,
                                tokens_output = usage_metrics.tokens_output + EXCLUDED.tokens_output,
                                tools_executed = usage_metrics.tools_executed + EXCLUDED.tools_executed,
                                updated_at = NOW()
                            "#
                        )
                        .bind(tenant_id)
                        .bind(period_start_utc)
                        .bind(report.conversations as i64)
                        .bind(report.messages as i64)
                        .bind(report.tokens_input as i64)
                        .bind(report.tokens_output as i64)
                        .bind(report.tools_executed as i64)
                        .execute(&state.db)
                        .await;

                        match aggregate_result {
                            Ok(_) => {
                                debug!("Updated usage metrics for tenant {}", tenant_id);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to update usage metrics: {}", e);
                            }
                        }

                        // Insert per-model usage records for metered billing.
                        // Every token must be tracked — warn loudly if harness_id is missing.
                        if !report.model_usage.is_empty() {
                            let mut hid = report
                                .harness_id
                                .as_deref()
                                .and_then(|s| uuid::Uuid::parse_str(s).ok());

                            // Fallback: look up harness from tenant if not in report
                            if hid.is_none() {
                                hid = sqlx::query_scalar(
                                    "SELECT id FROM harness_instances WHERE tenant_id = $1 AND status != 'deprovisioned' LIMIT 1"
                                )
                                .bind(tenant_id)
                                .fetch_optional(&state.db)
                                .await
                                .ok()
                                .flatten();

                                if hid.is_some() {
                                    tracing::warn!(
                                        "Activity report missing harness_id, using fallback for tenant {}",
                                        tenant_id
                                    );
                                }
                            }

                            if let Some(hid) = hid {
                                for entry in &report.model_usage {
                                    let cost =
                                        crate::billing::metered_billing::calculate_cost_microcents(
                                            &entry.model_id,
                                            entry.tokens_input,
                                            entry.tokens_output,
                                        );
                                    let _ = sqlx::query(
                                        r#"
                                        INSERT INTO llm_usage_records
                                        (tenant_id, harness_id, model_id, tokens_input, tokens_output,
                                         cost_microcents, period_start, period_end)
                                        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                                        "#,
                                    )
                                    .bind(tenant_id)
                                    .bind(hid)
                                    .bind(&entry.model_id)
                                    .bind(entry.tokens_input as i64)
                                    .bind(entry.tokens_output as i64)
                                    .bind(cost)
                                    .bind(period_start_utc)
                                    .bind(period_end_utc)
                                    .execute(&state.db)
                                    .await
                                    .map_err(|e| {
                                        tracing::warn!(
                                            "Failed to insert llm_usage_record for {}: {}",
                                            entry.model_id, e
                                        );
                                    });
                                }
                            } else {
                                tracing::error!(
                                    "BILLING LEAK: {} model_usage entries from tenant {} dropped — no harness_id found",
                                    report.model_usage.len(), tenant_id
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to store activity report: {}", e);
                    }
                }
            } else {
                tracing::warn!("Invalid period timestamp format in activity report");
            }
        } else {
            debug!("Invalid tenant_id format: {}", tenant_id_str);
        }
    } else {
        debug!("No tenant_id provided in activity report (backwards compatibility mode)");
    }

    Json(ActivityResponse {
        accepted: true,
        server_time: Utc::now(),
    })
}

// ── Version Check ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct VersionInfo {
    latest_version: String,
    minimum_version: String,
    release_notes_url: Option<String>,
    update_required: bool,
}

async fn get_latest_version(State(state): State<PlatformState>) -> impl IntoResponse {
    let latest_version: String = sqlx::query_scalar(
        "SELECT version FROM releases WHERE status = 'available' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    Json(VersionInfo {
        latest_version,
        minimum_version: "0.1.0".to_string(),
        release_notes_url: None,
        update_required: false,
    })
}

// ── Siblings Discovery ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SiblingsQuery {
    tenant_id: String,
    /// The calling harness's ID (excluded from results)
    exclude_harness_id: Option<String>,
}

#[derive(Serialize)]
struct SiblingHarness {
    harness_id: String,
    name: Option<String>,
    harness_role: String,
    packages: serde_json::Value,
    internal_url: Option<String>,
    status: String,
    healthy: Option<bool>,
    last_heartbeat: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct SiblingsResponse {
    siblings: Vec<SiblingHarness>,
}

/// Returns all harness instances for a tenant, excluding the caller.
///
/// Used by the orchestrator module on the primary harness to discover
/// specialist and worker harnesses.
async fn get_siblings(
    State(state): State<PlatformState>,
    axum::extract::Query(params): axum::extract::Query<SiblingsQuery>,
) -> impl IntoResponse {
    let tenant_id = match uuid::Uuid::parse_str(&params.tenant_id) {
        Ok(id) => id,
        Err(_) => {
            return Json(SiblingsResponse { siblings: vec![] });
        }
    };

    let exclude_id = params
        .exclude_harness_id
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok());

    let query = if let Some(exclude) = exclude_id {
        sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                Option<String>,
                String,
                serde_json::Value,
                Option<String>,
                String,
                Option<bool>,
                Option<DateTime<Utc>>,
            ),
        >(
            r#"
            SELECT id, name, harness_role, packages, internal_url, status, healthy, last_heartbeat
            FROM harness_instances
            WHERE tenant_id = $1 AND id != $2
              AND status NOT IN ('deprovisioned', 'error')
            ORDER BY harness_role, name
            "#,
        )
        .bind(tenant_id)
        .bind(exclude)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                Option<String>,
                String,
                serde_json::Value,
                Option<String>,
                String,
                Option<bool>,
                Option<DateTime<Utc>>,
            ),
        >(
            r#"
            SELECT id, name, harness_role, packages, internal_url, status, healthy, last_heartbeat
            FROM harness_instances
            WHERE tenant_id = $1
              AND status NOT IN ('deprovisioned', 'error')
            ORDER BY harness_role, name
            "#,
        )
        .bind(tenant_id)
        .fetch_all(&state.db)
        .await
    };

    let siblings = match query {
        Ok(rows) => rows
            .into_iter()
            .map(
                |(id, name, role, packages, url, status, healthy, heartbeat)| SiblingHarness {
                    harness_id: id.to_string(),
                    name,
                    harness_role: role,
                    packages,
                    internal_url: url,
                    status,
                    healthy,
                    last_heartbeat: heartbeat,
                },
            )
            .collect(),
        Err(e) => {
            tracing::warn!("Failed to query siblings: {}", e);
            vec![]
        }
    };

    Json(SiblingsResponse { siblings })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_config_serialization() {
        let config = RemoteConfig {
            latest_version: Some("0.2.0".into()),
            enabled: true,
            model_overrides: vec![],
            feature_flags: std::collections::HashMap::new(),
            synced_at: "2025-01-01T00:00:00Z".into(),
        };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("0.2.0"));
        assert!(json.contains("\"enabled\":true"));
    }

    #[test]
    fn test_version_info_serialization() {
        let info = VersionInfo {
            latest_version: "0.1.0".into(),
            minimum_version: "0.1.0".into(),
            release_notes_url: None,
            update_required: false,
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("0.1.0"));
    }
}
