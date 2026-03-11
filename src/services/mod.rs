//! Background services for the AMOS platform.
//!
//! This module provides scheduled background tasks that power the AMOS
//! token economy, including:
//!
//! - Nightly bounty generation and emission distribution
//! - Contribution tracking and reward calculations
//! - On-chain bounty proof submission

pub mod bounty_service;

pub use bounty_service::BountyService;
