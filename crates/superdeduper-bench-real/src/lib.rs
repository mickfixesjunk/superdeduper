//! superdeduper-bench-real
//!
//! Phase 0 trait extraction DEFAULT IMPL. Holds the real bench-flow +
//! submission logic moved out of the engine binary's `src/leaderboard/`.
//! Implements [`BenchExecutor`] + [`SubmissionExecutor`] from the iface
//! crate so engine call sites can reach the bench code without knowing
//! how the bench is implemented.
//!
//! ## Phase 1 (this commit, 2026-05-31)
//!
//! - Crate scaffold + workspace member registered.
//! - [`d7_probe`] module moved here from `src/leaderboard/d7_probe.rs`
//!   (self-contained; blake3-only deps; no external engine callers).
//!   Re-exported back to engine via `pub use` from `leaderboard::mod.rs`
//!   so any future engine consumer of the D7 calibration math reaches it
//!   at the original path.
//! - [`BenchReal`] struct implements both iface traits with method bodies
//!   returning `Err(BenchError::Unavailable)`. Validates the trait surface
//!   compiles against the workspace dep graph; real method bodies land in
//!   Phase 2 when the remaining 5 modules cross the cut-line.
//!
//! ## Phase 2 (post-launch)
//!
//! - Move `bench_run`, `bench_client`, `bench`, `bench_corpus`,
//!   `submission` (HTTP path) modules across the cut-line.
//! - Replace `Err(Unavailable)` stub bodies with real delegations to the
//!   moved helpers.
//! - Engine call-site rewrites (~50+ sites; see
//!   `docs/phase-0-p0d-move-plan.md` §3).
//!
//! See `docs/phase-0-p0d-move-plan.md` for the full move catalog.

pub mod bench;
pub mod bench_client;
pub mod bench_corpus;
pub mod bench_run;
pub mod d7_probe;
pub mod submission_http;

use std::path::Path;
use superdeduper_bench_iface::{
    BenchContext, BenchError, BenchExecutor, BenchOutcome, DebugDedupDiffReport,
    HardwareFingerprint, InstallKey, SubmissionExecutor, SubmissionInputs, SubmitOutcome,
};

/// Default [`BenchExecutor`] + [`SubmissionExecutor`] implementation.
///
/// Phase 1 (2026-05-31): all methods return
/// `Err(BenchError::Unavailable)`. Validates the trait surface compiles
/// against the workspace dep graph. Phase 2 (post-launch) replaces the
/// stub bodies with real delegations to the moved bench modules.
///
/// Constructible without state today; future Phase 2 wiring may add
/// fields (HTTP client handle, retry policy, etc.). Callers should always
/// go through the [`new`](Self::new) constructor so a default-field
/// extension stays backwards-compatible.
#[derive(Default)]
pub struct BenchReal {
    _phase_2_state: (),
}

impl BenchReal {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BenchExecutor for BenchReal {
    /// Phase 3 v0.3.21 + cancellation fix v0.3.24 (2026-06-01):
    /// REAL implementation. Delegates to `crate::bench_run::run` which
    /// takes a `&AtomicBool` for cancel-poll; the trait surface uses a
    /// `Fn() -> bool + Send + Sync` callback so callers can drive
    /// cancellation from arbitrary state. The v0.3.21 implementation
    /// sampled `cancel()` once at entry which left mid-run cancel
    /// broken (codex-review found, design URGENT 2026-06-01 22:23Z).
    /// This commit bridges the two by spawning a scoped propagator
    /// thread that polls `cancel()` every ~100ms for the duration of
    /// the bench run + forwards into the AtomicBool. Propagator exits
    /// the moment cancel fires OR the bench run returns (done_flag).
    fn run_bench(
        &self,
        ctx: BenchContext,
        progress: &mut (dyn FnMut(&str) + Send),
        cancel: &(dyn Fn() -> bool + Send + Sync),
        submit_fn: Option<&mut dyn FnMut(&SubmissionInputs) -> SubmitOutcome>,
        hardware_detect: &dyn Fn(Option<&Path>) -> HardwareFingerprint,
    ) -> Result<BenchOutcome, BenchError> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let cancel_flag = AtomicBool::new(false);
        let done_flag = AtomicBool::new(false);

