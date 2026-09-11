//! Core crate for the Lithograph engine.
//!
//! The Phase 02 storage foundation lives here so every later graph mutation is
//! forced through immutable Layer/Commit history rather than a mutable current
//! graph side channel.

#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]
#![forbid(unsafe_code)]

/// Frozen Cypher compatibility profile implemented by the first Lithograph release.
pub const CYPHER_PROFILE: &str = "CY25-2026.08";

pub mod cypher;
pub mod query;
pub mod storage;
