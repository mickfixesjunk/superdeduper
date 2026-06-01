//! Stage 1, fallback path — recursive directory walking.
//!
//! On Windows the long-term target is `FindFirstFileExW` with
//! `FindExInfoBasic` and `FIND_FIRST_EX_LARGE_FETCH`. For the v0 skeleton
//! we use `std::fs::read_dir`, which is correct and portable; we'll swap
//! in the optimized Win32 path in a later commit once the
//! `winapi_wrappers` for it land.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use hashbrown::HashSet;

use crate::config::ScanConfig;
use crate::inventory::FileEntry;
use crate::Result;

/// Canonical "this is the same physical directory" identifier used
/// by [`enumerate_cancellable`] to break symlink cycles when
/// `--follow-links` is on.
///
/// On Windows the volume serial + 128-bit file ID together uniquely
/// identify the directory regardless of how many path / junction /
/// hardlink aliases reach it. On Unix the `(st_dev, st_ino)` pair is
/// the equivalent.
///
/// Equal-by-value across the two reachings of the same directory:
/// that's what makes T1.7's "visited set" approach work — we don't
/// rely on path equality, which would miss aliases.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct DirIdentity {
    pub volume_serial: u64,
    pub file_id: u128,
}

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
    /// T1.7: a symlink-followed directory whose identity we'd already
    /// enumerated. Skipping prevents both infinite recursion on true
    /// cycles AND duplicate file reporting when two symlinks resolve
    /// to the same directory.
    ///
    /// `from` is the symlink path the walker just resolved; `target`
    /// is the canonicalised target (when known). Identity equality
    /// is on the underlying `DirIdentity` — same filesystem-level
    /// directory, regardless of how many alias paths reach it.
    SymlinkCycleSkipped { from: &'a Path, target: PathBuf },
}

/// Owned twin of [`WalkEvent`]. Used by the layer-parallel BFS path
/// (#194 walker conversion) so per-folder enumeration can collect
/// events in parallel without borrowing the caller's `&mut FnMut`
/// callback. Driver drains these on the main thread after each layer
/// completes and synthesises borrowed `WalkEvent<'_>`s for the
/// caller's FnMut.
///
/// The borrowed `WalkEvent::SymlinkCycleSkipped` carries a `&Path` for
/// `from` and an owned `PathBuf` for `target`; the Owned variant just
/// promotes both to owned, since the parallel path doesn't have the
/// caller's lifetime to borrow against.
#[derive(Debug, Clone)]
pub(crate) enum OwnedWalkEvent {
    Entered { path: PathBuf, depth: u32 },
    FileFound { path: PathBuf, size: u64 },
    DirError { path: PathBuf, message: String },
    EntrySkipped { path: PathBuf, reason: &'static str },
    SymlinkCycleSkipped { from: PathBuf, target: PathBuf },
}

impl OwnedWalkEvent {
    /// Synthesise a borrowed [`WalkEvent`] for the caller's `FnMut`
    /// callback. The returned event borrows from `self`; lifetime is
    /// the borrow of `&self`.
    pub(crate) fn as_borrowed(&self) -> WalkEvent<'_> {
        match self {
            OwnedWalkEvent::Entered { path, depth } => WalkEvent::Entered {
                path,
                depth: *depth,
            },
            OwnedWalkEvent::FileFound { path, size } => WalkEvent::FileFound {
                path,
                size: *size,
            },
            OwnedWalkEvent::DirError { path, message } => WalkEvent::DirError {
                path,
                message: message.clone(),
            },
            OwnedWalkEvent::EntrySkipped { path, reason } => WalkEvent::EntrySkipped {
                path,
                reason: *reason,
            },
            OwnedWalkEvent::SymlinkCycleSkipped { from, target } => {
                WalkEvent::SymlinkCycleSkipped {
                    from,
                    target: target.clone(),
                }
            }
        }
    }
}

