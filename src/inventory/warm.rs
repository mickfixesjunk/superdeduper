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
    let mut by_ref: HashMap<u64, InventoryRecord> = baseline.into_iter().collect();

    // Drain the delta.
    let (delta, next_usn) =
        read_usn_journal_delta(volume_guid, saved_meta.last_usn).map_err(|e| {
            tracing::info!(
                volume = %volume_guid,
                error = %e,
                "USN delta read failed; will fall back"
            );
            e
        })?;

    let mut created = 0u64;
    let mut updated = 0u64;
    let mut deleted = 0u64;
    let mut missing_parent = false;

    for r in &delta {
        // USN_REASON_FILE_DELETE close = file gone. Mask 0x200 is
        // FILE_DELETE according to ntifs.h.
        const USN_REASON_FILE_DELETE: u32 = 0x00000200;
        let was_present = by_ref.contains_key(&r.file_ref);
        if r.reason & USN_REASON_FILE_DELETE != 0 {
            if by_ref.remove(&r.file_ref).is_some() {
                deleted += 1;
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
            };
            // Temporarily insert so reconstruct_path can find it.
            by_ref.insert(r.file_ref, provisional_record);
            let path =
                match reconstruct_path(r.file_ref, &by_ref, volume_root_for(volume_guid, roots)) {
                    Some(p) => p,
                    None => {
                        by_ref.remove(&r.file_ref);
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
                    } else if created > 0 {
                        created -= 1;
                    }
                    continue;
                }
            }
        };
        by_ref.insert(
            r.file_ref,
            InventoryRecord {
                parent_ref: r.parent_ref,
                usn: r.usn,
                attributes: r.attributes,
                name: r.name.clone(),
                size,
                mtime,
            },
        );
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

    // Persist the new snapshot.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let records: Vec<(u64, InventoryRecord)> = by_ref.into_iter().map(|(k, v)| (k, v)).collect();
    let new_meta = InventoryMeta {
        journal_id: live.journal_id,
        last_usn: next_usn,
        captured_at_unix: now,
    };
    cache
        .lock()
        .save_inventory_snapshot(volume_guid, &new_meta, &records)?;

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
/// but indexed off the `InventoryRecord` map. Returns `None` when
/// the chain is broken (malformed metadata) or hits the cycle
/// guard.
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
        let parent = by_ref.get(&cursor)?;
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
/// outputs.
#[cfg(windows)]
fn entries_from_records(
    by_ref: &HashMap<u64, InventoryRecord>,
    volume_root: &Path,
    cfg: &ScanConfig,
    roots: &[PathBuf],
) -> Vec<FileEntry> {
    let mut out = Vec::new();
    for (file_ref, rec) in by_ref {
        if rec.is_directory() {
            continue;
        }
        let full = match reconstruct_path(*file_ref, by_ref, volume_root.to_path_buf()) {
            Some(p) => p,
            None => continue,
        };
        if !under_any_root(&full, roots) {
            continue;
        }
        let size = rec.size as u64;
        if size < cfg.min_size {
            continue;
        }
        if let Some(max) = cfg.max_size {
            if size > max {
                continue;
            }
        }
        if !path_passes_globs(&full, cfg) {
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
        });
    }
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
