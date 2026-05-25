//! Persistent per-scan history.
//!
//! v1 MVP scope per GH #38: store one JSON file per scan at
//! `<data_dir>/scan-history/<scan_id>.json`. Captures the scan's
//! start/finish timestamps, roots, walker totals, dup stats, and
//! a `submission_state` field that v2 will use to drive a
//! "Resubmit to leaderboard" button.
//!
//! v1 explicitly does NOT include:
//!   * The full HMAC-signed submission payload (v2)
//!   * Action receipts (v2)
//!   * App-start "you have N pending submissions" prompt (v2)
//!   * Crash-detection / interrupted-scan reaping (v2)
//!
//! v1 DOES include:
//!   * `record_completed(...)` hook called from `gui::live::run()` at
//!     scan-finish — every completed scan in the GUI flow now leaves
//!     a history row.
//!   * `list()` returning history newest-first for the upcoming
//!     History tab.
//!   * `load(scan_id)` / `delete(scan_id)` to support detail views +
//!     user-initiated forget.
//!
//! ## Storage layout
//!
//! Per-scan JSON file (not a SQLite DB) for v1 because:
//!   - Each scan record is independent — no cross-row queries
//!     beyond "sort by started_at desc".
//!   - Atomic write via `write-then-rename` is trivially safe with
//!     small JSON files; the SQLite WAL alternative adds a build
//!     dep (rusqlite already pulled in via cache, but adding a new
//!     opens-and-writes-on-every-scan path is more failure surface).
//!   - Inspect-by-hand friendlier when triaging customer reports.
//!
//! Filename is the scan_id as a hyphenated UUID v4 → safe across
//! all three target filesystems (NTFS, ext4, APFS).
//!
//! ## Schema versioning
//!
//! `CURRENT_SCHEMA_VERSION` bumps on incompatible changes. Loaders
//! that see a higher version than they understand skip the row
//! (logs a warning) rather than crashing — forward compat for
//! sd installs that downgrade.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Bump on every incompatible schema change. v2's resubmit work
/// will add the submission payload + receipts fields — those fields
/// will be `#[serde(default)]` so v1 → v2 reads cleanly without
/// version bump; bump only when removing/renaming a v1 field.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Submission state at the time the row was last touched.
/// v1 only ever writes `Pending`; v2 will transition through the
/// other states as the resubmit pipeline lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionState {
    /// Scan completed; payload not yet sent to the leaderboard.
    Pending,
    /// Server accepted the payload.
    Submitted,
    /// Server rejected the payload (4xx) or network failed (5xx /
    /// timeout). v2 surfaces the error in the History panel.
    Failed,
    /// Scan started but did not reach the ScanFinished event before
    /// the app exited. v2 detects this on app start; v1 never writes
    /// this state.
    Interrupted,
}

/// One row in the history. v1 captures the user-visible fields the
/// GUI's read-only list needs. v2 will add the payload + receipts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRecord {
    pub schema_version: u32,
    /// UUID v4, used both as the record's identity AND its filename.
    pub scan_id: String,
    /// Unix epoch seconds when the scan started.
    pub started_at_unix: u64,
    /// Unix epoch seconds when the scan finished. None means the
    /// scan was recorded mid-flight (v2 territory; v1 always writes
    /// this).
    pub completed_at_unix: Option<u64>,
    /// Channel slug (`prod`, `dev`, `local`) captured at scan time.
    pub channel: String,
    /// Roots the user scanned. Stringified for cross-platform JSON.
    pub roots: Vec<String>,
    pub total_files: u64,
    pub total_bytes_read: u64,
    pub total_dups: u64,
    /// Inode-aware reclaim (matches what the GUI header + scan-
    /// finished modal show).
    pub reclaimable_bytes: u64,
    pub submission_state: SubmissionState,
}

