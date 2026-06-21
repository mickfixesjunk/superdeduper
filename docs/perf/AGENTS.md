# docs/perf — Agent Orientation

## Purpose

One-off methodology notes for performance-oriented bench harnesses
that don't belong inside `docs/perf-98-findings.md` (the main perf
findings doc). Currently holds a single artifact: the HDD-profile
bench methodology authored by superdeduper-overflow 2026-05-31 for
sdd-testwin to execute against Mick's NEO E:\ HDD corpus.

This is a docs-only directory; no code references it at compile time.

## Files

- `hdd-profile-bench-methodology.md` — companion doc for
  `scripts/bench/Run-SdHddBench.ps1`. Captures: scan-time flag
  inventory (`ScanArgs`), invocation pattern, corpus + runtime
  budgeting, cold-cache enforcement via RAMMap.exe, optional
  PhysicalDisk perf-counter capture, procedure for sdd-testwin,
  and an expected-analysis loop that feeds findings back into
  `docs/perf-98-findings.md`.

## Invariants

- `scan` is non-destructive (no files touched). `dedupe` is the
  destructive subcommand and is NEVER invoked in this bench.
- Bench MUST run with `--no-cache` (engine SQLite cache off) — cold
  hash-cache regime is the question being measured.
- OS page cache is independent of `--no-cache`; cold OS cache is
  enforced via RAMMap.exe Sysinternals tool, not via the engine.
- `--force-hash` is forbidden in this harness (inflates IO 5-10×;
  results don't transfer to real scan path).
- Strictly serial — one config at a time; no parallel sd instances.
- `SUPERDEDUPER_CHANNEL=local` keeps the bench airgapped from prod
  telemetry endpoints.

## Dependencies

Code surfaces the doc references (all in this repo):

- `src/cli.rs::ScanArgs` — flag inventory the doc enumerates.
- `src/main.rs` stderr timing emit — the `--- timing (<algo>) ---`
  block that the harness parses.
- `scripts/bench/Run-SdHddBench.ps1` — the harness itself.
- `docs/perf-98-findings.md` — sibling doc; receives any HDD-curve
  recommendation rows after analysis.
- `docs/testing/cli-flag-matrix.md` (F-CLI-5 section) — the broader
  flag-coverage matrix.
- `src/pipeline/io_threads_probe.rs` — NEW since this doc was
  written (2026-06-02+); auto-bracket probe at scan start. Doc does
  not yet mention the probe or the `SUPERDEDUPER_IOTHREADS_PARKED=1`
  escape hatch.

## Refactor Hints

- If `ScanArgs` flag set changes (rename / new flag / default
  change), update §"Engine flag inventory" table. Particularly the
  `--io-threads` default row — see Wire Surfaces.
- If the stderr timing banner format changes in `src/main.rs`,
  update the §"Output the harness consumes" code block.
- The doc references `docs/perf-98-findings.md` by relative path
  `docs/perf-98-findings.md` — when this file lives at
  `docs/perf/hdd-profile-bench-methodology.md`, the actual
  relative path back is `../perf-98-findings.md`. Currently the
  doc reads as if it lives at repo root.

## Wire Surfaces

External tools and stable contracts the doc binds to:

- **stderr timing banner** — parsed by `Run-SdHddBench.ps1` and any
  future bench. Current shape (verified `src/main.rs:2198`):
  ```
  --- timing (<algo-tag>) ---
  stage 1 inventory:    ... ms (NNN files)
  ...
  ```
  `<algo-tag>` is `cfg.hash_algo.tag()` — e.g. `river5-aes-ni`,
  `river5-stub-xxh3`. The doc shows `(river5)` literally, which is
  drift (see findings).
- **`--io-threads` default** — doc says `threads × 3`; engine now
  ALSO auto-caps at 16 when scan root is HDD (rotational). The doc
  table entry omits the cap and the new auto-probe (v0.3.31+
  per-instance bracket).
- **`SUPERDEDUPER_CHANNEL=local`** — env-var escape; still honored
  by the channel banner.
- **RAMMap.exe `-Ew` / `-Es`** — Sysinternals CLI surface; external,
  stable.
- **`Get-Counter \PhysicalDisk(N)\...`** — Windows perfmon counter
  names; external, stable.

## Findings Cross-Ref

See `FINDINGS_FOLLOWUP.md` (root) for line-cited drift.
