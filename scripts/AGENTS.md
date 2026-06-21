# scripts -- AGENTS guide

## Purpose

This directory holds the engineering automation around the superdeduper
Rust workspace: release cross-builds, CI/precheck gates, git hooks,
end-to-end harnesses, perf/io-thread sweeps, swarm health checks, and
end-user install. Nothing here is shipped inside the engine binary;
all of it is used by humans/agents on the dev box (WSL on `neo-wsl`)
and by sdd-testwin on the Windows host.

Most scripts cluster around the **release cycle**: cross-compile
binaries via `cargo zigbuild`, drop to two known paths
(per-sha archive + latest-overwrite), gate each drop with the
`fast-gate.sh` dispatcher, and surface stranded verified work before
tagging via `release-integrity-check.sh`. Two `pre-push`/`precheck`
scripts guard against the recurring Cargo.lock "path source" CI
regression. `swarm-health-check.sh` is the boss-agent observability
sweep over the tmux swarm.

`bench/` is the perf-investigation harness: PowerShell rigs that drive
the Windows GUI/CLI through cold-cache HDD scenarios with RAMMap +
`FILE_FLAG_NO_BUFFERING` pre-reads, and Bash counterparts for
io-threads scaling on Linux/WSL.

## Files

### `build-mac-tailnet.sh`
SSH out to a tailnet-reachable Intel Mac, native-build the CLI and GUI
release binaries there, rsync them back into the canonical
`target/x86_64-apple-darwin/release/` path. Exists because
`cargo-zigbuild` cannot satisfy AppKit/Foundation/libobjc. Soft-fails
with exit 2 if the Mac is unreachable so `cross-build-drop.sh` can
skip rather than abort.
- Public surface: env vars `MAC_BUILD_HOST` (default `macbook-air`),
  `MAC_BUILD_USER` (default `mick`), `MAC_BUILD_DIR`.
- Exit codes: 0 ok, 1 build/rsync fail, 2 unreachable, 3 bootstrap fail.
- Called by: `cross-build-drop.sh`.
- Invariant: ONLY produces x86_64-apple-darwin output; aborts on Apple
  Silicon hosts (rc=3).

### `check-feature-flag-consistency.sh`
CI smoke-diff between `scripts/cross-build-drop.sh` and
`.github/workflows/release.yml` feature flags. Catches the v0.2.4
regression class where the GH-release binary shipped without
`telemetry`/`similar-*`. Also enforces a Tier-4 floor
(`gui telemetry similar-images similar-audio` for GUI builds,
`+ audio` on Windows GUI).
- Exit codes: 0 match, 1 mismatch / floor-miss, 2 parse error.
- Called by: external CI / pre-merge.
- Tuples checked: (windows,cli), (windows,gui), (linux,cli),
  (linux,gui).

### `cross-build-drop.sh`
Cross-build 4-to-8 release binaries (Linux musl CLI/GUI, Windows GNU
x86_64 CLI/GUI, Windows ARM64 gnullvm CLI/GUI, optional Intel Mac
CLI/GUI via tailnet) via `cargo zigbuild`. Drops to both
`/mnt/c/Users/NeoMatrix/sdd-builds/<sha>/` (archive-by-sha) AND
`/mnt/c/Users/NeoMatrix/projects/mickfixesjunk/` (latest), writes
`SHA256SUMS` + `LATEST.txt`. Runs the post-build fast-gate as a
promote gate; honors `SD_ARCHIVE_ONLY=1`, `SD_SKIP_FAST_GATE=1`,
`SD_SKIP_MAC=1`.
- Invariants:
  - Per-sha archive is never overwritten (warn-only if it exists).
  - Latest dir is ALWAYS overwritten on a green gate.
  - Branch-behind-main check warns + 5s sleep when building from a
    branch behind `origin/main`.
- Called by: humans/agents from the WSL host.

### `fast-gate.sh` / `fast-gate.ps1`
Thin engine-side dispatchers to the executor-owned fast-subset gate
wrapper. Spec: `testdesign/specs/fast-subset-gate.md`.
- Contract: exit 0 GREEN, 1 row-failed, 2 broken (refuse to ship
  ungated), 3 absent (ship-with-warning).
