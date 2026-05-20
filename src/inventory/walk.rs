//! Stage 1, fallback path — recursive directory walking.
//!
//! On Windows the long-term target is `FindFirstFileExW` with
//! `FindExInfoBasic` and `FIND_FIRST_EX_LARGE_FETCH`. For the v0 skeleton
//! we use `std::fs::read_dir`, which is correct and portable; we'll swap
//! in the optimized Win32 path in a later commit once the
//! `winapi_wrappers` for it land.

use std::fs;
use std::path::Path;

use crate::config::ScanConfig;
use crate::inventory::FileEntry;
use crate::Result;

pub fn enumerate(cfg: &ScanConfig) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    for root in &cfg.roots {
        if !root.exists() {
            return Err(crate::Error::PathNotFound(root.clone()));
        }
        walk(root, cfg, &mut out)?;
    }
    Ok(out)
}

fn walk(dir: &Path, cfg: &ScanConfig, out: &mut Vec<FileEntry>) -> Result<()> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(path = %dir.display(), "permission denied; skipping");
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Race with deletion mid-scan; not worth raising.
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "open dir failed; skipping");
            return Ok(());
        }
    };

    for entry in read {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, dir = %dir.display(), "skipping entry");
                continue;
            }
        };
        let path = entry.path();

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if !cfg.follow_links && metadata.file_type().is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            walk(&path, cfg, out)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let size = metadata.len();
        if size < cfg.min_size {
            continue;
        }
        if let Some(max) = cfg.max_size {
            if size > max {
                continue;
            }
        }

        if !path_passes_globs(&path, cfg) {
            continue;
        }

        out.push(FileEntry {
            path,
            size,
            mtime: filetime_ticks(&metadata),
            file_ref: 0,
            parent_ref: 0,
            usn: 0,
            attributes: 0,
            volume_guid: None,
        });
    }
    Ok(())
}

fn path_passes_globs(path: &Path, cfg: &ScanConfig) -> bool {
    if let Some(inc) = &cfg.include {
        if !inc.is_match(path) {
            return false;
        }
    }
    if let Some(exc) = &cfg.exclude {
        if exc.is_match(path) {
            return false;
        }
    }
    true
}

#[cfg(windows)]
fn filetime_ticks(m: &std::fs::Metadata) -> i64 {
    use std::os::windows::fs::MetadataExt;
    m.last_write_time() as i64
}

#[cfg(not(windows))]
fn filetime_ticks(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;
            UNIX_EPOCH_AS_FILETIME + (d.as_nanos() / 100) as i64
        })
        .unwrap_or(0)
}
