//! Role/scope-based access control — AMOS as the control plane between a
//! business and AI.
//!
//! Two principal axes, one scope catalog:
//! - **User axis:** a human's `role` (`owner|admin|member|viewer`) maps to a
//!   set of scopes.
//! - **AI/machine axis:** an API key (used by Claude / an autonomous agent)
//!   carries an explicit `scopes[]` *subset* — the effective scopes are
//!   `role_scopes(creator) ∩ key.scopes`, so you can constrain what the AI may
//!   do without exceeding the human who issued the key. An empty key scope
//!   list means "full role scopes" (back-compat for keys issued before RBAC).
//!
//! Scopes are enforced on the MCP tool surface (and REST), and recorded on the
//! proof receipts so authorization decisions are auditable.

use std::collections::HashSet;

/// Canonical scope catalog (`resource:action`). Keep this the single source of
/// truth — tools and roles reference these constants.
pub mod scope {
    pub const APP_READ: &str = "app:read";
    pub const APP_DEPLOY: &str = "app:deploy";
    pub const APP_CONTROL: &str = "app:control";
    /// Provision/tear down isolated environments (staging/preview) for an app.
    pub const ENV_PROVISION: &str = "env:provision";
    /// Read objects in the tenant app's bucket (s3_list/s3_get).
    pub const STORAGE_READ: &str = "storage:read";
    /// Write/delete objects in the tenant app's bucket (s3_put/s3_delete).
    pub const STORAGE_WRITE: &str = "storage:write";
    pub const BUILD_RUN: &str = "build:run";
    /// Read-only SQL against the tenant's app DB (governed db_query verb).
    /// Sensitive — granted to operators+ and intended to be human-set per key.
    pub const DB_READ: &str = "db:read";
    /// Single-statement DML writes against the tenant's app DB (governed
    /// db_write verb). The most sensitive scope: direct mutation of prod data.
    /// Owner-only by default and never auto-granted — a human enables it per key.
    pub const DB_WRITE: &str = "db:write";
    pub const HARNESS_READ: &str = "harness:read";
    pub const HARNESS_PROVISION: &str = "harness:provision";
    pub const HARNESS_CONTROL: &str = "harness:control";
    pub const RECEIPTS_READ: &str = "receipts:read";
    pub const BILLING_READ: &str = "billing:read";
    pub const BILLING_MANAGE: &str = "billing:manage";
    // ── Engine scopes (governed business building blocks; see engines architecture).
    //    Finance is the reference engine; later engines add their own (marketing:*, …).
    /// Read the tenant's finance engine (board / truth / history / qbo accounts).
    pub const FINANCE_READ: &str = "finance:read";
    /// Write finance data (budgets, actuals, categories, QBO mappings) — proof-carrying.
    pub const FINANCE_WRITE: &str = "finance:write";
    /// Connect finance integrations / set billing keys (credentials). Owner-only.
    pub const FINANCE_CONNECT: &str = "finance:connect";

    /// Every scope, in ascending privilege-ish order (used for `owner`).
    pub const ALL: &[&str] = &[
        APP_READ,
        APP_DEPLOY,
        APP_CONTROL,
        ENV_PROVISION,
        STORAGE_READ,
        STORAGE_WRITE,
        BUILD_RUN,
        DB_READ,
        DB_WRITE,
        HARNESS_READ,
        HARNESS_PROVISION,
        HARNESS_CONTROL,
        RECEIPTS_READ,
        BILLING_READ,
        BILLING_MANAGE,
        FINANCE_READ,
        FINANCE_WRITE,
        FINANCE_CONNECT,
    ];
}

/// Scopes granted by a user role.
///
/// - `viewer`  — read-only (status/logs/receipts/billing read).
/// - `member`  — viewer + operate apps/builds/harnesses (the day-to-day operator).
/// - `admin`   — member + billing read (manage settings/users handled out of band).
/// - `owner`   — everything, including `billing:manage`.
pub fn scopes_for_role(role: &str) -> HashSet<&'static str> {
    use scope::*;
    let viewer: &[&str] = &[APP_READ, HARNESS_READ, RECEIPTS_READ, BILLING_READ];
    let member: &[&str] = &[
        APP_READ,
        APP_DEPLOY,
        APP_CONTROL,
        ENV_PROVISION,
        STORAGE_READ,
        STORAGE_WRITE,
        BUILD_RUN,
        DB_READ,
        HARNESS_READ,
        HARNESS_PROVISION,
        HARNESS_CONTROL,
        RECEIPTS_READ,
        BILLING_READ,
        FINANCE_READ,
        FINANCE_WRITE,
    ];
    match role {
        "owner" => ALL.iter().copied().collect(),
        // admin currently mirrors member's operational scopes + billing read;
        // user/tenant administration is enforced separately (require_admin).
        "admin" => member.iter().copied().collect(),
        "member" => member.iter().copied().collect(),
        "viewer" => viewer.iter().copied().collect(),
        _ => HashSet::new(),
    }
}

