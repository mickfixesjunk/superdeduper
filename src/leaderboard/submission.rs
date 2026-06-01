//! Submit a completed scan's results to the leaderboard backend.
//!
//! Flow per client-spec §6:
//! 1. Build the payload from scan results + hardware fingerprint
//! 2. Canonicalize to JSON (sorted keys via [`hmac_signer::canonical_body`])
//! 3. HMAC-sign with the install_key
//! 4. POST to `/api/v1/submit` with `X-Sd-Signature` header
//! 5. On 5xx / network failure: enqueue to disk for next-launch retry
//!    (50-submission cap per spec §6.5)
//! 6. On 200: surface rank + achievements to the caller
//! 7. On 409 (`duplicate_submission`): treat as neutral "no change"
//!
//! The wire payload shape is not yet locked against the live JSON
//! schema at `https://api.superdeduper.io/api/v1/submit/schema.json` —
//! follow-up commit will regenerate the struct via `typify`. For now
//! the body is built as a `serde_json::Value` so field additions
//! don't churn a Rust type.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::hmac_signer;
use super::install::InstallState;
use superdeduper_bench_real::submission_http;

// Phase 2-B 2026-06-01: SubmissionInputs struct moved to bench-iface
// (alongside RunShape / ResultSummary / CanonicalBench / HardwareFingerprint
// migrated earlier in this phase). Engine keeps the builder code that
// constructs SubmissionInputs values + every consumer; the struct
// definition now lives in iface so bench-real can take SubmissionInputs
// across the trait boundary without back-importing engine internals.
// See [[project_v032_modularization_overflow_queue]] / Phase 2-B plan.
pub use superdeduper_bench_iface::SubmissionInputs;

// Phase 2-B 2026-06-01: CanonicalBench struct moved to bench-iface
// (same pattern as RunShape / ResultSummary). Engine retains the
// builder (to_canonical_bench in bench_client) + every caller.
pub use superdeduper_bench_iface::CanonicalBench;

// Phase 2-B 2026-06-01: RunShape struct definition moved to the
// bench-iface crate. Engine keeps the FEATURE_BIT_* constants +
// build_payload + every caller that constructs RunShape values;
// only the struct definition migrated. The re-export below means
// every existing call site that wrote `super::submission::RunShape`
// or `leaderboard::submission::RunShape` resolves unchanged.
pub use superdeduper_bench_iface::RunShape;


// Phase 2-B 2026-06-01: ResultSummary struct moved to bench-iface
// (same pattern as RunShape). Engine keeps ACTION_BYTES_KEY_* constants
// + build_payload code that constructs ResultSummary. Re-export below
// keeps every existing call site resolving.
pub use superdeduper_bench_iface::ResultSummary;

/// Locked key for the "delete-to-recycle bytes" entry in
/// [`ResultSummary::actions_taken_summary`]. Web's `lifetime-audit.ts`
/// validates against this exact string; any drift here will cause
/// submissions to be rejected.
pub const ACTION_BYTES_KEY_DELETED_TO_RECYCLE: &str = "deleted_to_recycle_bytes";
/// Locked key for permanent-delete bytes.
pub const ACTION_BYTES_KEY_DELETED_PERMANENTLY: &str = "deleted_permanently_bytes";
/// Locked key for hardlink-replace bytes.
pub const ACTION_BYTES_KEY_HARDLINK_REPLACED: &str = "hardlink_replaced_bytes";

// `features_used_bitmap` bit assignments. Stable; new features
// append. Reserved bits stay 0 until claimed.
pub const FEATURE_BIT_CACHE: u64 = 1 << 0;
pub const FEATURE_BIT_FORMAT_AWARE: u64 = 1 << 1;
// Bit 2 reserved — formerly `FEATURE_BIT_PARANOID`, removed in #131
// (v0.2.16) along with the no-op `--paranoid` flag. Stay 0 until a
// future feature claims it; do not reuse for an unrelated meaning,
// historical submissions stored this bit semantically.
pub const FEATURE_BIT_FOLLOW_LINKS: u64 = 1 << 3;
pub const FEATURE_BIT_ALLOW_SYSTEM_PATHS: u64 = 1 << 4;
pub const FEATURE_BIT_ALLOW_RECALL_ON_READ: u64 = 1 << 5;
pub const FEATURE_BIT_REFERENCE_ROOTS: u64 = 1 << 6;
pub const FEATURE_BIT_INCLUDE_GLOB: u64 = 1 << 7;
pub const FEATURE_BIT_EXCLUDE_GLOB: u64 = 1 << 8;

