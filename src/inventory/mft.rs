//! Stage 1, fast path — direct MFT enumeration via `FSCTL_ENUM_USN_DATA`.
//!
//! For every NTFS volume covered by the scan roots, we:
//! 1. Open `\\.\Volume{…}\` with `CreateFileW`.
//! 2. Page through `FSCTL_ENUM_USN_DATA`, accumulating
//!    `(file_ref, parent_ref, name, attributes, usn)` for every
//!    on-volume record.
//! 3. Reconstruct full paths bottom-up by walking parent refs, caching
//!    every directory's resolved path so each is built at most once.
//! 4. Filter the result to records whose reconstructed path falls
//!    under one of the user's scan roots and matches the include /
//!    exclude globs.
//!
//! Records we can't reconstruct (orphans, unreachable parents) are
//! logged at debug level and dropped — they never appear in the
//! output. The walker fallback picks them up if the user disabled MFT
//! enumeration entirely.

use std::path::{Path, PathBuf};

use crate::config::ScanConfig;
use crate::inventory::FileEntry;
use crate::Result;

#[cfg(windows)]
pub fn enumerate(
    cfg: &ScanConfig,
    cache: Option<&std::sync::Arc<parking_lot::Mutex<crate::cache::Cache>>>,
) -> Result<Vec<FileEntry>> {
    use hashbrown::HashMap;

    use crate::winapi_wrappers::volume_for_path;

    // Group roots by volume.
    //
    // `std::fs::canonicalize` on Windows returns a verbatim path
    // (`\\?\F:\Github`), but the path we reconstruct from MFT
    // records starts with `F:\…` — no verbatim prefix. Comparing
    // those with `starts_with` returns false for every file, which
    // before this fix meant a scan of `F:\Github` from an
    // MFT-capable shell finished in seconds with 0 files. Strip the
    // verbatim prefix here so the two path shapes line up.
    let mut roots_by_volume: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for root in &cfg.roots {
        let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let abs = strip_verbatim_prefix(&abs);
        let vol = volume_for_path(&abs)?;
        roots_by_volume.entry(vol).or_default().push(abs);
    }

    let mut out = Vec::new();
    for (volume, roots) in roots_by_volume {
        // Try the warm path first when we have a cache to persist /
        // restore the snapshot through. Falls through to the full
        // MFT walk when there's no snapshot yet, the journal
        // wrapped, or the delta references a subtree we don't have.
        // `--no-cache` (cfg.use_cache=false) also skips the warm
        // path so a flag named --no-cache really means "fresh
        // everything", not "fresh hashes but reuse the snapshot."
        let warm_outcome = if let (Some(c), true) = (cache, cfg.use_cache) {
            match crate::inventory::warm::try_warm(cfg, &volume, &roots, c) {
                Ok(o) => Some(o),
                Err(e) => {
                    tracing::info!(
                        volume = %volume,
                        error = %e,
                        "warm-path errored; falling back to cold MFT"
                    );
                    None
                }
            }
        } else {
            None
        };
        match warm_outcome {
            Some(crate::inventory::warm::WarmOutcome::Applied {
                files,
                delta_records,
                created,
                updated,
                deleted,
            }) => {
                tracing::info!(
                    volume = %volume,
                    files = files.len(),
                    delta_records,
                    created,
                    updated,
                    deleted,
                    "warm-path Stage 1: applied USN delta"
                );
                out.extend(files);
                continue;
            }
            Some(crate::inventory::warm::WarmOutcome::Fallback { reason }) => {
                tracing::info!(
                    volume = %volume,
                    reason,
                    "warm-path Stage 1: falling back to cold MFT"
                );
            }
            None => {}
        }
        out.extend(enumerate_volume(&volume, &roots, cfg, cache)?);
    }
    Ok(out)
}

#[cfg(not(windows))]
pub fn enumerate(
    _cfg: &ScanConfig,
    _cache: Option<&std::sync::Arc<parking_lot::Mutex<crate::cache::Cache>>>,
) -> Result<Vec<FileEntry>> {
    Err(crate::Error::Unsupported(
        "inventory::mft: only available on Windows",
    ))
}

