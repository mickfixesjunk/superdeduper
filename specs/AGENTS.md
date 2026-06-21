# specs — AGENTS guide

## Purpose

This directory holds ratified design specifications for the superdeduper
perf / ship-gate methodology. These are NOT code or runtime artifacts —
they are authoritative reference documents that describe how matrix
testing, ship-gate criteria, and verdict interpretation work for the
v0.3.x perf chain. The canonical spec lives at the top level; the
`historical/` subdirectory preserves superseded specs that are still
useful as context for chain archaeology.

Specs here are authored by the `testdesign` agent and ratified by
`design` + (sometimes) Mick. They are referenced by sdd-testwin
(matrix runner implementation), engine (PR scoping + acceptance), and
other agents reasoning about ship-readiness. They are markdown-only and
have no compile-time dependency on the Rust crates.

## Files

### `v0.3.40-mick-corpus-ship-gate.md`

Canonical ship-gate spec for v0.3.40 through v0.3.42 (and current
reference for v0.3.43+). Defines:

- The 5-cell paired-CLI + Defender-toggle matrix layout (sections 3).
- The §2.1 engineering-fixable ratio criterion `(chunk_loop_total_ms +
  sum(perf-startup_named_buckets)) / cold-CLI_Defender-off <= 1.10x`
  with structural-startup carve-out (§2.4).
- File-watch wall measurement methodology (scan-history JSON mtime +
  size-stabilization; replaces CPU-idle-tail per design 2026-06-06
  methodology-gap report) (§4.1).
- Engine instrumentation env vars: `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE`,
  `SUPERDEDUPER_PERF_INSTRUMENT_RAYON`, `SUPERDEDUPER_PERF_INSTRUMENT_CHUNK_EMIT`,
  `SUPERDEDUPER_FORCE_IO_THREADS`, `SUPERDEDUPER_TEST_DATA_DIR` (§4.2).
- Auto-evaluation GREEN/EDGE/RED/ERROR labeling (§5) + verdict
  interpretation tree (§6).
- 14 sprint methodology lessons (§7) including the §7.13 lesson that
  "structural-bound carve-out is a HYPOTHESIS not a conclusion".
- v0.3.43 pre-scope (§8) — carryover items: Route b walker `stat()`,
  lazy-eframe-init, CPU thread-time, matrix-tooling repo extraction,
  subprocess-cell spec, storage-class generalization (closed).
- Ratification + ship outcome log (§10): v0.3.40 / v0.3.41 / v0.3.42 all
  shipped; release SHAs `ab75fd5`, `dde18bd`, `f355328`.

Public API: none (markdown).

Who reads this: testdesign (author), design (ratifier), sdd-testwin
(matrix-runner implementation), engine/overflow (PR scoping),
quality + codex-review (PR review reference).

### `historical/egui-kittest-scan-perf-mick-shape.md`

Superseded predecessor spec from the v0.3.39 chain. Two-cell
(synthetic + Mick-shape) gate using egui_kittest in-process harness +
`SUPERDEDUPER_TEST_PERF_RATIO_MICK_SHAPE` env var ratchet. Retained
because the v0.3.40 successor (header §7 line in v0.3.40 spec) explicitly
points back to this as historical context.

Public API: none.

Who reads this: anyone tracing the v0.3.39 -> v0.3.40 methodology
transition (in-process kittest -> subprocess production-binary matrix).

## Invariants / Gotchas

- **`v0.3.40-mick-corpus-ship-gate.md` is canonical** for v0.3.43+
  ship-gate decisions. Do not author a new top-level ship-gate spec
  without first checking if updating this one is appropriate; the §7
  sprint lessons accrete by version-fold (see header status line
  for the fold history).
- **The §2.1 numerator excludes the structural-startup residual.**
  Ratio is engineering-fixable wall over cold-CLI, NOT raw GUI wall.
  Refactorers updating the matrix runner must preserve this distinction;
  raw ratio is informational only (§2.3, §5.1).