- Env: `SD_BIN` (REQUIRED), `SD_HARNESS_DIR`, `SD_GATE_WRAPPER`.
- The Linux .sh dispatches to `testrunner` harness; the .ps1 to
  `sdd-testwin` harness. Both expect a wrapper named
  `cli-matrix-fast-gate.sh`/`.ps1`.

### `install-hooks.sh`
One-time installer; symlinks `.git/hooks/pre-push` ->
`scripts/pre-push-hook.sh`. Idempotent.

### `install.sh`
End-user one-line install. Downloads the latest GH release tarball
for the current OS+arch, verifies SHA256SUMS, installs into
`$SUPERDEDUPER_INSTALL_DIR` || `$HOME/.local/bin` ||
`/usr/local/bin` (sudo if needed).
- Env overrides: `SUPERDEDUPER_VERSION`, `SUPERDEDUPER_INSTALL_DIR`.
- Exit codes: 0 ok, 1 generic fail, 2 checksum mismatch, 3
  unsupported platform.
- Platforms: Linux x86_64; macOS x86_64 + aarch64; FreeBSD/ARM Linux
  rejected.

### `iothread-sweep.sh` / `iothread-sweep.ps1`
Quick warm-cache `--io-threads` sweeps to find a saturation point.
Both run a warmup pass then iterate `1 4 8 16 24 48 96` (sh) or
`1 8 16 24 48 96` (ps1), capturing per-stage timing.
- ps1 reads PowerShell `*>` UTF-16-BOM output and selects timing
  lines via regex.
- Linux version uses `$SUPERDEDUPER_BIN` (default
  `./target/release/superdeduper`).

### `pre-push-hook.sh`
Aborts push if `Cargo.lock` resolved `river5` to a path source
(no `source = "git+..."` line). Reflects the local `.cargo/config.toml`
patch that swaps `river5` to a sibling path. Remediation: run
`scripts/precheck.sh`, stage + commit Cargo.lock, retry push.

### `precheck.sh`
Runs CI commands locally (fmt --check, `clippy --locked
-D warnings`, `test --workspace --locked --features gui`) under the
CI-equivalent environment (temporarily stashes `.cargo/config.toml`).
Restores the patch on EXIT trap. Warns if `Cargo.lock` was modified
during the run.

### `release-integrity-check.sh`
Pre-cut release gate: for every `origin/fix/*` and `origin/feat/*`
branch with unmerged commits whose GH-issue is CLOSED, classify
whether the work is genuinely stranded or has been re-implemented.
Uses two signals: `git cherry` patch-id (deduces obvious cherry-picks
/ squash-merges) + dev-health distinctive-identifier method (extracts
added fn/struct/enum/const definitions and `git grep`s them on the
release ref).
- Outcomes per branch: CLEARED / STRANDED / AMBIGUOUS.
- Exit: 0 clean; 1 strand-or-ambiguous; 2 bad release ref.
- Requires `gh` for issue-state; degrades to UNKNOWN if absent.

### `swarm-health-check.sh`
Sweeps every tmux window in the `giga-superdeduper` session,
classifies each agent pane as OK / WEDGED_API_ERROR /
WEDGED_DISCOVERY / UNREACHABLE / IDLE / STOOD_DOWN. Primary signal
is the mtime of the latest Claude Code session JSONL at
`~/.claude/projects/<encoded-cwd>/*.jsonl`. Fallback is
`ACTIVE_PATTERN` regex over `tmux capture-pane` tail.
- Flags: `--quiet`, `--verbose`, `--loop` (re-sweep every
  `SWARM_HEALTH_INTERVAL_S`, default 3600).
- Exit: 0 healthy, 1 wedged.
- Excludes: `codex-review-bridge`, `codex-review-cli`, `design`,
  `dumbo`. Stood down: `czkawka`, `accountant`.