impl ScanRecord {
    /// Construct a finished-scan record. Sets `completed_at_unix`
    /// to `now()` and `submission_state` to `Pending` — both v1
    /// invariants.
    #[allow(clippy::too_many_arguments)] // call sites are flat by design; struct-of-args would just
                                         // push the labels to the call site without simplifying it
    pub fn new_finished(
        scan_id: String,
        started_at_unix: u64,
        channel: impl Into<String>,
        roots: Vec<String>,
        total_files: u64,
        total_bytes_read: u64,
        total_dups: u64,
        reclaimable_bytes: u64,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            scan_id,
            started_at_unix,
            completed_at_unix: Some(unix_now()),
            channel: channel.into(),
            roots,
            total_files,
            total_bytes_read,
            total_dups,
            reclaimable_bytes,
            submission_state: SubmissionState::Pending,
        }
    }
}

/// Generate a fresh `scan_id`. Uses a small custom hex generator to
/// avoid pulling `uuid` (which is gated behind the `telemetry`
/// feature) into the always-on path. Hex-encoded 128 bits is enough
/// uniqueness for "filename across one user's machine over years."
pub fn new_scan_id() -> String {
    let mut bytes = [0u8; 16];
    let now_nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix the nanos into the front 8 bytes; xorshift-derived bytes
    // for the back 8. Not crypto; just enough to avoid filename
    // collisions on rapid back-to-back scans.
    bytes[..8].copy_from_slice(&(now_nanos as u64).to_le_bytes());
    let mut x: u64 = now_nanos as u64 ^ 0xdead_beef_cafe_babe;
    for b in &mut bytes[8..] {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Append a finished-scan record. Writes atomically (`.tmp` →
/// rename) so a crash mid-write doesn't leave a half-written file
/// for `list()` to choke on.
pub fn record_completed(record: &ScanRecord) -> io::Result<PathBuf> {
    let dir = history_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", record.scan_id));
    let tmp = dir.join(format!("{}.json.tmp", record.scan_id));
    let json = serde_json::to_string_pretty(record).map_err(io_err)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// All scan records, newest-first. Skips files that don't parse as
/// the current schema (forward-compat + corruption-tolerance).
pub fn list() -> io::Result<Vec<ScanRecord>> {
    let dir = match history_dir() {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // Tolerate corrupt / forward-version files — log + skip.
        // The History panel showing fewer rows is preferable to it
        // refusing to render at all.
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?path, error = %e, "scan_history: read failed; skipping");
                continue;
            }
        };
        let record: ScanRecord = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(?path, error = %e, "scan_history: parse failed; skipping");
                continue;
            }
        };
        if record.schema_version > CURRENT_SCHEMA_VERSION {
            tracing::warn!(
                ?path,
                "scan_history: schema_version {} > {} (newer sd?); skipping",
                record.schema_version,
                CURRENT_SCHEMA_VERSION
            );
            continue;
        }
        out.push(record);
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.started_at_unix));
    Ok(out)
}

