//! # AMOS Platform
//!
//! The central service that powers the AMOS token economy.
//!
//! This is the **single centralized platform** that all customer harnesses
//! connect to. It provides:
//!
//! - Token economics engine (decay, emission, revenue distribution)
//! - Governance system (proposals, voting, quality gates)
//! - Customer billing and subscription management
//! - Harness provisioning (spin up/tear down customer containers)
//! - Solana blockchain integration
//! - gRPC API for harness-to-platform communication
//! - REST API for admin dashboard and operations
//!
//! ## Architecture
//!
//! The platform consists of:
//! - REST API (Axum) for admin operations
//! - gRPC server for harness communication
//! - Token economics engine
//! - Governance module
//! - Billing and subscription management
//! - Harness provisioning via Docker
//! - Solana on-chain integration

pub mod auth;
pub mod billing;
pub mod governance;
pub mod middleware;
pub mod provisioning;
pub mod routes;
pub mod server;
pub mod services;
pub mod solana;
pub mod state;

// Re-export core types
pub use amos_core::{AmosError, AppConfig, Result};
pub use state::PlatformState;

/// Library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
