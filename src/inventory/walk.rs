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
        // We intentionally KEEP the prefix end-to-end — File::open
        // also needs verbatim form to open these files, and
        // stripping at any later stage would re-introduce the
        // normalization bug. JSON output therefore shows
        // \\?\C:\... paths for files under such corpora; uglier
        // but functional.
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

    // Block N: Windows fast path via FileIdBothDirectoryInfo. Returns
    // every entry's name + size + attrs + inode + mtime in a single
    // batched call, eliminating the per-entry `metadata()` cost AND
    // populating `file_ref` so Stage 2b's resolve_file_ids skips us
    // entirely (it short-circuits per-file when file_ref != 0).
    //
    // On non-NTFS volumes or any API failure, falls through to the
    // existing `read_dir` path. The fall-through preserves the
    // original semantics exactly — fast path is opportunistic.
    #[cfg(windows)]
    {
        if let Some(enumeration) = crate::inventory::dir_enum::enumerate_dir_full(dir) {
            return walk_fast_path(dir, enumeration, cfg, out, callback, depth, cancel);
        }
        // Fall through to read_dir (logged at trace level — common on
        // network shares / non-NTFS where the API doesn't apply).
        tracing::trace!(
            path = %dir.display(),
            "FileIdBothDirectoryInfo unavailable; falling back to read_dir"
        );
    }

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

        let metadata = if metadata.file_type().is_symlink() {
            if !cfg.follow_links {
                callback(WalkEvent::EntrySkipped {
                    path: &path,
                    reason: "symlink (use --follow-links to include)",
                });
                continue;
            }
            // --follow-links: re-stat through the symlink so downstream
            // checks (is_file / is_dir / classify) see the TARGET's
            // attributes, not the link's. `entry.metadata()` returns
            // symlink_metadata on every platform, so a file-symlink
            // would otherwise be dropped by the `!is_file` branch below.
            // Per testdesign criterion #5: default.json and follow.json
            // had been byte-identical because of this — --follow-links
            // had zero observable effect.
            match fs::metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    callback(WalkEvent::EntrySkipped {
                        path: &path,
                        reason: "symlink target unreadable",
                    });
                    tracing::debug!(
                        path = %path.display(),
                        error = %e,
                        "symlink target stat failed; skipping",
                    );
                    continue;
                }
            }
        } else {
            metadata
        };

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
        // Extract Win32 file attributes on Windows so cloud-placeholder
        // classification works on the fallback path too. On other
        // platforms attributes stays 0 and placeholder.rs's
        // cross-platform stub returns NotPlaceholder.
        let attributes = win_file_attributes(&metadata);
        // When the file carries a reparse point, fetch its tag via
        // FSCTL_GET_REPARSE_POINT so classify() can distinguish
        // IO_REPARSE_TAG_DEDUP (ReparseDedup → hashable) from cloud
        // tags (RecallOnOpen etc. via tag-first detection) from
        // arbitrary unknowns. Without this, every reparse file
        // classifies as `OtherReparse(0)` — conservative-blocked,
        // but loses information the user needs.
        // FILE_ATTRIBUTE_REPARSE_POINT = 0x400.
        let reparse_tag = if (attributes & 0x400) != 0 {
            crate::winapi_wrappers::fetch_reparse_tag(&path)
        } else {
            None
        };
        // L0: on Linux/Unix, populate file_ref + volume_guid from
        // st_ino + st_dev so the engine's T0.5 partition_by_inode
        // sees real hardlink relationships. Without this, every
        // path got a synthetic per-file key, hardlinks were never
        // collapsed, and reclaimable_inode_bytes inflated on
        // hardlink-heavy corpora (e.g. /usr/lib's uutils multi-
        // call binary that ships 114 hardlinks to one inode).
        // Cheap: `metadata` was already fetched for the size +
        // filetime/win-attributes lines above; no extra syscall.
        let (file_ref, volume_guid) = inode_identity(&metadata);
        out.push(FileEntry {
            path,
            size,
            mtime: filetime_ticks(&metadata),
            file_ref,
            parent_ref: 0,
            usn: 0,
            attributes,
            volume_guid,
            placeholder: crate::inventory::placeholder::classify(attributes, reparse_tag),
        });
    }
    Ok(())
}

