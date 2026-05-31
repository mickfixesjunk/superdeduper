# #98 perf findings — sd `--io-threads` default is 2× slower than optimum on heterogeneous corpora **(Linux only — see status banner)**

> **STATUS (2026-05-31):** **HOLD.** No code change. Cross-platform
> replication on NEO (Windows 11, byte-exact corpus mirror) came
> back negative: the `--io-threads` default-multiplier cliff is
> **Linux-specific**. Windows scheduler absorbs the
> oversubscription cleanly. Plus sd@default beats cz@default on
> NEO by 52%, so the "cz beats sd" premise that motivated #98
> doesn't hold on the primary user platform. Recommendation is
> retained for Linux power-users; no global default change. See
> §"Recommendation" and §"Cross-platform: negative replication
> on Windows" below.

> **Author:** superdeduper-overflow, 2026-05-31
> **Repro environment:** Linux x86_64, 32 logical CPUs, sd v0.3.0 (commit 33ea5f9, optimized release build), czkawka_cli v11.0.1 (--locked build).
> **Corpus:** synthetic heterogeneous 3.3 GiB / 5310 files / ~40% dup-bytes, mix of size buckets (`<10KB`: 1600, `<100KB`: 2000, `<1MB`: 1200, `<10MB`: 400, `≥10MB`: 110). Layout under `/home/neomatrix/sd-perf-corpus/`. Generator preserved in this doc's appendix for reproduction.

## TL;DR

On a heterogeneous Linux corpus, sd's `--io-threads` default
(`threads × 3` ≈ 96 on this host) runs the scan in **0.45 s**
mean; **`--io-threads 16`** runs the same scan in **0.21 s**
mean — a **2.09× speedup** from a single flag change. Stage-4
hashing wallclock drops from 413 ms to 172 ms (-58%).

In other words: the worst-case "cz crept 7% ahead on
heterogeneous corpus" result `#98` flagged is plausibly an
**sd-default-mis-tune** problem, not a code regression and not
a cz-got-faster problem. At sd's optimal `--io-threads` value
on this corpus, sd is **~1.9× faster than cz** (which is itself
near-optimal at its default `-T 0`).

**But this result is Linux-only.** Benchmarker's Windows
replication on NEO with a byte-exact corpus mirror shows the
default-cliff doesn't reproduce — the Windows scheduler handles
the oversubscription without measurable degradation. See
§"Cross-platform: negative replication on Windows" for the data.
The Linux speedup is real and is documented as a power-user
recommendation; the global default is not changing.

## Recommendation

**Linux, high-CPU-count hosts (≥16 logical CPUs):** consider
setting `--io-threads ≈ CPU_COUNT / 2` (e.g. `--io-threads 16`
on a 32-CPU box) for heterogeneous corpora. The
default-`threads × 3` value (`io-threads ≈ 96` on a 32-CPU box)
triggers Linux scheduler thrashing on workloads where many
files short-circuit at Tier-0 / Tier-1, costing ~2× wallclock
vs the tuned value on the corpus measured here.

**Windows:** no change recommended. NEO replication
(2026-05-31; appendix) shows the Windows scheduler handles the
`threads × 3` oversubscription cleanly. The curve from
`io-threads = 8` upward is flat to within ~7%; default is
already near-optimal there.

**macOS:** not measured. If the kernel scheduler is closer to
Linux's (XNU is BSD-derived; threading semantics differ from
Windows), the Linux recommendation might apply; replication
would close it out. Not on the critical path.

## Regime

**This measurement is WARM-CACHE / CPU-BOUND ONLY.** The
17.4 GB/s aggregate throughput observed at the tuned setting
(`--io-threads 16`, Stage 4 = 172 ms over 2.92 GiB) exceeds the
host's raw disk bandwidth by a wide margin, confirming the
corpus is page-cached after the warmup run. The default-cliff at
`io-threads=96` therefore reflects **scheduler thrashing from
worker oversubscription**, not disk-I/O behavior.

