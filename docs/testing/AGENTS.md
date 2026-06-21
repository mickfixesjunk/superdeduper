# AGENTS.md — docs/testing/

## Purpose

Engine-side test orientation docs. Two surfaces:

- `cli-flag-matrix.md` — row-by-row enumeration of every CLI flag /
  subcommand exposed by `src/cli.rs`, with a Status column the matrix
  runners (testrunner Linux, sdd-testwin Windows) flip as rows are
  exercised. Tracks F-CLI-N findings inline.
- `gui.md` — minimum-viable GUI test harness layering (Tier 0 serde
  pin, Tier 1 widget-state, Tier 1 widget-render via egui_kittest;
  Tier 2/3 deferred). Documents what is shipped, what is NOT yet
  caught, and the `cargo test` invocations.

Both docs are READ by testdesign / testrunner / sdd-testwin agents to
plan matrix sweeps; the engine owns row enumeration and fix-for-RED.

## Files

- `cli-flag-matrix.md` — 262 lines. Status legend + F-CLI findings
  table + per-subcommand flag tables (`scan`, `dedupe`, `cache`,
  `drive-info`, `diagnose`, `register`, `config`, `achievements`,
  `account`, `submit-pending`, `bench-me`, `scan-history`, `debug`)
  + row-execution conventions + F-CLI-5 candidate-impl note for
  Win11 26200 ReFS Dev Drive detection + maintenance checklist.
- `gui.md` — 65 lines. Tier coverage table + bug class motivation
  + how-to-add patterns (widget-state, serde-layer) + run commands
  + egui 0.28 -> 0.32 upgrade history.

## Invariants

- Every new `#[arg(...)]` field added to a struct in `src/cli.rs`
  MUST land a matching row in `cli-flag-matrix.md` with Status ⚫
  UNVERIFIED (per the "Maintenance" section line 246-251).
- Status legend symbols are 🟢/🔴/🟡/⚫ — referenced verbatim by
  testrunner/sdd-testwin row reports.
- F-CLI-N IDs are append-only; do not renumber.
- GUI MV slice gates: `profile_deserialises_live_backend_shape`
  (Tier 0) + `badge_wall_classifies_granted_tiles_from_live_server_shape`
  (Tier 1 widget-state) are the named tests doc readers expect to
  find; renaming either without updating `gui.md` breaks the
  doc-to-test trace.

## Dependencies

- `cli-flag-matrix.md` reads `src/cli.rs` (struct field enumeration
  + `#[arg(...)]` shape + clap defaults + telemetry-gating).
- `gui.md` reads `src/leaderboard/catalog.rs` (test at line 408) and
  `src/gui/widgets/badge_wall.rs` (test at line 835, helper at 666).
- `gui.md` references `~/sd-bench-local/design/gui-test-harness-spec.md`
  (out-of-tree authoritative spec).
- Both consumed by testdesign / testrunner / sdd-testwin agents.

## Refactor Hints

- The cli-flag-matrix doc enumerates flags by hand; the
  enumeration is a near-mirror of `clap::Command::get_arguments()`
  which could be auto-generated as a build-time check that flags
  the diff vs the doc (info-level — manual curation has matrix-state
  semantics the auto-pass can't carry).
- ConfigCommand sub-row is intentionally stubbed ("see ConfigCommand
  enum at cli.rs:1034") — that line reference is stale, see
  Findings.

## Wire Surfaces

- Status column values 🟢/🔴/🟡/⚫ are read by testrunner /
  sdd-testwin row-execution scripts; changing the legend symbols
  ripples into those agents.
- F-CLI-N IDs ship into GitHub issue bodies (#151 umbrella).
- Test names `profile_deserialises_live_backend_shape` and
  `badge_wall_classifies_granted_tiles_from_live_server_shape`
  are also `cargo test` filter targets (`--lib badge_wall`,
  `--lib catalog`).

## Findings (this audit)

See FINDINGS_FOLLOWUP.md (aggregator will write it).