// Phase 2-B 2026-06-01: SubmitOutcome enum + RankEntry struct moved to
// bench-iface alongside SubmissionInputs. The trait surface
// (SubmissionExecutor::submit_recorded) returns SubmitOutcome across
// the cut-line, so the enum must live in iface. Engine keeps every
// consumer; only the type definitions migrated.
pub use superdeduper_bench_iface::{RankEntry, SubmitOutcome};

/// #144 — Derive the canonical JSON Schema for the submission wire
/// structs (`RunShape`, `ResultSummary`, `HardwareFingerprint`) +
/// render it as pretty JSON. ENGINE OWNS this schema: it's derived
/// from the Rust structs via `schemars`, committed to
/// `schema/submit.schema.json`, and a CI test
/// (`submission_schema_matches_committed`) fails if a struct field
/// changes without regenerating the committed file. That failure
/// is the #96 root-cause forcing function — a field rename can no
/// longer silently drift into a runtime 400 (is_dev_drive incident)
/// or a silently-disabled achievement (catalog drifts).
///
/// Web validates against the committed/published artifact (its
/// half of #96 — design routed the consumption mechanism to web).
///
/// To regenerate after an intentional struct change:
/// `SD_UPDATE_SCHEMA=1 cargo test --features telemetry
/// submission_schema_matches_committed` writes the new file; review
/// the diff + commit it alongside the struct change.
#[cfg(feature = "telemetry")]
pub fn wire_schema_json() -> String {
    let settings = schemars::gen::SchemaSettings::draft2019_09();
    let generator = schemars::gen::SchemaGenerator::new(settings);
    // Build a combined object describing all three top-level wire
    // blocks under stable keys. The combined doc is what web
    // validates a submission's hardware / run_shape / result_summary
    // sub-objects against.
    let mut gen = generator;
    let run_shape = gen.subschema_for::<RunShape>();
    let result_summary = gen.subschema_for::<ResultSummary>();
    let hardware = gen.subschema_for::<super::hardware::HardwareFingerprint>();
    let root = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2019-09/schema",
        "title": "superdeduper.submit.wire-structs",
        "description": "Engine-owned canonical schema for the /api/v1/submit \
                        payload sub-objects. Derived from the Rust structs via \
                        schemars (#144). DO NOT hand-edit — regenerate via \
                        SD_UPDATE_SCHEMA=1 cargo test.",
        "definitions": gen.definitions(),
        "properties": {
            "hardware": hardware,
            "run_shape": run_shape,
            "result_summary": result_summary,
        },
    });
    let mut s = serde_json::to_string_pretty(&root).expect("schema serializes");
    s.push('\n');
    s
}

/// Build the canonical JSON request body. Pure function so tests can
/// snapshot the exact wire bytes without spinning a server.
///
/// `install_id` is threaded as a separate param (rather than baked
/// into `SubmissionInputs`) so the most up-to-date value from the
/// current install state is always used — guards against a stale
/// pending payload outliving a "Reset install" rotation.
// Phase 2-B v0.3.20 (2026-06-01): build_payload body + effective_lane helper
// moved to bench-real submission_http. Re-exported here so every existing
// engine call site (`leaderboard::submission::build_payload(...)`) continues
// to resolve unchanged via the bench-real implementation. The cold-enforce
// lane override + canonical-bench top-level lift logic now lives entirely
// in bench-real; this engine module sees only the public function.
pub use superdeduper_bench_real::submission_http::build_payload;

