//! Submission disk-queue + archive + review storage.
//!
//! Codex-review item 3 (v0.3.27 follow-on, 2026-06-02): extracted from
//! `submission.rs` to give the on-disk submission state machinery its
//! own home. submission.rs becomes the engine-integration layer
//! (InstallState wrappers + schemars schema-derive + constants);
//! everything that touches a directory under the engine's config-data
//! path lives here.
//!
//! ## Surfaces
//!
//! * **Offline queue** (`queue_dir`, `enqueue`, `prune_queue`): 50-entry
//!   cap per §6.5; persists transient-failed submissions for next-launch
//!   retry by `submit-pending`.
//! * **Archive** (`archive_dir`, `archive_attempt`, `write_archive_entry`):
//!   permanent local record of every submission attempt (any outcome).
//! * **Review queue** (`review_dir`, `flag_for_review`, `try_upload_review`,
//!   `write_review_entry`): rejected submissions the user explicitly
//!   flagged for admin review. Local save always lands; backend POST
//!   is best-effort.

#![cfg(feature = "telemetry")]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::hmac_signer;
use super::install::InstallState;
use super::submission::{build_payload, RankEntry, SubmissionInputs, SubmitOutcome};

// ============================================================
// Directory helpers
// ============================================================

/// Directory under the install data dir where transient-failed
/// submissions are persisted.
pub fn queue_dir() -> std::io::Result<PathBuf> {
    let mut p = super::install::install_path()?;
    p.set_file_name("submission-queue");
    Ok(p)
}

/// Directory under the install data dir where EVERY submission
/// (successful, rejected, transient-failed) is archived. Gives the
/// user a permanent local record of what's been sent + how the
/// server responded — useful for diagnosis, audit, or restoring
/// runs that need re-submission after a backend fix.
pub fn archive_dir() -> std::io::Result<PathBuf> {
    let mut p = super::install::install_path()?;
    p.set_file_name("submission-archive");
    Ok(p)
}

/// Directory under the install data dir where the user has
/// explicitly flagged failed submissions for admin review. The
/// engine also tries to POST these to a backend review endpoint
/// (TBD; falls back to local-only when the endpoint isn't live).
pub fn review_dir() -> std::io::Result<PathBuf> {
    let mut p = super::install::install_path()?;
    p.set_file_name("submission-review");
    Ok(p)
}

// ============================================================
// Archive
// ============================================================

/// Archive a submission attempt — body + outcome + timestamp — to
/// the local archive directory. Called from every submit code path
/// regardless of outcome. Best-effort: failure is logged + swallowed
/// so a write error on the archive can't suppress the actual
/// submission outcome from the UI.
pub fn archive_attempt(inputs: &SubmissionInputs, install_id: &str, outcome: &SubmitOutcome) {
    let body = build_payload(inputs, install_id);
    let entry = ArchivedSubmission {
        archived_at_unix: crate::time::now_unix_i64(),
        archived_at_iso: now_iso8601(),
        run_uuid: inputs.run_uuid.clone(),
        install_id: install_id.to_string(),
        outcome_kind: outcome_kind_tag(outcome).to_string(),
        outcome_detail: serde_json::to_value(SerializableOutcome::from(outcome))
            .unwrap_or(serde_json::Value::Null),
        payload: body,
    };
    if let Err(e) = write_archive_entry(&entry) {
        crate::log_err!("leaderboard: archive write failed: {e:?}");
    }
}

fn write_archive_entry(entry: &ArchivedSubmission) -> std::io::Result<PathBuf> {
    let dir = archive_dir()?;
    std::fs::create_dir_all(&dir)?;
    // Archive grows forever (intentional — user wants a permanent
    // record of every submission). Could revisit a cap if it ever
    // grows large enough to matter; today each entry is < 4 KB.
    let filename = format!(
        "{}-{}-{}.json",
        entry.archived_at_unix,
        entry.outcome_kind,
        short_uuid(&entry.run_uuid),
    );
    let path = dir.join(filename);
    std::fs::write(&path, serde_json::to_vec_pretty(entry)?)?;
    Ok(path)
}

// ============================================================
// Review queue
// ============================================================

