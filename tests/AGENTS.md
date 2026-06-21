# tests — AGENTS guide

## Purpose
This directory holds the superdeduper **integration tests** (Rust's `tests/` convention — each `.rs` is its own crate, linking only against the public `superdeduper` library + `superdeduper` binary). They exercise end-to-end behaviours that the in-crate unit tests under `src/` cannot reach cleanly: real filesystem corpora, GUI-headless drives via `egui_kittest`, cross-stack crypto goldens for the V3.1 mutate-bench wire format, property-based fuzz, and cancellation/resume round-trips.

Most tests build a temp-directory corpus, configure a `ScanConfig` (or boot `SuperdeduperApp` for the GUI cells), run the pipeline, and assert structural invariants (recall=1.0, precision=1.0, keeper preserved, refusal on aliases, byte-exact goldens). A subset are platform-gated (`#![cfg(unix)]`, `#![cfg(windows)]`, `#![cfg(target_os = "linux")]`) and a subset are feature-gated (`#![cfg(feature = "gui")]`, `#![cfg(feature = "telemetry")]`).

The fixtures subdirectory carries a single binary asset: a pre-built v6-schema sqlite cache, used as the input to the v6 -> v7 invalidation regression.

## Files

### `akp_gui_linux.rs`
Adversarial Keeper-Preservation — Linux leg of #153 Tier C. Drives `gui::app::run_one_dedupe_action` (the per-file dispatch the GUI worker calls) with keeper-ALIASES (identical-path, symlink, hardlink) as the destructive target across `Remove/Recycle/SafeRename`. Asserts refusal + keeper byte-survival + alias-name survival, plus a negative control that distinct files DO get actioned, and that `Hardlink` on an already-hardlinked alias is a no-op Ok. NTFS counterpart is `akp_gui_ntfs.rs`.
- Feature gate: `#![cfg(feature = "gui")]`
- Calls: `superdeduper::gui::app::run_one_dedupe_action`, `superdeduper::cli::DedupeAction`
- Helpers: `tmpdir`, `assert_refused`, `assert_distinct_actioned`; trailing `_path_marker` is intentionally `#[allow(dead_code)]`

### `akp_gui_ntfs.rs`
NTFS leg of the same #153 Tier C surface. Tests \\?\ verbatim, 8.3 short-name, junction, case-variant aliases all refuse the keeper through the GUI dispatch seam. Adds v0.2.36 action-layer relocations: system-path guard cells (C:\Windows, C:\Program Files, junction-alias-to-system), allow-AppData/TEMP narrowing cells, reference-root guard cells, plus an A5 negative-control "unprotected distinct dedup must succeed."
- Feature gate: `#![cfg(all(windows, feature = "gui"))]`
- Calls: `gui::app::run_one_dedupe_action`, `cli::DedupeAction`
- Uses `cmd /c mklink /J` for junctions and `for %I in ... %~sI` for 8.3 lookup

### `cache_corpus_reset.rs`
Regression for GH #36 — engine cache hygiene under a `rm -rf corpus + cp -a pristine corpus` reset that preserves mtime but allocates fresh inodes. The actual cache key is `(volume_guid, file_ref)`, NOT `(path, mtime)` as initially mis-diagnosed; this test pins that the v6/v7 cache returns no stale rows after the inode flip. Skipped on CI (the GH ubuntu-latest runner has been observed reusing inode numbers).
- Calls: `superdeduper::cache::Cache::open`, `inventory::enumerate`, `pipeline::grouping/layout/hash::run_with_counters`
- CI skip: `if std::env::var("CI").is_ok()` returns early

### `cache_v6_invalidation.rs`
T2.1 criterion #9 — opening a v6 sqlite via `Cache::open` must drop owned tables and recreate them under v7 (invalidation-rebuild, NOT in-place migration). Reads the v6 fixture from `tests/fixtures/v6_cache.sqlite`, calls `Cache::open`, then re-opens with raw rusqlite to verify `meta.schema_version = 7`, `inventory_records.reparse_tag` column exists, and all owned tables are empty.

### `channel_config_precedence.rs`
Hermetic test for GH #30 (deferred layer-3 AC of #13) — channel resolution precedence CLI > ENV > config.toml > prod-default. Per-test `XDG_CONFIG_HOME` tempdir + `SUPERDEDUPER_CHANNEL` reset. Process-wide `SERIAL: Mutex<()>` guards parallel env mutation. Also pins malformed-config-toml and unknown-slug to error (not silent fallback).
- Platform gate: `#![cfg(target_os = "linux")]` — macOS `config_dir()` ignores `XDG_CONFIG_HOME`, Windows uses `APPDATA`
- Calls: `superdeduper::channel::{resolve_active_channel, Channel, ENV_VAR}`

### `gui_tier_a_linux.rs`
The largest file (1231 lines). Tier-A GUI operation driver — drives the REAL `SuperdeduperApp` via `egui_kittest::Harness::build_eframe`, no wgpu / no headless render. Covers boot, scan-to-table, every destructive bulk action (Nuke/Recycle/Safe-rename), Hardlink per-row, reference protection, system-path alias refusal, G-SUBMIT dispatch (telemetry-gated), scan-complete modal stats, post-action `actually_reclaimed_bytes` patching, plus the **scan-perf ratio cell** (the v0.3.36+ ship gate). Includes two corpus generators: `generate_perf_test_corpus` (1040-file synthetic, env-scaled) and `generate_mick_shape_corpus` (50K-100K production-shape, cached on disk).
- Feature gate: `#![cfg(feature = "gui")]`; sub-cells gated by `#[cfg(feature = "telemetry")]` and `#[cfg(unix)]`
- Process-wide `env_lock()` serializes XDG / HOME mutation across tests
- Hermetic isolation via `SUPERDEDUPER_TEST_DATA_DIR` (cross-platform) + `XDG_*` (Linux) — both honored by install/cache/scan-history/checkpoint resolvers
- Env knobs: `SUPERDEDUPER_TEST_PERF_CORPUS_SCALE`, `SUPERDEDUPER_TEST_PERF_MICK_SHAPE_SCALE`, `SUPERDEDUPER_TEST_PERF_RATIO`
- `generate_mick_shape_corpus` is `#[allow(dead_code)]` — used by a separate Mick-shape cell sdd-testwin authors

### `properties.rs`
Property-based correctness (proptest). Plants random universes of files with known equivalence classes; asserts (1) recall=1.0 / precision=1.0 against an oracle, (2) thread-invariance (`threads=1` vs `threads=4` produce identical group sets), (3) min-size filter discipline, (4) unique inputs never produce groups. Canonical-set comparison via `BTreeSet<BTreeSet<PathBuf>>`.
- Calls: `inventory::enumerate`, `pipeline::grouping::group_by_size`, `pipeline::layout::resolve`, `pipeline::hash::run`
- proptest config: 32 cases for the main test, 16 for the negative

### `reference_keeper_invariant_linux.rs`
A-ref-keeper regression (Mick 2026-05-30): in a multi-root scan with one reference root, every emitted group MUST place a reference-root file at `files[0]`. Three cells: reference added last + non-ref roots listed first (Smart strategy would otherwise pick deepest-newest); reference added first (regression against "files[0] = first walker emission" mask); no-reference-root negative control (must still emit groups, no false fail-closed).
- Feature gate: `#![cfg(feature = "gui")]`
- Drives `gui::live::spawn_with_settings` + collects `EngineEvent::DuplicateFound`
- Process-wide `env_lock()` for XDG mutation

### `resumability.rs`
Checkpoint persistence round-trip (Pause -> Resume contract). Exercises `gui::checkpoint::{save, load, summary, delete, mark_corrupt}` against a sample `Checkpoint` with two `DuplicateGroupSummary` records + a `SavedFileEntry`. Pins: missing file -> Ok(None) (not Err); corrupt JSON -> Err; default `ScanSettings` round-trips identically (catches future fields added without `#[serde(default)]`).
- Feature gate: `#![cfg(feature = "gui")]`

### `scan_resume_e2e.rs`
End-to-end resume regression for Mick's `D:\sdd-tests` report — cancelling mid-Tier-3 must persist a checkpoint (not be swallowed by `io::Error::Interrupted("cancelled")` short-circuiting the chunks-loop `?` operator). Two cells: `cancel_mid_hash_writes_checkpoint` (asserts ScanPaused + checkpoint on disk + saved_inventory populated); `resume_after_cancel_replays_checkpoint` (run 1 cold-cache writes; run 2 must hit the cache > 0 times — same path resume relies on).
- Feature gate: `#![cfg(feature = "gui")]`
- Process-wide `SERIAL: Mutex<()>` (test-A's env_cache_home would race test-B's)
- 4 MiB files via `make_corpus` to ensure Tier-3 streaming hasher fires

### `smoke.rs`
End-to-end smoke. Three cells: `finds_planted_duplicates` (3 dup + 1 unique in nested dirs); `empty_directory_yields_no_groups`; `single_file_positional_arg_combines_with_dir_root` (GH #32 regression — `walk()` used to silently zero-out single-file positional args). Cross-platform (no feature gate).
- Calls: full pipeline (`inventory::enumerate`, `grouping::group_by_size`, `layout::resolve`, `hash::run`)
- Name comparison via lowercased basenames (handles NTFS case quirks + `\\?\` verbatim returns from canonicalize)

### `symlink_loop_detection.rs`
T1.7 — `walk::enumerate_cancellable` visited-set / cycle detection. Cells for two-step / self / 3-hop cycles, linear chain (no cycle), follow-links-off (no cycle events at all), and scan-reset isolation (consecutive scans each start with empty visited set).
- Platform gate: `#![cfg(unix)]` — relies on `std::os::unix::fs::symlink`; Windows junction path exercised at runtime, not in test
- Drives `enumerate_cancellable` directly with a `WalkEvent` callback

### `v31_goldens.rs`
V3.1 mutate-bench cryptographic golden vectors — 20 deterministic corpora, byte-exact lock for `rep_hash` (tag 0x05) and `result_digest_v3.1` (domain `tcorpus-result-v3.1`). Two top-level tests: `v31_goldens_locked_byte_exact` (locks reference impl in this file against pinned hex); `v31_engine_primitives_lock_against_goldens` (locks the SHIPPED engine impls `bench_client::rep_hash_v3_1` + `result_digest_bytes_v3_1` against the same hex). Plus unit-level binding tests (path_index / file_size / K / domain-separation) and pairwise-distinct / no-internal-collision sanity asserts.
- Feature gate: `#![cfg(feature = "telemetry")]`
- Calls: `leaderboard::bench::{content_bytes_at, corpus_keys}`, `leaderboard::bench_client::{mutate_bytes_v3, per_file_key_v3, rep_hash_v3_1, result_digest_bytes_v3_1}`
- "LOCK" tombstone pattern: an un-pinned vector prints its computed hex on first run; maintainer pastes back

### `walker_fast_path.rs`
Block N walker smoke. Generic cells (find basic files, recurse subdirs, apply min-size); Windows-only cell asserts the fast-path populates `file_ref`, `volume_guid`, and `attributes` (the FileIdBothDirectoryInfo win — Stage 2b skip relies on these). Cross-platform (no feature gate); fast path is `#[cfg(windows)]` only.

### `fixtures/v6_cache.sqlite`
Pre-built v6-schema sqlite. Read-only fixture for `cache_v6_invalidation.rs`. **Not a source artifact** — do not edit; if v6 schema details ever need to change, regenerate from an explicit v6 build.

## Invariants / Gotchas

- **Env-var serialization.** Several tests mutate `XDG_*`, `HOME`, `APPDATA`, `SUPERDEDUPER_CHANNEL`, `SUPERDEDUPER_TEST_DATA_DIR`, `XDG_CACHE_HOME`, `CI`. Cargo runs tests inside one binary in parallel by default. Each affected file has either a process-wide `static SERIAL: Mutex<()>` (channel_config_precedence, scan_resume_e2e) or an `env_lock()` (gui_tier_a_linux, reference_keeper_invariant_linux). Add new tests in those files to existing locks.

- **Hermetic XDG isolation matters on Windows.** `SUPERDEDUPER_TEST_DATA_DIR` is the cross-platform escape hatch — without it, real `%LOCALAPPDATA%\superdeduper` state leaks into tests on sdd-testwin (stale settings, dismissed alpha modal, leftover checkpoint -> the 8/9-cells-fail-on-Windows artifact). The engine's install/cache/scan_history/checkpoint resolvers all honor it first.

- **`harness.step()` vs `harness.run()`.** `egui_kittest::Harness::run()` panics with `exceeded max_steps` whenever a spinner / continuous-repaint widget is active (preflight probe + live-scan animations). All long-running cells use `harness.step()` in a loop. See comments in `gui_tier_a_linux.rs`.

- **`click_all` rather than the first label match.** kittest's first label match for a control can be a non-interactive label / tooltip; clicking it is a no-op. `click_all` iterates every match — relies on the label substring being unique to one control.

- **V3.1 goldens are byte-exact load-bearing.** Any change to `rep_hash` body or `result_digest_v3.1` framing in `src/leaderboard/bench_client.rs` will break `v31_engine_primitives_lock_against_goldens`. The pinned hex in this file is canonical; if drifting intentionally, write the new spec into design first, then update both the engine impl AND the hex here in lockstep.

- **`cache_corpus_reset.rs` requires fresh inodes.** Skipped on CI because ubuntu-latest reuses inode numbers; the precondition `(inode_a_before, inode_b_before) != (after, after)` would otherwise fail with the test reporting "invalid — re-run".

- **`unsafe std::env::set_var` discipline.** Marked unsafe in newer Rust editions. All call sites are documented as single-threaded under the file's `SERIAL` / `env_lock` — preserve that invariant on any new tests.

- **`ScanConfig` literal duplication.** Every test that builds a `ScanConfig` directly (smoke, walker_fast_path, properties, cache_corpus_reset, symlink_loop_detection) duplicates the full struct literal. Adding a new field to `config::ScanConfig` requires touching all of them. Consider a `tests/common.rs` helper at some point.

## Dependencies

- **INCOMING**: nothing — `tests/` is the top of the test-target graph. Run via `cargo test` (cross-platform set) or with `--features gui` / `--features telemetry` to unlock the gated cells.
- **OUTGOING**: `superdeduper` crate (public API only); `proptest`, `tempfile`, `parking_lot`, `crossbeam_channel`, `rusqlite`, `blake3`, `accesskit`, `egui_kittest`, `humansize` (dev-dependencies in workspace `Cargo.toml`).

## Refactor Hints

- **`ScanConfig` builder.** The literal struct duplication across 5+ files is the single largest invitation to write a `tests/common/mod.rs` (or `tests/common.rs`) helper. Currently every change to `ScanConfig` is a five-file edit.

- **`env_lock` duplication.** `gui_tier_a_linux.rs::env_lock`, `reference_keeper_invariant_linux.rs::env_lock`, `channel_config_precedence.rs::SERIAL`, `scan_resume_e2e.rs::SERIAL` all implement the same poison-tolerant process-wide lock. A shared `tests/common/env_lock.rs` would dedup ~40 lines.

- **`generate_mick_shape_corpus` callers.** Declared `#[allow(dead_code)]` in `gui_tier_a_linux.rs` line 975 with comment "Used by Mick-shape cell which sdd-testwin authors separately." Confirmed no callers via grep within this file. If the sdd-testwin cell has shipped, the allow can be removed; if not, this is a parked helper.

- **`_path_marker` in `akp_gui_linux.rs`** (line 169) — explicitly `#[allow(dead_code)]`. Looks like a leftover marker; safe to delete unless used as an anchor for editor jumps.

- **`CancelTrigger` enum** (`scan_resume_e2e.rs` line 132) is declared `#[allow(dead_code)]` with only one variant `OnStatusContains`. If no other trigger shape ships in the next test, collapse to a tuple struct or a function pointer.

- **Cell-by-cell `std::fs::remove_dir_all(...).ok()` at the end of every test.** Could be a `Drop` guard on a test-local `TempCorpus` type — but `tempfile::TempDir` already does this; non-`tempfile` paths use manual cleanup. Low priority.

## Wire Surfaces (if any)

- **No HTTP endpoints owned here** — tests consume them (telemetry submit, leaderboard) only indirectly via `gui_tier_a_linux::tier_a_g_submit_dispatches_from_scan_complete_modal`, which deliberately stops at dispatch (Submitting state) without a real POST.
- **On-disk format versions**:
  - `tests/fixtures/v6_cache.sqlite` — the v6 sqlite schema. The test asserts `meta.schema_version` transitions to "7" and `inventory_records.reparse_tag` exists post-open.
  - V3.1 wire format goldens — `rep_hash` body (tag 0x05 || u64le(rep_pi) || u64le(size) || mutated || K) and `result_digest_v3.1` framing (u32le(19) || "tcorpus-result-v3.1" || K || u64le(n_groups) || per-group...). The pinned hex is the cross-stack lock — engine, eventual TS web verifier, and testrunner all must agree byte-for-byte.
- **Environment variables read by tests**:
  - `CI` (cache_corpus_reset — skip gate)
  - `SUPERDEDUPER_CHANNEL`, `XDG_CONFIG_HOME` (channel_config_precedence)
  - `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `HOME`, `SUPERDEDUPER_TEST_DATA_DIR` (gui_tier_a_linux, reference_keeper_invariant_linux, scan_resume_e2e)
  - `SUPERDEDUPER_TEST_PERF_CORPUS_SCALE`, `SUPERDEDUPER_TEST_PERF_MICK_SHAPE_SCALE`, `SUPERDEDUPER_TEST_PERF_RATIO` (gui_tier_a_linux scan-perf cell)
  - `APPDATA` (akp_gui_ntfs `allow_appdata_roaming`)
  - `CARGO_BIN_EXE_superdeduper` (gui_tier_a_linux scan-perf cell — Cargo-set, points at the built CLI)
  - `CARGO_MANIFEST_DIR` (cache_v6_invalidation — resolves the fixture path)
- **No CLI flags owned by this dir** — tests construct `ScanConfig` directly or drive the GUI; `--channel` / `--no-cache` are tested but defined in `src/cli.rs`.

## Non-source artifacts

- `fixtures/v6_cache.sqlite` — 57 KiB pre-built sqlite database in v6 schema. Read-only fixture for `cache_v6_invalidation.rs`; not regenerated by any test in this directory.
