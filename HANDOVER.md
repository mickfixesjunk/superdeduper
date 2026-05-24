# superdeduper agent — handover

> **What this file is.** A session bridge between the
> `superdeduper` engine agent on machine A and the same role
> running on machine B (and back). It carries (a) what happened
> in the most recent session, (b) the long-lived auto-memories
> that wouldn't otherwise transfer, and (c) the state the next
> session needs to pick up cleanly.
>
> **How it's used.** The agent's CLAUDE.md Session Start protocol
> should `cat ./HANDOVER.md` (if present) before posting intro or
> arming watchers, so the picked-up context is what the agent
> announces with. At session end the agent appends a new dated
> entry and commits the change so the file round-trips through
> the configs repo.
>
> **Append-only.** New sessions add a new `## Session: YYYY-MM-DD`
> block at the top. Older blocks are not edited (they're history,
> not state). The "Persistent memories" section at the bottom is
> the exception — that section is *replaced* when memories
> change.

---

## Session: 2026-05-23 (NEO / WSL — overnight push)

### Headline outcome

* **T2.1 (placeholder safety) phases 4–7 landed, then surface gap closed
  intra-session.** Initial Phase 7 was just a stderr Log line counter
  (insufficient per testdesign spec). Block I follow-up landed the full
  surface: JSON `skipped[]` array, `summary.placeholder_skipped`,
  schema bump `superdeduper.scan.v1 → v2`, `--placeholders-only` CLI
  flag, hint-suppression UX fix. -vvv trace events still deferred.
* **T2.3 (IFileOperation upgrade) landed and tested.** One real bug
  caught + fixed mid-bench (verbatim-prefix incompatible with
  SHCreateItemFromParsingName); now passes recycle smoke end-to-end.
* **T0.3 + T0.4 (walker-fallback for subdir scans) landed and verified.**
  Closes the silent file-loss bug AND closed most of the headline perf
  gap vs czkawka.
* **v3 baseline pinned** (true cold-cache numbers; prior baselines
  understated cold cost ~1.6x due to flood-flush insufficiency on the
  128 GB RAM box).
* **River5 R&D delivered a counter-intuitive verdict** (r7):
  river5-v15 and BLAKE3 are TIED on C:\\Windows. Tier 1 is the
  bottleneck (45% CPU share) and it's IO/syscall-bound (4.78 MB/s/thread,
  identical between algos) — NOT hash-bound. The "faster river5 variant"
  R&D direction is wrong; Tier-1 cost optimization (batched dir enum,
  mmap reads, handle pooling) is the real perf lever.
* **Dual-reclaimable metric** added (`unique_inodes` per group +
  `reclaimable_inode_bytes` in summary) — addresses the architectural
  asymmetry benchmarker surfaced at r5. Path-aware metric preserved
  for backwards compat.
* **v6 cache fixture** generated and committed to `tests/fixtures/v6_cache.sqlite`
  for criterion #9 (invalidation-rebuild verification).

### Where the engine is right now

* **Branch:** `feat/t2.1-placeholder-safety` (HEAD = **9f0b450**;
  ~18 commits ahead of `main`). T0 fix merged into main via rebase;
  T2.1 4–7 + T2.3 + polish + T2.3 fix + dual-report + v6 fixture +
  v6 invalidation test + Block I surface closure + UX hint fix +
  #5 symlink fix all stacked on top of that.
* **Origin:** well behind local. **Has NOT been pushed.** Mick
  needs to decide push timing and merge-to-main timing. Force-with-lease
  required because the rebase rewrote phase 1–3 commit SHAs.