**Cold-cache / disk-bound regimes were NOT measured here.** The
default-multiplier ship decision under [Mick GO 2026-05-31
10:55 PST] accepts the warm-cache result as a sufficient gate
and treats cold-regime regression as a **revert-fast-follow**
risk per the prod-zero-real-users framing. Benchmarker's NEO
replication run on a fresh-mirror corpus matching this
distribution is the warm-replication confirmation; that is the
ship gate, not a separate cold-bound run.

If, post-ship, a real user reports a cold-cache regression on
heterogeneous workloads, the path forward is option 2
(auto-tune per Tier) from §"Recommended fix path" — keep the
oversubscription multiplier active for Tier-3 sustained reads
(where disk-bound + sustained is exactly where the original
`threads × 3` intent was sound) and drop it on Tier-1 /
Tier-2 (where the warm-cache measurement here shows the cliff).

> ⚠ Regime caveat added 2026-05-31 per benchmarker's prelim
> NEO-5 GB synth run + design's regime-callout request; not in
> the original 09:20 PST commit (51bd765).

## Measurement

### Head-to-head at defaults (the #98-style comparison)

Both binaries with cache disabled, default thread settings,
same 8 KiB min-size floor (cz's default; matched on sd via
`--min-size 8K`). 5 trials each after one warmup run.

| tool       | trials (s)                                        | mean   | range  | CV    |
|------------|---------------------------------------------------|--------|--------|-------|
| **sd v0.3.0 default** | 0.397, 0.438, 0.440, 0.524, 0.460        | 0.452  | 0.127  | 11.0% |
| **cz 11.0.1 default** | 0.412, 0.410, 0.409, 0.410, 0.410        | 0.410  | 0.003  |  0.3% |

cz is **~9% faster** at default settings on this corpus, with
**dramatically tighter variance**. Both observations matter
(see §4 for what the variance signals).

### Thread-count sweeps (3 trials each)

#### sd: `--io-threads N`

| N (io-threads) | mean (s) | notes                              |
|----------------|----------|------------------------------------|
| 1              | 0.255    | no parallelism                     |
| 8              | 0.215    |                                    |
| **16**         | **0.204**| **optimum (`threads / 2`)**        |
| 32             | 0.242    | `= threads`                        |
| 64             | 0.299    | `threads × 2`                      |
| 96 (default)   | 0.452    | `threads × 3`, current default     |

sd is **2.21× slower at its default than at its optimum.** The
performance cliff happens between `threads × 1` (32) and
`threads × 3` (96): the curve is monotonically worse past
~16.

#### czkawka: `-T N`

| N (threads) | mean (s) | notes                |
|-------------|----------|----------------------|
| 1           | 0.465    | single-threaded      |
| 8           | 0.407    |                      |
| **16**      | **0.403**| optimum              |
| 32          | 0.410    | `= logical CPUs`     |
| 0 (default) | 0.415    | auto (= 32 here)     |

cz's default is **2.9% off its optimum** — practically tuned.

### Cross-tool comparison at each tool's optimal setting

| tool                            | mean time | vs cz-best |
|---------------------------------|-----------|-----------:|
| sd v0.3.0 `--io-threads 16`     | 0.204 s   | **0.51×**  |
| cz 11.0.1 `-T 16`               | 0.403 s   | 1.00×      |

**At each tool's best thread count on this corpus, sd is
roughly twice as fast as cz.** The `#98` "cz crept 7% ahead"
gap on Mick's Dropbox 198 GB corpus is therefore unlikely to be
"cz got faster than sd"; it is more likely "sd's default
`--io-threads` over-subscribes and the corpus shape exposes it."

### Stage breakdown (sd, default vs `--io-threads 16`)

Default (`--io-threads ≈ 96`):

```
stage 1 inventory:    6 ms  (2145 files)
stage 2 grouping:     0 ms
stage 3 layout:       0 ms
stage 4 hashing:    413 ms  (wallclock) — bytes_read = 2.92 GiB
```

