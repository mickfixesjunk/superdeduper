//! superdeduper — the fastest duplicate file finder for Windows / NTFS.
//!
//! This crate is split into focused modules that mirror the five-stage
//! pipeline described in the project spec:
//!
//! 1. [`inventory`] — Stage 1: MFT enumeration (with a directory-walk fallback)
//!    that produces a flat list of every file under the requested roots.
//! 2. [`pipeline::grouping`] — Stage 2: group by size, discard singletons.
//! 3. [`pipeline::layout`] — Stage 3: resolve physical extents, detect
//!    hardlinks and reflinks, compute the LCN sort key.
//! 4. [`pipeline::hash`] — Stage 4: progressive Tier 0–3 hashing.
//! 5. [`pipeline::confirm`] — Stage 5: optional paranoid byte compare and
//!    final output formatting.
//!
//! The [`winapi_wrappers`] module hides every `unsafe` FFI call behind a safe
//! Rust API. No `unsafe` code is permitted elsewhere in the crate.

pub mod cache;
pub mod cli;
pub mod config;
pub mod dedupe;
pub mod diagnose;
pub mod error;
pub mod telemetry;
pub mod inventory;
pub mod keep;
pub mod output;
pub mod pipeline;
pub mod winapi_wrappers;

#[cfg(feature = "gui")]
pub mod gui;

// G-track (leaderboards + achievements + canonical-bench).
// Gated behind the `telemetry` Cargo feature so distro / paranoid
// builds can ship without the network + crypto stack.
#[cfg(feature = "telemetry")]
pub mod leaderboard;

pub use error::{Error, Result};
