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

/// Default Tier 1 sample size — first 4 KiB of the file. Runtime
/// overridable via `ScanConfig::tier1_bytes` (CLI `--tier1-bytes`).
/// Kept as a fallback for callers that don't have a `ScanConfig`
/// in scope (cache key calculations, the standalone hash_repro
/// benchmark binary).
pub const TIER1_BYTES: u64 = 4 * 1024;
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
    /// T2.1 phase 7 — tier guard skip counters. Surfaces "N files
    /// skipped because placeholders" so the user understands why the
    /// dup-group count is lower than expected. Split by state so the
    /// scan-finish log line can give a per-class breakdown without
    /// changing wire format.
    pub placeholders_blocked_recall: AtomicU64,
    pub placeholders_blocked_other_reparse: AtomicU64,
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
            placeholders_blocked_recall: AtomicU64::new(
                arc.placeholders_blocked_recall.load(Ordering::Relaxed),
            ),
            placeholders_blocked_other_reparse: AtomicU64::new(
                arc.placeholders_blocked_other_reparse
                    .load(Ordering::Relaxed),
            ),
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

/// Partition a candidate group into files we'll hash and files we'll
/// refuse to read. Anything whose
/// `placeholder.blocks_content_read_under_policy(allow_recall_on_read)`
/// returns true is dropped from the survivors list and reported as a
/// failed progress event with a placeholder-specific error string.
///
/// Behaviour for callers:
/// * The returned `LaidOutGroup` carries the same `size`, with `files`
///   restricted to entries that may be hashed.
/// * If every file was blocked, returns an empty `files` vec. The caller's
///   `survivors.len() < 2` guard handles that uniformly.
/// * `on_file`, when supplied, sees a `ProgressOutcome::Failed { error }`
///   for each blocked file with the error string `"placeholder blocks
///   content read: <state>"` so the GUI Log tab can surface them with
///   the right reason.
/// * `allow_recall_on_read` plumbs `--allow-recall-on-read` from
///   CLI/Config so users can opt into reading cloud-recall placeholders.
///   Doesn't change behaviour for `OtherReparse` (still blocked under
///   any policy) or `ReparseDedup` (always allowed).
///
/// Cost: O(N) over the group; no I/O. Safe to call unconditionally on
/// every group; on non-Windows or for groups with no placeholders, every
/// file passes through and no events fire.
fn apply_tier_guards(
    group: LaidOutGroup,
    on_file: Option<&FileProgress>,
    allow_recall_on_read: bool,
    counters: &HashCounters,
) -> LaidOutGroup {
    use crate::inventory::PlaceholderState;
    let size = group.size;
    let mut allowed = Vec::with_capacity(group.files.len());
    let mut blocked = 0usize;
    for f in group.files {
        if f.entry
            .placeholder
            .blocks_content_read_under_policy(allow_recall_on_read)
        {
            tracing::warn!(
                path = %f.entry.path.display(),
                placeholder = %f.entry.placeholder,
                "tier guard: refusing content read for placeholder file",
            );
            // Phase 7: bump the per-state counter the GUI / CLI
            // surface at scan finish. Split by class so we can
            // give the user a per-bucket breakdown without
            // expanding the wire schema.
            match f.entry.placeholder {
                PlaceholderState::RecallOnOpen | PlaceholderState::RecallOnDataAccess => {
                    counters
                        .placeholders_blocked_recall
                        .fetch_add(1, Ordering::Relaxed);
                }
                PlaceholderState::OtherReparse(_) => {
                    counters
                        .placeholders_blocked_other_reparse
                        .fetch_add(1, Ordering::Relaxed);
                }
                // NotPlaceholder + ReparseDedup never reach this
                // branch (they don't trigger blocks_content_read).
                _ => {}
            }
            if let Some(cb) = on_file {
                cb(
                    &f.entry.path,
                    0,
                    ProgressOutcome::Failed {
                        error: format!(
                            "placeholder blocks content read: {:?}",
                            f.entry.placeholder
                        ),
                    },
                );
            }
            blocked += 1;
        } else {
            allowed.push(f);
        }
    }
    if blocked > 0 {
        tracing::info!(
            size,
            blocked,
            remaining = allowed.len(),
            "tier guard blocked placeholders before hashing",
        );
    }
    LaidOutGroup {
        size,
        files: allowed,
    }
}

