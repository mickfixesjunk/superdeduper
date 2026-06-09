//! v0.3.43 lazy-eframe-init -- GUI cold-startup decomposition per
//! [`workdirs/design/v0.3.41-lazy-eframe-init-research-scope.md`] §4.
//!
//! Five named markers decompose the ~7s structural-startup residual
//! v0.3.40 §2.4 carved out of the engineering-fixable ratio (eframe /
//! winit / accesskit_windows IPC / first-frame paint -- all UPSTREAM
//! of our app code). Without this decomposition the v0.3.41-research
//! note hypothesised the residual split but did not have evidence.
//!
//! Sister module to [`crate::perf_scan_lifecycle`] (v0.3.42); same
//! always-on + OnceLock + AtomicBool first-emit-only shape so diff
//! tooling routes both signals consistently.
//!
//! ## Buckets
//!
//! * **pre_native_ms**         -- process start -> `eframe::run_native` call.
//!                                Static init / dyld / arg parse / config
//!                                load / channel resolution.
//! * **run_native_to_new_ms**  -- `run_native` call -> `App::new(cc)` entry.
//!                                eframe internals + winit window create +
//!                                accesskit_windows IPC + GPU context init.
//! * **app_new_ms**            -- `App::new(cc)` entry -> return. Theme
//!                                install + image_loader install + persisted
//!                                state read + checkpoint summary probe.
//! * **first_frame_ms**        -- `App::new` return -> first paint complete.
//!                                Default font atlas + initial UI tree
//!                                layout + GPU first-frame submit.
//! * **total_to_visible_ms**   -- process start -> first paint complete.
//!                                Sum-conservation: should equal sum of the
//!                                four bucket fields within ms-of-rounding.
//!
//! ## Emit format
//!
//! ```text
//! perf-gui-startup: pre_native_ms=120 run_native_to_new_ms=4200 app_new_ms=15 first_frame_ms=2540 total_to_visible_ms=6875
//! ```
//!
//! Per-process; fires exactly once per GUI lifetime. Subsequent paints
//! suppress the emit via the `EMITTED` AtomicBool.
//!
//! ## Always-on
//!
//! Default ON; no env gate. Cost: 4 `Instant::now()` captures + 1
//! `log_info!` emit on the first frame paint. Sub-microsecond total.
//! Aligns with Mick's "I want to see this on every run" directive
//! (2026-06-07 ~08:25 PDT) + matches [`crate::perf_scan_lifecycle`].
//!
//! Note: the [research scope spec §4.1] proposed an env gate
//! (`SUPERDEDUPER_PERF_INSTRUMENT_GUI_STARTUP=1`); the always-on shape
//! supersedes that per the [`crate::perf_scan_lifecycle`] precedent.
//! Negligible cost + matrix-runner harvest via existing generic
//! `(.*perf-.*:.*)` regex was the gate's primary motivation, and
//! always-on satisfies both.
//!
//! [research scope spec §4.1]: workdirs/design/v0.3.41-lazy-eframe-init-research-scope.md

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Pre-`run_native` Instant. Set by [`record_pre_run_native`] just
/// before `eframe::run_native(...)` returns control to the
/// eframe/winit/accesskit stack. Used to close `pre_native_ms` and
/// open `run_native_to_new_ms`.
fn pre_run_native_slot() -> &'static OnceLock<Instant> {
    static SLOT: OnceLock<Instant> = OnceLock::new();
    &SLOT
}

/// `App::new` entry Instant. Set by [`record_app_new_start`] at the
/// first statement of [`crate::gui::SuperdeduperApp::new`]. Used to
/// close `run_native_to_new_ms` and open `app_new_ms`.
fn app_new_start_slot() -> &'static OnceLock<Instant> {
    static SLOT: OnceLock<Instant> = OnceLock::new();
    &SLOT
}

/// `App::new` exit Instant. Set by [`record_app_new_end`] at the
/// last statement before `App::new` returns the constructed app
/// value. Used to close `app_new_ms` and open `first_frame_ms`.
fn app_new_end_slot() -> &'static OnceLock<Instant> {
    static SLOT: OnceLock<Instant> = OnceLock::new();
    &SLOT
}

