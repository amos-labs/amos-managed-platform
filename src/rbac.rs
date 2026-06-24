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
    pub const BUILD_RUN: &str = "build:run";
    pub const HARNESS_READ: &str = "harness:read";
    pub const HARNESS_PROVISION: &str = "harness:provision";
    pub const HARNESS_CONTROL: &str = "harness:control";
    pub const RECEIPTS_READ: &str = "receipts:read";
    pub const BILLING_READ: &str = "billing:read";
    pub const BILLING_MANAGE: &str = "billing:manage";

    /// Every scope, in ascending privilege-ish order (used for `owner`).
    pub const ALL: &[&str] = &[
        APP_READ,
        APP_DEPLOY,
        APP_CONTROL,
        BUILD_RUN,
        HARNESS_READ,
        HARNESS_PROVISION,
        HARNESS_CONTROL,
        RECEIPTS_READ,
        BILLING_READ,
        BILLING_MANAGE,
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
        BUILD_RUN,
        HARNESS_READ,
        HARNESS_PROVISION,
        HARNESS_CONTROL,
        RECEIPTS_READ,
        BILLING_READ,
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
        "list_apps" | "app_status" | "app_logs" => APP_READ,
        "deploy_app" => APP_DEPLOY,
        "app_control" => APP_CONTROL,
        // Build-as-a-service
        "build_image" | "build_status" => BUILD_RUN,
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