/// #41 — re-POST a previously-recorded canonical payload. Same
/// signing flow as [`submit`] but the body comes from the stored
/// `scan_history::ScanRecord.submission_payload` rather than being
/// rebuilt from a fresh `SubmissionInputs`. This guarantees the
/// signature matches what the user actually saw at scan-finish; a
/// rebuild would re-detect hardware, re-stamp the timestamp, etc.
/// + potentially drift away from what the History row claims.
///
/// `built_with_install_id`: the install_id captured at payload-
/// build time (stored alongside the payload in the History row).
/// If the active install_id differs (user clicked "Reset install"
/// between scan + resubmit), the HMAC under the new key would
/// 401 against the server. Surfaces a clear
/// `Rejected{ status: 0, reason: "install changed since scan" }`
/// up-front rather than waiting for the server's 401.
///
/// `server_url`: the recorded channel's server URL (NOT the live
/// channel's). Channel-aware resubmit routes against the channel
/// the scan was captured on. The caller (GUI Resubmit button)
/// resolves this from
/// `scan_history::ScanRecord.submission_channel` →
/// `channel::server_url_for`.
pub fn submit_recorded_payload(
    state: &InstallState,
    payload: &serde_json::Value,
    built_with_install_id: &str,
    server_url: &str,
) -> SubmitOutcome {
    if !state.registered {
        return SubmitOutcome::Rejected {
            status: 0,
            reason: "install not registered — call `superdeduper register` first".to_string(),
        };
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return SubmitOutcome::Rejected {
                status: 0,
                reason: "install_key_hex malformed".to_string(),
            };
        }
    };
    // Phase 2-B v0.3.20 (2026-06-01): HTTP body + install_id-mismatch check
    // + timestamp-refresh + parse_ok/parse_error moved to bench-real
    // submission_http::submit_recorded_payload_inner. Engine retains
    // the InstallState-coupled API as this thin wrapper that unpacks the
    // creds; zero call-site rewrites for the 6 engine consumers of
    // submit_recorded_payload.
    submission_http::submit_recorded_payload_inner(
        server_url,
        &state.install_id,
        &install_key,
        payload,
        built_with_install_id,
    )
}

/// Build + sign + POST a submission. Network errors surface as
/// `Transient`; the caller decides whether to enqueue for retry.
/// This function never panics on network failure; it returns an
/// outcome.
pub fn submit(state: &InstallState, inputs: &SubmissionInputs) -> SubmitOutcome {
    if !state.registered {
        return SubmitOutcome::Rejected {
            status: 0,
            reason: "install not registered — call `superdeduper register` first".to_string(),
        };
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return SubmitOutcome::Rejected {
                status: 0,
                reason: "install_key_hex malformed".to_string(),
            };
        }
    };
    // Phase 2-B v0.3.20 (2026-06-01): HMAC + POST + parse_ok/parse_error
    // body moved to bench-real submission_http::submit_inner. Engine
    // retains the InstallState-coupled API as this thin wrapper. Zero
    // call-site rewrites for the 6+ engine consumers of submit().
    submission_http::submit_inner(&state.server_url, &state.install_id, &install_key, inputs)
}

// Phase 2-B v0.3.20 (2026-06-01): parse_ok, parse_error,
// duplicate_submission_id_from_409 moved to bench-real submission_http
// (private; consumed by submit_inner + submit_recorded_payload_inner).
// Engine retains no parse helpers -- the HTTP-response parsing is fully
// owned by bench-real. The 5 effective_lane unit tests + the
// duplicate_submission_id_from_409 unit test moved with the helpers (see
// crates/superdeduper-bench-real/src/submission_http.rs tests).

