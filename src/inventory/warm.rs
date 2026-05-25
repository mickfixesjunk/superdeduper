//! Warm-path Stage 1: apply a USN-journal delta to a cached
//! inventory baseline instead of re-walking the whole MFT.
//!
//! The plan:
//!
//! 1. **Validate the journal.** Ask Windows for the current
//!    `FSCTL_QUERY_USN_JOURNAL` state. If `journal_id` doesn't
//!    match what we saved last time, or our saved cursor is older
//!    than `first_usn` (journal wrapped or was rolled), the
//!    cursor is meaningless — return `None` and let the caller
//!    fall back to a full cold MFT walk.
//!
//! 2. **Load the snapshot.** Pull every persisted MFT record for
//!    this volume out of `inventory_records`. ~50 MB for a 500k-file
//!    AppData snapshot, comfortably under budget.
//!
//! 3. **Read the delta.** `FSCTL_READ_USN_JOURNAL` from the saved
//!    cursor forward yields one `UsnRecord` per change since last
//!    scan.
//!
//! 4. **Apply the delta.** Each USN reason gets mapped to a
//!    create/update/delete on the in-memory `by_ref` map. Where
//!    the record alone doesn't carry the new metadata (USN has the
//!    name + parent ref + reason mask but not size or mtime), we
//!    `std::fs::metadata` the path. That's per-changed-file —
//!    typical inter-scan churn on AppData is a few thousand
//!    records, so we trade 500k stat calls for ~5000.
//!
//! 5. **Reconstruct paths + save.** Same `reconstruct_path` the
//!    cold MFT path uses. New snapshot + cursor go back to the
//!    cache for next time.
//!
//! If at any step the delta references a parent_ref we don't
//! have (someone created a directory in a tree we never walked),
//! we currently conservatively return `None` — the caller falls
//! back to cold MFT and next time around the snapshot will be
//! complete. A future refinement could do a targeted directory
//! walk for just the missing subtree.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::cache::Cache;
use crate::config::ScanConfig;
use crate::inventory::FileEntry;
use crate::Result;

#[cfg(windows)]
use crate::cache::{InventoryMeta, InventoryRecord};
#[cfg(windows)]
use hashbrown::HashMap;
#[cfg(windows)]
use std::path::Path;