### `test-corpus.py`
End-to-end harness against the manual `/mnt/c/sdd-tests/test{1..50}`
corpus. Per test: bash reset.sh → run Windows EXE
`scan ... --format json --min-size 0 --no-cache` via WSL interop →
parse JSON → assert against per-test callable in `CHECKS`.
- Hardcoded paths: `EXE=/mnt/c/Users/Audio/superdeduper.exe`,
  `CORPUS_ROOT=/mnt/c/sdd-tests`,
  `ARTIFACT=/tmp/superdeduper-corpus-results.json`.
- Special-cases: `SCAN_SUBPATH[12]="scan_root"` for the
  peer-out-of-scope hardlink test; test27 has a pre-check verifying 4
  .jpg files survive reset; test48 uses the README's body-recount
  (22) not the top-table value (20).
- Exit: 0 all PASS, 1 any FAIL/ERROR, 2 EXE missing.

### `bench/Invoke-NoBufferPreRead.ps1`
FILE_FLAG_NO_BUFFERING cold-disk pre-read pass over a corpus. Used as
step 3 of the canonical RAMMap → settle → no-buffer-pre-read → trial
sequence. Buffers are 4 MB (LOH-aligned). Trailing
sub-sector-size bytes intentionally not read. Returns
`{Files,Bytes,WallMs,MBps,Errors}`.
- Wired by `Run-SdHddBench.ps1 -PreReadCache` (see Refactor Hints --
  the wiring claim is doc-only; the harness in this tree does not
  declare a `-PreReadCache` param).

### `bench/Run-MickCorpusMatrix.ps1`
Phase 4/5 ship-gate matrix template for the v0.3.4x perf push.
Five cells: cold-CLI/cold-GUI x Defender-off/on, plus warm-GUI
anchor. Scan-end signal is the appearance + size-stabilization of a
scan-history JSON in the hermetic data dir.
- Edit-before-run vars: `$BINARY_SHA`, `$LABEL`.
- Env vars set: `SUPERDEDUPER_TEST_DATA_DIR`,
  `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1`,
  `SUPERDEDUPER_PERF_INSTRUMENT_RAYON=1`,
  `SUPERDEDUPER_PERF_INSTRUMENT_CHUNK_EMIT=1`,
  `SUPERDEDUPER_FORCE_IO_THREADS=16`.
- Calls scheduled task `sdd-standby-purge` between cells; if absent,
  the matrix silently degrades (no error path).
