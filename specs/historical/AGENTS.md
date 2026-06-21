# specs/historical — AGENTS.md

## Purpose

Archived superseded specs. Kept for provenance/lineage of ship-gate
evolution; not the live source-of-truth. Live specs sit one level up at
`specs/`.

## Files

- `egui-kittest-scan-perf-mick-shape.md` — Mick-shape (50-100K corpus)
  GUI scan-perf cell spec, drafted 2026-06-05 as the SHIP gate for
  v0.3.39+. Superseded by the absolute-wall production-shape matrix
  methodology adopted in v0.3.40 (see
  `feedback_dev_loop_matrix_canonical_pr_first.md` and
  `feedback_perf_ship_gate_absolute_wall_over_ratio.md`).

## Invariants

- Files here MUST be clearly labeled historical / superseded; they
  should not be mistaken for live ship gates.
- Engine + sdd-testwin tooling should not be wired against this spec —
  ratios cited here (3.0x→1.5x ladder) were retired in favor of
  absolute-wall criteria on Mick-corpus.

## Dependencies

Cross-references to:
- `specs/egui-kittest-scan-perf-assertion.md` (synthetic cell; live).
- v0.3.39 perf chain memories (feedback_perf_ship_gate_absolute_wall_
  over_ratio, feedback_matrix_methodology_align_with_user_perception,
  io-threads-probe-lottery-mick-corpus-variance).
- engine c04f4d1 widget instrumentation
  (`SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1`).
- env vars: `SUPERDEDUPER_TEST_DATA_DIR`,
  `SUPERDEDUPER_TEST_PERF_RATIO_MICK_SHAPE`,
  `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE`.

## Refactor Hints

- The spec is NOT marked "HISTORICAL / SUPERSEDED" in its header; only
  the parent directory name signals archival. Anyone landing on the
  file via grep could mistake it for live. Recommend a top banner.
- Section numbering has two §5 headers (line 175 + line 189) — minor
  drift inherited at archival time; safe to leave or correct.
- The `feedback_profile_profile_profile.md` memory pin in §9 was
  drafted "lands when ratified"; verify whether it ever landed
  before treating §9 as accurate.

## Wire Surfaces

None live. All knobs documented here (env vars, ratchet bound,
generator entry point) are historical proposals; check live specs +
`tests/gui_tier_a_linux.rs` for current truth.
