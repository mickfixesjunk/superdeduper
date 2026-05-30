//! #79 Phase 4 — PATCH /api/v1/submit/{id}/actions client.
//!
//! After the user clicks Go on a scan + the worker finishes, the
//! engine submits the action's reclaim bytes to the leaderboard
//! via this client. The endpoint is idempotent UPSERT — engine
//! sends the COMPLETE `actions_taken_summary` map each PATCH
//! attempt (never a delta) and the server uses the latest value
//! as the source of truth. Retry semantics therefore re-send the
//! same payload until it lands or the retry budget is exhausted.
//!
//! Wire contract (locked with web 2026-05-25T19:16Z):
//! * Method: PATCH
//! * URL: `{server_url}/api/v1/submit/{submission_id}/actions`
//! * Headers: `X-Sd-Install-Id`, `X-Sd-Signature` (HMAC over the
//!   canonical body, same as POST /submit)
//! * Body: `{"actions_taken_summary": {"key": value, ...}}` where
//!   `key ∈ LOCKED_ACTION_KEYS` (see `crate::dedupe::LOCKED_ACTION_KEYS`)
//!
//! Response shape:
//! * 200 OK: `{"lifetime": {...}, "newly_granted_achievements": [...]}`
//!   — engine surfaces the newly-granted entries inline in the
//!   summary modal
//! * 401: install rotated / unlinked since the POST that issued
//!   the submission_id; surface "auth lost; sign in again"
//! * 4xx (other): engine bug; surface verbatim for triage
//! * 5xx / network: Transient; caller retries with backoff
//!
//! Retry policy (caller-driven): 500ms → 2s → 5s → 15s; on
//! terminal failure, queue the rollup for retry-on-next-run via
//! the existing submission-archive infrastructure.

#![cfg(feature = "telemetry")]

use std::collections::BTreeMap;

use super::install::InstallState;

/// Build the PATCH body from a populated `actions_taken_summary`
/// map. Kept as a free function so the modal + retry path can
/// also call it without dragging the whole submit closure.
pub fn build_actions_body(actions_taken_summary: &BTreeMap<String, u64>) -> serde_json::Value {
    serde_json::json!({
        "actions_taken_summary": actions_taken_summary,
    })
}

/// Outcome of a single PATCH attempt. The caller (typically the
/// summary-modal driver) interprets these to decide whether to
/// retry, hand off to the submission-archive queue, or render the
/// success state with `newly_granted_achievements`.
#[derive(Debug, Clone)]
pub enum ActionSubmitOutcome {
    /// 200 OK — server accepted + recomputed lifetime totals.
    /// `newly_granted_achievements` may be empty (the action
    /// didn't unlock anything new); when non-empty the modal
    /// surfaces "You unlocked: ..." inline.
    Accepted {
        lifetime_bytes_reclaimed: u64,
        newly_granted_achievements: Vec<String>,
    },
    /// 401 — install rotated / unlinked since the POST that
    /// issued the submission_id. Modal surfaces "auth lost; sign
    /// in again" + offers to re-submit after re-auth.
    Unauthorised(String),
    /// 4xx (other than 401) — engine bug; the action_taken_summary
    /// shape is wrong, the submission_id is stale, or the HMAC
    /// rejected. Surface verbatim for triage; don't retry.
    Rejected { status: u16, reason: String },
    /// 5xx / network / endpoint-not-deployed-yet (very unlikely
    /// for /actions specifically since it's prod-live, but kept
    /// for parity with the POST /submit shape). Caller retries
    /// with backoff; terminal failure routes to submission-archive.
    Transient { reason: String },
}

