# FINDINGS_FOLLOWUP.md — second-pass coverage closure

This is the follow-up rollup to [FINDINGS.md](./FINDINGS.md) (the first-pass
25-agent audit). It covers the seven subtrees the original sweep did not
visit: `.github/workflows/`, `examples/`, `data/`, `schema/`, `docs/perf/`,
`docs/testing/`, and `specs/historical/`. Each subtree got a per-directory
audit agent that wrote a fresh `AGENTS.md` and returned structured findings;
this document deduplicates them and groups by severity.

Totals across the seven follow-up subtrees:

- P1 (load-bearing): 0
- P2 (meaningful drift): 19
- P3 (cosmetic): 7
- Info (refactor opportunities): 18

The headline is doc-drift, not code defects. The bulk of the P2 surface is in
`docs/TESTING.md` (six rows describing CI workflows / coverage gates that do
not exist) and in `schema/submit.schema.json` (schemars doc-comment artifacts
plus stale "lives in module X" cross-refs after the bench-iface crate split).
No engine wire/format issue would break by acting on this follow-up's
findings; the risk is that a refactorer reading the docs trusts a stale claim
and codes against a ghost.

---

## 1. P1 — Load-bearing issues

None. The follow-up pass found zero P1-severity issues across all seven
subtrees.

---

## 2. P2 — Meaningful drift

### 2.1 `.github/workflows/` (7)

Doc-drift between `docs/TESTING.md` §6 and the actual CI YAML.

```
TESTING.md:214  - claims ci.yml has a 4-row Windows VHD/ReFS/non-admin + ubuntu matrix
                  reality: fmt + feature-flag-drift + clippy + test-linux + test-windows;
                  Windows leg is a single non-admin run with no VHD/ReFS coverage
TESTING.md:231  - claims a separate perf.yml nightly workflow
                  reality: no perf.yml exists in .github/workflows/
TESTING.md:164  - claims a bench-vs-fclones.yml workflow posting PR delta comments
                  reality: no such workflow exists
TESTING.md:228  - claims clippy::pedantic with an opt-out allow-list
                  reality: ci.yml runs plain `cargo clippy -- -D warnings` across four feature combos
TESTING.md:225  - claims cargo-llvm-cov 85% coverage gate on pipeline::* / winapi_wrappers::*
                  reality: no coverage tooling invoked in any workflow
TESTING.md:246  - cites `cargo test --test windows_integration` for VHD tests
                  reality: test-windows runs `cargo test --lib --bins` + `cargo test --test smoke`
.github/workflows/release.yml:458
                - scripts/check-feature-flag-consistency.sh tuple list covers only
                  (windows,cli|gui) (linux,cli|gui); release.yml now also builds macOS
                  (x86_64+aarch64) and FreeBSD with their own --features strings, invisible
                  to the drift gate. (scripts/AGENTS.md already flags the macOS half;
                  FreeBSD is the same gap extended.)
```

Recommended fix: `docs/TESTING.md` §6 needs a from-scratch rewrite against
the actual two-workflow CI surface; the feature-flag-drift script needs
macOS + FreeBSD tuples added (or refactored to enumerate target legs from
the YAML directly).

### 2.2 `examples/` (1)

```
examples/hash_microbench.rs:59
  - Variable `median_ns_per_byte` and the line-62 banner labeled "median" are
    actually the arithmetic mean (total_ns / (trials * len)).
    Fix: either compute a real median (sort per-trial ns/byte, pick middle)
    or rename to `mean_ns_per_byte`.
```

### 2.3 `schema/` (3)

```
schema/submit.schema.json:5
  - HardwareFingerprint description says struct lives in `leaderboard::hardware`.
    Canonical home is now `crates/superdeduper-bench-iface/src/lib.rs:289`;
    engine just re-exports via `pub use`.
    Same drift on RunShape (line 150) and ResultSummary (line 83) — both still
    point at `leaderboard::submission`.
schema/submit.schema.json:47
  - `is_dev_drive.title` = "130 Pathfinder Phase 3 — `true` when ANY local volume on"
    is the orphan first line of a Rust doc comment, promoted by schemars into
    `title` while the rest stays in `description`. Same artifact on
    `dry_run.title` (line 170) and `groups_reviewed_count.title` (line 198).
schema/submit.schema.json:248
  - `zero_byte_group_max.description` carries a G1.x preamble that in the Rust
    source describes the whole group of esoteric metric fields, not just this
    one. schemars associated the doc paragraph to the first field below it.
```