* **Working tree:** clean. EXEs at `C:\Users\NeoMatrix\projects\mickfixesjunk\`
  are sha **9f0b450**. Plus `superdeduper-gui.exe` at the same sha.
  Sha-tagged copies preserved for v-over-v: 8375594, f1be77a, 0e1c3d7,
  c8edbe0, 7764797, 22d3feb, 28c8882, fa201ae, 723a110, 1083a25, 9f0b450.

Branch state (oldest → newest, post-rebase):

```
f211d6c T2.1: placeholder state + classify (WIP)           # rebased from e08a7aa
b5ba0d9 T2.1: OtherReparse also blocks content reads       # rebased from 94b9261
554fedf T2.1 phase 2: wire classify() into FileEntry        # rebased from d23985e
a539608 T2.1 phase 3: action-layer guards                   # rebased from 5e495a8
0e1c3d7 T2.1 phase 4: tier guards in hash worker            # new
b604da7 T2.1 phase 5: cache schema v6 → v7                  # new
c8edbe0 T2.1 phase 6: --allow-recall-on-read + ...          # new
7764797 T2.1 phase 7: placeholder skip counter (CLI + GUI)  # new
22d3feb T2.3: SHFileOperationW → IFileOperation             # new
536e162 clippy: derive Default on PlaceholderState          # new (polish)
28c8882 T2.3 fix: strip verbatim prefix before ...          # new (bug fix)
fa201ae T2.3 polish: strip_verbatim_prefix test + help text # new (polish)
723a110 Dual reclaimable metrics: unique_inodes + ...       # new (architectural)
f0d0de7 Add tests/fixtures/v6_cache.sqlite for criterion #9 # new (test fixture)
91af0a3 tests: cache v6→v7 invalidation (criterion #9)     # new (testrunner-authored)
cc0cda8 T2.1 Phase 7 surface: JSON skipped[] + summary ... # new (Block I main)
1083a25 Phase 7 UX: suppress recall-flag hint when 0 ...   # new (UX fix)
9f0b450 #5 fix: --follow-links re-stats + snake_case Display # new (post-validation fixes)
```

And on main (fast-forwarded from `fix/t0-walker-subdir-fallback`):
```
f1be77a T0.3+T0.4: walker fallback for subdirectory scans   # new
```

### What landed (in commit-time order)

| Block | What | Commit | Notes |
|-------|------|--------|-------|
| A | T0.3+T0.4 walker fallback for subdir scans | f1be77a (on main) | Fixes hardlink-alias data loss + 3.49x → 1.46x perf gap vs cz on C:\Windows |
| C | T2.1 phase 4: tier guards in hash worker | 0e1c3d7 | apply_tier_guards() at run_group entry |
| D | T2.1 phase 5: cache schema v6 → v7 | b604da7 | New reparse_tag column; warm-path now uses persisted tag in classify |
| E | T2.1 phase 6: CLI policy flags | c8edbe0 | --allow-recall-on-read (scan), --allow-destructive-on-deduped (dedupe) |
| F | T2.1 phase 7: placeholder skip counter | 7764797 | Per-state buckets in HashCounters, scan-finish Log line CLI+GUI |
| G | T2.3 IFileOperation upgrade | 22d3feb + 28c8882 fix + fa201ae polish | Modern COM API replaces SHFileOperationW; one real bug (verbatim prefix) caught and fixed mid-bench |
| H | River5 R&D | (queued bench r7) | Empirical comparison river5 vs blake3 on v3 corpus; variant work deferred to data-driven design session |

Plus: T0 fix merged into main (fast-forward from `fix/t0-walker-subdir-fallback`),
T2.1 branch rebased on top.

### Bench history (this session)

| Round | What | Outcome |
|-------|------|---------|
| sys32-r1 (pre-session) | recovered/integrity-caveats | findings only, not baseline |
| sys32-r2-broader-scope | first clean v2 attempt | 13.24s sd, 3.79s cz (1:3.49) — but warm-contaminated, see r5 |
| sys32-r3-postfix | T0 fix verify | 6.90s sd, 3.67s cz (1:1.88) — perf gap closed substantially, but still warm-contaminated |
| sys32-r4-tierguards-regression | T2.1 phase 4 regression | structural pass, but anomalous wall-clocks → exposed flood-flush insufficiency on 128 GB RAM |
| **sys32-r5-t21-7phase (v3 baseline)** | full T2.1 7-phase regression on new flush harness | **TRUE cold: 8.67s sd, 5.94s cz (1:1.46)** |
| sys32-r6-t23-regression | T2.3 IFileOperation regression | scan PASS byte-identical; recycle smoke FAIL → fix → PASS (~6 min cycle) |
| **sys32-r7-rd-river5-vs-blake3** | hash-algo R&D | **TIED (0.08% delta, noise). Tier 1 IO-bound, NOT hash-bound. Direction-changing data.** |

### Critical bench-harness change (benchmarker, mid-session)

Replaced 64 GB cache-flood with `NtSetSystemInformation(MemoryPurgeStandbyList)`
because 64 GB flood was insufficient on 128 GB RAM workstation. Standby list
retains pages past LRU pressure on quiet boxes. The new flush actually
empties standby — verified via canary read latencies. Saves ~45s/slot and
gives true cold numbers. r3 baseline numbers are NOT comparable to r5+ because
of this flush-method change; v3 (r5) is the new anchor.

### Decisions made this session

* **Sequencing: T0 fix ships BEFORE T2.1 phase 4–7.** Locked with design
  (Mick endorsed). Correctness fix (silent file loss) + perf fix
  (directive priority 1) outweigh shipping T2.1 first.
* **Phase 7 surface = stderr Log line** (not new EngineEvent variant).
  Minimum schema churn, uses existing GUI Log panel.
* **--allow-recall-on-read** does NOT extend to `OtherReparse` —
  asymmetric on purpose. Recall is known cloud-hydration trade-off
  user can opt into; unknown reparses might be HSM / PrjFS / etc.
* **--allow-destructive-on-deduped** unblocks ONLY `ReparseDedup`.
  Recall states stay blocked even with the flag — cloud safety is a
  different concern than FS-dedup transparency.
* **Block H: empirical bench before variant work.** v15 is already the
  deliberately-fast variant per its own docstring; speculative C
  variant work overnight isn't valuable until the bench tells us
  WHERE river5 actually loses to BLAKE3 on our workloads.
* **Reclaimable-bytes asymmetry is architectural, not a bug.** sd
  reports `reclaimable_path_bytes` (path-aware, what walker enumerated);
  cz reports `reclaimable_inode_bytes` (inode-aware, what dedup would
  actually free). Both are valid; they answer different questions.
  Dual-reporting is the right fix for v4; deferred to future session.
* **Cache-flush method:** NtSetSystemInformation. Documented; saves
  ~45s/slot; gives true cold numbers; standard going forward.
* **T2.1 surface gap (testdesign call):** the full Phase 7 surface
  (JSON `skipped[]` + summary.placeholder_skipped + schema v2 bump +
  `--placeholders-only` flag + verbose trace events) sits AHEAD of
  T0.5 inode-dedup in next session's queue. testdesign explicitly
  prioritised "finish the feature, don't move on." See follow-ups.
* **R&D pivot (r7 verdict):** Block H is closing as "hash R&D was
  wrong direction; pivot to Tier 1 syscall optimization." Concrete
  next levers — batched dir enum (FindFirstFileExW + LARGE_FETCH
  already in walker doc-comment), mmap'd 4K head reads, handle
  pooling.
* **Dual-reclaimable: shipped now, not deferred.** Was on the
  "next-session" list but landed in 723a110 since the schema
  additivity is small and the architectural-correctness value is
  high. Path-aware (`reclaimable_bytes`) preserved for backwards
  compat; new `reclaimable_inode_bytes` is what users want for "how
  much disk will I get back."

### Engine follow-ups (NOT done this session)

These are queued, can pick up whenever:

**Highest priority (next session):**

* **Reparse tag fetcher** (Windows-side `fetch_reparse_tag(&Path) ->
  Option<u32>` in `winapi_wrappers` via DeviceIoControl
  FSCTL_GET_REPARSE_POINT). Walker calls it when
  `attrs & FILE_ATTRIBUTE_REPARSE_POINT`, passes tag to `classify()`.
  Resolves the `OtherReparse(0)` issue sdd-testwin flagged AND
  enables tag-first cloud classification (Win11 25H2 testability
  fix). Roughly 50 lines + tests.
* **classify() tag-first cloud detection** — recognize cloud tag
  ranges (0x9000001A → `RecallOnOpen`, 0x9000101A →
  `RecallOnDataAccess`, other 0x900xxxx → conservative recall).
  Ships with the fetcher; together they unblock test52 fixtures
  exercising real classify variants without Cloud Filter API.
* **-vvv trace events** on every classify call returning
  non-`NotPlaceholder`. Spec leftover from Phase 7. Trivial wire-up.
* **EntrySkipped → JSON `skipped[]` propagation** (per testdesign
  9f0b450 verification). Walker emits an EntrySkipped event when a
  symlink target stat fails; current pipeline derives skipped[] from
  the FileEntry stream and so misses these. Fix: thread a
  `&mut Vec<SkippedFile>` collector into walker, push records on
  walker-error paths. Small, generalises to future walker-error
  cases. Slot after tag fetcher + classify update, before T0.5.

**After those:**

* **T2.1 Phase 7 surface gap closure** — **DONE this session** in
  Block I (cc0cda8 + 1083a25). Crossed off.
* **R&D pivot — Tier 1 syscall cost optimization** (Block H follow-up,
  re-scoped from "river5 fast variant"). Real perf-priority-1 work.
  Concrete levers per benchmarker's r7 analysis:
  - Batched directory enumeration with `FindFirstFileExW` +
    `FIND_FIRST_EX_LARGE_FETCH` (already in walker.rs doc-comment as
    "long-term target")
  - Memory-mapped 4K head reads instead of buffered ReadFile
  - File handle pooling across tier-1 worker batches
  - Speculative readahead: start hashing while inventory streams

**Lower priority (whenever):**

* **Dual-report reclaimable** — **DONE this session** (723a110).
  `reclaimable_inode_bytes` added to summary; `unique_inodes` per
  group. Crossed off the list.
* **Inode-dedup-before-hashing (T0.5?)** — sd currently hashes per-path,
  so hardlink-heavy corpora hash the same bytes N times. Group by
  file_ref before tier pipeline; expand to paths at output time.
  Estimated win: significant — would close most of the remaining
  1.46x perf gap vs cz on hardlink-heavy corpora like C:\Windows.
* **FSCTL_GET_REPARSE_POINT fetcher** — phase 5 added the storage
  column (`reparse_tag`) but cold-path producers leave it `None`.
  Wiring up the ioctl would let warm-path resume see the correct
  ReparseDedup vs OtherReparse classification without re-classifying
  conservatively as `OtherReparse(0)`.
* **--force-walker CLI flag** — escape hatch for users who want to
  bypass MFT path on whole-volume scans (currently MFT auto-engages
  there). Considered but rejected for tonight; logged for later.
* **IFileOperation batching** — currently one COM round-trip per
  recycle; bulk-dedupe could queue many DeleteItem calls before a
  single PerformOperations. Probably worth ~ms-per-file savings on
  large dedupe runs.
* **GUI toggle for --allow-recall-on-read** — Log line tells users
  the flag exists; an explicit GUI checkbox is the more discoverable
  surface. Pair with the placeholder counter for context.
* **stderr WARN path noise** — every WARN line shows `\?\C:\...`
  paths because of the verbatim prefix. Cosmetic; could strip in
  the tracing layout. Pre-existing, not new.

### Coordination state at session end

* All NEO Windows bench slots closed (sys32-r6 done; r7 queued).
* testdesign: notified that T2.1 branch is ready for their 12
  acceptance criteria runs; corpus rebuild done (357,580 files /
  51 GB / hardlinks intact).
* testrunner: asked which sha to test, answered `fa201ae` (current
  HEAD); should be running cargo criteria #1, #2, #3, #7, #9 now.
* sdd-testwin: no open WAITING ONs.
* design: endorsed sequencing; no open asks.
* river5: not pinged this session (no API change needed).
* czkawka: spec-corrected to `-m 1` (not `-m 0`); standing comparative
  binary unchanged from r2.

### Pick-up instructions for the next session (this side)

1. `git sweep` first — verify nothing new landed while away.
2. **Discuss with Mick:** push timing for the local branch
   (~10 commits ahead of origin), and merge-to-main timing for
   the T2.1 branch (it's ready when he is).
3. If r7 hasn't fired or its results haven't been processed,
   coordinate with benchmarker.
4. **Highest-leverage next-engine work:** reparse-tag fetcher +
   classify() tag-first cloud detection (Windows-side, ~50 lines for
   the fetcher via DeviceIoControl FSCTL_GET_REPARSE_POINT). Once it
   lands, sdd-testwin's test52 fixtures correctly classify (currently
   all show as `OtherReparse(0)` because reparse_tag is None at
   classify time). See "Engine follow-ups" for full scope.
5. **Second-highest:** Block H pivot — Tier 1 syscall cost
   optimization. `FindFirstFileExW` + `LARGE_FETCH` first; mmap'd
   reads second; benchmarker has the r7 baseline to compare against.
6. **Third (deferred from this session):** T0.5 inode-dedup-before-
   hashing. Real perf win for hardlink-heavy corpora. Touches
   `src/pipeline/grouping.rs` (group by file_ref before tier
   pipeline) + `src/pipeline/hash.rs` (expand to paths at output
   time, reuse `link_equivalent` plumbing). testdesign explicitly
   slotted this AFTER the surface closure.

For criterion #9 (cache schema v6→v7 invalidation): v6 fixture is
committed at `tests/fixtures/v6_cache.sqlite`. Test shape per the
testdesign-superdeduper.md post.

### Notes that bit me (subtle stuff for next-me)

* **Rebase needs `git -c user.name=... -c user.email=...`** — repo
  has no committer identity configured. `-c` survives across the
  rebase's per-commit replays. CLAUDE.md forbids `git config` (which
  would persist), per-command `-c` doesn't.
* **`tail -3 fileA fileB` on this box** does NOT take `-N` with
  multiple file args; says "unexpected argument '-3'". Use `cat`
  in a loop or `head -3 fileA; head -3 fileB`.
* **`EXIT=$? ; tail ...` is wrong** — captures tail's exit, not the
  command's. Use `cmd > log 2>&1; echo EXIT=$? >> log` correctly
  ordered.
* **Background builds via cargo zigbuild** can race on the artifact
  directory file lock if you kick CLI + GUI at the same time
  immediately after a switching branches. They serialize OK, just
  slow. Better to wait for one to finish or kick sequentially.
* **Cross-bench cache contamination on 128 GB RAM is real** — flood
  method does NOT work. NtSetSystemInformation is the correct API.
  This is THE single biggest harness improvement of the session.

---

## Session: 2026-05-22 (this box)

### Where the engine is right now

* **Branch:** `feat/t2.1-placeholder-safety` (4 commits ahead of `main`).
* **What's in flight:** T2.1 placeholder-safety feature. Phases 1-3 landed today; phases 4-7 pending. The branch is not yet merged; expect to keep landing phases on it until 7 lands, then PR + merge.

Recent commits (oldest → newest on the branch):

```
e08a7aa T2.1: placeholder state + classify (WIP)             # phase 1
94b9261 T2.1: OtherReparse also blocks content reads         # phase 1 follow-up (safer default)
d23985e T2.1 phase 2: wire classify() into the FileEntry stream
5e495a8 T2.1 phase 3: action-layer guards
```

### What's done vs what's next

| Phase | What | Status |
|-------|------|--------|
| 1 | `PlaceholderState` enum, `classify()`, `blocks_*()` predicates in `src/inventory/placeholder.rs` | **done** |
| 2 | Wire `classify()` at the three `FileEntry` producer sites (mft cold path, warm-path delta, walker fallback). Walker now also reads `attributes` from `MetadataExt::file_attributes()` on Windows. | **done** |
| 3 | `guard_destructive(path)` at every `action_*` entry in `src/dedupe.rs`. Defense in depth over planner-side filtering — protects single-file GUI flows the planner doesn't see. | **done** |
| 4 | **Tier guards.** Hash reads refuse on `blocks_content_read()`. Should land in `src/pipeline/hash.rs` — the per-file hash worker checks placeholder state before opening for read. The block list is broader than the destructive list (covers any reparse), so cloud-recall files don't get accidentally triggered during a scan. | **next** |
| 5 | Cache schema v6 → v7 + `test_fixtures` updated. Persist `placeholder` state across scans so the warm-path doesn't have to re-classify from `attributes` alone (and so we can store the actual `reparse_tag` when we backfill it). | pending |
| 6 | CLI flags for placeholder policy (allow recall-on-read? allow destructive on dedup'd?). Default = current conservative behavior. | pending |
| 7 | GUI counter showing placeholder buckets. Surfaces the "N files were skipped because they're placeholders" so users aren't confused why their dup count is lower than expected. | pending |

### Decisions made this session

* **Option A on test51 dup-bytes ratio (14% vs 30% target).** Both tools see the same workload; relative perf vs czkawka is what matters. Don't re-tune `_build.sh`. *testdesign-superdeduper.md, 01:46Z*.
* **`OtherReparse` blocks content reads** (not just destructive actions). Conservative default for reparse tags we haven't specifically identified. Already committed (`94b9261`).
* **Action-layer guards stay broad** — `blocks_destructive_action()` returns true for *every* non-`NotPlaceholder` state. We'll fine-tune later if a real use case shows up.

### Coordination state (snapshot at session end)

* Zero open `WAITING ON` tags across all 10 channels.
* **sdd-testwin**: native baseline now 50/50 PASS, matches WSL. Bench slot released. Has a phase-3 build of `superdeduper.exe` at `C:\Users\NeoMatrix\projects\mickfixesjunk` ready for next use.
* **testdesign**: bench-done landed on the test51 rebuild. Acked phases 2-3.
* **benchmarker**: idle. The post-perf-wave czkawka comparison bench (rebuilt 50GB test51, both tools default settings, R1 + R2 rows of `round2-matrix.py`) is the obvious next bench. Nobody's queued it.
* **river5**: idle. No open hash-lib coordination.
* **czkawka**: stood down.

### Pick-up instructions for the next session (this side)

1. Run `giga sweep` first — verify nothing new appeared while away.
2. If czkawka bench still hasn't run: post `bench-request` on `handoff.txt --as superdeduper` and clear yourself (it's your slot to use), then run `~/sd-bench-local/round2-matrix.py 'C:\sdd-bench-synthetic' --samples 5` or hand it to benchmarker.
3. Otherwise pick up T2.1 phase 4 (tier guards) — work happens in `src/pipeline/hash.rs`, the per-file hash worker is the right hook point.

### Watchdog notes (subtle things that bit me today)

* **sdd-testwin can miss its own bench-done post.** They completed in 2 min but didn't post until I poked them ~55 min later. Two takeaways: (1) trust their reported elapsed times in retrospect, not their "ETA" wallclock predictions; (2) consider a small `giga` feature (later — not now) that auto-asks for status if a bench-start isn't followed by a bench-done within 2× the estimate.
* **`giga init` pre-populates trust** as of v0.1.5 — so on NEO, the 8 trust prompts won't appear. But this only works if the configs maintainer runs `giga init` *before* spawning agents. Worth verifying.
* **Cargo.lock river5 path-source regression** keeps re-appearing. The pre-push hook catches it; just `git checkout -- Cargo.lock` before committing.

---

## Persistent memories (carry over across sessions / machines)

> These are distilled from `~/.claude/projects/-home-neo-projects-mickfixesjunk-superdeduper/memory/`
> as of session end. Replace this whole section when the underlying
> memories change.

### User / collaboration

* **Always build Windows EXEs.** After any superdeduper change, cross-compile via `cargo zigbuild --target x86_64-pc-windows-gnu --release` and drop the EXE at `C:\Users\NeoMatrix\projects\mickfixesjunk\superdeduper.exe` (or NEO's equivalent — `C:\Users\NeoMatrix\projects\mickfixesjunk\` since that's where Windows-side agents live there). Don't wait to be asked.
* **Commit-message strategy.** Full context goes in `~/sd-bench-local/superdeduper-commits/`. The GitHub commit message stays a dev-style one-liner. Detail in private, summary in public.
* **PII scrub style.** When scrubbing PII, never self-reference what's being removed (no "Cleanup: removed X" subjects — those leak the thing you scrubbed). Use `[redacted]` inline when keeping the rest of the message is useful. Pair message rewrites with `git filter-repo --replace-text` for blob scrubs.

### Project directives

* **Priority order:** (1) performance — being faster than czkawka in particular, (2) features, (3) everything else. `DIRECTIVES.md` in the engine repo is canonical.
* **Keep your identity.** When czkawka or another tool beats us on a metric, do not reflexively mimic their approach. Think through what's even better that fits superdeduper's identity (cross-platform, NTFS-native semantics, placeholder-safe).
* **Bench corpus sweet spot is ~50 GB.** The 100GB version that briefly existed was a mistake; reclaim space from there if more is needed.

### Empirical findings

* **river5-v15 vs BLAKE3 (small files):** BLAKE3 was 2.58× faster than river5-v15 on the AppData corpus despite river5's microbench claim. Microbench gains don't survive small-file workloads. (Stored in detail at `river5-v15-vs-blake3-bench.md`.)

### Test corpus

* **Location:** `/mnt/c/sdd-tests/` on this box. NEO doesn't have the corpus locally — it's ~50 GB and we said unnetworked. If a NEO session needs to bench, either replicate the corpus or use a smaller synthetic set.
* **Workflow:** vault+reset for tests 1-50 (functional); `_build.sh` for test51 (bench). Latest test51 rebuild is the 14% dup-bytes corpus (Option A).

### Comms / infrastructure (post-detour)

* **giga-harness exists** as of today. Replaces the old ad-hoc `watch-channel.sh` / `watch-channel.ps1` watchers and the manual `start-agents.ps1`. Agents post via `giga post`, watch via `giga watch`, see channel state via `giga sweep`. Bench-coordination convention unchanged.
* **Repos:** `giga-harness` (public) at github.com/mickfixesjunk/giga-harness. `giga-harness-configs` (private) — owned by a different agent now, not me.

---

## Round-trip protocol

* End-of-session: this file gets a new `## Session: YYYY-MM-DD` block at the top, the "Persistent memories" section is updated if anything changed, then it's committed to the configs repo.
* NEO's superdeduper agent reads this file at session start (per CLAUDE.md Session Start protocol — needs the configs maintainer to add `cat ./HANDOVER.md 2>/dev/null` or equivalent to step 0).
* NEO appends their own session block when they finish, commits, pushes. This box pulls next time.
* Conflict resolution: only one machine writes per session window. If both somehow append concurrently, merge by chronological order — newer block at top.
