# HDD-profile bench — methodology + flag inventory

> **Author:** superdeduper-overflow, 2026-05-31
> **Audience:** sdd-testwin (executor), benchmarker + design + overflow (analysis).
> **Context:** design routed 2026-05-31 09:23 PST — bench scan-profile variations on Mick's
> single-physical-HDD E:\ with 4 mirror folders (Option A: sdd-testwin runs on Mick's NEO
> Windows box).
> **Companion harness:** `scripts/bench/Run-SdHddBench.ps1`.

## Goal

Characterise sd's scan-time behaviour on a **single-physical-HDD multi-folder**
workload. The corpus on Mick's NEO box:

- `E:\DROPBOX` — reference/keeper (Mick's authoritative copy)
- `E:\SAMSUNG-T5-1TB` — mirror of previous external drive
- `E:\SEAGATE-2TB` — mirror of previous external drive
- `E:\WD-BLACK-4TB` — mirror of previous external drive

All four folders live on **one physical HDD** (E:). The bench-relevant question:
**does sd's current `--io-threads` default (`threads × 3`) behave well on HDD
heavy queue depths**, or is the right answer lower (sequential reads, head doesn't
thrash)?

Scan-only — **no `dedupe` action ever runs**. The deliverable is a profile matrix:
wall-clock + stage breakdown + peak memory + (optional) PhysicalDisk perf counters
across an `--io-threads` sweep.

## Engine flag inventory (the relevant scan-time surface)

Greppable from `src/cli.rs` (`pub struct ScanArgs`) and the stderr timing emit in
`src/main.rs`. Verified against sd v0.3.1 head (commit 6c70354).

### Invocation pattern for this bench

```
superdeduper.exe scan <path>... --no-cache --io-threads <N> --format json
```

`scan` is the canonical NON-DESTRUCTIVE subcommand. No `--dry-run` flag exists
because `scan` itself doesn't touch files — duplicates are reported only. Destructive
actions live behind `dedupe`, which we **never run** in this bench.

### Flags the harness sweeps / sets

| flag | semantic | default | bench use |
|------|----------|--------:|-----------|
| `--io-threads N` | hashing-worker count for the rayon par_iter | `threads × 3` (≈ 96 on a 32-CPU box) | **sweep target.** Recommended sweep: `1, 2, 4, 8, 16, 32, 96` (engine default = `0` ⇒ `threads × 3`). |
| `--threads N` | OS / inventory-walker / size-grouping thread count | logical CPUs | Hold at default for the matrix; vary in a second-pass sweep if `--io-threads` curve points there. |
| `--no-cache` | disable persistent hash cache | OFF (cache on) | **REQUIRED** for cold/repeatable bench. Otherwise trial 2+ short-circuits via cache hits and the curve is meaningless. |
| `--format json` | structured output | `text` | makes stdout parseable; the harness extracts dup-group counts as a quick sanity gate ("did the scan actually find anything?"). |
| `--min-size BYTES` | skip files smaller than this | `4K` | optional runtime bound — set higher (e.g. `1M`) to skip the long tail of tiny files. NOT used in the default sweep — Mick wants real-shape numbers. |
| `--max-size BYTES` | skip files larger than this | none | **per-file** cap, not corpus-total. Useful only if a single huge file dominates. NOT used in the default sweep. |
| `--placeholders-only` | walker-only (skip stages 2-4) | OFF | NOT a bench mode for HDD-profile work — too short. Available as a "is the walker working?" sanity check. |
| `--force-hash` | Tier-3 every file (bypass size-grouping) | OFF | **DO NOT USE** in this bench — inflates IO load by a factor of 5-10×; results don't transfer to the real-usage scan path. |

### Flags that DO NOT exist (and why this matters)

| missing flag | implication for the bench |
|--------------|---------------------------|
| `--max-files N` | Can't bound the scan by file count. Runtime-bounding via folder subset (see §"Runtime budgeting"). |
| `--max-bytes N` (corpus-total) | Same — runtime-bounding via folder subset. |
| `--deadline T` / time-bound | Can't tell the engine "stop after T minutes." Long-running configs MUST be allowed to complete in the harness (no early-abort) OR run on a subset. |
| `--dry-run` | `scan` is already non-destructive; this flag would be redundant. |

If sdd-testwin hits a config that wants any of the missing flags as a hard
requirement, escalate to engine-main on the bilateral — they may want to ship one.

### Output the harness consumes

sd's `--vvv` (or default `INFO`) tracing emits structured stage-completion lines
via `tracing::info!` macros. Plus stderr gets a human-readable timing block at the
end of every scan:

```
--- timing (river5) ---
stage 1 inventory:    XXX ms (NNN files)
stage 2 grouping:     XXX ms
stage 3 layout:       XXX ms
stage 4 hashing:      XXX ms (wallclock) — bytes_read=Y.YY GiB
  (per-tier CPU-summed includes file open + read + hash; compare effective MB/s side-by-side across algos)
  Tier 0 fmt :    NNN files ·    Y MiB hashed ·   NNN ms CPU-summed · ...
  Tier 1 head:    NNN files · ...
  Tier 2 hmt :    NNN files · ...
  Tier 3 full:    NNN files · ...
total wallclock:      XXX ms
```

The harness greps this block from stderr and emits a markdown table column per
field. **Don't** rely on `--format json`'s top-level `summary.timing_*` fields —
the version-pinning between sd commits has bitten us; stderr `--- timing ---` is
the stable surface.

## Corpus + runtime budgeting

Full corpus is multi-TB. A 7-config sweep at, say, ~2 hours per config = 14 hours
wall. That's too long for an iteration loop, and Mick's NEO is shared.

**Recommended budgeting (configurable in the harness):**

1. **Matrix sweep on a single-folder subset.** Pick `SAMSUNG-T5-1TB` (smallest at
   ~1 TB nominal). Run the full `--io-threads` sweep against that one folder. 7
   configs × estimated 20-60 min each = ~3-7 hours. Bounded + comparable.
2. **Validate on full 4-folder corpus** at the OPTIMAL `--io-threads` value the
   matrix surfaces. One run, ~2-4 hours.
3. **(Optional) Second-pass `--threads` sweep** if the curve from step 1 says
   inventory-walker-side parallelism matters.

Harness defaults to step 1 (`-MatrixSubsetFolder SAMSUNG-T5-1TB` in
`Run-SdHddBench.ps1`); step 2 is `-FullCorpusValidate`.

## Cache regime (cold vs warm)

**Cold-cache** matches Mick's real usage ("first-time scan of these mirror
folders") and is the bench regime of interest.

Windows OS page cache is global; sd's `--no-cache` flag turns off only the
**engine's** persistent SQLite hash cache, not the OS page cache. The two are
separate concerns.

### Cold-cache enforcement options (in order of preference)

1. **RAMMap.exe + `Empty Standby List` + `Empty Working Sets`** (Sysinternals,
   free). Between trials: `RAMMap.exe -Ew` (working sets) and `RAMMap.exe -Es`
   (standby list). The harness's `-DropCacheBetweenTrials` switch invokes this
   if `RAMMap.exe` is on `$PATH`.
2. **Reboot between trials.** Most authoritative; impractical for a 7-config ×
   3-trial sweep (21+ reboots).
3. **Single trial per config + ack the variance.** If RAMMap isn't available,
   run one trial per config and capture variance via the harness's `-Trials 1`
   switch (default 3). The OS page cache will be warm from the previous trial,
   masking real HDD IO time.

**Recommendation: install RAMMap.exe on Mick's NEO and use option 1.** The
download URL is in the harness preflight check.

### What sd's `--no-cache` does

Engine-side: skips the SQLite cache lookup / write for hash results. Does NOT
flush OS page cache. Required for the bench so the second `--no-cache` trial
doesn't short-circuit via the persistent cache.

## Memory profile

The harness captures peak working-set bytes per config via PowerShell's
`Get-Process` polling. This is mainly a sanity-watch — if peak working set climbs
linearly with `--io-threads`, that's a flag (each rayon worker carries a Tier-1
read buffer; bounded but worth observing). Expect ~50-300 MB per worker for
sustained-read configurations.

