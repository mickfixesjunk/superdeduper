# inventory — AGENTS guide

## Purpose
Stage 1 of the superdeduper pipeline: produce a `Vec<FileEntry>` listing every eligible file under the user's scan roots, applying min/max-size, include/exclude globs, master Settings -> Exclusions, and superdeduper-self-footprint filters.

Two acquisition strategies live here:

* `walk` (default since v0.3.16 Path B, Mick GO 2026-06-01): layer-parallel BFS using rayon; on Windows uses the per-directory `FileIdBothDirectoryInfo` fast path (via `dir_enum`) and falls back to stdlib `read_dir`; on Unix uses `read_dir`.
* `mft` (opt-in `--force-mft`, Windows, requires admin): direct `FSCTL_ENUM_USN_DATA` enumeration per NTFS volume with parent-ref path reconstruction. May silently elide hardlink aliases and historically skipped exclusion filters; retained as an escape hatch.

A `warm` path layered under `mft` applies a USN-journal delta to a cached baseline so a re-scan pays only the cost of inter-scan churn instead of a full MFT re-walk. `placeholder` classifies cloud / dedup / reparse points so downstream stages can refuse to force-hydrate cloud stubs.

## Files

### `mod.rs`
Public entry point. Defines `FileEntry` (the common output type) and the `enumerate` / `enumerate_with_skipped` functions. Dispatches between walker and MFT based on `cfg.force_mft` + `all_roots_are_volume_roots`. Also derives the `SkippedFile` list (placeholder-class outcomes + walker's `EntrySkipped` for `symlink target unreadable`) so the JSON output's `skipped[]` array is populated.

- Public API:
  - `pub struct FileEntry { path, size, mtime, file_ref, parent_ref, usn, attributes, volume_guid, placeholder }` — common record produced by all strategies.
  - `pub use placeholder::PlaceholderState`
  - `pub fn enumerate(cfg, cache) -> Result<Vec<FileEntry>>`
  - `pub fn enumerate_with_skipped(cfg, cache) -> Result<(Vec<FileEntry>, Vec<SkippedFile>)>`
  - Re-exports of submodules: `dir_enum` (windows-only), `mft`, `placeholder`, `walk`, `warm`.
- Who calls this: `src/main.rs:2074` (CLI scan), `tests/walker_fast_path.rs`, `tests/cache_corpus_reset.rs`; `gui/live.rs` calls `inventory::walk::enumerate_cancellable` directly.
- Key types: `FileEntry`. Note `file_ref`/`usn` may be `0` on the walker fallback (non-NTFS concepts).

### `placeholder.rs`
Cloud-files / NTFS reparse-point classification. The `classify(attrs, reparse_tag)` function maps Win32 attributes + reparse tag onto `PlaceholderState`. Drives skip decisions at the action layer and content-read tier guards (don't force-hydrate cloud stubs).

- Public API:
  - `pub enum PlaceholderState { NotPlaceholder, RecallOnOpen, RecallOnDataAccess, ReparseDedup, OtherReparse(u32) }` — Default, Serde, Hash.
  - `pub fn blocks_content_read(self) -> bool`
  - `pub fn emits_event(self) -> bool`
  - `pub fn blocks_destructive_action(self) -> bool`
  - `pub fn blocks_content_read_under_policy(self, allow_recall_on_read) -> bool`
  - `pub fn blocks_destructive_action_under_policy(self, allow_destructive_on_deduped) -> bool`
  - `impl fmt::Display` — snake-case tags (must match JSON `placeholder` field).
  - `pub fn classify(attrs: u32, reparse_tag: Option<u32>) -> PlaceholderState` (windows + non-windows stub).
- Who calls this: `dedupe.rs` (`placeholder_state_for`), `pipeline::hash`, `pipeline::grouping`, all three inventory producers (`mft`, `warm`, `walk`/`walk_fast_path`/`enumerate_one_folder_*`).
- Key invariant: snake-case `Display` strings MUST match the JSON `placeholder` field; divergence was caught by testdesign on commit 1083a25.

### `walk.rs`
The fallback / default inventory path. Layer-parallel BFS over the scan roots. On Windows uses `dir_enum::enumerate_dir_full` (one syscall per dir via `FileIdBothDirectoryInfo`) and populates `file_ref` from the batched call so Stage 2b can short-circuit; on Unix uses `read_dir`. Per-root walks run optionally in parallel via `cfg.parallel_roots` (rayon), with per-root visited-dir sets for symlink cycle detection.

- Public API:
  - `pub struct DirIdentity { volume_serial: u64, file_id: u128 }`
  - `pub enum WalkEvent<'a> { Entered, FileFound, DirError, EntrySkipped, SymlinkCycleSkipped }`
  - `pub fn enumerate(cfg) -> Result<Vec<FileEntry>>`
  - `pub fn enumerate_with_progress<F>(cfg, callback) -> Result<...>`
  - `pub fn enumerate_cancellable<F>(cfg, cancel, callback) -> Result<...>` — called by `gui/live.rs`
  - `pub fn dir_identity(path) -> Option<DirIdentity>` — symlink cycle helper.
  - `pub(crate) fn file_id_for(path)` (windows) — used by `pipeline::grouping::resolve_file_ids`.
  - `pub(crate) enum OwnedWalkEvent { ... }` — owned twin of `WalkEvent` for cross-thread replay.
- Who calls this: `inventory::mod::enumerate_with_skipped`, `gui::live::scan_thread`, `pipeline::grouping`, tests under `tests/walker_fast_path.rs`.
- Key invariants:
  - Events emitted in BFS layer order (every depth-N `Entered` precedes any depth-N+1 event); within a layer, sibling order is rayon-undefined.
  - The `\\?\` verbatim prefix is intentionally retained on emitted paths so `File::open` later sees trailing-dots / reserved-DOS names correctly (test30 bug).
  - When `cfg.parallel_roots`, each root gets its own `visited_dirs`; cross-root duplicates are caught by post-walk `dedup_by_path`.
- Feature gates: `#[cfg(windows)]` for `walk_fast_path`, `enumerate_one_folder_fast_path`, `file_id_for`, `to_verbatim`, `win_file_attributes`; `#[cfg(unix)]` for `inode_identity`.

### `mft.rs`
Windows-only opt-in fast path: enumerate the entire MFT via `FSCTL_ENUM_USN_DATA`, reconstruct each record's path from its parent_ref chain, filter to scan roots + globs + size. Falls back to walker on `ACCESS_DENIED` (non-admin) or any other failure.

- Public API:
  - `pub fn enumerate(cfg, cache) -> Result<Vec<FileEntry>>` (windows). Non-windows returns `Error::Unsupported`.
- Who calls this: `inventory::mod::enumerate_with_skipped` only (via `--force-mft`).
- Key invariants:
  - Persisted snapshot writes use the PRE-enum journal `next_usn` as the cursor (not `max(record_usn, next_usn)`) so concurrent writes during the long enum land in the next delta and ERROR_INVALID_PARAMETER is avoided on the warm-path follow-up.
  - The `volume_root` MUST be the drive-letter form with a trailing backslash (`F:\`), not the verbatim form or `F:` (drive-relative) — `PathBuf::push` semantics depend on it; covered by `volume_root_push_keeps_separator` regression test.
  - Roots are canonicalised + verbatim-stripped before comparison so `\\?\F:\Github` and `F:\Github` line up.
- Feature gates: `#[cfg(windows)]` for `enumerate_volume`, `persist_cold_snapshot`, `reconstruct_path`, `strip_verbatim_prefix`; `#[cfg(not(windows))]` stubs.

### `warm.rs`
Apply a USN-journal delta to a cached `InventoryRecord` baseline instead of re-walking the MFT. Validates `journal_id` + cursor freshness, loads `inventory_records` from cache, reads the delta via `FSCTL_READ_USN_JOURNAL`, applies create/update/delete per record, re-fetches metadata for survivors via `std::fs::metadata`, then writes ONLY the touched rows back through `apply_inventory_delta` (NOT a full snapshot replace — that was a 60-90s write-amplification regression).

- Public API:
  - `pub enum WarmOutcome { Applied{files,delta_records,created,updated,deleted}, Fallback{reason} }`
  - `pub fn try_warm(cfg, volume_guid, roots, cache) -> Result<WarmOutcome>` (windows + non-windows stub returning `Fallback`).
- Who calls this: `inventory::mft::enumerate` only.
- Key invariants:
  - Lenient `reconstruct_path` semantics MUST match `inventory::mft::reconstruct_path` byte-for-byte (missing parent => break, use what we have). An earlier strict variant filtered out 85% of records.
  - The delta-only persistence via `apply_inventory_delta` (NOT `save_inventory_snapshot`) — full-snapshot replace caused 60-90s Defender write-amp per warm scan.

### `dir_enum.rs` (windows-only)
Single-directory enumeration via `GetFileInformationByHandleEx + FileIdBothDirectoryInfo`. One syscall returns name + size + attributes + inode + mtime for every child of a directory. Used by `walk_bfs` per-folder (file fast path) and by `pipeline::grouping::resolve_file_ids` (batched inode-id resolution).

- Public API:
  - `pub struct DirInodeMap { volume_guid: Option<String>, by_name: HashMap<OsString, u64> }`
  - `pub fn enumerate_dir(dir: &Path) -> Option<DirInodeMap>` — name->inode only (used by grouping).
  - `pub struct DirEntryFull { name, size, attributes, file_id, mtime_filetime, is_dir }`
  - `pub struct DirFullEnumeration { volume_guid, entries }`
  - `pub fn enumerate_dir_full(dir) -> Option<DirFullEnumeration>` — full per-entry info (used by walk).
- Who calls this: `inventory::walk` (`enumerate_one_folder_pure` -> `enumerate_one_folder_fast_path`), `pipeline::grouping`.
- Feature gate: `#![cfg(windows)]` at module level — entire file is windows-only.

## Invariants / Gotchas
- **walker is the default**; `--force-mft` only takes the MFT path when EVERY root is a volume root. Mixed roots silently use the walker.
- **MFT path elides hardlink aliases**: reconstructs only the primary `parent_ref` path per inode; aliases outside the scan root vanish (738 vs 11,299 paths on the same corpus per benchmarker D'').
- **placeholder snake_case Display strings MUST match the JSON `placeholder` field** in `pipeline::SkippedFile`.
- **Windows path emissions keep the `\\?\` verbatim prefix** through the walker so downstream `File::open` handles trailing dots / reserved DOS names. Do NOT strip in the walker.
- **persist_cold_snapshot uses pre-enum `next_usn` as cursor**, never `max(record_usn, next_usn)`. The latter would trip `ERROR_INVALID_PARAMETER` if the journal was reset between record write and snapshot save.
- **`walk_bfs` cycle pruning is single-writer on the driver thread**: `visited_dirs` is touched only in phase A before the parallel harvest; cross-thread access would race.
- **walker post-pass `dedup_by_path`**: defensive net against overlapping roots / unresolved aliases. Pairs with `pipeline::mod::assert_unique_paths` debug-assert downstream.
- **`OtherReparse(_)` blocks reads by default** even though symlinks and junctions are filtered upstream — HSM/PrjFS/unknown cloud are hydration-class; safer-by-default.
- **warm-path `reconstruct_path` must match cold path lenient semantics**, byte-for-byte.

## Dependencies
- INCOMING:
  - `src/main.rs` (CLI scan, line 2074): `inventory::enumerate_with_skipped`
  - `src/gui/live.rs`: `inventory::walk::enumerate_cancellable`
  - `src/pipeline/grouping.rs`: `dir_enum::enumerate_dir`, `walk::file_id_for`
  - `src/pipeline/hash.rs`, `src/pipeline/layout.rs`, `src/pipeline/image_hash/tier4.rs`, `src/pipeline/audio_hash/tier4.rs`, `src/dedupe.rs`, `src/leaderboard/predicates.rs`, `src/gui/results_store.rs`: `FileEntry`, `PlaceholderState`, `placeholder::classify`
  - `tests/walker_fast_path.rs`, `tests/cache_corpus_reset.rs`
- OUTGOING:
  - `crate::cache` (`Cache`, `InventoryMeta`, `InventoryRecord`)
  - `crate::config::ScanConfig`
  - `crate::winapi_wrappers` (volume_for_path, UsnEnum, query_usn_journal_state, read_usn_journal_delta, fetch_reparse_tag, FileRef)
  - `crate::exclusions` (Decision)
  - `crate::pipeline::SkippedFile`
  - `rayon`, `hashbrown`, `parking_lot`, `serde`, `tracing`, `windows` crate

## Refactor Hints
- **Suspect dead code: the recursive `walk()` (walk.rs:509) and `walk_fast_path()` (walk.rs:813)**. Confirmed via `grep -n "^fn walk\b\|walk_fast_path"` — `walk()` is only called recursively (lines 681, 891, 964) and from `walk_fast_path`; `walk_fast_path` is only called from `walk()` (line 557). The live path is `walk_one_root_buffered -> walk_bfs -> enumerate_one_folder_pure -> {enumerate_one_folder_fast_path | enumerate_one_folder_read_dir}`. This is ~500 lines of recursive-walker code that can be deleted, with a careful read for any subtle semantics drift between the two implementations (they're documented as mirrors, but worth diffing the filter-ladder ordering).
- **Two `reconstruct_path` implementations (mft.rs:426 + warm.rs:347)** with a documented invariant that they must stay byte-identical. Candidate for a shared helper in either `mft.rs` or a new `path_reconstruct` submodule.
- **Two `under_any_root` + `path_passes_globs` pairs** (mft.rs:506/519 + warm.rs:487/495). Trivial dedup target.
- **`_UNUSED_HOOK` const in dir_enum.rs:176** — silences an unused-imports lint but is fragile; consider `#[allow(unused_imports)]` on the import line instead.
- **`PathNotFound` returned by `walk_one_root_buffered` after pushing a `DirError` event** is double-signaling — driver throws away the event by `?`-propagating the error; intentional? worth a comment.
- **placeholder.rs's reparse-tag constants (`IO_REPARSE_TAG_DEDUP` etc.) duplicated** in `walk.rs` (line 832 `IO_REPARSE_TAG_SYMLINK`, 0x400 attribute bit scattered in multiple files). Single consts module under `inventory::reparse_tags` would help.

## Wire Surfaces
- **Filesystem syscalls (Windows)**: `FSCTL_ENUM_USN_DATA`, `FSCTL_READ_USN_JOURNAL`, `FSCTL_QUERY_USN_JOURNAL`, `GetFileInformationByHandleEx(FileIdBothDirectoryInfo|FileIdInfo)`, `CreateFileW` with `FILE_FLAG_BACKUP_SEMANTICS`.
- **CLI flags consumed (via `ScanConfig`)**: `--force-mft`, `--follow-links`, `--no-cache`, `--min-size`/`--max-size`, include/exclude globs, `parallel_roots`.
- **JSON `skipped[]` array** in scan output: placeholder snake-case strings (`recall_on_open`, `recall_on_data_access`, `reparse_dedup`, `other_reparse`) plus `symlink_target_unreadable`. The string contract is enforced by `pipeline::SkippedFile::from_state` + `PlaceholderState::Display`.
- **On-disk format**: `InventoryMeta { journal_id, last_usn, captured_at_unix }` + `InventoryRecord { parent_ref, usn, attributes, name, size, mtime, reparse_tag }` (stored via `Cache::save_inventory_snapshot` / `apply_inventory_delta`).
- **Reparse-tag classification constants** (placeholder.rs `classify`): `IO_REPARSE_TAG_DEDUP=0x80000013`, `IO_REPARSE_TAG_CLOUD_RECALL_ON_OPEN=0x9000001A`, `IO_REPARSE_TAG_CLOUD_RECALL_ON_DATA_ACCESS=0x9000101A`.
