//! HTTP API route definitions.

use axum::{routing::get, Router};

use crate::state::PlatformState;

pub mod auth;
pub mod billing;
pub mod discovery;
pub mod governance;
pub mod health;
pub mod provisioning;
pub mod sync;
pub mod tenants;
pub mod token;
pub mod ui;

/// Build all API routes (nested under /api/v1 by server.rs).
pub fn api_routes() -> Router<PlatformState> {
    Router::new()
        .merge(health::routes())
        .merge(auth::routes())
        .merge(tenants::routes())
        .merge(token::routes())
        .merge(governance::routes())
        .merge(billing::routes())
        .merge(provisioning::routes())
        .merge(sync::routes())
        // Also serve the catalog at /api/v1 itself
        .route("/", get(discovery::api_catalog))
}
