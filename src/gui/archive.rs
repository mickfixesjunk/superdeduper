//! Archive Dupes: write + restore.
//!
//! Two operations on the same on-disk manifest format:
//!
//! * **Write** (during "Archive dupes"): for each non-keeper duplicate
//!   in the current results, move the file under a chosen destination
//!   folder, preserving its source drive letter + directory tree.
//!   Record `(original_path → archived_path)` in a JSON manifest
//!   alongside the moved files.
//!
//! * **Restore** (via File → Open Archive Manifest…): read the
//!   manifest, walk its entries, and move each archived file back to
//!   its original path. Conflicts (something already at the original
//!   path) are surfaced as warnings; we never overwrite.
//!
//! The manifest is the only durable record of where files came from —
//! the move operation is destructive on the source side, so a missing
//! or corrupt manifest means there's no automatic way to undo. That's
//! why we write it atomically (`.tmp` → rename) and include a schema
//! string so future format changes reject old files cleanly.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

pub const ARCHIVE_SCHEMA: &str = "superdeduper.archive.v1";

/// One archive run = one of these JSON files. Filename pattern:
/// `superdeduper-archive-manifest-<ISO-timestamp>.json` so multiple
/// runs into the same destination folder produce distinct files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveManifest {
    pub schema: String,
    pub created_at_unix: u64,
    pub destination: PathBuf,
    pub entries: Vec<ArchiveManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveManifestEntry {
    /// Where the file lived before the archive run.
    pub original_path: PathBuf,
    /// Where the file is now (under `destination`).
    pub archived_path: PathBuf,
    /// The "keeper" sibling that was left in place. Recorded so the
    /// restore loader can mention it in conflict messages and so a
    /// user inspecting the manifest can see what the file was a
    /// duplicate *of*.
    pub keeper_path: PathBuf,
    /// Content hash from the scan that produced this duplicate set.
    /// Stored as the algo's hex output (length depends on algo).
    pub content_hash: String,
    /// Bytes; redundant with the file on disk but useful for
    /// "are you sure you want to restore X GiB?" prompts.
    pub size: u64,
}

/// Outcome of a single entry's restore attempt. Categorised so the
/// summary can show what went wrong without dumping a wall of text.
#[derive(Debug, Clone)]
pub enum RestoreOutcome {
    /// File successfully moved back to its original path.
    Restored,
    /// The archived file isn't where the manifest says it should
    /// be — probably a separate manual move or delete after the
    /// archive run.
    ArchivedMissing,
    /// Something already lives at `original_path`. We never
    /// overwrite — the user has to deal with the conflict.
    OriginalExists,
    /// rename / copy itself errored.
    IoError(String),
}

#[derive(Debug, Clone, Default)]
pub struct RestoreSummary {
    pub restored: u64,
    pub archived_missing: u64,
    pub original_exists: u64,
    pub io_errors: u64,
}

/// #80 Bug C — rollup of an archive run, broken down by outcome
/// and by failure-reason bucket. The `moved_bytes` field is the
/// **only** byte counter that should ever be credited as
/// `archived_bytes` to the leaderboard (per #79). Failed-side
/// bytes never reclaimed any disk and crediting them would be a
/// silent leaderboard-cheating vector — see #80's spec.
///
/// Failure reason buckets:
/// * `failed_access_denied_*` — rename failed, copy succeeded,
///   `remove_file(src)` failed with PermissionDenied. This is the
///   #80 root-cause case (TrustedInstaller-owned dirs). With the
///   Bug A fix the orphan copy has already been removed by the
///   time we tally this bucket.
/// * `failed_cross_device_*` — rename and the copy fallback both
///   failed. Most commonly: cross-device source where the copy
///   itself fails (read denied on the source) or the destination
///   ran out of space mid-copy.
/// * `failed_other_*` — anything else (mkdir failed, generic IO,
///   etc.). Bundled into a single bucket because the user-actionable
///   guidance is the same (\"check the log\").
#[derive(Debug, Clone, Default)]
pub struct ArchiveActionSummary {
    pub moved_count: u64,
    pub moved_bytes: u64,
    pub failed_access_denied_count: u64,
    pub failed_access_denied_bytes: u64,
    pub failed_cross_device_count: u64,
    pub failed_cross_device_bytes: u64,
    pub failed_other_count: u64,
    pub failed_other_bytes: u64,
    pub user_stopped: bool,
    pub destination: PathBuf,
}