/// Fire one PATCH attempt. Pure I/O — no retry / no backoff /
/// no persistence. Caller wraps in a retry loop.
pub fn submit_actions(
    state: &InstallState,
    submission_id: &str,
    actions_taken_summary: &BTreeMap<String, u64>,
) -> ActionSubmitOutcome {
    if !state.registered {
        return ActionSubmitOutcome::Rejected {
            status: 0,
            reason: "install not registered".to_string(),
        };
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return ActionSubmitOutcome::Rejected {
                status: 0,
                reason: "install_key_hex malformed".to_string(),
            };
        }
    };
    let payload = build_actions_body(actions_taken_summary);
    let body = super::hmac_signer::canonical_body(&payload);
    let signature = super::hmac_signer::sign(&install_key, &body);
    let url = format!(
        "{}/api/v1/submit/{}/actions",
        state.server_url.trim_end_matches('/'),
        submission_id,
    );
    let response = ureq::patch(&url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Install-Id", &state.install_id)
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(15))
        .send_bytes(&body);
    match response {
        Ok(resp) => parse_ok(resp),
        Err(ureq::Error::Status(401, resp)) => ActionSubmitOutcome::Unauthorised(
            resp.into_string()
                .unwrap_or_else(|e| format!("<read error: {e}>")),
        ),
        Err(ureq::Error::Status(code, resp)) if (400..500).contains(&code) => {
            ActionSubmitOutcome::Rejected {
                status: code,
                reason: resp
                    .into_string()
                    .unwrap_or_else(|e| format!("<read error: {e}>")),
            }
        }
        Err(ureq::Error::Status(code, resp)) => ActionSubmitOutcome::Transient {
            reason: format!(
                "{code}: {}",
                resp.into_string()
                    .unwrap_or_else(|e| format!("<read error: {e}>"))
            ),
        },
        Err(ureq::Error::Transport(t)) => ActionSubmitOutcome::Transient {
            reason: format!("transport: {t}"),
        },
    }
}

