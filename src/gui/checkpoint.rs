//! Pause / resume checkpoint that survives an app restart.
//!
//! When the user pauses (or the app exits mid-scan), the engine
//! serialises a `Checkpoint` to `%LOCALAPPDATA%\superdeduper\scan-checkpoint.json`
//! (and the XDG equivalent on non-Windows). On next launch, the GUI
//! offers to resume.
//!
//! The checkpoint is intentionally coarse: it tracks WHICH roots and
//! settings were in play, the set of size-group ranges already
//! confirmed, and the duplicates we'd already reported. Resume
//! re-enumerates from scratch (the inventory phase is cheap relative
//! to hashing) and skips size groups whose checksum already matches
//! the checkpoint's "already-done" list.
//!
//! Anything more granular (per-file resume mid-tier) would require
//! threading state through the rayon-parallel hash, which adds
//! complexity without a clear win — re-running an interrupted hash
//! tier is fast because the cache already has every completed file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};

use crate::gui::events::DuplicateGroupSummary;
use crate::gui::state::{RootEntry, ScanSettings};
use crate::{Error, Result};

/// Lightweight file entry persisted in the checkpoint so resume can
/// skip the inventory walk. Carries just enough to feed Stage 2
/// (size grouping) — paths, size, mtime, and the Windows-only
/// volume_guid / file_ref / usn fields that drive cache invalidation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedFileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub file_ref: u64,
    pub parent_ref: u64,
    pub usn: i64,
    pub attributes: u32,
    pub volume_guid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema: String,
    pub created_at_unix: u64,
    pub roots: Vec<RootEntry>,
    pub settings: ScanSettings,
    /// Hex BLAKE3 prefixes of groups already reported. Used so a
    /// resumed scan doesn't double-report the same duplicates if it
    /// re-encounters them.
    pub completed_hashes: Vec<String>,
    /// Snapshot of duplicates the GUI already had so it can be
    /// restored on resume.
    pub previous_duplicates: Vec<DuplicateGroupSummary>,
    /// Full file list from the inventory walk, persisted so resume
    /// can skip Stage 1 entirely. `None` ⇒ inventory hadn't finished
    /// when the pause fired, and a fresh walk is required.
    #[serde(default)]
    pub saved_inventory: Option<Vec<SavedFileEntry>>,
    /// #108 — Bytes hashed cumulatively across all resume sessions
    /// for this scan. Updated at every pause save site. On resume,
    /// the engine seeds its local `total_bytes_read` from this so the
    /// submission payload's `bytes_scanned` reflects total work
    /// across pauses, not just post-resume work. Old (v0.2.10)
    /// checkpoints deserialise this as 0 via `#[serde(default)]` —
    /// behaviour matches pre-#108 (under-reports cumulative work in
    /// the leaderboard payload) until the user pauses again.
    #[serde(default)]
    pub cumulative_bytes_scanned: u64,
    /// #108-extended — Files counted cumulatively across resume
    /// sessions. Same shape + same rationale as
    /// `cumulative_bytes_scanned`: payload's `files_scanned` must
    /// be chain-cumulative so the backend's
    /// `files_scanned / wall_clock_seconds ≤ disk_class_iops_ceiling`
    /// sanity check doesn't trip when bytes is cumulative but files
    /// is per-spawn. `#[serde(default)]` keeps old checkpoints
    /// loadable.
    #[serde(default)]
    pub cumulative_files_scanned: u64,
    /// #108-extended — Wall-clock seconds spent scanning,
    /// cumulatively across resume sessions. Saved at every pause
    /// point. On resume, the new spawn's `scan_started_at` is
    /// shifted backward by this value so `Instant::now() -
    /// shifted_start` produces the cumulative wall-clock at any
    /// point. Backend's two throughput sanity checks
    /// (`bytes_scanned / wall_clock`, `files_scanned / wall_clock`)
    /// both depend on this being cumulative together with the byte
    /// + file counters.
    #[serde(default)]
    pub cumulative_wall_clock_seconds: u64,
}

impl Checkpoint {
    pub fn new(roots: Vec<RootEntry>, settings: ScanSettings) -> Self {
        Self {
            schema: "superdeduper.checkpoint.v1".into(),
            created_at_unix: crate::time::now_unix_secs(),
            roots,
            settings,
            completed_hashes: Vec::new(),
            previous_duplicates: Vec::new(),
            saved_inventory: None,
            cumulative_bytes_scanned: 0,
            cumulative_files_scanned: 0,
            cumulative_wall_clock_seconds: 0,
        }
    }

    pub fn record(&mut self, group: &DuplicateGroupSummary) {
        self.completed_hashes.push(group.content_hash.clone());
        self.previous_duplicates.push(group.clone());
    }
}

/// Default checkpoint file location. Same root as the cache.
pub fn default_checkpoint_path() -> Result<PathBuf> {
    let mut p = crate::cache::default_cache_path()?;
    p.set_file_name("scan-checkpoint.json");
    Ok(p)
}

