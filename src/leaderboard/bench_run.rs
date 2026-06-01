//! T-BENCH-ME shared `--bench-me` / GUI-button orchestration.
//!
//! Phase 2-B body-move 2026-06-01: the 1500+ LOC body moved to
//! `crates/superdeduper-bench-real/src/bench_run.rs`. This file is the
//! engine-side re-export shim so every existing import path
//! (`leaderboard::bench_run::run`, `bench_run::BenchOutcome`,
//! `bench_run::Cancelled`, `bench_run::DebugDedupDiffReport`,
//! `bench_run::debug_dedup_diff`) resolves unchanged via the bench-real
//! crate's public surface.
//!
//! See `crates/superdeduper-bench-real/src/bench_run.rs` for the actual
//! implementation.
#![cfg(feature = "telemetry")]

pub use superdeduper_bench_real::bench_run::*;
