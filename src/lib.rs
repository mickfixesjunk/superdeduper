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
//! 5. Final output formatting. (The previous Stage 5 `pipeline::confirm`
//!    byte-by-byte verification stub was removed in #131; real
//!    byte-by-byte verification is a v0.3.x feature scope.)
//!
//! The [`winapi_wrappers`] module hides every Windows `unsafe` FFI call
//! behind a safe Rust API. Other `unsafe` blocks exist where the
//! cross-platform syscall surface requires them — e.g. CPUID intrinsics
//! in `leaderboard::hardware`, `libc::statfs` on Linux, the
//! `GetFileInformationByHandleEx` file-id resolution in `dedupe`'s
//! keeper-identity gate — each kept minimal + commented at the call
//! site. #134: the prior claim of "no unsafe elsewhere in the crate" was
//! aspirational and false by grep; the policy is "unsafe is justified at
//! the call site," not "absent."

/// A-home-env-serial -- process-wide serial gate for unit tests that
/// mutate a HOME-equivalent env var (HOME, XDG_DATA_HOME, XDG_CACHE_HOME,
/// LOCALAPPDATA, USERPROFILE). Cargo's parallel test runner schedules
/// every unit test in this crate inside one binary, so two modules'
/// tests both calling `env::set_var("HOME", ...)` race -- observed as
/// #146 (`trash_root_falls_back_to_home_local_share` failed under
/// parallel `cargo test`, passed in isolation). The per-module SERIAL
/// gates that existed before only serialized WITHIN their own module;
/// a test in `platform::linux::trash::tests` racing a test in
/// `scan_history::tests` or `dedupe::tests` was still possible.
///
/// Every HOME-mutating unit test MUST acquire this BEFORE touching the
/// env, AND hold the guard across both the mutation and any subsequent
/// reads that depend on it:
///
/// ```ignore
/// let _g = crate::test_serial::home_env_guard();
/// std::env::set_var("HOME", &fake_home);
/// // ... test body that calls into HOME-reading code ...
/// // restore on exit; _g drops at end of scope.
/// ```
///
/// `parking_lot::Mutex` is poison-tolerant: if a test panics while
/// holding the guard, the next acquirer gets a clean lock rather than
/// `PoisonError`. That matters because test failure shouldn't wedge the
/// rest of the test suite.
#[cfg(test)]
pub(crate) mod test_serial {
    use parking_lot::{Mutex, MutexGuard};

    static HOME_ENV_SERIAL: Mutex<()> = Mutex::new(());

    /// Acquire the shared HOME-env serial gate. Hold the returned
    /// guard for the entire duration of any test body that mutates a
    /// HOME-equivalent env var.
    pub fn home_env_guard() -> MutexGuard<'static, ()> {
        HOME_ENV_SERIAL.lock()
    }
}

pub mod action_receipt;
pub mod cache;
pub mod channel;
pub mod cli;
pub mod config;
pub mod debug;
pub mod dedupe;
pub mod diagnose;
pub mod error;
pub mod exclusions;
pub mod inventory;
pub mod keep;
pub mod log;
pub mod output;
pub mod path_display;
pub mod pipeline;
pub mod platform;
pub mod scan_history;
pub mod schema;
pub mod time;
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