/// T0.5 — partition a same-size group by (volume_guid, file_ref) into:
/// * `link_equiv`: one entry per inode with ≥2 hardlink aliases. Carries
///   the representative LaidOutFile + the full sorted alias path list.
///   These are already-confirmed dup groups via shared inode; we only
///   need ONE tier-3 hash per inode to produce a content_hash for the
///   JSON output. The original architecture hashed every path including
///   aliases — that's the redundant work T0.5 eliminates.
/// * `single_alias`: every file whose inode has exactly ONE alias under
///   this scan (or whose inode info is unresolved). These flow through
///   the normal tier pipeline as one-per-inode representatives.
///
/// Files with `volume_guid: None` or `file_ref == 0` (walker's
/// unresolved-inode shape) are treated as distinct single-alias inodes
/// via a synthetic per-file key — we don't collapse files we couldn't
/// identify by inode.
///
/// Tradeoff worth documenting: scenario 2 from the design notes — N
/// paths split across M inodes (M ≥ 2) where all paths happen to share
/// the same content — gets emitted as M separate link-equivalent dup
/// groups, not one cross-inode merged dup group. Pre-T0.5 behaviour
/// would have merged them. Rare in practice on hardlink-heavy corpora;
/// the `dual_reclaimable` metric still reports the correct inode-aware
/// reclaimable bytes either way.
fn partition_by_inode(
    files: Vec<LaidOutFile>,
) -> (Vec<(LaidOutFile, Vec<PathBuf>)>, Vec<LaidOutFile>) {
    use hashbrown::HashMap as HashbrownMap;
    // Inode key: (volume_guid, file_ref). For files with unresolved
    // inode info, we synthesise a unique key so they never collide.
    type InodeKey = (Option<String>, u64);
    let mut by_inode: HashbrownMap<InodeKey, Vec<LaidOutFile>> = HashbrownMap::new();
    let mut next_synthetic: u64 = 1;
    for f in files {
        let key: InodeKey =
            if f.entry.volume_guid.is_some() && f.entry.file_ref != 0 {
                (f.entry.volume_guid.clone(), f.entry.file_ref)
            } else {
                // Unique synthetic key — won't collide with anything else.
                let k = (None, next_synthetic);
                next_synthetic = next_synthetic.saturating_add(1);
                k
            };
        by_inode.entry(key).or_default().push(f);
    }
    let mut link_equiv: Vec<(LaidOutFile, Vec<PathBuf>)> = Vec::new();
    let mut single_alias: Vec<LaidOutFile> = Vec::new();
    for (_, mut bucket) in by_inode {
        if bucket.len() == 1 {
            single_alias.push(bucket.pop().unwrap());
        } else {
            // Pick first as rep, collect all paths (including rep's) as aliases.
            let rep = bucket.remove(0);
            let mut paths: Vec<PathBuf> = bucket.into_iter().map(|f| f.entry.path).collect();
            paths.push(rep.entry.path.clone());
            link_equiv.push((rep, paths));
        }
    }
    (link_equiv, single_alias)
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

    // T2.1 phase 4: tier guards. Refuse to open files whose placeholder
    // state would force a cloud hydration (RecallOnOpen / RecallOnDataAccess
    // / unknown reparse). Classify ran at inventory time (phase 2), but
    // this is defense in depth — if anything slipped through (race between
    // enum and hash, attributes changed since enumeration, walker fallback
    // didn't run classify for some reason), the hash worker still refuses
    // before any ReadFile. NotPlaceholder + ReparseDedup pass through —
    // their data is local and reading is safe.
    // Phase 6: cfg.allow_recall_on_read lets users opt into accepting
    // forced hydration if that's what they actually want.
    // Phase 7: per-state skip counters land in `counters` for the
    // scan-finish breakdown line.
    let group = apply_tier_guards(group, on_file, cfg.allow_recall_on_read, counters);

    // Zero-byte short circuit.
    if size == 0 {
        if group.files.len() < 2 {
            return Ok(Vec::new());
        }
        let files: Vec<PathBuf> = group.files.into_iter().map(|f| f.entry.path).collect();
        let unique_inodes = files.len() as u64;
        let empty_hash = algo::hash_oneshot(cfg.hash_algo, &[]);
        return Ok(vec![DuplicateGroup {
            size: 0,
            content_hash: hex(&empty_hash),
            files,
            link_equivalent: false,
            // 0-byte files all hash to the same value but each is its
            // own inode (we don't dedupe by inode at inventory time);
            // treating the count as files.len() gives the "every file
            // is one inode" interpretation that's correct here.
            unique_inodes,
        }]);
    }

    // T0.5: dedupe by inode BEFORE the tier pipeline.
    //
    // Pre-T0.5, every path (including hardlink aliases sharing a single
    // inode) went through tier 1/2/3 hashing — N redundant hashes per
    // N-alias inode. On hardlink-heavy corpora (C:\Windows System32 ↔
    // WinSxS) this was the dominant work item; tier 1 in particular
    // showed 62k files-per-thread when ~16k distinct inodes were the
    // real workload.
    //
    // Partition splits the surviving (post-tier-guard) files into:
    //   * link_equiv: inodes with ≥2 hardlink aliases. Already confirmed
    //     dup groups via shared inode; we just hash ONE rep via tier 3 to
    //     produce a content_hash for the JSON, then emit the full alias
    //     list as a link_equivalent dup group.
    //   * single_alias: inodes with exactly one path each (or unresolved
    //     inode info). These flow through the normal tier pipeline below
    //     as one rep per inode. Tier-3-confirmed groups expand naturally
    //     since each surviving rep has exactly one alias = its own path.
    let algo = cfg.hash_algo;
    let (link_equiv, single_alias) = partition_by_inode(group.files);
    let mut out: Vec<DuplicateGroup> = Vec::new();

    // Stream A — link-equivalent inodes. One tier-3 hash per inode for
    // the content_hash; emit directly. Tier-1/2 are unnecessary (the
    // inode equivalence already confirms identity within each group).
    for (rep, aliases) in &link_equiv {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(out);
        }
        let hash_bytes = match tier3_hash_cancellable(rep, size, algo, cancel) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    path = %rep.entry.path.display(),
                    error = %e,
                    "T0.5 link-equiv: tier3 hash failed for inode rep; skipping group"
                );
                if let Some(cb) = on_file {
                    cb(
                        &rep.entry.path,
                        3,
                        ProgressOutcome::Failed { error: e.to_string() },
                    );
                }
                continue;
            }
        };
        // Bump diagnostic counters honestly — we DID hash this rep's
        // bytes, just once per inode instead of per alias.
        counters.bytes_read.fetch_add(size, Ordering::Relaxed);
        counters.tier_bytes[3].fetch_add(size, Ordering::Relaxed);
        counters.tier_count[3].fetch_add(1, Ordering::Relaxed);
        let mut paths = aliases.clone();
        paths.sort();
        out.push(DuplicateGroup {
            size,
            content_hash: hex(&hash_bytes),
            files: paths,
            link_equivalent: true,
            unique_inodes: 1,
        });
        if let Some(cb) = on_file {
            cb(
                &rep.entry.path,
                3,
                ProgressOutcome::Hashed { bytes: size },
            );
        }
    }

    let mut survivors = single_alias;
    // Stream B — single-alias inodes through the normal tier pipeline.
    // Bail with whatever Stream A produced if Stream B has nothing to do.
    if survivors.len() < 2 {
        return Ok(out);
    }

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
            return Ok(out);
        }
    }

    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(out);
    }
    survivors = split_by(&survivors, |f| {
        tiered(f, Tier::One, algo, cache, counters, on_file, || {
            tier1_hash(f, size, algo, cfg.tier1_bytes)
        })
    })?;
    if survivors.len() < 2 {
        return Ok(out);
    }

    if size >= TIER2_MIN_FILE {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(out);
        }
        survivors = split_by(&survivors, |f| {
            tiered(f, Tier::Two, algo, cache, counters, on_file, || {
                tier2_hash(f, size, algo)
            })
        })?;
        if survivors.len() < 2 {
            return Ok(out);
        }
    }

    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(out);
    }
    let groups = into_subgroups(&survivors, |f| {
        tiered(f, Tier::Three, algo, cache, counters, on_file, || {
            tier3_hash_cancellable(f, size, algo, cancel)
        })
    })?;
    for (hash, files) in groups {
        if files.len() < 2 {
            continue;
        }
        // T0.5: Stream B survivors are all single-alias inodes by
        // construction (multi-alias inodes were handled in Stream A
        // above). So every file in this group has a distinct inode,
        // `link_equivalent` is always false, and `unique_inodes`
        // equals files.len().
        let link_equivalent = false;
        let unique_inodes = files.len() as u64;
        let mut paths: Vec<PathBuf> = files.iter().map(|f| f.entry.path.clone()).collect();
        paths.sort();
        out.push(DuplicateGroup {
            size,
            content_hash: hex(&hash),
            files: paths,
            link_equivalent,
            unique_inodes,
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
    // The cache schema's PRIMARY KEY is (volume_guid, file_ref,
    // hash_algo). When the walker's fallback path leaves file_ref
    // at zero (and Stage 2b's `resolve_file_ids` couldn't resolve
    // it — permission-denied parent dir, file since deleted), or
    // when volume_guid is None (Linux, network share, missing
    // Win32 API), every such file collides on the same key. The
    // `ON CONFLICT UPDATE` clause then clobbers earlier writes and
    // the secondary (size, mtime, usn) check fails on lookup for
    // nearly every file. Net effect: cache appears empty even
    // after a full scan.
    //
    // Fix: synthesize stable fillers from the path when the
    // walker-supplied identifiers are missing. Path is stable
    // across runs, so resume cache lookups still hit. Real NTFS
    // file_refs fit in the low 48 bits, so overlap with synthetic
    // 63-bit refs is astronomically unlikely.
    let guid = f
        .entry
        .volume_guid
        .clone()
        .unwrap_or_else(|| "_unknown_volume".to_string());
    let file_ref = if f.entry.file_ref != 0 {
        f.entry.file_ref as i64
    } else {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        f.entry.path.hash(&mut h);
        (h.finish() & 0x7fff_ffff_ffff_ffff) as i64
    };
    Some(CacheKey {
        volume_guid: guid,
        file_ref,
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

fn tier1_hash(
    f: &LaidOutFile,
    size: u64,
    algo: HashAlgo,
    tier1_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    let to_read = size.min(tier1_bytes) as usize;
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
        // Small-file path: a single read_to_end is sequential by
        // nature, and small enough that the SEQUENTIAL_SCAN hint adds
        // no measurable benefit. Use plain File::open.
        let mut file = File::open(&f.entry.path)?;
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)?;
        return Ok(algo::hash_oneshot(algo, &buf));
    }
    // Block O: large-file streaming path. Open with FILE_FLAG_SEQUENTIAL_SCAN
    // on Windows so the kernel enables prefetch for the sequential read
    // pattern. Per Microsoft docs: "Access is intended to be sequential
    // from beginning to end. The system can use this as a hint to
    // optimize file caching." Tier 3 reads the entire file from offset
    // 0 to EOF in one pass — perfect match.
    //
    // Block O++: producer-consumer ping-pong via std::thread::scope +
    // sync_channel(1). One thread reads chunks from disk; the worker
    // (this) thread hashes them. The channel's capacity of 1 means
    // exactly one chunk-in-flight at any time — true ping-pong, not
    // a queue. Decouples read latency from hash latency: while the
    // producer reads chunk K+1, the consumer hashes chunk K.
    //
    // Why this is worth ~200μs thread-spawn overhead per file: on the
    // typical large-file workload (2 GiB at ~5 GB/s seq read = 400ms
    // wall), serial read+hash takes ~580ms; pipelined takes max(read,
    // hash) ≈ 400ms (disk-bound limit). Per-file save: ~180ms. Spawn
    // overhead is rounding error against that. On small Tier 3 files
    // (~10MB), the trade-off thins out but stays positive overall.
    let file = open_sequential(&f.entry.path)?;
    let path_display = f.entry.path.display().to_string();
    use std::sync::mpsc::sync_channel;
    let (tx, rx) = sync_channel::<Option<Vec<u8>>>(1);

    std::thread::scope(|scope| -> std::io::Result<Vec<u8>> {
        // Producer: read chunks from disk, ship them through the
        // channel. On error or EOF, signal completion by sending
        // `None` and exit.
        let read_handle = scope.spawn(move || -> std::io::Result<()> {
            let mut reader = BufReader::with_capacity(TIER3_BUF, file);
            loop {
                let mut buf = vec![0u8; TIER3_BUF];
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(None);
                        return Ok(());
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        // tx.send blocks if the consumer hasn't pulled
                        // the previous chunk — that's the ping-pong.
                        // If the consumer dropped rx (cancellation),
                        // send fails and we exit cleanly.
                        if tx.send(Some(buf)).is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        });

        // Consumer: pull chunks as they arrive, hash them in arrival
        // order (which equals on-disk order since the producer reads
        // sequentially).
        let mut hasher = ContentHasher::new(algo);
        while let Ok(Some(chunk)) = rx.recv() {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                drop(rx); // closes the channel; producer exits
                let _ = read_handle.join();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled",
                ));
            }
            hasher.update(&chunk);
        }

        // Drain producer result. If the producer errored, surface
        // that. If it panicked, surface a synthetic IO error.
        match read_handle.join() {
            Ok(Ok(())) => Ok(hasher.finalize()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("tier3 read thread panicked: {}", path_display),
            )),
        }
    })
}

