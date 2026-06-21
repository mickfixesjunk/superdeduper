# gui — AGENTS guide

## Purpose

`src/gui` is the desktop GUI layer of superdeduper. It is built on egui/eframe
and pairs a single-threaded UI state model (`state::UiState`) with a
crossbeam-channel event stream produced by the scan engine running on its own
thread. The render loop drains events each frame and mutates UI state with no
locking.

This directory owns the application shell (`app.rs`), the engine-to-UI bridge
(`live.rs`), the event wire format (`events.rs`), the persistence layer for
pause/resume + cross-session results (`checkpoint.rs`, `results_store.rs`,
`project.rs`, `archive.rs`, `drive_overrides.rs`), modal state machines
(`preflight.rs`, `resume_tier.rs`, `resubmit.rs`), the dark-CRT visual theme
(`theme.rs`), self-debug diagnostics (`diagnostics.rs`), the resume
catch-up effect (`particles.rs`, `sound.rs`), keyboard-shortcut surface
(`accessibility.rs`), and per-channel perf instrumentation
(`perf_channel.rs`). Subdirectories (`widgets/`, `preview/`) host the
individual panels and the preview pane respectively, and are audited
separately.

The `SuperdeduperApp` exported from `app.rs` is the only public entry point
the binary crate consumes.

## Files

### `mod.rs`
Module root. Wires every submodule. `pub use app::SuperdeduperApp;` is the
single re-export. Two cfg gates: `resubmit` requires `feature = "telemetry"`;
`sound` requires `feature = "audio"`. Test-only `theme_snapshot_test` is gated
on `cfg(test)`.

### `accessibility.rs`
Keyboard-shortcut catalog + dispatch consumed once per frame by `app.rs`.
Provides `Shortcuts` (constant chord catalog), `AccessibilityAction` enum
(`StartScan` / `CancelScan`), `consume_pressed_shortcuts(ctx)`, and
human-readable label helpers (`shortcut_label_start_scan`,
`shortcut_label_cancel_scan`). The whole layer is a no-op overlay on top of
egui's existing click handling — its real role is providing a SendKeys path
for `sdd-testwin`'s UIA harness (Ctrl+R / F5 = Start scan, Esc = Cancel).
Has full unit-test coverage for the chord uniqueness invariant.

### `app.rs`
The `eframe::App` implementation. Owns `SuperdeduperApp` (~50 fields), the
File-menu state machine (`MenuAction`), per-modal render flow
(`Flow::Continue` / `Flow::Return`), and the central `update()` loop.

Public surface:
- `pub struct SuperdeduperApp` + `impl eframe::App`
- `pub fn run_one_dedupe_action(action, target, keeper, references) -> Result<()>` —
  keeper-safety SEAM that every GUI destructive action funnels through.
  Exposed for testdesign's egui_kittest cells.
- `pub(crate) fn perf_instrument_update_enabled()` — gated by
  `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE` env var.
- `pub(crate) fn perf_instrument_chunk_emit_enabled()` — gated by
  `SUPERDEDUPER_PERF_INSTRUMENT_CHUNK_EMIT` env var.

Critical internal methods:
- `new()` — boots the app, probes the on-disk checkpoint summary, classifies
  the resume tier, spawns the catalog fetch + history prune.
- `accept_resume()` / `apply_resume_hydrated()` — the #99 PR1 split between
  spawning a worker for disk I/O and applying the loaded bundle on the UI
  thread.
