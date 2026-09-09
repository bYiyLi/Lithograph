//! Core crate for the Lithograph engine.
//!
//! Phase 00 intentionally contains no graph product behavior. Later phases add
//! parser, storage, planning, execution, and versioning functionality here.

/// Frozen Cypher compatibility profile implemented by the first Lithograph release.
pub const CYPHER_PROFILE: &str = "CY25-2026.08";
