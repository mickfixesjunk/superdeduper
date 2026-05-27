//! Cross-session persistence of the most recent scan results so that
//! safe-rename can be resumed (or just continued) after the app
//! restarts — provided the root folders haven't materially changed.
//!
//! File lives next to the cache and scan-checkpoint in
//! `%LOCALAPPDATA%\superdeduper\results-state.json` on Windows (and the
//! equivalent XDG location elsewhere). The flow:
//!
//! 1. **On scan completion** — `save()` writes the full duplicate-
//!    group list along with a cheap per-root fingerprint
//!    (`(count, sum_size, max_mtime)`).
//! 2. **On safe-rename** — the engine appends the renamed path to
//!    `renamed_paths` and re-saves so a crash mid-batch loses at
//!    most one file's worth of state.
//! 3. **On app launch** — `load_matching()` recomputes the
//!    fingerprint for the current roots and only returns the saved
//!    state if everything matches. Drift ⇒ caller re-scans.
//!
//! The fingerprint walk is the same loop as `inventory::walk` but
//! discards everything except `(file_count, sum_size, max_mtime)`,
//! so it's just `read_dir` + `metadata` — no hashing, no I/O on the
//! file bodies. Fast enough to run at app start without blocking.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::gui::events::DuplicateGroupSummary;
use crate::gui::state::{RootEntry, ScanSettings};
use crate::{Error, Result};

const SCHEMA: &str = "superdeduper.results-state.v1";

/// What we remember about a single scan root so a later "did this
/// change?" check is cheap and approximate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootFingerprint {
    pub path: PathBuf,
    pub file_count: u64,
    pub sum_size: u64,
    /// Largest file mtime under the root, in unix seconds. Bumps if
    /// any file is added or modified.
    pub max_mtime_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultsState {
    pub schema: String,
    pub saved_at_unix: u64,
    pub roots: Vec<RootEntry>,
    pub settings: ScanSettings,
    pub fingerprints: Vec<RootFingerprint>,
    pub duplicates: Vec<DuplicateGroupSummary>,
    /// Files the user has already safe-renamed. Recorded so the next
    /// session knows not to offer them again.
    pub renamed_paths: Vec<PathBuf>,
}

impl ResultsState {
    pub fn new(
        roots: Vec<RootEntry>,
        settings: ScanSettings,
        duplicates: Vec<DuplicateGroupSummary>,
        fingerprints: Vec<RootFingerprint>,
    ) -> Self {
        Self {
            schema: SCHEMA.into(),
            saved_at_unix: now_unix(),
            roots,
            settings,
            fingerprints,
            duplicates,
            renamed_paths: Vec::new(),
        }
    }
}

/// Canonical on-disk location. Sits next to the cache so removing
/// the superdeduper data dir wipes everything in one stroke.
pub fn default_results_state_path() -> Result<PathBuf> {
    let mut p = crate::cache::default_cache_path()?;
    p.set_file_name("results-state.json");
    Ok(p)
}

pub fn save(state: &ResultsState) -> Result<()> {
    let path = default_results_state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(state)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load() -> Result<Option<ResultsState>> {
    let path = default_results_state_path()?;
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let state: ResultsState = serde_json::from_slice(&bytes)?;
    if !state.schema.starts_with("superdeduper.results-state") {
        return Ok(None);
    }
    Ok(Some(state))
}

pub fn delete() -> Result<()> {
    let path = default_results_state_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Walk `root` once and produce a (count, sum, max-mtime) summary.
/// Doesn't open file contents — just metadata. Errors inside the
/// walk are swallowed so a permission-denied subdirectory still
/// produces a useful fingerprint for the rest.
pub fn fingerprint_root(root: &Path) -> RootFingerprint {
    let mut file_count = 0u64;
    let mut sum_size = 0u64;
    let mut max_mtime_unix: i64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            file_count = file_count.saturating_add(1);
            sum_size = sum_size.saturating_add(meta.len());
            if let Ok(mt) = meta.modified() {
                if let Ok(d) = mt.duration_since(SystemTime::UNIX_EPOCH) {
                    let s = d.as_secs() as i64;
                    if s > max_mtime_unix {
                        max_mtime_unix = s;
                    }
                }
            }
        }
    }
    RootFingerprint {
        path: root.to_path_buf(),
        file_count,
        sum_size,
        max_mtime_unix,
    }
}

/// Returns the saved state iff:
///   1. A state file exists at the canonical location, and
///   2. Its `roots` and `settings` match the currently-configured
///      ones (modulo set ordering of reference flags), and
///   3. The currently-on-disk fingerprint for every saved root is
///      within tolerance of the saved fingerprint (count within
///      0.5%, sum_size within 0.5%, max_mtime equal).
///
/// Otherwise returns `Ok(None)` and (optionally) deletes the stale
/// state so the next session starts clean.
pub fn load_matching(roots: &[RootEntry], settings: &ScanSettings) -> Result<Option<ResultsState>> {
    let Some(saved) = load()? else {
        return Ok(None);
    };
    if saved.roots != roots || &saved.settings != settings {
        return Ok(None);
    }
    for fp in &saved.fingerprints {
        let now_fp = fingerprint_root(&fp.path);
        if !fingerprints_match(fp, &now_fp) {
            return Ok(None);
        }
    }
    Ok(Some(saved))
}

fn fingerprints_match(a: &RootFingerprint, b: &RootFingerprint) -> bool {
    if a.path != b.path {
        return false;
    }
    // Tolerances cover transient noise (temp files, OS-managed
    // bookkeeping files like Thumbs.db updating). Anything beyond
    // that probably means the user actually added/removed files and
    // a re-scan is the right call.
    let count_tol = (a.file_count.max(b.file_count) / 200).max(1);
    if a.file_count.abs_diff(b.file_count) > count_tol {
        return false;
    }
    let size_tol = (a.sum_size.max(b.sum_size) / 200).max(1024);
    if a.sum_size.abs_diff(b.sum_size) > size_tol {
        return false;
    }
    // mtime is the most sensitive signal; require an exact match.
    a.max_mtime_unix == b.max_mtime_unix
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_match_tolerates_small_drift() {
        let a = RootFingerprint {
            path: PathBuf::from("/x"),
            file_count: 10_000,
            sum_size: 10_000_000_000,
            max_mtime_unix: 1_700_000_000,
        };
        let b = RootFingerprint {
            path: PathBuf::from("/x"),
            file_count: 10_005,
            sum_size: 10_000_050_000,
            max_mtime_unix: 1_700_000_000,
        };
        assert!(fingerprints_match(&a, &b));
    }

    #[test]
    fn fingerprint_match_fails_on_mtime_bump() {
        let a = RootFingerprint {
            path: PathBuf::from("/x"),
            file_count: 100,
            sum_size: 1_000_000,
            max_mtime_unix: 1_700_000_000,
        };
        let b = RootFingerprint {
            path: PathBuf::from("/x"),
            file_count: 100,
            sum_size: 1_000_000,
            max_mtime_unix: 1_700_000_001,
        };
        assert!(!fingerprints_match(&a, &b));
    }

    #[test]
    fn fingerprint_match_fails_on_path_mismatch() {
        let a = RootFingerprint {
            path: PathBuf::from("/x"),
            file_count: 1,
            sum_size: 1,
            max_mtime_unix: 1,
        };
        let b = RootFingerprint {
            path: PathBuf::from("/y"),
            file_count: 1,
            sum_size: 1,
            max_mtime_unix: 1,
        };
        assert!(!fingerprints_match(&a, &b));
    }
}
