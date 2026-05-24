//! `.superdeduper` project bundles.
//!
//! A *project* captures everything a user would want to keep across
//! sessions for a single scan target: the roots, the engine
//! settings, the confirmed-duplicates list, and (when applicable) an
//! archive manifest from a destructive Archive Dupes run. It's
//! orthogonal to the per-file rusqlite hash cache, which lives in
//! `%LOCALAPPDATA%\superdeduper\cache.db` and is purely a performance
//! optimisation — projects are *user state*, cache is *derived data*.
//!
//! On disk a project is a folder, not a single file:
//!
//! ```text
//! my-scan.superdeduper/
//!   project.json          # schema + roots + settings + metadata
//!   duplicates.json       # the confirmed dup groups (can be large)
//!   archive-manifest.json # optional — present iff Archive Dupes ran
//! ```
//!
//! Bundle-as-folder makes the multi-asset story natural: future
//! additions (per-session diagnostics, custom annotations, drive
//! detection overrides) get their own files alongside `project.json`
//! without forcing a schema bump on every change. The `.superdeduper`
//! folder suffix lets Explorer / Finder treat it as one item.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gui::events::DuplicateGroupSummary;
use crate::gui::state::{RootEntry, ScanSettings};
use crate::Result;

/// Top-level project metadata. Roots + settings live here so a small
/// `project.json` is enough to identify what the project covers
/// without parsing the (potentially huge) `duplicates.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFile {
    /// Versioned schema string. Bump when fields change incompatibly.
    /// Old projects with an unknown schema are rejected on load with
    /// a clear error instead of silently misparsed.
    pub schema: String,
    /// Free-form name the user can edit later — defaults to the
    /// bundle folder's filename without the `.superdeduper` suffix.
    pub name: String,
    /// Unix seconds when the project was first saved. Stable across
    /// re-saves; used for the recents list ordering.
    pub created_at_unix: u64,
    /// Unix seconds when the project was last saved. Updated every
    /// Save / Save As.
    pub updated_at_unix: u64,
    /// What was scanned.
    pub roots: Vec<RootEntry>,
    /// Engine settings at the time of the last save. Loading the
    /// project restores these so a future re-scan matches the
    /// original conditions.
    pub settings: ScanSettings,
    /// Lightweight stat block so File → Open can show "12,447 dups,
    /// 5.4 GiB reclaimable" without parsing duplicates.json.
    pub stats: ProjectStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectStats {
    pub duplicate_groups: u64,
    pub duplicate_files: u64,
    pub reclaimable_bytes: u64,
}

/// The confirmed-duplicates list, written separately because it can
/// run to tens of MB for big scans and there's no point reading it
/// just to populate the recents list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DuplicatesFile {
    pub schema: String,
    pub duplicates: Vec<DuplicateGroupSummary>,
}

pub const PROJECT_SCHEMA: &str = "superdeduper.project.v1";
pub const DUPLICATES_SCHEMA: &str = "superdeduper.duplicates.v1";
/// Folder suffix that marks a directory as a superdeduper project
/// bundle. Open / Save dialogs use this to filter folder pickers.
pub const PROJECT_SUFFIX: &str = ".superdeduper";

/// Save (or overwrite) a project bundle at `dir`. Creates the
/// directory if missing. Write order is: temp file → rename, so a
/// crash mid-save leaves the previous version intact.
pub fn save(
    dir: &Path,
    name: &str,
    created_at_unix: u64,
    roots: &[RootEntry],
    settings: &ScanSettings,
    duplicates: &[DuplicateGroupSummary],
) -> Result<()> {
    std::fs::create_dir_all(dir)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let stats = ProjectStats {
        duplicate_groups: duplicates.len() as u64,
        duplicate_files: duplicates.iter().map(|g| g.files.len() as u64).sum(),
        // Inode-aware reclaim — skips fully-hardlinked groups and
        // uses unique_inodes for partial-hardlink groups. Matches
        // what the header tile + leaderboard payload report.
        // Without this, a project saved during a C:\Windows-heavy
        // run would persist an inflated reclaim figure that re-
        // hydrated wrong on next launch.
        reclaimable_bytes: duplicates
            .iter()
            .map(crate::gui::state::inode_aware_savings)
            .sum(),
    };

    let project = ProjectFile {
        schema: PROJECT_SCHEMA.into(),
        name: name.to_string(),
        created_at_unix: if created_at_unix == 0 {
            now
        } else {
            created_at_unix
        },
        updated_at_unix: now,
        roots: roots.to_vec(),
        settings: settings.clone(),
        stats,
    };
    write_atomic(&dir.join("project.json"), &project)?;

    let duplicates_file = DuplicatesFile {
        schema: DUPLICATES_SCHEMA.into(),
        duplicates: duplicates.to_vec(),
    };
    write_atomic(&dir.join("duplicates.json"), &duplicates_file)?;

    Ok(())
}

