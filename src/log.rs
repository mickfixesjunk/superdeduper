//! Phase 0 (2026-05-31): the implementation moved to the `superdeduper-log`
//! leaf crate so the future bench-real / bench-stub crates can use the
//! `log_info!` / `log_warn!` / `log_err!` macros without depending on the
//! entire engine binary.
//!
//! This module is now a thin re-export shim — every existing call site that
//! refers to `crate::log::write_line(...)` continues to work unchanged. The
//! `log_info!` / `log_warn!` / `log_err!` macros are re-exported at the
//! engine crate root via `pub use superdeduper_log::*` in `src/lib.rs` so
//! existing `crate::log_info!(...)` call sites in engine / GUI compile
//! unchanged. Tests for the implementation live with the impl in the leaf
//! crate (`crates/superdeduper-log/src/lib.rs`).
//!
//! Always-on: NOT gated behind any Cargo feature. Per Mick's 2026-05-29
//! PERSIST-logs-always directive, the disk log must work on both
//! `--features telemetry` and `--no-default-features` builds.

pub use superdeduper_log::write_line;
