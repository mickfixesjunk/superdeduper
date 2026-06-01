//! Stage 1 — file inventory.
//!
//! Two strategies live here:
//!
//! * [`mft`] — fast path: enumerate `FSCTL_ENUM_USN_DATA` once per volume,
//!   reconstruct paths from parent references, filter to the scan roots.
//! * [`walk`] — fallback path: `FindFirstFileExW` (Windows) / `read_dir`
//!   (other platforms) recursion. Used when MFT enum fails or the volume
//!   isn't NTFS.
//!
//! Both produce the same [`FileEntry`] stream so the downstream pipeline
//! stays agnostic to how the inventory was acquired.

#[cfg(windows)]
pub mod dir_enum;
pub mod mft;
pub mod placeholder;
pub mod walk;
pub mod warm;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::cache::Cache;
use crate::config::ScanConfig;
use crate::winapi_wrappers::FileRef;
use crate::Result;

pub use placeholder::PlaceholderState;

/// One file in the inventory.
///
/// Fields populated by the walker fallback may leave `file_ref` and `usn`
/// as `0` since those concepts only exist on NTFS.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    /// 100ns FILETIME ticks since 1601-01-01 UTC. `0` if unknown.
    pub mtime: i64,
    pub file_ref: FileRef,
    pub parent_ref: FileRef,
    pub usn: i64,
    pub attributes: u32,
    /// Volume GUID path (e.g. `\\?\Volume{...}\`) when known.
    pub volume_guid: Option<String>,
    /// Cloud-placeholder / reparse-point classification, derived
    /// from `attributes` (and reparse_tag when available) at the
    /// time this entry was produced. Drives the action-layer and
    /// tier-guard checks downstream — a `RecallOnOpen` file
    /// shouldn't be hashed unless the user explicitly opts in.
    pub placeholder: PlaceholderState,
}

/// Enumerate every eligible file under the scan roots.
///
/// Strategy (v0.3.16 Path B; Mick GO 2026-06-01 07:53 PDT):
/// * **Default**: route every root through the directory walker.
///   Layer-parallel BFS (per `walk_bfs`) handles volume roots and
///   subdirectories alike; correctness is uniform.
/// * **Opt-in**: `--force-mft` + every root is a volume root +
///   admin-elevated process → use the MFT fast path. Falls back to
///   the walker on EACCES (non-admin) or any other MFT enum failure.
///
/// Why walker as the default (the Path B decision):
///
/// 1. MFT path silently elides hardlink aliases. It reconstructs ONE
///    canonical path per inode via the primary `parent_ref` chain.
///    For hardlinked files whose primary path lies outside the scan
///    root (e.g. a System32 file whose MFT-primary alias is in
///    WinSxS), `under_any_root` rejects the reconstructed path and
///    the file is silently dropped from the inventory — measured at
///    738 inodes when the walker fallback saw 11,299 paths on the
///    same corpus, and 685k MFT entries vs 793k walker entries on a
///    full C:\ scan (benchmarker D'' 2026-06-01 07:45 PDT). The
///    walker sees every alias as a FileEntry.
///
/// 2. MFT path historically skipped the engine's exclusion filters
///    (`is_superdeduper_self_path`, `dropped_by_exclusions`) that the
///    walker applies. Net effect on cell-D was ~41k system-DLL /
///    OS-protected paths surfaced as dedup candidates that the user
///    explicitly excluded by policy.
///
/// 3. Per-file enumeration cost. MFT path reads the **full volume's**
///    USN records via `FSCTL_ENUM_USN_DATA` then does a separate
///    `fs::metadata()` syscall per surviving record (to fetch size,
///    which the USN record doesn't carry). Walker uses
///    `FileIdBothDirectoryInfo` per-folder — name + size + attrs +
///    mtime + inode in one batched call. Benchmarker D' measured
///    walker walk_us/file ~17 us on full C:\ vs MFT walk_us/file
///    ~143 us = 8.3x faster per-file even at volume scale.
///
/// MFT is retained as an opt-in escape hatch for admin power-users
/// who want raw walk-stage speed on a volume where they understand
/// the hardlink-elision + exclusion-skip tradeoffs.
///
/// Filtering against `min_size` / `max_size` / include / exclude globs
/// happens here so downstream stages never see ineligible files.
///
/// `cache` is optional — when provided it enables the warm-path
/// inventory: a USN-journal delta is applied to a saved baseline
/// rather than re-walking the MFT. First-ever scan still pays the
/// full cold cost. Pass `None` to disable warm-path entirely (the
/// `superdeduper scan --no-cache` codepath does this).
pub fn enumerate(cfg: &ScanConfig, cache: Option<&Arc<Mutex<Cache>>>) -> Result<Vec<FileEntry>> {
    let (files, _skipped) = enumerate_with_skipped(cfg, cache)?;
    Ok(files)
}