fn parse_ok(resp: ureq::Response) -> ActionSubmitOutcome {
    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return ActionSubmitOutcome::Transient {
                reason: format!("200 OK but body parse failed: {e}"),
            };
        }
    };
    let lifetime_bytes_reclaimed = body
        .get("lifetime")
        .and_then(|l| l.get("bytes_reclaimed"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let newly_granted_achievements = body
        .get("newly_granted_achievements")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    // Accept either {"id": "..."} or a bare string.
                    a.get("id")
                        .and_then(|i| i.as_str())
                        .or_else(|| a.as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    ActionSubmitOutcome::Accepted {
        lifetime_bytes_reclaimed,
        newly_granted_achievements,
    }
}

/// Build a single-key `actions_taken_summary` map from a
/// `DedupeActionSummary`. Convenience for the common single-action
/// case (the user clicks Go once with one action type selected).
/// Returns `None` for non-credited actions (SafeRename) — caller
/// skips the PATCH in that case rather than firing an empty map.
pub fn actions_summary_from_dedupe(
    s: &crate::dedupe::DedupeActionSummary,
) -> Option<BTreeMap<String, u64>> {
    let key = s.locked_action_key()?;
    if s.ok_bytes == 0 {
        // Nothing actually reclaimed (every file failed). Skip
        // the PATCH so we don't credit a zero-bytes row that web
        // would treat as a valid-but-empty update.
        return None;
    }
    let mut m = BTreeMap::new();
    m.insert(key.to_string(), s.ok_bytes);
    Some(m)
}

/// Same but for archive (separate type per #80 Bug C). Archive
/// always credits `archived_bytes` regardless of failure
/// reasons — the bucket counters are for the UX modal, not the
/// PATCH payload.
///
/// Gated on `feature = "gui"` because `ArchiveActionSummary` lives
/// in `gui::archive`; CLI-only builds skip the archive worker
/// entirely so the helper is unreachable in that combo.
#[cfg(feature = "gui")]
pub fn actions_summary_from_archive(
    s: &crate::gui::archive::ArchiveActionSummary,
) -> Option<BTreeMap<String, u64>> {
    if s.moved_bytes == 0 {
        return None;
    }
    let mut m = BTreeMap::new();
    m.insert("archived_bytes".to_string(), s.moved_bytes);
    Some(m)
}

// ============================================================
// Status slot + retry-with-backoff worker
// ============================================================
//
// The Phase 3 modal renders a footer that reflects the PATCH
// state in real time: Submitting → Credited / Retrying / Queued.
// State flows through a process-wide OnceLock slot rather than a
// channel because the spawn site is `&self` (the App state can't
// be mutated from the worker thread directly) and the polling
// site (main UI thread) reads each frame.

use parking_lot::Mutex;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub enum ActionSubmissionStatus {
    /// Worker spawned; first PATCH attempt in flight.
    Submitting,
    /// At least one attempt failed; backing off before retry.
    /// `attempt` is 1-indexed (so the 1st retry is attempt 2).
    Retrying { attempt: u32, last_error: String },
    /// PATCH succeeded. Lifetime + newly-granted achievements
    /// surface in the modal.
    Credited {
        lifetime_bytes_reclaimed: u64,
        newly_granted_achievements: Vec<String>,
    },
    /// All retries exhausted (or a non-retryable Rejected /
    /// Unauthorised arrived). v1 surfaces the reason; v2 will
    /// queue the rollup for retry-on-next-run via the existing
    /// submission-archive infra.
    Queued { reason: String },
}

static STATUS: OnceLock<Mutex<Option<ActionSubmissionStatus>>> = OnceLock::new();

pub fn store_status(s: ActionSubmissionStatus) {
    let m = STATUS.get_or_init(|| Mutex::new(None));
    *m.lock() = Some(s);
}

pub fn peek_status() -> Option<ActionSubmissionStatus> {
    STATUS.get().and_then(|m| m.lock().clone())
}

pub fn clear_status() {
    if let Some(m) = STATUS.get() {
        *m.lock() = None;
    }
}

/// Spawn the PATCH submission worker for one Go-action rollup.
/// Retries with the spec'd cadence (500ms → 2s → 5s → 15s) on
/// Transient outcomes; bails immediately on Rejected /
/// Unauthorised. Writes status to the process-wide slot at every
/// state change so the modal polls fresh values each frame.
/// v0.2.39 (E) — fire a 2-phase worker that first POSTs `/api/v1/submit`
/// (silently submitting the scan the user skipped) using the supplied
/// `SubmissionInputs`, then PATCHes `/api/v1/submit/{new_submission_id}/actions`
/// with the action-credit summary. Preserves correct attribution: the credit
/// lands under the scan that produced it, not under some unrelated future
/// submission. Used when `peek_pending_submission_id()` is None at action-fire
/// time but `peek_pending()` still has the scan's SubmissionInputs (i.e., the
/// user clicked Skip in the post-scan modal then performed a dedupe action
/// before closing the session).
///
/// On Phase-1 transient failure: queues the action credit to disk via
/// `pending_actions::append` (drained on the next successful /submit).
/// On Phase-1 terminal failure (Rejected / Unauthorised): same — queue to
/// disk so a re-auth + re-submit recovers the credit. On Phase-2 retry-
/// exhausted: existing `spawn_submit_worker` writes ActionSubmissionStatus::
/// Queued; the disk fallback covers the next-launch drain.
pub fn spawn_auto_submit_then_patch(
    state: InstallState,
    inputs: super::submission::SubmissionInputs,
    actions_taken_summary: BTreeMap<String, u64>,
) {
    store_status(ActionSubmissionStatus::Submitting);
    let run_uuid = inputs.run_uuid.clone();
    std::thread::Builder::new()
        .name("sd-auto-submit-then-action".into())
        .spawn(move || {
            crate::log_info!(
                "v0.2.39 (E): auto-submit triggered for skipped scan run_uuid={} (actions: {:?})",
                run_uuid,
                actions_taken_summary
            );
            // Phase 1: POST /submit.
            let outcome = super::submission::submit(&state, &inputs);
            let submission_id = match outcome {
                super::submission::SubmitOutcome::Accepted { submission_id, .. } => {
                    super::submission::store_pending_submission_id(submission_id.clone());
                    // We've consumed the in-memory pending inputs; clear so a
                    // double-fire doesn't re-submit.
                    let _ = super::submission::take_pending();
                    crate::log_info!(
                        "v0.2.39 (E): auto-submit Accepted submission_id={} run_uuid={}",
                        submission_id,
                        run_uuid
                    );
                    submission_id
                }
                other => {
                    crate::log_warn!(
                        "v0.2.39 (E): auto-submit failed ({other:?}); queueing action credit to disk for next /submit"
                    );
                    queue_to_disk(&actions_taken_summary, &run_uuid);
                    store_status(ActionSubmissionStatus::Queued {
                        reason: format!("auto-submit failed: {other:?}"),
                    });
                    return;
                }
            };
            // Phase 2: PATCH /actions — delegate to the existing worker so
            // retry budget + state-machine semantics are shared.
            spawn_submit_worker(state, submission_id, actions_taken_summary);
        })
        .expect("spawn auto-submit-then-action thread");
}

/// Queue every (action_key, ok_bytes) entry in `summary` to the disk queue,
/// stamped with `run_uuid` + now. Drains on next /submit Accepted.
fn queue_to_disk(summary: &BTreeMap<String, u64>, run_uuid: &str) {
    let now = crate::time::now_unix_i64();
    for (k, v) in summary {
        let entry = super::pending_actions::PendingAction {
            action_key: k.clone(),
            ok_bytes: *v,
            run_uuid: run_uuid.to_string(),
            queued_at: now,
        };
        if let Err(e) = super::pending_actions::append(&entry) {
            crate::log_err!(
                "pending_actions::append failed for {k}={v} run={run_uuid}: {e}"
            );
        } else {
            crate::log_info!(
                "pending_actions::append queued {k}={v} run={run_uuid} (will drain on next /submit Accepted)"
            );
        }
    }
}

/// v0.2.39 #99 — drain the disk queue after a successful /submit Accepted.
/// Aggregates queued entries by action_key (PATCH UPSERT semantics make this
/// safe), folds them into `current` if provided, and PATCHes the merged
/// summary against the new submission_id. Clears the disk queue on PATCH
/// success; leaves it (or appends new) on Transient.
///
/// Called from the GUI submit success path AFTER store_pending_submission_id.
/// If `current` is empty (no in-flight current-scan actions, the drain is
/// pure-queue catch-up), the merged summary is just the aggregate.
pub fn drain_pending_after_submit(
    state: InstallState,
    submission_id: String,
    current: BTreeMap<String, u64>,
) {
    if super::pending_actions::is_empty() && current.is_empty() {
        return;
    }
    std::thread::Builder::new()
        .name("sd-pending-actions-drain".into())
        .spawn(move || {
            let queued = match super::pending_actions::drain_aggregated() {
                Ok(a) => a,
                Err(e) => {
                    crate::log_err!("pending_actions::drain failed: {e}; skipping");
                    return;
                }
            };
            // Merge queued + current (sum by key — PATCH UPSERT).
            let mut merged: BTreeMap<String, u64> = current;
            for (k, v) in &queued {
                *merged.entry(k.clone()).or_insert(0) += *v;
            }
            if merged.is_empty() {
                return;
            }
            crate::log_info!(
                "v0.2.39 #99: draining {} queued action credits onto submission_id={} (merged: {:?})",
                queued.len(),
                submission_id,
                merged
            );
            // Reuse the standard worker for the PATCH + retry semantics.
            spawn_submit_worker(state, submission_id, merged);
        })
        .expect("spawn pending-actions-drain thread");
}

pub fn spawn_submit_worker(
    state: InstallState,
    submission_id: String,
    actions_taken_summary: BTreeMap<String, u64>,
) {
    store_status(ActionSubmissionStatus::Submitting);
    std::thread::Builder::new()
        .name("superdeduper-action-submit".into())
        .spawn(move || {
            // Backoff cadence locked at 500ms / 2s / 5s / 15s per
            // #79 spec. Four total attempts (initial + 3 retries).
            let backoffs = [
                None,
                Some(std::time::Duration::from_millis(500)),
                Some(std::time::Duration::from_secs(2)),
                Some(std::time::Duration::from_secs(5)),
                Some(std::time::Duration::from_secs(15)),
            ];
            let mut last_transient: Option<String> = None;
            for (attempt, sleep) in backoffs.iter().enumerate() {
                if let Some(d) = sleep {
                    store_status(ActionSubmissionStatus::Retrying {
                        attempt: attempt as u32,
                        last_error: last_transient.clone().unwrap_or_default(),
                    });
                    std::thread::sleep(*d);
                }
                let outcome = submit_actions(&state, &submission_id, &actions_taken_summary);
                match outcome {
                    ActionSubmitOutcome::Accepted {
                        lifetime_bytes_reclaimed,
                        newly_granted_achievements,
                    } => {
                        // #122 / #123 — local action credit now
                        // lands in the History row via the new
                        // `record_local_action_for_latest_scan` path
                        // in app.rs (fires regardless of submit
                        // state). Removing the duplicate
                        // `update_reclaim_for_submission` call here
                        // avoids double-counting: the local path
                        // already added these bytes; the server
                        // separately tracks them in DDB; PATCH
                        // success just confirms server received the
                        // credit, no local mutation needed. The helper
                        // itself stays available for resubmit-from-
                        // history flows that need to populate a
                        // pre-fix row.
                        let _ = (&actions_taken_summary, &submission_id);
                        store_status(ActionSubmissionStatus::Credited {
                            lifetime_bytes_reclaimed,
                            newly_granted_achievements,
                        });
                        return;
                    }
                    ActionSubmitOutcome::Unauthorised(reason) => {
                        store_status(ActionSubmissionStatus::Queued {
                            reason: format!("auth lost: {reason}"),
                        });
                        return;
                    }
                    ActionSubmitOutcome::Rejected { status, reason } => {
                        store_status(ActionSubmissionStatus::Queued {
                            reason: format!("rejected ({status}): {reason}"),
                        });
                        return;
                    }
                    ActionSubmitOutcome::Transient { reason } => {
                        last_transient = Some(reason);
                        continue;
                    }
                }
            }
            // Retries exhausted.
            store_status(ActionSubmissionStatus::Queued {
                reason: format!(
                    "transient failure after 4 attempts: {}",
                    last_transient.unwrap_or_else(|| "<no error captured>".into())
                ),
            });
        })
        .expect("spawn action-submit thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::DedupeAction;
    use crate::dedupe::DedupeActionSummary;

    #[test]
    fn build_actions_body_shape_matches_wire_contract() {
        let mut m = BTreeMap::new();
        m.insert("deleted_to_recycle_bytes".to_string(), 1_234_567u64);
        m.insert("hardlink_replaced_bytes".to_string(), 2_222u64);
        let v = build_actions_body(&m);
        let s = serde_json::to_string(&v).unwrap();
        // BTreeMap serialises in key order — pin that so the
        // canonical_body HMAC step gets deterministic input
        // regardless of insertion order.
        assert!(
            s.contains(r#""actions_taken_summary":{"deleted_to_recycle_bytes":1234567,"hardlink_replaced_bytes":2222}"#),
            "got: {s}"
        );
    }

    #[test]
    fn actions_summary_from_dedupe_skips_safe_rename() {
        let s = DedupeActionSummary {
            action: DedupeAction::SafeRename,
            ok_count: 10,
            ok_bytes: 1024,
            failed_count: 0,
            failed_bytes: 0,
            user_stopped: false,
        };
        assert!(
            actions_summary_from_dedupe(&s).is_none(),
            "SafeRename has no LOCKED_ACTION_KEYS slot",
        );
    }

    #[test]
    fn actions_summary_from_dedupe_skips_zero_bytes() {
        // Every file failed — there's nothing to credit. PATCH
        // would technically succeed with a `{"key": 0}` payload
        // but that crashes the UPSERT-with-zero edge on the web
        // side per their cc6c5e74 sanity check; better to skip.
        let s = DedupeActionSummary {
            action: DedupeAction::Recycle,
            ok_count: 0,
            ok_bytes: 0,
            failed_count: 5,
            failed_bytes: 1024,
            user_stopped: false,
        };
        assert!(actions_summary_from_dedupe(&s).is_none());
    }

    #[test]
    fn actions_summary_from_dedupe_returns_single_key_map() {
        let s = DedupeActionSummary {
            action: DedupeAction::Recycle,
            ok_count: 3,
            ok_bytes: 1_500_000,
            failed_count: 0,
            failed_bytes: 0,
            user_stopped: false,
        };
        let m = actions_summary_from_dedupe(&s).expect("recycle is credited");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("deleted_to_recycle_bytes"), Some(&1_500_000));
    }
}