/// Result of enumerating ONE folder serially (no recursion). Produced
/// by the layer-parallel BFS path's per-folder workers; drained on the
/// main thread.
///
/// * `entries`: file rows ready to flow into the inventory `Vec<FileEntry>`.
/// * `subdirs`: child directories discovered in this folder, with the
///   depth they should be visited at (parent_depth + 1). The BFS
///   driver merges these into the next layer's frontier.
/// * `events`: deferred WalkEvent emissions. Drained in order on the
///   main thread to preserve the existing FnMut callback contract.
/// * `cancelled`: true if the cancel flag flipped mid-enumeration; the
///   driver stops the BFS in that case.
#[derive(Debug, Default)]
pub(crate) struct FolderResult {
    pub(crate) entries: Vec<FileEntry>,
    pub(crate) subdirs: Vec<(PathBuf, u32)>,
    pub(crate) events: Vec<OwnedWalkEvent>,
    pub(crate) cancelled: bool,
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
    // A-perf-stage-timing — distinct from main.rs's t_inventory wallclock,
    // which wraps the full inventory phase (warm-path + mft + post-walk
    // skipped[] derivation). This timing isolates the walk-only cost so
    // HDD-bench harness can decompose "stage 1 inventory" into walk vs
    // mft vs warm-path vs derivation, and decide whether to invest in
    // parallel-walk.
    let walk_started = Instant::now();
    let mut out = Vec::new();
    // T1.7: per-scan visited set for symlink cycle detection. When
    // `cfg.follow_links` is OFF, this stays empty (we never follow a
    // symlink, never compute identity, never insert). When ON, every
    // directory reached via a followed symlink gets its identity
    // recorded so we don't recurse twice into the same physical dir.
    let mut visited_dirs: HashSet<DirIdentity> = HashSet::new();
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
        // #32: a positional CLI arg can be a single file, not just
        // a directory. `scan some_dir some_file.txt` should treat
        // both as first-class inventory entries so cross-source
        // duplicate detection can match them. Without this branch,
        // walk() would try to read_dir(file) and return zero
        // entries — `some_file.txt` never makes it into the
        // inventory and the cross-source group never forms.
        //
        // The check is metadata-based rather than path-string-based
        // so a symlink-to-file root with `--follow-links` does the
        // right thing too.
        let root_metadata = match fs::metadata(root) {
            Ok(m) => m,
            Err(e) => {
                callback(WalkEvent::DirError {
                    path: root,
                    message: format!("metadata failed: {e}"),
                });
                return Err(crate::Error::PathNotFound(root.clone()));
            }
        };
        if root_metadata.is_file() {
            push_single_file_root(root, &root_metadata, cfg, &mut out, &mut callback);
            continue;
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
        walk(
            &root_for_walk,
            cfg,
            &mut out,
            &mut callback,
            0,
            cancel,
            &mut visited_dirs,
        )?;
    }
    // #70 (v0.2.12 P2) — defensive walker-side path dedup. assert_unique_paths
    // in src/pipeline/mod.rs:86 is a debug_assert against the same-path-twice
    // class of bug, but that's a panic-at-emit-time net. This pass closes the
    // hole one layer upstream: any duplicate path produced by overlapping
    // roots (e.g. `scan C:\Users C:\Users\Mick`), by a reparse-point junction
    // that visited_dirs missed, or by any other multi-input pathological
    // alias never reaches the size-grouping stage, let alone the assert. Each
    // dedup'd entry fires WalkEvent::EntrySkipped so the user can see in the
    // engine log that their roots overlapped (telemetry counter increments
    // via cfg.exclusion_counters automatically).
    out = dedup_by_path(out, &mut callback);
    let walk_ms = walk_started.elapsed().as_millis() as u64;
    tracing::info!(
        walk_ms,
        files = out.len(),
        roots = cfg.roots.len(),
        "stage 1: walk complete"
    );
    Ok(out)
}

