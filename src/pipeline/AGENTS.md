# pipeline — AGENTS guide

## Purpose

`src/pipeline/` owns stages 2-5 of a scan: size-grouping (`grouping`),
physical layout / LCN annotation (`layout`), progressive content
hashing (`hash`, with its `algo` and `format` submodules), and perceptual
similarity for images (`image_hash`) and audio (`audio_hash`). It also
contains an unused-but-staged IOCP read scheduler (`iocp`) and an
opt-out startup probe (`io_threads_probe`) that decides the default
`--io-threads` value by measuring real cold-read throughput on the
target volume.

The top-level `mod.rs` is also where the cross-stage value types live:
`DuplicateGroup` (the engine's output), `SkippedFile` (placeholder
files surfaced as informational), `SimilarityKind` (byte-identical vs
perceptual-image vs perceptual-audio), and the `assert_unique_paths`
defensive invariant check that fires in debug builds.

Hash, image_hash, and audio_hash subdirectories are audited separately
and are not described here beyond their entry points.

## Files

### `mod.rs`

Cross-stage shared types + a debug-only group-shape invariant check.
Declares the submodules and the public `DuplicateGroup`,
`SkippedFile`, `SimilarityKind` enum.

- Public API:
  - `struct SkippedFile { path, placeholder, reparse_tag }` — JSON-stable
    record of an inventory placeholder. Field `placeholder` is the
    lowercase string form of `inventory::PlaceholderState`.
  - `SkippedFile::from_state(path, state) -> Option<Self>` — returns
    `None` for `NotPlaceholder`.
  - `enum SimilarityKind { ByteIdentical (default), PerceptualImage,
    PerceptualAudio }` — kebab-case serde tags, `#[serde(default)]` on
    consumer-side keeps v2 JSON readable.
  - `fn assert_unique_paths(group: &DuplicateGroup)` — debug-only
    invariant guard (#70 data-loss class); a `DuplicateGroup` must not
    list the same path twice.
  - `struct DuplicateGroup { size, content_hash, files, link_equivalent,
    unique_inodes, similarity_kind, decode_warning_paths, file_sizes }`
    — confirmed dup-group, used as both wire and in-memory shape.
- Who calls this: `inventory`, `output`, `main`, `gui::live`, the
  perceptual Tier-4 modules, integration tests.
- Key invariants:
  - For perceptual groups, `size` is the **largest** member's size,
    not a shared size; per-member sizes live in `file_sizes`.
  - `unique_inodes == 0` in older JSON = "unknown"; consumers fall
    back to the path-aware metric.
  - `assert_unique_paths` only fires when `cfg!(debug_assertions)` is
    true (in practice = debug + test builds).

### `grouping.rs` (Stage 2)

Bucket inventoried files by `u64` size; drop singleton buckets.
Also owns `resolve_file_ids`, which batches NTFS file-id resolution
per-parent-directory after size grouping so unique-size files don't
pay a `GetFileInformationByHandle` syscall they'd never need.

- Public API:
  - `struct SizeGroup { size, files }`
  - `fn group_by_size(files: Vec<FileEntry>) -> Vec<SizeGroup>` — zero-byte
    files form one group if multiple, else dropped.
  - `fn resolve_file_ids(groups: &mut [SizeGroup])` — Windows: batched
    `FileIdBothDirectoryInfo` per unique parent dir + per-file slow-path
    fallback. Non-Windows: no-op (sentinel values left as-is).
- Who calls this: `src/main.rs`, `src/gui/live.rs`, integration tests.
- Feature gates: `#[cfg(windows)]` / `#[cfg(not(windows))]` on
  `resolve_file_ids`.

### `layout.rs` (Stage 3)

Today a near-no-op pass-through that lifts every `SizeGroup` into a
`LaidOutGroup` of `LaidOutFile { entry, start_lcn: None }`. The
module doc says "Implementation lands in Implementation Order step 4";
the real LCN extent map has not landed.

- Public API:
  - `struct LaidOutFile { entry: FileEntry, start_lcn: Option<u64> }`
  - `struct LaidOutGroup { size: u64, files: Vec<LaidOutFile> }`
  - `fn resolve(groups: Vec<SizeGroup>) -> Result<Vec<LaidOutGroup>>`
    — currently always returns groups with `start_lcn = None`.
- Who calls this: `src/main.rs`, `src/gui/live.rs`, tests.

### `hash.rs` (Stage 4)

Tier 0-3 progressive hashing. The bulk of the perf-sensitive
engine code lives here. Hash compute is delegated to `algo`; format-
aware Tier-0 fingerprints to `format`.

- Tier constants:
  - `TIER1_BYTES: u64 = 4 KiB` (default; runtime override via
    `ScanConfig::tier1_bytes`)
  - `TIER3_ONESHOT_THRESHOLD = 1 MiB` (file-size cutoff for the slurp
    path)
  - `TIER2_REGION = 64 KiB` (head / mid / tail)
  - `TIER2_MIN_FILE = 256 KiB` (smaller files skip Tier 2 entirely)
  - `TIER3_BUF = 1 MiB` (chunked-read buffer for large files)
- Public API:
  - `struct HashCounters` — atomic per-tier counters (micros, bytes,
    count) plus cache hit/write/drift/failure counters plus
    `tier2_input_files` / `tier2_survivors` plus
    `placeholders_blocked_recall` / `placeholders_blocked_other_reparse`.
  - `enum ProgressOutcome { Hashed{bytes}, Cached{bytes}, Failed{error} }`
  - `type FileProgress = Arc<dyn Fn(&Path, u8, ProgressOutcome) + Send + Sync>`
  - `type GroupComplete = Arc<dyn Fn(&DuplicateGroup) + Send + Sync>`
    — streaming dup-group emit, fires from rayon workers as each
    group is finalized inside `run_group`.
  - `fn run(...)` / `fn run_with_counters(...)` /
    `fn run_with_progress(...)` / `fn run_cancellable(...)` /
    `fn run_cancellable_with_pool(...)` / `fn run_streaming(...)`
    — increasingly-featured entry points, all delegate to a private
    `run_with_counters_inner`.
  - `fn build_io_pool(cfg) -> Result<rayon::ThreadPool>` — exposed so
    the GUI can build the pool once and share it across chunked calls.
  - `pub(crate) fn cache_key(f, algo) -> Option<CacheKey>` — synthesises
    stable filler values when walker-supplied `volume_guid` / `file_ref`
    are missing.
  - `pub use algo::{ContentHasher, HashAlgo};` re-export.
  - `pub mod algo;` `pub mod format;` (submodules audited separately).
- Who calls this: `src/main.rs` (CLI), `src/gui/live.rs` (GUI),
  `tests/*`, `src/bin/hash_repro.rs`, `examples/hash_microbench.rs`.
- Key invariants:
  - Tier escalation: a file only sees tier N+1 if at least one OTHER
    survivor in its group also passed tier N.
  - Single rayon `ThreadPool` shared across chunked invocations
    (`shared_pool: Some(&pool)`); pre-#195 each chunk rebuilt threads.
  - The producer-consumer ping-pong in `tier3_hash_cancellable` uses a
    `sync_channel(1)` for true one-chunk-in-flight semantics —
    decouples read from hash latency.
  - `cold_enforced` short-circuits all tiers to read whole file via
    `superdeduper_bench_real::read_uncached` (no sector-alignment math).
  - Stream A (link-equivalent inodes) hashes ONE rep per inode at Tier 3
    directly and emits the full alias list with `link_equivalent=true`.
  - The cache-key falls back to a path-derived synthetic when
    `file_ref == 0` or `volume_guid is None`; without this every
    unresolved-inode file would collide on the same primary key.
- Feature gates: `#[cfg(windows)]` / `#[cfg(not(windows))]` on
  `open_sequential`. Env-gated diagnostics:
  `SUPERDEDUPER_PERF_INSTRUMENT_RAYON` enables per-worker
  attribution (see `RayonPerfSlots`).

### `io_threads_probe.rs`

Startup throughput probe (Mick GO 2026-06-02 mechanism β). Walks a
small probe corpus from the first scan root, measures wall-clock for
1 / 4 / 8 / 16 io-threads via `superdeduper_bench_real::read_uncached`
(bypasses OS page cache), picks the winning io-threads count.

- Public API:
  - `fn probe_optimal_io_threads(scan_root: &Path) -> std::io::Result<usize>`
- Constants:
  - `PROBE_CANDIDATES = [1, 4, 8, 16]`
  - `PROBE_MAX_FILES = 16`
  - `PROBE_FILE_MAX_BYTES = 1 MiB` (filter cap; actually `read_uncached`
    reads the whole file)
  - `PROBE_FILE_MIN_BYTES = 64 KiB`
  - `PROBE_WALK_TIME_CAP = 500 ms`
  - `PROBE_TOTAL_TIME_CAP = 15 s`
- Who calls this: `src/config.rs::compute_default_io_threads`.

### `iocp.rs`

Staged but unwired IOCP / LCN-sorted read scheduler. Provides a
`Scheduler` trait with `BufferedScheduler` (cross-platform) and
`WindowsScheduler` (IOCP-backed; reads still execute via the buffered
backend under the hood — only the LCN sort is real).

- Public API:
  - `struct ReadRequest { start_lcn_bytes, length, path, file_offset }`
  - `struct ReadCompletion { request, bytes, latency_us }`
  - `trait Scheduler { submit; run_to_completion }`
  - `mod buffered`: `BufferedScheduler`, `align_up`, `read_range`
  - `#[cfg(windows)] mod win`: `WindowsScheduler::{new, submit,
    pending_len, port_handle, run_to_completion}`, `associate`,
    `type _Overlapped = OVERLAPPED` (`#[allow(dead_code)]`).
- Who calls this: nothing outside the file. The hashing tiers state
  they "currently go through `std::fs::File` directly" and the
  scheduler is "the substrate the next engine pass will rewire them
  onto." The next engine pass took a different route (per-file
  `BufReader` + `sync_channel(1)` ping-pong inside
  `tier3_hash_cancellable`); the IOCP module remains dormant.

