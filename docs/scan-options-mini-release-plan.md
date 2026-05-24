# Scan-options mini-release — implementation plan

**Date:** 2026-05-24
**Scope:** File exclusion (Settings → Exclusions + CLI flags) + T3.4 Windows Search Index integration
**Estimate:** ~10-13 eng-days combined (6-8 for exclusion, 4-5 for T3.4)
**Branch:** `feat/scan-options` (new; off `feat/g-track` once G1 verifies)

## 1. Why one release

Both features sit at the **scan-input layer**: they shape what the walker/dedupe pipeline sees. Different surfaces (one filters, one substitutes), but they share the same configuration surface (CLI flags + Settings UI) and ship cleanest as a single "you control what gets scanned + how" narrative.

Mick + design's call (channel 2026-05-24T07:41Z): land them in parallel, not serially.

## 2. Module landing zones

### File exclusion

```
src/exclusions/
  mod.rs              -- ExclusionPolicy struct + ExclusionPolicy::evaluate(path) -> Excluded | Included
  presets.rs          -- the 8 preset packs as compile-time const data
  matcher.rs          -- glob matching + extension matching (globset, already a dep)
  config.rs           -- TOML serde shape for the [exclusions] config section
```

**Walker hook:** add a single call in `src/inventory/walk.rs::push_entry` (or wherever the include/exclude glob check already lives, ~line 288). The existing include/exclude already runs path-pattern matches; we extend it with extension + preset-pack matches. Drop on match; increment counter.

**Counter wiring:** new `ExclusionCounters { excluded_files: AtomicU64, excluded_bytes: AtomicU64 }` on the same shared-Arc surface the existing `EngineCounters` use. Surfaces in scan summary as "X files (Y bytes) excluded by Settings → Exclusions."

### T3.4 Windows Search Index

```
src/inventory/search_index.rs    -- Windows-only module; cfg(windows)-gated
  -- ISearchManager + OLE DB query wrapper (COM)
  -- filter DSL → Windows Search SQL translator
  -- coverage check (ISearchCatalogManager::GetCatalog status)
  -- streams candidate paths into the existing inventory shape
```

**Inventory hook:** `src/inventory/mod.rs::enumerate` gains a new arm — if `cfg.use_search_index == true`, dispatch to `search_index::query_candidates(filter)` instead of walking. Both paths produce `Vec<FileEntry>`; the rest of the pipeline downstream is unchanged.

**No GUI surface in v1** per the T3.4 spec (CLI-only "power-user feature"). Saves ~2 days; revisit when T1.2 image-similarity lands and the natural workflow "Find duplicate photos via index filter" becomes a GUI button.

### Shared

Both features touch:

- `src/cli.rs::ScanArgs` — new flags (`--exclusions`, `--exclude-preset`, `--exclude-ext`, `--exclude-pattern`, `--no-exclusions` for exclusion; `--use-search-index`, `--filter`, `--rebuild-index-first` for T3.4)
- `src/config.rs::ScanConfig` — new fields, plumbed through `from_args` with validation
- `src/output.rs::summarize` — new summary field for "excluded_files" + "excluded_bytes"

No actual code sharing beyond the cfg-plumbing pattern. The implementations are independent.

## 3. Sequence within the release

### Phase A — File exclusion (~6-8 days)

Cross-platform, biggest UI surface, the user-visible piece. Land first.

1. **Day 1 — config + matcher core.**
   - `src/exclusions/mod.rs` + `matcher.rs` + `config.rs`
   - TOML serde shape under `[exclusions]`
   - `ExclusionPolicy::evaluate` returning `Excluded { reason: PresetPackId | CustomExt | CustomPattern } | Included`
   - Unit tests for matcher (extension hit, path hit, no-match, both off)
2. **Day 2 — preset packs (data) + walker hook.**
   - `presets.rs` with the 8 packs as `const` data structures
   - Walker call at `src/inventory/walk.rs` (extends existing globset filter)
   - Counter wiring → `EngineCounters`
3. **Day 3 — CLI flags + config persistence.**
   - All 5 new ScanArgs flags
   - `sd config set exclusions.*` subcommands (extends the existing `sd config` skeleton)
   - Integration tests against synthetic dirs (`.dll`, `node_modules/`, custom patterns)
4. **Days 4-5 — GUI Settings → Exclusions tab.**
   - Master toggle
   - 8 preset-pack rows with expand-arrow showing contents
   - Custom extensions list (add/remove rows)
   - Custom patterns list (add/remove rows)
   - Footer: "Excluded last scan: N files (Y bytes)"
5. **Day 6 — tests + scan-summary line + log-tab counter surface.**
   - All 12 acceptance criteria covered
   - Cross-build green
6. **Optional Day 7-8 — polish + acceptance-criterion validation against testwin.**

**Acceptance gate before moving to Phase B:** all 12 criteria from `file-exclusion-spec.md` §3 pass on both Linux + Windows.

### Phase B — T3.4 Windows Search Index (~4-5 days)

Windows-only, smaller, can start in parallel from Day 3 if I'm not bottlenecked on the GUI work. Realistically sequencing as Days 7-11 once Phase A is done.