Recommended fix: update doc-comment placement in
`crates/superdeduper-bench-iface/src/lib.rs` so the schemars-generated
`title` field is a clean one-liner; regenerate the schema with
`SD_UPDATE_SCHEMA=1 cargo test`.

### 2.4 `docs/perf/` (3)

```
docs/perf/hdd-profile-bench-methodology.md:76
  - Shows stderr timing banner as `--- timing (river5) ---`.
    src/main.rs:2198 emits `--- timing ({}) ---` where {} is `cfg.hash_algo.tag()`
    producing e.g. `river5-aes-ni` or `river5-stub-xxh3`. A harness grepping
    for the literal `(river5)` misses every modern run.
docs/perf/hdd-profile-bench-methodology.md:48
  - `--io-threads` default row says `threads × 3 (~96 on a 32-CPU box)` with
    no mention of the HDD auto-cap at 16 (src/cli.rs:506-513). On Mick's NEO
    E:\ rotational drive the actual default is 16, not ~96; the sweep grid
    1,2,4,8,16,32,96 framing is off.
docs/perf/hdd-profile-bench-methodology.md:48
  - Doc predates src/pipeline/io_threads_probe.rs (v0.3.31+ probe-once
    auto-bracket {1,4,8,16}). Bench harness uses explicit --io-threads so the
    probe is bypassed, but the "Engine flag inventory" + "Flags that DO NOT
    exist" sections need a note that 'engine default' now means 'probe-picked'.
```

### 2.5 `docs/testing/` (3)

```
docs/testing/cli-flag-matrix.md:81
  - scan flag table omits --force-mft (ScanArgs.force_mft, src/cli.rs:539).
    Matrix sweep will not exercise the MFT direct-enum fast path.
docs/testing/cli-flag-matrix.md:81
  - scan flag table omits --parallel-roots (ScanArgs.parallel_roots,
    src/cli.rs ~line 550, v0.3.23). Matrix sweep will not exercise multi-root
    rayon par_iter walk.
docs/testing/cli-flag-matrix.md:81
  - scan flag table omits --cold-enforced (ScanArgs.cold_enforced).
    Matrix sweep will miss the cold-cache enforcement knob.
```

### 2.6 `specs/historical/` (2)

```
specs/historical/egui-kittest-scan-perf-mick-shape.md:1
  - File lives under historical/ but header reads as a live ratified SHIP gate
    ("STANDING SHIP GATE for v0.3.39+", "SPEC RATIFIED 2026-06-05"). No
    HISTORICAL/SUPERSEDED banner. Add a top-of-file callout:
    "HISTORICAL — superseded by absolute-wall matrix methodology (v0.3.40+)."
specs/historical/egui-kittest-scan-perf-mick-shape.md:136
  - §3 prescribes ratio ratchet 3.0x -> 2.0x -> 1.5x -> 1.2x. Per
    [[feedback_perf_ship_gate_absolute_wall_over_ratio]] this gate was
    structurally bounded by ~30s GUI startup tail on small baselines and
    replaced by absolute-wall criteria (<=90s OR <=3.5x on Mick-corpus for
    v0.3.39; <=1.10x cold-GUI/cold-CLI for v0.3.40).
```

---

## 3. P3 — Cosmetic

- `scripts/AGENTS.md:267` — Refactor Hints says `build-macos:` is at lines
  267-319; actual span is 267-363, build-freebsd starts at 365. Upper bound
  stale.
- `schema/submit.schema.json:67` — HardwareFingerprint description claims
  `additionalProperties: false` is enforced; the generated schema does not
  emit that keyword. Web-side Zod is the actual gate.
- `schema/submit.schema.json:10` — All u32/u64 fields carry `"minimum": 0.0`
  (float zero) rather than integer zero. Schemars artifact, harmless.
