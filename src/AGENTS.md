# src — AGENTS guide

## Purpose

This directory is the root of the `superdeduper` library + binary crate. It hosts the engine's process entry points (`main.rs`, `lib.rs`, `bin/`), the CLI surface (`cli.rs`), the resolved-scan configuration layer (`config.rs`), the persistent SQLite cache (`cache.rs`), the destructive-action engine (`dedupe.rs`), the diagnose subcommand (`diagnose.rs`), the per-scan history persistence (`scan_history.rs`), and a set of small leaf-modules that are deliberately kept tiny so they can be re-used from every layer without dependency cycles (`time.rs`, `path_display.rs`, `error.rs`, `schema.rs`, `keep.rs`, `output.rs`, `action_receipt.rs`, `channel.rs`, `log.rs`, `perf_gui_startup.rs`, `perf_scan_lifecycle.rs`).

`lib.rs` is the crate root that re-exports the public surface; `main.rs` is the CLI binary that dispatches `clap` subcommands into the library. The GUI binary lives under `bin/superdeduper_gui.rs` and pulls the same library symbols. Major sub-directories (`inventory/`, `pipeline/`, `gui/`, `leaderboard/`, `platform/`, `winapi_wrappers/`, `debug/`, `exclusions/`) are out of scope of this AGENTS.md and own their own docs.

The five-stage scan pipeline (inventory → grouping → layout → hashing → output) is in `inventory/` + `pipeline/`. This dir owns: the entry-point wiring, the CLI vocabulary, the persistence shapes (cache rows, scan-history rows, results-file JSON, action receipts), the cross-cutting helpers (channels, time, paths, schema), and the destructive-side gate stack.

## Files

### `lib.rs` (140 lines)
- Crate root + module tree. Re-exports `error::{Error, Result}` and the `superdeduper_log::{log_err, log_info, log_warn}` macros so older `crate::log_info!(...)` call sites resolve unchanged after the Phase 0 leaf-crate split.
- Provides `test_serial::home_env_guard()` — a process-wide mutex unit tests MUST acquire before mutating any HOME-equivalent env var (HOME, XDG_DATA_HOME, XDG_CACHE_HOME, LOCALAPPDATA, USERPROFILE). Resolves #146 (`parking_lot::Mutex` chosen because it's poison-tolerant).
- Public API:
  - `pub fn leaderboard_corpus_sig(sizes: &[u64]) -> String` — telemetry-gated corpus-shape signature (size-bucket histogram, hashed with blake3 but tagged `sha256:` in the output).
  - `pub mod ...` — every top-level module gate.
- Feature gates: `gui` (compiles `gui` module), `telemetry` (compiles `leaderboard` module and `leaderboard_corpus_sig`).
- Who calls this: external (the `superdeduper` binary in `main.rs`, the `superdeduper_gui` binary in `bin/`, the integration tests under `tests/`).

### `main.rs` (2885 lines)
- The `superdeduper` CLI binary. Parses `clap::Cli`, sets the process-global channel via `channel::set_active_channel`, then dispatches to per-subcommand functions (`run_scan`, `run_dedupe`, `run_cache`, `run_drive_info`, `run_debug`, `run_register`, `run_config`, `run_achievements`, `run_account`, `run_submit_pending`, `run_bench_me`, `run_scan_history`).
- First statement of `fn main` is `perf_scan_lifecycle::record_process_start()` so the TTWS baseline is captured before any other work.
- Non-prod footer line: every CLI command on `dev` or `local` prints `(channel: ... — submissions go to ...)` on stderr after success (suppressed under `--quiet`).
- Functions are all crate-private. Who calls this: external (the binary entry point).
- Feature gates: many subcommand dispatch arms are `#[cfg(feature = "telemetry")]`.