- Output: `C:\sdd-tests-matrix-output\cold-vs-cold-mick-corpus-<LABEL>\`
  with per-cell stderr/stdout/json, instrumentation-harvest.txt,
  summary.txt.
- Criteria: cold-GUI/cold-CLI Defender-off paired ratio <= 1.10x.
- Backups user state files (`app.ron`, `recent-projects.json`,
  `results-state.json`, `scan-checkpoint.json`) under suffix
  `.benchbak-<sha>-<label>`; restores in `finally`.

### `bench/Run-SdHddBench.ps1`
Earlier-generation HDD-profile harness (sdd-testwin owner). Sweeps
`--io-threads` (default `1,2,4,8,16,32,96`) over a matrix or
runs a single-config full-corpus validation. Optional cold-cache via
RAMMap `-Ew -Es -Em -E0` between trials; optional 1 Hz PhysicalDisk
counters. Outputs CSV + Markdown table to `-OutDir`.
- Companion methodology doc:
  `docs/perf/hdd-profile-bench-methodology.md`.
- Hard-coded fallback path: `C:\Tools\RAMMap.exe`.

### `bench/iothread-scaling.sh`
Warm-cache Linux variant: sweeps `--io-threads in {1,2,4,8,16,32,64}`,
3 runs each, emits a Markdown table of (Wall, Stage4, Tier1 CPU,
Tier3 CPU). Used during v0.3.29 work-stealing investigation.

### `bench/wsl-profile.sh`
sd-vs-czkawka comparative wall + per-stage breakdown + peak RSS,
3 runs each. Warm-cache (no `sudo drop_caches`). Reads
`SUPERDEDUPER_LOG=info` tracing for `walk complete files=N`.

### `fast-gate.ps1` (PowerShell engine-side dispatcher)
See `fast-gate.sh` -- same contract, Windows variant. Currently
hard-coded to exit 3 (absent) until sdd-testwin authors the wrapper
(see header line 14-15).

## Invariants / Gotchas

- **Cargo.lock path-source poison.** Any local `cargo build/check`
  with `.cargo/config.toml` river5 patch active will rewrite the lock
  to omit the `source = "git+..."` line. `pre-push-hook.sh` catches
  this; `precheck.sh` is the canonical fix path (stashes the patch,
  rebuilds the lock against git).
- **cross-build drop has two destinations always.** Per the
  [[sdd-builds archive required on every release]] memory the build
  must go to BOTH `mickfixesjunk\` (latest, overwrite) AND
  `sdd-builds\<sha>\` (per-commit archive for Mick's bisects). Never
  drop only one.
- **fast-gate exit-2 vs exit-3 distinction is load-bearing.**
  exit-2 (broken) blocks promote; exit-3 (absent) ships-with-warning.
  The cross-build-drop wrapper acts on both; do not collapse them.
- **`release-integrity-check.sh` is conservative.** Both STRANDED
  (true strand) and AMBIGUOUS (partial-marker / no-marker) block the
  cut. Cleared (re-implementation by definition-identifier presence)
  is the only auto-pass.
- **swarm-health-check excludes `dumbo`** by name. Per
  [[feedback_dumbo_cleanroom_isolation]] sweeping the pane wouldn't
  itself leak, but a WEDGED nudge from design would. Skipping the
  pane is the cleanroom-preserving choice.
- **Mick-corpus matrix backups must round-trip.** The `finally` block
  restores Defender state + the four user state files. Any new state
  file added to the GUI must be added to the `$stateFiles` array or
  the matrix will not restore real-user state across cells.
- **`Invoke-NoBufferPreRead.ps1` does NOT read trailing
  sub-sector-size bytes.** Intentional -- the pass exercises the
  disk path, not byte-correct content.

## Dependencies

- **INCOMING (callers from outside this dir):**
  - `.github/workflows/release.yml` (build feature strings are
    smoke-diffed against `cross-build-drop.sh`).
  - Humans/agents running the release cycle.
  - Boss / design agent running `swarm-health-check.sh --loop`.
  - sdd-testwin running `bench/*` PowerShell rigs.
  - The pre-push hook (`.git/hooks/pre-push` -> `pre-push-hook.sh`).
- **OUTGOING (other dirs/repos):**
  - `cargo zigbuild`, `cargo`, `rustup` on the toolchain.
  - `RAMMap.exe` (Sysinternals; expected on PATH or `C:\Tools\`).
  - `gh` CLI (release-integrity-check.sh).
  - `tmux`, Claude Code session JSONL dir (swarm-health-check.sh).
  - The `testrunner`/`sdd-testwin` harness dirs via
    `SD_HARNESS_DIR`.
  - The engine itself via the compiled `superdeduper(-gui)(.exe)`.

## Refactor Hints

- **`check-feature-flag-consistency.sh` macOS gap.** The header says
  "macOS-only build in release.yml isn't checked because the local
  cross-build script has no macOS path." That premise is now stale:
  `cross-build-drop.sh` invokes `build-mac-tailnet.sh`, and
  `release.yml` has a `build-macos:` section (lines 267-319 of the
  workflow). The script's tuple list (`win-cli/win-gui/linux-cli/
  linux-gui`) should grow `mac-cli` + `mac-gui`. Doc + code drift
  pair.
- **`fast-gate.ps1` header comment.** Says "Until sdd-testwin authors
  the Windows wrapper, this dispatcher exits 3 (absent)." Verify
  current status; if the wrapper now exists on the Windows box, the
  comment is stale.
- **`scripts/iothread-sweep.sh` thread list mismatches `.ps1`.** sh
  iterates `1 4 8 16 24 48 96`; ps1 iterates `1 8 16 24 48 96` (no 4).
  Probably an oversight -- align both or document why.
- **`Invoke-NoBufferPreRead.ps1` header claims
  "Run-SdHddBench.ps1 wires this in when -PreReadCache is passed."**
  Run-SdHddBench.ps1 declares no `-PreReadCache` parameter and does
  not invoke this script (it uses RAMMap-only cache eviction). Either
  doc-drift or unfinished wiring; mark or fix.
- **`Run-MickCorpusMatrix.ps1` scan-history field check.** Uses
  `started_at_unix` / `completed_at_unix`. Confirm these are the
  actual on-disk field names emitted by the engine's scan-history
  writer; if the schema changed (e.g. `started_at` /
  `completed_at`), every Mick-corpus matrix would false-ERROR.
- **`swarm-health-check.sh` STOOD_DOWN list duplication.**
  `czkawka` and `accountant` appear both as agents in
  STOOD_DOWN and as memories; consider a YAML or env list externalized
  so the swarm config isn't edited in-script.
- **`test-corpus.py` test27 jpg-count == "4"** is brittle to README
  changes; consider asserting `>= expected_count` or making 4 a
  named constant.
- **`release-integrity-check.sh` marker regex is rust-only**
  (`-- '*.rs'` filter on `git grep`). Branches whose deliverable is
  pure-spec / pure-yaml fixtures will be auto-AMBIGUOUS regardless of
  their actual landed state. Acceptable for now (the gate is meant
  for engine), but document the limitation.
- **Suspect dead code: none.** All scripts have a clear caller (CI,
  cross-build, install one-liner, sdd-testwin, manual perf rig). No
  hits for orphaned scripts.

## Wire Surfaces

- **install.sh end-user surface:**
  `https://github.com/mickfixesjunk/superdeduper/releases/download/<tag>/superdeduper-<version>-<os>-<arch>.tar.gz`
  + sibling `SHA256SUMS`. Tarball layout
  `superdeduper-<version>-<os>-<arch>/{superdeduper,superdeduper-gui}`.
- **cross-build-drop.sh disk surface:**
  - `/mnt/c/Users/NeoMatrix/sdd-builds/<sha>/`
  - `/mnt/c/Users/NeoMatrix/projects/mickfixesjunk/`
  Files: `superdeduper-{linux-x86_64,gui-linux-x86_64,
  windows-x86_64.exe,gui-windows-x86_64.exe,windows-arm64.exe,
  gui-windows-arm64.exe[,mac-x86_64,gui-mac-x86_64]}`,
  `SHA256SUMS`, `LATEST.txt`.
- **fast-gate exit-code contract (engine wire):** 0/1/2/3 as above,
  consumed by `cross-build-drop.sh` post-build hook.
- **Environment variables read:**
  - `SD_BIN`, `SD_HARNESS_DIR`, `SD_GATE_WRAPPER` (fast-gate
    dispatchers).
  - `SD_SKIP_FAST_GATE`, `SD_SKIP_MAC`, `SD_ARCHIVE_ONLY`,
    `MAC_BUILD_HOST`, `MAC_BUILD_USER`, `MAC_BUILD_DIR`
    (cross-build-drop.sh + build-mac-tailnet.sh).
  - `SUPERDEDUPER_BIN`, `SWEEP_LOG`, `SWEEP_THREADS`
    (iothread-sweep.sh).
  - `SUPERDEDUPER_TEST_DATA_DIR`,
    `SUPERDEDUPER_PERF_INSTRUMENT_UPDATE`,
    `SUPERDEDUPER_PERF_INSTRUMENT_RAYON`,
    `SUPERDEDUPER_PERF_INSTRUMENT_CHUNK_EMIT`,
    `SUPERDEDUPER_FORCE_IO_THREADS`
    (Run-MickCorpusMatrix.ps1).
  - `SUPERDEDUPER_CHANNEL=local` (Run-SdHddBench.ps1).
  - `SUPERDEDUPER_LOG` (wsl-profile.sh).
  - `SUPERDEDUPER_VERSION`, `SUPERDEDUPER_INSTALL_DIR` (install.sh).
  - `SWARM_HEALTH_INTERVAL_S`, `SWARM_JSONL_FRESH_S`,
    `CLAUDE_PROJECTS_ROOT`, `SWARM_WORKDIR_ROOT`
    (swarm-health-check.sh).
  - `CZKAWKA_BIN` (wsl-profile.sh).
  - `FAST_GATE_CORPUS` (fast-gate.sh; passes through to wrapper).

## Non-source artifacts

None in this dir (all source).
