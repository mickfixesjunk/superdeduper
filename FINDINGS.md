# FINDINGS.md - superdeduper Codebase Audit Rollup

## Header

This document is the aggregated rollup of a multi-agent codebase audit of the superdeduper engine repository. Twenty-five subagents were dispatched in parallel, one per top-level directory (and a few sub-directories where size warranted it). Each agent read every Rust / markdown / TOML file in its assigned directory, wrote a fresh `AGENTS.md` describing the public API surface, invariants, dependencies, and wire surfaces, and returned a structured list of documentation / code drift findings against a shared severity schema. This file collects those findings, dedupes cross-directory hits, and groups them by severity.

Generated during ultracode audit; date from commit log or session context.

Totals across all 25 agents:
- **P1 (load-bearing):** 4
- **P2 (meaningful drift):** 41
- **P3 (cosmetic):** 23
- **Info (refactor opportunities):** 65
- **Grand total:** 133 findings

(Count corrected post-codex review on PR #187: the synthesis aggregator
initially reported `P1=3` but the P1 section below enumerates four issues —
`src/dedupe.rs` validation doc, `src/config.rs` hash_algo doc,
`src/exclusions/mod.rs` PresetPackId attribution, and
`crates/superdeduper-bench-real/src/bench_run.rs` fused docstring. All four
ship as doc-only fixes in PR #186; underlying code refactors for P1-1
(dedupe mtime) and P1-3 (exclusions per-pack) queued for v0.3.44.)

---

## P1 - Load-bearing issues

### src/

```
src/dedupe.rs:9
```
Module header safety contract claims "Before any destructive action, the file's (size, mtime) is re-checked against the results-file's snapshot." Reality: `validate_file` (line 820-829) only checks `meta.len() == expected_size`; mtime is NOT re-checked. A refactorer relying on the documented "multi-layered" guarantee could miss a content-change-without-size-change attack window, or remove the size check thinking mtime covers it.
**Fix:** Either add the mtime re-check to `validate_file`, or amend the module header to say the snapshot check is size-only.

```
src/config.rs:84
```
Doc on `ScanConfig::hash_algo` says "BLAKE3 is the default; DDH-128 is the in-development alternative (currently an xxhash3-128 stub)." Reality: CLI default is River5 (cli.rs:647 `default_value_t = HashAlgoArg::River5`), DDH-128 was renamed to River5, and River5 is now AES-NI-accelerated (not an xxhash3-128 stub).
**Fix:** Update doc to "River5 is the default; BLAKE3 is the cryptographic alternative" and remove the DDH-128 / xxhash3 framing.

### src/exclusions/

```
src/exclusions/mod.rs:286 (and :295 for the extension variant)
```
`ExclusionPolicy::evaluate` returns `ExclusionReason::PresetPackPath(PresetPackId::SystemLibraries)` hard-coded for ANY preset-pack path hit, regardless of which pack actually matched (same applies to PresetPackExtension at line 295). The doc on `Decision` (line 126) and `ExclusionReason::PresetPackPath` (line 135) both promise the reason carries "the rule class / pack that triggered the exclusion." The scan summary's per-pack breakdown is structurally wrong: any pack that excludes a file gets attributed to SystemLibraries.
**Fix:** Compile per-pack globsets (the Day-2 comment at line 152 already anticipates this) so `evaluate` can return the real `PresetPackId`.

### crates/superdeduper-bench-real/

```
crates/superdeduper-bench-real/src/bench_run.rs:482
```
A single 45-line doc-comment block (lines 482-527) is fused to `signal_dedup_ready` (line 528) but its content covers three different functions concatenated: lines 482-493 describe `read_uncached`, lines 494-495 describe `cold_bypass_reliable`, lines 496-527 describe `signal_dedup_ready`. Net effect: `read_uncached` (defined at lines 851 / 912 / 927 / 1014 for Linux / macOS / Windows / fallback) and `cold_bypass_reliable` (line 836) carry NO doc comments because their docstrings landed on the wrong function. A refactorer reading "read_uncached" finds no inline guidance for a load-bearing cache-bypass primitive.
**Fix:** Split the doc-block back to its three intended targets.

---

## P2 - Meaningful drift

### src/

```
src/config.rs:88
```
Doc on `ScanConfig::exclusion_policy` says "Defaults to disabled (master toggle OFF); compile from `crate::exclusions::ExclusionConfig` at scan start once the GUI / CLI exposes a way to populate the config (Days 3-5 of the scan-options branch)." Issue #81 shipped: `build_cli_exclusion_policy` returns safe-defaults ON by default; `--exclusions off` is the opt-out.
**Fix:** Rewrite as "Defaults to safe-defaults ON; `--exclusions off` to disable."

```
src/action_receipt.rs:256
```
Docstring on `pub fn read_inode_and_nlink` says "Cross-platform; on Windows we use the file_ref via `winapi_wrappers` if available, else fall back to `MetadataExt::file_index`." Neither is true: the Windows impl returns `(0, 1)` unconditionally. Receipts asserting inode equality on Windows fail by construction.
**Fix:** Either wire the Windows file_index path or amend the docstring to "Windows: stub returning (0, 1) until file_index lands."