/// Load a single record by scan_id. Returns None if not found.
pub fn load(scan_id: &str) -> io::Result<Option<ScanRecord>> {
    let dir = history_dir()?;
    let path = dir.join(format!("{}.json", scan_id));
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Delete a record. Idempotent — already-absent is not an error.
pub fn delete(scan_id: &str) -> io::Result<()> {
    let dir = history_dir()?;
    let path = dir.join(format!("{}.json", scan_id));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn history_dir() -> io::Result<PathBuf> {
    Ok(data_dir()?.join("scan-history"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn io_err(e: impl std::error::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(windows)]
fn data_dir() -> io::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA not set"))?;
    let mut p = PathBuf::from(local);
    p.push("superdeduper");
    Ok(p)
}

#[cfg(target_os = "macos")]
fn data_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("superdeduper");
    Ok(p)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn data_dir() -> io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("superdeduper");
        return Ok(p);
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("share");
    p.push("superdeduper");
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Same env-mutating-test serial-gate pattern as
    // `src/platform/linux/trash.rs::tests`. These tests set
    // HOME / XDG_DATA_HOME to a tempdir to sandbox the history
    // directory; running in parallel would race on the env var.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn isolate(label: &str) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix(&format!("sd-history-test-{label}-"))
            .tempdir()
            .unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
            std::env::set_var("HOME", dir.path());
            #[cfg(windows)]
            std::env::set_var("LOCALAPPDATA", dir.path());
        }
        dir
    }

    #[test]
    fn new_scan_id_is_32_hex_chars() {
        let id = new_scan_id();
        assert_eq!(id.len(), 32, "got {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "got {id}");
    }

    #[test]
    fn new_scan_id_is_unique_across_back_to_back_calls() {
        // The mix-in of nanos + xorshift-derived bytes should
        // disambiguate two calls within the same nanosecond
        // (which Linux's clock_gettime won't even produce, but
        // the property should hold anyway).
        let a = new_scan_id();
        let b = new_scan_id();
        assert_ne!(a, b);
    }

    #[test]
    fn record_completed_then_list_round_trips() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("round-trip");
        let id = new_scan_id();
        let record = ScanRecord::new_finished(
            id.clone(),
            1_700_000_000,
            "prod",
            vec!["/tmp/test-corpus".to_string()],
            42,
            12345,
            5,
            999,
        );
        let path = record_completed(&record).expect("write");
        assert!(path.exists(), "history file should exist at {path:?}");
        let listed = list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].scan_id, id);
        assert_eq!(listed[0].total_files, 42);
        assert_eq!(listed[0].reclaimable_bytes, 999);
        assert_eq!(listed[0].submission_state, SubmissionState::Pending);
    }

    #[test]
    fn list_sorts_newest_first() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("sort");
        for ts in [1700, 1500, 1900, 1100, 1800] {
            let record = ScanRecord::new_finished(
                new_scan_id(),
                ts,
                "prod",
                vec!["/tmp/t".to_string()],
                1,
                1,
                1,
                1,
            );
            record_completed(&record).unwrap();
        }
        let listed = list().unwrap();
        let times: Vec<u64> = listed.iter().map(|r| r.started_at_unix).collect();
        assert_eq!(times, vec![1900, 1800, 1700, 1500, 1100]);
    }

    #[test]
    fn list_skips_unparseable_files() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("skip-bad");
        // Write one good + one garbage file.
        let good =
            ScanRecord::new_finished(new_scan_id(), 1_700_000_000, "prod", vec![], 1, 1, 0, 0);
        record_completed(&good).unwrap();
        let bad_path = history_dir().unwrap().join("notajson.json");
        fs::write(&bad_path, b"{ not valid json at all }").unwrap();
        let listed = list().unwrap();
        assert_eq!(listed.len(), 1, "garbage file should be skipped");
    }

    #[test]
    fn list_skips_forward_version_files() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("forward-version");
        let dir = history_dir().unwrap();
        fs::create_dir_all(&dir).unwrap();
        let future_path = dir.join("future.json");
        // Future schema_version → should be skipped, not crashed.
        fs::write(
            &future_path,
            r#"{"schema_version":99,"scan_id":"future","started_at_unix":1,"completed_at_unix":null,"channel":"prod","roots":[],"total_files":0,"total_bytes_read":0,"total_dups":0,"reclaimable_bytes":0,"submission_state":"pending"}"#,
        )
        .unwrap();
        let listed = list().unwrap();
        assert_eq!(listed.len(), 0);
    }

    #[test]
    fn list_returns_empty_when_dir_missing() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("empty");
        // Dir doesn't exist yet; list() should return Ok([]) not error.
        let listed = list().unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn delete_is_idempotent() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("delete-idempotent");
        // Deleting a non-existent record is a no-op, not an error.
        delete("does-not-exist").unwrap();
        let record =
            ScanRecord::new_finished(new_scan_id(), 1_700_000_000, "prod", vec![], 1, 1, 0, 0);
        record_completed(&record).unwrap();
        delete(&record.scan_id).unwrap();
        delete(&record.scan_id).unwrap(); // again — still no error
        assert_eq!(list().unwrap().len(), 0);
    }

    #[test]
    fn load_returns_none_for_missing() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("load-missing");
        assert!(load("does-not-exist").unwrap().is_none());
    }
}