## Optional: PhysicalDisk performance counters

For a deeper read on HDD-specific behaviour, the harness optionally captures:

- `\PhysicalDisk(N)\Avg. Disk Queue Length` — sustained queue depth
- `\PhysicalDisk(N)\Disk Reads/sec` — IOPS
- `\PhysicalDisk(N)\Avg. Disk sec/Read` — per-IO latency
- `\PhysicalDisk(N)\Disk Read Bytes/sec` — throughput

Sampled every 1 s during the scan via `Get-Counter`. Output goes to a CSV per
trial. Useful for cross-validating the wall-clock curve: at the optimum
`--io-threads`, queue depth should be ~1-2 (HDD's head doesn't benefit from
deeper queues); at oversubscribed `--io-threads`, queue depth balloons and
average sec/read climbs as the head thrashes.

Disabled by default (`-CaptureDiskCounters $false`); set `-CaptureDiskCounters
$true` if Mick's NEO has Performance Monitor counters available + sdd-testwin
has elevation to read them.

## Compatibility

- **PowerShell:** the harness targets Windows PowerShell 5.1 (built-in on
  Win10/Win11) AND PowerShell 7+. No PS-7-only operators (`??`, `?:`, `?.`)
  are used.
- **Elevation:** required if `-CaptureDiskCounters` is set (PhysicalDisk
  counters need admin). Not required otherwise.
- **RAMMap.exe:** required if `-DropCacheBetweenTrials` is set. Free
  download from Sysinternals; URL in the harness preflight check.

## Procedure (what sdd-testwin runs)