#[cfg(windows)]
fn enumerate_volume(
    volume: &str,
    roots: &[PathBuf],
    cfg: &ScanConfig,
    cache: Option<&std::sync::Arc<parking_lot::Mutex<crate::cache::Cache>>>,
) -> Result<Vec<FileEntry>> {
    use hashbrown::HashMap;

    use crate::winapi_wrappers::{query_usn_journal_state, UsnEnum, UsnJournalState, UsnRecord};

    // Capture the journal state BEFORE doing the MFT enumeration so
    // the cursor we persist is consistent: anything written to the
    // volume *during* enumeration will be > this `next_usn` and
    // therefore picked up by the next scan's delta. If we queried
    // *after* enum (which is what an earlier version did) the
    // saved cursor would skip past concurrent activity, leaving
    // those changes invisible until the file involved is touched
    // again. Skipping the query entirely is fine — we just won't
    // be warm-eligible next time.
    let pre_enum_journal: Option<UsnJournalState> = query_usn_journal_state(volume).ok();

    let mut by_ref: HashMap<u64, UsnRecord> = HashMap::new();

    let mut enumerator = UsnEnum::open(volume)?;
    while let Some(batch) = enumerator.next_batch()? {
        for r in batch {
            by_ref.insert(r.file_ref, r);
        }
    }

    // `volume` is the `\\?\Volume{…}\` GUID form — correct for opening
    // the raw device, *wrong* for building file paths users can match
    // against roots like `F:\Github`. Derive the drive-letter prefix
    // from any of the roots (they all share this volume so any root
    // works). Fall back to the trimmed GUID only if no root carries a
    // disk prefix (defensive: the roots have already been
    // canonicalised + verbatim-stripped before we got here).
    // NB the trailing backslash: on Windows `F:` is drive-relative
    // (current dir on F:) while `F:\` is the drive root.
    // `PathBuf::from("F:").push("GitHub")` yields `F:GitHub` —
    // missing the separator — and `starts_with("F:\\GitHub")` then
    // returns false for every file. The trailing backslash makes
    // sure push() does the right thing.
    let volume_root = roots
        .iter()
        .find_map(|r| {
            use std::path::{Component, Prefix};
            let mut comps = r.components();
            if let Some(Component::Prefix(p)) = comps.next() {
                if let Prefix::Disk(letter) = p.kind() {
                    return Some(PathBuf::from(format!("{}:\\", letter as char)));
                }
            }
            None
        })
        .unwrap_or_else(|| PathBuf::from(volume.trim_end_matches('\\')));
    let mut path_cache: HashMap<u64, PathBuf> = HashMap::new();
    let mut out = Vec::new();
    // Capture per-file size for the snapshot persistence pass below.
    // Without this, the snapshot writes `size: 0` for every file and
    // the warm-path Stage 1 inventory comes back with all-zero sizes
    // for files that weren't part of the USN delta — collapsing the
    // entire corpus into a single 0-byte size group. See the bug
    // reported on `sdd-testwin-superdeduper.md` 2026-05-22T17:35Z.
    let mut file_sizes: HashMap<u64, u64> = HashMap::new();
    // Filter-pass counters — emitted as a single tracing event at the
    // end so "MFT found 250k records, 0 made it past root match" is a
    // one-line diagnosis instead of mysterious silence. This is what
    // would have caught the verbatim-prefix and GUID-vs-drive-letter
    // bugs immediately.
    let total_records = by_ref.len();
    let mut filtered_dir = 0usize;
    let mut filtered_no_path = 0usize;
    let mut filtered_root = 0usize;
    let mut filtered_metadata = 0usize;
    let mut filtered_size = 0usize;
    let mut filtered_glob = 0usize;
    for (_ref, record) in &by_ref {
        // Skip directories — only files become FileEntry rows.
        let is_dir = (record.attributes & 0x10) != 0; // FILE_ATTRIBUTE_DIRECTORY
        if is_dir {
            filtered_dir += 1;
            continue;
        }
        let full = match reconstruct_path(record.file_ref, &by_ref, &mut path_cache, &volume_root) {
            Some(p) => p,
            None => {
                filtered_no_path += 1;
                continue;
            }
        };
        if !under_any_root(&full, roots) {
            filtered_root += 1;
            continue;
        }
        // FSCTL_ENUM_USN_DATA doesn't include file size. We fetch it via
        // GetFileAttributesEx for files that survived the path filter.
        let size = match std::fs::metadata(&full) {
            Ok(m) => m.len(),
            Err(_) => {
                filtered_metadata += 1;
                continue;
            }
        };
        // Remember the size for the cold-snapshot pass below. Files
        // outside scan scope (filtered_root, filtered_metadata) stay
        // absent from this map and the snapshot writes 0 for them —
        // they're never going to be hashed in this run anyway. The
        // warm-path's metadata re-stat is the safety net if they
        // ever come into scope on a future run.
        file_sizes.insert(record.file_ref, size);
        if size < cfg.min_size {
            filtered_size += 1;
            continue;
        }
        if let Some(max) = cfg.max_size {
            if size > max {
                filtered_size += 1;
                continue;
            }
        }
        if !path_passes_globs(&full, cfg) {
            filtered_glob += 1;
            continue;
        }
        out.push(FileEntry {
            path: full,
            size,
            mtime: record.mtime_filetime,
            file_ref: record.file_ref,
            parent_ref: record.parent_ref,
            usn: record.usn,
            attributes: record.attributes,
            volume_guid: Some(volume.to_string()),
        });
    }

    // Empty-result diagnostic: if we found MFT records but nothing
    // survived the filter, that's almost certainly a bug (wrong
    // volume_root prefix, mis-canonicalised root, etc.). Promote to
    // WARN so the default log level shows it without `-v`.
    let anomaly = total_records > 0 && out.is_empty();
    if anomaly {
        tracing::warn!(
            volume = %volume,
            roots = ?roots,
            volume_root = %volume_root.display(),
            total_mft_records = total_records,
            accepted = out.len(),
            filtered_dir,
            filtered_no_path,
            filtered_root,
            filtered_metadata,
            filtered_size,
            filtered_glob,
            "MFT enumeration produced records but none survived filtering — \
             check root path vs MFT path prefix (likely a Drive: vs \\\\?\\Volume{{…}} mismatch)",
        );
    } else {
        tracing::info!(
            volume = %volume,
            roots = ?roots,
            volume_root = %volume_root.display(),
            total_mft_records = total_records,
            accepted = out.len(),
            filtered_dir,
            filtered_no_path,
            filtered_root,
            filtered_metadata,
            filtered_size,
            filtered_glob,
            "mft inventory filter pass",
        );
    }

    // Persist a fresh snapshot so the next scan can take the warm
    // path. Captures every record we walked (directories included
    // — `reconstruct_path` needs them) plus the USN cursor we just
    // observed. Errors are non-fatal: the cold scan already
    // succeeded, we just lose the next-warm-scan optimisation.
    if let Some(c) = cache {
        if let Some(journal) = pre_enum_journal {
            if let Err(e) = persist_cold_snapshot(volume, &by_ref, &file_sizes, journal, c) {
                tracing::warn!(
                    volume = %volume,
                    error = %e,
                    "failed to persist inventory snapshot after cold MFT walk — \
                     next scan will pay full cold cost too"
                );
            }
        } else {
            tracing::info!(
                volume = %volume,
                "no pre-enum USN journal state captured (FSCTL_QUERY_USN_JOURNAL \
                 failed earlier); skipping snapshot save"
            );
        }
    }

    Ok(out)
}

