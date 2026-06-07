# egui_kittest scan-perf MICK-SHAPE cell — SHIP gate

> Author: testdesign 2026-06-05 14:00 PDT. Routed to: sdd-testwin
> (cell implementation) + engine (corpus generator parameterization).
> Status: SPEC RATIFIED 2026-06-05 13:55 PDT per design Option B
> two-cell approach.
>
> Source ask: design 2026-06-05 10:40 PDT — cell PASS at 2.14x on
> 1040-file synthetic-corpus DID NOT transfer to Mick's 312K-file
> C:\sdd-tests user experience (still ~5min on v0.3.38 = same as
> v0.3.33 partial-fix state). The synthetic-cell is necessary but NOT
> sufficient for production ship.

## 0. Purpose — STANDING SHIP GATE for v0.3.39+

This cell is the **SHIP gate** for GUI-perf-touching ships going
forward. Replaces the synthetic-cell's role as ship gate (synthetic
cell stays as engineering-iteration gate).

**Per-ship workflow** (two-gate):
1. Engine iterates fast on synthetic-cell during perf-candidate work
   (seconds per run; catches incremental regressions).
2. When synthetic-cell PASSES + engine wants to cut a tag: run
   Mick-shape cell.
3. Mick-shape cell PASS → push tag → auto-promote → ship.
4. Mick-shape cell FAIL → engine investigates corpus-scale cost;
   iterate; cycle through both gates.

**Why two cells**:
- Synthetic cell (1040 files; deterministic seed 0xC0FFEE; ~seconds
  per run): fast feedback loop for engine iteration; catches regression
  classes that don't depend on production-scale corpus.
- Mick-shape cell (50-100K mixed files; deterministic; ~minutes per
  run): production-realistic; catches corpus-scale costs that synthetic
  doesn't expose (e.g., badge_wall growing with results state; body
  cost scaling with file count).

**Per [[feedback-gui-tests-primary-gate-not-mick-eyeball]]** policy:
both cells are PRIMARY GATES; Mick eyeball is rarity.

**Per HANDOVER lesson 2026-06-05 13:48 PDT**: "When a cell PASSES but
corpus differs materially from production, HOLD ship signal pending
production-shape verification." This cell IS the production-shape
verification.

## 1. Cell shape

```rust
// tests/gui_tier_a_linux.rs (or separate test file)
#[test]
fn tier_a_gui_scan_perf_mick_shape_within_cli_ratio() {
    let _env = env_lock();
    let temp_data_dir = TempDir::new().expect("test data dir");
    std::env::set_var("SUPERDEDUPER_TEST_DATA_DIR", temp_data_dir.path());

    // Corpus: pre-staged OR generated on first invocation (cached).
    // See §2 for shape; engine owns generator per design ratification.
    let corpus = generate_or_load_mick_shape_corpus(&temp_data_dir);

    // CLI warmup (same pattern as synthetic cell).
    let _ = run_cli_scan(&corpus, &["--no-cache"]);

    // CLI timed (warm).
    let t_cli_start = std::time::Instant::now();
    run_cli_scan(&corpus, &["--no-cache"]);
    let t_cli = t_cli_start.elapsed();

    // GUI via egui_kittest in-process (corpus warm from CLI).
    let t_gui_start = std::time::Instant::now();
    let (mut harness, _ctx) = build_eframe_harness();
    seed_root_via_egui_kittest(&mut harness, &corpus);
    invoke_start_scan(&mut harness);
    let total_step_time = wait_for_scan_complete_with_step_timing(&mut harness);

    // Ratio assert (use total_step_time per spec §4 O3 metric).
    let ratio_bound: f64 = std::env::var("SUPERDEDUPER_TEST_PERF_RATIO_MICK_SHAPE")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let observed_ratio = total_step_time.as_secs_f64() / t_cli.as_secs_f64();

    if observed_ratio > ratio_bound {
        dump_widget_breakdown(&harness); // sub-widget telemetry per c04f4d1
        panic!("MICK-SHAPE: GUI step_time {:.2}s > CLI {:.2}s * {:.1}x bound (observed {:.2}x)",
            total_step_time.as_secs_f64(), t_cli.as_secs_f64(), ratio_bound, observed_ratio);
    }
}
```

**Reuses existing patterns**:
- `env_lock + isolated_env + SUPERDEDUPER_TEST_DATA_DIR` (working
  post v0.3.37 Path A landing).
- `build_eframe_harness` (existing tier_a_app cell).
- `click_all('Continue') + click_all('Start scan') + 400-frame poll`
  for scan-complete signal.
- O3 metric `total_step_time` (per existing spec §4 wall-time
  discipline post 19:54 PDT rewrite).