        let result = std::thread::scope(|s| {
            // Propagator: bridge the closure-based cancel surface to
            // the AtomicBool the moved bench_run::run path watches.
            // Poll every 100ms; the bench loop checks its AtomicBool
            // between stages + inside long-running streaming reads
            // (CancelReader, dedup workers) so 100ms is fine-grained
            // enough that a GUI cancel feels instant.
            s.spawn(|| {
                while !done_flag.load(Ordering::Relaxed) {
                    if cancel() {
                        cancel_flag.store(true, Ordering::Relaxed);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });

            let r = crate::bench_run::run(
                &ctx.install_id,
                &ctx.install_key,
                &ctx.server_url,
                &ctx.corpus_version,
                &ctx.tier,
                ctx.workroot.as_deref(),
                ctx.fresh,
                &cancel_flag,
                progress,
                ctx.lane.as_deref(),
                submit_fn,
                hardware_detect,
            );
            // Tell the propagator to exit -- bench is done, no need
            // to keep polling cancel(). Scope blocks here until the
            // thread observes done_flag and exits cleanly.
            done_flag.store(true, Ordering::Relaxed);
            r
        });

        result.map_err(|e| {
            // Distinguish Cancelled (clean abort) from other failures.
            if e.downcast_ref::<crate::bench_run::Cancelled>().is_some() {
                BenchError::Cancelled
            } else if e.downcast_ref::<std::io::Error>().is_some() {
                BenchError::CorpusIo(format!("{e:#}"))
            } else {
                // Network / HTTP / parse errors arrive as anyhow chains
                // from ureq + serde_json failures; surface verbatim.
                BenchError::Network(format!("{e:#}"))
            }
        })
    }

    /// Phase 2-B v0.3.20 (2026-06-01): REAL implementation. Delegates to
    /// `bench_run::debug_dedup_diff` (moved here in 16d13b9). The
    /// `anyhow::Result` -> `Result<_, BenchError>` mapping classifies IO
    /// errors as `CorpusIo` and everything else as `Internal` (the
    /// callers `sd debug dedup-diff` + the GUI diagnostic surface render
    /// the message verbatim either way; the variant just steers the
    /// telemetry channel).
    fn debug_dedup_diff(
        &self,
        corpus_dir: &Path,
    ) -> Result<DebugDedupDiffReport, BenchError> {
        crate::bench_run::debug_dedup_diff(corpus_dir).map_err(|e| {
            if e.downcast_ref::<std::io::Error>().is_some() {
                BenchError::CorpusIo(format!("{e:#}"))
            } else {
                BenchError::Internal(format!("{e:#}"))
            }
        })
    }
}

impl SubmissionExecutor for BenchReal {
    /// Phase 3 v0.3.21 (2026-06-01): REAL implementation. The expanded
    /// trait sig now includes `server_url`; delegates to
    /// `crate::submission_http::submit_inner`. The bench-real path
    /// returns `SubmitOutcome` directly (no anyhow/Err) so the trait's
    /// `Result<_, BenchError>` always reports `Ok(outcome)` -- callers
    /// inspect the variant (Rejected / Transient / etc.) for failure
    /// classification.
    fn submit_recorded(
        &self,
        inputs: SubmissionInputs,
        server_url: &str,
        install_id: &str,
        install_key: &InstallKey,
    ) -> Result<SubmitOutcome, BenchError> {
        Ok(crate::submission_http::submit_inner(
            server_url,
            install_id,
            &install_key.0,
            &inputs,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_real_constructs() {
        let _ = BenchReal::new();
    }

    #[test]
    fn bench_real_is_dyn_safe_for_bench_executor() {
        let _: Box<dyn BenchExecutor> = Box::new(BenchReal::new());
    }

    #[test]
    fn bench_real_is_dyn_safe_for_submission_executor() {
        let _: Box<dyn SubmissionExecutor> = Box::new(BenchReal::new());
    }

    // Phase 3 v0.3.21 (2026-06-01): the Phase-1 `phase_1_run_bench_returns_unavailable`
    // unit test was removed -- BenchReal::run_bench now has a real body that
    // hits `POST /bench/start`, which a unit test should not exercise. The
    // real-body path is covered by E2E bench-me integration runs (CLI + GUI).
    #[test]
    fn run_bench_dyn_safe_after_phase_3_real_body() {
        // Sanity: the expanded trait sig is still dyn-safe; this keeps the
        // earlier dyn-safety assertion meaningful post-Phase-3.
        let _: Box<dyn BenchExecutor> = Box::new(BenchReal::new());
    }
}
