//! Stage 1, fallback path — recursive directory walking.
//!
//! On Windows the long-term target is `FindFirstFileExW` with
//! `FindExInfoBasic` and `FIND_FIRST_EX_LARGE_FETCH`. For the v0 skeleton
//! we use `std::fs::read_dir`, which is correct and portable; we'll swap
//! in the optimized Win32 path in a later commit once the
//! `winapi_wrappers` for it land.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::ScanConfig;
use crate::inventory::FileEntry;
use crate::Result;

/// Streaming progress emitted by [`enumerate_with_progress`]. Consumers
/// translate these into [`EngineEvent`]s for the GUI; the CLI uses a
/// no-op consumer.
pub enum WalkEvent<'a> {
    /// About to enumerate this directory.
    Entered { path: &'a Path, depth: u32 },
    /// One eligible file picked up. Filter rules have already passed.
    FileFound { path: &'a Path, size: u64 },
    /// `read_dir` or `metadata` failed on this path. Most common
    /// cause on Windows is permission denied (e.g. `C:\Program Files`
    /// without elevation, or `\System Volume Information` on external
    /// drives).
    DirError { path: &'a Path, message: String },
    /// One read_dir entry was skipped because its `metadata()` errored
    /// or it was an unsupported type (junction not followed, etc.).
    EntrySkipped {
        path: &'a Path,
        reason: &'static str,
    },
}

pub fn enumerate(cfg: &ScanConfig) -> Result<Vec<FileEntry>> {
    enumerate_with_progress(cfg, |_| {})
}

pub fn enumerate_with_progress<F>(cfg: &ScanConfig, callback: F) -> Result<Vec<FileEntry>>
where
    F: FnMut(WalkEvent<'_>),
{
    enumerate_cancellable(cfg, None, callback)
}

/// Like [`enumerate_with_progress`] but polls an optional cancel
/// flag before every directory recursion and before every entry. If
/// the flag flips to `true`, the walk stops and returns whatever it
/// has collected so far — caller is expected to honour the cancel
/// signal too (e.g. by emitting Paused and discarding the partial
/// list).
pub fn enumerate_cancellable<F>(
    cfg: &ScanConfig,
    cancel: Option<&AtomicBool>,
    mut callback: F,
) -> Result<Vec<FileEntry>>
where
    F: FnMut(WalkEvent<'_>),
{
    let mut out = Vec::new();
    for root in &cfg.roots {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        if !root.exists() {
            callback(WalkEvent::DirError {
                path: root,
                message: "path not found".into(),
            });
            return Err(crate::Error::PathNotFound(root.clone()));
        }
        walk(root, cfg, &mut out, &mut callback, 0, cancel)?;
    }
    Ok(out)
}

fn walk<F>(
    dir: &Path,
    cfg: &ScanConfig,
    out: &mut Vec<FileEntry>,
    callback: &mut F,
    depth: u32,
    cancel: Option<&AtomicBool>,
) -> Result<()>
where
    F: FnMut(WalkEvent<'_>),
{
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(());
    }
    callback(WalkEvent::Entered { path: dir, depth });

    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            callback(WalkEvent::DirError {
                path: dir,
                message: "permission denied".into(),
            });
            tracing::warn!(path = %dir.display(), "permission denied; skipping");
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(e) => {
            callback(WalkEvent::DirError {
                path: dir,
                message: e.to_string(),
            });
            tracing::warn!(path = %dir.display(), error = %e, "open dir failed; skipping");
            return Ok(());
        }
    };

    for entry in read {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                callback(WalkEvent::DirError {
                    path: dir,
                    message: format!("entry error: {e}"),
                });
                tracing::warn!(error = %e, dir = %dir.display(), "skipping entry");
                continue;
            }
        };
        let path = entry.path();

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                callback(WalkEvent::EntrySkipped {
                    path: &path,
                    reason: "metadata failed",
                });
                tracing::debug!(path = %path.display(), error = %e, "metadata failed; skipping");
                continue;
            }
        };

        if !cfg.follow_links && metadata.file_type().is_symlink() {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "symlink (use --follow-links to include)",
            });
            continue;
        }

        if metadata.is_dir() {
            walk(&path, cfg, out, callback, depth + 1, cancel)?;
            continue;
        }
        if !metadata.is_file() {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "not a regular file",
            });
            continue;
        }

        let size = metadata.len();
        if size < cfg.min_size {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "below min-size",
            });
            continue;
        }
        if let Some(max) = cfg.max_size {
            if size > max {
                callback(WalkEvent::EntrySkipped {
                    path: &path,
                    reason: "above max-size",
                });
                continue;
            }
        }

        if !path_passes_globs(&path, cfg) {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "filtered by include/exclude",
            });
            continue;
        }

        callback(WalkEvent::FileFound { path: &path, size });

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
