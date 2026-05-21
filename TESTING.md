# Testing Strategy

This document is the testing plan for `superdeduper`. It augments the
`Quality Bar` section of the project spec with the concrete test
shapes, harnesses, and CI matrix we'll maintain.

The guiding principle: **correctness is non-negotiable, performance is
the product**. Tests must prove both, and a regression in either is a
release blocker.

## 1. Correctness — platform-agnostic

These tests exercise the pipeline stages that don't depend on the Win32
API and so can run on any host (useful for fast local iteration and for
the Linux CI lane).

### 1.1 Adversarial content shapes

A parametrized test family that plants pairs of files designed to
defeat exactly one progressive-hashing tier:

| Shape | Defeats | Why it matters |
|---|---|---|
| Same head, different tail | Tier 1 | Catches premature "done" after head sample |
| Same head + tail, different middle | Tier 2 | Catches the head/tail-only short-circuit |
| Identical except one bit at random offset | Tier 3 | Final hash must distinguish |
| Identical except final sector | Tier 3 | Off-by-sector errors in stream reads |
| Empty + empty | zero-byte handling | Spec requires one group, not silent drop |
| 1-byte + 1-byte (same/different) | min-size boundary | Both included with `--min-size 0` |

Each shape is one test case; the harness runs them as a table.

### 1.2 Property tests (proptest)

* **Equivalence-class recall, no false positives.** Generate a random
  universe `[(content_class_id, file_count)]`, materialize files, scan,
  and assert: every class with `count ≥ 2` appears as exactly one
  group; no group contains files from different classes. The oracle
  computes SHA-256 directly; we compare grouping, not hash values.
* **Order invariance.** The same universe produces the same set of
  groups regardless of input file order, scan-root order, or
  `--threads` value.
* **Tier progression monotonicity.** A file released at tier N is never
  reported as a duplicate, no matter what other files exist.
* **Idempotence.** Running `scan` twice on the same tree yields
  identical results (modulo the cache making the second run faster).

### 1.3 Boundary fuzz

Property test over file sizes drawn from a distribution biased toward
the tier thresholds: `min_size ± 1`, `4 KiB ± 1`, `64 KiB ± 1`,
`256 KiB ± 1`, `1 MiB ± 1`, `4 GiB + 1` (32-bit boundary). Each test
verifies the file is correctly included/excluded and, if included,
hashed correctly.

### 1.4 Filter matrix

Table-driven cross product over `--include` × `--exclude` × `--min-size`
× `--max-size` × `--follow-links`. For each cell, plant a known input
and assert the included set matches the spec.

### 1.5 Output formats

Snapshot tests for the `text`, `json`, and `csv` writers against a
fixed group set. Schema version bumps are intentional and trip the
snapshot guard.

## 2. Correctness — Windows-specific

These are the differentiator tests: they verify the pieces no other
deduper bothers to do well on Windows.

### 2.1 VHD-backed NTFS fixture

A reusable test harness creates a VHD via `New-VHD`, formats NTFS,
mounts it, populates it, runs the scan, unmounts, deletes. This gives
us reproducible MFT layouts and USN journals without touching the
host's real filesystem. Helper: `tests/support/vhd.rs`.

### 2.2 MFT vs walker oracle

For every Windows integration test we run the same input through both
the `FSCTL_ENUM_USN_DATA` fast path and the `FindFirstFileExW`
fallback and assert identical output. Catches any inventory drift
between the two paths.

### 2.3 Hardlink and junction collapse

Plant N files linked via `mklink /H` (hardlinks) and `mklink /J`
(junctions). Assert hardlinked files collapse into a single
equivalence class without any reads (instrumented hash counter must
stay at zero for the collapsed members). Junctions are followed only
with `--follow-links`.

### 2.4 ReFS block-clone detection

On a separate ReFS-formatted VHD, use `CopyFileEx` with
`COPY_FILE_REQUEST_COMPRESSED_TRAFFIC` (or the equivalent
`DUPLICATE_EXTENTS_DATA` IOCTL) to create block-cloned files. Assert
they are detected as duplicates without hashing.

### 2.5 USN delta correctness

1. Scan a populated volume cold; record cached USN.
2. Modify the last byte of exactly one file.
3. Re-scan. Assert: only that file rehashes (instrumented counter);
   the result set is correct; reported "files rehashed" count is 1.

### 2.6 Long paths

