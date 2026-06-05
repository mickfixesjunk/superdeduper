//! Engine↔GUI handoff for the most-recent scan's submission state.
//!
//! Codex-review item 3 (v0.3.27 2026-06-02): extracted from
//! `submission.rs` to give the GUI global slots their own home.
//! The state is small (4 static slots) but conceptually distinct from
//! the HTTP wrappers + the disk-queue/archive paths -- a future
//! cleanup that wanted to lock-free these slots or migrate to a
//! channel-based handoff shouldn't have to wade through 1500 LOC of
//! unrelated HMAC + tar + clock helpers.
//!
//! ## Slots
//!
//! * `PENDING: Mutex<Option<SubmissionInputs>>` -- the engine builds
//!   `SubmissionInputs` at scan-end (see `gui::live::run`) and stashes
//!   it here. The GUI's "Submit run" button reads on click. Single
//!   global is sufficient: only one scan runs at a time + only the
//!   *most recent* is submittable from the UI.
//! * `LAST_OUTCOME: Mutex<Option<SubmitOutcome>>` -- so the UI can
//!   render "rank #4 in Win11 9950X3D / +2 achievements" inline after
//!   the worker thread returns from submit().
//! * `PENDING_SUBMISSION_ID: Mutex<Option<String>>` (#79) -- the
//!   submission_id of the most-recently-Accepted submission. Read by
//!   `action_submission` PATCH client to credit the right row.

#![cfg(feature = "telemetry")]

use parking_lot::Mutex;
use std::sync::OnceLock;

use super::submission::{RankEntry, SubmissionInputs, SubmitOutcome};

static PENDING: OnceLock<Mutex<Option<SubmissionInputs>>> = OnceLock::new();
static LAST_OUTCOME: OnceLock<Mutex<Option<SubmitOutcome>>> = OnceLock::new();
/// #79 — submission_id of the most-recently-Accepted submission
/// for the in-memory scan. Set by the submit worker on
/// `SubmitOutcome::Accepted`; cleared when a fresh scan starts.
/// Read by the post-Go `action_submission` PATCH client so it
/// knows which submission row to credit. Lives as a static slot
/// (not on `SuperdeduperApp`) because the submit worker thread
/// has only `&self` access to the App, which can't mutate fields.
static PENDING_SUBMISSION_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Store the freshest scan's submission inputs. Overwrites any
/// previous value — scan-end semantics: the user can only submit
/// the latest run.
pub fn store_pending(inputs: SubmissionInputs) {
    let m = PENDING.get_or_init(|| Mutex::new(None));
    *m.lock() = Some(inputs);
}

/// Snapshot the pending inputs without removing them. Used by the
/// GUI to decide whether to enable the Submit button + to display
/// the run's stats next to it.
pub fn peek_pending() -> Option<SubmissionInputs> {
    PENDING.get().and_then(|m| m.lock().clone())
}

/// Remove + return the pending inputs. Called by the Submit worker
/// thread; ensures the same payload can't be double-submitted by
/// rapid clicks.
pub fn take_pending() -> Option<SubmissionInputs> {
    PENDING.get().and_then(|m| m.lock().take())
}

pub fn store_last_outcome(o: SubmitOutcome) {
    let m = LAST_OUTCOME.get_or_init(|| Mutex::new(None));
    *m.lock() = Some(o);
}

pub fn peek_last_outcome() -> Option<SubmitOutcome> {
    LAST_OUTCOME.get().and_then(|m| m.lock().clone())
}

pub fn clear_last_outcome() {
    if let Some(m) = LAST_OUTCOME.get() {
        *m.lock() = None;
    }
}

/// #79 — Store the submission_id from the most recent Accepted
/// /api/v1/submit response so the post-Go PATCH credits the
/// right row. Overwrites any previous value (scan-end semantic;
/// a fresh scan supersedes the old submission_id).
pub fn store_pending_submission_id(id: String) {
    let m = PENDING_SUBMISSION_ID.get_or_init(|| Mutex::new(None));
    *m.lock() = Some(id);
}

/// #79 — Read the submission_id stashed by the most recent
/// Accepted submission. `None` ⇒ the user hasn't submitted yet
/// (anonymous flow, or pre-Go state), so PATCH should not fire.
pub fn peek_pending_submission_id() -> Option<String> {
    PENDING_SUBMISSION_ID.get().and_then(|m| m.lock().clone())
}

/// #79 — Wipe the pending submission_id. Called when a fresh
/// scan starts so an action taken AFTER the new scan doesn't
/// credit the OLD scan's submission.
pub fn clear_pending_submission_id() {
    if let Some(m) = PENDING_SUBMISSION_ID.get() {
        *m.lock() = None;
    }
}

/// Merge fresh ranks into the stored Accepted outcome. Called by the
/// ranks-poll worker once the backend's async compute completes.
/// No-op if the stored outcome isn't Accepted — by then the modal
/// has either moved on or the submission failed; either way the
/// ranks are stale.
pub fn update_last_outcome_ranks(fresh: Vec<RankEntry>) {
    let m = match LAST_OUTCOME.get() {
        Some(m) => m,
        None => return,
    };
    let mut guard = m.lock();
    if let Some(SubmitOutcome::Accepted { ranks, .. }) = guard.as_mut() {
        *ranks = fresh;
    }
}
