//! Pause / resume checkpoint that survives an app restart.
//!
//! When the user pauses (or the app exits mid-scan), the engine
//! serialises a `Checkpoint` to `%LOCALAPPDATA%\superdupe\scan-checkpoint.json`
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
            schema: "superdupe.checkpoint.v1".into(),
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
    if !cp.schema.starts_with("superdupe.checkpoint") {
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
        assert_eq!(loaded.schema, "superdupe.checkpoint.v1");
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