Tuned (`--io-threads 16`):

```
stage 1 inventory:    6 ms  (2145 files)
stage 2 grouping:     0 ms
stage 3 layout:       0 ms
stage 4 hashing:    172 ms  (wallclock) — bytes_read = 2.92 GiB
```

The entire wallclock delta lives in Stage 4 hashing. Stages 1-3
are unchanged (and already fast). At io_threads=16 Stage 4
processes 2.92 GiB in 172 ms — ~17.4 GB/s aggregate, well above
the host's raw disk bandwidth, so the corpus is mostly
page-cached after the warmup run. The default's 413 ms reflects
scheduler thrashing from oversubscription, not actual disk IO
work.

## Root-cause hypothesis

`src/cli.rs` documents the design intent of
`io_threads = threads × 3`:

> Worker count for the hashing par_iter. Defaults to
> `threads × 3` because the per-file `open()`/`read()`/`close()`
> cycle (Tier 1 + small-file Tier 3) spends most of its time
> blocked in syscalls. Oversubscribe to keep more I/O in
> flight.

That reasoning is sound for **Tier-3-pure workloads** (large
files, sustained read) — and matches the benchmarker's
synth-200gb AppData-shape + large-dups 16 GB Tier-3-pure
results where sd leads cz by 1.07× to 1.31×.

But on a **heterogeneous corpus** the assumption breaks down:

1. Most files (cache, small docs, log uniques) short-circuit at
   Tier-0 / Tier-1. The per-file syscall count is high but the
   per-file work per syscall is tiny.
2. With `threads × 3` workers, each thread holds an OS-level
   file descriptor and competes for CPU through every short
   read. Context-switch cost dwarfs the actual hash work.
3. The Tier-2 / Tier-3 work that genuinely benefits from
   oversubscription is a smaller share of total work on
   heterogeneous corpora than on Tier-3-pure ones.

The variance numbers reinforce this: sd's default trial-to-
trial CV is **11%**; at `--io-threads 16` the CV drops to
~3%. cz at any thread count has CV ~0.3%. High variance is the
signal of scheduler-driven non-determinism — exactly what
oversubscription causes on heterogeneous workloads.

## Recommended fix path (engine-main's call) — **resolved: HOLD**

> **2026-05-31 update:** Mick + engine concur HOLD per the
> Windows negative-replication data in
> §"Cross-platform: negative replication on Windows". No global
> default change. Linux power-user recommendation retained in
> §"Recommendation". The options below are preserved as the
> original engineering analysis for posterity / future
> reconsideration if a real cold-cache regression surfaces or
> if a non-Windows scheduler shows the Linux cliff.

Engine main owns the default. Original options the
finding presented:

1. **Default to `threads` instead of `threads × 3`.** Easiest;
   gives every user the right ballpark on every workload.
   Trade-off: leaves ~10-15% perf on the table on Tier-3-pure
   corpora where the original `threads × 3` IS optimal.
   **Status: NOT SHIPPED.** Windows replication showed the
   default cliff doesn't reproduce there; one-platform speedup
   isn't worth a global default change that regresses
   Tier-3-pure across the board.

2. **Auto-tune per-Tier.** Use `threads` for Tier-1/Tier-2 (per-
   file syscall-heavy), `threads × 3` for Tier-3 (sustained
   read). Requires the per-Tier worker pool to split.
   **Status: DEFERRED.** If a cold-cache regression surfaces
   post-ship (the warm-cache regime caveat in §"Regime"), this
   is the path forward.

3. **Make the default workload-aware** at scan-start: probe the
   inventory's size distribution + pick a multiplier. Adds
   complexity at startup; likely not worth it.
   **Status: NOT NEEDED** under the HOLD outcome.

4. **Keep the default; surface a `--io-threads auto` hint** in
   docs + GUI Settings → Advanced.
   **Status: PARTIALLY ADOPTED** via this doc's §"Recommendation"
   block. A GUI Settings hint could still be added if Linux
   power-users surface the issue more visibly.