/// Save the post-cold-walk snapshot for `volume` so subsequent
/// scans can take the warm path. Pulled into its own function so
/// the cold-walk loop above doesn't grow another bloc of cache
/// manipulation. The `journal` argument is the state captured
/// **before** the MFT enumeration started — using that as the
/// cursor guarantees the next scan's delta covers anything written
/// during the (potentially long) MFT walk.
#[cfg(windows)]
fn persist_cold_snapshot(
    volume: &str,
    by_ref: &hashbrown::HashMap<u64, crate::winapi_wrappers::UsnRecord>,
    file_sizes: &hashbrown::HashMap<u64, u64>,
    journal: crate::winapi_wrappers::UsnJournalState,
    cache: &std::sync::Arc<parking_lot::Mutex<crate::cache::Cache>>,
) -> Result<()> {
    use crate::cache::{InventoryMeta, InventoryRecord};

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;

    let records: Vec<(u64, InventoryRecord)> = by_ref
        .iter()
        .map(|(&file_ref, r)| {
            let is_dir = (r.attributes & 0x10) != 0;
            // Directories carry the -1 sentinel (they're never
            // hashed, and the size field is unused for them).
            // Files use the size stat'd during the filter pass
            // (file_sizes map). Files outside scan scope weren't
            // stat'd and stay at 0; warm-path metadata re-stat is
            // the safety net if they ever come into scope on a
            // future run. The previous shape unconditionally wrote
            // size=0 for every file, which made the warm-path
            // Stage 1 inventory return all-zero sizes for any
            // file that wasn't part of the USN delta — collapsing
            // duplicate detection completely.
            let size = if is_dir {
                -1
            } else {
                file_sizes.get(&file_ref).copied().unwrap_or(0) as i64
            };
            (
                file_ref,
                InventoryRecord {
                    parent_ref: r.parent_ref,
                    usn: r.usn,
                    attributes: r.attributes,
                    name: r.name.clone(),
                    size,
                    mtime: r.mtime_filetime as u64,
                },
            )
        })
        .collect();

    let meta = InventoryMeta {
        journal_id: journal.journal_id,
        // Use the pre-enum `next_usn` as the cursor. Critically NOT
        // `max(max_usn_from_records, next_usn)`: a record's stored
        // USN can be anything ≤ the journal's next_usn-at-write-
        // time, but if the journal was recreated/reset between
        // the record being written and our snapshot, the record's
        // USN can be *higher* than the new journal's next_usn —
        // and `FSCTL_READ_USN_JOURNAL(StartUsn = that)` rejects
        // with ERROR_INVALID_PARAMETER on the next scan. Using
        // pre-enum journal.next_usn keeps the cursor inside the
        // live journal's range; concurrent writes during enum
        // land at USNs > this point and surface as delta records
        // on the next scan.
        last_usn: journal.next_usn,
        captured_at_unix: now,
    };
    cache
        .lock()
        .save_inventory_snapshot(volume, &meta, &records)?;
    Ok(())
}

