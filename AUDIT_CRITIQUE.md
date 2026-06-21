# AUDIT_CRITIQUE.md - Completeness review of superdeduper codebase audit

> **HISTORICAL — pre-follow-up state.** This critique was written immediately
> after the first-pass 25-agent audit and lists 5 unaudited subtrees as the
> primary coverage gap. Those subtrees were subsequently audited in a follow-
> up pass; see [FINDINGS_FOLLOWUP.md](./FINDINGS_FOLLOWUP.md) for the closure
> (44 additional findings, 0 P1, and the 7 missing `AGENTS.md` files now
> committed alongside this PR). The original recommendations below are kept
> intact for the audit trail; the "Add the 5 missing AGENTS.md files"
> follow-up at the end of this document IS NOW COMPLETE.

Post-audit critique by the completeness critic. Scope: verify that the 25-agent
audit (FINDINGS.md rollup + per-directory AGENTS.md files) actually covers the
repository, and flag what was missed.

---

## 1. Coverage gaps

### AGENTS.md presence (25/25)

All 25 expected per-directory AGENTS.md files are present on disk. Line counts
range from 64 lines (src/debug) to 503 lines (crates/superdeduper-bench-real).
None are stubs. FINDINGS.md exists at 589 lines / ~35 KB and is non-trivial.

### Unaudited directories that contain source / config

The audit assigned an agent to every directory that holds Rust source. The
following directories were NOT audited but contain meaningful artifacts; flag
each for follow-up:

- `.github/workflows/` - 2 YAML files (ci.yml, release.yml). NOT covered by any
  AGENTS.md, despite TESTING.md and scripts/AGENTS.md both making claims about
  CI workflow contents. The 25-agent split treated CI as out-of-tree.
- `src/platform/linux/` - 4 files (mod.rs, mount_info.rs, reflink.rs, trash.rs)
  loaded via `pub mod linux;` from src/platform/mod.rs. The platform/ AGENTS.md
  is at the parent level and covers these files inline, so this is not a gap
  per se, but no dedicated linux/ AGENTS.md exists. Acceptable.
- `examples/` - hash_microbench.rs (a real benchmark referenced from
  src/pipeline/hash.rs comments). NOT audited. One-file directory but it is
  Rust source.
- `data/` - cpu-brackets-catalog.json (load-bearing leaderboard data file).
  NOT audited. The leaderboard AGENTS.md may reference this implicitly.
- `schema/` - submit.schema.json (wire contract for leaderboard submit
  endpoint). NOT audited. Cross-references to submit shape live in
  crates/superdeduper-bench-real and src/leaderboard.