- `docs/perf/hdd-profile-bench-methodology.md:260` — Linkage section
  references `docs/perf-98-findings.md` and `docs/testing/cli-flag-matrix.md`
  as repo-root-relative; correct relative paths from `docs/perf/` are
  `../perf-98-findings.md` and `../testing/cli-flag-matrix.md`.
- `docs/testing/cli-flag-matrix.md:129` and `:134` — "ConfigCommand enum at
  cli.rs:1034" cited twice; actual definition is at `src/cli.rs:1086`.
- `specs/historical/egui-kittest-scan-perf-mick-shape.md:189` — Two `## 5.`
  headers (line 175 + line 189); latter should be §5b or §6.

---

## 4. Info — Refactor opportunities

### 4.1 CI / workflow ergonomics

- `release.yml:365` — FreeBSD VM is build-only; the in-file comment notes it
  "is capable of running cargo test if we ever want native FreeBSD tests."
  Adding even a `cargo test --lib` smoke would catch FreeBSD-specific
  regressions before tagging.
- `ci.yml:9` (and `release.yml:25`) — env-block comment is purely
  retrospective about a removed `-D warnings` setting. Once egui deprecations
  are cleaned, shrink to a one-liner.
- `ci.yml:155` — Windows artifact name uses `${{ github.sha }}`. If trigger
  ever flips to tag-push, `github.ref_name` gives a more recognizable name.
- `release.yml:47` — Windows matrix has a single entry (aarch64-pc-windows-msvc
  was removed pending river5 AES-NI work). 1-entry matrix is fine; keeping
  the matrix shape makes the re-add trivial.
- `release.yml:98` — `if: ${{ env.CODE_SIGN_PFX_BASE64 != '' }}` resolves
  against an env declared in the step's own `env:` block (lines 100-102).
  Works; a one-line comment would protect future refactors.

### 4.2 Bench / examples shape

- `examples/hash_microbench.rs:68` — `SIZE` (100 MiB) and `TRIALS` (7) are
  const-baked. `SDD_BENCH_SIZE_MB` / `SDD_BENCH_TRIALS` env reads would let
  perf runs sweep without recompiling.
- `examples/hash_microbench.rs` — `src/pipeline/audio_hash/AGENTS.md:156`
  notes the absence of an audio-hash bench example; `examples/` is the
  natural home for `audio_hash_microbench.rs`.
- `examples/hash_microbench.rs:50` — All output is on stderr (intentional —
  keeps stdout clean). A one-line comment would prevent a future "fix" to
  println!.

### 4.3 Catalog / classifier hygiene

- `data/cpu-brackets-catalog.json:57` — high-end pattern
  `apple\s*m[234]\s*(max|pro)?` has the suffix group optional, so bare
  "Apple M3" / "Apple M4" match high-end even though flagship's
  `apple\s*m[34]\s*(max|ultra)` covers Max+Ultra. Disambiguation relies on
  classifier iteration order (flagship checked first per `display_order`).
  Document the ordering contract in `cpu_brackets.rs`.
- `data/cpu-brackets-catalog.json:3` — `classifier_version=4` is hard-coded;
  no CI check that it matches the live `/api/v1/cpu-brackets/catalog`
  endpoint. Consider a release-time curl-and-diff (memory
  `[[reference_achievements_catalog_yaml]]` records the same pattern).
- `data/cpu-brackets-catalog.json:22` — Several patterns are unanchored
  fragments (e.g. `core\s*ultra\s*9\s*2` for 200-series). Brittle if Intel
  ships a 3xx series in the same naming scheme. Document or anchor with `\b`.

### 4.4 Schema regen ergonomics

- `schema/submit.schema.json:270` — Top-level description tells reader to
  regen via `SD_UPDATE_SCHEMA=1 cargo test`, but does not name the test or
  mention the bench-iface crate's `telemetry` feature. A one-line hint
  (`cargo test -p superdeduper-bench-iface --features telemetry schema_regen`
  or similar) saves a refactorer cycle.
- `schema/submit.schema.json:2` — Schema declares `draft/2019-09`. Modern
  tooling defaults to draft 2020-12; bump when schemars + web-side validator
  allow.

### 4.5 Perf / testing docs lifecycle

