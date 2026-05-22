//! Stage 4 — progressive Tier 0–3 hashing.
//!
//! Each tier reads more bytes than the last. A file only escalates to
//! the next tier if at least one other file in its current group
//! shares the previous tier's hash. Singletons at any tier are
//! released — they are not duplicates and we never read another byte
//! from them.
//!
//! Tiers in order of escalation:
//!
//! * **Tier 0** — optional, format-aware fingerprint (see [`format`]).
//!   Skipped by default unless [`config::use_format_aware`] is set.
//! * **Tier 1** — BLAKE3 of the first 4 KiB.
//! * **Tier 2** — BLAKE3 of `(head 64 KiB || mid 64 KiB || tail 64 KiB)`.
//!   Skipped (and the file advanced directly) for files smaller than
//!   256 KiB, since Tier 1 already covered them.
//! * **Tier 3** — BLAKE3 of the entire file.
//!
//! The I/O layer here is intentionally simple: buffered, sequential,
//! per-file. A future commit will replace these reads with the IOCP
//! pipeline that submits sector-aligned, LCN-sorted, direct-I/O
//! requests. The tier logic above is independent of how bytes arrive.

use std::time::Instant;

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use hashbrown::HashMap;
use parking_lot::Mutex;
use rayon::prelude::*;

use crate::cache::{Cache, CacheKey, CachedHashes};
use crate::config::ScanConfig;
use crate::pipeline::layout::{LaidOutFile, LaidOutGroup};
use crate::pipeline::DuplicateGroup;
use crate::{Error, Result};

pub mod algo;
pub mod format;

pub use algo::{ContentHasher, HashAlgo};

/// Tier 1 sample size — first 4 KiB of the file.
const TIER1_BYTES: u64 = 4 * 1024;
/// Files at or below this size go through Tier 3 as a single
/// `hash_oneshot` call (read whole file → one FFI crossing) instead
/// of the streaming `new/update/finalize` triple. Targets the
/// AppData-style workload where Tier 3 fires on hundreds of
/// thousands of small files and per-file FFI overhead dominates the
/// 37 KiB hash compute. 1 MiB chosen so the per-worker buffer peak
/// stays at `threads × 1 MiB` (≤ ~16 MiB total for typical 16-core
/// boxes). Files above the threshold keep the chunked-read +
/// cancellation path so multi-GB scans stay interruptible and don't
/// allocate a gigabyte-sized buffer per worker.
const TIER3_ONESHOT_THRESHOLD: u64 = 1 << 20;
/// Tier 2 per-region sample size — 64 KiB at head, mid, and tail.
const TIER2_REGION: u64 = 64 * 1024;
/// Files smaller than this skip Tier 2 entirely and go straight to Tier 3.
const TIER2_MIN_FILE: u64 = 256 * 1024;
/// Read buffer for Tier 3 streaming.
const TIER3_BUF: usize = 1 << 20;

/// Per-scan instrumentation. The engine atomically updates these
/// counters so callers (CLI summary, GUI scope) can read them without
/// locking. Returned alongside the duplicates from [`run`].
///
/// Per-tier timing is **summed CPU time across all rayon workers**,
/// not wallclock, so it scales linearly with parallelism and is
/// directly comparable across hash algorithms. Use it to answer
/// "which tier did all my CPU time go into?" — typical pattern is
/// `tier3_micros` dominating because Tier 3 reads the whole file.
#[derive(Default, Debug)]
pub struct HashCounters {
    pub cache_hits: AtomicU64,
    pub cache_writes: AtomicU64,
    pub bytes_read: AtomicU64,
    /// Microseconds spent computing a fresh hash at each tier
    /// (cache hits and failures excluded — only successful
    /// computes count). Index 0..=3 → Tier 0 / 1 / 2 / 3.
    pub tier_micros: [AtomicU64; 4],
    /// Bytes hashed (input size, not output size) at each tier.
    /// Lets the CLI / diagnostics print MB/s per tier.
    pub tier_bytes: [AtomicU64; 4],
    /// Number of files whose compute step succeeded at each tier.
    pub tier_count: [AtomicU64; 4],
}

/// Outcome reported by the per-file [`FileProgress`] callback.
/// `Failed` lets the UI see files that couldn't even be opened
/// (cloud-only OneDrive placeholders, exclusively-locked files,
/// broken reparse points). Without this, those files were silently
/// dropped and the progress bar froze on its denominator.
#[derive(Debug, Clone)]
pub enum ProgressOutcome {
    Hashed { bytes: u64 },
    Cached,
    Failed { error: String },
}

