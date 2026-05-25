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

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::pipeline::{DuplicateGroup, SimilarityKind};

/// Bump on every incompatible schema change. v2's resubmit work
/// will add the submission payload + receipts fields — those fields
/// will be `#[serde(default)]` so v1 → v2 reads cleanly without
/// version bump; bump only when removing/renaming a v1 field.
///
/// v2: added `groups_by_similarity_kind` (#49). The new field has
/// `#[serde(default)]` so v1 rows on disk still deserialise — the
/// map just lands empty. The version bump is informational; future
/// loaders that want to display the breakdown only when present
/// can check `schema_version >= 2`.
///
/// v3: #41 — added the resubmit-pipeline state. New fields:
///   * `submission_payload: Option<Value>` — captured HMAC-ready
///     JSON at scan-finish so resubmit is a single POST against
///     the recorded payload (not a rebuild — drift across
///     install-rotate would invalidate the signature).
///   * `built_with_install_id: Option<String>` — install_id at
///     payload-build time. Used to detect "user reset their
///     install between scan + resubmit" (HMAC would mismatch);
///     resubmit surfaces this as a clear error rather than
///     silently failing on the server side.
///   * `last_attempt_at_unix: Option<u64>` + `attempt_count: u32`
///     — drives the crash-detection modal's "older than N
///     minutes" filter + the user-visible "retried 3 times"
///     display.
///   * `submission_channel: Option<String>` — channel slug at
///     scan time, captured separately from the live channel so
///     channel-aware resubmit can route the POST against the
///     ORIGINAL channel even after the user switched. Cross-
///     channel resubmit is blocked with a surfaced error.
///
/// All v3 fields are `#[serde(default)]`; v1/v2 rows still load.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

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
    /// #49 — per-`SimilarityKind` group counts for the scan.
    /// Keys are the lowercase-kebab serialisation of `SimilarityKind`
    /// (e.g. `"byte-identical"`, `"perceptual-image"`,
    /// `"perceptual-audio"`); values are group counts. Empty when
    /// the scan produced no groups, and ALWAYS empty on v1 rows
    /// loaded from disk (the field is `#[serde(default)]`).
    ///
    /// Lets the GUI History tab display "32 perceptual + 30
    /// byte-identical" instead of "62 groups total", and lets the
    /// resubmit semantics in #41 v2 reconcile against the original
    /// composition.
    #[serde(default)]
    pub groups_by_similarity_kind: BTreeMap<String, u64>,
    /// #41 — captured HMAC-ready submission JSON at scan-finish.
    /// `Some(_)` ⇒ the row is resubmittable (POST this body with
    /// the install's key + the recorded \[X-Sd-Signature\] header
    /// the resubmitter computes from \[built_with_install_id\]).
    /// `None` ⇒ v1/v2 row from before the payload was persisted,
    /// or the build failed at scan time (telemetry off etc.).
    #[serde(default)]
    pub submission_payload: Option<serde_json::Value>,
    /// #41 — install_id captured at payload-build time so the
    /// resubmitter can detect "user reset their install between
    /// scan + resubmit." If `current install_id != built_with_install_id`,
    /// the HMAC under the new key would mismatch + the server
    /// would 401; better to surface that BEFORE the POST.
    #[serde(default)]
    pub built_with_install_id: Option<String>,
    /// #41 — channel slug at scan time, captured separately from
    /// the live channel. Resubmit routes against this channel
    /// rather than the live one — cross-channel resubmit is
    /// blocked with a surfaced error.
    #[serde(default)]
    pub submission_channel: Option<String>,
    /// #41 — unix seconds of the most recent resubmit attempt.
    /// `None` ⇒ no attempt yet (just-finished row). Drives the
    /// crash-detection modal's "older than N minutes" filter.
    #[serde(default)]
    pub last_attempt_at_unix: Option<u64>,
    /// #41 — total resubmit attempts so far. Surface in the
    /// History panel after `attempt_count >= 2` so the user
    /// knows the row has been retried.
    #[serde(default)]
    pub attempt_count: u32,
}

