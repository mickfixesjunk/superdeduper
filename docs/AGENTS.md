# docs/ — AGENTS guide

## Purpose

`docs/` is the superdeduper repo's design-spec + perf-record + harness-doc archive. It is not user-facing documentation (that lives elsewhere — release notes, website, in-app help). The audience is the swarm of agents (engine, design, testdesign, testrunner, sdd-testwin, benchmarker, overflow, dumbo) that drive the codebase: specs that motivated landed code, design specs awaiting implementation, performance findings + their resolutions, and operational runbooks for the swarm-health Monitor.

The directory mixes three lifecycle states:
- **Historical / superseded** (e.g. `leaderboard-spec.md` is explicitly tombstoned in favour of design's `~/sd-bench-local/` specs; `iocp-tier3-spec.md` is DEFERRED).
- **Active reference** (`swarm-health-check.md` operational runbook, `testing/cli-flag-matrix.md` rolling matrix, `perf/hdd-profile-bench-methodology.md` companion to a shipped harness).
- **Pre-implementation specs** (`exclusions-preset-content-draft.md` — content now landed; `preflight-spec.md` for GUI agent; `walker-fast-path-spec.md` Block N; `phase-0-trait-extraction.md` workspace refactor).

A refactorer must check the status banner at the top of each spec before treating any content as a source of truth — several of these specs describe code paths or modules that no longer exist (e.g. `src/leaderboard/bench.rs`, `src/leaderboard/bench_corpus.rs`, `src/leaderboard/d7_probe.rs`, `src/telemetry.rs`).

## Files

### `exclusions-preset-content-draft.md`
Day-2 review draft of the 8 preset packs (Pack 1 System libraries, Pack 2 Build artefacts, ..., Pack 8 AV signature databases) with all extensions + glob patterns. Per the doc's "Sequencing once approved" section, this content is intended to land as `src/exclusions/presets.rs`. That code now exists and uses `BuiltinPresets` (a `PresetSource` impl) as foreshadowed by the doc. Some draft notes (e.g. "wire `ExclusionPolicy::compile` to call the real impl instead of `EmptyPresets`") are now historical — the production wiring is complete.
- Public API: n/a (markdown)
- Who reads this: design (sign-off), engine (impl baseline)
- Feature gates: n/a

### `iocp-tier3-spec.md`
Block O+ IOCP-driven Tier-3 hashing design — explicitly **DEFERRED**. Documents the cheaper Block O++ producer-consumer ping-pong shipped instead, and preserves the IOCP submit/wait pseudocode + risk register in case a workload surfaces that Block O++ cannot address. Critical for refactorers: `src/pipeline/iocp.rs::WindowsScheduler::run_to_completion` is documented as a stub that routes through the buffered backend; this matches current code (see `src/pipeline/iocp.rs:257-274`).
- Public API: n/a (markdown)
- Who reads this: engine (pipeline track), benchmarker (if a Tier-3-IO regression appears)

### `leaderboard-spec.md`
**SUPERSEDED.** Pre-G-track leaderboard sketch. Canonical specs now live at `~/sd-bench-local/design/gamification-{design,client-spec,backend-spec,threat-model}.md`. The "Engine stub" section references `src/telemetry.rs` as the engine-side module — that file does not exist in the current tree; the bench/leaderboard code lives under `src/leaderboard/` (`submission.rs`, `bench_run.rs`, `bench_client.rs`, etc.). Do not implement against this document.
- Public API: n/a
- Who reads this: archival only

### `perf-98-findings.md`
Issue #98 perf-record: documents that the `--io-threads` default (`threads × 3`) has a ~2.21x cliff on Linux but is benign on Windows (NVMe + USB-HDD). Final disposition: **HOLD** the default; Linux power-users get a per-user `--io-threads ≈ threads/2` recommendation. Includes a four-regime curve-shape comparison + the v0.3.3 instrumented decision that **parallel-walk is NOT on the roadmap** (walk_ms ≈ 0 on subdir scans). Documents the canonical per-stage tracing fields `walk_ms`, `mft_ms`, `hash_io_ms`, `elapsed_ms` (sd ≥ 0.3.3).
- Public API: n/a
- Who reads this: engine (perf), benchmarker, design, sdd-testwin

### `phase-0-trait-extraction.md`
Scope doc for the BenchExecutor/SubmissionExecutor trait extraction (P0-A). Drafted 2026-05-31. Resolves Q1-Q5; lays out a workspace skeleton under `crates/superdeduper-bench-iface`, `-bench-real`, `-bench-stub` plus the leaf crates `superdeduper-hmac-signer` and `superdeduper-log`. The leaf crates exist; the iface/real/stub triad is in `crates/`. Several of the modules listed as moving into `-real` (`src/leaderboard/bench.rs`, `src/leaderboard/bench_corpus.rs`, `src/leaderboard/d7_probe.rs`) are not present in the current tree (either already moved or never created under those names — see Findings).
- Public API: n/a
- Who reads this: engine, infosec, design

### `preflight-spec.md`
Pre-flight modal (Transunion/FICO score-card aesthetic) design spec for the GUI agent. References `src/diagnose.rs::build_recommendations` as the engine-side recommendations engine; `src/diagnose.rs` exists in the tree. Submission endpoint references `https://api.superdeduper.com/v1/preflight-submit`; the engine telemetry/registration code (`src/leaderboard/registration.rs`) currently defaults to `https://api.superdeduper.io` (per `cli-flag-matrix.md` `register` row) — domain divergence between spec and current default.
- Public API: n/a
- Who reads this: GUI agent, engine, design

### `scan-options-mini-release-plan.md`
Implementation plan (dated 2026-05-24) for the `feat/scan-options` branch combining file-exclusion + T3.4 Windows Search Index. Phase A (file-exclusion) has landed (`src/exclusions/` exists); Phase B (T3.4 search-index) — `src/inventory/search_index.rs` is referenced but does not exist in the tree. Plan references a CLI flag set; `cli-flag-matrix.md` confirms the exclusion flags (`--exclusions`, `--exclusion-pack`, `--exclusion-pack-disable`, `--list-exclusion-packs`) shipped, but uses different names than the plan (no `--exclude-preset`, `--exclude-ext`, `--exclude-pattern`, `--no-exclusions`).
- Public API: n/a
- Who reads this: engine, design

### `swarm-health-check.md`
Operational runbook for `scripts/swarm-health-check.sh`. Owner: design (swarm boss). Cadence hourly via persistent Monitor. Documents the JSONL-mtime primary classifier (vs pane-text fallback), the four classification statuses (OK/STOOD_DOWN/IDLE/WEDGED_*), the serialization rule (one nudge at a time, 30s apart), env-var knobs (`SWARM_JSONL_FRESH_S`, `SWARM_HEALTH_INTERVAL_S`, `CLAUDE_PROJECTS_ROOT`, `SWARM_WORKDIR_ROOT`), and the V2 IDLE_BAD deferral.
- Public API: n/a (operational doc)
- Who reads this: design (swarm boss), operator

### `walker-fast-path-spec.md`
Block N: replace `fs::read_dir` with `FILE_ID_BOTH_DIR_INFO`-based `enumerate_dir_full` so the walker emits FileEntries with `file_ref` already populated and Stage 2b becomes unnecessary. Status: spec, not implemented. References `src/inventory/walk.rs::walk()`, `src/inventory/dir_enum.rs::enumerate_dir`, `src/pipeline/grouping::resolve_file_ids`. Caveat: `perf-98-findings.md` v0.3.3 instrumented finding shows walk is sub-millisecond on subdir scans — this work is no longer on the roadmap unless a cold-MFT whole-volume scan changes the calculus.
- Public API: n/a
- Who reads this: engine

### `perf/hdd-profile-bench-methodology.md`
Companion to `scripts/bench/Run-SdHddBench.ps1`. Documents the bench corpus (E:\ multi-folder mirrors), the engine flag inventory the harness assumes, the cache-regime enforcement options (RAMMap preferred), the optional PhysicalDisk perf-counter capture, and the cold-vs-warm trade-offs. Linked from `perf-98-findings.md` as the cold-cache HDD counterpart to that doc's warm-cache NVMe regime data.
- Public API: n/a
- Who reads this: sdd-testwin (executor), benchmarker, overflow, design

### `testing/cli-flag-matrix.md`
Rolling matrix for issue #151. Status legend (🟢/🔴/🟡/⚫); list of findings F-CLI-1..F-CLI-5; per-subcommand flag table mirroring `pub struct ScanArgs`/`DedupeArgs`/etc. from `src/cli.rs`. Documents row-execution conventions and a candidate-impl note for F-CLI-5 (Win11 ReFS Dev Drive detection).
- Public API: n/a
- Who reads this: testdesign (curates), testrunner (Linux exec), sdd-testwin (Windows exec), engine (fixes RED rows)

### `testing/gui.md`
MV-slice GUI test harness doc for `feat/g-track`. Tier-0 serde + Tier-1 widget-state + Tier-1 widget-render (egui_kittest) shipped; Tier-2 mockito + Tier-3 visual regression deferred. References specific test fns (`profile_deserialises_live_backend_shape` in `src/leaderboard/catalog.rs`; `badge_wall_classifies_granted_tiles_from_live_server_shape` in `src/gui/widgets/badge_wall.rs`) plus the egui 0.28→0.32 upgrade in commit `04297a5`.
- Public API: n/a
- Who reads this: engine GUI work, testdesign

## Invariants / Gotchas

- **Status banners are load-bearing.** Several docs (`leaderboard-spec.md`, `iocp-tier3-spec.md`, `walker-fast-path-spec.md`) declare themselves SUPERSEDED / DEFERRED / not-implemented at the top. Treating their bodies as ship-able code intent will cause re-implementation of work that was explicitly tabled.
- **Spec-to-code drift on `src/leaderboard/`.** Both `leaderboard-spec.md` (`src/telemetry.rs`) and `phase-0-trait-extraction.md` (`src/leaderboard/bench.rs`, `bench_corpus.rs`, `d7_probe.rs`) reference modules that do not exist in the current tree. Some are pre-implementation (`telemetry.rs` was a stub plan); others may have been moved into `crates/superdeduper-bench-real/` during P0-D — verify in `crates/` before chasing a "missing" module.
- **`perf-98-findings.md` is canonical for the default-multiplier HOLD.** Any future "should we change the `--io-threads` default" question must consult its four-regime table + the HOLD rationale before re-opening.
- **`testing/cli-flag-matrix.md` is the test-execution surface for #151.** When adding a CLI flag in `src/cli.rs`, the maintenance section requires adding a row here with ⚫ status. The doc explicitly does NOT track per-row test fixture paths (`tests/fixtures/cli-flag-matrix/<row-id>/`) so that path convention lives only here.
- **`swarm-health-check.md` references swarm topology constants** (`STOOD_DOWN = {czkawka, accountant}`, `EXCLUDED_WINDOWS` containing dumbo, `SWARM_JSONL_FRESH_S=120`). These live in `scripts/swarm-health-check.sh`; the doc and script must stay in sync when an agent is stood-down/up.
- **`perf/hdd-profile-bench-methodology.md` flag inventory was verified against sd v0.3.1 head (commit `6c70354`).** If a Scan flag has been renamed since, the table is stale.

## Dependencies

- INCOMING (who references docs/):
  - `scripts/bench/Run-SdHddBench.ps1` — companion to `perf/hdd-profile-bench-methodology.md`
  - `scripts/swarm-health-check.sh` — companion to `swarm-health-check.md`
  - Agent `AGENTS.md` files across the swarm reference `docs/perf-98-findings.md`, `docs/testing/cli-flag-matrix.md`
- OUTGOING (what docs/ references):
  - `src/exclusions/presets.rs`, `src/exclusions/mod.rs` (presets draft + scan-options plan)
  - `src/pipeline/iocp.rs` (iocp spec)
  - `src/inventory/walk.rs`, `src/inventory/dir_enum.rs`, `src/pipeline/grouping.rs` (walker-fast-path)
  - `src/leaderboard/*` (leaderboard, phase-0)
  - `crates/superdeduper-bench-{iface,real,stub}`, `crates/superdeduper-{hmac-signer,log}` (phase-0)
  - `src/cli.rs` (cli-flag-matrix, hdd-profile)
  - `src/diagnose.rs` (preflight-spec)
  - `~/sd-bench-local/design/*` (cross-repo: gamification + file-exclusion specs)

## Refactor Hints

- Tombstone `leaderboard-spec.md` more aggressively — the SUPERSEDED banner is already at the top, but the body still talks about a `src/telemetry.rs` that never existed. Either delete it or shrink to a single-paragraph pointer to the design specs.
- `walker-fast-path-spec.md` should get a closing status note pointing at `perf-98-findings.md`'s v0.3.3 finding ("walk is sub-millisecond; parallel-walk not on roadmap"). Currently the spec reads as if implementation is still pending.
- `scan-options-mini-release-plan.md` Phase A is shipped; Phase B (T3.4) is not. A status banner at the top would prevent treating Phase B as an active commitment.
- `phase-0-trait-extraction.md` lists 6 modules moving into `-real` (`bench_run.rs`, `bench_client.rs`, `submission.rs`-partial, `bench.rs`, `bench_corpus.rs`, `d7_probe.rs`). Verify the current state of `crates/superdeduper-bench-real/src/` and update the doc with which slices (P0-B...P0-F) actually shipped.
- `preflight-spec.md` references `api.superdeduper.com`; engine code uses `api.superdeduper.io`. Pick one and reconcile.
- Several docs lack a "last reviewed" or "last verified against commit" footer. `perf/hdd-profile-bench-methodology.md` is the model (it cites "sd v0.3.1 head, commit 6c70354").

## Wire Surfaces (if any)

Docs reference but do not own these wire surfaces:
- `POST /v1/leaderboard-submit`, `GET /v1/leaderboard/:bucket`, etc. (leaderboard-spec.md — SUPERSEDED; current canonical is in design's gamification specs)
- `POST /v1/preflight-submit` (preflight-spec.md)
- `POST /api/v1/submit`, `/bench/start`, `/bench/submit` (phase-0-trait-extraction.md — describes engine-side trait boundary, not the wire)
- Diagnose JSON schema `superdeduper.diagnose.v1` (preflight-spec.md — owned by `src/diagnose.rs`)

CLI surfaces documented (canonical surface in `src/cli.rs`):
- All scan/dedupe/cache/drive-info/diagnose/register/config/achievements/account/submit-pending/bench-me/scan-history/debug flags (testing/cli-flag-matrix.md)
- Bench-only flags relevant to HDD profiling: `--io-threads`, `--threads`, `--no-cache`, `--format`, `--min-size`, `--max-size`, `--placeholders-only`, `--force-hash` (perf/hdd-profile-bench-methodology.md)

Environment variables:
- `SWARM_HEALTH_INTERVAL_S`, `SWARM_JSONL_FRESH_S`, `CLAUDE_PROJECTS_ROOT`, `SWARM_WORKDIR_ROOT` (swarm-health-check.md)
- `SUPERDEDUPER_CHANNEL=local` (perf/hdd-profile-bench-methodology.md — recommended for airgapped bench runs)

## Non-source artifacts

None. All files in this tree are markdown.
