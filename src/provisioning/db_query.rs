//! Governed read-only SQL against a tenant's *application* database.
//!
//! The MCP `db_query` verb (scope `db:read`) lets an operator/agent inspect an
//! app's data — e.g. "does `get_pool_users` return rows?" — without ECS-exec or
//! raw psql. Every call is scoped, capped, and emits a proof receipt.
//!
//! Safety (approach A): we connect with the app's existing credentials but run
//! inside a `READ ONLY` transaction with a statement timeout, and wrap the
//! caller's SQL as a subquery (`SELECT to_jsonb(t) FROM (<sql>) t LIMIT n`) —
//! which both forces it to be a single SELECT (writes/DDL fail to parse as a
//! subquery) and lets Postgres render arbitrary columns/types to JSON. The
//! read-only transaction is the real guardrail; the wrapping is defense in depth.

use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::time::Duration;

use amos_core::{AmosError, Result};

/// Run a read-only query against `db_url` and return rows as JSON values.
/// `max_rows` is clamped to [1, 10000].
pub async fn run_readonly_query(
    db_url: &str,
    sql: &str,
    max_rows: i64,
) -> Result<Vec<serde_json::Value>> {
    let cap = max_rows.clamp(1, 10_000);
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(AmosError::Validation("empty sql".into()));
    }
    // Wrap as a subquery → arbitrary SELECT in, JSON rows out; non-SELECT input
    // (INSERT/UPDATE/DDL) fails to parse here, and the READ ONLY tx blocks it
    // at the engine level regardless.
    let wrapped = format!("SELECT to_jsonb(t) AS row FROM ({trimmed}) AS t LIMIT {cap}");

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(db_url)
        .await
        .map_err(|e| AmosError::Internal(format!("connect to app db failed: {e}")))?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AmosError::Internal(format!("begin tx: {e}")))?;
    // READ ONLY must precede any data-touching statement in the tx.
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(|e| AmosError::Internal(format!("set read only: {e}")))?;
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await
        .ok();

    let result = sqlx::query(&wrapped).fetch_all(&mut *tx).await;
    // Read-only tx — always roll back; nothing should have changed anyway.
    let _ = tx.rollback().await;
    pool.close().await;

    let rows = result.map_err(|e| AmosError::Validation(format!("query failed: {e}")))?;
    Ok(rows
        .iter()
        .map(|r| {
            r.try_get::<serde_json::Value, _>("row")
                .unwrap_or(serde_json::Value::Null)
        })
        .collect())
}

/// Outcome of a governed write. `committed` is false for a dry run (the tx was
/// rolled back); `rows` are the affected rows via `RETURNING`.
#[derive(Debug)]
pub struct WriteOutcome {
    pub affected: usize,
    pub committed: bool,
    pub rows: Vec<serde_json::Value>,
}

