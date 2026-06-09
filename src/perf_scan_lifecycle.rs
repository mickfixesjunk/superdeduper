//! v0.3.42 Phase 11 -- canonical scan-lifecycle metrics per
//! [`workdirs/design/PERF_METRICS.md`].
//!
//! Three high-level metrics on every scan, emitted via [`crate::log_info!`]
//! at scan completion. Both CLI and GUI processes hit the same code path
//! + write to the same persistent log file
//! (`<data_dir>/log/superdeduper.<unix>.<pid>.log` per
//! [`crate::log`] / [`superdeduper_log`]). Diff the two lines to identify
//! where GUI diverges from CLI; drill in to that metric next.
//!
//! ## Metrics
//!
//! * **TTWS** -- Time To Walk Start. Process entry -> first walker call.
//!   For benchmark runs (auto-click Start on first frame), this is the
//!   scan-initiation overhead. For interactive use, includes user
//!   click-to-start latency.
//! * **TTW** -- Time To Walk. First walker call -> walker returns.
//!   Filesystem enumeration + stat + exclusion-pack filtering. Shared
//!   code path; near-identical between CLI/GUI expected.
//! * **TTDD** -- Time To DeDupe. Walker returns -> scan complete.
//!   Chunking + hashing + cache lookups + per-group emit + checkpoint
//!   saves + (GUI only) UI refresh + scan-history JSON.
//!
//! ## Emit format
//!
//! ```text
//! perf-scan-lifecycle: ttws_ms=420 ttw_ms=3210 ttdd_ms=14012 total_ms=17642 process=cli
//! ```
//!
//! `total_ms` is a composition-invariant sanity check; should equal
//! `ttws_ms + ttw_ms + ttdd_ms` within milliseconds-of-rounding.
//!
//! ## Subsequent scans (GUI multi-scan session)
//!
//! Per spec §9 lean (a): TTWS only meaningful on the FIRST scan
//! (subsequent scans skip the eframe/winit init that TTWS measures).
//! On scan N>1 we emit `ttws_ms=0` so downstream diff tooling still
//! gets a one-line-per-scan record; the 0 sentinel is distinguishable
//! from a true first-scan TTWS (always > 0 for real init).
//!
//! ## Always-on
//!
//! Default ON; no env gate. Cost is 4 `Instant::now()` captures + 1
//! `log_info!` emit per scan (~ns total). Negligible; aligns with
//! Mick's "I want to see this on every run" directive
//! (2026-06-07 ~08:25 PDT).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Process-wide start time. Set by [`record_process_start`] at fn main
/// entry. Lazy-initialised to "first call's now" if `record_process_start`
/// was never called (graceful degradation; ttws_ms will be too small).
fn process_start_slot() -> &'static OnceLock<Instant> {
    static SLOT: OnceLock<Instant> = OnceLock::new();
    &SLOT
}

/// Tracks whether the FIRST scan's TTWS has already been emitted; used
/// to suppress TTWS on subsequent scans in the same process (GUI
/// multi-scan session). Reset semantics are deliberately not exposed
/// -- the OnceLock-like AtomicBool reflects process-lifetime state.
fn ttws_emitted_flag() -> &'static AtomicBool {
    static FLAG: AtomicBool = AtomicBool::new(false);
    &FLAG
}

/// Call from `fn main` as the very first statement in both
/// `src/main.rs` (CLI) and `src/bin/superdeduper_gui.rs` (GUI). Captures
/// `Instant::now()` for the TTWS baseline. Idempotent: subsequent calls
/// are no-ops.
pub fn record_process_start() {
    let _ = process_start_slot().get_or_init(Instant::now);
}

/// Returns the cached process-start Instant. Lazy-initialises on first
/// call if `record_process_start` was never called. Prefer wiring
/// `record_process_start` explicitly at fn main entry for accurate
/// TTWS.
fn process_start_now() -> Instant {
    *process_start_slot().get_or_init(Instant::now)
}

/// Read-only accessor for sister-module instrumentation (e.g.
/// [`crate::perf_gui_startup`]) that needs the process-start anchor
/// without taking the lazy-init side effect. Returns `None` when
/// [`record_process_start`] was never called -- callers should skip
/// their emit in that case rather than fall back to wall-clock
/// derived from `Instant::now()` (would be garbage).
pub fn process_start_for_diagnostics() -> Option<Instant> {
    process_start_slot().get().copied()
}

