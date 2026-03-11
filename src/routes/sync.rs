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
use uuid::Uuid;

use crate::state::PlatformState;

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/sync/heartbeat", post(receive_heartbeat))
        .route("/sync/config", get(get_config))
        .route("/sync/activity", post(receive_activity))
        .route("/sync/version", get(get_latest_version))
}

// ── Heartbeat ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HeartbeatPayload {
    harness_version: String,
    deployment_mode: String,
    uptime_secs: u64,
    healthy: bool,
    timestamp: String,
    /// Optional tenant_id for identifying which harness is sending
    tenant_id: Option<String>,
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
        payload.harness_version, payload.deployment_mode,
        payload.uptime_secs, payload.healthy, payload.tenant_id,
    );

    // Update harness status in database if tenant_id is provided
    if let Some(tenant_id_str) = &payload.tenant_id {
        if let Ok(tenant_id) = uuid::Uuid::parse_str(tenant_id_str) {
            let result = sqlx::query(
                r#"
                UPDATE harness_instances
                SET last_heartbeat = NOW(),
                    harness_version = $1,
                    healthy = $2
                WHERE tenant_id = $3 AND status != 'deprovisioned'
                "#
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
                        debug!("No harness instance found for tenant {} (may not be provisioned yet)", tenant_id);
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
        debug!("No tenant_id provided in heartbeat (backwards compatibility mode)");
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
    debug!("Config request: version={:?}, tenant_id={:?}", query.version, query.tenant_id);

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
                "#
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
                    debug!("No config found in database for tenant {}, using defaults", tenant_id);
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch config from database: {}, using defaults", e);
                }
            }
        }
    }

    Json(RemoteConfig {
        latest_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        enabled,
        model_overrides: vec![],
        feature_flags,
        synced_at: Utc::now().to_rfc3339(),
    })
}

// ── Activity Ingest ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ActivityReport {
    period_start: String,
    period_end: String,
    conversations: u64,
    messages: u64,
    tokens_input: u64,
    tokens_output: u64,
    tools_executed: u64,
    models_used: Vec<String>,
    timestamp: String,
    /// Optional tenant_id for identifying which harness is reporting
    tenant_id: Option<String>,
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
                    "#
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

async fn get_latest_version() -> impl IntoResponse {
    Json(VersionInfo {
        latest_version: env!("CARGO_PKG_VERSION").to_string(),
        minimum_version: "0.1.0".to_string(),
        release_notes_url: None,
        update_required: false,
    })
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
