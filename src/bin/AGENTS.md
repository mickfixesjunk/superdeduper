# bin — AGENTS guide

## Purpose
Auxiliary binaries that live alongside the main library crate. Cargo picks each `*.rs` here up as a `[[bin]]` target. They split into two flavours:

1. The shipping desktop GUI entry point (`superdeduper_gui.rs`) — the production binary end-users launch.
2. Diagnostic / reproducer tools (`dir_probe.rs`, `hash_repro.rs`) used by engine + benchmarker workflows to investigate specific bugs and to measure hash-cost in isolation from the full pipeline.

The library crate (`superdeduper::*`) holds the actual logic; these binaries are thin drivers / harnesses against it.

## Files

### `superdeduper_gui.rs`
Production GUI launcher. Sets the Windows release subsystem to `windows` (suppresses the auto-allocated console), records the `perf_scan_lifecycle` process-start anchor before any eframe/winit init, resolves the active channel (ENV > persisted config > `prod`), builds the eframe `NativeOptions` (1440x900, vsync, app icon loaded from `OUT_DIR/app_icon.bin` written by `build.rs`), records the `perf_gui_startup` pre-run-native anchor, then hands off to `eframe::run_native` with `SuperdeduperApp`. CLI flags: `--live` plus positional `PATHS` to seed the roots panel and immediately start a scan.

- Public API: none (binary `main`).
- Who calls this: end-user / shell. Window title is `SuperDeDuper v{CARGO_PKG_VERSION} · {SD_BUILD_SHA}`.
- Key invariants:
  - `perf_scan_lifecycle::record_process_start()` MUST be the first call in `main` (before `Args::parse()`) so the TTWS anchor is accurate.
  - `perf_gui_startup::record_pre_run_native()` MUST fire immediately before `eframe::run_native` so the pre_native_ms / run_native_to_new_ms split is meaningful.
  - `SD_BUILD_SHA` and `OUT_DIR/app_icon.bin` are produced by `build.rs`; both must be present at compile time.
- Feature gates: telemetry-only startup oauth/install diagnostic block under `#[cfg(feature = "telemetry")]`. Windows release builds get `#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]`.

### `hash_repro.rs`
Standalone hash-cost reproducer. Two subcommands: `gen-corpus` builds a synthetic corpus matching a log-bucketed file-size histogram (default approximates AppData / C:\Users; `--histogram` accepts a JSON override; `--dup-ratio` / `--avg-group-size` plant byte-identical dup groups). `bench` enumerates the corpus and runs the same tier 1 / tier 2 / tier 3 sequence as Stage 4 on a named rayon pool (`hash-repro-{i}`), reporting per-tier wallclock and per-file mean cost. Uses `superdeduper::pipeline::hash::algo::{hash_oneshot, ContentHasher, HashAlgo}`.

- Public API: none (binary `main`). Internal types `Cli`, `Cmd`, `HashAlgoArg`, `Bucket`, `TierTimings`.
- Who calls this: external (engineers + river5 / benchmarker handoff).
- Key constants (mirrored by hand from `src/pipeline/hash.rs`): `TIER1_BYTES=4 KiB`, `TIER2_REGION=64 KiB`, `TIER2_MIN_FILE=256 KiB`, `TIER3_ONESHOT_THRESHOLD=1 MiB`, `TIER3_BUF=1 MiB`.
- Feature gates: none.

### `dir_probe.rs`
Diagnostic for the D:\Studio enumeration bug. Calls `std::fs::read_dir` on both the supplied path and its `\\?\`-verbatim-prefixed form (Windows only) and prints what each returns; if they disagree, the bug is in stdlib `read_dir` on verbatim paths, otherwise it is in the walker.

- Public API: none.
- Who calls this: external (manual diagnosis).
- Feature gates: verbatim-prefix branch is `#[cfg(windows)]`.

## Invariants / Gotchas
- `superdeduper_gui.rs` ordering: process-start must precede `Args::parse`; pre-run-native must immediately precede `eframe::run_native`. Reordering breaks the perf-lifecycle baselines that the v0.3.42/v0.3.43 perf work depends on.
- `hash_repro.rs` constants and tier dispatch logic must stay byte-equivalent to `src/pipeline/hash.rs` (TIER1_BYTES, TIER2_REGION, TIER2_MIN_FILE, TIER3_ONESHOT_THRESHOLD, TIER3_BUF + the "tier2 only when size >= TIER2_MIN_FILE" gate); otherwise its measurements no longer mirror Stage 4.
- `hash_repro.rs` xorshift_fill seeding is deterministic on group-id so members of the same dup group are byte-identical. Do not switch to per-file seeding without also locking sizes.
- `dir_probe.rs` only does anything interesting on Windows (the verbatim probe is `#[cfg(windows)]`).

## Dependencies
- INCOMING: cargo `[[bin]]` targets; end-users for `superdeduper-gui`, engineers for the other two.
- OUTGOING:
  - `superdeduper::gui::SuperdeduperApp`, `superdeduper::channel::*`, `superdeduper::perf_scan_lifecycle`, `superdeduper::perf_gui_startup`, `superdeduper::leaderboard::{oauth,install}` (telemetry feature).
  - `superdeduper::pipeline::hash::algo::{hash_oneshot, ContentHasher, HashAlgo}`.
  - external crates: `eframe`, `egui`, `clap`, `rayon`, `serde`, `serde_json`, `anyhow`, `hashbrown`.

## Refactor Hints
- Tier-constant duplication between `hash_repro.rs` and `src/pipeline/hash.rs` is brittle — a small `pub(crate)` "tier params" module exposed through a `pub` re-export would let the bin depend on the canonical values instead of hand-mirroring them. The header comment already flags this ("Kept in sync by hand").
- `superdeduper_gui.rs` decodes the app icon header bytes by indexing `ICON_BYTES[0..8]` — if `build.rs` ever writes a 0-byte file, this panics at startup. Adding a length check (or moving the parsing helper into `gui::` with a unit test) would harden it.
- `dir_probe.rs` is a one-shot diagnostic; if the D:\Studio bug is closed it can probably be retired (info-only).
- `hash_repro.rs` has no caller besides humans; the `TierTimings` / `bench_one_pass` split could move into `superdeduper::pipeline::hash::bench` so other benches (criterion, future regression harnesses) reuse the same tier-driver code.

## Wire Surfaces
- CLI flags owned here:
  - `superdeduper-gui`: `--live`, positional `PATHS`.
  - `hash_repro gen-corpus`: `--dir`, `--total-bytes`, `--histogram`, `--dup-ratio`, `--avg-group-size`.
  - `hash_repro bench`: `--dir`, `--hash-algo {blake3,river5}`, `--threads`, `--repetitions`.
  - `dir_probe`: positional `<path>`.
- Env / compile-time: `SD_BUILD_SHA`, `CARGO_PKG_VERSION`, `OUT_DIR/app_icon.bin` (all set by `build.rs`).
- No HTTP / JSON-schema / on-disk format surface owned by this dir.

## Non-source artifacts
None in this directory.