```
src/cache.rs:90
```
Doc on `WarmCacheEntry` says "Use `Cache::warm_load_all` to build the HashMap once at Stage 4 start; pass `Arc<HashMap<_>>` through the hash pipeline." Architecture changed: `warm_in_place` stores the map internally on the Cache struct, and `lookup_detailed` consults `self.warm_map` directly.
**Fix:** Rewrite the doc to describe the internal-warm-map architecture.

```
src/lib.rs:137
```
`leaderboard_corpus_sig` constructs a blake3 hasher but prefixes the output `format!("sha256:{}", ...)`. May be a stable server-contract identifier, but no doc explains why a blake3 digest carries a `sha256:` tag.
**Fix:** Add a comment naming the contract (or rename to `blake3:` if the server tolerates it).

### src/gui/

```
src/gui/results_store.rs:179
```
Doc-block on `load_matching` claims it "Otherwise returns Ok(None) and (optionally) deletes the stale state so the next session starts clean." The implementation does not delete anything - just returns `Ok(None)`. Stale results-state.json files accumulate indefinitely.
**Fix:** Either wire the cleanup call in or drop the "(optionally) deletes" clause.

### src/gui/widgets/

```
src/gui/widgets/resume_modal.rs:149
```
Hover text on Start-fresh button claims the "BLAKE3 / DDH-128 cache is preserved." DDH-128 was renamed to River5.
**Fix:** Update user-facing string to "BLAKE3 / River5 cache."

### src/gui/preview/

```
src/gui/preview/fallback_text.rs:48
```
Truncation banner formats `MAX_BYTES as u64 + 1` as the file's total size ("of {}"). Result: prints e.g. "Showing first 64 KiB of 64 KiB" regardless of real file size.
**Fix:** Read real length via `fs::metadata` like the hex viewer does.

```
src/gui/preview/classify.rs:173
```
Comment says "Be lenient on UTF-8 multi-byte chars", implying multi-byte bytes count as printable. The filter only counts tab/CR/LF and 0x20..=0x7E - bytes >= 0x80 are NOT counted, so non-ASCII UTF-8 text can fail the 90% threshold and route to hex.
**Fix:** Either widen the predicate or fix the comment.

```
src/gui/preview/classify.rs:85
```
`TEXT_EXTENSIONS` includes `dockerfile`, `gitignore`, `gitattributes`, `editorconfig`, `license`, `readme`. These are full filenames without an extension, so `Path::extension()` returns None and the allowlist never matches.
**Fix:** Add a filename-allowlist branch or remove the dead entries.

### src/leaderboard/

```
src/leaderboard/hardware.rs:458
```
Doc comment for `bus_type_to_disk_class` references `[refine_usb_with_sat_rotation_rate]` but that function does not exist. The actual SAT-pass-through refinement function is `query_sat_rotation_rate`.
**Fix:** Rename the link.

