//! Per-harness database lifecycle management.
//!
//! Each harness gets its own PostgreSQL database for full tenant isolation.
//! The database is created on the same RDS instance using an admin connection
//! derived from `ECS_HARNESS_DATABASE_URL`. Harnesses auto-run migrations on
//! startup via `sqlx::migrate!`, so an empty database is all we need.

use amos_core::{AmosError, Result};
use sqlx::{postgres::PgConnectOptions, ConnectOptions, Connection, PgConnection};
use tracing::{error, info, warn};
use uuid::Uuid;

/// Generate a stable database name from a harness ID.
/// Format: `harness_{first 12 chars of UUID without hyphens}`
/// e.g., harness_cf1e19bc2901
pub fn database_name_for_harness(harness_id: Uuid) -> String {
    let hex = harness_id.as_simple().to_string();
    format!("harness_{}", &hex[..12])
}

/// Build a connection URL for a specific harness database, given the base
/// (admin) database URL and the target database name.
pub fn database_url_for_harness(base_url: &str, db_name: &str) -> String {
    // base_url looks like: postgres://user:pass@host:5432/amos_harness
    // We need to replace the database name at the end.
    if let Some(pos) = base_url.rfind('/') {
        // Strip any query params from the db name portion
        let prefix = &base_url[..pos];
        format!("{}/{}", prefix, db_name)
    } else {
        // Shouldn't happen, but fallback
        format!("{}/{}", base_url, db_name)
    }
}

/// Parse the base URL into PgConnectOptions pointing at the `postgres` maintenance DB.
fn admin_connect_options(base_url: &str) -> Result<PgConnectOptions> {
    let opts: PgConnectOptions = base_url
        .parse()
        .map_err(|e| AmosError::Internal(format!("Invalid database URL: {}", e)))?;
    // Connect to the `postgres` maintenance database for CREATE/DROP DATABASE.
    Ok(opts.database("postgres"))
}

/// Create a new database for a harness. Idempotent — skips if it already exists.
///
/// Returns the full connection URL for the new database.
pub async fn create_harness_database(base_url: &str, harness_id: Uuid) -> Result<String> {
    let db_name = database_name_for_harness(harness_id);
    let opts = admin_connect_options(base_url)?;

    let mut conn = PgConnection::connect_with(&opts.disable_statement_logging())
        .await
        .map_err(|e| AmosError::Internal(format!("Failed to connect to postgres DB: {}", e)))?;

    // Check if database already exists
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(&db_name)
    .fetch_one(&mut conn)
    .await
    .map_err(|e| AmosError::Internal(format!("Failed to check database existence: {}", e)))?;

    if exists {
        info!(db_name = %db_name, "Harness database already exists, reusing");
    } else {
        // CREATE DATABASE cannot be parameterized, but db_name is derived from a UUID
        // (hex chars only) so it's safe from injection.
        let create_sql = format!("CREATE DATABASE \"{}\"", db_name);
        sqlx::query(&create_sql)
            .execute(&mut conn)
            .await
            .map_err(|e| AmosError::Internal(format!("Failed to create database '{}': {}", db_name, e)))?;

        info!(db_name = %db_name, harness_id = %harness_id, "Created per-harness database");
    }

    Ok(database_url_for_harness(base_url, &db_name))
}

/// Drop a harness database. Best-effort — logs errors but doesn't fail the caller.
pub async fn drop_harness_database(base_url: &str, db_name: &str) {
    let opts = match admin_connect_options(base_url) {
        Ok(o) => o,
        Err(e) => {
            error!(db_name = %db_name, "Failed to parse base URL for drop: {}", e);
            return;
        }
    };

    let mut conn = match PgConnection::connect_with(&opts.disable_statement_logging()).await {
        Ok(c) => c,
        Err(e) => {
            error!(db_name = %db_name, "Failed to connect for drop: {}", e);
            return;
        }
    };

    // Terminate active connections to the database before dropping.
    let terminate_sql = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}' AND pid <> pg_backend_pid()",
        db_name
    );
    let _ = sqlx::query(&terminate_sql).execute(&mut conn).await;

    let drop_sql = format!("DROP DATABASE IF EXISTS \"{}\"", db_name);
    match sqlx::query(&drop_sql).execute(&mut conn).await {
        Ok(_) => info!(db_name = %db_name, "Dropped harness database"),
        Err(e) => warn!(db_name = %db_name, "Failed to drop harness database: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_name_deterministic() {
        let id = Uuid::parse_str("cf1e19bc-2901-43cc-acb7-85e1144199c3").unwrap();
        assert_eq!(database_name_for_harness(id), "harness_cf1e19bc2901");
    }

    #[test]
    fn database_name_only_hex() {
        // Verify no hyphens or special chars in the name
        let id = Uuid::new_v4();
        let name = database_name_for_harness(id);
        assert!(name.starts_with("harness_"));
        assert!(name[8..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn database_url_replacement() {
        let base = "postgres://user:pass@host:5432/amos_harness";
        let url = database_url_for_harness(base, "harness_abc123def456");
        assert_eq!(url, "postgres://user:pass@host:5432/harness_abc123def456");
    }
}
