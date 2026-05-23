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
pub mod walk;
pub mod warm;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::cache::Cache;
use crate::config::ScanConfig;
use crate::winapi_wrappers::FileRef;
use crate::Result;

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
}

/// Enumerate every eligible file under the scan roots.
///
/// Strategy:
/// * If every scan root is a volume root (e.g. `C:\`, `D:\`, `/`), try the
///   MFT fast path first; on failure (or non-NTFS), fall back to walking.
/// * If any scan root is a strict subdirectory (e.g. `C:\Windows`), skip
///   MFT entirely and go straight to walker.
///
/// Why the subdir carve-out:
/// MFT enum reads the **full volume's** records, then `under_any_root`
/// filters them. For a subdir scan that's wasted work — the headline finding
/// from sys32-r2-broader-scope was 95% of wall-clock spent in Stage 1 because
/// of this. The walker is bounded to the target subtree.
///
/// Worse, MFT reconstructs ONE canonical path per inode via the primary
/// `parent_ref` chain. For hardlinked files whose primary path lies outside
/// the scan root (e.g. a System32 file whose MFT-primary alias is in
/// WinSxS), `under_any_root` rejects the reconstructed path and the file
/// is silently dropped from the inventory — measured at 738 inodes when the
/// walker fallback saw 11,299 paths on the same corpus. That's data-loss
/// territory, not just a perf issue.
///
/// The walker sees hardlink aliases natively (one FileEntry per directory
/// entry) and doesn't pay the whole-volume MFT cost. MFT remains the
/// fast-path for whole-volume scans where it's genuinely faster.
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
    if !all_roots_are_volume_roots(&cfg.roots) {
        tracing::info!(
            roots = ?cfg.roots,
            "scan root is a subdirectory; using walker (MFT path skipped — see inventory/mod.rs doc)",
        );
        return walk::enumerate(cfg);
    }
    match mft::enumerate(cfg, cache) {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(error = %e, "MFT enumeration unavailable, falling back to directory walk");
            walk::enumerate(cfg)
        }
    }
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
    fn mixed_roots_force_walker() {
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