### `cli.rs` (1229 lines)
- Pure `clap` derive-API definitions for every CLI surface. Parsing only — dispatch is in `main.rs`.
- Public API (selected):
  - `Cli`, `Command` — top-level entry.
  - `ScanArgs`, `DedupeArgs`, `DiagnoseArgs`, `RegisterArgs`, `BenchMeArgs`, `SubmitPendingArgs` — per-subcommand argument bundles.
  - `ScanHistoryCommand`, `AccountCommand`, `NicknameCommand`, `DebugCommand`, `CacheCommand`, `ConfigCommand`, `AchievementsCommand` — subcommand enums.
  - `OutputFormat`, `KeepStrategy`, `DedupeAction`, `ScanMode`, `HashAlgoArg`, `ImageHashAlgoArg`, `ImageSimilarityThresholdArg`, `ExclusionsToggle`, `ShareValue`, `BenchTier`, `SnapshotFormat`, `CliBenchLane` — `ValueEnum`s with `Display`/`FromStr` where applicable.
  - `pub fn parse_size(s: &str) -> Result<u64>` — human-friendly size parser (`"4K"`, `"512M"`, etc.); binary multipliers.
- Default `--hash-algo` is `River5` (alias-accepted: `ddh128`, `river128`).
- Default `--strategy` is `Smart`; `Interactive` is `#[value(skip)]` (GUI-only).
- Default `--image-hash-algorithm` is `Dhash`; `Phash` is the legacy DoubleGradient spelling.
- F-CLI-1 regression test pins `ImageHashAlgoArg::default() == Dhash`.
- #159 `--action trash` is a clap alias for `--action recycle` so the macOS/Linux vocabulary works on Windows scripts.
- Who calls this: `main.rs` (CLI dispatch), `bin/superdeduper_gui.rs` indirectly via the engine, integration tests.

### `config.rs` (795 lines)
- Validated, engine-ready `ScanConfig`. Compiles CLI `ScanArgs` → glob sets, parsed size limits, exclusion policy, default io-threads.
- Public API:
  - `pub struct ScanConfig` — the validated config; fields are pub for engine consumers.
  - `pub fn ScanConfig::from_args(args: &ScanArgs) -> Result<Self>` — validation entry point.
- Crate-internal:
  - `default_io_threads` / `default_io_threads_uncached` — per-disk-class table (HDD=1, NVMe=8, SSD=8, WSL=1, unknown=8). Probe-once cache via `OnceLock` is v0.3.41 Phase 9 (γ) stabilization for the GUI-multi-scan case + the matrix variance band root cause.
  - `SUPERDEDUPER_FORCE_IO_THREADS` (env override, highest priority), `SUPERDEDUPER_IOTHREADS_PARKED` (env, forces 1).
- Who calls this: `main.rs` (`run_scan`), `gui::live::run`.

### `cache.rs` (1418 lines)
- Per-machine SQLite cache keyed by `(volume_guid, file_ref, hash_algo)`. Schema is currently version `"7"` (column `reparse_tag` on `inventory_records`).
- Public API:
  - `pub struct Cache` — connection wrapper. `Cache::open`, `Cache::open_default`, `lookup`, `lookup_detailed`, `store`, `warm_in_place`, `predict_hits`, `warm_load_all`, `stats`, plus inventory-snapshot APIs.
  - `pub struct CacheKey`, `pub struct CachedHashes`, `pub struct WarmCacheEntry`, `pub struct InventoryMeta`, `pub struct InventoryRecord`, `pub struct CacheStats`.
  - `pub enum LookupOutcome { Hit, Drift{reason}, NoRow }`, `pub enum DriftReason { Size, Mtime, Usn }`.
  - `pub fn lookup_warm(map, key) -> LookupOutcome` — lock-free in-memory variant.
  - `pub fn schema_state(path) -> Result<SchemaState>` + `pub enum SchemaState { Current, Mismatch, Uninitialized, NoCache }`.
  - `pub fn default_cache_path() -> Result<PathBuf>` — honors `SUPERDEDUPER_TEST_DATA_DIR` first.
- SQLite PRAGMAs locked: `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`.
- Tier columns use `COALESCE(excluded.X, X)` on conflict so per-tier store calls don't clobber earlier tiers (regression fix; see Refactor Hints).
- Who calls this: `main.rs` (`run_scan`, `run_cache`), `gui::live`, `pipeline::hash`.