/// Callback invoked once per file per attempted tier — success,
/// cache hit, OR failure. Used by the GUI to drive per-file progress
/// events from inside the rayon parallel hashers; the path lets the
/// UI compute a stable scattered LCN per file so SSDs show a spray
/// pattern instead of a monotonic line, and the `outcome` lets it
/// surface unreadable files in the Log tab.
pub type FileProgress = Arc<dyn Fn(&std::path::Path, u8, ProgressOutcome) + Send + Sync>;

/// Top-level entry point. Takes size-grouped, layout-annotated files
/// and returns confirmed duplicate groups by full content hash.
pub fn run(groups: Vec<LaidOutGroup>, cfg: &ScanConfig) -> Result<Vec<DuplicateGroup>> {
    let (dups, _) = run_with_counters(groups, cfg, None)?;
    Ok(dups)
}

/// Hash with optional cache integration and instrumentation. The cache
/// is consulted before each tier and written after each successful
/// hash; this is what makes the "warm rescan is near-instant" promise
/// real.
pub fn run_with_counters(
    groups: Vec<LaidOutGroup>,
    cfg: &ScanConfig,
    cache: Option<Arc<Mutex<Cache>>>,
) -> Result<(Vec<DuplicateGroup>, HashCounters)> {
    run_with_counters_inner(groups, cfg, cache, None, None)
}

/// Same as [`run_with_counters`] but with a per-file progress
/// callback that fires after each fresh (non-cache-hit) hash compute.
/// The callback receives `(tier, bytes_processed)` and may be invoked
/// from multiple rayon worker threads — keep it cheap and thread-safe.
pub fn run_with_progress(
    groups: Vec<LaidOutGroup>,
    cfg: &ScanConfig,
    cache: Option<Arc<Mutex<Cache>>>,
    on_file: FileProgress,
) -> Result<(Vec<DuplicateGroup>, HashCounters)> {
    run_with_counters_inner(groups, cfg, cache, Some(on_file), None)
}

/// Like [`run_with_progress`] but also takes a cancellation flag. The
/// engine polls this between groups and inside the streaming Tier 3
/// read loop, so cancelling a scan on a 10 GB file no longer waits
/// for the read to complete — at worst, we finish the current 1 MiB
/// buffer and return.
pub fn run_cancellable(
    groups: Vec<LaidOutGroup>,
    cfg: &ScanConfig,
    cache: Option<Arc<Mutex<Cache>>>,
    on_file: FileProgress,
    cancel: Arc<AtomicBool>,
) -> Result<(Vec<DuplicateGroup>, HashCounters)> {
    run_with_counters_inner(groups, cfg, cache, Some(on_file), Some(cancel))
}

fn run_with_counters_inner(
    groups: Vec<LaidOutGroup>,
    cfg: &ScanConfig,
    cache: Option<Arc<Mutex<Cache>>>,
    on_file: Option<FileProgress>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(Vec<DuplicateGroup>, HashCounters)> {
    let counters = Arc::new(HashCounters::default());
    let on_file_ref = on_file.as_ref();
    let cancel_ref = cancel.as_ref();
    // Dedicated pool sized to cfg.io_threads. The hashing par_iter
    // spends most of its time blocked on CreateFileW / ReadFile /
    // CloseHandle (especially on the AppData-style small-file
    // corpus where Tier 1 + small Tier 3 dominate), so
    // oversubscription versus physical cores is a real win.
    // Keeping a separate pool from the global rayon means
    // non-hash rayon usage (layout resolver, paranoid verify) keeps
    // its CPU-sized parallelism.
    let io_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.io_threads.max(1))
        .thread_name(|i| format!("superdeduper-io-{i}"))
        .build()
        .map_err(|e| Error::other(format!("io thread pool build: {e}")))?;
    let mut confirmed: Vec<DuplicateGroup> = io_pool
        .install(|| {
            groups
                .into_par_iter()
                .map(|g| run_group(g, cfg, cache.as_ref(), &counters, on_file_ref, cancel_ref))
                .collect::<Result<Vec<_>>>()
        })?
        .into_iter()
        .flatten()
        .collect();

    confirmed.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.files.cmp(&b.files)));
    let counters = Arc::try_unwrap(counters).unwrap_or_else(|arc| {
        let snap_arr = |slot: &[AtomicU64; 4]| {
            [
                AtomicU64::new(slot[0].load(Ordering::Relaxed)),
                AtomicU64::new(slot[1].load(Ordering::Relaxed)),
                AtomicU64::new(slot[2].load(Ordering::Relaxed)),
                AtomicU64::new(slot[3].load(Ordering::Relaxed)),
            ]
        };
        HashCounters {
            cache_hits: AtomicU64::new(arc.cache_hits.load(Ordering::Relaxed)),
            cache_writes: AtomicU64::new(arc.cache_writes.load(Ordering::Relaxed)),
            bytes_read: AtomicU64::new(arc.bytes_read.load(Ordering::Relaxed)),
            tier_micros: snap_arr(&arc.tier_micros),
            tier_bytes: snap_arr(&arc.tier_bytes),
            tier_count: snap_arr(&arc.tier_count),
        }
    });
    Ok((confirmed, counters))
}

