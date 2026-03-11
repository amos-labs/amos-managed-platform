//! API discovery and fallback endpoints.
//!
//! Provides machine-readable endpoint catalogs at `/` and `/api/v1`,
//! plus an agent-friendly 404 fallback with actionable error details.

use axum::{
    extract::OriginalUri,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Serialize;

/// Platform metadata returned at the root path.
#[derive(Serialize)]
pub struct ApiCatalog {
    service: &'static str,
    version: &'static str,
    api_base: &'static str,
    documentation: &'static str,
    endpoints: Vec<EndpointGroup>,
}

#[derive(Serialize)]
pub struct EndpointGroup {
    group: &'static str,
    description: &'static str,
    endpoints: Vec<Endpoint>,
}

#[derive(Serialize)]
pub struct Endpoint {
    method: &'static str,
    path: &'static str,
    description: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<&'static str>,
}

/// `GET /` and `GET /api/v1` — return the full API endpoint catalog.
pub async fn api_catalog() -> impl IntoResponse {
    Json(build_catalog())
}

/// Fallback handler for any unmatched route.
///
/// Returns a structured 404 with the requested path, a clear error message,
/// and the full list of available endpoints so an agent can self-correct.
pub async fn not_found(OriginalUri(uri): OriginalUri) -> impl IntoResponse {
    let requested_path = uri.path().to_string();

    // Find close matches for the requested path
    let suggestions = find_suggestions(&requested_path);

    let body = NotFoundResponse {
        error: format!("No endpoint matches '{}'.", requested_path),
        code: "route_not_found",
        requested_path,
        hint: "All API endpoints are under /api/v1. Use GET / for the full endpoint catalog.",
        suggestions,
        catalog_url: "/",
    };

    (StatusCode::NOT_FOUND, Json(body))
}

#[derive(Serialize)]
struct NotFoundResponse {
    error: String,
    code: &'static str,
    requested_path: String,
    hint: &'static str,
    suggestions: Vec<String>,
    catalog_url: &'static str,
}

fn find_suggestions(requested: &str) -> Vec<String> {
    // Normalise: strip leading slash, lowercase
    let normalised = requested.trim_start_matches('/').to_lowercase();

    // All known path prefixes an agent might try
    let known_paths: Vec<(&str, &str)> = vec![
        ("api/v1/health", "GET /api/v1/health"),
        ("api/v1/readiness", "GET /api/v1/readiness"),
        ("api/v1/token/stats", "GET /api/v1/token/stats"),
        ("api/v1/token/decay-rate", "GET /api/v1/token/decay-rate"),
        ("api/v1/token/emission", "GET /api/v1/token/emission"),
        ("api/v1/token/calculate-decay", "POST /api/v1/token/calculate-decay"),
        ("api/v1/token/revenue-split", "POST /api/v1/token/revenue-split"),
        ("api/v1/governance/proposals", "GET|POST /api/v1/governance/proposals"),
        ("api/v1/governance/proposals/{id}", "GET /api/v1/governance/proposals/{id}"),
        ("api/v1/governance/proposals/{id}/vote", "POST /api/v1/governance/proposals/{id}/vote"),
        ("api/v1/governance/proposals/{id}/gates", "GET /api/v1/governance/proposals/{id}/gates"),
        ("api/v1/billing/customers", "GET|POST /api/v1/billing/customers"),
        ("api/v1/billing/customers/{id}", "GET /api/v1/billing/customers/{id}"),
        ("api/v1/billing/customers/{id}/usage", "GET /api/v1/billing/customers/{id}/usage"),
        ("api/v1/billing/customers/{id}/subscribe", "POST /api/v1/billing/customers/{id}/subscribe"),
        ("api/v1/billing/plans", "GET /api/v1/billing/plans"),
        ("api/v1/billing/usage", "GET /api/v1/billing/usage"),
        ("api/v1/billing/calculate", "POST /api/v1/billing/calculate"),
        ("api/v1/billing/pricing", "GET /api/v1/billing/pricing"),
        ("api/v1/provision/harness", "POST /api/v1/provision/harness"),
        ("api/v1/provision/harness/{id}", "GET|DELETE /api/v1/provision/harness/{id}"),
        ("api/v1/provision/harness/{id}/start", "POST /api/v1/provision/harness/{id}/start"),
        ("api/v1/provision/harness/{id}/stop", "POST /api/v1/provision/harness/{id}/stop"),
        ("api/v1/provision/harness/{id}/logs", "GET /api/v1/provision/harness/{id}/logs"),
        ("api/v1/sync/heartbeat", "POST /api/v1/sync/heartbeat"),
        ("api/v1/sync/config", "GET /api/v1/sync/config"),
        ("api/v1/sync/activity", "POST /api/v1/sync/activity"),
        ("api/v1/sync/version", "GET /api/v1/sync/version"),
    ];

    let mut matches = Vec::new();

    for (path, display) in &known_paths {
        // Exact substring match (e.g. user typed "health" or "billing")
        let path_lower = path.to_lowercase();
        if path_lower.contains(&normalised) || normalised.contains(&path_lower) {
            matches.push(display.to_string());
        }
    }

    // If no substring matches, try matching the last path segment
    if matches.is_empty() {
        if let Some(last_segment) = normalised.rsplit('/').next() {
            if !last_segment.is_empty() {
                for (path, display) in &known_paths {
                    if path.contains(last_segment) {
                        matches.push(display.to_string());
                    }
                }
            }
        }
    }

    // De-duplicate and limit
    matches.dedup();
    matches.truncate(5);
    matches
}

fn build_catalog() -> ApiCatalog {
    ApiCatalog {
        service: "AMOS Platform",
        version: crate::VERSION,
        api_base: "/api/v1",
        documentation: "https://docs.openclaw.ai",
        endpoints: vec![
            EndpointGroup {
                group: "Health",
                description: "Service health and readiness checks",
                endpoints: vec![
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/health",
                        description: "Liveness check. Returns {status, version}.",
                        auth: None,
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/readiness",
                        description: "Deep readiness check (DB, Redis, Solana). Returns component status.",
                        auth: None,
                    },
                ],
            },
            EndpointGroup {
                group: "Token Economics",
                description: "AMOS token supply, emission schedule, decay rates, and revenue splits",
                endpoints: vec![
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/token/stats",
                        description: "Token supply breakdown: total, treasury, entity, investor, community, reserve allocations and emission parameters.",
                        auth: None,
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/token/decay-rate",
                        description: "Current decay rate. Query params: ?revenue_cents=N&costs_cents=N",
                        auth: None,
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/token/calculate-decay",
                        description: "Calculate token decay for a specific stake context. Body: {tenure_days, current_balance, original_balance, vault_tier, days_inactive}.",
                        auth: None,
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/token/revenue-split",
                        description: "Calculate revenue split. Body: {amount, payment_type: 'usdc'|'amos'}.",
                        auth: None,
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/token/emission",
                        description: "Daily emission amount. Query params: ?day_index=N (0 = genesis).",
                        auth: None,
                    },
                ],
            },
            EndpointGroup {
                group: "Governance",
                description: "On-chain governance proposals, voting, and quality gates",
                endpoints: vec![
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/governance/proposals",
                        description: "List all governance proposals with vote tallies.",
                        auth: None,
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/governance/proposals",
                        description: "Create a proposal. Body: {title, description, proposer_wallet, proposal_type: 'feature'|'parameter'|'treasury'|'research'}. Requires minimum stake.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/governance/proposals/{id}",
                        description: "Get proposal detail including votes. Replace {id} with a UUID.",
                        auth: None,
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/governance/proposals/{id}/vote",
                        description: "Cast a vote. Body: {voter_wallet, support: true|false}. Weight derived from on-chain stake.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/governance/proposals/{id}/gates",
                        description: "Quality gate status for a proposal (benchmark, A/B test, feedback, steward approval).",
                        auth: None,
                    },
                ],
            },
            EndpointGroup {
                group: "Billing",
                description: "Customer management, subscriptions, usage metering, and pricing",
                endpoints: vec![
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/billing/customers",
                        description: "List all customers.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/billing/customers",
                        description: "Create a customer. Body: {name, email, organization?, plan?: 'free'|'starter'|'growth'|'enterprise'}. Returns API key.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/billing/customers/{id}",
                        description: "Get customer detail. Replace {id} with a UUID.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/billing/customers/{id}/usage",
                        description: "Get usage metrics for a customer in the current billing period.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/billing/customers/{id}/subscribe",
                        description: "Subscribe a customer to a plan. Body: {plan: 'free'|'starter'|'growth'|'enterprise', payment_method_id?}.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/billing/plans",
                        description: "List available subscription plans with pricing and limits.",
                        auth: None,
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/billing/usage",
                        description: "Get aggregated usage for the authenticated customer.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/billing/calculate",
                        description: "Calculate billing for compute usage records. Body: {customer_id, compute_records: [{model_name, input_tokens, output_tokens}], pay_with_amos}.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/billing/pricing",
                        description: "Get model pricing, markup percentage, and AMOS discount percentage.",
                        auth: None,
                    },
                ],
            },
            EndpointGroup {
                group: "Provisioning",
                description: "Harness container lifecycle: provision, start, stop, deprovision",
                endpoints: vec![
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/provision/harness",
                        description: "Provision a new harness container. Body: {customer_id, region?, instance_size?: 'small'|'medium'|'large', environment?, env_vars?}. Requires Docker.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/provision/harness/{id}",
                        description: "Get harness container status. Replace {id} with the container ID.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/provision/harness/{id}/start",
                        description: "Start a stopped harness container.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/provision/harness/{id}/stop",
                        description: "Stop a running harness container.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "DELETE",
                        path: "/api/v1/provision/harness/{id}",
                        description: "Deprovision (remove) a harness container permanently.",
                        auth: Some("Bearer token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/provision/harness/{id}/logs",
                        description: "Retrieve recent logs from a harness container.",
                        auth: Some("Bearer token"),
                    },
                ],
            },
            EndpointGroup {
                group: "Sync",
                description: "Harness-to-platform sync: heartbeat, config, activity reporting, versioning",
                endpoints: vec![
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/sync/heartbeat",
                        description: "Report harness heartbeat. Body: {harness_version, deployment_mode, uptime_secs, healthy, timestamp}.",
                        auth: Some("X-Harness-Token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/sync/config",
                        description: "Fetch remote config for a harness. Query params: ?version=x.y.z",
                        auth: Some("X-Harness-Token"),
                    },
                    Endpoint {
                        method: "POST",
                        path: "/api/v1/sync/activity",
                        description: "Report usage activity. Body: {period_start, period_end, conversations, messages, tokens_input, tokens_output, tools_executed, models_used, timestamp}.",
                        auth: Some("X-Harness-Token"),
                    },
                    Endpoint {
                        method: "GET",
                        path: "/api/v1/sync/version",
                        description: "Get latest harness version info and whether an update is required.",
                        auth: None,
                    },
                ],
            },
        ],
    }
}