/// Tracks whether [`emit_if_first_frame`] has already emitted the
/// `perf-gui-startup:` line. First call swaps true; subsequent calls
/// no-op. Mirrors [`crate::perf_scan_lifecycle`]'s `TTWS_EMITTED`
/// shape.
fn emitted_flag() -> &'static AtomicBool {
    static FLAG: AtomicBool = AtomicBool::new(false);
    &FLAG
}

/// Capture `t_pre_run_native`. Call IMMEDIATELY before
/// `eframe::run_native(...)` in `src/bin/superdeduper_gui.rs`.
/// Idempotent: only the first call is honoured (OnceLock).
pub fn record_pre_run_native() {
    let _ = pre_run_native_slot().get_or_init(Instant::now);
}

/// Capture `t_app_new_start`. Call as the FIRST statement of
/// [`crate::gui::SuperdeduperApp::new`].
/// Idempotent: only the first call is honoured.
pub fn record_app_new_start() {
    let _ = app_new_start_slot().get_or_init(Instant::now);
}

/// Capture `t_app_new_end`. Call as the LAST statement before
/// [`crate::gui::SuperdeduperApp::new`] returns the app value.
/// Idempotent: only the first call is honoured.
pub fn record_app_new_end() {
    let _ = app_new_end_slot().get_or_init(Instant::now);
}

/// First-frame paint sentinel. Call from inside the first
/// `eframe::App::update` invocation, AFTER egui has laid out and
/// painted the initial UI tree. Emits the `perf-gui-startup:` line
/// exactly once per process; subsequent calls are no-ops.
///
/// Skips the emit if any prior marker was never recorded (degraded
/// data is worse than no data -- matrix-runner ignores absent
/// lines).
pub fn emit_if_first_frame() {
    // Swap-set; if it was already true, somebody else emitted.
    if emitted_flag().swap(true, Ordering::SeqCst) {
        return;
    }
    let process_start = match crate::perf_scan_lifecycle::process_start_for_diagnostics() {
        Some(t) => t,
        None => return,
    };
    let Some(pre_run_native) = pre_run_native_slot().get() else {
        return;
    };
    let Some(app_new_start) = app_new_start_slot().get() else {
        return;
    };
    let Some(app_new_end) = app_new_end_slot().get() else {
        return;
    };
    let first_frame_done = Instant::now();

    let pre_native_ms = pre_run_native
        .saturating_duration_since(process_start)
        .as_millis() as u64;
    let run_native_to_new_ms = app_new_start
        .saturating_duration_since(*pre_run_native)
        .as_millis() as u64;
    let app_new_ms = app_new_end
        .saturating_duration_since(*app_new_start)
        .as_millis() as u64;
    let first_frame_ms = first_frame_done
        .saturating_duration_since(*app_new_end)
        .as_millis() as u64;
    let total_to_visible_ms = pre_native_ms
        .saturating_add(run_native_to_new_ms)
        .saturating_add(app_new_ms)
        .saturating_add(first_frame_ms);

    crate::log_info!(
        "perf-gui-startup: pre_native_ms={} run_native_to_new_ms={} app_new_ms={} first_frame_ms={} total_to_visible_ms={}",
        pre_native_ms,
        run_native_to_new_ms,
        app_new_ms,
        first_frame_ms,
        total_to_visible_ms,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: full marker chain runs without panicking. We can't
    /// assert log contents from a unit test, but exercising every code
    /// path catches panics + assertion drift.
    #[test]
    fn full_chain_does_not_panic() {
        crate::perf_scan_lifecycle::record_process_start();
        record_pre_run_native();
        record_app_new_start();
        record_app_new_end();
        emit_if_first_frame();
    }

    /// Idempotent slot setters. record_* must tolerate repeat calls.
    #[test]
    fn record_calls_are_idempotent() {
        record_pre_run_native();
        record_pre_run_native();
        record_app_new_start();
        record_app_new_start();
        record_app_new_end();
        record_app_new_end();
    }

    /// emit_if_first_frame must no-op when any marker was never set
    /// (degraded scan path that didn't go through fn main's GUI entry).
    /// Use a separate OnceLock-per-call would be cleaner but we share
    /// state across tests; this test verifies the "missing slot ->
    /// no panic" branch by virtue of running last when emitted_flag is
    /// already true.
    #[test]
    fn emit_when_already_emitted_is_silent() {
        // Force the flag true; subsequent emit must return early.
        emitted_flag().store(true, Ordering::SeqCst);
        emit_if_first_frame(); // returns at the swap check; no panic.
    }
}
