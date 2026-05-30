//! #99 — persistent action-credit queue. When the engine fires a dedupe
//! action but cannot immediately PATCH `/api/v1/submit/{id}/actions` (because
//! `peek_pending_submission_id()` is None AND no in-memory `SubmissionInputs`
//! is available for an auto-submit fallback), the credit gets queued to disk
//! here. The queue drains on the next successful `/submit` POST.
//!
//! Layout: `<data_dir>/install/pending_actions.json`, a JSON array of
//! [`PendingAction`] entries. Per-install (lives alongside `install.json`), so
//! "Reset install" wipes the queue with the rest of the install state.
//!
//! Failure mode it covers: process restart between `scan complete -> store_pending(inputs)`
//! and the user clicking an action. At that point the in-memory
//! `submission::PENDING` slot is gone (OnceLock dies on restart), so auto-
//! submit can't reconstruct the scan payload from memory. The disk queue
//! holds the action credit until ANY future `/submit` succeeds, then folds
//! the queued bytes into that new submission's PATCH.
//!
//! Tradeoffs:
//! - Bytes-correct lifetime (user's reclaim is never silently dropped).
//! - Attribution is approximate: queued credits get folded under whichever
//!   submission_id arrives next, not the scan that produced them. The
//!   alternative (rebinding via run_uuid) needs a web-contract change and is
//!   the broader scope (D).
//! - Stale-queue policy: drop entries older than [`STALE_AGE_SECS`] (30 days)
//!   on every drain attempt, so a re-imaged install doesn't credit ancient
//!   queued bytes against an unrelated future submission.

#![cfg(feature = "telemetry")]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::install;

/// Entries older than this on drain attempt are dropped (not folded into the
/// new submission). 30 days. A re-imaged install or long-paused user
/// shouldn't credit ancient bytes against an unrelated future scan.
pub const STALE_AGE_SECS: i64 = 30 * 24 * 60 * 60;

/// One queued action credit. `action_key` matches the `LOCKED_ACTION_KEYS` set
/// (e.g. `deleted_to_recycle_bytes`); `ok_bytes` is the same value the
/// `actions_taken_summary` PATCH would carry. `run_uuid` and `queued_at`
/// are forensic — useful for diagnosing where a queued credit came from
/// without changing PATCH semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAction {
    pub action_key: String,
    pub ok_bytes: u64,
    pub run_uuid: String,
    pub queued_at: i64,
}

fn pending_file() -> io::Result<PathBuf> {
    let mut p = install::data_dir_public()?;
    p.push("install");
    fs::create_dir_all(&p)?;
    p.push("pending_actions.json");
    Ok(p)
}

/// Append a single pending entry to the disk queue. Round-trips through
/// `load_all` so we don't truncate an existing list (or race with another
/// caller in the same process — the read-write is wrapped per-call; for
/// concurrent processes there's no cross-process lock today, but the v0.2.39
/// flow is single-writer per install at action-fire time).
pub fn append(entry: &PendingAction) -> io::Result<()> {
    let path = pending_file()?;
    let mut list = load_all()?;
    list.push(entry.clone());
    let json = serde_json::to_vec_pretty(&list)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&path, json)
}

/// Snapshot the current queue without removing anything.
pub fn peek() -> Vec<PendingAction> {
    load_all().unwrap_or_default()
}

/// True when the queue file exists and contains at least one entry.
/// Used to fast-skip the drain attempt on the hot `/submit` Accepted path
/// when there's nothing queued (the common case).
pub fn is_empty() -> bool {
    peek().is_empty()
}