- `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1` infrastructure for diagnostic
  dump on fail (per c04f4d1 widget spans + perf-update phase
  breakdown).

**Differences from synthetic cell**:
- Larger corpus (50-100K vs 1040).
- Configurable ratchet via separate env var
  `SUPERDEDUPER_TEST_PERF_RATIO_MICK_SHAPE` (independent of synthetic
  cell's `SUPERDEDUPER_TEST_PERF_RATIO`).
- Longer per-run wall time (minutes vs seconds).

## 2. Corpus shape — MICK-SHAPE

Target: production-realistic; matches Mick's C:\sdd-tests + similar
user-corpora shape (dev-tree + media + mixed).

| Bucket | Count | Size | Content |
|--------|-------|------|---------|
| Small (dev-tree shape) | 35-70K | 4KB-64KB; many size-twins (tier 1 + 2 exercise) | random with deterministic per-file collision pattern |
| Medium (mixed shape) | 12.5-25K | 64KB-10MB | random; few size-twins |
| Large (media shape) | 2.5-5K | 10MB+; few size-twins | random; tier 3 hash work dominates |
| Total | 50-100K files | ~10-50 GiB on disk | exercises tier 1 + 2 + 3 + badge_wall results-state |

**Deterministic PRNG seed**: e.g., `0xCAFEBABE` (or whatever engine
picks; documented in spec).

**Generation strategy**:
- Pre-staged once per machine; cached at known path under
  TempDir::new() OR a stable path (e.g.,
  `%TEMP%\sdd-mick-shape-corpus-v1`).
- Regeneration only on cell-version bump (spec §6 versioning).
- Stage time ~1-5 min one-time setup; acceptable.

**Engine ownership** (per design 13:55 PDT): engine owns the corpus
generator. Same machinery that engine's planning to parameterize
to 10K as Step 1 of their diagnostic plan produces this 50-100K
corpus. testdesign owns the SPEC; engine owns the GENERATOR
implementation.

## 3. Ratchet bound (N)

**Start at N=3.0x** (same as synthetic). Configurable via
`SUPERDEDUPER_TEST_PERF_RATIO_MICK_SHAPE` env var.

**Tightening plan** (post-data):
- 3.0x: current; production-shape ship gate during iteration.
- 2.0x: post-stabilize (engine + design call when data accumulates).
- 1.5x: target steady-state.
- 1.2x or parity: stretch.

**One-way ratchet**: no relaxation without explicit design + Mick
review. Same policy as synthetic cell.

**Note on initial bound**: 3.0x may be too tight OR too loose for
Mick-shape; first multi-run data will calibrate. If initial runs land
30x+ on production-realistic corpus, engine has substantial work
ahead (mirrors original 60x → 16-18x → 2x cycle).

## 4. Workflow — when this cell runs

**NOT run per-iteration** (too slow).

**Run frequency**:
- **Engine cuts tag**: run Mick-shape cell pre-tag → PASS = push tag.
- **Ratchet tightening proposal**: run Mick-shape cell to confirm
  improvement before tightening.
- **Investigation diagnostic**: run with
  `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1` for widget-level breakdown
  on production-realistic workload.

**Who runs**:
- **Engine** runs pre-tag (engineer's own machine or CI).
- **sdd-testwin** runs as ship-gate verification on Windows hardware
  (matches Mick's hardware class better than engine's box).
- **Mick** runs on actual C:\sdd-tests when convenient (corpus
  generator may not exactly match his actual files; cell measures the
  cell-corpus shape; Mick's actual workload may differ further).

## 5. Engine + testdesign ownership split

Per design 13:55 PDT ratification:

- **Engine owns**: corpus generator (`fn generate_mick_shape_corpus`
  parameterized to 50-100K; deterministic seed; same machinery as
  10K Step 1 diagnostic generator). Lives in engine repo
  (`src/test_support/` or similar).
- **testdesign owns**: spec (this file) + cell catalog entry +
  per-version ratchet target.
- **sdd-testwin owns**: cell implementation in
  `tests/gui_tier_a_linux.rs` (or separate test file) + run
  invocation + ship-gate report format.

## 5. Diagnostic-on-fail

When cell FAILS, dump:
1. Observed wall-times + ratio + cal telemetry.
2. **Widget-level breakdown** (per c04f4d1 sub-widget spans):
   - cache_banner / scan_mode / roots / funnel / badge_wall mean+max.
   - If badge_wall dominates with corpus scale: corpus-scale-dependent
     widget cost confirmed.
3. **Comparative**: same metrics from synthetic-cell run (if available)
   to isolate corpus-scale-only growth.
4. **Engine diagnostic line**: `GUI scan: io-threads selected = N` +
   any other engine-emitted perf telemetry.

Engine grep on CI failure.

## 6. Pending / open

- **Corpus exact spec** (file-size distribution; total files; total
  GiB on disk): engine call based on profiling Mick's actual
  C:\sdd-tests histogram. Currently 50-100K is the design-bracket;
  finalize to specific number post-Mick-diagnostic.
- **Corpus location strategy**: pre-staged at stable path OR
  TempDir-per-run with caching? Cell run-time penalty trade-off.
- **First-run baseline**: what ratio does the current binary
  (v0.3.38) hit on this corpus? Calibrates the initial ratchet bound.
- **Cross-platform run**: cell needs to compile/run on Windows
  (sdd-testwin's egui_kittest harness already validated post-v0.3.37
  Path A); Linux for CI cross-validation.

## 7. Spec §4 update on synthetic cell (cross-reference)

The existing `egui-kittest-scan-perf-assertion.md` spec §4 should add
a "what this cell PROVES vs DOESN'T" subsection (ratified by design
2026-06-05 13:55 PDT):

> **What the synthetic-cell PROVES**:
> - GUI scan ratio `T_gui/T_cli` on a specific 1040-file deterministic
>   corpus (seed 0xC0FFEE; 500 unique + 250 size-twin pairs + 20 dup
>   pairs).
> - Engine perf state at the cell-corpus scale.
> - Cross-platform widget shape (sidebar/badge_wall dominance
>   preserved across Linux+Windows+debug+release).
> - Engine perf regressions inside the cell-corpus scale (catches
>   hardcoded magic numbers, cpu*3 io_threads, etc).
>
> **What the synthetic-cell does NOT PROVE**:
> - Generalized production-corpus speedup.
> - User-visible improvement on Mick-shape or larger corpora.
> - Body-cost scaling behavior (whether sidebar/badge_wall renders
>   constant-per-frame or proportional-to-results).
> - Cold-cache + cold-disk realism (cell is warm; production may be
>   cold).
>
> **The Mick-shape cell** (specs/egui-kittest-scan-perf-mick-shape.md)
> covers the corpus-scale gap. SHIP gate = Mick-shape cell PASS.
> Synthetic-cell remains the engineering-iteration gate.

## 8. Provenance

- Design URGENT routing 2026-06-05 10:40 PDT (cell PASS at 2.14x on
  synthetic did NOT transfer to Mick's 312K-file C:\sdd-tests user
  experience; Option A/B/C/D ask).
- testdesign Option B call 2026-06-05 13:48 PDT (two-cell approach
  + mea culpa for 08:46 PDT corpus-mismatch caveat downgrade).
- Design ratification 2026-06-05 13:55 PDT (Option B; spec §4
  articulation; engine corpus generator ownership; Mick-shape cell
  becomes SHIP gate).
- HANDOVER lesson 2026-06-05 13:48 PDT: "When cell PASSES but corpus
  differs materially from production, HOLD ship signal pending
  production-shape verification."
- v0.3.38 ship bytes already out (sdd-builds + mickfixesjunk + GH
  Latest per design 13:55 PDT); perf claim overstated; future cuts
  gate on this Mick-shape cell.
- Engine c04f4d1 widget instrumentation infrastructure (sub-widget
  spans inside SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1) — reusable for
  this cell's diagnostic-on-fail.

## 9. HANDOVER memory pin (drafted; lands when ratified)

`feedback_profile_profile_profile.md` content (combined post-mortem
across multiple sprint lessons):

> Test profile + test corpus + test platform ALL need to match
> production target. cargo test --release may not honor profile
> inheritance on workspaces with custom profile config. WSL is not a
> reliable proxy for Windows on egui-stack code. 1040-file synthetic
> corpus is not a reliable proxy for Mick-shape production corpora.
>
> Discipline:
> - Multi-profile cross-check (release + release-debug + debug; flag
>   >5x absolute delta).
> - Cal telemetry sanity (avg_step <2ms in release; debug-profile
>   inflates to 4-5ms).
> - WSL+release-debug vs Windows+release vs Windows+release-debug.
> - cargo flamegraph adds ~+40% ETW sampling overhead; subtract from
>   ratchet comparison.
> - When cell PASSES but corpus differs materially from production,
>   HOLD ship signal pending production-shape verification.
>
> Cost of one delay round-trip is much cheaper than shipping a
> fix-that-isn't.

Will land in `/home/neomatrix/.claude/projects/-home-neomatrix--giga/memory/feedback_profile_profile_profile.md` once design ratifies the memory text.