/// Outcome of a warm-path attempt for one volume. The caller pairs
/// these with the inventory it built so the diagnostic line in
/// `inventory::mft::enumerate` can say *which* path fired for
/// *which* volume.
pub enum WarmOutcome {
    /// Warm path succeeded — these `FileEntry`s came from
    /// `baseline + delta` and the snapshot has been re-saved.
    Applied {
        files: Vec<FileEntry>,
        delta_records: u64,
        created: u64,
        updated: u64,
        deleted: u64,
    },
    /// Warm path bailed; caller must do the full MFT walk. Reason
    /// is logged at info level so we can see which volumes are
    /// hitting which fallback path.
    Fallback { reason: &'static str },
}

#[cfg(windows)]
pub fn try_warm(
    cfg: &ScanConfig,
    volume_guid: &str,
    roots: &[PathBuf],
    cache: &Arc<Mutex<Cache>>,
) -> Result<WarmOutcome> {
    use crate::winapi_wrappers::{query_usn_journal_state, read_usn_journal_delta};

    let saved_meta = match cache.lock().load_inventory_meta(volume_guid)? {
        Some(m) => m,
        None => {
            return Ok(WarmOutcome::Fallback {
                reason: "no snapshot yet",
            })
        }
    };

    let live = match query_usn_journal_state(volume_guid) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(
                volume = %volume_guid,
                error = %e,
                "USN journal query failed; falling back to cold MFT"
            );
            return Ok(WarmOutcome::Fallback {
                reason: "journal query failed",
            });
        }
    };

    if live.journal_id != saved_meta.journal_id {
        // Journal was deleted + recreated since we last looked.
        // Saved cursor is meaningless against the new journal.
        cache.lock().invalidate_inventory_snapshot(volume_guid)?;
        return Ok(WarmOutcome::Fallback {
            reason: "journal id changed",
        });
    }
    if saved_meta.last_usn < live.first_usn {
        // Journal rolled past our cursor; the records we needed
        // are gone.
        cache.lock().invalidate_inventory_snapshot(volume_guid)?;
        return Ok(WarmOutcome::Fallback {
            reason: "journal rolled past cursor",
        });
    }
    if saved_meta.last_usn > live.next_usn {
        // Saved cursor is beyond what the journal has allocated.
        // Shouldn't happen with the pre-enum-cursor fix in
        // `inventory::mft::persist_cold_snapshot`, but if a stale
        // snapshot from a build that had the
        // `max(max_usn, next_usn)` bug is still on disk, this
        // would otherwise hit ERROR_INVALID_PARAMETER on the
        // FSCTL_READ_USN_JOURNAL call. Invalidate and cold-walk;
        // the fresh snapshot we write at the end will be in range.
        cache.lock().invalidate_inventory_snapshot(volume_guid)?;
        return Ok(WarmOutcome::Fallback {
            reason: "saved cursor past journal head (stale snapshot from older build)",
        });
    }

    // Load every saved record for this volume into a HashMap keyed
    // by file_ref. `reconstruct_path` will use it just like the
    // cold path uses its fresh `by_ref` build.
    let baseline = cache.lock().load_inventory_records(volume_guid)?;
    let baseline_loaded = baseline.len();
    let mut by_ref: HashMap<u64, InventoryRecord> = baseline.into_iter().collect();
    tracing::info!(
        volume = %volume_guid,
        baseline_rows = baseline_loaded,
        by_ref_after_load = by_ref.len(),
        "warm-path baseline snapshot loaded"
    );

    // Drain the delta. We pass the journal ID the snapshot stored
    // (already validated against `live.journal_id` above) — Windows
    // rejects FSCTL_READ_USN_JOURNAL with ERROR_INVALID_PARAMETER
    // when the ID field is zero, despite MSDN claiming 0 bypasses
    // verification.
    let (delta, next_usn) =
        read_usn_journal_delta(volume_guid, saved_meta.last_usn, saved_meta.journal_id).map_err(
            |e| {
                tracing::info!(
                    volume = %volume_guid,
                    error = %e,
                    "USN delta read failed; will fall back"
                );
                e
            },
        )?;

    let mut created = 0u64;
    let mut updated = 0u64;
    let mut deleted = 0u64;
    let mut missing_parent = false;
    // Track the small set of rows we'll write back to the cache at
    // the end. The previous shape rewrote the full ~2.4M-row
    // snapshot every warm scan; tracking only the records the
    // delta actually touched cuts that to typical inter-scan
    // churn (~few thousand rows) so the post-warm-write step
    // stops dominating the wallclock.
    let mut delta_upserts: Vec<(u64, InventoryRecord)> = Vec::new();
    let mut delta_deletes: Vec<u64> = Vec::new();

    for r in &delta {
        // USN_REASON_FILE_DELETE close = file gone. Mask 0x200 is
        // FILE_DELETE according to ntifs.h.
        const USN_REASON_FILE_DELETE: u32 = 0x00000200;
        let was_present = by_ref.contains_key(&r.file_ref);
        if r.reason & USN_REASON_FILE_DELETE != 0 {
            if by_ref.remove(&r.file_ref).is_some() {
                deleted += 1;
                delta_deletes.push(r.file_ref);
            }
            continue;
        }

        // Anything else is a "this record exists now" event —
        // whether it's brand new or a modify, we re-fetch its
        // metadata fresh.
        if !was_present {
            // For a new record, sanity-check we know the parent.
            // If we don't, the snapshot is missing a subtree and
            // we'd produce bad paths — bail and let cold MFT
            // rebuild from scratch.
            if !by_ref.contains_key(&r.parent_ref) && r.parent_ref != 0 {
                missing_parent = true;
                break;
            }
            created += 1;
        } else {
            updated += 1;
        }
        let is_dir = (r.attributes & 0x10) != 0;
        let (size, mtime) = if is_dir {
            (-1i64, 0u64)
        } else {
            // Need a real-disk metadata call. We don't have the
            // full path yet — reconstruct it from the parent
            // chain in `by_ref` plus the record's own name.
            let provisional_record = InventoryRecord {
                parent_ref: r.parent_ref,
                usn: r.usn,
                attributes: r.attributes,
                name: r.name.clone(),
                size: 0,
                mtime: 0,
                // Not yet fetched; warm path re-classifies later.
                reparse_tag: None,
            };
            // Temporarily insert so reconstruct_path can find it.
            by_ref.insert(r.file_ref, provisional_record);
            let path =
                match reconstruct_path(r.file_ref, &by_ref, volume_root_for(volume_guid, roots)) {
                    Some(p) => p,
                    None => {
                        by_ref.remove(&r.file_ref);
                        // If this record WAS in the snapshot,
                        // remove it on the next apply too — we
                        // can't produce a valid path for it any
                        // more.
                        if was_present {
                            delta_deletes.push(r.file_ref);
                        }
                        continue;
                    }
                };
            match std::fs::metadata(&path) {
                Ok(m) => (m.len() as i64, r.mtime_filetime as u64),
                Err(_) => {
                    // File vanished between USN event and stat —
                    // treat as deletion.
                    by_ref.remove(&r.file_ref);
                    if was_present {
                        deleted += 1;
                        if updated > 0 {
                            updated -= 1;
                        }
                        // Was in the baseline snapshot — remove
                        // from disk on the next apply.
                        delta_deletes.push(r.file_ref);
                    } else if created > 0 {
                        created -= 1;
                        // Never made it into the snapshot, so
                        // nothing to delete on disk.
                    }
                    continue;
                }
            }
        };
        let record = InventoryRecord {
            parent_ref: r.parent_ref,
            usn: r.usn,
            attributes: r.attributes,
            name: r.name.clone(),
            size,
            mtime,
            // Same as cold-path: tag fetch isn't wired up yet. Once it
            // is, set this to the real value here so the warm-path
            // snapshot persists it across scans.
            reparse_tag: None,
        };
        by_ref.insert(r.file_ref, record.clone());
        delta_upserts.push((r.file_ref, record));
    }

    if missing_parent {
        cache.lock().invalidate_inventory_snapshot(volume_guid)?;
        return Ok(WarmOutcome::Fallback {
            reason: "delta references unknown parent directory",
        });
    }

    // Build the FileEntry list the engine consumes.
    let volume_root = volume_root_for(volume_guid, roots);
    let files = entries_from_records(&by_ref, &volume_root, cfg, roots);

    // Persist ONLY the delta — not the full snapshot. The previous
    // shape did `save_inventory_snapshot` here, which DELETEd every
    // row for the volume and re-INSERTed all ~2.4M MFT records.
    // On Defender-monitored Windows that was 60-90s of pure write
    // amplification per warm scan. `apply_inventory_delta` does
    // exactly the writes the delta represents — typical inter-scan
    // churn is a few thousand rows → sub-second.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let new_meta = InventoryMeta {
        journal_id: live.journal_id,
        last_usn: next_usn,
        captured_at_unix: now,
    };
    cache
        .lock()
        .apply_inventory_delta(volume_guid, &new_meta, &delta_upserts, &delta_deletes)?;

    Ok(WarmOutcome::Applied {
        files,
        delta_records: delta.len() as u64,
        created,
        updated,
        deleted,
    })
}