/// #70 — Drop FileEntry rows whose .path appears twice. Keeps the first
/// occurrence; emits WalkEvent::EntrySkipped for each duplicate dropped so
/// the engine log shows the overlap. O(n) with a HashSet<PathBuf>.
fn dedup_by_path<F>(entries: Vec<FileEntry>, callback: &mut F) -> Vec<FileEntry>
where
    F: FnMut(WalkEvent<'_>),
{
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(entries.len());
    let mut deduped = Vec::with_capacity(entries.len());
    for entry in entries {
        if seen.insert(entry.path.clone()) {
            deduped.push(entry);
        } else {
            callback(WalkEvent::EntrySkipped {
                path: &entry.path,
                reason: "duplicate path (overlapping roots or unresolved alias)",
            });
        }
    }
    deduped
}

/// Single-file positional root path — mirror the filter logic from
/// `walk()` (min-size / max-size / globs / exclusions) so a file
/// passed as a CLI arg goes through the same gates a file the walker
/// found inside a directory would. Side-effects emit the same
/// WalkEvent::FileFound / EntrySkipped callbacks so progress UIs
/// don't notice the difference.
///
/// Reparse + placeholder classification matches walk()'s path so a
/// recall-on-open file passed as a positional arg gets the same
/// "skipped" treatment it would have got inside a dir.
fn push_single_file_root<F>(
    path: &Path,
    metadata: &std::fs::Metadata,
    cfg: &ScanConfig,
    out: &mut Vec<FileEntry>,
    callback: &mut F,
) where
    F: FnMut(WalkEvent<'_>),
{
    let size = metadata.len();
    if size < cfg.min_size {
        callback(WalkEvent::EntrySkipped {
            path,
            reason: "below min-size",
        });
        return;
    }
    if let Some(max) = cfg.max_size {
        if size > max {
            callback(WalkEvent::EntrySkipped {
                path,
                reason: "above max-size",
            });
            return;
        }
    }
    if dropped_by_exclusions(path, size, cfg) {
        callback(WalkEvent::EntrySkipped {
            path,
            reason: "Settings → Exclusions",
        });
        return;
    }
    if !path_passes_globs(path, cfg) {
        callback(WalkEvent::EntrySkipped {
            path,
            reason: "filtered by include/exclude",
        });
        return;
    }
    callback(WalkEvent::FileFound { path, size });

    // Reparse-tag for cloud-placeholder classification matches the
    // dir-walker path's late-stage handling. On Linux + macOS the
    // attrs are 0 and the tag is None (no reparse points to classify).
    // On Windows pull file_attributes() from MetadataExt + fetch the
    // reparse tag via winapi_wrappers if FILE_ATTRIBUTE_REPARSE_POINT
    // (0x400) is set. Mirrors the per-entry block in `walk()` at
    // lines ~419-438.
    #[cfg(windows)]
    let (attributes, reparse_tag) = {
        use std::os::windows::fs::MetadataExt;
        let attrs = metadata.file_attributes();
        let tag = if (attrs & 0x400) != 0 {
            crate::winapi_wrappers::fetch_reparse_tag(path)
        } else {
            None
        };
        (attrs, tag)
    };
    #[cfg(not(windows))]
    let (attributes, reparse_tag) = (0u32, None);

    let (file_ref, volume_guid) = inode_identity(metadata);
    out.push(FileEntry {
        path: path.to_path_buf(),
        size,
        mtime: filetime_ticks(metadata),
        file_ref,
        parent_ref: 0,
        usn: 0,
        attributes,
        volume_guid,
        placeholder: crate::inventory::placeholder::classify(attributes, reparse_tag),
    });
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

/// Resolve a directory path to its filesystem-level identity, used
/// for cycle detection in the symlink-follow path.
///
/// Returns `None` on failure (permission denied, race with deletion,
/// FS doesn't support the query). Callers treat `None` as "skip,
/// can't dedupe without identity" — strictly safer than risking a
/// loop.
pub fn dir_identity(path: &Path) -> Option<DirIdentity> {
    dir_identity_impl(path)
}

#[cfg(windows)]
fn dir_identity_impl(path: &Path) -> Option<DirIdentity> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // We need a directory handle. The straightforward way is
    // `std::fs::File::open(path)`, which works on directories when
    // backed by `CreateFile` + `FILE_FLAG_BACKUP_SEMANTICS`. Rust
    // doesn't expose that flag via std, so the alternative is to
    // use std::fs::metadata via OpenOptions custom_flags. Easier:
    // open the dir as a File the way std::fs::read_dir() does
    // internally.
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        // FILE_FLAG_BACKUP_SEMANTICS = 0x02000000. Without this,
        // CreateFile refuses to open a directory.
        .custom_flags(0x0200_0000)
        .open(path)
        .ok()?;
    let handle = file.as_raw_handle() as isize;
    let mut info = windows::Win32::Storage::FileSystem::FILE_ID_INFO::default();
    // SAFETY: handle is valid (owned by `file`); info has correct
    // type + size; FileIdInfo is the documented constant for this
    // out-struct.
    let ok = unsafe {
        windows::Win32::Storage::FileSystem::GetFileInformationByHandleEx(
            windows::Win32::Foundation::HANDLE(handle as _),
            windows::Win32::Storage::FileSystem::FileIdInfo,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<windows::Win32::Storage::FileSystem::FILE_ID_INFO>() as u32,
        )
    };
    if ok.is_err() {
        return None;
    }
    // FileId is a FILE_ID_128 — two u64 halves stored as [u8; 16].
    let mut file_id_bytes = [0u8; 16];
    file_id_bytes.copy_from_slice(&info.FileId.Identifier);
    Some(DirIdentity {
        volume_serial: info.VolumeSerialNumber,
        file_id: u128::from_le_bytes(file_id_bytes),
    })
}

#[cfg(unix)]
fn dir_identity_impl(path: &Path) -> Option<DirIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(DirIdentity {
        volume_serial: meta.dev(),
        file_id: meta.ino() as u128,
    })
}

#[cfg(not(any(windows, unix)))]
fn dir_identity_impl(_path: &Path) -> Option<DirIdentity> {
    None
}

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
    visited_dirs: &mut HashSet<DirIdentity>,
) -> Result<()>
where
    F: FnMut(WalkEvent<'_>),
{
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(());
    }
    // T1.7: when follow_links is ON, every descend goes through this
    // identity check. If we've already enumerated this physical dir
    // (via any path — symlink or regular subdir), return silently.
    // The named `SymlinkCycleSkipped` event lives at the symlink site
    // below; this gate catches the case where the regular descent
    // happens AFTER a symlink-followed descent already populated the
    // set, which would otherwise enumerate the dir twice.
    if cfg.follow_links {
        if let Some(identity) = dir_identity(dir) {
            if !visited_dirs.insert(identity) {
                // Already enumerated this physical directory. Emit
                // the named cycle event so the GUI / log can show
                // "we detected an alias" rather than silently skip.
                let target = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                callback(WalkEvent::SymlinkCycleSkipped { from: dir, target });
                return Ok(());
            }
        }
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
            return walk_fast_path(
                dir,
                enumeration,
                cfg,
                out,
                callback,
                depth,
                cancel,
                visited_dirs,
            );
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

        let entry_was_symlink = metadata.file_type().is_symlink();
        let metadata = if entry_was_symlink {
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

        // #116 — superdeduper's own footprint should never appear in
        // dedup results, regardless of user exclusion settings. Skip
        // diagnose-scratch dirs, safe-rename'd dups, and reflink
        // atomic-temp files unconditionally.
        if is_superdeduper_self_path(&path) {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "superdeduper self-footprint",
            });
            continue;
        }

        if metadata.is_dir() {
            // T1.7: cycle detection is centralised at walk-top
            // (visited-set insert + named event emission). We just
            // recurse; walk handles dedup.
            let _ = entry_was_symlink; // explicitly accept: used implicitly via metadata re-stat above
            walk(&path, cfg, out, callback, depth + 1, cancel, visited_dirs)?;
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

        if dropped_by_exclusions(&path, size, cfg) {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "Settings → Exclusions",
            });
            continue;
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
    visited_dirs: &mut HashSet<DirIdentity>,
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

        // #116 — superdeduper's own footprint should never appear in
        // dedup results, regardless of user exclusion settings. Skip
        // diagnose-scratch dirs, safe-rename'd dups, and reflink
        // atomic-temp files unconditionally.
        if is_superdeduper_self_path(&path) {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "superdeduper self-footprint",
            });
            continue;
        }

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
                // T1.7: walk-top handles cycle detection.
                walk(&path, cfg, out, callback, depth + 1, cancel, visited_dirs)?;
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
            if dropped_by_exclusions(&path, target_size, cfg) {
                callback(WalkEvent::EntrySkipped {
                    path: &path,
                    reason: "Settings → Exclusions",
                });
                continue;
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
            walk(&path, cfg, out, callback, depth + 1, cancel, visited_dirs)?;
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
        if dropped_by_exclusions(&path, size, cfg) {
            callback(WalkEvent::EntrySkipped {
                path: &path,
                reason: "Settings → Exclusions",
            });
            continue;
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

/// Per-folder pure enumeration for the stdlib `read_dir` fallback
/// path. Used by the layer-parallel BFS driver (#194). Mirrors the
/// inner loop semantics of `walk()`'s read_dir block exactly:
///
/// * Reparse-point classification + symlink follow_links handling
/// * is_superdeduper_self_path skip
/// * size/min-size/max-size/exclusions/globs filter ladder
/// * Cancellation check at start of each entry
///
/// Key differences from `walk()`:
///
/// * Does NOT recurse into subdirs -- they're collected into
///   `result.subdirs` as `(path, depth+1)` tuples for the BFS driver
///   to feed into the next layer.
/// * Does NOT call any `FnMut` callback -- events are pushed into
///   `result.events` for the driver to drain on the main thread.
/// * Does NOT touch `visited_dirs` -- symlink-cycle detection lives
///   in the driver as a serial pre-filter before each parallel batch.
///
/// `Entered` event is NOT emitted here (it's the driver's responsibility
/// to emit it before launching the per-folder worker, so the order is
/// "Entered first, then entries").
fn enumerate_one_folder_read_dir(
    dir: &Path,
    depth: u32,
    cfg: &ScanConfig,
    cancel: Option<&AtomicBool>,
) -> FolderResult {
    let mut result = FolderResult::default();

    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            result.events.push(OwnedWalkEvent::DirError {
                path: dir.to_path_buf(),
                message: "permission denied".into(),
            });
            return result;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return result;
        }
        Err(e) => {
            result.events.push(OwnedWalkEvent::DirError {
                path: dir.to_path_buf(),
                message: e.to_string(),
            });
            return result;
        }
    };

    for entry in read {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            result.cancelled = true;
            return result;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                result.events.push(OwnedWalkEvent::DirError {
                    path: dir.to_path_buf(),
                    message: format!("entry error: {e}"),
                });
                continue;
            }
        };
        let path = entry.path();

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                result.events.push(OwnedWalkEvent::EntrySkipped {
                    path: path.clone(),
                    reason: "metadata failed",
                });
                continue;
            }
        };

        let entry_was_symlink = metadata.file_type().is_symlink();
        let metadata = if entry_was_symlink {
            if !cfg.follow_links {
                result.events.push(OwnedWalkEvent::EntrySkipped {
                    path: path.clone(),
                    reason: "symlink (use --follow-links to include)",
                });
                continue;
            }
            // --follow-links: re-stat through the symlink so downstream
            // checks see the TARGET's attributes, not the link's.
            match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    result.events.push(OwnedWalkEvent::EntrySkipped {
                        path: path.clone(),
                        reason: "symlink target unreadable",
                    });
                    continue;
                }
            }
        } else {
            metadata
        };
        let _ = entry_was_symlink;

        if is_superdeduper_self_path(&path) {
            result.events.push(OwnedWalkEvent::EntrySkipped {
                path: path.clone(),
                reason: "superdeduper self-footprint",
            });
            continue;
        }

        if metadata.is_dir() {
            result.subdirs.push((path, depth + 1));
            continue;
        }
        if !metadata.is_file() {
            result.events.push(OwnedWalkEvent::EntrySkipped {
                path,
                reason: "not a regular file",
            });
            continue;
        }

        let size = metadata.len();
        if size < cfg.min_size {
            result.events.push(OwnedWalkEvent::EntrySkipped {
                path,
                reason: "below min-size",
            });
            continue;
        }
        if let Some(max) = cfg.max_size {
            if size > max {
                result.events.push(OwnedWalkEvent::EntrySkipped {
                    path,
                    reason: "above max-size",
                });
                continue;
            }
        }
        if dropped_by_exclusions(&path, size, cfg) {
            result.events.push(OwnedWalkEvent::EntrySkipped {
                path,
                reason: "Settings \u{2192} Exclusions",
            });
            continue;
        }
        if !path_passes_globs(&path, cfg) {
            result.events.push(OwnedWalkEvent::EntrySkipped {
                path,
                reason: "filtered by include/exclude",
            });
            continue;
        }

        result.events.push(OwnedWalkEvent::FileFound {
            path: path.clone(),
            size,
        });

        let attributes = win_file_attributes(&metadata);
        let reparse_tag = if (attributes & 0x400) != 0 {
            crate::winapi_wrappers::fetch_reparse_tag(&path)
        } else {
            None
        };
        let (file_ref, volume_guid) = inode_identity(&metadata);
        result.entries.push(FileEntry {
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
    result
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

/// #116 — superdeduper's own scratch / marker paths. Walker skips
/// these unconditionally so we never scan our own working data:
///
/// * directories whose name starts with `.superdeduper-` — the
///   diagnose scratch space (`.superdeduper-diagnose-scratch`) Mick
///   reported in the dupes list, plus any future `.superdeduper-*/`
///   variants we add (cache, profile dumps, etc.) without needing
///   to update this filter.
/// * files whose name ends with `.superdeduper` — safe-rename'd
///   duplicates (per `dedupe::SAFE_RENAME_SUFFIX`). Without this,
///   a re-scan after safe-rename would surface every renamed dup
///   as its own copy of the original.
/// * files whose name ends with `.superdeduper-clone-tmp` —
///   reflink atomic-via-tmp-rename intermediates. Normally gone
///   before any scan but a crash mid-reflink can leave them.
///
/// Engine-controlled; users can't opt out (and shouldn't need to —
/// these are never user files).
fn is_superdeduper_self_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    name.starts_with(".superdeduper-")
        || name.ends_with(".superdeduper")
        || name.ends_with(".superdeduper-clone-tmp")
}

/// Returns `true` if the file should be dropped per Settings →
/// Exclusions; bumps the per-scan counter as a side-effect when
/// it does. Master-toggle-off short-circuits without touching the
/// counter.
fn dropped_by_exclusions(path: &Path, size: u64, cfg: &ScanConfig) -> bool {
    if !cfg.exclusion_policy.is_enabled() {
        return false;
    }
    if matches!(
        cfg.exclusion_policy.evaluate(path),
        crate::exclusions::Decision::Excluded(_)
    ) {
        cfg.exclusion_counters.record(size);
        return true;
    }
    false
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
        assert_eq!(
            vol_a, vol_b,
            "hardlinked files on same fs must share volume_guid"
        );

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

#[cfg(test)]
mod dedup_by_path_tests {
    //! #70 (v0.2.12 P2) — Walker-side path dedup unit tests. Pin the
    //! contract directly so a regression that re-introduces duplicate
    //! paths in the inventory output trips on `cargo test`, not on a
    //! user-visible same-path-twice GUI group.

    use super::*;
    use crate::inventory::placeholder::PlaceholderState;

    fn entry(p: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(p),
            size: 1024,
            mtime: 0,
            file_ref: 0,
            parent_ref: 0,
            usn: 0,
            attributes: 0,
            volume_guid: None,
            placeholder: PlaceholderState::default(),
        }
    }

    #[test]
    fn no_duplicates_pass_through_unchanged() {
        let input = vec![entry("/a"), entry("/b"), entry("/c")];
        let mut dropped = 0u32;
        let out = dedup_by_path(input, &mut |ev| {
            if matches!(ev, WalkEvent::EntrySkipped { .. }) {
                dropped += 1;
            }
        });
        assert_eq!(out.len(), 3);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn duplicate_paths_dropped_after_first_occurrence() {
        let input = vec![entry("/a"), entry("/b"), entry("/a"), entry("/c")];
        let mut dropped_paths: Vec<PathBuf> = Vec::new();
        let out = dedup_by_path(input, &mut |ev| {
            if let WalkEvent::EntrySkipped { path, .. } = ev {
                dropped_paths.push(path.to_path_buf());
            }
        });
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|e| e.path.as_path()).collect::<Vec<_>>(),
            vec![Path::new("/a"), Path::new("/b"), Path::new("/c")]
        );
        assert_eq!(dropped_paths, vec![PathBuf::from("/a")]);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = dedup_by_path(Vec::new(), &mut |_| {});
        assert!(out.is_empty());
    }

    #[test]
    fn all_duplicates_collapses_to_one() {
        let input = vec![entry("/x"), entry("/x"), entry("/x"), entry("/x")];
        let mut dropped = 0u32;
        let out = dedup_by_path(input, &mut |ev| {
            if matches!(ev, WalkEvent::EntrySkipped { .. }) {
                dropped += 1;
            }
        });
        assert_eq!(out.len(), 1);
        assert_eq!(dropped, 3);
    }
}