impl ArchiveActionSummary {
    pub fn failed_count(&self) -> u64 {
        self.failed_access_denied_count
            + self.failed_cross_device_count
            + self.failed_other_count
    }

    pub fn failed_bytes(&self) -> u64 {
        self.failed_access_denied_bytes
            + self.failed_cross_device_bytes
            + self.failed_other_bytes
    }

    /// Categorise an `std::io::Error` from the move/copy/remove path
    /// into one of the three failure buckets. Used by the archive
    /// worker as it tallies; isolated here so the categorisation
    /// rule is unit-testable.
    pub fn classify_error(err: &std::io::Error) -> ArchiveFailureBucket {
        use std::io::ErrorKind::*;
        match err.kind() {
            PermissionDenied => ArchiveFailureBucket::AccessDenied,
            CrossesDevices | StorageFull => ArchiveFailureBucket::CrossDevice,
            _ => ArchiveFailureBucket::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFailureBucket {
    AccessDenied,
    CrossDevice,
    Other,
}

/// Parse a manifest from disk. Errors are returned verbatim so the
/// caller can surface the message to the user.
pub fn load_manifest(path: &Path) -> Result<ArchiveManifest> {
    let bytes = std::fs::read(path)?;
    let manifest: ArchiveManifest = serde_json::from_slice(&bytes)
        .map_err(|e| crate::Error::other(format!("archive manifest parse: {e}")))?;
    if manifest.schema != ARCHIVE_SCHEMA {
        return Err(crate::Error::other(format!(
            "unknown archive manifest schema {:?} (this build understands {})",
            manifest.schema, ARCHIVE_SCHEMA
        )));
    }
    Ok(manifest)
}

/// Try to move one entry back to its original path. Mirrors the
/// rename → copy+remove fallback the writer uses so cross-volume
/// archives still restore cleanly.
pub fn restore_one(entry: &ArchiveManifestEntry) -> RestoreOutcome {
    if !entry.archived_path.exists() {
        return RestoreOutcome::ArchivedMissing;
    }
    if entry.original_path.exists() {
        return RestoreOutcome::OriginalExists;
    }
    // Ensure the target directory exists. The user may have wiped
    // the original folder between archive + restore; recreating it
    // is the right thing to do for "put it back where it was".
    if let Some(parent) = entry.original_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return RestoreOutcome::IoError(format!("mkdir {}: {e}", parent.display()));
        }
    }
    // Try the fast atomic rename first; fall back to copy + remove
    // for cross-volume moves (rename on Windows fails with ERROR_
    // NOT_SAME_DEVICE when src and dest are on different volumes).
    let direct = std::fs::rename(&entry.archived_path, &entry.original_path);
    if direct.is_ok() {
        return RestoreOutcome::Restored;
    }
    match std::fs::copy(&entry.archived_path, &entry.original_path) {
        Ok(_) => {
            if let Err(e) = std::fs::remove_file(&entry.archived_path) {
                // Copy succeeded but the archive copy is stuck —
                // file is at both locations. Surface this as an
                // IO error so the user knows to clean up.
                return RestoreOutcome::IoError(format!(
                    "restored to {} but couldn't remove {}: {e}",
                    entry.original_path.display(),
                    entry.archived_path.display()
                ));
            }
            RestoreOutcome::Restored
        }
        Err(e) => RestoreOutcome::IoError(format!(
            "copy {} → {}: {e}",
            entry.archived_path.display(),
            entry.original_path.display()
        )),
    }
}