/// Build the `groups_by_similarity_kind` map for a finished scan's
/// duplicate-group vec. Empty map for empty input. Used by both the
/// CLI + GUI scan-finish sites; centralised here so the slug strings
/// stay in lock-step with whatever `#[serde(rename_all = "kebab-case")]`
/// emits for `SimilarityKind`.
pub fn similarity_kind_breakdown(groups: &[DuplicateGroup]) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    for g in groups {
        let slug = match g.similarity_kind {
            SimilarityKind::ByteIdentical => "byte-identical",
            SimilarityKind::PerceptualImage => "perceptual-image",
            SimilarityKind::PerceptualAudio => "perceptual-audio",
        };
        *out.entry(slug.to_string()).or_insert(0) += 1;
    }
    out
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
        groups_by_similarity_kind: BTreeMap<String, u64>,
    ) -> Self {
        let channel_string = channel.into();
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            scan_id,
            started_at_unix,
            completed_at_unix: Some(unix_now()),
            // submission_channel mirrors channel at construction
            // time so the v3 resubmit pipeline has a stable
            // routing target even if the user switches channels
            // between scan + resubmit.
            submission_channel: Some(channel_string.clone()),
            channel: channel_string,
            roots,
            total_files,
            total_bytes_read,
            total_dups,
            reclaimable_bytes,
            submission_state: SubmissionState::Pending,
            groups_by_similarity_kind,
            submission_payload: None,
            built_with_install_id: None,
            last_attempt_at_unix: None,
            attempt_count: 0,
        }
    }

    /// #41 — attach a freshly-built HMAC-ready submission body
    /// (`Some(value)`) and the install_id under which it was
    /// built. Called after `new_finished` once the leaderboard
    /// module has assembled the payload. Telemetry-off builds
    /// skip this step and leave both fields `None`; the History
    /// panel renders the row but the Resubmit button stays
    /// disabled.
    pub fn with_submission_payload(
        mut self,
        payload: serde_json::Value,
        install_id: impl Into<String>,
    ) -> Self {
        self.submission_payload = Some(payload);
        self.built_with_install_id = Some(install_id.into());
        self
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
    // Sort: started_at desc, scan_id asc as a tiebreaker. Without
    // the secondary key, N parallel scans within the same wall-clock
    // second sort in undefined order (testrunner #38 v1 Gap 2 — they
    // hit this in their orchestrator's mock with five concurrent
    // scans). scan_id is a 32-hex random string, so lexical asc
    // gives a stable + arbitrary tiebreak.
    out.sort_by(|a, b| {
        b.started_at_unix
            .cmp(&a.started_at_unix)
            .then_with(|| a.scan_id.cmp(&b.scan_id))
    });
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

/// #41 — load the row, transition its `submission_state` (+ bump
/// `last_attempt_at_unix` + `attempt_count`), write it back atomically.
/// Used by the resubmit pipeline after each POST attempt and by the
/// app-start interrupted-scan reaper.
///
/// Returns `Ok(false)` if the row doesn't exist (race against the
/// user clicking Delete while a resubmit was in flight) — caller
/// treats that as a no-op, not an error. Bubbles up real IO/parse
/// errors.
pub fn update_submission_state(
    scan_id: &str,
    state: SubmissionState,
    increment_attempt: bool,
) -> io::Result<bool> {
    let mut record = match load(scan_id)? {
        Some(r) => r,
        None => return Ok(false),
    };
    record.submission_state = state;
    if increment_attempt {
        record.last_attempt_at_unix = Some(unix_now());
        record.attempt_count = record.attempt_count.saturating_add(1);
    }
    record_completed(&record)?;
    Ok(true)
}

/// #41 — list every row whose `submission_state == Pending` and
/// whose most recent activity is older than `threshold_secs` from
/// `now`. "Most recent activity" = `last_attempt_at_unix` when
/// present, else `completed_at_unix`, else `started_at_unix`.
///
/// Drives the app-start "Resubmit N pending scans?" modal — we
/// don't want to nag the user about a Pending row that just
/// finished 30 seconds ago in this session.
pub fn list_pending_older_than(threshold_secs: u64) -> io::Result<Vec<ScanRecord>> {
    let now = unix_now();
    let cutoff = now.saturating_sub(threshold_secs);
    let mut out: Vec<ScanRecord> = list()?
        .into_iter()
        .filter(|r| r.submission_state == SubmissionState::Pending)
        .filter(|r| {
            let latest = r
                .last_attempt_at_unix
                .or(r.completed_at_unix)
                .unwrap_or(r.started_at_unix);
            latest <= cutoff
        })
        .collect();
    // list() already sorts newest-first; keep that ordering so the
    // modal renders the most-recent pending entry first.
    out.sort_by(|a, b| {
        b.started_at_unix
            .cmp(&a.started_at_unix)
            .then_with(|| a.scan_id.cmp(&b.scan_id))
    });
    Ok(out)
}

/// #41 — delete every row whose `started_at_unix` is older than
/// `retention_secs` from `now`. Best-effort: failures on individual
/// files are logged + swallowed so one corrupt row can't block
/// retention enforcement on the rest. Returns the count of rows
/// actually removed.
///
/// Pass `0` to disable retention (returns 0 without touching any
/// file). The GUI's Settings → Privacy widget treats "forever" as
/// `retention_secs == 0`.
pub fn prune_older_than(retention_secs: u64) -> io::Result<u64> {
    if retention_secs == 0 {
        return Ok(0);
    }
    let now = unix_now();
    let cutoff = now.saturating_sub(retention_secs);
    let mut pruned = 0u64;
    for record in list()? {
        if record.started_at_unix < cutoff {
            match delete(&record.scan_id) {
                Ok(()) => pruned += 1,
                Err(e) => tracing::warn!(
                    scan_id = %record.scan_id,
                    error = %e,
                    "scan_history: prune failed (ignored, will retry next pass)",
                ),
            }
        }
    }
    Ok(pruned)
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
            BTreeMap::new(),
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
                BTreeMap::new(),
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
        let good = ScanRecord::new_finished(
            new_scan_id(),
            1_700_000_000,
            "prod",
            vec![],
            1,
            1,
            0,
            0,
            BTreeMap::new(),
        );
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
        let record = ScanRecord::new_finished(
            new_scan_id(),
            1_700_000_000,
            "prod",
            vec![],
            1,
            1,
            0,
            0,
            BTreeMap::new(),
        );
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

    /// #49 — `similarity_kind_breakdown` slugs in lock-step with
    /// `SimilarityKind`'s `#[serde(rename_all = "kebab-case")]`,
    /// and the per-variant tally is correct.
    #[test]
    fn similarity_kind_breakdown_counts_each_variant() {
        use crate::pipeline::{DuplicateGroup, SimilarityKind};
        let g = |kind: SimilarityKind| DuplicateGroup {
            similarity_kind: kind,
            ..Default::default()
        };
        let groups = vec![
            g(SimilarityKind::ByteIdentical),
            g(SimilarityKind::ByteIdentical),
            g(SimilarityKind::ByteIdentical),
            g(SimilarityKind::PerceptualImage),
            g(SimilarityKind::PerceptualImage),
            g(SimilarityKind::PerceptualAudio),
        ];
        let out = similarity_kind_breakdown(&groups);
        assert_eq!(out.get("byte-identical").copied(), Some(3));
        assert_eq!(out.get("perceptual-image").copied(), Some(2));
        assert_eq!(out.get("perceptual-audio").copied(), Some(1));
        assert_eq!(out.len(), 3, "no extra keys: {out:?}");
    }

    #[test]
    fn similarity_kind_breakdown_empty_input() {
        let out = similarity_kind_breakdown(&[]);
        assert!(
            out.is_empty(),
            "empty groups → empty map (avoids zero-padded keys)"
        );
    }

    /// #41 — update_submission_state flips the state + bumps
    /// last_attempt_at_unix + attempt_count, then writes back.
    #[test]
    fn update_submission_state_round_trips() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("update-state");
        let id = new_scan_id();
        let r = ScanRecord::new_finished(
            id.clone(),
            1_700_000_000,
            "prod",
            vec![],
            1,
            1,
            0,
            0,
            BTreeMap::new(),
        );
        record_completed(&r).unwrap();

        let touched = update_submission_state(&id, SubmissionState::Failed, true).unwrap();
        assert!(touched, "row existed, so update should report true");
        let after = load(&id).unwrap().expect("row still loadable");
        assert_eq!(after.submission_state, SubmissionState::Failed);
        assert_eq!(after.attempt_count, 1);
        assert!(after.last_attempt_at_unix.is_some());

        // Another transition, no attempt bump.
        update_submission_state(&id, SubmissionState::Submitted, false).unwrap();
        let again = load(&id).unwrap().unwrap();
        assert_eq!(again.submission_state, SubmissionState::Submitted);
        assert_eq!(
            again.attempt_count, 1,
            "second update with increment_attempt=false should not bump"
        );

        // Missing row → Ok(false), not error.
        let missing = update_submission_state("does-not-exist", SubmissionState::Failed, true)
            .expect("missing row is not an error");
        assert!(!missing);
    }

    /// #41 — `prune_older_than(0)` is a no-op (used by GUI when the
    /// retention setting is "forever").
    #[test]
    fn prune_older_than_zero_is_noop() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("prune-zero");
        let id = new_scan_id();
        let r = ScanRecord::new_finished(
            id.clone(),
            1_000,
            "prod",
            vec![],
            1,
            1,
            0,
            0,
            BTreeMap::new(),
        );
        record_completed(&r).unwrap();
        let pruned = prune_older_than(0).unwrap();
        assert_eq!(pruned, 0);
        assert!(load(&id).unwrap().is_some(), "row should survive");
    }

    /// #41 — `prune_older_than` removes rows whose started_at is
    /// older than the cutoff and leaves fresher rows alone.
    #[test]
    fn prune_older_than_removes_only_aged_rows() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("prune-aged");
        let old = ScanRecord::new_finished(
            new_scan_id(),
            1_000, // ancient
            "prod",
            vec![],
            1,
            1,
            0,
            0,
            BTreeMap::new(),
        );
        let fresh = ScanRecord::new_finished(
            new_scan_id(),
            unix_now(), // right now
            "prod",
            vec![],
            1,
            1,
            0,
            0,
            BTreeMap::new(),
        );
        let old_id = old.scan_id.clone();
        let fresh_id = fresh.scan_id.clone();
        record_completed(&old).unwrap();
        record_completed(&fresh).unwrap();
        // 1-day cutoff = anything older than 24h must go.
        let pruned = prune_older_than(86_400).unwrap();
        assert_eq!(pruned, 1, "only the ancient row should be pruned");
        assert!(load(&old_id).unwrap().is_none(), "ancient row was removed");
        assert!(load(&fresh_id).unwrap().is_some(), "fresh row survived");
    }

    /// #41 — `list_pending_older_than` filters by state AND age.
    #[test]
    fn list_pending_older_than_filters_state_and_age() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _td = isolate("pending-sweep");
        // Three rows: aged-pending (should match), fresh-pending
        // (should not — under threshold), aged-submitted (should
        // not — wrong state).
        let now = unix_now();
        let aged_pending = ScanRecord {
            started_at_unix: now.saturating_sub(3_600),
            completed_at_unix: Some(now.saturating_sub(3_500)),
            ..ScanRecord::new_finished(
                new_scan_id(),
                now.saturating_sub(3_600),
                "prod",
                vec![],
                1,
                1,
                0,
                0,
                BTreeMap::new(),
            )
        };
        let aged_pending_id = aged_pending.scan_id.clone();
        let fresh_pending = ScanRecord::new_finished(
            new_scan_id(),
            now,
            "prod",
            vec![],
            1,
            1,
            0,
            0,
            BTreeMap::new(),
        );
        let mut aged_submitted = ScanRecord {
            started_at_unix: now.saturating_sub(3_600),
            completed_at_unix: Some(now.saturating_sub(3_500)),
            ..ScanRecord::new_finished(
                new_scan_id(),
                now.saturating_sub(3_600),
                "prod",
                vec![],
                1,
                1,
                0,
                0,
                BTreeMap::new(),
            )
        };
        aged_submitted.submission_state = SubmissionState::Submitted;

        record_completed(&aged_pending).unwrap();
        record_completed(&fresh_pending).unwrap();
        record_completed(&aged_submitted).unwrap();

        let rows = list_pending_older_than(300).unwrap();
        let matched: Vec<&str> = rows.iter().map(|r| r.scan_id.as_str()).collect();
        assert_eq!(
            matched,
            vec![aged_pending_id.as_str()],
            "only aged + pending should match (fresh-pending excluded by age; \
             aged-submitted excluded by state)"
        );
    }

    /// #49 — v1 rows on disk (no `groups_by_similarity_kind` field)
    /// still deserialise cleanly under v2's struct; the new field
    /// defaults to an empty map.
    #[test]
    fn deserialises_v1_record_with_empty_kind_breakdown() {
        // Hand-crafted v1 JSON — no `groups_by_similarity_kind` key.
        let v1 = r#"{
            "schema_version": 1,
            "scan_id": "v1-row",
            "started_at_unix": 1700000000,
            "completed_at_unix": 1700000100,
            "channel": "prod",
            "roots": ["/tmp/c"],
            "total_files": 12,
            "total_bytes_read": 4096,
            "total_dups": 3,
            "reclaimable_bytes": 1024,
            "submission_state": "pending"
        }"#;
        let rec: ScanRecord = serde_json::from_str(v1).expect("v1 JSON must parse under v2 schema");
        assert_eq!(rec.schema_version, 1);
        assert!(
            rec.groups_by_similarity_kind.is_empty(),
            "missing field defaults to empty map: {:?}",
            rec.groups_by_similarity_kind
        );
    }
}