#[cfg(windows)]
fn reconstruct_path(
    file_ref: u64,
    by_ref: &hashbrown::HashMap<u64, crate::winapi_wrappers::UsnRecord>,
    cache: &mut hashbrown::HashMap<u64, PathBuf>,
    volume_root: &Path,
) -> Option<PathBuf> {
    if let Some(p) = cache.get(&file_ref) {
        return Some(p.clone());
    }
    // Walk parents up to a sane recursion bound to break loops on bad
    // metadata.
    let record = by_ref.get(&file_ref)?;
    let mut segments = vec![record.name.clone()];
    let mut cursor = record.parent_ref;
    for _ in 0..1024 {
        if cursor == 0 || cursor == record.file_ref {
            break;
        }
        let parent = match by_ref.get(&cursor) {
            Some(p) => p,
            None => break,
        };
        if (parent.attributes & 0x10) == 0 {
            // Non-directory parent ⇒ malformed.
            return None;
        }
        if parent.name.is_empty() || parent.parent_ref == cursor {
            break;
        }
        segments.push(parent.name.clone());
        cursor = parent.parent_ref;
    }
    let mut path = volume_root.to_path_buf();
    for s in segments.iter().rev() {
        path.push(s);
    }
    cache.insert(file_ref, path.clone());
    Some(path)
}

/// Strip Windows' `\\?\` verbatim prefix from a canonicalised path
/// so it compares cleanly against the `Drive:\…` style paths that
/// MFT enumeration produces. No-op on non-Windows shapes.
#[cfg(windows)]
fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut comps = p.components();
    if let Some(Component::Prefix(prefix)) = comps.next() {
        match prefix.kind() {
            Prefix::VerbatimDisk(letter) => {
                // `\\?\C:\foo` → `C:\foo`.
                let mut out = PathBuf::from(format!("{}:\\", letter as char));
                for c in comps {
                    if matches!(c, Component::RootDir) {
                        continue;
                    }
                    out.push(c.as_os_str());
                }
                return out;
            }
            Prefix::VerbatimUNC(_, _) | Prefix::Verbatim(_) => {
                // Less common shapes — at least drop the `\\?\`.
                let s = p.to_string_lossy();
                if let Some(rest) = s.strip_prefix(r"\\?\") {
                    return PathBuf::from(rest);
                }
            }
            _ => {}
        }
    }
    p.to_path_buf()
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    p.to_path_buf()
}

#[allow(dead_code)]
fn under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return true;
    }
    for r in roots {
        if path.starts_with(r) {
            return true;
        }
    }
    false
}

#[allow(dead_code)]
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

#[cfg(all(test, windows))]
mod tests {
    //! Regression coverage for the Windows path-prefix surgery. Tests
    //! gated on `windows` so the cross-platform CI on Linux still
    //! passes — these specifically exercise `PathBuf` Windows
    //! drive-prefix semantics.
    use super::*;

    #[test]
    fn volume_root_push_keeps_separator() {
        // The crux of the F:\Github bug: `PathBuf::from("F:")` is
        // drive-relative, not drive-root. Pushing "GitHub" produced
        // "F:GitHub" (no separator) and starts_with("F:\\GitHub")
        // returned false for every file in the volume.
        let mut p = PathBuf::from("F:\\");
        p.push("GitHub");
        p.push("subdir");
        assert_eq!(p.to_string_lossy().as_ref(), r"F:\GitHub\subdir");

        // And starts_with against the canonicalised-then-verbatim-
        // stripped root must succeed:
        let root = PathBuf::from(r"F:\GitHub");
        assert!(p.starts_with(&root));
    }

    #[test]
    fn drive_relative_root_does_not_satisfy_starts_with() {
        // Asserts the *opposite* shape — guards against someone
        // "simplifying" the trailing backslash back out.
        let mut p = PathBuf::from("F:");
        p.push("GitHub");
        let root = PathBuf::from(r"F:\GitHub");
        assert!(
            !p.starts_with(&root),
            "if this assertion ever fires, the Windows PathBuf semantics changed \
             and the trailing-backslash workaround can be removed"
        );
    }

    #[test]
    fn strip_verbatim_prefix_drive_form() {
        let p = PathBuf::from(r"\\?\F:\GitHub");
        let stripped = strip_verbatim_prefix(&p);
        assert_eq!(stripped.to_string_lossy().as_ref(), r"F:\GitHub");
    }
}
