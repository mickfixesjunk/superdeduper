//! #41 — Background resubmit worker for scan-history rows.
//!
//! The History panel's Resubmit button hands a `scan_id` to
//! [`request_resubmit`]. The worker thread loads the row, replays
//! the recorded canonical payload against the recorded channel's
//! server URL, signs with the install's CURRENT key, and updates
//! the row's `submission_state` based on the outcome.
//!
//! Only one resubmit may be in flight process-wide. Subsequent
//! clicks while a resubmit is in flight no-op (the panel uses
//! [`in_flight_scan_id`] to grey the button).
//!
//! The render loop polls [`drain_outcome`] each frame to surface
//! the last finished resubmit's outcome (success / failure / cross-
//! install-rejection).

#![cfg(all(feature = "gui", feature = "telemetry"))]

use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::leaderboard::submission::SubmitOutcome;
use crate::scan_history::{self, SubmissionState};

/// Slot for the currently-in-flight resubmit, by scan_id. `Some(id)`
/// while the worker is running; back to `None` once it finishes.
fn in_flight_slot() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Slot for the most recent finished resubmit. Drained by the
/// History panel each frame so the outcome surfaces inline.
/// `(scan_id, outcome)`.
fn last_outcome_slot() -> &'static Mutex<Option<(String, SubmitOutcome)>> {
    static SLOT: OnceLock<Mutex<Option<(String, SubmitOutcome)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Is there a resubmit running right now? Used by the History panel
/// to grey out Resubmit buttons on OTHER rows while one is active.
pub fn in_flight_scan_id() -> Option<String> {
    in_flight_slot().lock().clone()
}

/// Drain the most recent finished resubmit's outcome. Returns
/// `Some(_)` exactly once per resubmit, then `None` until the next
/// one completes.
pub fn drain_outcome() -> Option<(String, SubmitOutcome)> {
    last_outcome_slot().lock().take()
}

/// Start a resubmit for `scan_id` in a background thread. Returns
/// `Err` if the row can't be resubmitted right now (already in
/// flight; record missing; no payload captured; cross-channel;
/// install state unavailable). On `Ok`, the worker handles the
/// POST + state update; observers should poll [`drain_outcome`].
pub fn request_resubmit(scan_id: &str) -> Result<(), String> {
    {
        let slot = in_flight_slot().lock();
        if slot.is_some() {
            return Err("another resubmit is already in flight".into());
        }
    }

    let record = scan_history::load(scan_id)
        .map_err(|e| format!("history read failed: {e}"))?
        .ok_or_else(|| "scan history row not found".to_string())?;

    let payload = record
        .submission_payload
        .clone()
        .ok_or_else(|| "this row has no captured submission payload (older scan?)".to_string())?;
    let built_with_install_id = record
        .built_with_install_id
        .clone()
        .ok_or_else(|| "this row has no captured install_id".to_string())?;
    let submission_channel = record
        .submission_channel
        .clone()
        .or_else(|| Some(record.channel.clone()))
        .ok_or_else(|| "this row has no channel recorded".to_string())?;

    // Resolve recorded channel → server URL. Channel-aware resubmit
    // routes the POST against the channel the scan was captured on,
    // NOT the live channel. Cross-channel resubmit is allowed at
    // this level (the user explicitly clicked the row); the server
    // distinguishes by the install_id's channel binding.
    let channel = submission_channel
        .parse::<crate::channel::Channel>()
        .map_err(|e| format!("unparseable submission_channel: {e}"))?;
    let server_url = crate::channel::server_url_for(channel).to_string();

    let install_state = crate::leaderboard::install::load_for(channel)
        .map_err(|e| format!("install state read failed: {e}"))?
        .ok_or_else(|| {
            format!(
                "no install registered on channel `{channel}` — \
                 run `superdeduper register --channel {channel}` first"
            )
        })?;

    // Claim the in-flight slot so a concurrent click no-ops.
    *in_flight_slot().lock() = Some(scan_id.to_string());

    let scan_id_for_thread = scan_id.to_string();
    std::thread::Builder::new()
        .name("superdeduper-resubmit".into())
        .spawn(move || {
            let outcome = crate::leaderboard::submission::submit_recorded_payload(
                &install_state,
                &payload,
                &built_with_install_id,
                &server_url,
            );
            // Archive every attempt under the same path the live
            // submission flow uses; gives the user a permanent
            // local record across both surfaces.
            let install_id_owned = install_state.install_id.clone();
            // archive_attempt expects SubmissionInputs (live-submit
            // shape). We don't have that for replay; skip the local
            // archive on the resubmit path. The History row itself
            // is the durable trail.
            let new_state = match &outcome {
                SubmitOutcome::Accepted { .. } | SubmitOutcome::DuplicateNoChange => {
                    SubmissionState::Submitted
                }
                SubmitOutcome::Rejected { .. } | SubmitOutcome::FlaggedForReview { .. } => {
                    SubmissionState::Failed
                }
                SubmitOutcome::Transient { .. } => {
                    // #117 — after MAX_RESUBMIT_ATTEMPTS transient failures,
                    // give up and mark Failed so the launch-time modal stops
                    // nagging. User can manually re-try from the History tab.
                    let prior_attempts = scan_history::load(&scan_id_for_thread)
                        .ok()
                        .flatten()
                        .map(|r| r.attempt_count)
                        .unwrap_or(0);
                    scan_history::transient_outcome_state(prior_attempts)
                }
            };
            if let Err(e) =
                scan_history::update_submission_state(&scan_id_for_thread, new_state, true)
            {
                tracing::warn!(
                    scan_id = %scan_id_for_thread,
                    error = %e,
                    "resubmit: state update failed (row remains in prior state)",
                );
            }
            // Surface result + clear in-flight slot. Ordering:
            // drop in-flight FIRST so the panel sees the outcome
            // for-real-this-time on the same frame the worker
            // finishes.
            let _ = install_id_owned;
            *last_outcome_slot().lock() = Some((scan_id_for_thread.clone(), outcome));
            *in_flight_slot().lock() = None;
        })
        .map_err(|e| {
            *in_flight_slot().lock() = None;
            format!("spawn resubmit worker: {e}")
        })?;

    Ok(())
}