- `start_live()` → `launch_scan()` → `crate::gui::live::spawn_with_settings`.
  Routes through `detect_settings_drift()` (#51 modal) and `preflight` modal.
- `drain_events()` — the per-frame event-loop integrator. Bounds the drain
  to 512 events per frame. Special-cases ScanStarted / ScanFinished /
  ScanPaused / ResumeHydrated / Archive+Dedupe summaries / DriveDiscovered.
- `persist_results_after_scan()` — writes `results_store::save()` so safe-
  rename resumes after restart.
- The 12 `render_*` modal fns implementing the #140 modal-extraction pattern.

Environment variables read here:
- `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE` (cached via `OnceLock`)
- `SUPERDEDUPER_PERF_INSTRUMENT_CHUNK_EMIT` (cached via `OnceLock`)
- `SUPERDEDUPER_SKIP_ACCESSKIT_DURING_SCAN` (cached via `OnceLock`)
- `SUPERDEDUPER_PERF_SKIP_SIDEBAR_DURING_SCAN` (cached via `OnceLock`)

Feature gates: `telemetry` cfg-gates the bench modal, scan-complete modal,
post-scan submission flow, badge-multiplier detail modal, and resubmit prompt.

### `archive.rs`
"Archive Dupes" manifest format + restore helpers. Defines
`ArchiveManifest` (schema-versioned JSON), `ArchiveManifestEntry`,
`RestoreOutcome` (Restored / ArchivedMissing / OriginalExists / IoError),
`RestoreSummary`, `ArchiveActionSummary` (#80 Bug C rollup with three
failure buckets), and `ArchiveFailureBucket`. The
`SchemaVersioned` impl parses the combined `name.vN` form so old v1
manifests still load via `crate::schema::check`.

Public fns: `load_manifest(path)`, `restore_one(entry)`.
Public consts: `ARCHIVE_SCHEMA = "superdeduper.archive.v1"`.

### `checkpoint.rs`
Pause/resume artifact written to `%LOCALAPPDATA%\superdeduper\scan-checkpoint.json`.

Public types: `SavedFileEntry`, `Checkpoint` (carries the v0.3.42 cumulative
counters `cumulative_bytes_scanned`, `cumulative_files_scanned`,
`cumulative_wall_clock_seconds` for chain-cumulative submission payloads),
`CheckpointSummary`, `SaveWorker`, `SaveWorkerHandle`.

Public fns: `default_checkpoint_path()`, `load(path)`, `save(path, cp)`,
`summary(path)`, `archive(path)` (rename to `.json.bak`), `mark_corrupt(path)`
(rename to `.json.corrupt`), `delete(path)`.

`SaveWorker` (v0.3.41 Phase 4) runs `checkpoint::save` on a background
thread off the chunk-loop critical path. v0.3.42 Phase 11c extension adds
`attach_live_state(Arc<Mutex<Checkpoint>>)` for the 1000ms timer-driven
auto-flush. Single-slot replace-on-enqueue semantics; explicit enqueue takes
priority over the live-state snapshot; shutdown drains the final pending +
live-state snapshot.

### `diagnostics.rs`
Per-scan self-debug log written to `diagnostics/report-<uuid>.txt`. The
sampler thread snapshots `EngineCounters` every 10s into `[STATE]` lines.

Public types: `EngineCounters` (cross-thread `AtomicU64` counters; `Arc`-wrapped
sub-counters shared with the hashing workers), `DiagnosticsLog`.

Public fns: `DiagnosticsLog::open()` → `Option<Arc<Self>>` (non-fatal on
failure), `log()`, `log_hash_failure()` (caps at 50 samples), `finalize()`
(closes file, renames to include duration suffix). `spawn_state_sampler()`
runs the bg thread.

Reads env var `SUPERDEDUPER_DIAGNOSTICS_DIR` to override the output
directory (default `diagnostics/` under cwd).

### `drive_overrides.rs`
Persisted HDD/SSD render override per volume GUID. Stored at
`%LOCALAPPDATA%\superdeduper\drive-overrides.json` (atomic write).

Public: `DriveOverrides` struct (schema + `HashMap<String, bool>`), const
`OVERRIDES_SCHEMA = "superdeduper.drive-overrides.v1"`,
`overrides_path()`, `load()`, `set(volume_guid, value)`.

Override-bool semantics: `Some(true)` = force SSD, `Some(false)` = force HDD,
missing key = use auto-detection. Volume GUID (not drive letter) is the
stable key across external-drive replug cycles.

### `events.rs`
Engine-to-UI wire format. Single `EngineEvent` enum + named structs.

Public types: `DriveId` type alias, `Stage` enum (8 variants + `ALL` const +
`label()` + `label_with_algo()`), `DriveInfo`, `ReadSample`,
`DuplicateGroupSummary` (with `link_equivalent`, `unique_inodes`,
`similarity_kind` fields all `#[serde(default)]` for old-checkpoint
compatibility), `LogLevel`, `EngineEvent`, `ResumeHydrateOutcome` (#99 PR1),
`FileActionOutcome` (#83), `OverallStage`.

Event variants include `DuplicatesFoundBatch(Vec<...>)` (v0.3.40 batching),
`ArchiveActionSummary` (#80 Bug C), `DedupeActionSummary` (#79),
`ResumeHydrated` (#99 PR1), `FileActionCompleted` (#83). The full enum is
exhaustively matched by `state::UiState::apply`.

### `live.rs`
The engine-side bridge. Spawns a dedicated worker thread and runs the full
scan pipeline (inventory → size-group → layout → tier 0–3 → confirm),
emitting `EngineEvent`s into the `PerfTx` channel.

Public fns:
- `spawn(tx, roots)` — legacy `--live` CLI entry point.
- `spawn_with_settings(tx, roots, settings, cancel, defender_rtp_pre,
  scan_mode, image_similarity_threshold, image_hash_algorithm,
  audio_similarity_threshold)` — the production path.
- `volume_guid_for(path) -> Option<String>` — Windows: queries
  `winapi_wrappers::volume_for_path`; non-Windows: `None`.

Internal `run()` is the heart of the engine-side: pre-normalizes reference
roots once (#191), seeds Checkpoint state, wires the SaveWorker auto-flush,
drives the rayon hashing par_iter, emits per-stage ticks, handles cancel +
pause snapshots, and tallies cumulative counters across resume sessions.

`detect_seek_penalty(path)` is the cross-platform HDD/SSD probe: Windows
uses IOCTL via `winapi_wrappers`; macOS shells out to `/usr/sbin/diskutil`
and parses "Solid State: Yes/No" (#158); other Unix defaults to HDD.

### `particles.rs`
Resume cache-fast-forward sparkle effect anchored to the progress bar.
Public type `Sparkles` with `tick()`, `is_fast_forwarding()`,
`force_catch_up()`, `paint()`, `reset()`, `active()`. `SparkleSignals`
emitted by `tick()`. Hard 3s ceiling via `FF_MAX_DURATION_SECS`. Strictly
gated by `app.resume_effect_active` upstream; the rate-spike heuristic
alone is not sufficient.

### `perf_channel.rs`
Instrumentation for the scan-worker → GUI crossbeam channel
(A-perf-channel-h2). `PerfTx` wraps `Sender<EngineEvent>` with per-send
timing; static `AtomicU64` counters track tx/rx counts, contended sends,
dropped sends, queue depth samples. `drain_and_format()` snapshots and
resets counters once per emit cycle, returning a single perf-channel line
when the window has any activity.

Activation is the same `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1` env var that
gates `perf-update` / `perf-chunks` / `perf-streaming`.

### `preflight.rs`
Slice-1 pre-flight diagnose modal state. Public types: `PreflightState`
(Idle / Probing / Showing / Failed), `Grade`, `AxisScore`, `DiskAxis`,
`DriveScore`, `PreflightAction`. Public fns: `spawn_probe(roots)`,
`grade_report(r)`. Reference targets: `HARDWARE_REF_MBPS = 20_000`,
`DISK_REF_MBPS = 3_000`, `HASH_REF_MBPS = 50_000`. Letter grade thresholds
follow the standard A/B/C/D/F percent buckets.

### `project.rs`
`.superdeduper` bundle persistence. Public types: `ProjectFile`,
`ProjectStats`, `DuplicatesFile`, `RecentProjects`, `RecentProject`. Public
consts: `PROJECT_SCHEMA`, `DUPLICATES_SCHEMA`, `PROJECT_SUFFIX`. Public fns:
`save(dir, name, created_at_unix, roots, settings, duplicates)`,
`load(dir)`, `default_bundle_name(roots)`, `recents_path()`, `load_recents()`,
`touch_recent(path, name)`.

Reclaimable bytes persisted in `ProjectStats` are inode-aware via
`crate::gui::state::inode_aware_savings` so hardlink-heavy corpora
(C:\Windows / WinSxS) don't inflate the figure 4x.

### `resubmit.rs`
Feature-gated (`#![cfg(all(feature = "gui", feature = "telemetry"))]`).
Single-flight process-wide worker for History-panel resubmits. Public fns:
`request_resubmit(scan_id)`, `request_resubmit_batch(scan_ids)`,
`in_flight_scan_id()`, `drain_outcome()`. Uses two `OnceLock<Mutex<...>>`
static slots for in-flight + last-outcome.

### `results_store.rs`
Cross-session persistence of the most recent scan's confirmed-duplicates
list at `%LOCALAPPDATA%\superdeduper\results-state.json`. Used by Safe-rename
resume. Public types: `RootFingerprint`, `ResultsState`. Public fns:
`default_results_state_path()`, `save(state)`, `load()`, `delete()`,
`fingerprint_root(root)`, `load_matching(roots, settings)`.

The `load_matching` tolerance is 0.5% on `(file_count, sum_size)` plus an
exact match on `max_mtime_unix`. Drift beyond that returns `None` so the
caller re-scans.

### `resume_tier.rs` (#99 PR2)
Pure-function classification of resume scenarios. Public types:
`ResumeTier` (Full / Warm / InventoryOnly / Marker / Fresh), `SessionContext`.
Public fns: `classify_resume_tier(prior, current)`,
`classify_resume_tier_from_summary(summary, current)`,
`roots_match_canonical(a, b)` (case + trailing-slash-tolerant guard, A14).

Tier semantics + reuse predicates: `Full`/`Warm` restore previous
duplicates; `Full`/`Warm`/`InventoryOnly` reuse the saved inventory.

### `sound.rs`
Feature-gated (`#[cfg(feature = "audio")]` at the mod-level reference).
Synth-only — no embedded WAVs. Public fns: `play_done_chime()` (perfect-fifth
C5→G5), `play_fastforward_start()` (dystopian swell), `play_caught_up()`
(metallic hit). All three spawn detached threads and silently swallow
audio-device failures.

### `state.rs`
Live UI state. Single-threaded; the render thread reads, the
`drain_events` loop mutates. Public types: `RootEntry`, `ScanSettings`,
`LogEntry`, `ActionState`, `CacheVolumeSummary`, `UiState`, `OverallProgress`,
`Totals`, `StageCounter`, `DriveLive`. Public consts:
`THROUGHPUT_WINDOW_SECS = 30.0`.

Public fns: `inode_aware_savings(group) -> u64`,
`apply_file_action_to_duplicates(duplicates, src, outcome) -> Option<usize>`
(#114 — returns `None` when no group matched, `Some(0)` matched-no-group-removed,
`Some(1)` matched-and-removed), `fmt_wallclock(d) -> String`.

`ScanSettings` carries a deprecated `paranoid: bool` field (#131) with
`#[serde(default, skip_serializing)]` — left in place so old persisted
configs still deserialize.

### `theme.rs`
Dark oscilloscope palette. Public color consts (`BG`, `PANEL`, `PANEL_DEEP`,
`TEXT_HI`, `TEXT_LO`, `ACCENT`, `ACCENT_DIM`, `WARN`, `HOT`, `COOL`, `HDD`,
`SSD`), `STAGE_COLORS: [Color32; 8]`. Public fns: `install(ctx)` (locks
`ThemePreference::Dark` BEFORE writing visuals to defeat egui 0.32+ system-
theme auto-flipping), `humansize(bytes) -> String`.

### `theme_snapshot_test.rs`
`#![cfg(all(test, feature = "gui"))]` only. Regression surveillance for
theme drift via `egui_kittest::Harness` rendered PNGs. Per-pixel SHA-256
against committed fixtures in `tests/fixtures/theme/`. Single-frame snapshots
+ a multi-frame stability test that simulates a mid-test system-theme flip.
All tests are `#[ignore]`'d on CI (need a wgpu adapter); run locally with
`cargo test -- --ignored`.

## Invariants / Gotchas

1. **Single-thread mutation of `UiState`.** No locking anywhere in the
   `UiState` struct. The render thread reads, the `drain_events` integrator
   writes. Any new mutator must run on the UI thread; cross-thread writes
   land through the `EngineEvent` channel.

2. **`duplicates` vec ↔ `duplicate_hashes` set must stay in lockstep.**
   Every push to `state.duplicates` requires a `duplicate_hashes.insert()`
   on the same content_hash, and every pop/remove requires the matching
   remove. The #39/#40 dedupe-on-resume relies on `O(1)` hash membership;
   the linear `iter().any(...)` it replaced made apply `O(n²)` at 26K+
   groups.

3. **`ScanStarted` preserves duplicates state for resume, NOT for fresh.**
   `state::apply(ScanStarted)` preserves `duplicates`, `duplicate_hashes`,
   `totals.duplicates`, `totals.reclaimable_bytes`, and `logs` across the
   per-scan default-init. Fresh scans MUST clear those fields in
   `app::start_live` before invoking the engine (the App side is gated on
   `!self.resume_effect_active`). Refactors that move the wipe risk
   re-introducing #99 PR5's regression.

4. **`SaveWorker` shutdown ordering.** The cancellation path in
   `live::run` must shut down the bg worker BEFORE the sync cumulative-aware
   save so the bg thread doesn't race the sync save on the same path. Both
   use atomic rename, but serialising via shutdown is the documented
   invariant.

5. **`SaveWorker::attach_live_state` lock holding.** The chunk-loop
   `record()` calls and the bg-thread snapshot must each hold the
   `Arc<Mutex<Checkpoint>>` lock for microseconds only. `checkpoint::save`
   runs OUTSIDE the lock. Refactors that copy-with-lock-held will reintroduce
   the chunk-loop blocking that v0.3.42 Phase 11c removed.

6. **Schema strings are wire surfaces.** `superdeduper.checkpoint.v1`,
   `superdeduper.archive.v1`, `superdeduper.project.v1`,
   `superdeduper.duplicates.v1`, `superdeduper.results-state.v1`, and
   `superdeduper.drive-overrides.v1` are all on-disk identifiers; bumping
   them invalidates user state. `ArchiveManifest`'s `SchemaVersioned` impl
   (#92) parses the combined `name.vN` form to join the canonical schema
   policy without rewriting old files.

7. **`Resume` modal is launch-time-only.** `pending_resume` /
   `pending_resume_tier` cycle together; both `None` after the user picks.
   `pending_drift_modal` is the mid-session settings-drift sibling and uses
   the same `CheckpointSummary` shape but a distinct user-choice flow.

8. **Resume tier classification is a pure function.** `classify_resume_tier`
   takes a loaded Checkpoint + SessionContext and returns a `ResumeTier`
   with no I/O. The drift-matrix test
   (`tests/scan_resume_drift_matrix.rs`) covers the full cross-product;
   adding a new resume axis means extending `SessionContext` + adding cells
   to the matrix.

9. **Sparkles MUST be gated by `resume_effect_active` + `OverallStage::
   Hashing`.** The rate-threshold heuristic alone false-fires during real
   hashing on small files. `app.rs::update` gates the `tick()` call; the
   first tick after `reset()` snaps the baseline so PR11's bar-jump from
   0→credit doesn't register as a rate spike.

10. **Reference paths are pre-normalized once per scan.** `live.rs::run`
    builds the `reference_set` with `strip_verbatim_prefix` applied;
    per-frame `reference_belongs(candidate, set)` normalizes only the
    candidate. Pre-#191 each frame re-stripped every reference root and
    burned CPU at 30fps.

11. **`drive_render_overrides` is NOT in `PersistedAppState`.** Drive IDs
    change between scans; the persistent layer keys on volume GUID
    (`drive_overrides::load`/`set`). Restoration happens in `drain_events`
    on `DriveDiscovered`.

## Dependencies

- **INCOMING**: only `bin/superdeduper_gui.rs` instantiates
  `SuperdeduperApp::new`. Some symbols are also reached by:
  - `src/cli/live.rs` (the `--live` CLI flag) → `gui::live::spawn` +
    `theme::install`.
  - `tests/scan_resume_drift_matrix.rs` → `gui::resume_tier::*` +
    `gui::checkpoint::Checkpoint`.
  - testdesign's egui_kittest cells → `gui::app::run_one_dedupe_action`,
    `gui::accessibility::*`.

- **OUTGOING**:
  - `crate::cache` (cache path resolution, schema state, `Cache`).
  - `crate::checkpoint` / `crate::dedupe` / `crate::diagnose` /
    `crate::dedupe::DedupeActionSummary` / `crate::inventory` /
    `crate::pipeline` / `crate::cli` / `crate::config` /
    `crate::channel` / `crate::leaderboard` / `crate::scan_history` /
    `crate::schema` / `crate::time` / `crate::winapi_wrappers` /
    `crate::platform`.
  - External: `egui`, `eframe`, `egui_extras`, `egui_kittest` (test only),
    `crossbeam-channel`, `serde` / `serde_json`, `parking_lot`,
    `hashbrown`, `globset`, `rayon`, `humansize`, `rodio` (audio feature),
    `tracing`.
  - `src/gui/widgets/*` (rendered modals + panels).

## Refactor Hints

- **Suspect dead code** (verified by `grep -rn`):
  - `gui::sound::play_fastforward_start` and `gui::sound::play_caught_up`
    have NO callers anywhere in `src/` or `tests/`. The doc-comment at
    `app.rs:4502` ("Resume catch-up sounds intentionally removed — the
    synth attempts didn't land") confirms they're disconnected by
    intent. Either resurrect them via the sparkles signal path or delete
    them with the comment block.
    Grep: `grep -rn "play_fastforward_start\|play_caught_up" src/ tests/`
  - `gui::diagnostics::DiagnosticsLog::elapsed()` has no callers; its
    doc says it's "used by `finalize`," but `finalize` reaches into
    `self.started_at.elapsed()` directly. Either route `finalize` through
    `self.elapsed()` or drop the pub fn.
    Grep: `grep -rn "DiagnosticsLog.*\.elapsed\b" src/ tests/`
  - `gui::results_store::delete` is unused anywhere in `src/` or `tests/`.
    Kept as an API contract piece (`results-state.json` cleanup hatch)
    but the comment about "deletes the stale state so the next session
    starts clean" in `load_matching` is aspirational — `load_matching`
    actually only returns `None` and leaves the file untouched.
    Grep: `grep -rn "results_store::delete" src/ tests/`

- **Persisted-config flag pollution**: `ScanSettings::paranoid`
  (state.rs:48, #131-deprecated, kept for old-config deserialize) is a
  candidate for a wider sweep once the old-format compatibility window
  closes. The serde shape (`default, skip_serializing`) means new writes
  already drop it; new readers are explicitly forbidden by the comment.

- **`apply_file_action_to_duplicates` is O(N_groups × M_files)** per
  event. The doc-block at state.rs:693 flags a `HashMap<PathBuf,
  (group_idx, file_idx)>` follow-up to make this O(1). Tracked in
  the comment as deferred from v0.2.13.1 to keep patch scope contained.

- **`live.rs` is ~3600 lines** and would benefit from extraction:
  - the seek-penalty detection block (`detect_seek_penalty`,
    `macos_seek_penalty_via_diskutil`, `parse_diskutil_solid_state`)
    is self-contained and tested.
  - `build_config` + `saved_files_from_runtime` + the inventory glue
    are independent enough to live in a `live/build.rs` sibling.

- **`app.rs` is ~5400 lines.** The #140 modal extraction has already
  pulled 12 `render_*` modal fns out of `update()`. The next batch of
  candidates: the menubar action-dispatch + the persistence helpers
  (`menu_save`, `menu_save_as`, `load_project_from`, `save_project_to`).

- **`SuperdeduperApp` field count is ~50.** Many of the `pending_*`
  modal slots could move into a `PendingModals` struct so the App
  shrinks. Caveat: cfg-gates on `telemetry` make the simple struct
  refactor a per-feature combinator.

- **Stale line refs in comments** (file paths exist, line numbers
  don't):
  - `particles.rs:124` references `app.rs:3489`. Actual sparkles tick
    site is `app.rs:4501`.
  - `events.rs:251-253` references `gui/app.rs:441`. Line 441 is now
    the unrelated `scan_history::prune_older_than` block; the
    forensic-log comment likely targeted what is now line ~600+ inside
    `accept_resume`.

- **`SaveWorker::shutdown` errors are swallowed** intentionally —
  the doc-block at checkpoint.rs:266-269 flags it. Adding a logging
  channel would require plumbing a Tx; the cancellation-path sync
  save is the durability gate, so the silent failure is by design.

## Wire Surfaces

**On-disk schemas** (under `%LOCALAPPDATA%\superdeduper\` on Windows;
XDG equivalent on Linux/macOS):
- `scan-checkpoint.json` → `superdeduper.checkpoint.v1` (pause/resume).
- `results-state.json` → `superdeduper.results-state.v1` (safe-rename
  resume).
- `drive-overrides.json` → `superdeduper.drive-overrides.v1`
  (HDD/SSD per volume GUID).
- `recent-projects.json` → no schema field; MRU list.
- Project bundle directory `*.superdeduper/`:
  - `project.json` → `superdeduper.project.v1`
  - `duplicates.json` → `superdeduper.duplicates.v1`
  - `archive-manifest.json` (optional) → `superdeduper.archive.v1`
- Diagnostics: `diagnostics/report-<uuid>.txt` then renamed to
  `report-<uuid>-<HHh-MMm-SSs>.txt`.
- Archived checkpoints: `<stem>-<ISO-timestamp>.json.bak`; corrupt
  variants: `<stem>-<ISO-timestamp>.json.corrupt`.

**eframe persistence key**: `"superdeduper.app.v1"` —
`PersistedAppState { roots, settings, results_tab }`.

**Environment variables read**:
- `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE`
- `SUPERDEDUPER_PERF_INSTRUMENT_CHUNK_EMIT`
- `SUPERDEDUPER_SKIP_ACCESSKIT_DURING_SCAN`
- `SUPERDEDUPER_PERF_SKIP_SIDEBAR_DURING_SCAN`
- `SUPERDEDUPER_DIAGNOSTICS_DIR`
- `SUPERDEDUPER_CHUNK_SIZE` (read via `live::chunk_size_max` — present
  but no longer load-bearing post-v0.3.42 single-chunk pivot)

**Keyboard shortcuts owned by this dir**:
- `Ctrl/Cmd+R` and `F5` → Start scan
- `Esc` → Cancel/Pause scan