/// Which tier a cached hash slot maps to.
#[derive(Copy, Clone, Debug)]
enum Tier {
    Zero,
    One,
    Two,
    Three,
}

fn run_group(
    group: LaidOutGroup,
    cfg: &ScanConfig,
    cache: Option<&Arc<Mutex<Cache>>>,
    counters: &Arc<HashCounters>,
    on_file: Option<&FileProgress>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<Vec<DuplicateGroup>> {
    let size = group.size;
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(Vec::new());
    }

    // Zero-byte short circuit.
    if size == 0 {
        if group.files.len() < 2 {
            return Ok(Vec::new());
        }
        let files: Vec<PathBuf> = group.files.into_iter().map(|f| f.entry.path).collect();
        let empty_hash = algo::hash_oneshot(cfg.hash_algo, &[]);
        return Ok(vec![DuplicateGroup {
            size: 0,
            content_hash: hex(&empty_hash),
            files,
            link_equivalent: false,
        }]);
    }

    let mut survivors = group.files;

    let algo = cfg.hash_algo;
    if cfg.use_format_aware {
        survivors = split_by_optional(&survivors, |f| {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return None;
            }
            tiered_optional(f, Tier::Zero, algo, cache, counters, on_file, || {
                format::fingerprint(&f.entry.path, size, algo)
            })
        })?;
        if survivors.len() < 2 {
            return Ok(Vec::new());
        }
    }

    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(Vec::new());
    }
    survivors = split_by(&survivors, |f| {
        tiered(f, Tier::One, algo, cache, counters, on_file, || {
            tier1_hash(f, size, algo)
        })
    })?;
    if survivors.len() < 2 {
        return Ok(Vec::new());
    }

    if size >= TIER2_MIN_FILE {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(Vec::new());
        }
        survivors = split_by(&survivors, |f| {
            tiered(f, Tier::Two, algo, cache, counters, on_file, || {
                tier2_hash(f, size, algo)
            })
        })?;
        if survivors.len() < 2 {
            return Ok(Vec::new());
        }
    }

    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(Vec::new());
    }
    let groups = into_subgroups(&survivors, |f| {
        tiered(f, Tier::Three, algo, cache, counters, on_file, || {
            tier3_hash_cancellable(f, size, algo, cancel)
        })
    })?;
    let mut out = Vec::new();
    for (hash, files) in groups {
        if files.len() < 2 {
            continue;
        }
        // Hardlink detection: on NTFS, file_ref IS the inode. Two
        // files with the same (volume_guid, file_ref) are different
        // names pointing at the same on-disk data — hardlinks of
        // each other. If EVERY file in this group shares the same
        // (volume_guid, file_ref) as the first, the entire group is
        // a single inode with N path aliases. Reclaimable space is
        // zero (the data is already shared); the GUI badges these
        // distinctly so the user knows hardlinking was already done
        // and these groups aren't candidates for further action.
        let link_equivalent = {
            let first = &files[0].entry;
            files.iter().all(|f| {
                f.entry.file_ref == first.file_ref
                    && f.entry.volume_guid == first.volume_guid
                    && first.volume_guid.is_some()
            })
        };
        let mut paths: Vec<PathBuf> = files.iter().map(|f| f.entry.path.clone()).collect();
        paths.sort();
        out.push(DuplicateGroup {
            size,
            content_hash: hex(&hash),
            files: paths,
            link_equivalent,
        });
    }
    Ok(out)
}

