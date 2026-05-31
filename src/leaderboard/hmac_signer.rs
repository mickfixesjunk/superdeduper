//! Phase 0 (2026-05-31): the implementation moved to the
//! `superdeduper-hmac-signer` leaf crate so the future bench-real /
//! bench-stub crates can depend on it without dragging in the entire
//! engine binary.
//!
//! This module is now a thin re-export shim — every existing callsite
//! (engine + GUI + leaderboard internals) continues to call
//! `crate::leaderboard::hmac_signer::sign(...)` /
//! `::canonical_body(...)` /
//! `::sign_canonical(...)` unchanged. New code should ideally import
//! `superdeduper_hmac_signer` directly to skip the indirection.
//!
//! Original test suite lives with the impl in the leaf crate.

pub use superdeduper_hmac_signer::{canonical_body, sign, sign_canonical};