#[cfg(not(windows))]
pub fn try_warm(
    _cfg: &ScanConfig,
    _volume_guid: &str,
    _roots: &[PathBuf],
    _cache: &Arc<Mutex<Cache>>,
) -> Result<WarmOutcome> {
    Ok(WarmOutcome::Fallback {
        reason: "non-Windows: USN journal unavailable",
    })
}

/// Recover a file's full path by walking parent_ref pointers up to
/// the volume root. Same algorithm as `inventory::mft::reconstruct_path`
/// — including the LENIENT "break on missing parent" semantics
/// which were the bug here before: the warm path was returning
/// None whenever the chain hit an unwalkable MFT entry (root MFT,
/// system metadata files, certain reparse points), filtering out
/// 85% of records that should have come back as files. The cold
/// path uses `match { None => break }` and produces a usable path
/// from whatever segments it managed to walk; we now do the same.
#[cfg(windows)]
fn reconstruct_path(
    file_ref: u64,
    by_ref: &HashMap<u64, InventoryRecord>,
    volume_root: PathBuf,
) -> Option<PathBuf> {
    let record = by_ref.get(&file_ref)?;
    let mut segments = vec![record.name.clone()];
    let mut cursor = record.parent_ref;
    for _ in 0..1024 {
        if cursor == 0 || cursor == file_ref {
            break;
        }
        // Lenient: missing parent ⇒ stop walking, use what we have.
        // Matches `inventory::mft::reconstruct_path` byte-for-byte.
        let parent = match by_ref.get(&cursor) {
            Some(p) => p,
            None => break,
        };
        if !parent.is_directory() {
            return None;
        }
        if parent.name.is_empty() || parent.parent_ref == cursor {
            break;
        }
        segments.push(parent.name.clone());
        cursor = parent.parent_ref;
    }
    let mut path = volume_root;
    for s in segments.iter().rev() {
        path.push(s);
    }
    Some(path)
}