- **Lockstep methodology (§7.9)**: any engine PR adding new `perf-*:`
  emit prefixes, new `SUPERDEDUPER_PERF_INSTRUMENT_*` env vars, or new
  perf knobs (like `SUPERDEDUPER_FORCE_IO_THREADS`) requires a matching
  sdd-testwin runner hotfix (harvest regex + env-var setting + verdict
  report shape). Generic substring match does NOT cover sub-prefixes
  (e.g., `perf-chunks` regex won't match `perf-chunk-emit`).
- **perf-chunk-emit emits microseconds; chunk_emit_ms_total is in
  milliseconds.** Convert before sum-conservation comparison (§4.3).
- **Lifecycle decomposition fields are DURATIONS, not cumulative
  timestamps** (§4.3 + §7.11). Invariant: `total_ms ≈ ttws_ms + ttw_ms
  + ttdd_ms` (raw sum, not difference).
- **The `historical/` file is superseded** — do not treat it as current
  ship-gate criteria.

## Dependencies

- INCOMING: referenced by `feedback_dev_loop_matrix_canonical_pr_first.md`
  (locked dev loop), engine + overflow PR bodies, sdd-testwin matrix
  runner at `scripts/bench/Run-MickCorpusMatrix.ps1`, and design/quality
  reviewers reasoning about ship-readiness.
- OUTGOING: references several memory files in §9 (e.g.
  `feedback_perf_ship_gate_absolute_wall_over_ratio`,
  `feedback_cell_methodology_test_vs_real_binary_gap`,
  `reference_swarm_gh_shared_account_self_approve_block`) plus engine
  PRs #167-#178 and runner hotfix SHAs (da05506, 264d588, 2e03487,
  9a7b82f, 9d679e7, eb71ba2) and engine release SHAs (a66f813, ab75fd5,
  5c59624, dde18bd, 7f79b85, f355328, 53843b1).

## Refactor Hints

- This directory has no Rust / TOML / JSON / shell code to refactor.
  Refactoring here means spec edits.
- §8 Item 4 (matrix-tooling repo migration) is a pending decision —
  if it lands, the references in §10 sub-sections to in-engine-repo
  `scripts/bench/Run-MickCorpusMatrix.ps1` will need updates.
- §8 Item 6 is marked CONFIRMED CLOSED 2026-06-07; if a future spec
  iteration further consolidates §8, this item can be moved to a
  "closed" sub-section or dropped.
- Several §7 lessons (§7.3, §7.6, §7.8, §7.13) trace a single
  investigation arc; a future editorial pass could cross-link them as a
  named pattern ("variance illusion -> profile-first -> hypothesis-not-
  conclusion").

## Wire Surfaces (if any)

The spec references but does not own the following wire surfaces:

- Env vars consumed by the engine binaries (owned by engine; spec only
  documents matrix-runner setting): `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE`,
  `SUPERDEDUPER_PERF_INSTRUMENT_RAYON`, `SUPERDEDUPER_PERF_INSTRUMENT_CHUNK_EMIT`,
  `SUPERDEDUPER_FORCE_IO_THREADS`, `SUPERDEDUPER_TEST_DATA_DIR`,
  `SUPERDEDUPER_TEST_PERF_RATIO_MICK_SHAPE` (historical only).
- Scan-history JSON schema fields the runner depends on for file-watch
  end-detection: `scan_id`, `started_at_unix`, `completed_at_unix`,
  `wall_ms`. CLI top-level schema: `schema=superdeduper.scan.v2`,
  `groups`.
- `perf-*:` emit prefixes the harvest regex must enumerate:
  `perf-chunks`, `perf-streaming`, `perf-startup`, `perf-rayon-hash`,
  `perf-rayon-hash-worker`, `perf-chunk-emit`, `perf-scan-lifecycle`.

## Non-source artifacts

None. Directory contains only markdown.