/// The scope required to invoke an MCP tool. `None` = no scope gate (e.g. a
/// pure-metadata call). Tools not listed default to "no gate" so adding a new
/// read-only tool doesn't accidentally lock everyone out; privileged tools
/// MUST be listed here.
pub fn required_scope(tool: &str) -> Option<&'static str> {
    use scope::*;
    Some(match tool {
        // App lifecycle
        "list_apps" | "app_status" | "app_logs" | "deploy_status" => APP_READ,
        "deploy" | "deploy_app" | "app_redeploy" => APP_DEPLOY,
        "provision_env" | "teardown_env" => ENV_PROVISION,
        "s3_list" | "s3_get" => STORAGE_READ,
        "s3_put" | "s3_delete" => STORAGE_WRITE,
        "app_control" => APP_CONTROL,
        // Build-as-a-service
        "build_image" | "build_status" => BUILD_RUN,
        // Governed DB access
        "db_query" => DB_READ,
        "db_write" => DB_WRITE,
        // Read-only schema introspection (same sensitivity as a read).
        "describe_table" => DB_READ,
        // Finance engine (governed reference engine).
        "finance_board" | "finance_history" | "finance_truth" | "qbo_accounts"
        | "revenue_summary" | "org_subscriptions" | "churn_snapshot" => FINANCE_READ,
        "create_finance_line"
        | "update_finance_line"
        | "set_finance_actual"
        | "set_finance_budget"
        | "set_finance_mapping" => FINANCE_WRITE,
        "set_billing_key" => FINANCE_CONNECT,
        // Environments / harness
        "list_harnesses" | "harness_status" | "harness_logs" | "get_harness_config"
        | "list_releases" => HARNESS_READ,
        "provision_harness" => HARNESS_PROVISION,
        "harness_control" | "set_harness_config" => HARNESS_CONTROL,
        // Proof / audit
        "list_receipts" => RECEIPTS_READ,
        _ => return None,
    })
}

/// Compute a principal's effective scopes.
///
/// `role` is the human role (or, for an API key, the creator's role).
/// `key_scopes` is the API key's explicit scope list, if the principal is a
/// key; `None` (a JWT user) or an empty list yields the full role scopes.
pub fn effective_scopes(role: &str, key_scopes: Option<&[String]>) -> HashSet<String> {
    let role_scopes: HashSet<String> = scopes_for_role(role)
        .into_iter()
        .map(String::from)
        .collect();
    match key_scopes {
        Some(ks) if !ks.is_empty() => {
            let requested: HashSet<String> = ks.iter().cloned().collect();
            // A key can never exceed its creator's role.
            role_scopes.intersection(&requested).cloned().collect()
        }
        _ => role_scopes,
    }
}

