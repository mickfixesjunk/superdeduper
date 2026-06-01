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

use std::path::Path;
use superdeduper_bench_iface::{
    BenchContext, BenchError, BenchExecutor, BenchOutcome, DebugDedupDiffReport, InstallKey,
    SubmissionExecutor, SubmissionInputs, SubmitOutcome,
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
    fn run_bench(
        &self,
        _ctx: BenchContext,
        _progress: &mut dyn FnMut(&str),
        _cancel: &dyn Fn() -> bool,
    ) -> Result<BenchOutcome, BenchError> {
        Err(BenchError::Unavailable)
    }

    fn debug_dedup_diff(
        &self,
        _corpus_dir: &Path,
    ) -> Result<DebugDedupDiffReport, BenchError> {
        Err(BenchError::Unavailable)
    }
}

impl SubmissionExecutor for BenchReal {
    fn submit_recorded(
        &self,
        _inputs: SubmissionInputs,
        _install_id: &str,
        _install_key: &InstallKey,
    ) -> Result<SubmitOutcome, BenchError> {
        Err(BenchError::Unavailable)
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

    #[test]
    fn phase_1_run_bench_returns_unavailable() {
        let r = BenchReal::new();
        let ctx = BenchContext {
            install_id: "id".into(),
            corpus_version: "cv".into(),
            tier: "quick".into(),
            lane: None,
        };
        let mut progress = |_: &str| {};
        let cancel = || false;
        let res = r.run_bench(ctx, &mut progress, &cancel);
        assert!(matches!(res, Err(BenchError::Unavailable)));
    }
}