### `dedupe.rs` (2540 lines)
- The destructive-action engine + the `superdeduper dedupe` subcommand. Safety contracts are layered (CLI-planner → action_* wrappers → perform_action helper) so the GUI's per-row actions hit the same gate stack the CLI does.
- Public API:
  - `pub struct ResultsFile`, `pub struct Summary`, `pub struct Outcome`, `pub struct DedupeActionSummary`.
  - `pub const LOCKED_ACTION_KEYS: &[&str]` — mirror of the server's locked action-credit keys; boundary test pins them against `DedupeActionSummary::locked_action_key`.
  - `pub const SAFE_RENAME_SUFFIX: &str = ".superdeduper"`.
  - `pub fn run(args: &DedupeArgs) -> Result<Outcome>` — main planner.
  - `pub fn action_remove(path, keeper, references)`, `action_recycle`, `action_hardlink`, `action_reflink`, `action_safe_rename` — single-file destructive entry points used by the GUI.
  - `pub fn unsuperdeduper_root(root) -> Result<(u64, u64, u64)>` — undo helper for `safe-rename` batches.
  - `pub fn is_system_path(path) -> bool` — OS-critical-path check; shared by the CLI planner + the GUI action layer.
- Key invariants: reference paths NEVER modified; system-critical paths refused unless `--allow-system-paths`; pre-action validate via `validate_file` (size check only — see Invariants).
- Who calls this: `main.rs` (`run_dedupe`), `gui::*` (per-row action paths).

### `diagnose.rs` (1249 lines)
- `superdeduper diagnose` subcommand. Probes hash compute throughput, Tier 1 syscall throughput, Tier 3 sequential throughput; detects Defender state (Windows), CPU thread count, RAM, hash algo impl identity. Emits text or JSON.
- Public API:
  - `pub struct DiagnoseReport`, `pub struct DriveProbeResult`, `pub struct SystemInfo`, `pub struct HashProbeResult`, `pub struct Tier1ProbeResult`, `pub struct Tier3ProbeResult`, `pub struct DefenderState`, `pub struct Recommendation`, `pub enum MachineProfile`, `pub enum RecommendationImpact`.
  - `pub fn run_probes(target_paths, skip_io) -> anyhow::Result<DiagnoseReport>`.
  - `pub fn run(args: DiagnoseArgs) -> anyhow::Result<()>`.
  - `pub fn probe_defender() -> DefenderState`.
- Schema string `DiagnoseReport::schema` is the wire contract the GUI preflight modal reads — bumping is a UI contract change.
- Who calls this: `main.rs` (`Command::Diagnose` → `superdeduper::diagnose::run`), GUI preflight modal.

### `scan_history.rs` (1443 lines)
- Per-scan JSON-file history under `<data_dir>/scan-history/<scan_id>.json`. Schema version `4`; loaders skip rows with `schema_version > CURRENT_SCHEMA_VERSION` (forward-compat for sd downgrades).
- Public API:
  - `pub const CURRENT_SCHEMA_VERSION: u32 = 4`, `pub const MAX_RESUBMIT_ATTEMPTS: u32 = 3`.
  - `pub enum SubmissionState`.
  - `pub struct ScanRecord` — the row shape.
  - `pub fn transient_outcome_state(prior_attempts) -> SubmissionState`.
  - `pub fn similarity_kind_breakdown(groups) -> BTreeMap<String, u64>`.
  - `pub fn new_scan_id() -> String`, `pub fn record_completed(record) -> io::Result<PathBuf>`, `pub fn list() -> io::Result<Vec<ScanRecord>>`, `pub fn load(scan_id) -> io::Result<Option<ScanRecord>>`, `pub fn delete(scan_id) -> io::Result<()>`.
  - `pub fn update_submission_state`, `mark_submitted`, `set_submission_id`, `find_by_submission_id`, `update_reclaim_for_submission`, `record_local_action_for_latest_scan`, `list_pending_older_than`, `prune_older_than`.
  - `pub fn history_dir() -> io::Result<PathBuf>`.