/// T2.1 phase 7 surface — same as [`enumerate`] but also returns the
/// list of placeholders observed (every entry classified as
/// non-`NotPlaceholder` during inventory). Used by the CLI/GUI to emit
/// a `skipped[]` array in the JSON scan output separately from the
/// dup-group set.
///
/// All three producers (mft, warm, walk) already call `classify()` and
/// stamp `PlaceholderState` onto each `FileEntry` (phase 2). We derive
/// the skipped list from the entry stream rather than threading a
/// parallel collector through every producer — single read of the
/// FileEntry vec, no extra allocations during enumeration, no API
/// churn at the producer level.
///
/// `placeholder == ReparseDedup` files appear in BOTH `files` and
/// `skipped` because they're observed AND hashable; downstream
/// consumers filter as appropriate.
pub fn enumerate_with_skipped(
    cfg: &ScanConfig,
    cache: Option<&Arc<Mutex<Cache>>>,
) -> Result<(Vec<FileEntry>, Vec<crate::pipeline::SkippedFile>)> {
    // Block K: also collect walker-side error skips that don't produce
    // a FileEntry (the canonical case is `--follow-links` hitting a
    // cloud-recall symlink target that can't be stat'd on Win11 25H2
    // without a Cloud Filter sync root). Walker emits these via the
    // existing `WalkEvent::EntrySkipped` event stream — we just need
    // to consume it from the inventory side rather than relying on
    // the post-walk FileEntry-stream derivation alone.
    //
    // The closure pushes to a local Vec via FnMut. Reasons that should
    // surface in JSON `skipped[]` get a corresponding SkippedFile;
    // others (permission denied, "not a regular file", glob/size
    // filter rejection) stay as tracing events only — they're not
    // placeholder-class outcomes.
    let mut walker_event_skipped: Vec<crate::pipeline::SkippedFile> = Vec::new();
    let walker_event_callback = |evt: walk::WalkEvent<'_>| {
        if let walk::WalkEvent::EntrySkipped { path, reason } = evt {
            // Currently only the symlink-target case is surfaced.
            // Other EntrySkipped reasons are filter/permission events
            // that don't correspond to placeholder-class outcomes;
            // adding them would mix filter-reject with
            // hydration-class concerns and confuse downstream
            // consumers. Extend deliberately if needed.
            if reason == "symlink target unreadable" {
                walker_event_skipped.push(crate::pipeline::SkippedFile {
                    path: path.to_path_buf(),
                    placeholder: "symlink_target_unreadable".to_string(),
                    reparse_tag: None,
                });
            }
        }
    };

    let files = if !cfg.force_mft || !all_roots_are_volume_roots(&cfg.roots) {
        // v0.3.16 Path B (Mick GO 2026-06-01 07:53 PDT): walker is the
        // default for every root, including volume roots like C:\.
        // Benchmarker D'/D'' (07:36-07:45 PDT) validated the walker
        // is BOTH faster (8.8x walk on full C:\) AND more correct (sees
        // all hardlink aliases; applies exclusion filters the MFT path
        // skipped). The MFT path's lower bytes-read isn't a perf win --
        // it's missing data (data-loss territory per the docstring
        // below). Path B chooses correctness as default.
        //
        // MFT path stays available via --force-mft for admin power-users
        // who accept the hardlink-canonical-path elision + missing
        // exclusion-filter coverage tradeoffs for raw speed on volumes
        // where they understand both costs. Non-admin processes can't
        // open \\?\Volume anyway -- MFT enum returns ACCESS_DENIED and
        // falls through to the walker.
        if cfg.force_mft && !all_roots_are_volume_roots(&cfg.roots) {
            tracing::info!(
                roots = ?cfg.roots,
                "force_mft requested but at least one root is a subdir; using walker (MFT only applies to volume roots)",
            );
        }
        walk::enumerate_with_progress(cfg, walker_event_callback)?
    } else {
        // --force-mft + every root is a volume root: opt into the MFT
        // fast path. Falls back to the walker on ACCESS_DENIED (non-
        // admin) or any other MFT enumeration failure.
        match mft::enumerate(cfg, cache) {
            Ok(v) => v,
            Err(e) => {
                // Path B (v0.3.16+): MFT is opt-in, so this fallback
                // fires only when the user explicitly requested
                // --force-mft AND the enum failed at runtime. Most
                // common cause is non-admin process token: MFT path
                // opens \\?\Volume via CreateFileW which requires
                // elevation. The walker fallback is correct + complete;
                // the user gets a hint about the prerequisite for the
                // MFT path they tried to opt into.
                tracing::warn!(
                    "--force-mft requested but MFT enumeration failed (typically requires running as Administrator). Falling back to directory walk."
                );
                tracing::debug!(error = %e, "MFT enumeration unavailable; walker fallback engaged");
                walk::enumerate_with_progress(cfg, walker_event_callback)?
            }
        }
    };
    // FileEntry-derived skips (placeholders the walker successfully
    // enumerated and stamped) plus walker-event skips (error paths
    // that didn't produce a FileEntry). Both go into the same vec
    // so downstream consumers see one unified `skipped[]`.
    let mut skipped: Vec<crate::pipeline::SkippedFile> = files
        .iter()
        .filter_map(|f| crate::pipeline::SkippedFile::from_state(f.path.clone(), f.placeholder))
        .collect();
    skipped.extend(walker_event_skipped);
    if !skipped.is_empty() {
        tracing::info!(
            count = skipped.len(),
            "inventory: placeholder files observed (see skipped[] in scan output)"
        );
    }
    Ok((files, skipped))
}