```
src/leaderboard/predicates.rs:13
```
Top-of-file predicate-status table lists `picky-eater` and `verify-veteran` as "stub (needs persistent counter)" but both are fully implemented and tested. Table also lacks `recursive` and `shadercache-hoarder` (#161).
**Fix:** Refresh the table.

```
src/leaderboard/hardware.rs:238
```
Doc block authored for `workdir_disk_class_platform` visually attaches to the immediately-following `is_wsl` function because there's no blank line separator.
**Fix:** Add a blank line / reorder.

### src/pipeline/

```
src/pipeline/hash.rs:19
```
Module-level doc says "A future commit will replace these reads with the IOCP pipeline." The IOCP module exists but is dead code; the real Tier-3 path took a different route (BufReader + sync_channel ping-pong). Same stale narrative repeated in `src/pipeline/iocp.rs:15-17, :165`.
**Fix:** Either delete the iocp module or update both docs to mark iocp as a parking lot.

```
src/pipeline/hash.rs:220
```
`HashCounters` doc claims "summed CPU time across all rayon workers." Code measures wall-clock via `Instant::now()`. Summed across workers (so it can exceed wall-clock) but not CPU time.
**Fix:** Rewrite as "wall-clock per-worker, summed across workers."

```
src/pipeline/io_threads_probe.rs:28
```
Doc says probe is bypassed when "`SUPERDEDUPER_IOTHREADS_PARKED=1` is set." Actual check is `env::var(...).is_ok()` - any value (e.g. `0`, `false`) skips the probe.
**Fix:** Document the looser semantics, or tighten the check to require `==1`.

### src/pipeline/hash/

```
src/pipeline/hash/format.rs:75
```
`fingerprint()` hashes `fmt as u8` BEFORE the parser bytes. Format discriminant ordering is part of the persisted Tier 0 fingerprint wire contract - reordering variants silently invalidates every cache row whose discriminant shifted.
**Fix:** Add an invariant block to both `Format` and `fingerprint`.

```
src/pipeline/hash/algo.rs:120
```
Doc on `hash_oneshot` claims it is "identical to `ContentHasher::new(algo).update(data).finalize()`" but `update` returns `()` so that chain doesn't compile.
**Fix:** Use the multi-statement form in the doc.

### src/pipeline/hash/format/

```
src/pipeline/hash/format.rs:80
```
Comment reads "Format-parser helpers ... stay available for the additional formats (MP4, MKV, PDF) on the roadmap." MP4, MKV, PDF are all fully implemented.
**Fix:** Drop the roadmap framing.

### src/pipeline/image_hash/

```
src/pipeline/image_hash/mod.rs:89
```
Docstring claims `DoubleGradient` is 128 bits and we "use the upper 8" bytes, but `bytes_to_u64` reads the FIRST 8 bytes. Load-bearing for cache stability.
**Fix:** Either rewrite to "first 8 bytes" or change the implementation (which would invalidate every persisted phash cache row).

```
src/pipeline/image_hash/mod.rs:155
```
`hash_image` doc says "Public-ish (pub-in-crate) so future Tier-4 pipeline integration can pass the decoded buffer directly" but the function is `pub`, Tier-4 has landed, and tier4 calls `hash_file`.
**Fix:** Downgrade to `pub(crate)` and update.

### src/pipeline/audio_hash/

```
src/pipeline/audio_hash/mod.rs:60
```
Doc references `examples/audio_profile.rs` as a working driver; file does not exist.
**Fix:** Either ship the example or drop the reference.

```
src/pipeline/audio_hash/mod.rs:12
```
Module doc says "V1 explicitly does NOT include: Tier-4 pipeline integration" but Tier-4 is shipped.
**Fix:** Remove the "not included" line.

```
src/pipeline/audio_hash/tier4.rs:17
```
Module doc says groups are reported with `similarity_kind = PerceptualImage` with TODO; code at line 317 already emits `SimilarityKind::PerceptualAudio` (GH #54).
**Fix:** Remove the TODO.

```
src/pipeline/audio_hash/tier4.rs:97
```
`find_similar_groups` doc says hash failures are "silently skipped." Per #119, every failure records `AudioDecodeWarning`.
**Fix:** Update the doc.

### src/inventory/

```
src/inventory/walk.rs:1242
```
Doc on `enumerate_one_folder_fast_path` says "for the Windows MFT fast path." The MFT fast path is `FSCTL_ENUM_USN_DATA` in `mft.rs`. This is the per-directory `FileIdBothDirectoryInfo` path, NOT the MFT path.
**Fix:** Rename "MFT fast path" to "per-directory fast path."

```
src/inventory/walk.rs:1077
```
`walk_one_root_buffered` pushes a `DirError OwnedWalkEvent` then returns `Err(PathNotFound)`; the caller `?`-propagates the error before replaying the buffered events. DirError event is silently dropped.
**Fix:** Either remove the event push or return events even on error.

### src/platform/

```
src/platform/linux/mount_info.rs:148
```
`parse_mounts_file` doc says "Public so unit tests can run against a fixture file" but no test exercises it - all tests call private `parse_mounts_body`.
**Fix:** Drop to `pub(crate)`/private or add a fixture-based test.

### src/winapi_wrappers/

```
src/winapi_wrappers/windows_impl.rs:282
```
`StorageDeviceInfo.sector_size` and `physical_sector_size` are hardcoded to 4096 with comment "populated by a later commit" but the struct field doc-comments do not surface this placeholder status.
**Fix:** Add a struct-field doc warning, or wire up `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR`.

### crates/superdeduper-bench-iface/

```
crates/superdeduper-bench-iface/src/lib.rs:42
```
Module doc references `docs/phase-0-p0d-move-plan.md`; file does not exist. Only `docs/phase-0-trait-extraction.md` is present.
**Fix:** Update the pointer or restore the missing doc.

```
crates/superdeduper-bench-iface/src/lib.rs:113
```
Doc paragraph still says "Opaque placeholder for the scaffold; P0-D replaces this" but the next paragraph says this IS the real shape.
**Fix:** Delete the stale "opaque placeholder" sentence.

### crates/superdeduper-bench-real/

```
crates/superdeduper-bench-real/src/d7_probe.rs:11
```
Module doc says "Probe EXECUTION lives in bench_run.rs as D7-B. Wire format lives in bench_client.rs as D7-C." Neither is true: `execute_probes` lives in `d7_probe.rs` itself; no D7 / calibration_seed code exists in `bench_client.rs`. Per `src/leaderboard/mod.rs:89`, "No engine call site currently reaches d7_probe."
**Fix:** Either wire D7-B/D7-C or correct the docstring.

```
crates/superdeduper-bench-real/src/lib.rs:60
```
`BenchReal` struct doc says "Constructible without state today; future Phase 2 wiring may add fields." Phase 2 + Phase 3 have shipped; the zero-sized `_phase_2_state: ()` field at line 66 is the leftover.
**Fix:** Remove the placeholder field and rewrite the doc.

```
crates/superdeduper-bench-real/src/bench_client.rs:904
```
Disabled `#[cfg(any())]` test at lines 910-966 references `super::super::submission::SubmissionInputs` which would create a circular dep. Phase 2-B has shipped.
**Fix:** Delete the disabled test or relocate.

### scripts/

```
scripts/check-feature-flag-consistency.sh:34
```
Header comment "macOS-only build in release.yml isn't checked because the local cross-build script has no macOS path." Stale: `cross-build-drop.sh` now invokes `build-mac-tailnet.sh`, and `release.yml` has a `build-macos:` section. The tuple list silently omits mac-cli / mac-gui.
**Fix:** Add mac-cli + mac-gui tuples.

```
scripts/bench/Invoke-NoBufferPreRead.ps1:31
```
Header claims "Run-SdHddBench.ps1 wires this in when `-PreReadCache` is passed." Run-SdHddBench.ps1 does NOT declare a `-PreReadCache` param.
**Fix:** Wire it through, or correct the header.

### docs/

```
docs/leaderboard-spec.md:212
```
Spec instructs adding `src/telemetry.rs`; no such file exists; bench / leaderboard code lives under `src/leaderboard/`.
**Fix:** Add a stronger "SUPERSEDED" banner mid-body.

```
docs/phase-0-trait-extraction.md:152
```
Lists `src/leaderboard/{bench,bench_corpus,d7_probe}.rs` as modules to move. None exist under `src/leaderboard/` (likely already moved into `crates/superdeduper-bench-real/`).
**Fix:** Update path references.

```
docs/preflight-spec.md:178
```
Endpoint listed as `https://api.superdeduper.com/v1/preflight-submit`; engine default is `https://api.superdeduper.io`.
**Fix:** Align on `.io`.

```
docs/scan-options-mini-release-plan.md:32, :48
```
References `src/inventory/search_index.rs` as Phase B landing zone (does not exist). Plan proposes CLI flags that never shipped under those names.
**Fix:** Add a status banner.

```
docs/walker-fast-path-spec.md:4
```
Status banner says "engine-side implementation deferred." `perf-98-findings.md` v0.3.3 finding ("parallel-walk is NOT on the roadmap") supersedes this.
**Fix:** Close the spec with a pointer to perf-98-findings.

### root / repo top-level

```
README.md:37, :76, :97, :166
```
Status banner says "v0.1.x is feature-active" (Cargo.toml is v0.3.42). Pin example uses v0.2.1. Non-goals list says "macOS support (not on the roadmap for v0.1.x; revisit at v0.2+)" while the rest of the file (and release.yml) advertise macOS support.
**Fix:** Refresh version markers; remove the "no macOS" non-goal.

```
TESTING.md:78, :163, :218, :232
```
Section 3.3 refers to `bench-vs-fclones.yml` (does not exist). Section 6 describes `perf.yml` (does not exist). Section 2.1 references `tests/support/vhd.rs` (does not exist). CI matrix lists NTFS-VHD admin + ReFS-VHD admin + cargo-llvm-cov 85% coverage + clippy::pedantic -D warnings; actual ci.yml has none of these.
**Fix:** Rewrite sections 2-3 and 6 to describe actual shipped workflows.

```
HANDOVER.md:25
```
Session block dated 2026-05-22 lists T2.1 phases 4-7 as pending. Engine has advanced to v0.3.42 (25+ minor versions later).
**Fix:** Truncate or replace with current state.

```
SECURITY.md:90
```
"Things we will never do" says "Bundle unrelated software, telemetry, or update-checks." Engine ships opt-in `telemetry` feature.
**Fix:** Reword to distinguish opt-in vs default-on.

```
Cargo.toml:85
```
Comment claims `[telemetry] Default-on in release builds` but `default = []` (line 184) and README release commands explicitly pass `--features telemetry`.
**Fix:** Rewrite to "telemetry is off by default; release builds opt in via `--features telemetry`."

```
build.rs:87
```
Docstring claims a 16-byte header `u64le rgba_len, u32le width, u32le height`. Actual code (lines 123-126) writes only 8 bytes: `w.to_le_bytes()` + `h.to_le_bytes()` (no rgba_len prefix).
**Fix:** Reconcile with the GUI runtime parser - either restore the rgba_len prefix or fix the docstrings.

---

## P3 - Cosmetic

- broken-claim **src/lib.rs:111** - `leaderboard_corpus_sig` "Mirrors leaderboard::submission" - grep finds 0 hits
- stale-comment **src/gui/particles.rs:124** - cites `app.rs:3489`; actual sparkles call site at `app.rs:4501`
- stale-comment **src/gui/events.rs:252** - `ResumeHydrated` doc cites `app.rs:441`; actual fires around line 631-641
- stale-comment **src/gui/diagnostics.rs:246** - doc-block describing `quick_uuid` precedes `format_duration_hms`
- doc-drift **src/gui/widgets/scan_history_panel.rs:477** - `format_unix_local` emits UTC; either rename to `format_unix_utc` or wire TZ
- stale-comment **src/gui/widgets/funnel.rs:21** - hover says "Five-stage funnel"; `Stage::ALL` has 8 entries
- stale-comment **src/gui/widgets/badge_wall.rs:1003** - test comments describe star/diamond/circle; render uses shield PNGs
- stale-comment **src/gui/preview/mod.rs:213** - `display_path` references "groups_table" as sibling (now under widgets/)
- doc-drift **src/leaderboard/account_badge_summary.rs:47** - references `fetch_or_mock`; actual function is `fetch`
- stale-comment **src/leaderboard/install.rs:227** - `data_dir_public` doc overstates consumer set
- stale-comment **src/leaderboard/install.rs:534** - Windows `fill_random` says "BCryptGenRandom"; actually falls to non-CSPRNG seeded_fill
- stale-comment **src/leaderboard/captcha.rs:6** - "will be reused for G3 OAuth"; G3 OAuth has shipped
- stale-comment **src/leaderboard/oauth.rs:28** - lists v1.1 follow-ups already shipped
- doc-drift **src/pipeline/mod.rs:87** - `assert_unique_paths` says "Wrapped in debug_assert!"; uses `if cfg!(debug_assertions) { assert!(...) }`
- doc-drift **src/pipeline/hash.rs:343** - `run_with_progress` doc says callback receives `(tier, bytes_processed)`; actual is `Fn(&Path, u8, ProgressOutcome)`
- stale-comment **src/pipeline/iocp.rs:165** - "Implementation wired up"; pipeline rewired onto BufReader + sync_channel
- doc-drift **src/pipeline/hash/format/mp3.rs:5** - doc says scan back from ID3v1 footer; code walks forward
- stale-comment **src/winapi_wrappers/windows_impl.rs:848** - "T2.3" internal ticket label historic
- stale-comment **src/exclusions/mod.rs:418** - "exercised on Day 2"; now done
- stale-comment **src/debug/snapshot.rs:308** - cites `walk.rs:695`; now at `walk.rs:1749`
- stale-comment **src/debug/snapshot.rs:327** - references commit SHA 7826172
- doc-drift **src/bin/superdeduper_gui.rs:50** - persistence-diagnostic comment dated 2026-05-25
- inconsistent-naming **scripts/iothread-sweep.ps1:15** - default list omits 4; .sh sibling includes it
- stale-comment **scripts/fast-gate.ps1:14** - "Until sdd-testwin authors the Windows wrapper"
- invariant-undocumented **scripts/bench/Run-MickCorpusMatrix.ps1:143** - depends on `sdd-standby-purge` task existing
- invariant-undocumented **scripts/bench/Run-MickCorpusMatrix.ps1:268** - scan-history JSON field names hard-coded
- invariant-undocumented **scripts/release-integrity-check.sh:107** - distinctive-identifier grep hard-coded to `*.rs`
- doc-drift **docs/iocp-tier3-spec.md:54** - "currently a STUB" reads as pending-fix
- stale-comment **docs/exclusions-preset-content-draft.md:5, :255** - now landed
- stale-comment **README.md:97** - PowerShell example downloads v0.2.1
- doc-drift **crates/superdeduper-bench-iface/src/lib.rs:114, :252, :526** - placeholder language; nonexistent `superdeduper-bench-stub`
- doc-drift **crates/superdeduper-hmac-signer/src/lib.rs:136** - test name claims RFC 4231 vector; only asserts `sig.len() == 64`
- dead-code **crates/superdeduper-bench-real/src/lib.rs:93** - unused `use std::time::Duration;`
- doc-drift **src/pipeline/hash/algo.rs:141** - test `river128_output_is_16_bytes`; algo is `HashAlgo::River5`
- stale-comment **src/inventory/walk.rs:271, :372** - cites `pipeline/mod.rs:86` (actual :90); "lines ~419-438" (now ~739-772)

---

## Info - Refactor opportunities

### Dead code / unused symbols

- **src/cache.rs:534** - `Cache::warm_load_all` is `pub` but only called by `warm_in_place`
- **src/gui/sound.rs:66, :88** - `play_fastforward_start`, `play_caught_up` no callers
- **src/gui/diagnostics.rs:118** - `DiagnosticsLog::elapsed()` unused
- **src/gui/results_store.rs:114** - `pub fn delete()` no callers
- **src/gui/widgets/groups_table.rs:198** - `pub fn show(...)` zero non-test callers
- **src/gui/preview/mod.rs:253** - `show_or_close` no callers
- **src/pipeline/iocp.rs** - entire module dead (no callers outside itself)
- **src/pipeline/layout.rs:22** - `layout::resolve` near-no-op pass-through
- **src/pipeline/hash/format.rs:84, :104, :111** - `read_n`, `read_u32_le`, `read_u16_le` no callers
- **src/pipeline/hash/format/jpeg.rs:180** - `_keep_alive` stub duplicates parent's allow(dead_code)
- **src/pipeline/audio_hash/mod.rs:123** - `pub const DEFAULT_THRESHOLD: u32 = 5` no callers
- **src/inventory/walk.rs:509, :813** - recursive `walk()` (~500 LOC) and `walk_fast_path` isolated cluster
- **src/winapi_wrappers/windows_impl.rs:192, :731** - `OwnedHandle::as_handle`, `pathbuf_from_wide` unused
- **src/leaderboard/install.rs:213** - `migrate_legacy_install_json` pub but internal-only
- **crates/superdeduper-bench-real/src/bench_corpus.rs:417, :596, :615, :633, :648, :385, :343** - leftover from pre-server-direct-verify Merkle model
- **crates/superdeduper-bench-real/src/bench_corpus.rs:322** - `dedup_efficiency` test-only
- **crates/superdeduper-bench-real/src/bench_client.rs:146, :176, :314, :481** - `result_digest_bytes*`, `file_raw_hash` only used internally
- **tests/akp_gui_linux.rs:169** - `_path_marker` no-op
- **tests/gui_tier_a_linux.rs:975** - `generate_mick_shape_corpus` allow(dead_code)
- **tests/scan_resume_e2e.rs:132** - `CancelTrigger` allow(dead_code), one variant used
- **src/bin/dir_probe.rs** - one-shot diagnostic

### Feature-gate / cfg cleanup

- **src/gui/widgets/oauth_chooser.rs:17** - inner `#![cfg(feature = "gui")]` redundant with `mod.rs:22` telemetry gate
- **src/pipeline/hash/format.rs:96** - `read_u32_be` `#[allow(dead_code)]` redundant (used by `jpeg.rs:16`)

### Duplication

- **src/inventory/{mft.rs:426, warm.rs:347}** - `reconstruct_path` duplicated with byte-identical invariant
- **src/inventory/{mft.rs:506+519, warm.rs:487+495}** - `under_any_root` + `path_passes_globs` duplicated
- **src/inventory/walk.rs:828+832+1264+1265, placeholder.rs:158-169** - reparse-tag constants duplicated inline
- **src/pipeline/hash/format/{jpeg,mkv,mp3,pdf,png,zip}.rs** - each redefines identical `fn io_err`
- **src/winapi_wrappers/windows_impl.rs:137 +3** - four `CreateFileW` open helpers share prep + flags
- **src/winapi_wrappers/windows_impl.rs:311** - `bus_type_name` mixes hex literals and named consts
- **src/debug/snapshot.rs:261** - `rfc3339_now` + `days_to_ymd` copied from `action_receipt`
- **src/leaderboard/mod.rs:24** - dual re-export styles
- **crates/superdeduper-log/src/lib.rs:57** - `log_data_dir()` duplicates `leaderboard::install::data_dir()`
- **tests/smoke.rs:46** - `ScanConfig` struct literal duplicated across 5+ test files
- **tests/gui_tier_a_linux.rs:34** - `env_lock` pattern reimplemented 4x

### Missing or stale doc / invariants

- **src/dedupe.rs:30** - older pub fields lack per-field doc comments
- **src/cache.rs:312** - schema-mismatch branch preserves `meta` table; invariant not surfaced
- **src/scan_history.rs:91** - module header narrates v1 roadmap; v2/v3/v4 have shipped
- **src/lib.rs:22** - self-acknowledged chronicle re unsafe policy
- **src/gui/state.rs:194, :217** - `UiState` lacks top-level doc; duplicates/duplicate_hashes lockstep invariant not surfaced
- **src/gui/preview/fallback_hex.rs:152** - `FULL_WIDTH = 49` derivation from `ROW_BYTES = 16` not asserted
- **src/gui/results_store.rs:53** - `saved_at_unix` only set in `::new()`
- **src/gui/preflight.rs:19** - `PreflightState` lacks enum-level docs
- **src/gui/widgets/bench_modal.rs:87** - `USER_TIERS` single-entry but walked as multi-tier
- **src/gui/widgets/groups_table.rs:110** - #156 bmp-glyph-fallback comments scattered
- **src/gui/widgets/settings_modal.rs:1** - 3179 LOC single file
- **src/gui/checkpoint.rs:100, :134** - hardcodes schema string; permissive starts_with
- **src/gui/app.rs:4632** - `check_resumable` byte-eq diverges from `detect_settings_drift` equivalence
- **src/leaderboard/cpu_brackets.rs:153** - `log_pattern_error` HashSet grows unbounded
- **src/leaderboard/hardware.rs:670** - `macos_diskutil_info` no timeout
- **src/leaderboard/install.rs:158** - `InstallState` lacks struct-level docstring
- **src/pipeline/hash.rs:226, :576, :1414, :324** - missing docs on HashCounters; ad-hoc snapshot helper; flat 64-KiB Tier-0 estimate; six entry points with growing tuple of Optionals
- **src/pipeline/hash/algo.rs:23, :84** - `HashAlgo::Blake3` no doc; river5 SHA `fd854fe` reference
- **src/pipeline/hash/format/mp4.rs:170** - `stsz` sum uses `wrapping_add`; part of on-disk Tier 0 contract
- **src/pipeline/hash/format/mkv.rs:98** - MKV/MP3/PNG/PDF payload byte caps are silent schema constants
- **src/pipeline/image_hash/tier4.rs:153, :233** - recursive `find`; group identity uses `OsStr::cmp` (not cross-platform reproducible)
- **src/pipeline/image_hash/mod.rs:178** - `HashError` TODO closed; failures silently dropped via tracing::debug
- **src/pipeline/audio_hash/mod.rs:75, :89, :95** - `profile::enabled` cached for process lifetime; `add` pub but internal; `snapshot` per-thread
- **src/pipeline/audio_hash/tier4.rs:221** - `fn find` recursive
- **src/inventory/walk.rs:200** - `enumerate_cancellable` doc accurate for dead recursive walk
- **src/inventory/dir_enum.rs:40, :176** - `DirEntryFull`/`DirFullEnumeration` pub fields lack docs; `_UNUSED_HOOK` fragile
- **src/inventory/mod.rs:38** - `FileEntry` pub fields half-documented
- **src/platform/mod.rs:21, :195, :217** - module-tree diagram stale; `TrashOutcome`/`linux::trash::TrashEntry` near-dupes; Windows IFileOperation result TODO #33 v2
- **src/platform/windows.rs:18** - `clone_file(src, dst)` -> `replace_with_reflink(dst, src)` arg swap undocumented
- **src/platform/linux/trash.rs:192** - collision-suffix deviates from XDG spec ' 2' convention
- **src/winapi_wrappers/windows_impl.rs:1035** - `let _ = &mut request;` no-op; `fetch_reparse_tag` doesn't use OwnedHandle
- **src/exclusions/mod.rs:56** - derived Ord on `PresetPackId` declaration-order-dependent (#157 settings-drift)
- **src/debug/snapshot.rs:68, :99, :107, :145, :237, :413** - bare pub enums lack docs; unbounded recursion; `mtime_to_ns` silently 0; `detect_filesystem` ignores arg
- **src/bin/hash_repro.rs:27, :487** - tier constants hand-mirrored; rayon pool comment contradictory
- **crates/superdeduper-bench-iface/src/lib.rs:272, :469, :552** - `InstallKey` field lacks doc; LOCKED keys buried; lifetime invariant weakly hinted; Cargo description 196 chars
- **crates/superdeduper-bench-real/src/bench_corpus.rs:306, :391** - `path_index == i` debug_assert only; awkward tuple return
- **crates/superdeduper-bench-real/src/bench_run.rs:1248** - `io_threads = cpu_threads * 3` magic number; measurement-fidelity contract not surfaced
- **crates/superdeduper-bench-real/src/bench.rs:445** - `hex32` test helper duplicated
- **crates/superdeduper-hmac-signer/src/lib.rs:54, :63, :88** - "bytes POST == bytes signed" implicit; `canonicalize` clones entire tree; per-byte `format!` allocation
- **crates/superdeduper-log/src/lib.rs:39, :154, :196** - Mutex<bool> could be AtomicBool; format may switch to RFC3339; smoke tests don't assert disk
- **tests/cache_corpus_reset.rs:84** - inconsistent `ExclusionCounters::new()` vs `::default()`
- **tests/v31_goldens.rs:153** - `hex32` lacks doc
- **tests/gui_tier_a_linux.rs:1107** - `tier_a_gui_scan_perf_within_cli_ratio` doesn't exercise Mick-shape corpus
- **tests/properties.rs:198** - `thread_invariance` doesn't assert recall vs oracle
- **tests/akp_gui_ntfs.rs:27** - commit-SHA narrative
- **scripts/swarm-health-check.sh:86** - swarm topology in-source arrays
- **scripts/test-corpus.py:298** - jpg-count pre-check compares `== '4'` as string
- **docs/swarm-health-check.md:10** - topology constants live in two places
- **docs/perf/hdd-profile-bench-methodology.md:33** - "Verified against commit" footer pattern not adopted elsewhere
- **docs/testing/cli-flag-matrix.md:219** - F-CLI-5 candidate impl note unverified
- **specs/v0.3.40-mick-corpus-ship-gate.md:122, :384, :453** - units note buried; closed items mixed with open; Run-MickCorpusMatrix.ps1 path may go stale
- **specs/historical/egui-kittest-scan-perf-mick-shape.md:1** - lacks self-supersede pointer
- **.gitignore:24** - CLAUDE.md / HANDOVER.md gitignored at engine repo level

---

## Per-directory summary

Sorted by P1 count desc, then total findings desc.

| Path | Files | Lines | P1 | P2 | P3 | Info | Total | AGENTS.md |
|------|------:|------:|---:|---:|---:|-----:|------:|-----------|
| src/exclusions | 4 | 1430 | 2 | 6 | 1 | 1 | 10 | src/exclusions/AGENTS.md |
| src/ | 19 | 14732 | 2 | 4 | 1 | 6 | 13 | src/AGENTS.md |
| crates/superdeduper-bench-real | 8 | 6167 | 1 | 3 | 1 | 9 | 14 | crates/superdeduper-bench-real/AGENTS.md |
| docs | 11 | 2400 | 0 | 5 | 5 | 2 | 12 | docs/AGENTS.md |
| src/leaderboard | 22 | 12523 | 0 | 3 | 4 | 6 | 13 | src/leaderboard/AGENTS.md |
| (root) | 14 | 1800 | 0 | 7 | 4 | 1 | 12 | AGENTS.md |
| src/gui | 20 | 14713 | 0 | 1 | 3 | 10 | 14 | src/gui/AGENTS.md |
| src/pipeline | 6 | 5050 | 0 | 3 | 3 | 6 | 12 | src/pipeline/AGENTS.md |
| src/pipeline/hash/format | 8 | 1525 | 0 | 1 | 1 | 6 | 8 | src/pipeline/hash/format/AGENTS.md |
| src/inventory | 6 | 5052 | 0 | 2 | 2 | 8 | 12 | src/inventory/AGENTS.md |
| src/pipeline/audio_hash | 2 | 964 | 0 | 3 | 0 | 4 | 7 | src/pipeline/audio_hash/AGENTS.md |
| src/pipeline/image_hash | 2 | 993 | 0 | 2 | 1 | 3 | 6 | src/pipeline/image_hash/AGENTS.md |
| crates/superdeduper-bench-iface | 2 | 769 | 0 | 2 | 3 | 4 | 9 | crates/superdeduper-bench-iface/AGENTS.md |
| src/pipeline/hash | 2 | 299 | 0 | 2 | 1 | 5 | 8 | src/pipeline/hash/AGENTS.md |
| src/winapi_wrappers | 3 | 1278 | 0 | 1 | 1 | 6 | 8 | src/winapi_wrappers/AGENTS.md |
| src/gui/widgets | 28 | 12301 | 0 | 1 | 3 | 5 | 9 | src/gui/widgets/AGENTS.md |
| src/gui/preview | 5 | 643 | 0 | 3 | 1 | 3 | 7 | src/gui/preview/AGENTS.md |
| scripts | 16 | 1850 | 0 | 2 | 4 | 3 | 9 | scripts/AGENTS.md |
| src/platform | 7 | 894 | 0 | 1 | 1 | 4 | 6 | src/platform/AGENTS.md |
| src/bin | 3 | 793 | 0 | 1 | 1 | 3 | 5 | src/bin/AGENTS.md |
| src/debug | 2 | 699 | 0 | 1 | 1 | 5 | 7 | src/debug/AGENTS.md |
| tests | 15 | 4500 | 0 | 0 | 0 | 10 | 10 | tests/AGENTS.md |
| crates/superdeduper-hmac-signer | 2 | 189 | 0 | 0 | 1 | 3 | 4 | crates/superdeduper-hmac-signer/AGENTS.md |
| crates/superdeduper-log | 3 | 220 | 0 | 0 | 0 | 4 | 4 | crates/superdeduper-log/AGENTS.md |
| specs | 2 | 743 | 0 | 0 | 0 | 4 | 4 | specs/AGENTS.md |

---

## Audit coverage

### Directories audited

- `/home/neomatrix/projects/mickfixesjunk/superdeduper/` (root) - `AGENTS.md`
- `src/` - `src/AGENTS.md`
- `src/gui/` - `src/gui/AGENTS.md`
- `src/gui/widgets/` - `src/gui/widgets/AGENTS.md`
- `src/gui/preview/` - `src/gui/preview/AGENTS.md`
- `src/leaderboard/` - `src/leaderboard/AGENTS.md`
- `src/pipeline/` - `src/pipeline/AGENTS.md`
- `src/pipeline/hash/` - `src/pipeline/hash/AGENTS.md`
- `src/pipeline/hash/format/` - `src/pipeline/hash/format/AGENTS.md`
- `src/pipeline/image_hash/` - `src/pipeline/image_hash/AGENTS.md`
- `src/pipeline/audio_hash/` - `src/pipeline/audio_hash/AGENTS.md`
- `src/inventory/` - `src/inventory/AGENTS.md`
- `src/platform/` - `src/platform/AGENTS.md`
- `src/winapi_wrappers/` - `src/winapi_wrappers/AGENTS.md`
- `src/exclusions/` - `src/exclusions/AGENTS.md`
- `src/debug/` - `src/debug/AGENTS.md`
- `src/bin/` - `src/bin/AGENTS.md`
- `crates/superdeduper-bench-iface/` - `crates/superdeduper-bench-iface/AGENTS.md`
- `crates/superdeduper-bench-real/` - `crates/superdeduper-bench-real/AGENTS.md`
- `crates/superdeduper-hmac-signer/` - `crates/superdeduper-hmac-signer/AGENTS.md`
- `crates/superdeduper-log/` - `crates/superdeduper-log/AGENTS.md`
- `tests/` - `tests/AGENTS.md`
- `scripts/` - `scripts/AGENTS.md`
- `docs/` - `docs/AGENTS.md`
- `specs/` - `specs/AGENTS.md`

### Directories flagged as missed

No agent's summary contained the words "missed" or "did not audit." Reasonable adjacent omissions for a future pass:

- `assets/`, `data/`, `diagnostics/`, `examples/` - not in the 25-agent assignment list
- `.github/workflows/` - cross-referenced from `TESTING.md` and root findings but not audited as a directory
- `crates/` top-level Cargo workspace metadata - audited only via per-crate subdirectory passes

---

## Methodology

This audit was multi-agent and read-only. Twenty-five subagents were spawned in parallel, one per directory, each with explicit instructions to: (1) read every file in its assigned directory; (2) write a fresh `AGENTS.md` documenting the directory's purpose, per-file public API, invariants, dependency graph, refactor hints, and wire surfaces; (3) return a structured-findings JSON payload against a shared severity schema (p1 / p2 / p3 / info) with kind tags (`doc-drift`, `broken-claim`, `stale-comment`, `dead-code`, `invariant-undocumented`, `missing-doc`, `inconsistent-naming`, `other`). No agent ran `cargo build`, `cargo test`, `cargo run`, or made code changes - this was a documentation-and-static-grep audit.

**Limitations:** Per-directory agents can miss cross-directory drift. The findings in this file capture cross-dir issues only where an individual agent surfaced both ends of the link (for example, the dedupe.rs / pipeline/iocp.rs / docs/iocp-tier3-spec.md trio about the deferred IOCP rewrite was caught because three agents independently flagged the same stale narrative). Drift that spans directories where no single agent saw both sides would slip through. Future audits should consider a second-pass synthesis step that grep-traverses the structured-findings JSON for cross-references.