/// Derive the drive-letter root (e.g. `C:\`) for path reconstruction.
/// Same shape as the cold path uses — see the explanatory comment
/// in `inventory::mft::enumerate_volume`.
#[cfg(windows)]
fn volume_root_for(volume_guid: &str, roots: &[PathBuf]) -> PathBuf {
    use std::path::{Component, Prefix};
    for r in roots {
        if let Some(Component::Prefix(p)) = r.components().next() {
            if let Prefix::Disk(letter) = p.kind() {
                return PathBuf::from(format!("{}:\\", letter as char));
            }
        }
    }
    PathBuf::from(volume_guid.trim_end_matches('\\'))
}

/// Convert the in-memory record map to `FileEntry`s, applying the
/// scan config's root + glob + size filters. Mirrors the cold MFT
/// path's filtering loop so warm and cold paths produce identical
/// outputs. Emits a single "warm-path filter pass" tracing line at
/// the end with per-bucket counts, matching the cold path's
/// "mft inventory filter pass" line — so a failure mode like
/// "everything got filtered as not-under-root" is one-line
/// diagnosable in the run log.
#[cfg(windows)]
fn entries_from_records(
    by_ref: &HashMap<u64, InventoryRecord>,
    volume_root: &Path,
    cfg: &ScanConfig,
    roots: &[PathBuf],
) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let mut filtered_dir = 0usize;
    let mut filtered_no_path = 0usize;
    let mut filtered_root = 0usize;
    let mut filtered_size = 0usize;
    let mut filtered_glob = 0usize;
    let total_records = by_ref.len();
    // Capture one example "rejected" path so the log shows the
    // shape of paths the under_any_root filter is seeing — most
    // useful when filtered_root accounts for everything and we
    // need to compare path shapes against the scan roots.
    let mut sample_rejected: Option<PathBuf> = None;
    for (file_ref, rec) in by_ref {
        if rec.is_directory() {
            filtered_dir += 1;
            continue;
        }
        let full = match reconstruct_path(*file_ref, by_ref, volume_root.to_path_buf()) {
            Some(p) => p,
            None => {
                filtered_no_path += 1;
                continue;
            }
        };
        if !under_any_root(&full, roots) {
            filtered_root += 1;
            if sample_rejected.is_none() {
                sample_rejected = Some(full);
            }
            continue;
        }
        let size = rec.size as u64;
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
            mtime: rec.mtime as i64,
            file_ref: *file_ref,
            parent_ref: rec.parent_ref,
            usn: rec.usn,
            attributes: rec.attributes,
            volume_guid: None,
            placeholder: crate::inventory::placeholder::classify(rec.attributes, rec.reparse_tag),
        });
    }
    tracing::info!(
        roots = ?roots,
        volume_root = %volume_root.display(),
        total_records,
        accepted = out.len(),
        filtered_dir,
        filtered_no_path,
        filtered_root,
        filtered_size,
        filtered_glob,
        sample_rejected = ?sample_rejected.as_ref().map(|p| p.display().to_string()),
        "warm-path filter pass",
    );
    out
}

#[cfg(windows)]
fn under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return true;
    }
    roots.iter().any(|r| path.starts_with(r))
}

#[cfg(windows)]
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