pub fn load(path: &Path) -> Result<Option<Checkpoint>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let cp: Checkpoint = serde_json::from_slice(&bytes)?;
    if !cp.schema.starts_with("superdeduper.checkpoint") {
        return Ok(None);
    }
    Ok(Some(cp))
}

pub fn save(path: &Path, cp: &Checkpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(cp)?;
    // Atomic-ish write: write to .tmp then rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Tiny non-destructive read used by the launch-time Resume/Start
/// Fresh modal. Returns enough to render the prompt (timestamp,
/// roots, dup count) without committing to threading the whole
/// state through the engine. `Ok(None)` = no file present.
/// `Err(_)` = file exists but is corrupt; caller is expected to
/// rename it via [`mark_corrupt`].
#[derive(Debug, Clone)]
pub struct CheckpointSummary {
    pub created_at_unix: u64,
    pub roots: Vec<RootEntry>,
    pub duplicate_count: usize,
    pub has_saved_inventory: bool,
    /// #99 PR2 — Carried so `classify_resume_tier` can run at GUI
    /// startup against the launch-time modal data without reloading
    /// the full Checkpoint. Cheap (~hundreds of bytes); same disk
    /// load already happens to compute the other summary fields.
    pub settings: ScanSettings,
}

pub fn summary(path: &Path) -> Result<Option<CheckpointSummary>> {
    match load(path)? {
        Some(cp) => Ok(Some(CheckpointSummary {
            created_at_unix: cp.created_at_unix,
            roots: cp.roots,
            duplicate_count: cp.previous_duplicates.len(),
            has_saved_inventory: cp.saved_inventory.is_some(),
            settings: cp.settings,
        })),
        None => Ok(None),
    }
}

/// Rename the existing checkpoint to a timestamped `.bak` sibling so
/// the user can recover from a Start-Fresh later. Filename pattern:
/// `<stem>-<ISO-timestamp>.json.bak`. Idempotent — if the source
/// file doesn't exist, returns `Ok(None)`.
pub fn archive(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = iso_timestamp_now();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("checkpoint");
    let archived = parent.join(format!("{stem}-{stamp}.json.bak"));
    std::fs::rename(path, &archived)?;
    Ok(Some(archived))
}

/// Rename a corrupt checkpoint so the user can inspect it later.
/// Pattern: `<stem>-<ISO-timestamp>.json.corrupt`. Distinct
/// extension from `.bak` so a corrupt file isn't confused with a
/// safely-archived one the user might want to restore.
pub fn mark_corrupt(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = iso_timestamp_now();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("checkpoint");
    let archived = parent.join(format!("{stem}-{stamp}.json.corrupt"));
    std::fs::rename(path, &archived)?;
    Ok(Some(archived))
}

fn iso_timestamp_now() -> String {
    // Filename-safe ISO-8601 in UTC: 2026-05-20T13-45-22Z.
    // (Hyphens between time components — colons aren't valid in
    // Windows filenames.)
    let (y, mo, d, h, mi, s) = crate::time::unix_to_ymdhms(crate::time::now_unix_i64());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z")
}

pub fn delete(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// v0.3.41 Phase 4 -- background-thread checkpoint saver. Moves the
/// ~255ms-per-chunk checkpoint::save off the chunk-loop critical path
/// (it was 99.974% of chunk_emit_ms_total per the testdesign 22:55 PDT
/// matrix verdict; 21.9s of 22.4s).
///
/// Pattern: single replace-on-enqueue slot. The chunk loop calls
/// [`SaveWorker::enqueue`] with a fresh `Checkpoint` snapshot; the bg
/// thread waits on a Condvar, picks up the latest snapshot, drops the
/// lock, and runs `checkpoint::save`. If the chunk loop enqueues again
/// while the bg thread is mid-write, the new snapshot replaces any
/// still-pending older one (single-slot semantics) -- we only care
/// about the latest crash-recovery state, never about save-every-chunk
/// durability.
///
/// Shutdown: `shutdown()` (or `Drop`) signals stop + notifies the
/// Condvar + joins. The bg thread drains any post-stop pending snapshot
/// before returning so the very-latest enqueue isn't lost.
///
/// Race notes:
/// * The chunk loop NEVER blocks on disk I/O. The mutex is held for
///   `take()` / `replace()` only (microseconds).
/// * Cancellation paths in `live::run` shutdown the worker BEFORE the
///   sync cumulative-aware save, so the bg thread doesn't race the
///   sync save on the same path. Both use tempfile + atomic rename, so
///   even if they did race the file would never be torn -- but
///   serialising via shutdown is the right invariant.
/// * Errors from `checkpoint::save` are SWALLOWED inside the bg thread
///   (logging on failure would require plumbing a tx; checkpoint save
///   failure on the periodic per-chunk save is non-fatal -- the
///   cancellation-path sync save will surface it via the existing log
///   if it still fails at pause time).
pub struct SaveWorker {
    inner: Arc<SaveWorkerInner>,
    handle: Option<JoinHandle<()>>,
}

struct SaveWorkerInner {
    path: PathBuf,
    pending: Mutex<Option<Checkpoint>>,
    notify: Condvar,
    stop: AtomicBool,
}

impl SaveWorker {
    /// Spawn the bg thread for the given checkpoint path. Returns
    /// immediately; the bg thread waits on its first enqueue.
    pub fn spawn(path: PathBuf) -> Self {
        let inner = Arc::new(SaveWorkerInner {
            path,
            pending: Mutex::new(None),
            notify: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let inner_for_thread = Arc::clone(&inner);
        let handle = std::thread::Builder::new()
            .name("sdd-checkpoint-saver".into())
            .spawn(move || run_save_loop(inner_for_thread))
            .expect("checkpoint saver thread spawn");
        Self {
            inner,
            handle: Some(handle),
        }
    }

    /// Replace the pending checkpoint snapshot with `cp`. Returns
    /// immediately; does NOT block on disk I/O.
    pub fn enqueue(&self, cp: Checkpoint) {
        {
            let mut guard = self.inner.pending.lock();
            *guard = Some(cp);
        }
        self.inner.notify.notify_one();
    }

    /// Signal stop + join. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            self.inner.stop.store(true, Ordering::Release);
            self.inner.notify.notify_one();
            // Join is best-effort; a bg-thread panic shouldn't break
            // the caller (the sync-save path is the durability gate).
            let _ = h.join();
        }
    }
}

impl Drop for SaveWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_save_loop(inner: Arc<SaveWorkerInner>) {
    loop {
        let pending = {
            let mut guard = inner.pending.lock();
            while guard.is_none() && !inner.stop.load(Ordering::Acquire) {
                inner.notify.wait(&mut guard);
            }
            guard.take()
        };
        if let Some(cp) = pending {
            let _ = save(&inner.path, &cp);
        }
        if inner.stop.load(Ordering::Acquire) {
            // Drain the LAST pending: an enqueue may have raced with
            // the stop signal between our take() above and the notify
            // arriving. Saving it here preserves the
            // "latest-snapshot-survives" guarantee even at shutdown.
            let last = inner.pending.lock().take();
            if let Some(cp) = last {
                let _ = save(&inner.path, &cp);
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn roundtrip_preserves_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        let mut cp = Checkpoint::new(
            vec![RootEntry {
                path: PathBuf::from("/tmp/x"),
                is_reference: true,
            }],
            ScanSettings::default(),
        );
        cp.record(&DuplicateGroupSummary {
            size: 4096,
            content_hash: "deadbeef".into(),
            files: vec![PathBuf::from("/tmp/x/a"), PathBuf::from("/tmp/x/b")],
            link_equivalent: false,
            unique_inodes: 2,
            similarity_kind: crate::pipeline::SimilarityKind::ByteIdentical,
        });
        save(&path, &cp).unwrap();

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.roots.len(), 1);
        assert!(loaded.roots[0].is_reference);
        assert_eq!(loaded.completed_hashes, vec!["deadbeef".to_string()]);
        assert_eq!(loaded.previous_duplicates.len(), 1);
        assert_eq!(loaded.schema, "superdeduper.checkpoint.v1");
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never.json");
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn delete_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        delete(&path).unwrap(); // already absent
        save(&path, &Checkpoint::new(vec![], ScanSettings::default())).unwrap();
        delete(&path).unwrap();
        assert!(!path.exists());
    }

    fn cp_with_hash(hash: &str) -> Checkpoint {
        let mut cp = Checkpoint::new(vec![], ScanSettings::default());
        cp.record(&DuplicateGroupSummary {
            size: 0,
            content_hash: hash.into(),
            files: vec![],
            link_equivalent: false,
            unique_inodes: 0,
            similarity_kind: crate::pipeline::SimilarityKind::ByteIdentical,
        });
        cp
    }

    #[test]
    fn save_worker_persists_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        let worker = SaveWorker::spawn(path.clone());
        worker.enqueue(cp_with_hash("aa"));
        drop(worker); // shutdown via Drop drains the pending snapshot.
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.completed_hashes, vec!["aa".to_string()]);
    }

    #[test]
    fn save_worker_last_enqueue_wins_after_shutdown() {
        // The bg worker may be mid-save when shutdown lands; the drain
        // step in run_save_loop guarantees the final enqueue still
        // ends up on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        let mut worker = SaveWorker::spawn(path.clone());
        // Rapid-fire several enqueues; bg thread may collapse some.
        for h in ["aa", "bb", "cc", "dd"] {
            worker.enqueue(cp_with_hash(h));
        }
        worker.enqueue(cp_with_hash("final"));
        worker.shutdown();
        let loaded = load(&path).unwrap().unwrap();
        // Exactly which intermediate snapshots got saved is racy
        // (single-slot replace can drop earlier ones), but the FINAL
        // enqueue must always survive due to the drain step.
        assert_eq!(loaded.completed_hashes, vec!["final".to_string()]);
    }

    #[test]
    fn save_worker_shutdown_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        let mut worker = SaveWorker::spawn(path);
        worker.shutdown();
        worker.shutdown(); // no panic; handle already taken.
    }

    #[test]
    fn save_worker_no_enqueue_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        let worker = SaveWorker::spawn(path.clone());
        drop(worker); // shutdown without any enqueue: nothing written.
        assert!(!path.exists());
    }
}