/// Extract (file_ref, volume_guid) from a Metadata.
///
/// On Unix: `(st_ino, Some("linux-dev-{st_dev}"))`. T0.5's
/// `partition_by_inode` uses `(volume_guid, file_ref)` as the inode
/// key — populating both fields makes hardlink groups collapse
/// correctly on Linux.
///
/// On Windows: returns `(0, None)`. Windows has its own
/// `resolve_file_ids` pass that uses `GetFileInformationByHandle` to
/// populate the canonical NTFS file_ref; we leave the walker's
/// default here to preserve that flow.
#[cfg(unix)]
fn inode_identity(metadata: &std::fs::Metadata) -> (u64, Option<String>) {
    use std::os::unix::fs::MetadataExt;
    let file_ref = metadata.ino();
    let volume_guid = Some(format!("linux-dev-{}", metadata.dev()));
    (file_ref, volume_guid)
}

#[cfg(not(unix))]
fn inode_identity(_metadata: &std::fs::Metadata) -> (u64, Option<String>) {
    (0, None)
}

/// Block N — Windows fast path. Iterate `enumeration.entries`, push
/// FileEntry directly without a per-entry metadata() syscall.
///
/// Semantics mirror the read_dir path exactly:
/// * Reparse points with the symlink tag honour `--follow-links`.
/// * Other reparse points (dedup, cloud-recall, junctions, unknown)
///   get a `placeholder` state via `classify()` and flow through the
///   normal pipeline — guard logic at the action + hash layers
///   decides what to actually do.
/// * size/min-size/max-size/glob filters apply identically.
#[cfg(windows)]
fn walk_fast_path<F>(
    dir: &Path,
    enumeration: crate::inventory::dir_enum::DirFullEnumeration,
    cfg: &ScanConfig,
    out: &mut Vec<FileEntry>,
    callback: &mut F,
    depth: u32,
    cancel: Option<&AtomicBool>,
) -> Result<()>
where
    F: FnMut(WalkEvent<'_>),
{
    // FILE_ATTRIBUTE_REPARSE_POINT bit — same definition used at
    // classify() call sites. Inlined to keep the loop tight.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    // IO_REPARSE_TAG_SYMLINK from ntifs.h. The fast path doesn't have
    // Rust's `is_symlink()` available (that's a metadata-derived
    // bool); we check the tag explicitly via `fetch_reparse_tag`.
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    for entry in enumeration.entries {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(());
        }
        let path = dir.join(&entry.name);

        // For reparse-point entries, fetch the actual tag so we can
        // tell symlinks (handled specially per --follow-links) from
        // other reparse types (mount points, dedup, cloud) which get
        // their placeholder state from classify().
        let reparse_tag = if (entry.attributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            crate::winapi_wrappers::fetch_reparse_tag(&path)
        } else {
            None
        };
        let is_symlink = reparse_tag == Some(IO_REPARSE_TAG_SYMLINK);

        if is_symlink {
            if !cfg.follow_links {
                callback(WalkEvent::EntrySkipped {
                    path: &path,
                    reason: "symlink (use --follow-links to include)",
                });
                continue;
            }
            // Re-stat through the symlink — same handling as the
            // read_dir path. The target's metadata determines whether
            // we recurse (target is a dir) or push (target is a file).
            let target_meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    callback(WalkEvent::EntrySkipped {
                        path: &path,
                        reason: "symlink target unreadable",
                    });
                    tracing::debug!(
                        path = %path.display(),
                        error = %e,
                        "symlink target stat failed; skipping",
                    );
                    continue;
                }
            };
            if target_meta.is_dir() {
                walk(&path, cfg, out, callback, depth + 1, cancel)?;
                continue;
            }
            if !target_meta.is_file() {
                callback(WalkEvent::EntrySkipped {
                    path: &path,
                    reason: "not a regular file",
                });
                continue;
            }
            let target_size = target_meta.len();
            if target_size < cfg.min_size {
                callback(WalkEvent::EntrySkipped {
                    path: &path,
                    reason: "below min-size",
                });
                continue;
            }
            if let Some(max) = cfg.max_size {
                if target_size > max {
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
            callback(WalkEvent::FileFound {
                path: &path,
                size: target_size,
            });
            let target_attrs = win_file_attributes(&target_meta);
            let target_reparse_tag = if (target_attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                crate::winapi_wrappers::fetch_reparse_tag(&path)
            } else {
                None
            };
            out.push(FileEntry {
                path,
                size: target_size,
                mtime: filetime_ticks(&target_meta),
                // Target file_ref isn't available cheaply — leave 0
                // and let Stage 2b resolve via the slow path.
                // Symlink targets are rare enough that the per-file
                // cost is fine.
                file_ref: 0,
                parent_ref: 0,
                usn: 0,
                attributes: target_attrs,
                volume_guid: None,
                placeholder: crate::inventory::placeholder::classify(
                    target_attrs,
                    target_reparse_tag,
                ),
            });
            continue;
        }

        if entry.is_dir {
            walk(&path, cfg, out, callback, depth + 1, cancel)?;
            continue;
        }

        // Regular file branch — apply the same filter ladder as the
        // read_dir path, using the batched metadata from the dir
        // enumeration.
        let size = entry.size;
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
            mtime: entry.mtime_filetime,
            // file_ref + volume_guid already resolved by the batched
            // call — Stage 2b will short-circuit per-file when both
            // are non-default.
            file_ref: entry.file_id,
            parent_ref: 0,
            usn: 0,
            attributes: entry.attributes,
            volume_guid: enumeration.volume_guid.clone(),
            placeholder: crate::inventory::placeholder::classify(entry.attributes, reparse_tag),
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
    // Settings → Exclusions filter (preset packs + custom). Master
    // toggle defaults OFF; this short-circuits to Included when
    // disabled so the per-scan cost is one bool check.
    if matches!(
        cfg.exclusion_policy.evaluate(path),
        crate::exclusions::Decision::Excluded(_)
    ) {
        return false;
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
fn win_file_attributes(m: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes()
}

#[cfg(not(windows))]
fn win_file_attributes(_m: &std::fs::Metadata) -> u32 {
    0
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

#[cfg(test)]
mod inode_identity_tests {
    //! Unit tests for the inode-identity walker helper. The
    //! integration check (whole-engine scan of a directory with
    //! hardlinks) lives in tests/walker_fast_path.rs and the
    //! verify-scan in the dev iteration; these tests pin the
    //! helper's contract directly.

    use super::inode_identity;
    use std::fs;
    use std::io::Write;

    #[test]
    #[cfg(unix)]
    fn extracts_ino_and_dev_on_unix() {
        // Write a file, fetch its metadata, assert inode_identity
        // returns non-zero file_ref + a volume_guid that matches
        // the `linux-dev-{dev}` format. We can't assert specific
        // numbers (vary per host), only the shape.
        let tmp = std::env::temp_dir().join(format!(
            "sd-inode-identity-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = fs::File::create(&tmp).unwrap();
        f.write_all(b"hi").unwrap();
        drop(f);
        let meta = fs::metadata(&tmp).unwrap();

        let (file_ref, volume_guid) = inode_identity(&meta);
        assert_ne!(file_ref, 0, "file_ref must be the real st_ino");
        let vol = volume_guid.expect("volume_guid populated on Unix");
        assert!(
            vol.starts_with("linux-dev-"),
            "volume_guid format mismatch: {vol}"
        );
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    #[cfg(unix)]
    fn hardlinks_share_file_ref_and_volume_guid() {
        // Two hardlinks of the same inode MUST produce the same
        // (file_ref, volume_guid) — that's the invariant T0.5's
        // partition_by_inode relies on to collapse them into one
        // link_equivalent group.
        let dir = std::env::temp_dir().join(format!(
            "sd-inode-identity-hl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let original = dir.join("a.bin");
        let link = dir.join("b.bin");
        let mut f = fs::File::create(&original).unwrap();
        f.write_all(b"shared").unwrap();
        drop(f);
        fs::hard_link(&original, &link).unwrap();

        let m_a = fs::metadata(&original).unwrap();
        let m_b = fs::metadata(&link).unwrap();
        let (ino_a, vol_a) = inode_identity(&m_a);
        let (ino_b, vol_b) = inode_identity(&m_b);
        assert_eq!(ino_a, ino_b, "hardlinked files must share file_ref");
        assert_eq!(vol_a, vol_b, "hardlinked files on same fs must share volume_guid");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn distinct_files_have_distinct_file_refs() {
        // Two genuinely separate files (cp, not ln) must have
        // different st_ino values. If this ever fails on a real
        // filesystem, it's a fs/kernel bug, not ours — but pin
        // the assumption.
        let dir = std::env::temp_dir().join(format!(
            "sd-inode-identity-distinct-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let (ino_a, _) = inode_identity(&fs::metadata(&a).unwrap());
        let (ino_b, _) = inode_identity(&fs::metadata(&b).unwrap());
        assert_ne!(ino_a, ino_b, "distinct files must have distinct file_refs");
        let _ = fs::remove_dir_all(&dir);
    }
}