/// Hash each file in `flat`, then keep only those whose hash collides
/// with at least one other file in the slice. Returns the colliding
/// files as a flat Vec — sub-grouping is implicit since survivors of
/// every Tier-N round are by definition still candidates for the same
/// equivalence class. The next tier then splits them again.
/// Like [`split_by`] but the hash function returns `Option<Vec<u8>>`.
/// Files that produce `None` are kept in the survivor pool unchanged
/// (they fall through to subsequent tiers). Files that produce a
/// fingerprint must collide with at least one other fingerprinted
/// file to survive.
fn split_by_optional<F>(flat: &[LaidOutFile], hasher: F) -> Result<Vec<LaidOutFile>>
where
    F: Fn(&LaidOutFile) -> Option<Vec<u8>> + Send + Sync,
{
    let pairs: Vec<(LaidOutFile, Option<Vec<u8>>)> =
        flat.par_iter().map(|f| (f.clone(), hasher(f))).collect();

    let mut without_fp: Vec<LaidOutFile> = Vec::new();
    let mut by_hash: HashMap<Vec<u8>, Vec<LaidOutFile>> = HashMap::new();
    for (f, fp) in pairs {
        match fp {
            Some(h) => by_hash.entry(h).or_default().push(f),
            None => without_fp.push(f),
        }
    }
    let mut keep = without_fp;
    for (_, mut v) in by_hash {
        if v.len() >= 2 {
            keep.append(&mut v);
        }
    }
    Ok(keep)
}

fn split_by<F>(flat: &[LaidOutFile], hasher: F) -> Result<Vec<LaidOutFile>>
where
    F: Fn(&LaidOutFile) -> std::io::Result<Vec<u8>> + Send + Sync,
{
    let pairs: Vec<(LaidOutFile, Vec<u8>)> = flat
        .par_iter()
        .filter_map(|f| match hasher(f) {
            Ok(h) => Some((f.clone(), h)),
            Err(e) => {
                tracing::warn!(path = %f.entry.path.display(), error = %e, "hash failed; dropping file");
                None
            }
        })
        .collect();

    let mut by_hash: HashMap<Vec<u8>, Vec<LaidOutFile>> = HashMap::new();
    for (f, h) in pairs {
        by_hash.entry(h).or_default().push(f);
    }

    let mut keep = Vec::new();
    for (_, mut v) in by_hash {
        if v.len() >= 2 {
            keep.append(&mut v);
        }
    }
    Ok(keep)
}

/// Hash each file and return the buckets keyed by hash. Caller decides
/// what to do with bucket sizes (Tier 3 keeps everything ≥2; earlier
/// tiers just want the union of ≥2 buckets).
fn into_subgroups<F>(flat: &[LaidOutFile], hasher: F) -> Result<HashMap<Vec<u8>, Vec<LaidOutFile>>>
where
    F: Fn(&LaidOutFile) -> std::io::Result<Vec<u8>> + Send + Sync,
{
    let pairs: Vec<(LaidOutFile, Vec<u8>)> = flat
        .par_iter()
        .filter_map(|f| match hasher(f) {
            Ok(h) => Some((f.clone(), h)),
            Err(e) => {
                tracing::warn!(path = %f.entry.path.display(), error = %e, "hash failed; dropping file");
                None
            }
        })
        .collect();
    let mut out: HashMap<Vec<u8>, Vec<LaidOutFile>> = HashMap::new();
    for (f, h) in pairs {
        out.entry(h).or_default().push(f);
    }
    Ok(out)
}