/// Whether `effective` grants `required`.
pub fn allows(effective: &HashSet<String>, required: &str) -> bool {
    effective.contains(required)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_is_read_only() {
        let s = scopes_for_role("viewer");
        assert!(s.contains(scope::APP_READ));
        assert!(!s.contains(scope::APP_DEPLOY));
        assert!(!s.contains(scope::BUILD_RUN));
    }

    #[test]
    fn member_can_operate_not_bill() {
        let s = scopes_for_role("member");
        assert!(s.contains(scope::APP_DEPLOY));
        assert!(s.contains(scope::BUILD_RUN));
        assert!(!s.contains(scope::BILLING_MANAGE));
    }

    #[test]
    fn db_write_is_owner_only() {
        // db:read is a day-to-day operator scope; db:write is not — only owner.
        assert!(scopes_for_role("member").contains(scope::DB_READ));
        assert!(!scopes_for_role("member").contains(scope::DB_WRITE));
        assert!(!scopes_for_role("admin").contains(scope::DB_WRITE));
        assert!(scopes_for_role("owner").contains(scope::DB_WRITE));
        assert_eq!(required_scope("db_write"), Some(scope::DB_WRITE));
    }

    #[test]
    fn storage_scopes_are_operator_level() {
        assert_eq!(required_scope("s3_get"), Some(scope::STORAGE_READ));
        assert_eq!(required_scope("s3_list"), Some(scope::STORAGE_READ));
        assert_eq!(required_scope("s3_put"), Some(scope::STORAGE_WRITE));
        assert_eq!(required_scope("s3_delete"), Some(scope::STORAGE_WRITE));
        let m = scopes_for_role("member");
        assert!(m.contains(scope::STORAGE_READ));
        assert!(m.contains(scope::STORAGE_WRITE));
        assert!(!scopes_for_role("viewer").contains(scope::STORAGE_WRITE));
    }

    #[test]
    fn env_provision_is_operator_scoped() {
        assert_eq!(required_scope("provision_env"), Some(scope::ENV_PROVISION));
        assert_eq!(required_scope("teardown_env"), Some(scope::ENV_PROVISION));
        // operators (member) and owner can provision envs; viewer cannot.
        assert!(scopes_for_role("member").contains(scope::ENV_PROVISION));
        assert!(scopes_for_role("owner").contains(scope::ENV_PROVISION));
        assert!(!scopes_for_role("viewer").contains(scope::ENV_PROVISION));
    }

    #[test]
    fn finance_verbs_map_to_finance_scopes() {
        // Reads → finance:read.
        for t in [
            "finance_board",
            "finance_history",
            "finance_truth",
            "qbo_accounts",
            "revenue_summary",
            "org_subscriptions",
            "churn_snapshot",
        ] {
            assert_eq!(required_scope(t), Some(scope::FINANCE_READ), "{t}");
        }
        // Writes → finance:write.
        for t in [
            "create_finance_line",
            "update_finance_line",
            "set_finance_actual",
            "set_finance_budget",
            "set_finance_mapping",
        ] {
            assert_eq!(required_scope(t), Some(scope::FINANCE_WRITE), "{t}");
        }
        // Connect (credentials) → finance:connect.
        assert_eq!(
            required_scope("set_billing_key"),
            Some(scope::FINANCE_CONNECT)
        );

        // member operates finances (read+write) but cannot connect integrations;
        // finance:connect is owner-only (it sets credentials).
        let m = scopes_for_role("member");
        assert!(m.contains(scope::FINANCE_READ));
        assert!(m.contains(scope::FINANCE_WRITE));
        assert!(!m.contains(scope::FINANCE_CONNECT));
        assert!(scopes_for_role("owner").contains(scope::FINANCE_CONNECT));
    }

    #[test]
    fn owner_has_everything() {
        let s = scopes_for_role("owner");
        assert!(s.contains(scope::BILLING_MANAGE));
        assert_eq!(s.len(), scope::ALL.len());
    }

    #[test]
    fn unknown_role_gets_nothing() {
        assert!(scopes_for_role("nobody").is_empty());
    }

    #[test]
    fn privileged_tools_require_scopes() {
        assert_eq!(required_scope("deploy_app"), Some(scope::APP_DEPLOY));
        assert_eq!(required_scope("build_image"), Some(scope::BUILD_RUN));
        assert_eq!(required_scope("list_receipts"), Some(scope::RECEIPTS_READ));
    }

    #[test]
    fn api_key_narrows_role_never_exceeds() {
        // A member-created key restricted to read-only.
        let eff = effective_scopes("member", Some(&["app:read".into(), "receipts:read".into()]));
        assert!(allows(&eff, scope::APP_READ));
        assert!(!allows(&eff, scope::APP_DEPLOY)); // narrowed away

        // A key asking for billing:manage it can't have (creator is member).
        let eff2 = effective_scopes("member", Some(&["billing:manage".into()]));
        assert!(!allows(&eff2, scope::BILLING_MANAGE)); // intersection drops it
    }

    #[test]
    fn empty_key_scopes_mean_full_role() {
        let eff = effective_scopes("member", Some(&[]));
        assert!(allows(&eff, scope::APP_DEPLOY));
        let eff_none = effective_scopes("member", None);
        assert!(allows(&eff_none, scope::APP_DEPLOY));
    }
}