/// Validate a single DML statement and return it wrapped so the affected rows
/// come back as JSON. Pure (no DB) so the guardrails are unit-testable:
///   - single statement only (no `;` inside)
///   - INSERT / UPDATE / DELETE only — DDL (DROP/ALTER/CREATE/TRUNCATE) and
///     SELECT are rejected; schema changes belong in the app's migrations
///   - UPDATE/DELETE must carry a WHERE unless `allow_unfiltered`
///
/// We append `RETURNING *` when absent and wrap as a data-modifying CTE
/// (`WITH _w AS (<dml> RETURNING *) SELECT to_jsonb(_w) ...`) so we capture the
/// exact rows changed and their count. Values should be passed as bound params
/// (`$1`, …), never interpolated — so literals containing `;` aren't an issue.
pub fn build_write_query(sql: &str, allow_unfiltered: bool) -> Result<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(AmosError::Validation("empty sql".into()));
    }
    if trimmed.contains(';') {
        return Err(AmosError::Validation(
            "only a single statement is allowed (no ';'); pass values as $1,$2,… params".into(),
        ));
    }
    // Tokenize to lowercase identifier words for keyword checks.
    let words: Vec<String> = trimmed
        .to_ascii_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|s| s.to_string())
        .collect();
    let verb = words.first().map(String::as_str).unwrap_or("");
    if !matches!(verb, "insert" | "update" | "delete") {
        return Err(AmosError::Validation(
            "db_write allows only INSERT/UPDATE/DELETE — DDL and SELECT are not permitted \
             (schema changes go through the app's migrations / deploy)"
                .into(),
        ));
    }
    if matches!(verb, "update" | "delete")
        && !allow_unfiltered
        && !words.iter().any(|w| w == "where")
    {
        return Err(AmosError::Validation(
            "UPDATE/DELETE without a WHERE clause is blocked; pass allow_unfiltered=true to override"
                .into(),
        ));
    }
    let has_returning = words.iter().any(|w| w == "returning");
    let inner = if has_returning {
        trimmed.to_string()
    } else {
        format!("{trimmed} RETURNING *")
    };
    Ok(format!(
        "WITH _w AS ({inner}) SELECT to_jsonb(_w) AS row FROM _w"
    ))
}

/// Run a governed single-statement DML write against `db_url`. Guardrails: the
/// statement is validated by [`build_write_query`]; it runs inside a transaction
/// with a statement timeout; if the affected-row count exceeds `max_rows` the tx
/// is rolled back (nothing is committed); and `dry_run` always rolls back after
/// measuring the effect. `params` bind to `$1..$n` (typed by JSON kind).
pub async fn run_write_dml(
    db_url: &str,
    sql: &str,
    params: &[serde_json::Value],
    max_rows: i64,
    dry_run: bool,
    allow_unfiltered: bool,
) -> Result<WriteOutcome> {
    let cap = max_rows.clamp(1, 10_000);
    let wrapped = build_write_query(sql, allow_unfiltered)?;

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(db_url)
        .await
        .map_err(|e| AmosError::Internal(format!("connect to app db failed: {e}")))?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AmosError::Internal(format!("begin tx: {e}")))?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await
        .ok();

    let mut q = sqlx::query(&wrapped);
    for p in params {
        q = match p {
            serde_json::Value::Null => q.bind(Option::<String>::None),
            serde_json::Value::Bool(b) => q.bind(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    q.bind(i)
                } else {
                    q.bind(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => q.bind(s.clone()),
            // Arrays/objects bind as jsonb.
            other => q.bind(other.clone()),
        };
    }

    let result = q.fetch_all(&mut *tx).await;
    let rows = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.rollback().await;
            pool.close().await;
            return Err(AmosError::Validation(format!("write failed: {e}")));
        }
    };
    let affected = rows.len();

    // Blast-radius guard: refuse to commit beyond the cap.
    if affected as i64 > cap {
        let _ = tx.rollback().await;
        pool.close().await;
        return Err(AmosError::Validation(format!(
            "write would affect {affected} rows, exceeding max_rows {cap} — aborted, nothing committed"
        )));
    }

    let committed = if dry_run {
        let _ = tx.rollback().await;
        false
    } else {
        tx.commit()
            .await
            .map_err(|e| AmosError::Internal(format!("commit: {e}")))?;
        true
    };
    pool.close().await;

    let out_rows = rows
        .iter()
        .map(|r| {
            r.try_get::<serde_json::Value, _>("row")
                .unwrap_or(serde_json::Value::Null)
        })
        .collect();
    Ok(WriteOutcome {
        affected,
        committed,
        rows: out_rows,
    })
}

