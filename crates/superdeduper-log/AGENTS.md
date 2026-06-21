# superdeduper-log — AGENTS guide

## Purpose
Leaf crate that provides the engine's persistent log fan-out: every
`log_info!` / `log_warn!` / `log_err!` call writes to BOTH stderr (preserving
the historical `eprintln!` observable behavior) AND a rotating per-process
log file under `<data_dir>/log/superdeduper.<unix_secs>.<pid>.log`.

Phase 0 extraction (2026-05-31) lifted the implementation out of the engine
binary's `src/log.rs` so other leaf crates (`superdeduper-bench-real`, future
`superdeduper-bench-stub`) can emit through the same macros without depending
on the entire engine. The engine's `src/log.rs` is now a thin re-export shim
(`pub use superdeduper_log::write_line;` plus `pub use ::{log_err, log_info,
log_warn};` at the engine `lib.rs`).

Always-on: NOT gated by any Cargo feature. Disk persistence is mandatory on
both `--features telemetry` and `--no-default-features` builds per Mick's
2026-05-29 PERSIST-logs-always directive.

## Files

### `Cargo.toml`
- Package `superdeduper-log` v0.1.0, edition 2021, `publish = false`.
- Single dependency: `parking_lot = "0.12"` (used for the `Mutex` around
  the lazy file handle + open-attempted flag).

### `src/lib.rs`
- The entire crate. Module docs explain the Phase 0 extraction rationale
  and the always-on contract.
- Public API:
  - `pub fn write_line(level: &str, args: std::fmt::Arguments)` — fan one
    line to stderr, then (lazily on first call) open and append to the disk
    log. Fail-soft on all IO errors.
  - `#[macro_export] macro_rules! log_info!` — INFO-level fan-out macro.
  - `#[macro_export] macro_rules! log_warn!` — WARN-level fan-out macro.
  - `#[macro_export] macro_rules! log_err!` — ERR-level fan-out macro
    (level string is the 4-char `"ERR "` to keep column alignment with
    INFO / WARN in the on-disk format).
- Private helpers:
  - `fn slot() -> &'static Mutex<Option<File>>` — OnceLock-backed lazy
    file handle.
  - `fn open_attempted() -> &'static Mutex<bool>` — single-shot guard so
    we never re-attempt the open if the first try failed.
  - `fn log_data_dir() -> io::Result<PathBuf>` — per-platform resolver
    (Windows `%LOCALAPPDATA%\superdeduper`, macOS `~/Library/Application
    Support/superdeduper`, Linux `$XDG_DATA_HOME/superdeduper` else
    `~/.local/share/superdeduper`).
  - `fn open_log_file() -> Option<File>` — builds `<data_dir>/log/`,
    rotates so at most 9 prior files remain (the soon-to-be-created file
    becomes the 10th), then opens append.
- Who calls this: engine binary (via `src/log.rs` shim + engine `lib.rs`
  re-export) and `superdeduper-bench-real` (direct `use
  superdeduper_log::{log_info, log_warn};` in `bench_run.rs`).
- Tests: two smoke-tests under `#[cfg(test)]` that exercise `write_line`
  and macro expansion. They do NOT assert the disk file is created.

## Invariants / Gotchas
- **Always-on**: do not add a `#[cfg(feature = "telemetry")]` gate to any
  code in this crate; the closed-source telemetry-off binary path also
  needs persistent logs.
- **Lazy single-shot open**: `open_attempted` flips true unconditionally
  on first call. If `open_log_file()` returns `None`, every subsequent
  call goes stderr-only — by design (don't pound a failing fs on every
  line). A refactor that retries open later must add explicit reset logic.
- **Fail-soft**: every IO operation uses `.ok()` / `let _ = ...`. Never
  introduce a `?` or `unwrap` here — logging must not break the caller.
- **Rotation timing**: rotation runs BEFORE the new file is created so
  the directory listing does not include the file currently being opened.
  Filenames are `superdeduper.<unix_secs:010>.<pid>.log` — the zero-padded
  10-digit ts makes lexicographic sort match chronological order through
  year 2286. Do not change the prefix `superdeduper.` or suffix `.log`
  without updating the rotation filter.
- **Lock ordering**: `open_attempted` is acquired and released BEFORE
  `slot()` is locked for the append. Holding both simultaneously is
  unnecessary and would risk a future deadlock if a third lock joins.
- **Duplicated `data_dir` resolver**: `log_data_dir()` is intentionally
  a copy of `leaderboard::install::data_dir()`. The module doc calls this
  out — keep them in sync, or extract `install::data_dir` into its own
  leaf crate (separate slice).
- **`$crate` in the macros** expands to `::superdeduper_log`. Callers
  that import via the engine re-export (`crate::log_info!(...)`) and
  callers that import directly (`superdeduper_log::log_info!(...)`) both
  resolve to the same `write_line`.
- **On-disk format**: `<unix_secs> [LEVEL] <message>\n`. The module doc
  notes this may switch to RFC3339 via chrono in Phase 2. Any tooling
  that grep / parses these files must not assume the format is locked.

## Dependencies
- **INCOMING**:
  - `superdeduper` engine crate (`src/log.rs` shim + `src/lib.rs`
    re-export of the three macros + `write_line`).
  - `superdeduper-bench-real` (`bench_run.rs`).
- **OUTGOING**:
  - `parking_lot` (Mutex).
  - `std` only (fs, io, path, sync::OnceLock, time, process).

## Refactor Hints
- The `log_data_dir()` duplication is the obvious cohesion smell — a
  future `superdeduper-paths` leaf crate could host the single
  `data_dir()` impl shared by `leaderboard::install` and this crate.
- `open_attempted` could be a `std::sync::atomic::AtomicBool` instead of
  `Mutex<bool>`, shaving one lock acquisition per log line. Low priority
  — these calls are already off the hot path.
- The smoke tests do not assert the file is created; a `tempdir`-based
  test that overrides `data_dir` (would need a `#[cfg(test)]` hook) would
  catch rotation regressions. Currently untestable without that seam.
- No suspect dead code: `write_line` is called by all three macros and is
  also called directly by the engine shim re-export; the three macros all
  have call-sites across the workspace (`rg log_info!` / `log_warn!` /
  `log_err!` confirms).

## Wire Surfaces
- **On-disk path**: `<data_dir>/log/superdeduper.<unix_secs:010>.<pid>.log`.
  Format `<unix_secs> [LEVEL] <message>\n`. Rotation keeps at most 10
  files (9 prior + the one being opened).
- **Env vars read**: `LOCALAPPDATA` (Windows), `HOME` (macOS, Linux
  fallback), `XDG_DATA_HOME` (Linux preferred). None are crate-specific —
  all are platform-conventional.
- No HTTP, no CLI flags, no JSON schema.
