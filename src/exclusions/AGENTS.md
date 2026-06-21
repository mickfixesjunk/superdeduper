# exclusions — AGENTS guide

## Purpose
Client-side file-exclusion filter for the dedupe scanner. Pairs a user-editable `ExclusionConfig` (TOML-persisted, exposed via Settings -> Exclusions in the GUI and `--exclude-*` flags in the CLI) with a compiled-at-scan-start `ExclusionPolicy` that the walker calls once per file. Implements the spec at `~/sd-bench-local/design/file-exclusion-spec.md` (Option B hybrid: extensions + path-glob patterns + named preset packs).

Sits between `inventory::walk` (caller, hot path) and `config` (persistence). The policy is built once per scan from a config + a `PresetSource` (production: `BuiltinPresets`) and consulted O(1) for extensions, O(N_globs) for paths per directory entry. Excluded files are dropped from the walker output and counted by `ExclusionCounters` (lock-free atomics, shared across worker threads) so the scan summary can report "Excluded by Settings -> Exclusions: N files (Y bytes)".

Per issue #81, the default config now ships with master toggle ON plus 4 footgun-class packs pre-checked (SystemLibraries, VcsInternals, OsSystemTrees, AvSignatureDatabases). A migration helper distinguishes "pristine pre-#81 config" from "user explicitly disabled" so an upgrade silently flips only never-touched configs.

## Files

### `mod.rs`
Module root. Defines `PresetPackId` (8-variant enum), `Decision`, `ExclusionReason`, the runtime `ExclusionPolicy` (with `compile` and the hot-path `evaluate`), the `PresetSource` trait + `EmptyPresets`, and `ExclusionCounters`/`ExclusionCountersSnapshot`. Re-exports `ExclusionConfig`, `ExclusionConfigError`, `BuiltinPresets`.
- Public API:
  - `enum PresetPackId` (+ `ALL`, `SAFE_DEFAULTS`, `label()`)
  - `enum Decision { Included, Excluded(ExclusionReason) }`
  - `enum ExclusionReason { PresetPackPath(PresetPackId), PresetPackExtension(PresetPackId), CustomPattern, CustomExtension }`
  - `struct ExclusionPolicy` with `disabled()`, `compile(&ExclusionConfig, &dyn PresetSource)`, `evaluate(&Path)`, `is_enabled()`
  - `trait PresetSource { fn get(&self, id) -> PresetPack }`
  - `struct PresetPack { extensions, paths }` (both `&'static [&'static str]`)
  - `struct EmptyPresets`
  - `struct ExclusionCounters` (+ `new`, `record`, `snapshot`)
  - `struct ExclusionCountersSnapshot { excluded_files, excluded_bytes }`
- Callers: `inventory::walk` (hot path), `gui::state` / `gui::app` / `gui::live` / `gui::widgets::settings_modal`, `config` (root), `pipeline::hash`, `main`.

### `config.rs`
User-facing `ExclusionConfig` struct serialised as a `[exclusions]` TOML section. Round-trip stable. `Default` implements #81 safe-defaults (master ON + 4 packs). `ExclusionConfigError` covers `BadPattern` (malformed user glob) and `BuildFailed` (rare globset failure).
- Public API: `struct ExclusionConfig { enabled, active_packs, custom_extensions, custom_patterns }`; `fn is_pristine_pre_safe_defaults(&self) -> bool`; `enum ExclusionConfigError { BadPattern, BuildFailed }`.
- Callers: GUI settings modal, root `config`, CLI flag parser in `main`.

### `matcher.rs`
Pure-function helper `lowercased_extension(&Path) -> Option<String>` extracting the lowercased file extension (no leading dot), with explicit semantics for multi-dot filenames, dotfiles, trailing dots, and extensionless files. Used by `ExclusionPolicy::evaluate` so the extension-side comparison is symmetric with rule normalisation.
- Public API: `fn lowercased_extension(&Path) -> Option<String>`.
- Callers: `super::ExclusionPolicy::evaluate`. No external callers (intra-module helper).

### `presets.rs`
Static const arrays of built-in preset content: 15 extensions in Pack 1 (System libraries) + 92 path patterns across packs 2-8. Forward-slash patterns; backslash normalisation happens at `evaluate()` time. `BuiltinPresets` is a zero-sized type implementing `PresetSource`.
- Public API: `struct BuiltinPresets` (impls `PresetSource`).
- Callers: any code that compiles a policy from the real preset data (walker setup in `inventory::walk`, GUI preview).

## Invariants / Gotchas