- Filename is the scan_id as a hyphenated UUID v4. Atomic write via write-then-rename.
- Who calls this: `main.rs` (`run_scan_history`, `run_submit_pending`), `gui::live` (scan-finish hook), CLI integration tests.

### `channel.rs` (525 lines)
- Server-channel selector (`prod` / `dev` / `local`). Single chokepoint for every cross-cutting consumer (telemetry submit, GUI banner, CLI footer, install-state loader, OAuth flow).
- Public API:
  - `pub const ENV_VAR: &str = "SUPERDEDUPER_CHANNEL"`, `pub const SERVER_URL_ENV_VAR: &str = "SUPERDEDUPER_SERVER_URL"`.
  - `pub enum Channel { Prod, Dev, Local }` + `as_slug`, `description`, `is_non_prod`, `all`, `FromStr`, `Display`.
  - `pub struct ChannelParseError`, `pub struct PersistedConfig`, `pub struct NetworkConfig`.
  - `pub fn resolve_server_url(channel) -> String` — honors `SUPERDEDUPER_SERVER_URL` override.
  - `pub fn server_url_for(channel) -> &'static str`, `pub fn frontend_url_for(channel) -> &'static str`.
  - `pub fn config_file_path() -> io::Result<PathBuf>`, `pub fn read_persisted_channel`, `pub fn write_persisted_channel`, `pub fn read_env_channel`, `pub fn resolve_active_channel`.
  - `pub fn set_active_channel(channel)`, `pub fn active_channel() -> Channel` — process-global atomic.
- Precedence: CLI `--channel` > env > config > default `prod`.
- Dev backend uses first-level subdomain `dev-api.superdeduper.io` (Cloudflare wildcard cert constraint). Frontend uses `dev.superdeduper.io` (apex via Pages, no wildcard constraint).
- Who calls this: `main.rs` (boot), `gui::*` (banner, settings_modal), `leaderboard::*` (submit URL).