// ============================================================
// Offline queue: 50-submission cap, drain-on-startup per §6.5.
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
    state: &super::install::InstallState,
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
fn try_upload_review(
    state: &super::install::InstallState,
    entry: &ReviewSubmission,
) -> Result<String, String> {
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

fn short_uuid(s: &str) -> String {
    s.chars().take(8).collect()
}

fn outcome_kind_tag(o: &SubmitOutcome) -> &'static str {
    match o {
        SubmitOutcome::Accepted { .. } => "accepted",
        SubmitOutcome::DuplicateNoChange { .. } => "duplicate",
        SubmitOutcome::Rejected { .. } => "rejected",
        SubmitOutcome::Transient { .. } => "transient",
        SubmitOutcome::FlaggedForReview { .. } => "flagged-for-review",
    }
}

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
enum SerializableOutcome {
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

/// RFC 3339 / ISO 8601 UTC timestamp of "now" with seconds
/// precision, e.g. `"2026-05-24T05:42:33Z"`. Backend schema requires
/// `timestamp` in this format (string with format: date-time).
fn now_iso8601() -> String {
    crate::time::now_iso8601()
}

/// Render a unix timestamp (seconds since epoch) as RFC 3339 UTC.
/// Delegates to `crate::time::unix_to_ymdhms` (Howard Hinnant
/// civil-from-days) so this and other call sites share a single
/// implementation post-#91. Test-only — runtime callers use
/// [`now_iso8601`] which goes via `crate::time::now_iso8601` end to
/// end.
#[cfg(test)]
fn iso8601_from_unix(unix: i64) -> String {
    let (year, mo, day, h, m, s) = crate::time::unix_to_ymdhms(unix);
    format!("{year:04}-{mo:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

// ============================================================
// Engine→GUI handoff for the most-recent scan's submission inputs.
//
// The engine builds [`SubmissionInputs`] at scan-end (see
// `gui::live::run`) and stashes them here. The GUI's "Submit run"
// button reads them on click. A single global slot is sufficient:
// only one scan runs at a time, and only the *most recent* scan
// is submittable from the UI. Older payloads either succeeded
// (and the slot was cleared) or sit in the offline queue.
//
// Last-outcome cell follows the same pattern so the UI can render
// "rank #4 in Win11 9950X3D / +2 achievements" inline after the
// worker thread returns from submit().
// ============================================================

use parking_lot::Mutex;
use std::sync::OnceLock;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inputs() -> SubmissionInputs {
        SubmissionInputs {
            client_version: "0.1.0".into(),
            run_uuid: "9d4a0000-0000-0000-0000-000000000001".into(),
            scan_id: None,
            bench: None,
            lane: None,
            hardware: crate::leaderboard::hardware::detect(),
            run_shape: RunShape {
                wall_clock_seconds: 5.678,
                bytes_scanned: 9_876_543,
                files_scanned: 1234,
                hash_algorithm: "river5-aes-ni".into(),
                walker_variant: "hybrid".into(),
                scope: "subdirectory".into(),
                features_used_bitmap: FEATURE_BIT_CACHE | FEATURE_BIT_FORMAT_AWARE,
                corpus_kind: "user-data".into(),
                cache_hit_ratio: None,
                easter_egg_hits: Vec::new(),
                zero_byte_group_max: None,
                max_hardlink_count_in_scan: None,
                name_collision_count: None,
                share_count_in_scope: None,
                dry_run: None,
                groups_reviewed_count: None,
            },
            result_summary: ResultSummary {
                duplicate_groups: 42,
                duplicate_bytes_reclaimable: 12345,
                largest_single_group_bytes: 1000,
                actions_taken_summary: std::collections::BTreeMap::new(),
                placeholder_skip_count: None,
                placeholder_skip_bytes: None,
                client_found_dupsets: None,
            },
        }
    }

    #[test]
    fn build_payload_has_all_required_top_level_keys() {
        let p = build_payload(&sample_inputs(), "test-install-id");
        // Backend schema's `required` set.
        for key in [
            "schema_version",
            "client_version",
            "install_id",
            "timestamp",
            "hardware",
            "run_shape",
            "result_summary",
        ] {
            assert!(p.get(key).is_some(), "missing required key '{key}'");
        }
        assert_eq!(
            p.get("install_id").and_then(|v| v.as_str()),
            Some("test-install-id")
        );
        assert_eq!(p.get("schema_version").and_then(|v| v.as_str()), Some("v1"));
    }

    #[test]
    fn build_payload_emits_canonical_bench_block_when_present() {
        let mut inputs = sample_inputs();
        inputs.run_shape.scope = "canonical-bench".into();
        inputs.run_shape.corpus_kind = "canonical-bench".into();
        inputs.result_summary.client_found_dupsets = Some(vec![vec![0, 6], vec![2, 8, 9]]);
        inputs.bench = Some(CanonicalBench {
            protocol_version: "tbench-1".into(),
            corpus_version: "corpus-v1-quick".into(),
            tier: "quick".into(),
            bench_run_id: "run-xyz".into(),
            bench_proof: serde_json::json!({
                "answers": [{ "path_index": 7, "byte_offset": 0, "byte_length": 16, "challenge_hash": "T0ue6DbvgHrGSD8Zs93DvT3G5i8o3eNUKSSbeyJVisY=" }],
                "result_digest": "2Ak/YbCbLu9E+xhuiv024PJd3OnS5ZTNzPJtDzh6aG0=",
            }),
            cold_enforced: true,
        });
        let p = build_payload(&inputs, "id");
        assert_eq!(p.get("protocol_version").and_then(|v| v.as_str()), Some("tbench-1"));
        // #106 — cold_enforced lifts to the top level alongside the bench fields.
        assert_eq!(p.get("cold_enforced").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(p.get("corpus_version").and_then(|v| v.as_str()), Some("corpus-v1-quick"));
        assert_eq!(p.get("tier").and_then(|v| v.as_str()), Some("quick"));
        assert_eq!(p.get("bench_run_id").and_then(|v| v.as_str()), Some("run-xyz"));
        assert_eq!(p.pointer("/bench_proof/result_digest").and_then(|v| v.as_str()), Some("2Ak/YbCbLu9E+xhuiv024PJd3OnS5ZTNzPJtDzh6aG0="));
        assert_eq!(p.pointer("/bench_proof/answers/0/path_index").and_then(|v| v.as_u64()), Some(7));
        assert!(p.pointer("/result_summary/client_found_dupsets").is_some());
        // ordinary (non-bench) submissions must NOT carry any of these keys.
        let plain = build_payload(&sample_inputs(), "id");
        for k in ["protocol_version", "corpus_version", "tier", "bench_run_id", "bench_proof"] {
            assert!(plain.get(k).is_none(), "non-bench payload must omit '{k}'");
        }
    }

    #[test]
    fn build_payload_does_not_emit_disallowed_top_level_keys() {
        // Backend schema is additionalProperties:false at the top
        // level. The pre-rewrite payload had `run_uuid`,
        // `sd_version`, `scan`, `submitted_at_unix` — all gone.
        let p = build_payload(&sample_inputs(), "x");
        for key in ["run_uuid", "sd_version", "scan", "submitted_at_unix"] {
            assert!(
                p.get(key).is_none(),
                "unexpected key '{key}' on payload — backend rejects with schema_invalid",
            );
        }
    }

    /// #150 — submit_recorded_payload must send the FLAT payload
    /// shape. /api/v1/submit looks up the HMAC key by the TOP-LEVEL
    /// `install_id` BEFORE verifying the signature, so install_id can
    /// never be nested. #60's outer envelope ({outer_timestamp,
    /// replay_of_scan_id, original_payload}) nested it → every
    /// resubmit 400'd install_id_missing (the universal v0.2.27
    /// regression). This pins the flat contract: install_id stays at
    /// the top, `timestamp` is refreshed (so the clock_skew check
    /// passes on resubmits), and NO envelope keys are introduced.
    /// Mirrors the in-place mutation the function performs (no server
    /// in unit tests).
    #[test]
    fn submit_recorded_payload_sends_flat_payload_with_refreshed_timestamp() {
        let captured = serde_json::json!({
            "schema_version": "v1",
            "client_version": "0.2.2",
            "install_id": "test-install",
            "timestamp": "1970-01-01T00:00:00Z",
            "hardware": {},
            "run_shape": {},
            "result_summary": { "scan_id": "scan-uuid-123" },
        });
        let mut refreshed = captured.clone();
        if let Some(obj) = refreshed.as_object_mut() {
            obj.insert(
                "timestamp".to_string(),
                serde_json::Value::String(now_iso8601()),
            );
        }

        // install_id stays at the TOP level — the regression was
        // re-nesting it under original_payload.
        assert_eq!(
            refreshed.get("install_id").and_then(|v| v.as_str()),
            Some("test-install"),
            "install_id must remain at the body top level (flat contract)"
        );
        // The #60 envelope keys must NOT come back.
        assert!(
            refreshed.get("original_payload").is_none(),
            "body must not re-nest the payload under original_payload"
        );
        assert!(
            refreshed.get("outer_timestamp").is_none(),
            "body must not wrap the payload in an outer envelope"
        );
        // timestamp refreshed away from the stale captured value so
        // the server's clock_skew check passes on a resubmit.
        let ts = refreshed.get("timestamp").and_then(|v| v.as_str()).unwrap();
        assert_ne!(
            ts, "1970-01-01T00:00:00Z",
            "timestamp must be refreshed before signing"
        );
        let year: u32 = ts[..4].parse().unwrap();
        assert!(year >= 2026, "refreshed timestamp must be current, got: {ts}");
        // Other identity fields ride along unchanged at the top level.
        assert_eq!(
            refreshed.get("client_version").and_then(|v| v.as_str()),
            Some("0.2.2")
        );
    }

    #[test]
    fn timestamp_is_rfc3339_seconds_zulu() {
        let p = build_payload(&sample_inputs(), "x");
        let ts = p.get("timestamp").and_then(|v| v.as_str()).unwrap();
        // Shape: YYYY-MM-DDTHH:MM:SSZ — 20 chars.
        assert_eq!(ts.len(), 20, "timestamp wrong length: {ts}");
        assert!(ts.ends_with('Z'), "timestamp must end with Z (UTC): {ts}");
        assert!(ts.contains('T'), "timestamp must have T separator: {ts}");
    }

    #[test]
    fn iso8601_matches_known_unix_anchors() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(iso8601_from_unix(1_704_067_200), "2024-01-01T00:00:00Z");
        // 2026-05-24T03:42:33Z = 1779594153
        assert_eq!(iso8601_from_unix(1_779_594_153), "2026-05-24T03:42:33Z");
        // Epoch
        assert_eq!(iso8601_from_unix(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn canonical_body_is_deterministic_across_inputs() {
        // Hardware uses detect() which queries the live machine — so
        // back-to-back calls return the same bytes, but a snapshot
        // test would break across machines. Just check shape.
        let p1 = build_payload(&sample_inputs(), "test-id");
        let p2 = build_payload(&sample_inputs(), "test-id");
        let b1 = hmac_signer::canonical_body(&p1);
        let b2 = hmac_signer::canonical_body(&p2);
        assert_eq!(b1, b2);
    }

    #[test]
    fn submit_rejects_when_not_registered() {
        let state = super::super::install::new_unregistered("https://example".into());
        // registered defaults to false; submit should refuse.
        let out = submit(&state, &sample_inputs());
        assert!(matches!(out, SubmitOutcome::Rejected { status: 0, .. }));
    }

    #[test]
    fn outcome_kind_tag_covers_every_variant() {
        // If a new SubmitOutcome variant ships without updating the
        // tag matcher, the archive file's `outcome_kind` field
        // would collide with the "rejected" default. This test
        // pins the mapping so any new variant must declare its tag.
        assert_eq!(
            outcome_kind_tag(&SubmitOutcome::Accepted {
                submission_id: "x".into(),
                ranks: vec![],
                achievements_unlocked: vec![],
                profile_url: None,
            }),
            "accepted"
        );
        assert_eq!(
            outcome_kind_tag(&SubmitOutcome::DuplicateNoChange { submission_id: None }),
            "duplicate"
        );
        assert_eq!(
            outcome_kind_tag(&SubmitOutcome::Rejected {
                status: 400,
                reason: "x".into(),
            }),
            "rejected"
        );
        assert_eq!(
            outcome_kind_tag(&SubmitOutcome::Transient { reason: "x".into() }),
            "transient"
        );
        assert_eq!(
            outcome_kind_tag(&SubmitOutcome::FlaggedForReview {
                review_id: None,
                local_path: "x".into(),
                original_status: 0,
                original_reason: "x".into(),
            }),
            "flagged-for-review"
        );
    }

    // Phase 2-B v0.3.20 (2026-06-01): duplicate_submission_id_from_409 test
    // moved to crates/superdeduper-bench-real/src/submission_http.rs alongside
    // the function it covers. See `duplicate_submission_id_from_409_extracts_id`
    // / `_handles_absent_field` / `_handles_malformed_json` in that module.

    #[test]
    fn serializable_outcome_round_trips_all_variants() {
        // The archive + review JSON files store outcomes via
        // SerializableOutcome (tagged enum). If serde derive ever
        // breaks for one variant, prior archive entries become
        // unreadable. Round-trip each variant.
        let variants = vec![
            SubmitOutcome::Accepted {
                submission_id: "abc".into(),
                ranks: vec![RankEntry {
                    category: "throughput".into(),
                    bracket: "9950X3D2".into(),
                    rank: 4,
                    bucket_size: 412,
                }],
                achievements_unlocked: vec!["hoarder".into(), "big-find".into()],
                profile_url: Some("https://superdeduper.io/profile/abc".into()),
            },
            SubmitOutcome::DuplicateNoChange { submission_id: None },
            SubmitOutcome::Rejected {
                status: 422,
                reason: "{\"error\":\"schema_invalid\"}".into(),
            },
            SubmitOutcome::Transient {
                reason: "transport: connection refused".into(),
            },
            SubmitOutcome::FlaggedForReview {
                review_id: Some("review-xyz".into()),
                local_path: "/tmp/x/y.json".into(),
                original_status: 400,
                original_reason: "{\"error\":\"install_id_missing\"}".into(),
            },
        ];
        for v in &variants {
            let s = SerializableOutcome::from(v);
            let bytes = serde_json::to_vec(&s).expect("serialize");
            let _: SerializableOutcome = serde_json::from_slice(&bytes).expect("deserialize");
        }
    }

    #[test]
    fn iso8601_handles_epoch_and_modern_dates() {
        // Defensive — exercise the civil-from-days math at a few
        // anchors so a future refactor that touches it can't break
        // silently.
        assert_eq!(iso8601_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_from_unix(1_704_067_200), "2024-01-01T00:00:00Z");
        // Anchor used by the rfc3339 unit test elsewhere.
        assert_eq!(iso8601_from_unix(1_779_594_153), "2026-05-24T03:42:33Z");
    }

    #[test]
    fn now_iso8601_format_is_well_formed() {
        // Shape: YYYY-MM-DDTHH:MM:SSZ — 20 chars, ends with Z,
        // contains T as the date/time separator. Catches a future
        // off-by-one or format-string typo in the rendering.
        let ts = now_iso8601();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert!(ts.chars().nth(10) == Some('T'));
    }

    #[test]
    fn build_payload_clamps_install_id_into_output() {
        // Defensive — the install_id arg is the ONLY way the freshest
        // post-rotation install_id enters the payload. A future
        // refactor that drops the threading would silently submit
        // against a stale id. Pin the wire-format claim.
        let inputs = sample_inputs();
        let p1 = build_payload(&inputs, "id-a");
        let p2 = build_payload(&inputs, "id-b");
        assert_eq!(p1.get("install_id").and_then(|v| v.as_str()), Some("id-a"));
        assert_eq!(p2.get("install_id").and_then(|v| v.as_str()), Some("id-b"));
    }

    #[test]
    fn action_bytes_keys_match_locked_schema() {
        // Web's `lifetime-audit.ts` validates against these exact
        // strings; drift here = backend rejects submissions. Test
        // pins the constants byte-for-byte so a casual rename
        // surfaces here first.
        assert_eq!(
            ACTION_BYTES_KEY_DELETED_TO_RECYCLE,
            "deleted_to_recycle_bytes"
        );
        assert_eq!(
            ACTION_BYTES_KEY_DELETED_PERMANENTLY,
            "deleted_permanently_bytes"
        );
        assert_eq!(
            ACTION_BYTES_KEY_HARDLINK_REPLACED,
            "hardlink_replaced_bytes"
        );
    }

    /// #144 — the committed schema/submit.schema.json must match
    /// the schema derived from the current struct definitions. A
    /// field rename / add / type change without regenerating the
    /// committed file fails here — the #96 root-cause CI gate.
    ///
    /// Regenerate after an intentional struct change:
    /// `SD_UPDATE_SCHEMA=1 cargo test --features telemetry
    /// submission_schema_matches_committed`
    #[test]
    fn submission_schema_matches_committed() {
        let derived = wire_schema_json();
        let committed_path = concat!(env!("CARGO_MANIFEST_DIR"), "/schema/submit.schema.json");
        if std::env::var("SD_UPDATE_SCHEMA").is_ok() {
            std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/schema"))
                .expect("create schema dir");
            std::fs::write(committed_path, &derived).expect("write schema");
            eprintln!("SD_UPDATE_SCHEMA: wrote {committed_path}");
            return;
        }
        let committed = std::fs::read_to_string(committed_path).unwrap_or_else(|e| {
            panic!(
                "schema/submit.schema.json missing or unreadable ({e}). \
                 Regenerate with SD_UPDATE_SCHEMA=1 cargo test --features \
                 telemetry submission_schema_matches_committed"
            )
        });
        assert_eq!(
            derived, committed,
            "the submission wire structs (RunShape / ResultSummary / \
             HardwareFingerprint) changed but schema/submit.schema.json \
             was not regenerated. This is the #96 drift gate — a field \
             change must bump the canonical schema in lockstep. \
             Regenerate: SD_UPDATE_SCHEMA=1 cargo test --features telemetry \
             submission_schema_matches_committed, review the diff, commit it.",
        );
    }

    #[test]
    fn action_bytes_keys_round_trip_through_btreemap() {
        // Round-trip the locked keys through the actual
        // BTreeMap<String, u64> field to confirm they encode
        // cleanly as JSON object keys (no surprise quoting / Unicode
        // shenanigans).
        let mut m = std::collections::BTreeMap::new();
        m.insert(ACTION_BYTES_KEY_DELETED_TO_RECYCLE.to_string(), 1234u64);
        m.insert(ACTION_BYTES_KEY_DELETED_PERMANENTLY.to_string(), 5678u64);
        m.insert(ACTION_BYTES_KEY_HARDLINK_REPLACED.to_string(), 9012u64);
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"deleted_to_recycle_bytes\":1234"));
        assert!(json.contains("\"deleted_permanently_bytes\":5678"));
        assert!(json.contains("\"hardlink_replaced_bytes\":9012"));
        let back: std::collections::BTreeMap<String, u64> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get(ACTION_BYTES_KEY_DELETED_TO_RECYCLE), Some(&1234));
    }

    // =====================================================================
    // Phase B.5 option (b) -- effective_lane / cold-enforce override.
    // Mick GO 2026-05-31; design 10:35 PST.
    // =====================================================================

    /// Construct a bench-shaped SubmissionInputs for the lane tests. `bench`
    /// + `lane` are caller-controlled so the lane-override matrix can flip
    /// each independently.
    fn bench_inputs(cold_enforced: Option<bool>, lane: Option<&str>) -> SubmissionInputs {
        let mut inputs = sample_inputs();
        inputs.run_shape.scope = "canonical-bench".into();
        inputs.run_shape.corpus_kind = "canonical-bench".into();
        inputs.bench = cold_enforced.map(|ce| CanonicalBench {
            protocol_version: "tbench-1".into(),
            corpus_version: "corpus-v1-quick".into(),
            tier: "quick".into(),
            bench_run_id: "run-xyz".into(),
            bench_proof: serde_json::json!({
                "answers": [{ "path_index": 0, "byte_offset": 0, "byte_length": 1, "challenge_hash": "" }],
                "result_digest": "",
            }),
            cold_enforced: ce,
        });
        inputs.lane = lane.map(str::to_string);
        inputs
    }

    /// Happy path: cold-enforced bench with explicit ranked preference --
    /// the user's lane choice flows through unchanged.
    #[test]
    fn build_payload_preserves_ranked_when_cold_enforced_true() {
        let inputs = bench_inputs(Some(true), Some("ranked"));
        let body = build_payload(&inputs, "id");
        assert_eq!(body.get("lane").and_then(|v| v.as_str()), Some("ranked"));
    }

    /// Phase B.5 KEYSTONE: cold-enforce false with user-requested ranked
    /// MUST be overridden to casual. This is the bug the slice closes.
    /// Phase 2-B v0.3.20: effective_lane helper now lives in bench-real;
    /// the wire-output assertion is the user-visible contract.
    #[test]
    fn build_payload_forces_casual_when_cold_enforced_false_overrides_ranked_request() {
        let inputs = bench_inputs(Some(false), Some("ranked"));
        let body = build_payload(&inputs, "id");
        assert_eq!(
            body.get("lane").and_then(|v| v.as_str()),
            Some("casual"),
            "cold-enforced false MUST force the wire body.lane to casual; \
             the user's ranked preference is overridden by the cold-enforce gate"
        );
    }

    /// cold-enforce false with no lane preference still emits casual on the
    /// wire -- the override fires regardless of whether the user asked for
    /// a specific lane.
    #[test]
    fn build_payload_forces_casual_when_cold_enforced_false_and_lane_unset() {
        let inputs = bench_inputs(Some(false), None);
        let body = build_payload(&inputs, "id");
        assert_eq!(body.get("lane").and_then(|v| v.as_str()), Some("casual"));
    }

    /// cold-enforced true with no lane preference: no override fires; the
    /// wire body simply omits the lane key, server falls back to its
    /// account-linkage gating (the legacy / pre-Mick-bench-lane behavior).
    #[test]
    fn build_payload_omits_lane_key_when_cold_enforced_true_and_lane_unset() {
        let inputs = bench_inputs(Some(true), None);
        let body = build_payload(&inputs, "id");
        assert!(
            body.get("lane").is_none(),
            "cold-enforced true with no lane preference must omit body.lane"
        );
    }

    /// Casual already chosen by user + cold-enforced true: pass-through,
    /// no override needed. Pins that the override is only ONE-DIRECTIONAL
    /// (ranked -> casual on cold-enforce false), never the reverse.
    #[test]
    fn build_payload_preserves_casual_choice_unchanged() {
        let inputs = bench_inputs(Some(true), Some("casual"));
        let body = build_payload(&inputs, "id");
        assert_eq!(body.get("lane").and_then(|v| v.as_str()), Some("casual"));
    }

    /// Non-bench submission (ordinary scan) with a lane preference: the
    /// override doesn't apply because there's no bench block, so no
    /// cold_enforced gate to honour. The lane flows through verbatim --
    /// pre-Phase-B.5 behavior preserved for ordinary scans.
    #[test]
    fn build_payload_does_not_override_non_bench_submission() {
        let mut inputs = sample_inputs();
        inputs.bench = None;
        inputs.lane = Some("ranked".to_string());
        let body = build_payload(&inputs, "id");
        assert_eq!(body.get("lane").and_then(|v| v.as_str()), Some("ranked"));
    }

    /// Non-bench submission with no lane preference: the wire body simply
    /// omits the lane key (legacy ordinary-scan shape).
    #[test]
    fn build_payload_non_bench_with_no_lane_omits_lane_key() {
        let inputs = sample_inputs();
        let body = build_payload(&inputs, "id");
        assert!(body.get("lane").is_none());
    }
}