/// Drain the queue: aggregate non-stale entries by `action_key` (summing
/// `ok_bytes`), DELETE the queue file, and return the aggregate. The PATCH
/// `actions_taken_summary` UPSERT semantics make summing safe (server
/// receives the COMPLETE map and treats it as source of truth). Stale entries
/// (queued > [`STALE_AGE_SECS`] ago) are dropped silently — see module docs.
pub fn drain_aggregated() -> io::Result<BTreeMap<String, u64>> {
    let entries = load_all()?;
    let path = pending_file()?;
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    let now = crate::time::now_unix_i64();
    let mut agg: BTreeMap<String, u64> = BTreeMap::new();
    for e in entries {
        let age = now.saturating_sub(e.queued_at);
        if age > STALE_AGE_SECS {
            // Stale — drop. The user's lifetime number is uplifted at the
            // moment the action runs locally (history credit), so dropping
            // the server-side delivery isn't catastrophic; just no remote
            // tally for very old queued credits.
            crate::log_warn!(
                "pending_actions: dropping stale entry (age={age}s > {STALE_AGE_SECS}s) for {} ({})",
                e.action_key,
                e.run_uuid
            );
            continue;
        }
        *agg.entry(e.action_key).or_insert(0) += e.ok_bytes;
    }
    Ok(agg)
}

fn load_all() -> io::Result<Vec<PendingAction>> {
    let path = pending_file()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path)?;
    // Treat 0-byte / whitespace-only files as empty queue rather than a parse
    // error. The file can legitimately exist as 0 bytes if a prior write was
    // interrupted before flush.
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("pending_actions json parse: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::OnceLock;

    /// Both tests in this module mutate the real per-platform data_dir's
    /// pending_actions.json. cargo's `test` defaults to multi-thread within a
    /// test binary, so we explicitly serialise these two with a mutex. Each
    /// test wipes the file before + after to ensure clean state.
    fn test_lock() -> &'static Mutex<()> {
        static SLOT: OnceLock<Mutex<()>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(()))
    }

    /// File-IO smoke: append two entries (different keys), peek, then drain;
    /// aggregate should sum within each key, the file should be gone after
    /// drain.
    #[test]
    fn append_peek_drain_round_trips() {
        let _g = test_lock().lock();
        let _ = std::fs::remove_file(pending_file().unwrap());
        let now = crate::time::now_unix_i64();
        append(&PendingAction {
            action_key: "deleted_to_recycle_bytes".into(),
            ok_bytes: 1_000,
            run_uuid: format!("test-{}", std::process::id()),
            queued_at: now,
        })
        .unwrap();
        append(&PendingAction {
            action_key: "deleted_to_recycle_bytes".into(),
            ok_bytes: 2_500,
            run_uuid: format!("test-{}-b", std::process::id()),
            queued_at: now,
        })
        .unwrap();
        append(&PendingAction {
            action_key: "hardlink_replaced_bytes".into(),
            ok_bytes: 500,
            run_uuid: format!("test-{}-c", std::process::id()),
            queued_at: now,
        })
        .unwrap();
        assert_eq!(peek().len(), 3);
        let agg = drain_aggregated().unwrap();
        assert_eq!(agg.get("deleted_to_recycle_bytes"), Some(&3_500));
        assert_eq!(agg.get("hardlink_replaced_bytes"), Some(&500));
        assert!(peek().is_empty(), "drain must remove the file");
    }

    #[test]
    fn drain_drops_stale_entries_silently() {
        let _g = test_lock().lock();
        let _ = std::fs::remove_file(pending_file().unwrap());
        let now = crate::time::now_unix_i64();
        // Stale: 31 days old.
        append(&PendingAction {
            action_key: "deleted_to_recycle_bytes".into(),
            ok_bytes: 9_999_999,
            run_uuid: "stale".into(),
            queued_at: now - (31 * 24 * 60 * 60),
        })
        .unwrap();
        // Fresh:
        append(&PendingAction {
            action_key: "deleted_to_recycle_bytes".into(),
            ok_bytes: 100,
            run_uuid: "fresh".into(),
            queued_at: now,
        })
        .unwrap();
        let agg = drain_aggregated().unwrap();
        // Stale dropped, only fresh credited.
        assert_eq!(agg.get("deleted_to_recycle_bytes"), Some(&100));
    }
}