1. Install RAMMap.exe if not present
   (https://learn.microsoft.com/en-us/sysinternals/downloads/rammap). Put it on
   `$PATH` or in `C:\Tools\RAMMap.exe`.
2. Build sd: latest main, `cargo build --release --features telemetry`. Drop the
   binary in a known location (the harness defaults to
   `target/release/superdeduper.exe`).
3. Open an elevated PowerShell (Run as Administrator) — required for RAMMap +
   perf counters.
4. From the repo root:
   ```powershell
   ./scripts/bench/Run-SdHddBench.ps1 `
     -SdExe ./target/release/superdeduper.exe `
     -MatrixSubsetFolder 'E:\SAMSUNG-T5-1TB' `
     -OutDir ./bench-out/hdd-2026-05-31 `
     -DropCacheBetweenTrials `
     -Trials 3
   ```
5. Harness writes:
   - `results.md` — markdown table summary.
   - `results.csv` — same data as CSV for further processing.
   - `trial-<config>-<N>.stderr.log` — raw sd stderr per trial (for
     spot-checking the parsing).
   - `trial-<config>-<N>.perf.csv` — perf-counter samples (only if
     `-CaptureDiskCounters`).
6. Optional full-corpus validation run at the optimum:
   ```powershell
   ./scripts/bench/Run-SdHddBench.ps1 `
     -SdExe ./target/release/superdeduper.exe `
     -FullCorpusValidate `
     -IoThreads 16 `  # use whatever the matrix surfaces as optimum
     -OutDir ./bench-out/hdd-2026-05-31-validate `
     -DropCacheBetweenTrials `
     -Trials 3
   ```
7. Post the `results.md` + a one-line headline ("opt at io-threads=N; default
   was X% off; validation matched / didn't match") to the engine bilateral.

## What NOT to bench in this harness

- **No destructive actions.** Don't pass `dedupe`. Don't pass `--action
  recycle/remove`. Don't run alongside any external tool that touches E:\
  during a trial.
- **No multiple sd instances in parallel.** Strictly serial — one config at a
  time. PhysicalDisk counters get noisy if two scanners hit the same platter.
- **No pre-flight `du` / `dir /s` measurement before the timed run.** That
  warms the page cache + adds to the corpus-stat overhead estimate without
  contributing to the profile data. If you need the corpus byte total, get it
  AFTER the matrix sweep completes.
- **No `--force-hash`.** It bypasses size-grouping and inflates IO 5-10× —
  results don't transfer to real-usage scan behaviour.
- **Don't use the dev-channel ChannelBanner / telemetry endpoints** during the
  bench. The harness sets `SUPERDEDUPER_CHANNEL=local` to keep the run
  airgapped from prod state.

## Expected analysis loop

After sdd-testwin posts `results.md`:

1. **overflow + benchmarker** read the io-threads curve.
   - If the curve is FLAT (similar to NEO Windows-NVMe replication in
     `perf-98-findings.md` §"Cross-platform: negative replication on
     Windows"): HDD scheduler is also absorbing the oversubscription. No
     change needed. Doc the result; close.
   - If the curve has a CLIFF (similar to the Linux finding in
     `perf-98-findings.md`): HDD-specific recommendation lands as a new
     row in the `## Recommendation` section of perf-98-findings.md.
2. **design** weighs the recommendation against the shipping
   default-multiplier strategy. May escalate to Mick if a default-change
   decision is needed (current state is HOLD per the 2026-05-31 cross-
   platform negative replication; an HDD-specific exception is a separate
   question).
3. **Optional follow-up bench:** if the optimum changes substantially
   between NVMe and HDD, that's the dependency that motivates option 2
   (auto-tune per Tier) in `perf-98-findings.md`'s `## Recommended fix
   path`.

## Linkage to existing perf docs

- **`docs/perf-98-findings.md`** documents the Linux io-threads cliff
  (warm-cache NVMe regime). This HDD-profile bench is the **cold-cache HDD
  regime** counterpart and feeds the same recommendation matrix.
- **`docs/testing/cli-flag-matrix.md` F-CLI-5 section** documents the engine
  flag surface that this methodology relies on (the `pub struct ScanArgs` flag
  inventory above).

## Open questions

1. Does Mick's NEO have `RAMMap.exe` accessible? If not, sdd-testwin needs to
   either install it (admin elevation required) or accept warm-cache numbers
   with a regime callout in `results.md`.
2. Do we want to vary `--threads` (inventory walker count) in a second-pass
   sweep, or hold it at default? The harness has the hook but defaults to
   "matrix only on `--io-threads`."
3. Should perf-counter sampling default to ON when sdd-testwin's run is
   elevated? Lean: yes, because the HDD-specific queue-depth + sec/read
   data are exactly what disambiguate "oversubscription is fine on HDD" vs
   "oversubscription thrashes the HDD head." Currently default OFF for
   conservatism; flip if sdd-testwin's environment permits.

Flag back to engine bilateral if any of these blocks the run.