- `docs/perf/` and `docs/testing/` - 3 markdown files total, NOT audited by
  docs/AGENTS.md (docs/AGENTS.md only covers top-level docs/*.md).
- `specs/historical/` - 1 file (egui-kittest-scan-perf-mick-shape.md). NOT
  audited; specs/AGENTS.md covers the parent dir.
- `tests/fixtures/` - 1 binary fixture (v6_cache.sqlite). Not source; flagging
  for completeness only.
- `diagnostics/` - 438 generated `report-*.txt` files. Not source; correctly
  skipped.
- `assets/` - 6 binary image files. Not source; correctly skipped.

Net: 5 unaudited subtrees that contain source / contract files
(.github/workflows, examples, data, schema, docs/perf, docs/testing,
specs/historical -- counted as 5 functional clusters).

---

## 2. Quality concerns from the random sample

Sampled three AGENTS.md files across the effort spectrum:

### High-effort: crates/superdeduper-bench-real/AGENTS.md (503 lines)
Required sections all present. Files section richly populated. Strong
invariants block. Reasonable spot-check pass.

### Medium-effort: src/gui/AGENTS.md (476 lines)
Required sections all present. Files section is exhaustive (per-file headers,
public surface, env vars, feature gates). Strong cross-references to
widgets/ and preview/ subdirs (handed off to other agents). Pass.

### Low-effort: src/debug/AGENTS.md (64 lines) and src/bin/AGENTS.md (68 lines)
Both cover Purpose / Files / Invariants / Dependencies / Refactor Hints /
Wire Surfaces. src/debug/AGENTS.md is short because the directory has only
two files (mod.rs + snapshot.rs); src/bin/ has three small bins. Section
density is appropriate to surface area. Both pass.

Quality concern: none of the sampled files has an explicit "Last updated" or
"Schema version" marker. If these are intended to be living docs, a future
re-audit will have no way to detect staleness without re-reading each file.
Low priority - flag for follow-up.

---

## 3. Suspected cross-directory drift the per-agent audits missed

Spot-checked 5 cross-file references. One drift hit not surfaced in
FINDINGS.md:

### A) Feature flag `similar-images` vs prose framing
- Cargo.toml line 204 defines `similar-images = ["dep:image", "dep:image_hasher"]`.
- src/pipeline/image_hash/mod.rs line 24: `#![cfg(feature = "similar-images")]`.
- This matches. The audit correctly flagged image_hash docstring drift (P1 at
  src/pipeline/image_hash/mod.rs:89) but did NOT flag a separate issue:
  Cargo.toml line 137 comment says "WebP / GIF / BMP / TIFF / ICO; image_hasher
  provides the dHash / ..." which is internally consistent.
- No new drift here. Pass.

### B) Linux platform submodule
- src/platform/mod.rs line 35 declares `pub mod linux;`.
- src/platform/linux/{mod.rs,mount_info.rs,reflink.rs,trash.rs} exist.
- src/platform/AGENTS.md (230 lines) is at the parent and appears to cover
  these inline. NOT a drift, but the per-agent split could have been finer.

### C) FINDINGS rollup vs README version banner
- Cargo.toml line 3: `version = "0.3.42"`.
- README.md is flagged in FINDINGS at P2 ("v0.1.x is feature-active" + "v0.2.1"
  pin example) - correctly caught.

### D) Telemetry feature default
- Cargo.toml line 184: `default = []`.
- FINDINGS P2 at Cargo.toml:85 ("Default-on in release builds") and at
  SECURITY.md:90 correctly catch the telemetry-default-on drift. Pass.

### E) Examples directory orphan
- examples/hash_microbench.rs exists.
- src/pipeline/audio_hash/mod.rs:60 was flagged in FINDINGS for referencing a
  non-existent `examples/audio_profile.rs`. The audit caught the audio side
  but did NOT explicitly check whether hash_microbench.rs is wired into CI or
  documented. Mild gap.

### Cross-drift flags identified: 1
(unaudited examples/ directory orphan - hash_microbench.rs has no AGENTS.md
coverage and no surfacing in scripts/ or docs/ agents.md files).

---

## 4. Recommendation: production-ready or follow-up pass?

### Verdict: production-ready with a small follow-up scope

The 25-agent audit is genuinely thorough. FINDINGS.md is a 132-finding rollup
with reasonable severity discipline (3 P1, 41 P2, 23 P3, 65 Info), and every
P1 has a concrete fix. The per-directory AGENTS.md files are usable
orientation documents - each has the required sections, the Files lists are
filled in with real file names + public API surfaces, and the Wire Surfaces /
Invariants blocks carry load-bearing detail (not boilerplate).

### Follow-up scope (small)

1. Add or fold the following 5 unaudited subtrees into the next pass:
   - `.github/workflows/` (cross-references in TESTING.md and scripts/AGENTS.md
     refer to CI workflow names that an audit could verify).
   - `examples/` (hash_microbench.rs).
   - `data/` (cpu-brackets-catalog.json - wire contract with leaderboard).
   - `schema/` (submit.schema.json - wire contract with bench-real).
   - `docs/perf/` + `docs/testing/` + `specs/historical/` (3 markdown files
     skipped by parent AGENTS.md scopes).

2. Add a small "Schema-version" or "Last-audited-at-commit" marker to each
   AGENTS.md so future drift detection is mechanical.

3. P1 findings (3) should ship as code fixes, not as doc fixes. The
   src/dedupe.rs mtime-recheck claim, src/exclusions PresetPackId attribution,
   and bench-real fused doc-block are all real correctness issues, not
   cosmetic.

Total estimated follow-up: one short session to add the 5 missing AGENTS.md
files + a P1 sweep in a separate engine-coding session. The bulk of the audit
is complete and trustworthy.
