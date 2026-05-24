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

pub mod action_receipt;
pub mod cache;
pub mod channel;
pub mod cli;
pub mod config;
pub mod dedupe;
pub mod diagnose;
pub mod error;
pub mod exclusions;
pub mod inventory;
pub mod keep;
pub mod output;
pub mod pipeline;
pub mod platform;
pub mod scan_history;
pub mod winapi_wrappers;

#[cfg(feature = "gui")]
pub mod gui;

// G-track (leaderboards + achievements + canonical-bench).
// Gated behind the `telemetry` Cargo feature so distro / paranoid
// builds can ship without the network + crypto stack.
#[cfg(feature = "telemetry")]
pub mod leaderboard;

/// Crate-internal helper so feature-gated callers (gui::live) can
/// compute the corpus signature hash without taking a direct dep on
/// the leaderboard module's path. Mirrors the implementation in
/// `leaderboard::submission`'s payload-build flow but lives at the
/// crate root so engine-side code can reach it without the
/// `leaderboard::` prefix.
#[cfg(feature = "telemetry")]
#[doc(hidden)]
pub fn leaderboard_corpus_sig(sizes: &[u64]) -> String {
    let mut counts: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    for &s in sizes {
        let bucket = match s / 1024 {
            0..=9 => "<10KB",
            10..=99 => "10-100KB",
            100..=999 => "100KB-1MB",
            1_000..=9_999 => "1MB-10MB",
            10_000..=99_999 => "10MB-100MB",
            _ => ">100MB",
        };
        *counts.entry(bucket).or_insert(0) += 1;
    }
    let mut hasher = blake3::Hasher::new();
    for (bucket, count) in counts {
        hasher.update(bucket.as_bytes());
        hasher.update(b":");
        hasher.update(&count.to_le_bytes());
        hasher.update(b"\n");
    }
    format!("sha256:{}", hasher.finalize().to_hex())
}

pub use error::{Error, Result};