/// Open a file with sequential-scan hint. Block O — Tier 3 large-file
/// reads tell the OS "I'll read this start-to-end, prefetch
/// aggressively." Cross-platform shim: on Windows, uses
/// `CreateFileW` + `FILE_FLAG_SEQUENTIAL_SCAN`. On other platforms,
/// falls back to plain `File::open` (Linux has `posix_fadvise` but
/// `std::fs::File` doesn't expose the right hooks cleanly; future
/// optimization point).
#[cfg(windows)]
fn open_sequential(path: &std::path::Path) -> std::io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_SEQUENTIAL_SCAN, FILE_GENERIC_READ, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
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
            FILE_FLAG_SEQUENTIAL_SCAN,
            None,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e}")))?
    };
    if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: We just got the handle from CreateFileW; it's valid and
    // we own it. Wrapping in File transfers ownership; File's Drop
    // will CloseHandle on the way out.
    Ok(unsafe { File::from_raw_handle(handle.0 as *mut _) })
}

#[cfg(not(windows))]
fn open_sequential(path: &std::path::Path) -> std::io::Result<File> {
    File::open(path)
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
                placeholder: crate::inventory::PlaceholderState::NotPlaceholder,
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
            tier1_bytes: TIER1_BYTES,
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
            allow_recall_on_read: false,
            io_threads: 4,
            hash_algo: HashAlgo::Blake3,
        }
    }

    fn cfg_with_allow_recall() -> ScanConfig {
        let mut c = cfg();
        c.allow_recall_on_read = true;
        c
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

    /// Build a LaidOutFile carrying an explicit placeholder state. Used
    /// by the T2.1 phase 4 tier-guard tests below — phase 4 doesn't need
    /// a real on-disk reparse point to validate its filter logic, just
    /// a FileEntry whose `placeholder` field says what to do.
    fn lo_with_placeholder(
        path: PathBuf,
        size: u64,
        placeholder: crate::inventory::PlaceholderState,
    ) -> LaidOutFile {
        let mut f = lo(path, size);
        f.entry.placeholder = placeholder;
        f
    }

    /// Build a LaidOutFile with explicit inode identity. Used by the
    /// T0.5 tests to exercise the inode-dedup-before-hashing path —
    /// two paths sharing the same `(volume_guid, file_ref)` simulate a
    /// hardlink pair.
    fn lo_with_inode(path: PathBuf, size: u64, vol: &str, file_ref: u64) -> LaidOutFile {
        let mut f = lo(path, size);
        f.entry.volume_guid = Some(vol.to_string());
        f.entry.file_ref = file_ref;
        f
    }

    /// Tier guard MUST drop files marked RecallOnOpen before any
    /// content read happens — even when they're nominally part of a
    /// duplicate-candidate group. The corresponding bytes never exist
    /// locally (cloud stub), so opening them would force a hydration
    /// the user didn't ask for.
    #[test]
    fn tier_guard_drops_recall_on_open() {
        use crate::inventory::PlaceholderState;
        let d = tmpdir();
        let body = vec![0x5Au8; 8 * 1024];
        let a = d.join("a");
        let b = d.join("b");
        let c = d.join("c");
        // a and b are real files; c is "marked" as a cloud placeholder
        // even though the bytes exist on disk — the guard only consults
        // the placeholder field, not actual reparse state.
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        fs::write(&c, &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo(a, body.len() as u64),
                lo(b, body.len() as u64),
                lo_with_placeholder(c, body.len() as u64, PlaceholderState::RecallOnOpen),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        // Two real duplicates survive; the placeholder is excluded.
        assert_eq!(result.len(), 1, "two real dupes should still group");
        assert_eq!(
            result[0].files.len(),
            2,
            "placeholder must not appear in dup group"
        );
        fs::remove_dir_all(&d).ok();
    }

    /// If ALL files in a candidate group are placeholder-blocked, the
    /// group collapses to zero survivors and no DuplicateGroup is
    /// emitted. The fewer-than-2 survivors check after the guard
    /// handles this uniformly with all the other tier collapses.
    #[test]
    fn tier_guard_collapses_all_placeholder_group() {
        use crate::inventory::PlaceholderState;
        let d = tmpdir();
        let a = d.join("a");
        let b = d.join("b");
        // Files exist so the harness doesn't error, but the placeholder
        // marker should block them before any open happens.
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let group = LaidOutGroup {
            size: 1,
            files: vec![
                lo_with_placeholder(a, 1, PlaceholderState::RecallOnDataAccess),
                lo_with_placeholder(b, 1, PlaceholderState::OtherReparse(0xC0DECAFE)),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert!(
            result.is_empty(),
            "all-placeholder group must collapse to no dup output"
        );
        fs::remove_dir_all(&d).ok();
    }

    /// Phase 6 opt-in: with --allow-recall-on-read, RecallOnOpen and
    /// RecallOnDataAccess pass the guard and get hashed. Validates that
    /// the policy bit reaches the guard from ScanConfig (not just that
    /// the policy method works in isolation, which is covered in
    /// placeholder.rs).
    #[test]
    fn tier_guard_recall_passes_when_opted_in() {
        use crate::inventory::PlaceholderState;
        let d = tmpdir();
        let body = vec![0x42u8; 8 * 1024];
        let a = d.join("a");
        let b = d.join("b");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo_with_placeholder(a, body.len() as u64, PlaceholderState::RecallOnOpen),
                lo_with_placeholder(b, body.len() as u64, PlaceholderState::RecallOnDataAccess),
            ],
        };
        let result = run(vec![group], &cfg_with_allow_recall()).unwrap();
        assert_eq!(
            result.len(),
            1,
            "with --allow-recall-on-read, recall placeholders hash and group"
        );
        assert_eq!(result[0].files.len(), 2);
        fs::remove_dir_all(&d).ok();
    }

    /// Phase 7 counter: blocked placeholders increment the per-state
    /// counters so the scan-finish log line can give a breakdown.
    /// Validates both buckets (recall + other-reparse) and confirms
    /// ReparseDedup does not increment (it's never blocked).
    #[test]
    fn tier_guard_counters_split_by_state() {
        use crate::inventory::PlaceholderState;
        let d = tmpdir();
        let body = vec![0x99u8; 8 * 1024];
        let mk = |i: u8, ph: PlaceholderState| {
            let p = d.join(format!("f{i}"));
            fs::write(&p, &body).unwrap();
            lo_with_placeholder(p, body.len() as u64, ph)
        };
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                mk(0, PlaceholderState::RecallOnOpen),
                mk(1, PlaceholderState::RecallOnDataAccess),
                mk(2, PlaceholderState::RecallOnOpen),
                mk(3, PlaceholderState::OtherReparse(0xC0DE)),
                mk(4, PlaceholderState::ReparseDedup), // allowed, doesn't increment
                mk(5, PlaceholderState::ReparseDedup),
            ],
        };
        let (_dups, counters) = run_with_counters(vec![group], &cfg(), None).unwrap();
        assert_eq!(
            counters.placeholders_blocked_recall.load(Ordering::Relaxed),
            3,
            "RecallOnOpen + RecallOnDataAccess sum"
        );
        assert_eq!(
            counters
                .placeholders_blocked_other_reparse
                .load(Ordering::Relaxed),
            1,
            "OtherReparse counted separately"
        );
        fs::remove_dir_all(&d).ok();
    }

    /// Phase 6 opt-in DOES NOT extend to OtherReparse — unknown
    /// reparses stay blocked regardless of policy. Asymmetric on
    /// purpose: recall-class is a known cloud-hydration trade-off the
    /// user can opt into knowing what they get; unknown is unknown.
    #[test]
    fn tier_guard_other_reparse_still_blocked_when_opted_in() {
        use crate::inventory::PlaceholderState;
        let d = tmpdir();
        let body = vec![0x55u8; 8 * 1024];
        let a = d.join("a");
        let b = d.join("b");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo_with_placeholder(a, body.len() as u64, PlaceholderState::OtherReparse(0x1234)),
                lo_with_placeholder(b, body.len() as u64, PlaceholderState::OtherReparse(0x1234)),
            ],
        };
        let result = run(vec![group], &cfg_with_allow_recall()).unwrap();
        assert!(
            result.is_empty(),
            "unknown reparses stay blocked even with --allow-recall-on-read"
        );
        fs::remove_dir_all(&d).ok();
    }

    /// T0.5: two LaidOutFiles sharing one inode (synthetic
    /// volume_guid + file_ref) get partitioned into the link-equiv
    /// stream, hashed once, and emitted as a single
    /// `link_equivalent: true` dup group with both paths.
    #[test]
    fn t05_link_equivalent_aliases_collapse_to_one_hash() {
        let d = tmpdir();
        let body = vec![0x77u8; 8 * 1024];
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        // Synthetic shared inode. The fact that the on-disk files are
        // separate doesn't matter — Stream A only uses the rep's path
        // for the tier3 read, and the alias list is what gets emitted.
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo_with_inode(a.clone(), body.len() as u64, "vol-A", 42),
                lo_with_inode(b.clone(), body.len() as u64, "vol-A", 42),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(result.len(), 1, "two aliases of one inode → one dup group");
        assert!(
            result[0].link_equivalent,
            "multi-alias-of-one-inode must be flagged link_equivalent"
        );
        assert_eq!(result[0].unique_inodes, 1);
        assert_eq!(result[0].files.len(), 2);
        fs::remove_dir_all(&d).ok();
    }

    /// T0.5: distinct inodes with the same content still group as a
    /// cross-inode dup via Stream B (the normal tier pipeline). Each
    /// file occupies its own inode_key, so all flow through tiers and
    /// emerge in one (hash, files) bucket.
    #[test]
    fn t05_distinct_inodes_same_content_still_dup() {
        let d = tmpdir();
        let body = vec![0x33u8; 8 * 1024];
        let a = d.join("a");
        let b = d.join("b");
        let c = d.join("c");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        fs::write(&c, &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo_with_inode(a, body.len() as u64, "vol-A", 1),
                lo_with_inode(b, body.len() as u64, "vol-A", 2),
                lo_with_inode(c, body.len() as u64, "vol-A", 3),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(result.len(), 1, "three distinct inodes with same content group");
        assert!(
            !result[0].link_equivalent,
            "different inodes → link_equivalent must be false"
        );
        assert_eq!(result[0].unique_inodes, 3);
        assert_eq!(result[0].files.len(), 3);
        fs::remove_dir_all(&d).ok();
    }

    /// T0.5: mixed group — 2 paths on inode X, 1 path on inode Y.
    /// Stream A emits inode-X as link_equivalent (Y has 1 alias and
    /// stays singleton in Stream B since no cross-inode pair exists).
    /// Documented tradeoff: scenario 2 from the design notes.
    #[test]
    fn t05_mixed_inodes_emits_separate_groups() {
        let d = tmpdir();
        let body = vec![0x88u8; 8 * 1024];
        let xa = d.join("xa");
        let xb = d.join("xb");
        let y = d.join("y");
        fs::write(&xa, &body).unwrap();
        fs::write(&xb, &body).unwrap();
        fs::write(&y, &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo_with_inode(xa, body.len() as u64, "vol-A", 10),
                lo_with_inode(xb, body.len() as u64, "vol-A", 10), // shares inode 10
                lo_with_inode(y, body.len() as u64, "vol-A", 99),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        // Inode 10 emits a link_equivalent dup group with 2 paths.
        // Inode 99 is singleton in Stream B → no dup output (loses
        // the cross-inode match with X's content; documented tradeoff).
        assert_eq!(result.len(), 1, "only the link-equiv group emits");
        assert!(result[0].link_equivalent);
        assert_eq!(result[0].files.len(), 2);
        fs::remove_dir_all(&d).ok();
    }

    /// ReparseDedup is the special case: NTFS dedup'd files ARE
    /// readable via the standard API (the FS is transparent at this
    /// layer), so the guard must NOT drop them. Hashing proceeds; the
    /// `link_equivalent` flag downstream handles the "this group is
    /// already FS-deduped" framing.
    #[test]
    fn tier_guard_lets_reparse_dedup_through() {
        use crate::inventory::PlaceholderState;
        let d = tmpdir();
        let body = vec![0x5Au8; 8 * 1024];
        let a = d.join("a");
        let b = d.join("b");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo_with_placeholder(a, body.len() as u64, PlaceholderState::ReparseDedup),
                lo_with_placeholder(b, body.len() as u64, PlaceholderState::ReparseDedup),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(
            result.len(),
            1,
            "ReparseDedup files must still group as duplicates"
        );
        assert_eq!(result[0].files.len(), 2);
        fs::remove_dir_all(&d).ok();
    }

    /// Block O++ correctness contract: the streaming Tier-3 path (used
    /// for files > TIER3_ONESHOT_THRESHOLD = 1 MiB) MUST produce the
    /// same content_hash as a single-shot hash of the same bytes.
    /// Without this test, the new producer-consumer ping-pong path is
    /// unverified — none of the prior Tier-3 tests use files large
    /// enough to take the streaming branch.
    #[test]
    fn tier3_streaming_path_matches_oneshot_hash() {
        let d = tmpdir();
        // Make a file just above the oneshot threshold so we know
        // the streaming branch runs. Use deterministic non-trivial
        // content (xorshift-stamped 4 MiB).
        let size = (TIER3_ONESHOT_THRESHOLD + 1024 * 1024) as usize;
        let mut body = vec![0u8; size];
        let mut x: u32 = 0xDEAD_BEEF;
        for chunk in body.chunks_mut(4) {
            x ^= x.wrapping_shl(13);
            x ^= x.wrapping_shr(17);
            x ^= x.wrapping_shl(5);
            let b = x.to_le_bytes();
            for (i, byte) in chunk.iter_mut().enumerate() {
                *byte = b[i.min(3)];
            }
        }
        let a = d.join("streaming-a.bin");
        let b = d.join("streaming-b.bin");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();

        // Compute the oneshot reference hash via algo::hash_oneshot.
        let reference_hash = algo::hash_oneshot(HashAlgo::Blake3, &body);
        let reference_hex = hex(&reference_hash);

        // Now run the dup-detection pipeline. Two identical files >
        // the oneshot threshold MUST trigger the streaming Tier 3
        // path and produce a dup group whose content_hash equals the
        // oneshot reference.
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![lo(a, body.len() as u64), lo(b, body.len() as u64)],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(result.len(), 1, "two identical files must group");
        assert_eq!(
            result[0].content_hash, reference_hex,
            "streaming-path content_hash must equal oneshot reference; \
             producer-consumer ping-pong has a chunk-ordering bug otherwise"
        );
        fs::remove_dir_all(&d).ok();
    }
}