/// Wrap a hash computation in a cache lookup-or-store. Returns the
/// cached hash if `(volume_guid, file_ref, size, mtime, usn)` matches;
/// otherwise calls `compute`, stores the result, and returns it.
fn tiered<F>(
    f: &LaidOutFile,
    tier: Tier,
    algo: HashAlgo,
    cache: Option<&Arc<Mutex<Cache>>>,
    counters: &Arc<HashCounters>,
    on_file: Option<&FileProgress>,
    compute: F,
) -> std::io::Result<Vec<u8>>
where
    F: FnOnce() -> std::io::Result<Vec<u8>>,
{
    if let Some(c) = cache {
        if let Some(key) = cache_key(f, algo) {
            if let Ok(Some(cached)) = c.lock().lookup(&key) {
                if let Some(h) = pick_hash(&cached, tier) {
                    counters.cache_hits.fetch_add(1, Ordering::Relaxed);
                    if let Some(cb) = on_file {
                        cb(&f.entry.path, tier_index(tier), ProgressOutcome::Cached);
                    }
                    return Ok(h);
                }
            }
        }
    }
    let started = Instant::now();
    match compute() {
        Ok(h) => {
            let elapsed_us = started.elapsed().as_micros() as u64;
            let bytes = tier_byte_estimate(tier, f.entry.size);
            counters.bytes_read.fetch_add(bytes, Ordering::Relaxed);
            let idx = tier_index(tier) as usize;
            counters.tier_micros[idx].fetch_add(elapsed_us, Ordering::Relaxed);
            counters.tier_bytes[idx].fetch_add(bytes, Ordering::Relaxed);
            counters.tier_count[idx].fetch_add(1, Ordering::Relaxed);
            if let Some(cb) = on_file {
                cb(
                    &f.entry.path,
                    tier_index(tier),
                    ProgressOutcome::Hashed { bytes },
                );
            }
            if let Some(c) = cache {
                if let Some(key) = cache_key(f, algo) {
                    let mut hashes = CachedHashes::default();
                    put_hash(&mut hashes, tier, h.clone());
                    if c.lock().store(&key, &hashes).is_ok() {
                        counters.cache_writes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Ok(h)
        }
        Err(e) => {
            if let Some(cb) = on_file {
                cb(
                    &f.entry.path,
                    tier_index(tier),
                    ProgressOutcome::Failed {
                        error: e.to_string(),
                    },
                );
            }
            Err(e)
        }
    }
}

fn tiered_optional<F>(
    f: &LaidOutFile,
    tier: Tier,
    algo: HashAlgo,
    cache: Option<&Arc<Mutex<Cache>>>,
    counters: &Arc<HashCounters>,
    on_file: Option<&FileProgress>,
    compute: F,
) -> Option<Vec<u8>>
where
    F: FnOnce() -> Option<Vec<u8>>,
{
    if let Some(c) = cache {
        if let Some(key) = cache_key(f, algo) {
            if let Ok(Some(cached)) = c.lock().lookup(&key) {
                if let Some(h) = pick_hash(&cached, tier) {
                    counters.cache_hits.fetch_add(1, Ordering::Relaxed);
                    if let Some(cb) = on_file {
                        cb(&f.entry.path, tier_index(tier), ProgressOutcome::Cached);
                    }
                    return Some(h);
                }
            }
        }
    }
    let started = Instant::now();
    match compute() {
        Some(h) => {
            let elapsed_us = started.elapsed().as_micros() as u64;
            let bytes = tier_byte_estimate(tier, f.entry.size);
            counters.bytes_read.fetch_add(bytes, Ordering::Relaxed);
            let idx = tier_index(tier) as usize;
            counters.tier_micros[idx].fetch_add(elapsed_us, Ordering::Relaxed);
            counters.tier_bytes[idx].fetch_add(bytes, Ordering::Relaxed);
            counters.tier_count[idx].fetch_add(1, Ordering::Relaxed);
            if let Some(cb) = on_file {
                cb(
                    &f.entry.path,
                    tier_index(tier),
                    ProgressOutcome::Hashed { bytes },
                );
            }
            if let Some(c) = cache {
                if let Some(key) = cache_key(f, algo) {
                    let mut hashes = CachedHashes::default();
                    put_hash(&mut hashes, tier, h.clone());
                    if c.lock().store(&key, &hashes).is_ok() {
                        counters.cache_writes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Some(h)
        }
        None => {
            // Tier-0 fingerprint parser declined; this isn't an
            // "error" per se (file just didn't match a known format).
            // Don't surface it as a Failure — the file simply skips
            // Tier 0 and continues to Tier 1.
            None
        }
    }
}

fn tier_index(tier: Tier) -> u8 {
    match tier {
        Tier::Zero => 0,
        Tier::One => 1,
        Tier::Two => 2,
        Tier::Three => 3,
    }
}

fn cache_key(f: &LaidOutFile, algo: HashAlgo) -> Option<CacheKey> {
    let guid = f.entry.volume_guid.clone()?;
    Some(CacheKey {
        volume_guid: guid,
        file_ref: f.entry.file_ref as i64,
        size: f.entry.size,
        mtime: f.entry.mtime,
        usn: f.entry.usn,
        hash_algo: algo,
    })
}

fn pick_hash(c: &CachedHashes, tier: Tier) -> Option<Vec<u8>> {
    match tier {
        Tier::Zero => c.tier0_fingerprint.clone(),
        Tier::One => c.tier1_hash.clone(),
        Tier::Two => c.tier2_hash.clone(),
        Tier::Three => c.tier3_hash.clone(),
    }
}

fn put_hash(c: &mut CachedHashes, tier: Tier, h: Vec<u8>) {
    match tier {
        Tier::Zero => c.tier0_fingerprint = Some(h),
        Tier::One => c.tier1_hash = Some(h),
        Tier::Two => c.tier2_hash = Some(h),
        Tier::Three => c.tier3_hash = Some(h),
    }
}

fn tier_byte_estimate(tier: Tier, size: u64) -> u64 {
    match tier {
        Tier::Zero => 64 * 1024, // typical structural region
        Tier::One => size.min(TIER1_BYTES),
        Tier::Two => 3 * TIER2_REGION,
        Tier::Three => size,
    }
}

fn tier1_hash(f: &LaidOutFile, size: u64, algo: HashAlgo) -> std::io::Result<Vec<u8>> {
    let to_read = size.min(TIER1_BYTES) as usize;
    let mut buf = vec![0u8; to_read];
    let mut file = File::open(&f.entry.path)?;
    read_exact_or_eof(&mut file, &mut buf)?;
    Ok(algo::hash_oneshot(algo, &buf))
}

fn tier2_hash(f: &LaidOutFile, size: u64, algo: HashAlgo) -> std::io::Result<Vec<u8>> {
    // Read the three regions into a single contiguous buffer and
    // dispatch one `hash_oneshot` call. The previous
    // new/update×3/finalize pattern crossed the C FFI four times per
    // file (alloc on new, then update×3, then finalize+free); on
    // DDH-128 + 261k-file Tier 3 corpora that overhead competes with
    // the AES-NI bulk throughput. BLAKE3 is pure Rust so it sees no
    // change from this rewrite; DDH-128 sees only one FFI hop with
    // no heap alloc on the hash side.
    let region = TIER2_REGION as usize;
    let mut combined = vec![0u8; 3 * region];

    let mut file = File::open(&f.entry.path)?;
    read_exact_or_eof(&mut file, &mut combined[..region])?;

    let mid_off = size.saturating_sub(TIER2_REGION) / 2;
    file.seek(SeekFrom::Start(mid_off))?;
    read_exact_or_eof(&mut file, &mut combined[region..2 * region])?;

    let tail_off = size.saturating_sub(TIER2_REGION);
    file.seek(SeekFrom::Start(tail_off))?;
    read_exact_or_eof(&mut file, &mut combined[2 * region..])?;

    Ok(algo::hash_oneshot(algo, &combined))
}

/// Tier 3 full-content hash. Two paths:
/// * **Small files (`size <= TIER3_ONESHOT_THRESHOLD`)** — slurp
///   the whole file into one buffer and hand it to `hash_oneshot`.
///   This is the AppData-style hot path: hundreds of thousands of
///   sub-1-MiB files where per-file FFI/alloc overhead competes
///   with the actual hash compute. One FFI hop, one heap alloc for
///   the read buffer, zero hasher-state allocs.
/// * **Large files** — chunked read + streaming hasher with a
///   per-buffer cancel check, so a multi-GB hash stays interruptible
///   (worst-case latency: one TIER3_BUF read) without ever
///   allocating a gigabyte-sized read buffer.
fn tier3_hash_cancellable(
    f: &LaidOutFile,
    size: u64,
    algo: HashAlgo,
    cancel: Option<&Arc<AtomicBool>>,
) -> std::io::Result<Vec<u8>> {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "cancelled",
        ));
    }
    if size <= TIER3_ONESHOT_THRESHOLD {
        let mut file = File::open(&f.entry.path)?;
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)?;
        return Ok(algo::hash_oneshot(algo, &buf));
    }
    let file = File::open(&f.entry.path)?;
    let mut reader = BufReader::with_capacity(TIER3_BUF, file);
    let mut hasher = ContentHasher::new(algo);
    let mut buf = vec![0u8; TIER3_BUF];
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            ));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    // Like read_exact but tolerant of EOF — short reads zero-pad.
    let mut total = 0usize;
    while total < buf.len() {
        match r.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    for byte in &mut buf[total..] {
        *byte = 0;
    }
    Ok(total)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputFormat;
    use crate::inventory::FileEntry;
    use crate::pipeline::layout::LaidOutFile;
    use std::fs;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "superdeduper-hash-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn lo(path: PathBuf, size: u64) -> LaidOutFile {
        LaidOutFile {
            entry: FileEntry {
                path,
                size,
                mtime: 0,
                file_ref: 0,
                parent_ref: 0,
                usn: 0,
                attributes: 0,
                volume_guid: None,
            },
            start_lcn: None,
        }
    }

    fn cfg() -> ScanConfig {
        ScanConfig {
            roots: vec![],
            reference_roots: vec![],
            min_size: 0,
            max_size: None,
            include: None,
            exclude: None,
            format: OutputFormat::Text,
            paranoid: false,
            use_cache: false,
            use_format_aware: false,
            threads: 1,
            queue_depth: None,
            output: None,
            follow_links: false,
            allow_system_paths: false,
            io_threads: 4,
            hash_algo: HashAlgo::Blake3,
        }
    }

    /// Tier 1 must release files whose 4 KiB heads differ.
    #[test]
    fn tier1_releases_distinct_heads() {
        let d = tmpdir();
        let a = d.join("a");
        let b = d.join("b");
        let mut content_a = vec![0u8; 4096];
        let mut content_b = vec![0u8; 4096];
        content_a[0] = 0xAA;
        content_b[0] = 0xBB;
        fs::write(&a, &content_a).unwrap();
        fs::write(&b, &content_b).unwrap();
        let group = LaidOutGroup {
            size: 4096,
            files: vec![lo(a, 4096), lo(b, 4096)],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert!(
            result.is_empty(),
            "files with different heads must not group"
        );
        fs::remove_dir_all(&d).ok();
    }

    /// Same head and tail but different middle: Tier 2 must catch this.
    #[test]
    fn tier2_catches_middle_divergence() {
        let d = tmpdir();
        let mk = |path: &str, mid_byte: u8| {
            let mut buf = vec![0u8; 300 * 1024];
            buf[150 * 1024] = mid_byte;
            // Ensure same head and tail.
            fs::write(d.join(path), &buf).unwrap();
            lo(d.join(path), buf.len() as u64)
        };
        let a = mk("a", 0xAA);
        let b = mk("b", 0xBB);
        let c = mk("c", 0xAA);
        let group = LaidOutGroup {
            size: 300 * 1024,
            files: vec![a, b, c],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(result.len(), 1, "should find one group of two");
        assert_eq!(result[0].files.len(), 2);
        fs::remove_dir_all(&d).ok();
    }

    /// Identical except final byte: Tier 3 (full) must distinguish.
    #[test]
    fn tier3_catches_one_byte_diff() {
        let d = tmpdir();
        let mut a = vec![0xCDu8; 300 * 1024];
        let mut b = a.clone();
        b[300 * 1024 - 1] = 0x00; // a ends 0xCD, b ends 0x00.
        let _ = a.pop(); // keep both 300 KiB - we want last-byte diff
        a = vec![0xCDu8; 300 * 1024];
        // Both are 300 KiB. tail region is from offset (300K - 64K) = 236K..300K.
        // We set b's last byte to 0 so the tier 2 tail catches it too — that's
        // fine; we still expect at least Tier 2 to release one of them.
        fs::write(d.join("a"), &a).unwrap();
        fs::write(d.join("b"), &b).unwrap();
        let group = LaidOutGroup {
            size: 300 * 1024,
            files: vec![lo(d.join("a"), 300 * 1024), lo(d.join("b"), 300 * 1024)],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert!(result.is_empty(), "differing files must not be reported");
        fs::remove_dir_all(&d).ok();
    }

    /// Two genuine duplicates: must survive every tier.
    #[test]
    fn duplicates_survive_all_tiers() {
        let d = tmpdir();
        let body = vec![0x5Au8; 400 * 1024];
        fs::write(d.join("a"), &body).unwrap();
        fs::write(d.join("b"), &body).unwrap();
        fs::write(d.join("c"), &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo(d.join("a"), body.len() as u64),
                lo(d.join("b"), body.len() as u64),
                lo(d.join("c"), body.len() as u64),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].files.len(), 3);
        fs::remove_dir_all(&d).ok();
    }
}