/// Load a project bundle from `dir`. The `duplicates.json` slot is
/// optional — projects that haven't been scanned yet (or were
/// emptied) just omit it; we return an empty `Vec` in that case
/// rather than erroring.
pub fn load(dir: &Path) -> Result<(ProjectFile, Vec<DuplicateGroupSummary>)> {
    let project_bytes = std::fs::read(dir.join("project.json"))?;
    let project: ProjectFile = serde_json::from_slice(&project_bytes)
        .map_err(|e| crate::Error::other(format!("project.json parse: {e}")))?;
    if project.schema != PROJECT_SCHEMA {
        return Err(crate::Error::other(format!(
            "unknown project schema {:?} (this build understands {})",
            project.schema, PROJECT_SCHEMA
        )));
    }

    let dups_path = dir.join("duplicates.json");
    let duplicates: Vec<DuplicateGroupSummary> = if dups_path.exists() {
        let bytes = std::fs::read(&dups_path)?;
        let dups: DuplicatesFile = serde_json::from_slice(&bytes)
            .map_err(|e| crate::Error::other(format!("duplicates.json parse: {e}")))?;
        if dups.schema != DUPLICATES_SCHEMA {
            return Err(crate::Error::other(format!(
                "unknown duplicates schema {:?}",
                dups.schema
            )));
        }
        dups.duplicates
    } else {
        Vec::new()
    };

    Ok((project, duplicates))
}

/// Default name for a new project bundle, derived from the first
/// root's filename. e.g. `C:\Users\X\AppData` → `AppData.superdeduper`.
pub fn default_bundle_name(roots: &[RootEntry]) -> String {
    let stem = roots
        .first()
        .and_then(|r| r.path.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    format!("{stem}{PROJECT_SUFFIX}")
}

fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| crate::Error::other(format!("json encode {}: {e}", path.display())))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Path of the user's "recent projects" index. We keep it in the
/// existing LocalAppData superdeduper folder next to the cache.
pub fn recents_path() -> Result<PathBuf> {
    let mut p = crate::cache::default_cache_path()?;
    p.set_file_name("recent-projects.json");
    Ok(p)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentProjects {
    /// Most-recently-opened first.
    pub entries: Vec<RecentProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentProject {
    pub path: PathBuf,
    pub name: String,
    /// Unix seconds of the last Open or Save Project. Used to sort.
    pub last_opened_unix: u64,
}

const RECENTS_MAX: usize = 10;

pub fn load_recents() -> Result<RecentProjects> {
    let path = recents_path()?;
    match std::fs::read(&path) {
        Ok(b) => Ok(serde_json::from_slice(&b).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RecentProjects::default()),
        Err(e) => Err(crate::Error::Io(e)),
    }
}

pub fn touch_recent(path: &Path, name: &str) -> Result<()> {
    let mut recents = load_recents().unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Drop any existing entry for this path, then insert at the
    // front so the recents list is MRU-ordered.
    recents.entries.retain(|e| e.path != path);
    recents.entries.insert(
        0,
        RecentProject {
            path: path.to_path_buf(),
            name: name.to_string(),
            last_opened_unix: now,
        },
    );
    recents.entries.truncate(RECENTS_MAX);
    let recents_path = recents_path()?;
    if let Some(parent) = recents_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&recents_path, &recents)?;
    Ok(())
}