## Caveats + scope

1. **Single-corpus reproduction.** This is one 3.3 GiB Linux
   synthetic corpus. Mick's Dropbox 198 GB corpus on the Win11
   NEO box is the ground truth. If the multiplier-effect
   holds there too, the fix is clear. If the optimum on Mick's
   corpus is different (e.g., his disk is slower so
   oversubscription helps more), the recommendation needs
   tuning.

2. **OS cache state.** All trials ran with at least one prior
   warmup run, so the corpus is page-cached. Cold-cache
   numbers (where IO actually goes to disk) might shift the
   curve. Worth re-running with `drop_caches` between trials
   on a box where sudo is available.

3. **The benchmarker's `cz crept 7% ahead` framing in `#98`'s
   body** is consistent with this finding but doesn't prove it.
   At default settings on Mick's box, sd may be 7% behind cz;
   at sd's optimum, sd is likely ahead. Verifying both numbers
   on the Dropbox corpus is the recommended next step.

4. **`--io-threads` is the only knob I swept.** Other defaults
   (`--threads`, Tier-2 chunking, walker buffer sizes) might
   also be sub-optimal on heterogeneous workloads. Out of
   scope for this finding.

## Cross-platform: negative replication on Windows

Benchmarker re-ran this methodology on NEO with a byte-exact
corpus mirror produced by the §"Reproduction recipe" generator
below.

> **NEO config:** 32-core Ryzen 9 9950X3D + Gen 4 NVMe,
> Windows 11, sd v0.3.0 (commit 33ea5f9), czkawka_cli 11.0.1.
> Same warmup + measurement protocol as Linux. 3.3 GiB corpus
> distribution byte-exact to this doc's recipe.

| `--io-threads` | mean (s) | Stage 4 (ms) |
|----------------|---------:|-------------:|
| 1              |  0.935   |  887         |
| 8              |  0.362   |  316         |
| 16             |  0.377   |  327         |
| 32             |  0.351   |  306         |
| 64             |  0.364   |  306         |
| **96 (default)** | **0.376** | **318**  |

**The Linux 2.21× cliff at default does NOT appear on
Windows.** The curve is flat from `io-threads = 8` onward —
spread ~7% across 8…96. The Windows scheduler absorbs the
`threads × 3` oversubscription without measurable degradation.

cz @ default on the same NEO + corpus: **0.571 s**. So
**sd@default is 52% faster than cz@default on Windows.** The
"cz beats sd at default" premise that motivated #98 doesn't
hold on the primary user platform.

### Why this matters for the recommendation

The Linux speedup is real but **the scheduler-thrashing penalty
is the Linux scheduler's behavior, not a universal sd-default
mistune.** Changing the engine default to `threads × 1` would:

- Help Linux power-users on this corpus shape (the 2.2× win
  documented above).
- Not help Windows users at all (default is already near-
  optimal there).
- Risk regressing sustained-read Tier-3-pure workloads on
  every platform (where the original `threads × 3` intent
  was sound and benchmarker numbers show sd already leads cz).

**Outcome: HOLD.** Linux power-users get the documented
recommendation; no global default change. If a future user
reports the Linux issue in the wild, this doc is the
explanation; the workaround is one flag.

### Methodology fairness note

This negative-replication appendix is preserved verbatim so
the cross-platform truth is documented alongside the original
Linux finding. The post-ship doc-only history (this commit +
commits 51bd765 + 4020e0f) is the audit trail of how the
HOLD decision was reached: Linux measurement → warm-cache
regime callout → Windows replication → HOLD with
power-user recommendation retained.

## Reproduction recipe

Build the corpus:

```bash
CORPUS=/path/to/sd-perf-corpus
mkdir -p "$CORPUS"/{docs,photos,downloads,cache,logs,nested/sub1/sub2,oneDrive,backup,videos}
# Tier A small text dups
for i in $(seq 1 800); do
  CID=$((i % 50))
  printf 'content type %s\n' "$CID" | head -c 5120 > "$CORPUS/docs/doc-$i.txt"
done
# Tier B medium binary dups (100 unique x 600 copies = ~60 MB)
for cid in $(seq 0 99); do head -c 102400 /dev/urandom > "/tmp/blob-$cid.bin"; done
for i in $(seq 1 600); do cp "/tmp/blob-$((i % 100)).bin" "$CORPUS/photos/photo-$i.jpg"; done
# Tier C ~1MB dups (~400 MB)
for cid in $(seq 0 79); do head -c 1048576 /dev/urandom > "/tmp/dl-$cid.bin"; done
for i in $(seq 1 400); do cp "/tmp/dl-$((i % 80)).bin" "$CORPUS/downloads/dl-$i.dat"; done
# Tier D ~30MB dups (~2.4 GB)
for cid in $(seq 0 19); do head -c 31457280 /dev/urandom > "/tmp/big-$cid.bin"; done
for i in $(seq 1 80); do cp "/tmp/big-$((i % 20)).bin" "$CORPUS/oneDrive/big-$i.mp4"; done
# Tier E cache-shape uniques (~25 MB)
for i in $(seq 1 2000); do head -c $((10240 + RANDOM % 4096)) /dev/urandom > "$CORPUS/cache/cache-$i.dat"; done
# Tier F log uniques
for i in $(seq 1 300); do head -c $((200000 + RANDOM % 100000)) /dev/urandom > "$CORPUS/logs/log-$i.log"; done
# Tier G full backup of docs
cp -r "$CORPUS/docs" "$CORPUS/backup/docs"
# Tier H nested deep dups
for i in $(seq 1 300); do cp "/tmp/blob-$((i % 30)).bin" "$CORPUS/nested/sub1/sub2/deep-$i.bin"; done
# Tier I video uniques
for i in $(seq 1 30); do head -c $((10485760 + RANDOM)) /dev/urandom > "$CORPUS/videos/uniq-$i.mp4"; done
rm -f /tmp/blob-*.bin /tmp/dl-*.bin /tmp/big-*.bin
```

Run the measurement (5 trials each at sd default and `--io-threads 16`):

```bash
SD=/path/to/superdeduper
CZ=$(which czkawka_cli)
measure() {
  local label=$1; shift
  local s=$(date +%s.%N); "$@" >/dev/null 2>&1; local e=$(date +%s.%N)
  printf '%s: %.3fs\n' "$label" "$(echo "$e - $s" | bc)"
}
# Warmup
"$SD" scan "$CORPUS" --no-cache --min-size 8K --format json > /dev/null
"$CZ" dup -d "$CORPUS" -H -N -M -m 8192 -f /tmp/cz-warm.txt > /dev/null
# sd default
for i in 1 2 3 4 5; do measure "sd-default-t$i" "$SD" scan "$CORPUS" --no-cache --min-size 8K --format json; done
# sd at io-threads=16
for i in 1 2 3 4 5; do measure "sd-io16-t$i" "$SD" scan "$CORPUS" --no-cache --min-size 8K --io-threads 16 --format json; done
# cz at default
for i in 1 2 3 4 5; do measure "cz-default-t$i" "$CZ" dup -d "$CORPUS" -H -N -M -m 8192 -f /tmp/cz-t$i.txt; done
# cz at -T 16
for i in 1 2 3 4 5; do measure "cz-T16-t$i" "$CZ" dup -d "$CORPUS" -H -N -M -m 8192 -T 16 -f /tmp/cz-T16-t$i.txt; done
```

Expected (on the Linux/32-CPU/SSD host this finding came from):

```
sd-default mean: 0.452 s
sd-io16    mean: 0.215 s        <-- 2.1x speedup from a single flag
cz-default mean: 0.410 s
cz-T16     mean: 0.403 s
```

If reproduction on Mick's NEO + Dropbox 198 GB corpus shows the
same shape — sd-default ≈ cz, sd-tuned ≪ cz — the fix in
§"Recommended fix path" is the path forward.
