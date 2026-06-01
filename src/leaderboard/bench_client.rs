//! T-BENCH-ME client-side bench primitives.
//!
//! Phase 2-B 2026-06-01: body moved to
//! `crates/superdeduper-bench-real/src/bench_client.rs`. This file is
//! a re-export shim so every existing call site that wrote
//! `leaderboard::bench_client::*` continues to resolve unchanged via
//! the bench-real crate's public surface. The original 1242-LOC body
//! (challenge_hash family + result_digest v1/v2/v3/v3.1 + rep_hash_v3_1
//! + answer_challenge_from_dir family + to_canonical_bench family) all
//! lives behind this pub use.
//!
//! See `crates/superdeduper-bench-real/src/bench_client.rs` for the
//! actual implementation.
#![cfg(feature = "telemetry")]

pub use superdeduper_bench_real::bench_client::*;