/// True when every root is the root of its volume (or filesystem on unix),
/// not a strict subdirectory.
fn all_roots_are_volume_roots(roots: &[PathBuf]) -> bool {
    roots.iter().all(|r| is_volume_root(r))
}

/// `C:\`, `\\?\C:\`, `\\server\share\`, `/` → true.
/// `C:\Windows`, `/home`, `C:` (drive-relative, no root) → false.
fn is_volume_root(path: &Path) -> bool {
    use std::path::Component;
    let mut comps = path.components();
    // Optional prefix (Windows: drive letter, UNC, verbatim). On unix
    // there is no prefix component so we skip nothing.
    if let Some(Component::Prefix(_)) = comps.clone().next() {
        comps.next();
    }
    matches!(comps.next(), Some(Component::RootDir)) && comps.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_roots_are_recognised() {
        // Cross-platform cases
        assert!(is_volume_root(Path::new("/")));
        assert!(!is_volume_root(Path::new("/home")));
        assert!(!is_volume_root(Path::new("/home/user")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_volume_roots_are_recognised() {
        assert!(is_volume_root(Path::new(r"C:\")));
        assert!(is_volume_root(Path::new(r"D:\")));
        assert!(is_volume_root(Path::new(r"\\?\C:\")));
        assert!(!is_volume_root(Path::new(r"C:\Windows")));
        assert!(!is_volume_root(Path::new(r"C:\Windows\System32")));
        assert!(!is_volume_root(Path::new(r"\\?\C:\Windows")));
        // `C:` (no trailing slash) is drive-relative, not a volume root.
        assert!(!is_volume_root(Path::new("C:")));
    }

    #[test]
    fn mixed_roots_force_mft() {
        // If any root is a subdir, the whole scan must use walker —
        // splitting per-volume would add architectural complexity for
        // a rarely-used multi-root case.
        let roots = vec![PathBuf::from("/"), PathBuf::from("/home/user")];
        assert!(!all_roots_are_volume_roots(&roots));
    }

    #[test]
    fn empty_root_list_is_trivially_volume_root() {
        // Won't actually happen — ScanConfig::from_args rejects empty
        // paths — but `all()` on an empty iterator returns true, so
        // document the trivia explicitly.
        assert!(all_roots_are_volume_roots(&[]));
    }
}