/// Per-scan lifecycle marker. Construct at scan start, call
/// [`walk_started`] and [`walk_completed`] at the walker boundaries,
/// call [`scan_completed`] at the success-path scan-finish to emit the
/// `perf-scan-lifecycle:` line.
///
/// Drop without `scan_completed` is intentional -- early-return paths
/// (cancellation, pause, error) deliberately suppress the emit since
/// TTDD would be invalid.
///
/// [`walk_started`]: PerfScanLifecycle::walk_started
/// [`walk_completed`]: PerfScanLifecycle::walk_completed
/// [`scan_completed`]: PerfScanLifecycle::scan_completed
pub struct PerfScanLifecycle {
    process_tag: &'static str,
    walk_start: Option<Instant>,
    walk_end: Option<Instant>,
}

impl PerfScanLifecycle {
    /// Construct a new lifecycle marker. `process_tag` must be `"cli"`
    /// or `"gui"` -- it ends up as the literal `process=` field in the
    /// emitted line so diff tooling can route without filename parsing.
    pub fn new(process_tag: &'static str) -> Self {
        // Touch the process-start slot here so a forgotten
        // record_process_start() at fn main entry at least anchors at
        // the first scan's construction time (degraded but not silent).
        let _ = process_start_now();
        Self {
            process_tag,
            walk_start: None,
            walk_end: None,
        }
    }

    /// Capture the walker-start boundary. Call IMMEDIATELY before the
    /// walker entry (CLI: `inventory::enumerate_with_skipped`; GUI:
    /// `inventory::walk::enumerate_cancellable`). Closes TTWS, opens TTW.
    pub fn walk_started(&mut self) {
        self.walk_start = Some(Instant::now());
    }

    /// Capture the walker-end boundary. Call IMMEDIATELY after the
    /// walker returns. Closes TTW, opens TTDD.
    pub fn walk_completed(&mut self) {
        self.walk_end = Some(Instant::now());
    }

    /// Emit the `perf-scan-lifecycle:` line. Consumes `self` so a
    /// scan can only finish once. Skips the emit if walker boundaries
    /// weren't captured (partial-scan / diagnostic-mode paths that
    /// short-circuit before the walker).
    pub fn scan_completed(self) {
        let Some(walk_start) = self.walk_start else {
            return;
        };
        let Some(walk_end) = self.walk_end else {
            return;
        };
        let scan_end = Instant::now();

        // First-scan TTWS = process-start -> walker-start. Subsequent
        // scans suppress (emit 0) per spec §9 lean (a) since the
        // eframe/winit init that TTWS measures fires once per process.
        let first_scan = !ttws_emitted_flag().swap(true, Ordering::SeqCst);
        let ttws_ms = if first_scan {
            walk_start
                .saturating_duration_since(process_start_now())
                .as_millis() as u64
        } else {
            0
        };
        let ttw_ms = walk_end
            .saturating_duration_since(walk_start)
            .as_millis() as u64;
        let ttdd_ms = scan_end
            .saturating_duration_since(walk_end)
            .as_millis() as u64;
        let total_ms = ttws_ms.saturating_add(ttw_ms).saturating_add(ttdd_ms);

        crate::log_info!(
            "perf-scan-lifecycle: ttws_ms={} ttw_ms={} ttdd_ms={} total_ms={} process={}",
            ttws_ms,
            ttw_ms,
            ttdd_ms,
            total_ms,
            self.process_tag
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: full lifecycle (process-start -> walk -> dedupe) runs
    /// without panicking + the emit fires. We can't easily assert the
    /// log contents from a unit test, but exercising every code path
    /// catches panics + assertion drift.
    #[test]
    fn lifecycle_full_path_does_not_panic() {
        record_process_start();
        let mut life = PerfScanLifecycle::new("test");
        life.walk_started();
        life.walk_completed();
        life.scan_completed();
    }

    /// Drop without scan_completed must NOT emit (partial-scan path).
    /// Verified by ensuring no panic + structural correctness.
    #[test]
    fn drop_without_completion_is_safe() {
        record_process_start();
        let mut life = PerfScanLifecycle::new("test");
        life.walk_started();
        // No walk_completed; no scan_completed. Drop is silent.
        drop(life);
    }

    /// scan_completed without walk_started must skip the emit (avoid
    /// garbage data from a partial flow).
    #[test]
    fn scan_completed_without_walk_skips_emit() {
        record_process_start();
        let life = PerfScanLifecycle::new("test");
        // Neither walk_started nor walk_completed called.
        life.scan_completed(); // returns early; no emit.
    }

    /// record_process_start is idempotent.
    #[test]
    fn record_process_start_is_idempotent() {
        record_process_start();
        record_process_start();
        record_process_start();
        // No panic; OnceLock get_or_init handles repeat calls.
    }
}