## Invariants / Gotchas

- **Group same-path is a data-loss bug**: `assert_unique_paths` exists
  to catch a regression that would, if shipped, let a Recycle-on-loser
  click delete a unique file. Any new emit site of `DuplicateGroup`
  must run through this guard (every emit in `run_group` already does).
- **Perceptual `size` semantics differ**: for `SimilarityKind != ByteIdentical`,
  `DuplicateGroup::size` is the largest member's size; per-member
  sizes live in `file_sizes` (added in #147). Code that checks
  "has-this-file-changed-since-scan" must use `file_sizes[i]` when
  populated, falling back to `size` otherwise.
- **Cache-key synthetic filler**: `cache_key` synthesises stable
  `volume_guid` / `file_ref` from the path when walker fields are
  missing, so the primary key doesn't collapse to a single row on
  Linux / network shares / permission-denied parents.
- **Streaming vs chunked GUI invocation**: `run_streaming` exists so
  the GUI can call the hash pipeline ONCE across the whole corpus and
  consume groups via callback into a lock-free channel + ~100 ms
  batching runner. Pre-streaming the GUI chunked the corpus and paid
  per-chunk pool-rebuild + re-sort overhead (until #195 hoisted the
  pool, then v0.3.40 hoisted the par_iter scope).
- **Tier guard depth**: `apply_tier_guards` runs at the start of
  `run_group` even though inventory already classified placeholders —
  defense in depth against races, attribute changes between enum and
  hash, and walker-fallback paths that don't classify.
- **Link-equivalent stream is serial per outer group**: Stream A is
  one-tier3-hash-per-inode-rep; no nested par_iter. Per-rayon-worker
  attribution is correct because that worker actually executed the
  file-task.
- **`tiered` doc says "summed CPU time"** but it's actually summed
  wall-clock time across workers (Instant-bracketed around `compute()`
  per file). Treat values as worker-summed Instant time, not perf-counter
  CPU time.
- **`tier_byte_estimate` is a heuristic**: Tier-0 byte estimate is a
  flat 64 KiB constant, not the actual bytes read by `format::fingerprint`.

## Dependencies

- INCOMING:
  - `src/main.rs` — CLI orchestrator: calls grouping, layout, hash::run_with_progress
  - `src/gui/live.rs` — engine driver, chunking, streaming consumer
  - `src/inventory/mod.rs` — produces `Vec<FileEntry>` + `Vec<SkippedFile>`
  - `src/output.rs` — serialises `DuplicateGroup` + `SkippedFile` to JSON
  - `src/cache.rs` — `Cache` / `CacheKey` / `CachedHashes` consumed by `tiered*`
  - `src/config.rs::compute_default_io_threads` — calls io_threads_probe
  - `src/bin/hash_repro.rs`, `examples/hash_microbench.rs`,
    `tests/{smoke,resumability,properties,cache_corpus_reset,...}`
- OUTGOING:
  - `crate::inventory::{FileEntry, PlaceholderState, walk, dir_enum}`
  - `crate::cache::{Cache, CacheKey, CachedHashes, LookupOutcome}`
  - `crate::config::ScanConfig`
  - External: `rayon`, `parking_lot`, `hashbrown`, `serde`, `tracing`,
    `windows-rs` (CreateFileW, IOCP), `superdeduper_bench_real::read_uncached`,
    `tempfile` (tests).

## Refactor Hints

- **Dead code: `pipeline::iocp`.** Verified by:
  `grep -rn "iocp::\|BufferedScheduler\|WindowsScheduler\|ReadRequest\|ReadCompletion\|associate" --include="*.rs"`
  returns only hits inside `src/pipeline/iocp.rs`. The hash pipeline
  comments still claim "the next engine pass will rewire onto IOCP" but
  the real path taken in `tier3_hash_cancellable` is a per-file
  `BufReader` + `sync_channel(1)` producer-consumer. Either pull the
  module out, or update the doc-comment to say "kept as a parking lot
  for the IOCP experiment."
- **`layout::resolve` is a no-op.** The struct + module exist solely
  to carry an Option<u64> that is always `None`. A refactor could
  either (a) wire real LCN extents per the original design, or
  (b) collapse `LaidOutFile` into `FileEntry` and delete the module.
  Currently a coupling tax — every caller (main, live, tests) has to
  thread groups through `layout::resolve` for no behavioural reason.
- **`run_with_progress` has no callers outside the hash module**:
  `grep -rn "run_with_progress\b"` shows only the definition and the
  delegating `run_cancellable*` callers. Could be inlined away once
  another entry-point variant is added.
- **`HashCounters` rebuild on `Arc::try_unwrap` failure** (lines
  576-604) is a 30-line snapshot helper that should be extracted to
  an `impl HashCounters { fn snapshot(&self) -> Self }`.
- **Six near-identical hash entry points** (`run`, `run_with_counters`,
  `run_with_progress`, `run_cancellable`, `run_cancellable_with_pool`,
  `run_streaming`) all delegate to `run_with_counters_inner` with a
  growing tuple of Optionals. A builder-pattern or `HashJob` config
  struct would reduce the surface.
- **`SUPERDEDUPER_IOTHREADS_PARKED` interpretation**: doc says "=1" sets
  it (`io_threads_probe.rs:28`, `config.rs:266`), but `config.rs:352/452`
  uses `env::var(...).is_ok()` so any non-empty value (`0`, `false`,
  empty string is rejected) skips the probe. Either tighten the env-var
  parse or update the docs.

## Wire Surfaces

- **JSON (output) surface**: `DuplicateGroup` and `SkippedFile` are
  the serde-serialised wire shapes. Forward compatibility hooks:
  - `unique_inodes` `#[serde(default)]` — 0 means "unknown" on read.
  - `similarity_kind` `#[serde(default)]` — older JSON without it lands
    as `ByteIdentical`; serialised kebab-case (`byte-identical`,
    `perceptual-image`, `perceptual-audio`).
  - `decode_warning_paths` `#[serde(default, skip_serializing_if = "Vec::is_empty")]`
  - `file_sizes` `#[serde(default, skip_serializing_if = "Vec::is_empty")]`
  - `SkippedFile.reparse_tag` `#[serde(skip_serializing_if = "Option::is_none")]`
- **Cache key surface**: `cache_key` synthesises a stable
  `(volume_guid, file_ref)` from the path when those fields are absent;
  changing that derivation breaks resume-cache hits across versions.
- **Environment variables read in this dir**:
  - `SUPERDEDUPER_PERF_INSTRUMENT_RAYON` (hash.rs) — enables per-worker
    attribution lines.
  - `SUPERDEDUPER_IOTHREADS_PARKED` (referenced in io_threads_probe doc,
    read in `src/config.rs`) — skip probe, fall back to (α) per-disk-class
    table.
- **CLI flags this dir owns** (read via `ScanConfig`): `--tier1-bytes`,
  `--io-threads`, `--allow-recall-on-read`, `--cold-enforced`,
  format-aware Tier-0 toggle (`use_format_aware`).

## Non-source artifacts

None at this level.