/// Resolve a postgres connection URL for a deployment. The secret supplies the
/// **password** (and may hold a full URL); non-secret parts (host/user/db/port)
/// come from the deployment's `aws_meta` so we never store the password in the
/// control plane. Resolution order:
///   1. `aws_meta.db_url_key`  → that key in the secret holds a full URL
///   2. secret `DATABASE_URL`  → a full URL, or the secret is itself a bare URL
///   3. assemble from parts: host/user/db/port taken from `aws_meta`
///      (`db_host`/`db_user`/`db_name`/`db_port`), falling back to the secret's
///      `PGHOST`/`PGUSER`/`PGDATABASE`/`PGPORT`; the password is read from the
///      secret at `aws_meta.db_password_key` (default `PGPASSWORD`).
///
/// Returns `None` if no connection can be derived. Never logs the URL.
pub fn resolve_db_url(aws_meta: &serde_json::Value, secret_json: &str) -> Option<String> {
    let meta = |k: &str| {
        aws_meta
            .get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    };

    // The secret may be a bare URL string rather than JSON.
    let secret: serde_json::Value = match serde_json::from_str(secret_json) {
        Ok(v) => v,
        Err(_) => {
            let s = secret_json.trim();
            if s.starts_with("postgres://") || s.starts_with("postgresql://") {
                return Some(s.to_string());
            }
            return None;
        }
    };
    let sget = |k: &str| {
        secret
            .get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    };

    // 1. explicit full-URL key in the secret.
    if let Some(k) = meta("db_url_key") {
        if let Some(u) = sget(&k) {
            return Some(u);
        }
    }
    // 2. conventional full URL in the secret.
    if let Some(u) = sget("DATABASE_URL") {
        return Some(u);
    }
    // 3. assemble: non-secret parts from aws_meta (fallback to secret), password
    //    from the secret.
    let host = meta("db_host")
        .or_else(|| sget("PGHOST"))
        .or_else(|| sget("PGIP"))?;
    let user = meta("db_user").or_else(|| sget("PGUSER"))?;
    let db = meta("db_name").or_else(|| sget("PGDATABASE"))?;
    let port = meta("db_port")
        .or_else(|| sget("PGPORT"))
        .unwrap_or_else(|| "5432".to_string());
    let pass_key = meta("db_password_key").unwrap_or_else(|| "PGPASSWORD".to_string());
    let pass = sget(&pass_key).unwrap_or_default();
    Some(format!("postgres://{user}:{pass}@{host}:{port}/{db}"))
}

#[cfg(test)]
mod tests {
    use super::build_write_query;

    #[test]
    fn insert_is_wrapped_with_returning() {
        let q = build_write_query("INSERT INTO t (a) VALUES ($1)", false).unwrap();
        assert!(q.starts_with("WITH _w AS (INSERT INTO t (a) VALUES ($1) RETURNING *)"));
        assert!(q.contains("to_jsonb(_w)"));
    }

    #[test]
    fn existing_returning_is_preserved() {
        let q = build_write_query("INSERT INTO t (a) VALUES ($1) RETURNING id", false).unwrap();
        assert!(q.contains("RETURNING id"));
        // not double-appended
        assert_eq!(q.matches("RETURNING").count(), 1);
    }

    #[test]
    fn select_and_ddl_are_rejected() {
        assert!(build_write_query("SELECT 1", false).is_err());
        assert!(build_write_query("DROP TABLE users", false).is_err());
        assert!(build_write_query("TRUNCATE users", false).is_err());
        assert!(build_write_query("ALTER TABLE users ADD c int", false).is_err());
    }

    #[test]
    fn unfiltered_update_delete_blocked_unless_overridden() {
        assert!(build_write_query("UPDATE users SET active = true", false).is_err());
        assert!(build_write_query("DELETE FROM users", false).is_err());
        assert!(build_write_query("UPDATE users SET active = true", true).is_ok());
        assert!(build_write_query("DELETE FROM users WHERE id = $1", false).is_ok());
    }

    #[test]
    fn multi_statement_rejected() {
        assert!(build_write_query("INSERT INTO t (a) VALUES (1); DROP TABLE t", false).is_err());
    }

    #[test]
    fn empty_rejected() {
        assert!(build_write_query("   ;  ", false).is_err());
    }
}
