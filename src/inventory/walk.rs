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
        // Convert the root to a verbatim (\\?\C:\...) path on
        // Windows so child enumeration bypasses Win32's path-name
        // normalization. Without this, filenames with trailing dots
        // or spaces are silently stripped/dropped by FindFirstFileW
        // (the call backing std::fs::read_dir). The verbatim prefix
        // propagates into every DirEntry::path() that the walker
        // emits, so downstream paths in FileEntry retain the prefix.
        // We strip it back for emitted paths so the user-facing
        // output (GUI, JSON) doesn't show ugly \\?\C:\... strings.
        #[cfg(windows)]
        let root_for_walk = to_verbatim(root);
        #[cfg(not(windows))]
        let root_for_walk = root.clone();
        walk(&root_for_walk, cfg, &mut out, &mut callback, 0, cancel)?;
    }
    Ok(out)
}

// Note: emitted paths intentionally keep the `\\?\` prefix when the
// walker added one (see `to_verbatim` above). The downstream hash
// stage opens files via `File::open(&entry.path)`, and File::open
// only sees files with trailing dots/spaces / reserved DOS names
// when given a verbatim path. Stripping the prefix here would
// reintroduce the test30 bug. User-visible paths in the JSON output
// keep the prefix — uglier but functional. If we ever want clean
// paths in the report, we can strip at the output layer (JSON
// serialization) instead of in the walker.

/// Convert a regular Windows path (`C:\foo\bar`) to its verbatim form
/// (`\\?\C:\foo\bar`). Already-verbatim paths and non-disk-prefixed
/// paths pass through unchanged. The Win32 file APIs interpret
/// verbatim paths literally and skip the legacy normalization that
/// strips trailing dots/spaces and remaps reserved DOS names.
#[cfg(windows)]
fn to_verbatim(p: &Path) -> std::path::PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    // Check if already verbatim by inspecting the wide encoding.
    let wide_first: Vec<u16> = p.as_os_str().encode_wide().take(4).collect();
    if wide_first.as_slice().starts_with(&[0x5C, 0x5C, 0x3F, 0x5C]) {
        // already "\\?\"
        return p.to_path_buf();
    }
    // Only prefix absolute paths with a drive letter — relative or
    // UNC paths get left alone (verbatim-UNC is a different form).
    if p.is_absolute() {
        let mut s = OsString::from(r"\\?\");
        s.push(p.as_os_str());
        std::path::PathBuf::from(s)
    } else {
        p.to_path_buf()
    }
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

        // Walker emits FileEntry with file_ref=0 / volume_guid=None.
        // Inode-id resolution happens later — see
        // `pipeline::grouping::resolve_file_ids`, called between the
        // size-grouping and layout stages. Files that don't survive
        // size grouping (i.e. have unique sizes) never need an inode
        // id, so resolving here would mean opening every file on the
        // walk to get information that almost all of them won't
        // need.
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

#[cfg(windows)]
pub(crate) fn file_id_for(path: &Path) -> Option<(u64, Option<String>)> {
    // Open the file with FILE_FLAG_BACKUP_SEMANTICS so the call
    // works for directories as well (we don't use it for dirs
    // currently but the flag is cheap and removes a footgun for
    // future callers). Read access is plenty; query-only calls
    // don't need write. FILE_SHARE_READ|WRITE|DELETE matches what
    // every other Win32 file open in this codebase uses so we
    // don't trip on someone else holding the file open.
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .ok()?
    };
    if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info).is_ok() };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if !ok {
        return None;
    }
    // Compose the 64-bit file id from its two halves. This is the
    // FileReferenceNumber for the inode — two hardlinks share it.
    let file_ref = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    // Encode the volume serial as a string so it fits the existing
    // Option<String> field. Two paths on the same volume produce the
    // same string; on different volumes they differ.
    let vol = Some(format!("vol-serial:0x{:08x}", info.dwVolumeSerialNumber));
    Some((file_ref, vol))
}