- `docs/perf/hdd-profile-bench-methodology.md:267` — §"Open questions" has
  been open since 2026-05-31. After v0.3.31 io_threads probe + design-routed
  HDD work, Q2 + Q3 are stale; close or re-route to `perf-98-findings.md`.
- `docs/perf/hdd-profile-bench-methodology.md:1` — Single-shot methodology
  note for a specific routed run; once sdd-testwin results fold into
  `perf-98-findings.md`, move to `docs/archive/` or add a "STATUS: superseded
  by perf-98-findings.md §<row>" banner.
- `docs/testing/cli-flag-matrix.md:246` — Manual enumeration drifts (see
  P2). Consider a clap-introspection build-time check that diffs registered
  args against the doc and warns on missing rows while preserving the
  hand-curated Status column.
- `docs/testing/gui.md:11` — Commit SHAs ce0ea9f / 03b07a8 / 04297a5 /
  64e70f2 cited as historical anchors; not verified in this audit. If any
  branch was rebased, refs no longer resolve.

### 4.6 Historical spec maintenance

- `specs/historical/egui-kittest-scan-perf-mick-shape.md:265` — §9 HANDOVER
  memory pin marked "drafted; lands when ratified" for
  `feedback_profile_profile_profile.md`. Verify status; link it or note it
  never landed.
- `specs/historical/egui-kittest-scan-perf-mick-shape.md:204` — §6 lists
  open items (corpus exact spec, location strategy, first-run baseline,
  cross-platform run). As a historical doc these will never be resolved
  in-file; annotate which were resolved elsewhere (matrix harness) or
  accept as archival open-state.

---

## 5. Per-directory summary

| Path                        | Files | Lines | P1 | P2 | P3 | Info | AGENTS.md                                                                                          |
|-----------------------------|-------|-------|----|----|----|------|----------------------------------------------------------------------------------------------------|
| `.github/workflows/`        | 2     | 683   | 0  | 7  | 1  | 4    | [`.github/workflows/AGENTS.md`](./.github/workflows/AGENTS.md)                                     |
| `examples/`                 | 1     | 81    | 0  | 1  | 0  | 3    | [`examples/AGENTS.md`](./examples/AGENTS.md)                                                       |
| `data/`                     | 1     | 122   | 0  | 0  | 0  | 3    | [`data/AGENTS.md`](./data/AGENTS.md)                                                               |
| `schema/`                   | 1     | 284   | 0  | 3  | 2  | 2    | [`schema/AGENTS.md`](./schema/AGENTS.md)                                                           |
| `docs/perf/`                | 1     | 282   | 0  | 3  | 1  | 2    | [`docs/perf/AGENTS.md`](./docs/perf/AGENTS.md)                                                     |
| `docs/testing/`             | 2     | 327   | 0  | 3  | 2  | 2    | [`docs/testing/AGENTS.md`](./docs/testing/AGENTS.md)                                               |
| `specs/historical/`         | 1     | 290   | 0  | 2  | 1  | 2    | [`specs/historical/AGENTS.md`](./specs/historical/AGENTS.md)                                       |
| **Totals**                  | **9** | **2069** | **0** | **19** | **7** | **18** | — |

---

## 6. Coverage closure

With this follow-up pass, the audited directory set for source-bearing
content is complete. Together, FINDINGS.md (25 agents, ~25 subtrees) and
FINDINGS_FOLLOWUP.md (7 agents, 7 subtrees) cover every directory that
contains hand-authored code, configuration, schema, or documentation.

Deliberately skipped (non-source / generated / binary):

- `diagnostics/` — generated dump captures (per-run JSON traces from
  egui_kittest + perf harness); content is run-output, not source.
- `assets/` — binary icon / image resources; no review surface beyond
  presence-check.
- `tests/fixtures/` — binary corpus and synthetic blobs (audio, image,
  filesystem snapshots) referenced by test code that was itself audited
  under `tests/` in FINDINGS.md.
- `target/`, `.git/`, lockfiles — standard build / VCS artifacts.

If a future pass wants to bottom out the last 1% of the tree, the only
remaining "source-like" candidate is a spot-check on `assets/` to confirm
that every PNG / ICO referenced by `src/gui/` actually exists at the path
the code expects — but that is an availability check, not an audit, and
belongs to the GUI test suite, not this rollup.