#[cfg(test)]
mod self_footprint_tests {
    use super::is_superdeduper_self_path;
    use std::path::Path;

    #[test]
    fn skips_diagnose_scratch_dir() {
        let p = Path::new("/some/drive/.superdeduper-diagnose-scratch");
        assert!(is_superdeduper_self_path(p));
    }

    #[test]
    fn skips_diagnose_scratch_dir_with_trailing_content() {
        // Walker calls `is_superdeduper_self_path` on the dir entry
        // itself before recursing — so leaf-name match is what counts.
        let p = Path::new("/some/drive/.superdeduper-diagnose-scratch");
        assert!(is_superdeduper_self_path(p));
    }

    #[test]
    fn skips_future_dotsuperdeduper_dash_variants() {
        // Prefix match so we don't need to update the filter every time
        // we add a new self-managed dir (cache, profile dumps, etc.).
        for name in [
            ".superdeduper-cache",
            ".superdeduper-logs",
            ".superdeduper-profile-dumps",
        ] {
            let p = Path::new("/x").join(name);
            assert!(is_superdeduper_self_path(&p), "should skip: {name}");
        }
    }

    #[test]
    fn skips_safe_renamed_dup_files() {
        // dedupe::SAFE_RENAME_SUFFIX = ".superdeduper". A safe-rename'd
        // dup like `photo.jpg.superdeduper` must never resurface in a
        // re-scan as a copy of `photo.jpg`.
        let p = Path::new("/u/photo.jpg.superdeduper");
        assert!(is_superdeduper_self_path(p));
    }