Plant paths longer than `MAX_PATH` (260) via the `\\?\` prefix. Both
inventory paths and the hash reader must succeed.

### 2.7 Concurrent mutation

Spawn a background thread that touches/extends/truncates random files
mid-scan. Engine must not crash, must not produce a corrupt group, and
must report any racing file as either correctly grouped or skipped
with a per-file error — never silently mixed.

### 2.8 Permission denied & locked files

Plant files with restrictive ACLs and files opened with exclusive
share modes. Engine continues, skipped files appear in a warnings
section, exit code stays clean.

### 2.9 Volume-spanning scans

Roots that span two volumes (one HDD VHD, one SSD VHD) — engine must
schedule them independently and saturate both drives. Verified via
per-drive throughput counters logged at trace level.

### 2.10 Direct I/O alignment

White-box test: every read submitted to the IOCP layer is sector-size
aligned in offset, length, and buffer address. Enforced by an
assertion in debug builds; a dedicated test triggers reads at every
tier and verifies the assertion never fires.

## 3. Performance regression

The "fastest" claim is meaningless without numbers that block CI when
they regress.

### 3.1 Criterion benches (already in spec)

* MFT enumeration throughput (files/sec on a 100k-file VHD)
* LCN-sorted vs naive parallel read order on a synthetic HDD-pattern workload
* Tier 0 fingerprint speed per supported format
* End-to-end scan on a synthetic mixed-size dataset

### 3.2 Performance gates

CI fails if any of these ratios degrade by more than 10% vs the
last-green main:

* MFT enum > 200k files/sec on the standard VHD
* LCN-sorted IOCP ≥ 2× the naive parallel baseline on the synthetic HDD pattern
* Cache hit ratio on an unchanged-tree rescan ≥ 99%
* RSS during a 1M-file scan ≤ 256 MB

### 3.3 Comparative bench vs fclones

A separate workflow (`bench-vs-fclones.yml`) runs `fclones group` and
`superdeduper scan` on a fixed 50 GB synthetic dataset and writes the
results to `bench-results.md`. The PR comment shows the delta.
Non-blocking but watched.

## 4. Safety tests for `dedupe`

Destructive operations need belt-and-braces tests because the cost of
a bug is unrecoverable.

* **Reference path inviolable.** Proptest with 100 iterations: random
  reference paths, random destructive actions; the engine must never
  emit an action targeting any reference path.
* **System path guard.** For each blocked prefix from the spec
  (`C:\Windows`, `C:\Program Files`, `C:\Program Files (x86)`,
  `C:\ProgramData`, `%USERPROFILE%\AppData`), assert refusal without
  `--allow-system-paths` and acceptance with it.
* **`--dry-run` is truly dry.** Snapshot the filesystem before and
  after; bytes must be identical, mtimes must be identical.
* **Mid-flight invalidation.** Between hash and action, mutate a file.
  The action must abort that group with a clear error; other groups
  proceed.
* **Recycle is reversible.** After `--action recycle`, the file must
  be retrievable from the Recycle Bin via the standard Win32 APIs.
* **Hardlink replacement preserves content.** After `--action
  hardlink`, all original paths read the keeper's bytes; free space
  decreases by `(N-1) × size` per group.
* **Cross-volume hardlink refusal.** Attempting to hardlink across
  volumes must refuse, not silently fall back to copy.

## 5. Fuzz

`cargo fuzz` targets, run weekly in CI plus on PRs that touch the
relevant module:

* **Tier 0 format parsers** (one target per format: MP4, MKV, JPEG,
  PNG, ZIP, PDF, MP3). Input: arbitrary bytes. Expectation: no panic,
  no crash, returns either a valid fingerprint or an error.
* **MFT record decoder.** Input: a synthetic
  `USN_RECORD_V2`/`V3`/`V4` bytestream. Expectation: parser returns
  Result, never panics, never reads OOB.
* **Path reconstructor.** Input: a random `(parent_ref → name)` graph
  including cycles, missing parents, duplicate refs. Expectation:
  cycles are detected and broken; partial paths returned with a flag.
* **Cache row decoder.** Input: arbitrary bytes in the `tier*_hash`
  BLOB columns. Expectation: malformed rows are treated as
  cache-miss, never panic.

## 6. CI matrix

GitHub Actions, two workflows:

### `ci.yml` — every PR

| OS | FS | Mode | What runs |
|---|---|---|---|
| windows-latest | NTFS (VHD) | admin | All correctness + Windows-specific tests |
| windows-latest | ReFS (VHD) | admin | ReFS block-clone + cross-volume tests |
| windows-latest | NTFS (host) | non-admin | Walker fallback only |
| ubuntu-latest  | ext4       | n/a   | Platform-agnostic tests + clippy + fmt |

Code coverage via `cargo-llvm-cov`; merging is blocked below 85% on
`pipeline::*` and `winapi_wrappers::*`.

Clippy at `-D warnings`, including `clippy::pedantic` with an
explicit allow-list for the rare opt-outs.

### `perf.yml` — main branch only, nightly

Runs the criterion benches and the comparative-vs-fclones job; updates
`benches/baseline.json` with the new numbers; opens a PR if any
performance gate from §3.2 fails.

## 7. Test code quality

* Every test asserts a single named property. Multi-assertion tests
  are split.
* No `sleep`-based synchronization; use channels or `Condvar`.
* Test fixtures are content-addressable: `tests/fixtures/<hash>/…` so
  CI caches them across runs.
* No network I/O from tests. Ever.
* `cargo test` runtime budget: under 60 seconds locally with `--release`.
  The Windows-specific VHD tests run under a separate `cargo test
  --test windows_integration` to keep the inner loop fast.
