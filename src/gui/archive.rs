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

pub const ARCHIVE_SCHEMA: &str = "superdupe.archive.v1";

/// One archive run = one of these JSON files. Filename pattern:
/// `superdupe-archive-manifest-<ISO-timestamp>.json` so multiple
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
