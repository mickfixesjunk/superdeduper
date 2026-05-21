//! Pause / resume checkpoint that survives an app restart.
//!
//! When the user pauses (or the app exits mid-scan), the engine
//! serialises a `Checkpoint` to `%LOCALAPPDATA%\superdeduper\scan-checkpoint.json`
//! (and the XDG equivalent on non-Windows). On next launch, the GUI
//! offers to resume.
//!
//! The checkpoint is intentionally coarse: it tracks WHICH roots and
//! settings were in play, the set of size-group ranges already
//! confirmed, and the duplicates we'd already reported. Resume
//! re-enumerates from scratch (the inventory phase is cheap relative
//! to hashing) and skips size groups whose checksum already matches
//! the checkpoint's "already-done" list.
//!
//! Anything more granular (per-file resume mid-tier) would require
//! threading state through the rayon-parallel hash, which adds
//! complexity without a clear win — re-running an interrupted hash
//! tier is fast because the cache already has every completed file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gui::events::DuplicateGroupSummary;
use crate::gui::state::{RootEntry, ScanSettings};
use crate::{Error, Result};

/// Lightweight file entry persisted in the checkpoint so resume can
/// skip the inventory walk. Carries just enough to feed Stage 2
/// (size grouping) — paths, size, mtime, and the Windows-only
/// volume_guid / file_ref / usn fields that drive cache invalidation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedFileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub file_ref: u64,
    pub parent_ref: u64,
    pub usn: i64,
    pub attributes: u32,
    pub volume_guid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema: String,
    pub created_at_unix: u64,
    pub roots: Vec<RootEntry>,
    pub settings: ScanSettings,
    /// Hex BLAKE3 prefixes of groups already reported. Used so a
    /// resumed scan doesn't double-report the same duplicates if it
    /// re-encounters them.
    pub completed_hashes: Vec<String>,
    /// Snapshot of duplicates the GUI already had so it can be
    /// restored on resume.
    pub previous_duplicates: Vec<DuplicateGroupSummary>,
    /// Full file list from the inventory walk, persisted so resume
    /// can skip Stage 1 entirely. `None` ⇒ inventory hadn't finished
    /// when the pause fired, and a fresh walk is required.
    #[serde(default)]
    pub saved_inventory: Option<Vec<SavedFileEntry>>,
}

impl Checkpoint {
    pub fn new(roots: Vec<RootEntry>, settings: ScanSettings) -> Self {
        Self {
            schema: "superdeduper.checkpoint.v1".into(),
            created_at_unix: now_unix(),
            roots,
            settings,
            completed_hashes: Vec::new(),
            previous_duplicates: Vec::new(),
            saved_inventory: None,
        }
    }

    pub fn record(&mut self, group: &DuplicateGroupSummary) {
        self.completed_hashes.push(group.content_hash.clone());
        self.previous_duplicates.push(group.clone());
    }
}

/// Default checkpoint file location. Same root as the cache.
pub fn default_checkpoint_path() -> Result<PathBuf> {
    let mut p = crate::cache::default_cache_path()?;
    p.set_file_name("scan-checkpoint.json");
    Ok(p)
}

pub fn load(path: &Path) -> Result<Option<Checkpoint>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let cp: Checkpoint = serde_json::from_slice(&bytes)
        .map_err(|e| Error::other(format!("checkpoint parse: {e}")))?;
    if !cp.schema.starts_with("superdeduper.checkpoint") {
        return Ok(None);
    }
    Ok(Some(cp))
}

pub fn save(path: &Path, cp: &Checkpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(cp)
        .map_err(|e| Error::other(format!("checkpoint encode: {e}")))?;
    // Atomic-ish write: write to .tmp then rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Tiny non-destructive read used by the launch-time Resume/Start
/// Fresh modal. Returns enough to render the prompt (timestamp,
/// roots, dup count) without committing to threading the whole
/// state through the engine. `Ok(None)` = no file present.
/// `Err(_)` = file exists but is corrupt; caller is expected to
/// rename it via [`mark_corrupt`].
#[derive(Debug, Clone)]
pub struct CheckpointSummary {
    pub created_at_unix: u64,
    pub roots: Vec<RootEntry>,
    pub duplicate_count: usize,
    pub has_saved_inventory: bool,
}

pub fn summary(path: &Path) -> Result<Option<CheckpointSummary>> {
    match load(path)? {
        Some(cp) => Ok(Some(CheckpointSummary {
            created_at_unix: cp.created_at_unix,
            roots: cp.roots,
            duplicate_count: cp.previous_duplicates.len(),
            has_saved_inventory: cp.saved_inventory.is_some(),
        })),
        None => Ok(None),
    }
}

/// Rename the existing checkpoint to a timestamped `.bak` sibling so
/// the user can recover from a Start-Fresh later. Filename pattern:
/// `<stem>-<ISO-timestamp>.json.bak`. Idempotent — if the source
/// file doesn't exist, returns `Ok(None)`.
pub fn archive(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = iso_timestamp_now();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("checkpoint");
    let archived = parent.join(format!("{stem}-{stamp}.json.bak"));
    std::fs::rename(path, &archived)?;
    Ok(Some(archived))
}

/// Rename a corrupt checkpoint so the user can inspect it later.
/// Pattern: `<stem>-<ISO-timestamp>.json.corrupt`. Distinct
/// extension from `.bak` so a corrupt file isn't confused with a
/// safely-archived one the user might want to restore.
pub fn mark_corrupt(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = iso_timestamp_now();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("checkpoint");
    let archived = parent.join(format!("{stem}-{stamp}.json.corrupt"));
    std::fs::rename(path, &archived)?;
    Ok(Some(archived))
}

fn iso_timestamp_now() -> String {
    // Filename-safe ISO-8601 in UTC: 2026-05-20T13-45-22Z.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z")
}

fn unix_to_ymdhms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    // Same civil-from-days dance as diagnostics::unix_to_ymdhms; we
    // duplicate it here to keep checkpoint.rs free of cross-module
    // ordering dependencies.
    let days = (secs / 86_400) as i64;
    let h = ((secs % 86_400) / 3600) as u32;
    let m = ((secs % 3600) / 60) as u32;
    let s = (secs % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (y + if mo <= 2 { 1 } else { 0 }) as i32;
    (year, mo, day, h, m, s)
}

pub fn delete(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn roundtrip_preserves_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        let mut cp = Checkpoint::new(
            vec![RootEntry {
                path: PathBuf::from("/tmp/x"),
                is_reference: true,
            }],
            ScanSettings::default(),
        );
        cp.record(&DuplicateGroupSummary {
            size: 4096,
            content_hash: "deadbeef".into(),
            files: vec![PathBuf::from("/tmp/x/a"), PathBuf::from("/tmp/x/b")],
        });
        save(&path, &cp).unwrap();

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.roots.len(), 1);
        assert!(loaded.roots[0].is_reference);
        assert_eq!(loaded.completed_hashes, vec!["deadbeef".to_string()]);
        assert_eq!(loaded.previous_duplicates.len(), 1);
        assert_eq!(loaded.schema, "superdeduper.checkpoint.v1");
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never.json");
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn delete_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        delete(&path).unwrap(); // already absent
        save(&path, &Checkpoint::new(vec![], ScanSettings::default())).unwrap();
        delete(&path).unwrap();
        assert!(!path.exists());
    }
}