- Path normalisation in `evaluate()`: backslashes are converted to forward slashes BEFORE matching, AND the Windows verbatim `\\?\` prefix is stripped via `crate::path_display::for_user_display` first. The order matters: stripping after slash-conversion would leave `//?/C:/...` which would not match `C:/Windows/**`. There is a regression test `verbatim_prefixed_path_matches_drive_anchored_pattern` locking this.
- `literal_separator(true)` on all globs: single `*` matches a single path segment only. `**` still spans separators. `build_glob_with_literal_separator` is the only path that should build globs for this module.
- Application order per spec section 2.4: path match runs FIRST, then extension. A `.dll` inside `node_modules` reports as `CustomPattern`, not `CustomExtension`.
- Extensions are stored lowercase with no leading dot (`normalize_extension`). Comparison is symmetric: `lowercased_extension` produces the same shape.
- `active_packs` is semantically a set; `Ord` is derived on `PresetPackId` purely so the settings-drift comparator (issue #157, label `A-settings-drift-tolerant`) can canonicalise order.
- Counters are `Arc<ExclusionCounters>` with `Ordering::Relaxed` atomics; lock-free for `u64` increments on all supported platforms. No need for `SeqCst`.
- Default config is now safe-defaults ON (#81). Upgrade migrations must check `is_pristine_pre_safe_defaults` BEFORE flipping, or you will override users who deliberately turned exclusions off in pre-#81 builds.
- Adding a 9th preset pack: append to enum, append to `PresetPackId::ALL` (do NOT reorder), add a matching arm in `BuiltinPresets::get`, bump the count test in `presets.rs`. Reordering will break `Ord` and consequently `active_packs` canonicalisation.

## Dependencies

- INCOMING:
  - `crate::inventory::walk` — calls `evaluate()` per file; the hot path.
  - `crate::pipeline::hash` — likely propagates exclusion counters.
  - `crate::config` — owns the parent config struct containing `ExclusionConfig`.
  - `crate::gui::{app,state,live,widgets::settings_modal}` — Settings UI + summary rendering.
  - `crate::main` — CLI flag wiring.
- OUTGOING:
  - `globset` (third-party) — pattern compilation + matching.
  - `serde` + `toml` — config (de)serialisation.
  - `thiserror` — error wiring.
  - `crate::path_display::for_user_display` — verbatim prefix stripping (mod.rs, line 276).

## Refactor Hints

- **p1 doc-bug — reason attribution lies about which pack matched.** `ExclusionPolicy::evaluate` (mod.rs lines 286 and 295) hard-codes `PresetPackId::SystemLibraries` whenever a non-custom preset rule fires, regardless of the pack that actually matched. The doc on `ExclusionReason::PresetPackPath` ("Path matched a glob pattern from this preset pack") and the `Decision` doc ("carries the rule class that triggered the exclusion so the scan summary ... can break down by source") both promise pack-accurate attribution. The mod-level doc even calls out "Day 2 may split per-pack to support per-pack counters" — but the current code returns a misleading `SystemLibraries` tag for every preset hit. Either build per-pack `GlobSet`/`HashSet` (and split iteration during compile), or downgrade the doc + add a `PresetPackUnknown` variant or `PresetPackAny` umbrella. Today the scan summary's per-pack breakdown is structurally wrong.
- **doc-drift in `config.rs` line 17:** references `ExclusionConfig::is_pristine_default_off`, but the actual function is `is_pristine_pre_safe_defaults`. Update the doc.
- **stale-comment in `mod.rs` lines 27-29:** the "Day 1 scope / Day 2 / Day 3 / Day 4-5" roadmap is now history — preset content, walker hook, CLI flags and GUI tab have all shipped. Convert to a "Current shape:" paragraph.
- **stale-comment in `mod.rs` lines 152-153 and 186-189:** "Day 1 builds a single combined globset", "Day 1 takes `presets` as an opaque trait object so the presets module can fill in real content on Day 2". The trait object is still used; Day 2 has happened; rephrase.
- **stale-comment in `mod.rs` lines 309-310 and 324-325:** "Day 1 only defines the shape; Day 2's `presets` module provides the actual data" / "Day 2 replaces with the real preset data". Real presets are in `presets.rs` today.
- **stale-comment in `mod.rs` line 418:** "Preset-pack hits exercised on Day 2 with the real preset content" — Day 2 has shipped; `presets.rs` tests now cover this.
- **info — dead-ish helpers:** `ExclusionCounters::record` is referenced by mod-level doc but `grep -n "\.record(" src/exclusions src/inventory src/pipeline` should confirm a real caller; if absent, the walker is bumping atomics directly. (Did not run grep — flagging for verification.)
- **inconsistent-naming — non-blocking:** the spec is called "file-exclusion-spec.md" but the module name is plural `exclusions`. Fine, but worth noting.
- **info:** `PresetPack` has no constructor or doc on the lifetime contract. Could add a tiny `pub const fn new(...)` for consistency, but tradeoff is small.

## Wire Surfaces

- TOML: `[exclusions]` section with fields `enabled`, `active_packs`, `custom_extensions`, `custom_patterns`. `PresetPackId` is `rename_all = "kebab-case"` so values are `"system-libraries"`, `"build-artefacts"`, etc. (`mod.rs` line 59).
- CLI flags (owned upstream in `main`): `--exclusions on/off`, `--no-exclusions`, plus `--exclude-*` mutators per the spec.
- GUI: Settings -> Exclusions tab (in `gui::widgets::settings_modal`) reads/writes `ExclusionConfig`.
- Persistence: round-trip stable through serde + toml; `default_enabled` + `default_active_packs` provide #81 safe-defaults for missing fields.
- No HTTP / network surfaces.