/// Save a rejected submission to the review-queue directory AND
/// attempt to POST it to the backend review endpoint. The local
/// save always happens (zero-dependency MVP); the POST is
/// best-effort — when the endpoint isn't live yet, the local copy
/// is the source of truth and Mick collects them manually from
/// beta testers.
///
/// Returns `(local_path, upload_review_id)`. `upload_review_id` is
/// `Some` only when the upload landed 200; `None` for any failure
/// (which still leaves the local save intact).
pub fn flag_for_review(
    state: &InstallState,
    inputs: &SubmissionInputs,
    rejection: &SubmitOutcome,
    user_note: Option<&str>,
) -> std::io::Result<(PathBuf, Option<String>)> {
    let body = build_payload(inputs, &state.install_id);
    let entry = ReviewSubmission {
        flagged_at_unix: crate::time::now_unix_i64(),
        flagged_at_iso: now_iso8601(),
        run_uuid: inputs.run_uuid.clone(),
        install_id: state.install_id.clone(),
        client_version: inputs.client_version.clone(),
        rejection: serde_json::to_value(SerializableOutcome::from(rejection))
            .unwrap_or(serde_json::Value::Null),
        user_note: user_note.map(String::from),
        payload: body.clone(),
    };
    let path = write_review_entry(&entry)?;
    // Best-effort upload. Endpoint went live (web commit c8cceb9),
    // so a real signed POST should land. Stderr line is kept as a
    // diagnostic trail; the structured review_id is what the GUI
    // surfaces to the user.
    let review_id = match try_upload_review(state, &entry) {
        Ok(review_id) => {
            crate::log_info!(
                "review: uploaded ({}). Backend review_id={review_id}. Local copy at {}",
                state.server_url,
                path.display()
            );
            Some(review_id)
        }
        Err(e) => {
            crate::log_warn!(
                "review: upload failed ({e}). Local copy at {}",
                path.display()
            );
            None
        }
    };
    Ok((path, review_id))
}

/// POST the review entry to the backend review endpoint. On 200,
/// returns the server-assigned review_id (string) — useful in
/// stderr so the user or admin can correlate when reading the
/// review queue. On any non-2xx, returns the body or transport
/// error as the Err string; caller decides surface treatment.
fn try_upload_review(state: &InstallState, entry: &ReviewSubmission) -> Result<String, String> {
    let install_key = state.install_key().ok_or("install_key malformed")?;
    let body_value = serde_json::to_value(entry).map_err(|e| format!("{e}"))?;
    let canonical = hmac_signer::canonical_body(&body_value);
    let signature = hmac_signer::sign(&install_key, &canonical);
    let url = format!(
        "{}/api/v1/submit/review",
        state.server_url.trim_end_matches('/')
    );
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(15))
        .send_bytes(&canonical);
    match resp {
        Ok(r) => {
            // Parse {accepted: true, review_id: "<uuid>"} per web's
            // contract; fall back to "<unknown>" if the body shape
            // changes later.
            let v: serde_json::Value = r.into_json().unwrap_or(serde_json::Value::Null);
            let review_id = v
                .get("review_id")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>")
                .to_string();
            Ok(review_id)
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(format!("HTTP {code}: {body}"))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("transport: {t}")),
    }
}

fn write_review_entry(entry: &ReviewSubmission) -> std::io::Result<PathBuf> {
    let dir = review_dir()?;
    std::fs::create_dir_all(&dir)?;
    let filename = format!(
        "{}-{}.json",
        entry.flagged_at_unix,
        short_uuid(&entry.run_uuid),
    );
    let path = dir.join(filename);
    std::fs::write(&path, serde_json::to_vec_pretty(entry)?)?;
    Ok(path)
}

// ============================================================
// Helpers
// ============================================================

fn short_uuid(s: &str) -> String {
    s.chars().take(8).collect()
}

pub(crate) fn outcome_kind_tag(o: &SubmitOutcome) -> &'static str {
    match o {
        SubmitOutcome::Accepted { .. } => "accepted",
        SubmitOutcome::DuplicateNoChange { .. } => "duplicate",
        SubmitOutcome::Rejected { .. } => "rejected",
        SubmitOutcome::Transient { .. } => "transient",
        SubmitOutcome::FlaggedForReview { .. } => "flagged-for-review",
    }
}

/// RFC 3339 / ISO 8601 UTC timestamp of "now" with seconds precision,
/// e.g. `"2026-05-24T05:42:33Z"`. Delegates to `crate::time::now_iso8601`.
pub(crate) fn now_iso8601() -> String {
    crate::time::now_iso8601()
}

