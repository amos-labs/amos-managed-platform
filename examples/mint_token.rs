//! Mint a short-lived MCP access token for local operation.
//!   cargo run --example mint_token -- <user_id> <tenant_id> <slug>
//! Uses AMOS__AUTH__JWT_SECRET (or the dev default).

use amos_platform::auth;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let user = a.get(1).cloned().expect("user_id");
    let tenant = a.get(2).cloned().expect("tenant_id");
    let slug = a.get(3).cloned().unwrap_or_else(|| "cuspr".to_string());
    let secret = std::env::var("AMOS__AUTH__JWT_SECRET")
        .unwrap_or_else(|_| "CHANGE-ME-in-production-amos-jwt-secret-2025".to_string());
    let token = auth::create_access_token(
        user.parse().expect("user uuid"),
        tenant.parse().expect("tenant uuid"),
        "admin",
        &slug,
        &secret,
        86_400,
    )
    .expect("token");
    println!("{token}");
}