### `time.rs` (200 lines)
- Single source of truth for `now_unix_*` + civil-from-days arithmetic. Replaces 8× duplicated `now_unix()` (mixed `u64` / `i64`) and 6× duplicated Howard Hinnant civil-from-days copies (#91 / #133).
- Public API:
  - `pub fn now_unix_i64() -> i64`, `pub fn now_unix_secs() -> u64` — clock-skew logs a warning, falls back to 0.
  - `pub fn now_unix_secs_checked() -> Option<u64>` — None on skew.
  - `pub fn now_iso8601() -> String` — `YYYY-MM-DDTHH:MM:SSZ`.
  - `pub fn unix_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32)` — handles negative epochs.
- Leaf module — imports nothing from the crate.
- Who calls this: cache, scan_history, action_receipt, gui, leaderboard.

### `path_display.rs` (117 lines)
- #73 — Single helper for user-facing path display. Strips Windows `\\?\` verbatim prefix; rewrites `\\?\UNC\…` back to `\\…`.
- Public API: `pub fn for_user_display(p: &Path) -> String`.
- Out of scope: `dedupe::action_receipt` records (which use the canonical Win32 form for inode-tracking + ACL audit).
- Who calls this: `output`, `dedupe::is_system_path`, `keep::score_file`, GUI rendering surfaces.

### `error.rs` (86 lines)
- Crate-wide error type. `#[derive(thiserror::Error)]`.
- Public API: `pub type Result<T>`, `pub enum Error` (variants: Io, PathNotFound, UnsupportedVolume, MftEnum, RetrievalPointers, UsnJournal, Cache, BadGlob, BadSize, Unsupported, EnvVarMissing, ConfigInvalid, Serde, Other).
- `Error::other(msg)` convenience constructor.

### `schema.rs` (267 lines)
- Locked persistence-versioning policy. `SchemaVersioned` trait + `check` helper enforce exact `(name, version)` equality on load; explicit `migrate_from_vN` helpers required for forward-load.
- Public API: `pub trait SchemaVersioned`, `pub enum CheckError { WrongSchema, UnsupportedVersion }`, `pub fn check<S: SchemaVersioned>(loaded: &S) -> Result<(), CheckError>`.
- Only adopted by 1 store as of audit time (`gui::archive::ArchiveManifest`). Other 10+ persistence layers (per pre-#92 inventory) stay on their legacy load-side check until they next rev — opportunistic migration.

### `keep.rs` (380 lines)
- Smart keep-strategy heuristic. Scored components: location (Recycle Bin / temp / cache penalised), path depth, filename markers (`_final` rewarded, `_draft` / `copy of ` / ` (1)` penalised), mtime recency.
- Public API:
  - `pub fn file_mtime(p: &Path) -> Option<SystemTime>`.
  - `pub struct KeepScore { total, breakdown }`.
  - `pub fn score_file(path, mtime) -> KeepScore`.
  - `pub fn pick_keeper<P: AsRef<Path>>(paths, mtimes) -> usize`.
- Single source of truth for the Smart-keeper tiebreak — both CLI (`dedupe::pick_keeper`'s Smart arm) and GUI (`gui::live::order_keeper_first`) route here so the two flows can't drift (#68).

### `output.rs` (483 lines)
- Output formatting for `superdeduper scan` (text / JSON / CSV / Markdown report).
- Public API:
  - `pub fn open_writer(output: Option<&Path>) -> std::io::Result<Box<dyn Write>>` (#137 shared file/stdout writer).
  - `pub fn write(out, format, groups, skipped, reference_paths) -> Result<()>`.
- JSON schema bumped to `"superdeduper.scan.v2"` (added `skipped[]` + `placeholder_skipped`).
- Reclaim is reported two ways: path-aware (`reclaimable_bytes` — overstates for hardlink-heavy corpora) + inode-aware (`reclaimable_inode_bytes` — what users actually get back).

### `action_receipt.rs` (441 lines)
- Structured NDJSON action receipts for `--integration-test-mode` (`superdeduper.action_receipt.v1`).
- Public API:
  - `pub enum ReceiptWriter { Stdout, File{path, handle}, Disabled }` + `from_flags`, `emit`.
  - `pub struct ActionReceipt`, `pub struct RecycleBinEntry`.
  - `pub fn action_label(action: DedupeAction) -> &'static str`.
  - `pub fn read_inode_and_nlink(path) -> Option<(String, u64)>` — cross-platform; **Windows currently returns `(0, 1)`** as a placeholder until the `windows`-crate `GetFileInformationByHandle` is plumbed (see Invariants).
- ISO 8601 UTC with millisecond precision via `time::unix_to_ymdhms`.

### `perf_gui_startup.rs` (285 lines)
- v0.3.43 lazy-eframe-init startup decomposition. Five always-on markers emitted once per GUI lifetime: `pre_native_ms`, `run_native_to_new_ms`, `app_new_ms`, `first_update_ms`, `total_to_first_update_ms`.
- Public API: `pub fn record_pre_run_native()`, `record_app_new_start()`, `record_app_new_end()`, `emit_if_first_frame()`, `pub struct FirstFrameEmitGuard`.
- Idempotent (OnceLock per slot, AtomicBool for the emit sentinel). Sub-microsecond cost.
- Wired from `src/bin/superdeduper_gui.rs` + `gui::SuperdeduperApp::new` + `gui::app::SuperdeduperApp::update`.

### `perf_scan_lifecycle.rs` (231 lines)
- v0.3.42 canonical scan-lifecycle metrics: TTWS / TTW / TTDD. Always-on; emit at scan completion. CLI + GUI both feed the same `crate::log::write_line` sink.
- Public API: `pub fn record_process_start()`, `pub fn process_start_for_diagnostics() -> Option<Instant>`, `pub struct PerfScanLifecycle` with `new`, `walk_started`, `walk_completed`, `scan_completed`.
- Subsequent scans in a long-lived GUI suppress TTWS (emit 0); first scan = real init.

### `log.rs` (18 lines)
- Thin re-export shim. The implementation moved to the `superdeduper-log` leaf crate (Phase 0 2026-05-31) so future bench-real / bench-stub crates can use the macros without depending on the engine binary.
- Public API: `pub use superdeduper_log::write_line`. The `log_info!` / `log_warn!` / `log_err!` macros are re-exported at the crate root via `lib.rs`.
- Always-on (NOT feature-gated).

## Invariants / Gotchas

1. **Pre-action validate is SIZE-ONLY, not (size, mtime).** `dedupe::validate_file` (line 820-829) checks only `meta.len() != expected_size`. The `dedupe.rs` module header (line 9-11) says "`(size, mtime)` is re-checked" — that's stale. Anyone adding mtime to the check needs to also surface it in `Outcome::skipped_invalidated` accounting.

2. **Windows inode resolution is a stub.** `action_receipt::inode_nlink_from_metadata` on Windows returns `(0, 1)` always (lines 271-284 of `action_receipt.rs`). On Linux/macOS it returns real `ino()` + `nlink()`. Anywhere a receipt asserts a Windows inode equality (hardlink-replace `inode_after == keeper_inode`) currently fails by construction on Windows; harnesses needing the real ID must shell out to `superdeduper debug snapshot`, which uses `CreateFileW` + `GetFileInformationByHandle` via the `windows` crate.

3. **`leaderboard_corpus_sig` uses blake3 but tags the output `sha256:`.** `lib.rs:137`: `format!("sha256:{}", hasher.finalize().to_hex())` where `hasher` is a `blake3::Hasher`. Server-side or verifier code that introspects the prefix would be misled. The `sha256:` may be a stable contract identifier — do not rename without confirming the server contract first.

4. **Cache schema bump = data wipe.** Bumping `cache::SCHEMA_VERSION` (currently `"7"`) drops every table the engine owns (`files`, `volumes`, `inventory_meta`, `inventory_records`) in `init_schema`. The `meta` table is preserved (carries schema_version itself). Don't change the drop list lightly.

5. **Cache `store` per-tier coalesce.** SQLite `ON CONFLICT … DO UPDATE SET tierN_hash = COALESCE(excluded.tierN_hash, tierN_hash)` — so a tier-3-only `store` does NOT clobber tier-1 / tier-2 from a prior call. Regression class: the earlier "set every column to excluded.X" version meant the deepest tier won and Tiers 1-2 re-ran from scratch on resume.

6. **`Channel::ACTIVE_CHANNEL` is process-global state.** Set ONCE at `main.rs` startup after the precedence chain resolves. The GUI Settings-channel-switch is the one legitimate mid-session re-call (with explicit user confirm). Anything that reads channel mid-process MUST go through `channel::active_channel()`, not through a re-resolution.

7. **HOME-env unit tests must serialise via `test_serial::home_env_guard()`.** Per-module SERIAL gates only block within a single module; cross-module HOME-env races (`platform::linux::trash::tests` × `scan_history::tests`, etc.) need the crate-wide gate. `parking_lot::Mutex` is intentional (poison-tolerant).

8. **`time::now_*` skew policy is log-and-fall-back-to-0.** Use `now_unix_secs_checked() -> Option<u64>` when persisting to a leaderboard payload or scan_history record — a 1970 timestamp is observably wrong post-hoc and poisons sort/dedupe.

9. **`path_display::for_user_display` is mandatory for user-facing surfaces.** All CLI text/JSON/Report output (#74), GUI rows + tooltips (#73), and Log-panel surfaces route through it. Exception: `dedupe::action_receipt` records use the canonical Win32 form (downstream test asserter pins the bytes).

10. **`perf_*` modules' OnceLock state never resets.** `perf_gui_startup::FirstFrameEmitGuard` and `perf_scan_lifecycle::ttws_emitted_flag` are process-lifetime. The "subsequent scan TTWS = 0" semantic depends on this. Unit tests for these modules already coexist on the same statics — they re-store the AtomicBool to false in setup, never reset the OnceLock slots.

11. **`force_mft` accepts hardlink-alias collapse + bypasses exclusion filters.** Documented at `config.rs:50-60`. Power-user only; defaults off post-v0.3.16.

12. **`safe-rename` suffix is `.superdeduper` and idempotent.** Files already ending in the suffix are a no-op (per `safe_rename_unguarded`). `unsuperdeduper_root` walks for the suffix; renaming the suffix would orphan past safe-renamed files.

## Dependencies

- INCOMING:
  - external: `main.rs` is the CLI binary entry; `bin/superdeduper_gui.rs` is the GUI binary entry. Both consume the lib via `use superdeduper::...`.
  - integration tests under `tests/` consume the lib via the same paths.
- OUTGOING (top-level, by directory):
  - `inventory/` — Stage 1 enumeration.
  - `pipeline/` — Stages 2-4 (grouping / layout / hashing) + helpers (`io_threads_probe`, `image_hash`).
  - `gui/` — eframe app (feature-gated `gui`).
  - `leaderboard/` — telemetry, achievements, captcha (feature-gated `telemetry`).
  - `platform/` — OS-specific trash, recycle, drive-info.
  - `winapi_wrappers/` — safe-Rust shim over `windows` crate FFI.
  - `debug/` — `debug snapshot` corpus / inode dump.
  - `exclusions/` — preset packs + custom-pattern compile.
  - leaf crates: `superdeduper-log` (logging macros).
  - external crates: clap, serde, serde_json, rusqlite, parking_lot, anyhow, thiserror, globset, humansize, csv, blake3, rayon, tracing, toml.

## Refactor Hints

- **`config.rs:84-87` — stale doc on `hash_algo`.** Says "BLAKE3 is the default; DDH-128 is the in-development alternative (currently an xxhash3-128 stub)." The CLI default is `River5`, and DDH-128 was renamed to river5 long ago (see cache.rs SCHEMA_VERSION notes v2 → v3). Should read: "river5 (default, 16-byte, AES-NI hardware-accelerated) or BLAKE3 (32-byte, cryptographic)."

- **`config.rs:88-94` — stale doc on `exclusion_policy`.** Says "Defaults to disabled (master toggle OFF)" but `build_cli_exclusion_policy` defaults to safe-defaults ON (#81 v0.2.7+). The "Compile … once the GUI / CLI exposes a way to populate the config (Days 3-5)" phrasing is also pre-#81 chronicle.

- **`dedupe.rs:9-11` — stale doc on validate_file.** Module header claims `(size, mtime)` is re-checked; only size is. Either tighten the doc to "size is re-checked" or extend `validate_file` to actually re-check mtime.

- **`action_receipt.rs:256-257` — misleading doc on `read_inode_and_nlink`.** Says "on Windows we use the file_ref via `winapi_wrappers` if available, else fall back to `MetadataExt::file_index`." Neither is true today; Windows returns `(0, 1)`. The inline comment at lines 271-284 is honest; the docstring on the pub fn isn't.

- **`cache.rs:90-93` — stale doc on `WarmCacheEntry`.** Says "Use `Cache::warm_load_all` to build the HashMap once at Stage 4 start; pass `Arc<HashMap<_>>` through the hash pipeline." Architecture changed: `warm_in_place` stores the map internally on the `Cache` and `lookup_detailed` consults it before SQLite. No external Arc/HashMap is passed through.

- **`cache.rs::warm_load_all` — possibly downgrade visibility.** Only one in-crate consumer (`warm_in_place` itself, on line 418). Could move to `pub(crate)` (and update the docstring above accordingly). Confirmed via:
  `grep -rn "warm_load_all" /home/neomatrix/projects/mickfixesjunk/superdeduper/ | grep -v "/cache.rs:"` → 0 hits.

- **`lib.rs:110-138` — `leaderboard_corpus_sig` doc claim "Mirrors the implementation in `leaderboard::submission`'s payload-build flow" is unverified.** `grep -rn "corpus_sig" leaderboard/` finds nothing comparable. Either the leaderboard module has its own variant somewhere subtle, or this comment outlived the mirror. Worth confirming before deduping.

- **`lib.rs:137` — `sha256:` prefix on a blake3 digest** is an inconsistent-naming class. If the prefix is a server contract identifier ("we hash with whatever we want, prefix is a versioning lever"), document that loudly. If it's a leftover from a pre-blake3-migration, fix.

- **`schema.rs` adoption is 1 of 11 stores.** The dense rationale block (`schema.rs:1-67`) inventoried 10 other persistence layers as candidates. Migration is opportunistic on rev; tracking who's done what would be useful (could be a row in `schema.rs` table).

- **`output.rs::display_path` is a 1-line wrapper around `path_display::for_user_display`.** Could be inlined; it exists for #74 chronicle reasons (the old local impl was different). Cohesion / no-op refactor.

- **CLI `Interactive` keep-strategy is `#[value(skip)]` — GUI-only.** The variant lives in the public enum so the GUI can use it; the CLI doesn't advertise it. Anyone pruning the variant must check `gui::*` for the consumer.

- **`io_threads` env vars: `SUPERDEDUPER_FORCE_IO_THREADS` (override), `SUPERDEDUPER_IOTHREADS_PARKED` (force 1).** Both checked in `config.rs::default_io_threads_uncached`; order: FORCE first, PARKED second, probe third, per-disk-class fallback last. The "PARKED" name reads like dead-code but is intentionally retained (`feedback_check_memories_before_routing_constraints` class: don't prune without grep).

## Wire Surfaces (if any)

- **CLI subcommands**: `scan`, `dedupe`, `cache (info|clear|vacuum)`, `drive-info`, `diagnose`, `debug (snapshot|make-bench-corpus|bench-dedup-diff|bench-cluster-audit|cpu-brand)`, `register`, `config`, `achievements`, `account`, `submit-pending`, `bench-me`, `scan-history (list|delete|resubmit|prune)`.
- **CLI flags** (global): `-v / -q` (verbosity), `--channel` (prod/dev/local).
- **JSON output schemas**:
  - `output::write` → `superdeduper.scan.v2`.
  - `action_receipt` → `superdeduper.action_receipt.v1`.
  - `diagnose::DiagnoseReport` → schema string TBD in module (consumed by GUI preflight).
  - `scan_history::ScanRecord` → version `4` (rev-on-incompat-change).
- **Env vars read here**:
  - `SUPERDEDUPER_CHANNEL` — channel selector.
  - `SUPERDEDUPER_SERVER_URL` — API URL override (test mocks).
  - `SUPERDEDUPER_TEST_DATA_DIR` — cache + install root for hermetic test runs.
  - `SUPERDEDUPER_FORCE_IO_THREADS` — pin io-threads N across CLI+GUI for matrix testing.
  - `SUPERDEDUPER_IOTHREADS_PARKED` — force io-threads = 1.
  - `LOCALAPPDATA` / `APPDATA` / `XDG_CONFIG_HOME` / `XDG_CACHE_HOME` / `HOME` — platform default-path resolution.
- **On-disk paths**:
  - `<data>/log/superdeduper.<unix>.<pid>.log` — persistent log (always-on).
  - `<cache>/superdeduper/cache.db` — SQLite cache.
  - `<data>/scan-history/<scan_id>.json` — per-scan record.
  - `<config>/superdeduper/config.toml` — `[network] channel = "..."` and future preferences.
  - `<install>/oauth.{channel}.json` — OAuth tokens (telemetry).
- **Persistent log emit prefixes** (matrix regex-keyed): `perf-scan-lifecycle:`, `perf-gui-startup:`.

## Non-source artifacts

None at this directory level — every file in `src/` (top level) is `.rs` source. Sub-directories own their own non-source files (e.g. `gui/assets/`).