// ============================================================
// Storage structs
// ============================================================

#[derive(Debug, Serialize, Deserialize)]
struct ArchivedSubmission {
    archived_at_unix: i64,
    archived_at_iso: String,
    run_uuid: String,
    install_id: String,
    outcome_kind: String,
    outcome_detail: serde_json::Value,
    payload: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReviewSubmission {
    flagged_at_unix: i64,
    flagged_at_iso: String,
    run_uuid: String,
    install_id: String,
    client_version: String,
    rejection: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_note: Option<String>,
    payload: serde_json::Value,
}

/// Mirror of `SubmitOutcome` that derives Serialize via tagged-union
/// JSON. Used so the archive + review files include the full
/// structured outcome (ranks, achievements, rejection reason)
/// without losing variant info.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SerializableOutcome {
    Accepted {
        submission_id: String,
        ranks: Vec<RankEntry>,
        achievements_unlocked: Vec<String>,
        profile_url: Option<String>,
    },
    DuplicateNoChange,
    Rejected {
        status: u16,
        reason: String,
    },
    Transient {
        reason: String,
    },
    FlaggedForReview {
        review_id: Option<String>,
        local_path: String,
        original_status: u16,
        original_reason: String,
    },
}

impl From<&SubmitOutcome> for SerializableOutcome {
    fn from(o: &SubmitOutcome) -> Self {
        match o {
            SubmitOutcome::Accepted {
                submission_id,
                ranks,
                achievements_unlocked,
                profile_url,
            } => SerializableOutcome::Accepted {
                submission_id: submission_id.clone(),
                ranks: ranks.clone(),
                achievements_unlocked: achievements_unlocked.clone(),
                profile_url: profile_url.clone(),
            },
            SubmitOutcome::DuplicateNoChange { .. } => SerializableOutcome::DuplicateNoChange,
            SubmitOutcome::Rejected { status, reason } => SerializableOutcome::Rejected {
                status: *status,
                reason: reason.clone(),
            },
            SubmitOutcome::Transient { reason } => SerializableOutcome::Transient {
                reason: reason.clone(),
            },
            SubmitOutcome::FlaggedForReview {
                review_id,
                local_path,
                original_status,
                original_reason,
            } => SerializableOutcome::FlaggedForReview {
                review_id: review_id.clone(),
                local_path: local_path.clone(),
                original_status: *original_status,
                original_reason: original_reason.clone(),
            },
        }
    }
}

// ============================================================
// Offline queue
// ============================================================

/// Persist a payload + signature pair so the next launch can retry.
/// Filename includes a timestamp + the first 8 hex chars of the
/// payload hash for de-dup.
pub fn enqueue(
    inputs: &SubmissionInputs,
    install_id: &str,
    signature: &str,
) -> std::io::Result<PathBuf> {
    let dir = queue_dir()?;
    std::fs::create_dir_all(&dir)?;
    // Cap at 50 entries — oldest gets evicted first.
    prune_queue(&dir, 50)?;
    let payload = build_payload(inputs, install_id);
    let body = hmac_signer::canonical_body(&payload);
    let body_hash_prefix = blake3::hash(&body).to_hex();
    let filename = format!(
        "{}-{}.json",
        crate::time::now_unix_i64(),
        &body_hash_prefix.as_str()[..8]
    );
    let path = dir.join(filename);
    let stored = QueuedSubmission {
        body: String::from_utf8_lossy(&body).into_owned(),
        signature: signature.to_string(),
        enqueued_at_unix: crate::time::now_unix_i64(),
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&stored)?)?;
    Ok(path)
}

/// Keep the queue at most `cap` entries by deleting the oldest.
fn prune_queue(dir: &std::path::Path, cap: usize) -> std::io::Result<()> {
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let m = e.metadata().ok()?.modified().ok()?;
            Some((m, p))
        })
        .collect();
    if entries.len() <= cap {
        return Ok(());
    }
    entries.sort_by_key(|(t, _)| *t);
    let drop_count = entries.len() - cap;
    for (_, p) in entries.into_iter().take(drop_count) {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct QueuedSubmission {
    body: String,
    signature: String,
    enqueued_at_unix: i64,
}