1. **Day 1 — `ISearchManager` COM wrapper.** Read-only query; no mutation. Hand-rolled via `windows` crate (already a dep). Tested via a hardcoded query against `SystemIndex`.
2. **Day 2 — filter DSL → SQL translator.** Five filter shapes per spec §3.2. Tests against synthetic filter strings.
3. **Day 3 — inventory integration + coverage check.** Hook into `inventory::enumerate`; dispatch on `cfg.use_search_index`. Coverage check (`CATALOG_STATUS_INDEXING`) warns user.
4. **Day 4 — placeholder respect + acceptance tests.** T2.1 placeholder-safe logic applies to search-returned paths the same way it applies to walker-returned paths.
5. **Day 5 — testwin verification.** Run acceptance criteria §1-§8 on real Windows.

**Acceptance gate:** all 8 criteria from `t3.4-windows-search-index-spec.md` §4 pass on Windows.

### Daily progress signal

Push commits to `feat/scan-options` daily; tag with phase + day. Bench-coord (testwin for Windows acceptance criteria, testrunner for Linux acceptance criteria on exclusion) routed as work lands.

## 4. Tests strategy

Both features get the same shape Mick + design have been getting:

- **Unit tests** for pure logic (matcher, filter translator, preset content). Land alongside the code.
- **Integration tests** under `tests/` for end-to-end behaviour against synthetic dirs / corpora. One per acceptance criterion.
- **Cross-platform**: exclusion tests run on both Linux + Windows; T3.4 tests run on Windows only (cfg-gated).
- **Cross-build check** before any commit lands on the branch.

Target: zero acceptance criterion regression on either feature when the daily commit lands.

## 5. Decisions surfaced

Items where I'd want Mick or design input before locking in:

### File exclusion

1. **Preset content sources.** The spec lists 8 packs with example entries but doesn't enumerate every extension/pattern. For "System libraries" — should the pack include `.lib`, `.a`, `.deb`, `.rpm` ? For "Build artefacts" — should `.cache/`, `.pytest_cache/`, `.tox/`, `coverage/` be in the default pack ? Will draft a complete content table during Day 2 and surface it for review before locking in the const data; user can always override via custom lists.
2. **Counter granularity.** Spec asks for "X files (Y bytes) excluded" total. Worth breaking out per-pack (e.g. "System libraries: 412 files; Build artefacts: 280 files") so the user can tell which packs are doing the work? Adds a few KB to ScanCounters; cheap, but adds Settings tab complexity. Default: total only; per-pack in v2 if asked.
3. **Custom-pattern UX in GUI.** Spec calls for a text-input list. Should there be a "test pattern" feature (type a pattern, paste a test path, click Test → see Match / No Match) ? Nice-to-have, half-day to build. Defer to v2 unless Mick wants it day-one.

### T3.4

4. **No-GUI v1 is right call?** Spec says "CLI-only power-user feature." Confirmed sensible by your "T1.2 pairing makes the GUI sensible later" framing. But beta testers running the EXE without a CLI will never discover the feature. Worth a "Try this from the CLI: `--use-search-index --filter ...`" hint somewhere in the Settings tab footer? Or stay strictly CLI-only?
5. **Filter DSL syntax.** Spec uses positional flags (`--type jpg,png` etc.). Alternative: a single `--filter "type:jpg AND size>100MB"` expression parsed via a small DSL. Spec leans toward positional. Confirm before I write the parser; positional is simpler + fits sd's existing `--include`/`--exclude` shape.
6. **`--rebuild-index-first` scope.** Spec §5 mentions it but doesn't list it as a Day-1 deliverable. Skipping for v1 (users can rebuild via Windows control panel); ship as v2 follow-up if demand.

## 6. Risk

- **Glob matching performance**: `globset` already used elsewhere in the engine; should be cheap. 100 active rules × walker rate = ~µs/file. Spec criterion 12 says "within 5% of baseline" — easy bar.
- **T3.4 COM lifetime management**: ISearchManager + IUnknown / AddRef / Release. The `windows` crate's RAII helpers handle this; pattern is the same as the existing `IFileOperation` work in T2.3 (`winapi_wrappers`). Refer to that for the COM idiom.
- **Search-SQL injection**: user filter strings translated to OLE DB query syntax. Must quote/escape. Use parameterised queries where possible; reject filter strings containing quotes/semicolons/etc.
- **Bench-coord on file-exclusion**: testrunner can verify the walker speed claim (criterion 12) on Linux easily. testwin for Windows; possibly also for the preset content validation (does Windows correctly identify `.dll` exclusion).
- **Bench-coord on T3.4**: testwin only (Windows-specific). Easy.

## 7. Open question for Mick — branch strategy

Two options:

- **(A) Single branch `feat/scan-options`** — both features land on it; ship as one PR/merge.
- **(B) Two branches `feat/file-exclusion` + `feat/search-index`** — file-exclusion lands first (cross-platform; easier to verify); T3.4 lands second. Each is its own merge.

Design's framing was "scan-options mini-release" implying (A). (B) gives more PR granularity for review but adds merge overhead.

Default to (A) unless Mick wants smaller mergeable units.

## 8. Trigger

Mick verifies G1 against `superdeduper-gui-OVERNIGHT-FINAL.exe` → reports clean reclaim + clean Submit → I cut `feat/scan-options` branch off `feat/g-track` and start at Phase A Day 1.

If verification surfaces another G1 bug → fix that first, scan-options waits.
