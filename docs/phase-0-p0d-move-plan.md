# Phase 0 P0-D — module-move dry-run sketch

> **Author:** overflow, 2026-05-31. Pre-execution plan for P0-D, written before
> engine main's P0-C iface scaffolding lands on `main`. Mechanical execution
> against the published iface follows once P0-C ships.
>
> **Scope:** move 6 modules out of `src/leaderboard/` into the new
> `crates/superdeduper-bench-real/src/` crate; rewrite engine call sites to
> reach the moved code via `BenchExecutor` / `SubmissionExecutor` trait
> methods. Per `docs/phase-0-trait-extraction.md` §7 row P0-D.

## ⓘ Execution status (2026-05-31, P0-D Phase 1 shipped on fa0dd5f)

The execution split into TWO phases on contact with reality:

**P0-D Phase 1 — SHIPPED** (`overflow/p0d-execute` / `fa0dd5f`, target
v0.3.7):

- ✅ Iface placeholder types **replaced with real shapes** for 3 of 7:
  `ChallengePosition` (was byte-exact; +`Copy` + canonical doc),
  `DebugDedupDiff` (new) + `DebugDedupDiffReport` (added `diffs` field
  the scaffold dropped), `InstallKey` (`Vec<u8>` → `[u8; 32]` matching
  engine's existing alias).
- ✅ **`superdeduper-bench-real` crate scaffolded** as workspace
  member with `iface` + `blake3` + `chacha20` + `base64` + `serde`
  deps. `BenchReal` struct implements **both** traits with method
  bodies returning `Err(BenchError::Unavailable)` — validates the
  trait surface compiles against the workspace dep graph.
- ✅ **3 of 6 module moves** via `git mv`: `d7_probe`, `bench`,
  `bench_corpus`. All self-contained (no cross-module engine deps).
  Inner `#![cfg(feature = "telemetry")]` gates dropped (bench-real is
  itself telemetry-gated at the engine dep level; inner gates were
  redundant).
- ✅ Engine `src/leaderboard/mod.rs` `pub use` re-exports preserve all
  existing import paths (`crate::leaderboard::{bench, bench_corpus,
  d7_probe}` continues to resolve unchanged).
- ✅ Engine `Cargo.toml`: bench-iface + bench-real added as **optional
  deps gated behind `telemetry`**. `--no-default-features` builds pull
  neither.
- ✅ Verification: 603 tests pass across the workspace (529 engine +
  57 bench-real + 4 iface + 13 v31 goldens). Both feature combos
  clean.

**P0-D Phase 2 — DEFERRED** (post-launch resume):

- ⏳ 3 of 7 iface type-replacements: `BenchContext`, `BenchOutcome`,
  `SubmissionInputs`, `SubmitOutcome`. Blocked on
  `HardwareFingerprint` struct-def relocation (real `SubmissionInputs`
  has a `hardware: HardwareFingerprint` field; the type lives in
  `src/leaderboard/hardware.rs` which engine main is editing for SAT
  pass-through). Cross-module dep was the gap in this plan's §4.1 the
  dry-run missed.
- ⏳ 3 of 6 remaining module moves: `bench_run.rs`, `bench_client.rs`,
  `submission.rs`. The submission.rs three-way split (§4 below) and
  the `Cancelled` → `BenchError::Cancelled` migration at
  `bench_modal.rs:241` (§6.1) are part of this Phase 2 work.
- ⏳ Real trait method bodies on `BenchReal` (currently all
  `Err(Unavailable)`). Wires `run_bench` → `bench_run::run`,
  `debug_dedup_diff` → `bench_run::debug_dedup_diff`,
  `submit_recorded` → `submission::submit_recorded`.
- ⏳ Engine call-site rewrites (~50+ sites; full enumeration in §3
  below). The pub-use shims keep Phase 1 compiling without touching
  any call site; Phase 2 either keeps the shims or rewrites the call
  sites to executor.method() per engine's preference.

**Phase 2 resume gates** (must be answered before execute restart):

1. Engine main SAT pass-through on `hardware.rs` lands.
2. Decision: `HardwareFingerprint` struct-def location.
   Engine main 15:13 PST lean: **new types crate**
   (e.g., `crates/superdeduper-hardware-iface/`) that both engine and
   bench-iface depend on; engine keeps the platform-detection fn
   bodies in `hardware.rs`, just the struct-def + serde derive moves.
3. Decision: `BenchContext` lifetime. Engine main 15:13 PST lean:
   **convert to owned at iface boundary** (allocations on every call;
   the dyn-safety win is worth it for a once-per-bench-run call).
4. Decision: `SubmitOutcome` enum shape with rank-entry variants.
   Engine main 15:13 PST lean: **keep as enum** (iface-acceptable);
   re-evaluate specific variant if a transitive dep surfaces.

The rest of this doc is the dry-run sketch as originally written;
read §§3, 4, 5, 6, 8 against the **Phase 2 deferred** scope above.

## 1. Modules to move

Six files (≈ 7100 LOC total) leave the engine binary's `src/leaderboard/`
and land in `crates/superdeduper-bench-real/src/`:

| Source path                          | LOC  | Disposition |
|--------------------------------------|------|-------------|
| `src/leaderboard/bench_run.rs`       | 1528 | Move whole file. |
| `src/leaderboard/bench_client.rs`    | 1238 | Move whole file. |
| `src/leaderboard/bench.rs`           |  654 | Move whole file. |
| `src/leaderboard/bench_corpus.rs`    | 1275 | Move whole file. |
| `src/leaderboard/d7_probe.rs`        |  721 | Move whole file. |
| `src/leaderboard/submission.rs`      | 1662 | **Split** along types-vs-HTTP-vs-persistence line — see §4 below. |

Test fixtures inside each module (`#[cfg(test)] mod tests`) move with the
file. The tests rerun in the new crate; verification by `cargo test -p
superdeduper-bench-real` in P0-F.

## 2. Per-module public surface — what the iface trait surfaces vs what stays internal

The new crates compile against `superdeduper-bench-iface`. The only items
the engine binary needs to reach are exposed via the executor traits.
Everything else is `pub(crate)` inside `bench-real` so the iface ABI stays
narrow.

### 2.1 `bench_run` (1528 LOC)

| Item                              | Disposition |
|-----------------------------------|-------------|
| `pub struct BenchOutcome`         | Moves to iface (callers downstream use it). |
| `pub struct Cancelled`            | REMOVED — replaced by `BenchError::Cancelled` variant per docs/phase-0-trait-extraction.md §6. Migration: `bench_modal.rs:241` `Err(e).downcast_ref::<Cancelled>()` → `matches!(err, BenchError::Cancelled)`. |
| `pub fn run(...)`                 | Becomes `BenchExecutor::run_bench(&self, ...)` trait method. Body moves into the trait impl on the `-real` struct; the function itself is `pub(crate)` inside `bench-real`. |
| `pub struct DebugDedupDiff`       | Moves to iface (used by `bench debug-dedup-diff` CLI subcommand output). |
| `pub struct DebugDedupDiffReport` | Same. |
| `pub fn debug_dedup_diff(dir)`    | Becomes `BenchExecutor::debug_dedup_diff(&self, dir)`. |

Private helpers (`signal_dedup_ready`, `full_content_dedup`, `read_uncached`,
`flatten_single_subdir`, `evict_corpus_pages`) move with the file as
`pub(crate)` or `fn`.

### 2.2 `bench_client` (1238 LOC)

All ~35 protocol-math functions move INTO `bench-real`. None of them are
called from outside `bench_run` / `bench` / `bench_corpus` / `submission`
in current code (verified via grep: the only external callers are
`tests/v31_goldens.rs` — see §6.3 test-migration).

- All `pub fn`/`pub const` become `pub(crate)` inside the moved file.
- The single integration-test consumer (`tests/v31_goldens.rs`) moves to
  `crates/superdeduper-bench-real/tests/v31_goldens.rs` so it can reach
  the now-private API via `bench_real::` (or however the crate names its
  module roots).

### 2.3 `bench` (654 LOC)

Corpus-keys / leaf-hash / merkle-root / audit-path math. Same as
`bench_client`: all-internal to bench, no engine callers, `pub(crate)`
inside the moved file. `pub struct BenchContext<'a>` moves to iface (P0-C
will surface the type per docs/phase-0-trait-extraction.md §4.2).

External callers: `main.rs:759` uses `leaderboard::bench` inside
`run_make_bench_corpus`. That code itself is bench-corpus generator
plumbing and is moved (see §3 main.rs entry-point map).

### 2.4 `bench_corpus` (1275 LOC)

Corpus generator + Merkle-proof builder. Same pattern: all-internal,
`pub(crate)` inside the moved file. External callers:
- `main.rs:598` (`use ... bench_corpus as bc`) — inside the `bench` CLI
  subcommand handler. Migration: the handler itself moves (see §3 below)
  OR it stays in main.rs and reaches into bench-real via the executor for
  a `corpus_manifest()` / `plan_corpus()` trait method.

Recommendation: keep the CLI handler in `main.rs` (it's CLI argument
parsing + dispatch — engine-binary territory) and surface
`plan_corpus(spec) -> CorpusPlan`, `served_manifest(spec, seed, plan) ->
ServedManifest`, `write_corpus(dir, k_content, plan) -> u64`,
`build_bench_proof_from_dir(...)` as `BenchExecutor` methods. The result
types (`TierSpec`, `CorpusPlan`, `CorpusManifest`, `ServedManifest`,
`BenchProof`, `LeafLoc`, `SampleProof`) move to iface so the CLI handler
can name them.

### 2.5 `d7_probe` (721 LOC)

D7-A calibration + D7-B execution. Pure functions; all `pub fn` become
`pub(crate)` inside the moved file. Types `FileEntry`, `ProbeTarget`,
`ProbeResult` move to iface — the D7-C wire format will reference them.

External callers today: none in `src/` (D7 is wired only inside
`bench_run` and `submission` flows). Engine call-sites map: zero.

### 2.6 `submission` (1662 LOC) — the split

This is the only file that **splits**. Per
`docs/phase-0-trait-extraction.md` §5, types stay in iface; HTTP/wire
functions move to `-real`. After further inventory, there's a **third**
bucket — local-state persistence — that stays in engine. See §4 for the
full breakdown.

## 3. Engine call-site rewrite map

External call sites that need editing during P0-D. None are inside the
files that move (those are the moved files; their internal calls rewire
to `pub(crate)` neighbors automatically).

### 3.1 `src/main.rs` — 14 call sites

CLI command handlers. Each one needs the same shape change:

```rust
// Before:
use superdeduper::leaderboard::bench_run;
let outcome = bench_run::run(opts, ...);

// After:
use superdeduper::{Engine, BenchExecutor};  // or wherever P0-C surfaces it
let engine = Engine::current();
let outcome = engine.executor().run_bench(opts, ...)
    .map_err(anyhow::Error::from)?;
```

Per-line catalog (file:line — call to rewrite):

| Site             | Today's call                                        | Becomes |
|------------------|-----------------------------------------------------|---------|
| `main.rs:165`    | `use ... submission::{self, SubmitOutcome};`        | Keep `SubmitOutcome` import via iface re-export; drop `submission` direct use. |
| `main.rs:224`    | `submission::submit_recorded_payload(state, payload, built_with, server)` | `executor.submit_recorded_payload(state, payload, built_with, server)` returning `Result<SubmitOutcome, BenchError>`. |
| `main.rs:598`    | `use ... bench_corpus as bc;`                       | Drop direct `bench_corpus` use; reach via executor for `plan_corpus` etc. **OR** keep direct use IF this stays in CLI handler bucket — see §2.4. |
| `main.rs:703`    | `use ... bench_run;`                                | Drop import; call via executor below. |
| `main.rs:707`    | `bench_run::debug_dedup_diff(&dir)`                 | `executor.debug_dedup_diff(&dir)?` |
| `main.rs:751-759`| `run_make_bench_corpus` body using `bench` + `bench_corpus` | Body rewires to executor calls per §2.4. |
| `main.rs:874`    | `use ... {bench_run, install, oauth, submission};`  | Drop `bench_run` + `submission`; keep `install` + `oauth` (stay in engine). |
| `main.rs:947`    | `bench_run::run(opts, ...)`                         | `executor.run_bench(opts, ...)?` |
| `main.rs:987,1010` | `submission::SubmitOutcome::*` pattern match     | `SubmitOutcome::*` (via iface re-export); no body change. |
| `main.rs:1370`   | `use ... submission::{self, SubmitOutcome};`        | Same as :165. |
| `main.rs:1508`   | `submission::submit_recorded_payload(...)`          | Same as :224. |
| `main.rs:2411`   | `use ... submission::{...};`                        | Subset import; types from iface re-export. |
| `main.rs:2515`   | `submission::build_payload(&inputs, install_id)`    | `executor.build_payload(&inputs, install_id)?` (returns `Result<serde_json::Value, BenchError>` for ABI stability). |

### 3.2 `src/gui/widgets/bench_modal.rs` — 6 call sites

The GUI bench-me button worker. Per docs §6 known traps, the
`Cancelled.downcast_ref()` migration is the load-bearing edit.

| Site | Today | Becomes |
|------|-------|---------|
| L188 | `use ... {bench_run, install, registration, submission};` | Drop `bench_run` + `submission` direct use. |
| L228 | `bench_run::run(opts, ...)`                              | `executor.run_bench(opts, ...)` |
| **L241** | `Err(e) if e.downcast_ref::<bench_run::Cancelled>().is_some() =>` | `Err(BenchError::Cancelled) =>` (cleaner; explicit variant match). |
| L284, L308, L317 | `submission::SubmitOutcome::*` match arms | `SubmitOutcome::*` via iface re-export; no body change. |

### 3.3 `src/gui/app.rs` — 22 call sites (all `submission::*`)

The bulk are local-state persistence functions that STAY IN ENGINE per
§4 split — so most of these become no-ops in P0-D (the import path
doesn't change; just where the symbol lives).

For each line that calls a function in the §4 "stays in engine" bucket
(`peek_pending`, `store_pending`, `take_pending`, `store_last_outcome`,
`peek_last_outcome`, `clear_last_outcome`, `store_pending_submission_id`,
`peek_pending_submission_id`, `clear_pending_submission_id`,
`update_last_outcome_ranks`): **no edit required**. The function still
lives at `crate::leaderboard::submission::*`.

For lines calling functions in the §4 "moves to bench-real" bucket
(`submit`, `submit_recorded_payload`, `build_payload`, `archive_attempt`,
`flag_for_review`, `enqueue`):

| Site | Today | Becomes |
|------|-------|---------|
| `app.rs:1042` | `use crate::leaderboard::submission;` | Keep import (for the no-edit persistence calls). |
| `app.rs:1298` | `submission::submit(&state, &inputs)` | `executor.submit(&state, &inputs)?` |
| `app.rs:1302` | `submission::archive_attempt(&inputs, install_id, &outcome)` | `executor.archive_attempt(&inputs, install_id, &outcome)?` |
| `app.rs:1431` | `use crate::leaderboard::submission;` | Keep import. |

### 3.4 `src/gui/live.rs` — 6 call sites

| Site | Today | Becomes |
|------|-------|---------|
| L1770 | `use crate::leaderboard::submission::{...};` | Split: persistence symbols stay imported from `leaderboard::submission`; type symbols re-import from iface. |
| L1951, L1963, L1976 | `submission::build_payload(&inputs, install_id)` | `executor.build_payload(&inputs, install_id)?` |
| L1994 | `submission::store_pending(inputs)` | **No edit** (stays in engine per §4). |
| L1996 | `submission::clear_last_outcome()` | **No edit** (stays in engine per §4). |

### 3.5 `src/gui/resubmit.rs` — 2 call sites

| Site | Today | Becomes |
|------|-------|---------|
| L23  | `use crate::leaderboard::submission::SubmitOutcome;` | `use crate::SubmitOutcome;` (or wherever P0-C re-exports the iface type). |
| L111 | `submission::submit_recorded_payload(...)` | `executor.submit_recorded_payload(...)?` |

### 3.6 `src/gui/widgets/scan_complete_modal.rs` — 1 call site (type only)

| Site | Today | Becomes |
|------|-------|---------|
| L30  | `use crate::leaderboard::submission::SubmitOutcome;` | Re-import from iface. |

### 3.7 `src/gui/widgets/scan_history_panel.rs` — 2 sites (type only)

L410, L411: `crate::leaderboard::submission::SubmitOutcome` references.
Re-import from iface.

### 3.8 `src/gui/widgets/settings_modal.rs` — 1 site (type only)

L3076-3077: `crate::leaderboard::submission::SubmitOutcome` reference.
Re-import from iface.

### 3.9 `tests/v31_goldens.rs` — 2 import sites

L34, L35, L772, L773: the test crate directly imports protocol-math
functions from `bench_client` + `bench`. Two paths:

- (preferred) Move the test file to
  `crates/superdeduper-bench-real/tests/v31_goldens.rs` so it can name
  the now-`pub(crate)` symbols inside the moved module. Cleanest.
- (alternative) Add `#[doc(hidden)] pub use` re-exports from
  `bench-real`'s lib.rs for the specific functions the test needs.
  Leaks the surface; rejected.

Recommendation: **move the test**.

## 4. `submission.rs` split — three buckets, not two

`docs/phase-0-trait-extraction.md` §5 framed this as a 2-way split
(types → iface, HTTP → real). Closer inspection reveals a **third** bucket
of local-state persistence functions that should stay in engine:

### 4.1 → iface (types only)

```
pub struct SubmissionInputs       (L36)
pub struct CanonicalBench         (L76)
pub struct RunShape               (L96)
pub struct ResultSummary          (L172)
pub enum   SubmitOutcome          (L234)
pub struct RankEntry              (L272)

pub const ACTION_BYTES_KEY_DELETED_TO_RECYCLE      (L212)
pub const ACTION_BYTES_KEY_DELETED_PERMANENTLY     (L214)
pub const ACTION_BYTES_KEY_HARDLINK_REPLACED       (L216)
pub const FEATURE_BIT_CACHE                        (L220)
pub const FEATURE_BIT_FORMAT_AWARE                 (L221)
pub const FEATURE_BIT_FOLLOW_LINKS                 (L226)
pub const FEATURE_BIT_ALLOW_SYSTEM_PATHS           (L227)
pub const FEATURE_BIT_ALLOW_RECALL_ON_READ         (L228)
pub const FEATURE_BIT_REFERENCE_ROOTS              (L229)
pub const FEATURE_BIT_INCLUDE_GLOB                 (L230)
pub const FEATURE_BIT_EXCLUDE_GLOB                 (L231)
```

GUI + CLI both name these; iface exposes them.

### 4.2 → bench-real (HTTP + wire-format + outcome archival)

```
pub fn wire_schema_json()                              (L298)
pub fn build_payload(inputs, install_id)               (L335)
pub fn submit_recorded_payload(state, payload, ...)    (L450)
pub fn submit(state, inputs)                           (L532)
pub fn archive_attempt(inputs, install_id, outcome)    (L688)
pub fn flag_for_review(...)                            (L715)
pub fn enqueue(...)                                    (L938)
```

These produce the canonical wire bytes, call the HTTP endpoint, and
write the result/outcome to disk. They are the bench/submission path
and belong in `-real`. Each is surfaced via a `SubmissionExecutor` trait
method per docs §4.1.

### 4.3 → stays in engine (local in-memory + disk state)

```
pub fn queue_dir() -> std::io::Result<PathBuf>         (L656)
pub fn archive_dir() -> std::io::Result<PathBuf>       (L667)
pub fn review_dir() -> std::io::Result<PathBuf>        (L677)
pub fn store_pending(inputs)                           (L1044)
pub fn peek_pending() -> Option<SubmissionInputs>      (L1052)
pub fn take_pending() -> Option<SubmissionInputs>      (L1059)
pub fn store_last_outcome(o)                           (L1063)
pub fn peek_last_outcome() -> Option<SubmitOutcome>    (L1068)
pub fn clear_last_outcome()                            (L1072)
pub fn store_pending_submission_id(id)                 (L1082)
pub fn peek_pending_submission_id() -> Option<String>  (L1090)
pub fn clear_pending_submission_id()                   (L1097)
pub fn update_last_outcome_ranks(fresh)                (L1108)
```

Rationale: these are **process-local in-memory state** (`OnceLock<Mutex<...>>`-
backed) consumed by non-bench engine code (GUI cross-frame state, CLI
`scan-history resubmit` flow). They never reach the bench/HTTP path. Behind
an executor boundary they would be wrong — the local-state surface needs
to be reachable without an executor in scope.

**Mechanical split:** the file becomes a directory:

```
src/leaderboard/submission/
├── mod.rs          (re-exports + module-level docs)
├── types.rs        (iface re-export shim — `pub use crate::iface::submission::*`)
├── state.rs        (the §4.3 bucket — moves here from current submission.rs)
└── (deleted: the §4.2 functions move to crates/superdeduper-bench-real/src/submission.rs)
```

OR keep `submission.rs` as a single file containing only the §4.3 bucket
+ re-exports — simpler, no module-dir restructure. **Recommended.**

## 5. Engine call-site rewrite by import path

| Today's path                                              | After P0-D |
|-----------------------------------------------------------|------------|
| `crate::leaderboard::bench_run::*`                        | Reach via `executor` trait method; type re-exports from iface. |
| `crate::leaderboard::bench_client::*`                     | NOT reachable from engine (all-internal to bench). |
| `crate::leaderboard::bench::*`                            | Same — internal to bench. |
| `crate::leaderboard::bench_corpus::*`                     | Surface key fns via `BenchExecutor::corpus_*` methods; types via iface. |
| `crate::leaderboard::d7_probe::*`                         | NOT reachable from engine (all-internal to bench). |
| `crate::leaderboard::submission::{SubmissionInputs, RunShape, SubmitOutcome, RankEntry, CanonicalBench, ResultSummary, FEATURE_BIT_*, ACTION_BYTES_KEY_*}` | Re-import from iface (`crate::iface::submission::*` or top-level re-export). |
| `crate::leaderboard::submission::{store_pending, peek_pending, take_pending, store_last_outcome, peek_last_outcome, clear_last_outcome, store_pending_submission_id, peek_pending_submission_id, clear_pending_submission_id, update_last_outcome_ranks, queue_dir, archive_dir, review_dir}` | **No change** — these stay in engine. |
| `crate::leaderboard::submission::{build_payload, submit, submit_recorded_payload, wire_schema_json, archive_attempt, flag_for_review, enqueue}` | Reach via `SubmissionExecutor` trait method. |

## 6. Trap-spot specifics

Per `docs/phase-0-trait-extraction.md` §6:

### 6.1 `gui/widgets/bench_modal.rs:241` — `Cancelled` downcast

Current:
```rust
Err(e) if e.downcast_ref::<bench_run::Cancelled>().is_some() => {
    finish("Cancelled — nothing was submitted.".into(), false, false)
}
```

After P0-D:
```rust
Err(BenchError::Cancelled) => {
    finish("Cancelled — nothing was submitted.".into(), false, false)
}
```

`bench_run::Cancelled` ceases to exist as a separate type; the variant
absorbs it. The behavior is identical; the type-system gets cleaner.

### 6.2 `hmac_signer` cycle avoidance

Already handled by P0-B (superdeduper-hmac-signer leaf crate landed
v0.3.2). `bench-real` adds it as a Cargo dep; no further action in P0-D.

### 6.3 `crate::log_*` macros

`docs/phase-0-trait-extraction.md` §6 chose option (b) — extract
`superdeduper-log` leaf crate. If that hasn't shipped yet (P0-B
delivered hmac-signer; log may still be pending), P0-D needs to either:

- (preferred) ship the log leaf crate as part of P0-D's first commit
  so `bench-real` can depend on it.
- (alternative) inline-redeclare the macros in `bench-real`'s lib.rs.

Recommendation: **flag to engine main during the handoff** — confirm
whether the log leaf crate is already a workspace member or needs to
land in this slice.

### 6.4 `schemars` dep for iface JsonSchema types

`SubmissionInputs`, `RunShape`, etc. derive `JsonSchema`. iface needs
`schemars` as a dep. P0-C concern, not P0-D — flagging in case engine
hasn't wired it.

## 7. File-collision risk against engine main's parallel work

Per engine main's 13:38 PST commitment, they will not touch the
following files during today's P0-C + v0.3.1-cutover work:

- `src/gui/state.rs`
- `src/dedupe.rs`
- `src/gui/widgets/bench_modal.rs`
- `src/leaderboard/bench_run.rs` — EXCEPT one edit at `:350`
- `src/leaderboard/bench_client.rs` — EXCEPT edits at `:576` + `:588`

P0-D-touched files this branch will edit on execute:

- `crates/superdeduper-bench-real/Cargo.toml` (NEW)
- `crates/superdeduper-bench-real/src/lib.rs` (NEW)
- `crates/superdeduper-bench-real/src/{bench_run, bench_client, bench, bench_corpus, d7_probe, submission}.rs` (MOVED FROM `src/leaderboard/`)
- `Cargo.toml` (workspace `members`)
- `src/leaderboard/mod.rs` (drop module declarations for the moved files)
- `src/leaderboard/submission.rs` (shrunk to §4.3 bucket only)
- `src/main.rs` (call-site rewrites per §3.1)
- `src/gui/app.rs` (call-site rewrites per §3.3)
- `src/gui/live.rs` (call-site rewrites per §3.4)
- `src/gui/resubmit.rs` (call-site rewrites per §3.5)
- `src/gui/widgets/bench_modal.rs` (call-site rewrites per §3.2)
- `src/gui/widgets/scan_complete_modal.rs` (type import per §3.6)
- `src/gui/widgets/scan_history_panel.rs` (type import per §3.7)
- `src/gui/widgets/settings_modal.rs` (type import per §3.8)
- `tests/v31_goldens.rs` MOVES to `crates/superdeduper-bench-real/tests/v31_goldens.rs`

**Collision matrix:**

| Engine main touches | P0-D touches | Conflict? |
|---------------------|--------------|-----------|
| `src/leaderboard/bench_run.rs:350` | Whole file MOVES to bench-real | YES — the v0.3.1-cutover edit at :350 needs to land BEFORE P0-D execute, OR P0-D execute rebases against post-cutover bench_run.rs and applies the moved file with the cutover edit included. |
| `src/leaderboard/bench_client.rs:576, :588` | Whole file MOVES to bench-real | Same pattern as bench_run. |
| iface crate | nothing in iface | None. |

Resolution: **execute P0-D after both P0-C and v0.3.1-cutover have
landed on main.** The order engine main has in flight already (v0.3.1-
cutover, then P0-C) means P0-D's branch base will be post-cutover and
the moved files will carry the cutover edits. Zero conflict.

## 8. Order of operations on execute

When P0-C + v0.3.1-cutover have both landed on main:

1. Branch `overflow/p0d-bench-real` off `main`.
2. Add `crates/superdeduper-bench-real/Cargo.toml` (workspace member).
3. Add `crates/superdeduper-bench-real/src/lib.rs` declaring the 6
   moved modules.
4. `git mv` each of the 6 files from `src/leaderboard/` to
   `crates/superdeduper-bench-real/src/`. For `submission.rs`: copy + edit
   into the bench-real path (§4.2 bucket) and a shrunken version into
   `src/leaderboard/submission.rs` (§4.3 bucket).
5. Inside the moved files, change every `pub fn` / `pub const` /
   `pub struct` whose item is referenced ONLY from the bench cluster to
   `pub(crate)`. Items referenced from engine stay `pub` so the executor
   trait impl can reach them.
6. Implement `BenchExecutor` + `SubmissionExecutor` for the
   `BenchReal` (or similarly-named) struct in `bench-real`. Each method
   is a thin call into the now-`pub(crate)` helper.
7. Edit `Cargo.toml` workspace members list to include `bench-real`.
8. Edit `src/leaderboard/mod.rs` to drop the 5 fully-moved modules; keep
   the `submission` declaration (it now contains only §4.3).
9. Apply the §3.1–3.9 call-site rewrites.
10. `git mv tests/v31_goldens.rs crates/superdeduper-bench-real/tests/v31_goldens.rs`
    and rewrite its imports to the new module paths.
11. `cargo build --features telemetry` → expect clean.
12. `cargo test --features telemetry` → expect 584+ tests pass (the v31
    goldens move with the file, so the count stays steady).
13. Commit + push as `overflow/p0d-bench-real`; engine cherry-picks for
    v0.3.5 (per the §7 slice plan ship targets).

## 9. Known unknowns

- **`superdeduper-log` leaf crate status.** P0-A planned it; if not yet
  shipped, P0-D ships it as a prerequisite commit. Engine to confirm.
- **`schemars` in iface.** P0-C concern; not P0-D's to solve, but
  flagging.
- **Workspace-test discovery.** Tests inside `crates/superdeduper-bench-
  real/tests/` need `cargo test --workspace` to pick them up. Verify in
  P0-F.
- **Cargo feature propagation.** Engine binary's `telemetry` feature
  currently enables bench code via `#[cfg(feature = "telemetry")]` on
  `pub mod leaderboard`. After P0-D, the gate moves to a workspace dep:
  engine binary enables `superdeduper-bench-real` (or `-stub`) via
  Cargo feature. The `telemetry` feature name may merge with `bench-
  real` or stay as a thin wrapper; engine to decide in P0-E.

## 10. Estimated effort against the §7 slice plan

| Step | Effort | Source of cost |
|------|--------|----------------|
| 1–4 (branch + scaffold + git mv) | 30 min | Mechanical |
| 5 (pub → pub(crate) edits) | 1 hr | One pass per moved file (6 files) |
| 6 (trait impls on BenchReal) | 2 hr | Per-method thin wrappers; ~10 methods |
| 7–8 (Cargo.toml + mod.rs) | 15 min | One-liners |
| 9 (call-site rewrites) | 2 hr | 50+ sites across §3.1–3.9 |
| 10 (tests/v31_goldens.rs move) | 15 min | git mv + import path edit |
| 11–12 (build + test) | 30 min | First-build typo fixup |
| 13 (commit + push) | 15 min | Commit message + verification |

**Total: ~6.5 hr.** The original slice plan estimated 2 days for P0-D;
the lower number here reflects the precise inventory work done in §3
(no discovery cost during execute). If engine wants to halve again by
parallelizing engine call-site rewrites (§3.1–3.9) onto engine main,
overflow can ship steps 1–8 + 10 + 12 in ~3.5 hr while engine handles
step 9.

## 11. Recommendation for engine main

- **Confirm `superdeduper-log` leaf crate readiness** before P0-D
  execute (preferred: ship as part of P0-C if not already).
- **Tag the v0.3.1-cutover edits** (`bench_run.rs:350`, `bench_client.rs
  :576/:588`) in the commit message so the post-cutover P0-D execute
  doesn't accidentally re-revert them during the file move.
- **Surface the iface `BenchContext` type early in P0-C** — `bench.rs`
  has `pub struct BenchContext<'a>` today; P0-C moves the canonical
  definition to iface and `bench.rs` consumes it. P0-D depends on that
  type being stable before the moved files compile.

---

**Ready to execute on engine's `P0-C-landed-on-main` signal.** No
blockers from overflow's side. Plan above is complete to the line-of-
edit level; mechanical execution from here.
