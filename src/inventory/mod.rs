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

pub mod mft;
pub mod walk;

use std::path::PathBuf;

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
/// Tries MFT enum first; on failure (or non-NTFS), falls back to walking.
/// Filtering against `min_size` / `max_size` / include / exclude globs
/// happens here so downstream stages never see ineligible files.
pub fn enumerate(cfg: &ScanConfig) -> Result<Vec<FileEntry>> {
    match mft::enumerate(cfg) {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(error = %e, "MFT enumeration unavailable, falling back to directory walk");
            walk::enumerate(cfg)
        }
    }
}