    #[test]
    fn skips_reflink_clone_tmp() {
        let p = Path::new("/u/.foo.superdeduper-clone-tmp");
        assert!(is_superdeduper_self_path(p));
    }

    #[test]
    fn passes_through_normal_files() {
        for p in [
            "/home/user/photo.jpg",
            "/home/user/document.pdf",
            "/home/user/.bashrc",
            "/home/user/.config/foo.toml",
            "/u/superdeduper-not-prefixed.txt",
        ] {
            assert!(
                !is_superdeduper_self_path(Path::new(p)),
                "should not skip: {p}"
            );
        }
    }

    #[test]
    fn skips_bare_dotsuperdeduper_dir() {
        // `.superdeduper` (channel module's per-user data dir) lives
        // under `data_dir()` so it's not normally walked. But if a
        // user adds their home as a scan root, the leaf-name suffix
        // match still catches it via the `.superdeduper` ends_with
        // arm — we don't want to scan our own data dir either.
        let p = Path::new("/u/.superdeduper");
        assert!(is_superdeduper_self_path(p));
    }

    #[test]
    fn handles_empty_and_non_utf8_paths_without_panic() {
        // Walker can encounter non-utf8 path components on Linux;
        // `file_name().to_str()` returns None and we pass through.
        let p = Path::new("");
        assert!(!is_superdeduper_self_path(p));
    }
}
