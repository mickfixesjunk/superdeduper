# Phase 0 — BenchExecutor trait extraction (P0-A scope)

> **Status:** scope doc, pre-implementation. Drafted 2026-05-31 per Mick GO
> (`design-superdeduper.md` 09:25 PST) on infosec's closed-source/binary-lib
> refactor spec (`design-infosec.md` 08:05 PDT, §7 Phase 0).
>
> **What this is:** workspace + trait surface + cut-line inventory for Phase 0
> of the bench/anti-cheat trait extraction. Phase 0 lives entirely in the
> public engine repo — Phase 1+ (private repo move) is deferred to a future
> cert-residual-telemetry-driven greenlight.
>
> **What this is NOT:** a security claim. Closing the source is a *cost
> amplifier* on attacker bootstrap, not a primary defense. V3.1, D7, SCoC,
> plausibility caps, and Phase B floor remain the cryptographic gates.

---

## 1. Goals

- Extract the leaderboard/anti-cheat module behind a `BenchExecutor` trait.
- Validate the cut-line in code so a future Phase 1 (private-repo move) is a
  Cargo.toml dependency swap, not another structural pass.
- Preserve current behavior end-to-end. All shipping tests pass under default
  features.
- Stay reversible: if Phase 0 surfaces a structural problem, the workspace
  re-flatten is a single PR.

## 2. Open question resolutions

| Q  | Question                                | Resolution                                                              |
| -- | --------------------------------------- | ----------------------------------------------------------------------- |
| Q1 | Separate iface crate?                   | YES (design GO). One-directional dep graph: engine + stub + real → iface. |
| Q2 | Public error type?                      | Structured `BenchError` enum (see §4.4). `anyhow` stays inside `-real` impl; surface a stable enum at the trait boundary. |
| Q3 | Async-compatible trait?                 | NO (design GO). Sync-only for v1. Async wrapper layered later if needed. |
| Q4 | Non-bench code that crosses the line?   | Submission types (`SubmitOutcome` etc.) STAY in iface — used by GUI; only the wire/HTTP/crypto methods move behind the trait. `hmac_signer` STAYS in engine — used by non-bench paths (action_submission). See §5 inventory. |

## 3. Workspace layout

```
superdeduper/                     # root crate (engine binary + libraries)
├── Cargo.toml                    # [workspace] members = [".", "crates/*"]
│                                 # [features] default = ["bench-real"]; opt-in = "bench-stub"
├── src/                          # engine binary + GUI + scan / pipeline / etc.
│                                 #   imports types + trait from crates/superdeduper-bench-iface
│                                 #   calls into selected impl through trait methods
├── crates/
│   ├── superdeduper-bench-iface/ # public types + BenchExecutor trait
│   │   ├── Cargo.toml
│   │   └── src/lib.rs            # SubmissionInputs, SubmitOutcome, BenchOutcome,
│   │                             # ChallengePosition, BenchError, BenchExecutor
│   ├── superdeduper-bench-real/  # current implementation (default)
│   │   ├── Cargo.toml            # depends on superdeduper-bench-iface
│   │   └── src/lib.rs            # impl BenchExecutor with current bench_run / bench_client / submission HTTP / d7_probe internals
│   └── superdeduper-bench-stub/  # Err(Unavailable) returns (--no-default-features)
│       ├── Cargo.toml            # depends on superdeduper-bench-iface
│       └── src/lib.rs            # impl BenchExecutor with Err(BenchError::Unavailable) per method
```

Dependency graph:

```
engine ──→ bench-iface
       ╲
        ╲ (cfg-selected via feature flag)
         ╲→ bench-real ──→ bench-iface
         ╲
          → bench-stub ──→ bench-iface
```

Only one of `bench-real` / `bench-stub` compiles into the engine binary; the
selection is via mutually-exclusive features in the root `Cargo.toml`.

## 4. iface surface

### 4.1 `BenchExecutor` trait (sync)

```rust
pub trait BenchExecutor: Send + Sync {
    /// Run the canonical bench-me loop: POST /bench/start, download corpus,
    /// dedup, answer challenges, submit. Returns the outcome the CLI / GUI
    /// surfaces to the user.
    fn run_bench(
        &self,
        ctx: BenchContext,
        progress: &mut dyn FnMut(&str),
        cancel: &dyn Fn() -> bool,
    ) -> Result<BenchOutcome, BenchError>;

    /// Submit a recorded scan payload (non-bench submission path).
    fn submit_recorded(
        &self,
        inputs: SubmissionInputs,
        install_id: &str,
        install_key: &InstallKey,
    ) -> Result<SubmitOutcome, BenchError>;

    /// Diagnostic helper used by `sd debug dedup-diff`.
    fn debug_dedup_diff(
        &self,
        corpus_dir: &std::path::Path,
    ) -> Result<DebugDedupDiffReport, BenchError>;
}
```

### 4.2 Public types that LIVE in iface (re-export from engine)

| Type                         | Source today                          | Notes                                                       |
| ---------------------------- | ------------------------------------- | ----------------------------------------------------------- |
| `SubmissionInputs`           | `leaderboard::submission`             | Built by engine, consumed by `submit_recorded`              |
| `SubmitOutcome`              | `leaderboard::submission`             | Returned by `submit_recorded`; consumed by GUI (~5 callsites) |
| `BenchOutcome`               | `leaderboard::bench_run`              | Returned by `run_bench`; consumed by CLI + GUI bench-modal  |
| `ChallengePosition`          | `leaderboard::bench_client`           | Wire type; needed by both engine + real                     |
| `ChallengeAnswer`            | `leaderboard::bench_client`           | Wire type                                                   |
| `RunShape`                   | `leaderboard::submission`             | Wire type; built by payload_meta                            |
| `CanonicalBench`             | `leaderboard::submission`             | Wire type                                                   |
| `ResultSummary`              | `leaderboard::submission`             | Wire type                                                   |
| `RankEntry`                  | `leaderboard::submission`             | Wire type                                                   |
| `DebugDedupDiffReport`       | `leaderboard::bench_run`              | Diagnostic                                                  |
| `BenchContext` (NEW)         | —                                     | Bundle (base_url, install_state, corpus_version, tier, lane, workdir) to keep `run_bench` signature stable |
| `InstallKey` (re-export)     | `leaderboard::install::InstallKey`    | Already a small type; iface re-exports                      |

### 4.3 Functions / impls that MOVE INTO `bench-real`

These move out of the engine binary's `src/leaderboard/*.rs` and into
`crates/superdeduper-bench-real/src/`. Reachable from engine ONLY via trait
methods on the active `BenchExecutor`.

| Source file                          | What moves                                                    |
| ------------------------------------ | ------------------------------------------------------------- |
| `src/leaderboard/bench_run.rs`       | `fn run(...)`, `fn signal_dedup_ready(...)`, `fn full_content_dedup(...)`, `fn read_uncached(...)`, `fn flatten_single_subdir(...)`, `fn evict_corpus_pages(...)` |
| `src/leaderboard/bench_client.rs`    | All `result_digest_*` / `challenge_hash_*` / `rep_hash_v3_1` / `result_digest_v3_1` / `per_file_key_v3` / `mutate_bytes_v3` / `keystream_at_v3` / `compute_rep_hashes_v3_1` / `to_canonical_bench_v3` / `file_raw_hash` / `answer_challenge_from_dir*` |
| `src/leaderboard/submission.rs`      | `fn submit_recorded_payload(...)`, `fn build_payload(...)`, `fn wire_schema_json()` — the HTTP + canonical wire-format functions. **Type definitions stay in iface.** |
| `src/leaderboard/bench.rs`           | `corpus_keys`, `content_bytes_at`, `leaf_hash`, `node_hash`, `merkle_root`, `audit_path`, `root_from_path`, `file_leaves`, `root_base64` — full module |
| `src/leaderboard/bench_corpus.rs`    | Full module (corpus generator + Merkle proof) |
| `src/leaderboard/d7_probe.rs`        | Full module (probe-offset derivation + execution; D7-A + D7-B shipped v0.2.77/.78) |

### 4.4 `BenchError` enum

```rust
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// Stub crate (or future closed-source crate not available on this build).
    #[error("bench not available in this build (compiled with --features bench-stub)")]
    Unavailable,

    /// Network / transport / HTTP errors. Captures ureq, IO, parse, etc.
    #[error("bench transport: {0}")]
    Transport(String),

    /// Server returned an error response (4xx / 5xx).
    #[error("bench server error: {status} {reason}")]
    ServerError { status: u16, reason: String },

    /// Bench was cancelled by the caller (matches current `Cancelled` token).
    #[error("bench cancelled")]
    Cancelled,

    /// Local I/O error during corpus download / dedup.
    #[error("bench I/O: {0}")]
    Io(String),

    /// Catch-all for unexpected internal errors. Carries the formatted anyhow chain.
    #[error("bench failed: {0}")]
    Internal(String),
}
```

`anyhow::Result` stays as the internal type inside `bench-real`; the trait
boundary always returns `Result<T, BenchError>`. Conversion happens at the
trait-method exit. This gives stable iface ABI while preserving the
current rich error-chain UX inside the impl crate.

## 5. Non-bench crossings — Q4 inventory

Files that have one foot in bench code and one foot in non-bench code:

| File                                  | Disposition                                                                            |
| ------------------------------------- | -------------------------------------------------------------------------------------- |
| `src/leaderboard/hmac_signer.rs`      | STAYS in engine. Used by `action_submission`, `pending_actions`, etc. Bench code in `-real` re-imports from engine via a `pub use` or duplicates the small (~6 KB) module. Decision in P0-C: prefer `pub use` to avoid duplication. |
| `src/leaderboard/install.rs`          | STAYS in engine. Install state is owned by the engine, not the bench. `-real` receives `InstallKey` by reference via `BenchContext`. |
| `src/leaderboard/submission.rs`       | TYPES stay in iface (GUI consumers). HTTP/wire functions move into `-real`. The file splits along type-vs-impl lines. |
| `src/leaderboard/oauth.rs`            | STAYS in engine. OAuth flow is non-bench (used by ranked-lane gate; predates bench). |
| `src/leaderboard/captcha.rs`          | STAYS in engine. Not part of the bench wire path. |
| `src/leaderboard/registration.rs`     | STAYS in engine. `/install/register` is a separate flow from bench. |
| `src/leaderboard/account_*`           | STAYS in engine. Account management endpoints. |
| `src/leaderboard/hardware.rs`         | STAYS in engine. Hardware fingerprint is built by engine before passing into `BenchContext`. |
| `src/leaderboard/predicates.rs`       | STAYS in engine. Achievement predicates are post-scan, non-bench. |
| `src/leaderboard/catalog.rs`          | STAYS in engine. Achievement catalog wire types. |
| `src/leaderboard/payload_meta.rs`     | STAYS in engine. Builds `RunShape` from scan results; the type lives in iface so this code consumes the iface type. |
| `src/leaderboard/pending_actions.rs`  | STAYS in engine. Queue management for resubmit-pending flow. |
| `src/leaderboard/ranks_poll.rs`       | STAYS in engine. Polls /ranks for resubmit-pending; not on bench path. |
| `src/leaderboard/action_submission.rs`| STAYS in engine. Non-bench /action/submit; HMAC-signed but separate from /bench/submit. |
| `src/leaderboard/vanity_slug.rs`      | STAYS in engine. URL slug for profile pages. |
| `src/leaderboard/account_display_name.rs` | STAYS in engine. Nickname management. |

Result: **6 modules move to `-real`** (bench_run, bench_client, submission [partial — HTTP only], bench, bench_corpus, d7_probe). **17 modules stay in engine.** Sub-file split required only for `submission.rs` (types → iface, HTTP → real).

## 6. Risks / known traps

- **`hmac_signer` duplication**: if `-real` `pub use crate::hmac_signer` via a path back into engine, the dependency cycle is rejected. Resolution: extract `hmac_signer.rs` into a small leaf crate `superdeduper-hmac-signer` that both engine + `-real` depend on. Adds a 4th crate but cleanest. **P0-A decision: yes, extract to leaf crate.** Same pattern for `install::InstallKey` if it depends on hmac_signer (it does — for sign / verify).
- **`crate::log_info!` / `crate::log_*` macros**: bench code calls these for structured logging. `-real` needs access to the same macros. Options: (a) re-define them locally with the same shape, (b) extract logging into another leaf crate. **P0-A decision: (b) — `superdeduper-log` leaf crate.** Keeps log line format consistent.
- **`river5` dep**: bench_run.rs uses `river5` for parallel I/O. Both engine and `-real` need it; just declare in `-real`'s Cargo.toml.
- **`anyhow::Result` callsites in engine code that called bench functions**: every call to `bench_run::run` etc. moves to `executor.run_bench(...)` returning `Result<_, BenchError>`. Engine call sites need a small `.map_err(anyhow::Error::from)` to bridge back to anyhow for legacy paths.
- **GUI `bench_modal.rs:241` checks `e.downcast_ref::<bench_run::Cancelled>()`**: under the new trait this becomes `matches!(err, BenchError::Cancelled)`. Cleaner; just need the explicit migration.
- **Schema codegen (`schemars`)**: types annotated `#[derive(JsonSchema)]` need the macro available in iface. Add `schemars` as iface dep.

## 7. Slice plan (recap from intake post)

| Slice  | Scope                                                | Estimate | Ship target  |
| ------ | ---------------------------------------------------- | -------- | ------------ |
| P0-A   | This doc + open-question resolutions                 | 0.5d     | (this commit; v0.3.1) |
| P0-B   | Workspace skeleton + leaf crates (hmac-signer, log)  | 1d       | v0.3.2       |
| P0-C   | iface crate + types + trait signature                | 1d       | v0.3.3       |
| P0-D   | -real impl (move 6 modules into the crate)           | 2d       | v0.3.4       |
| P0-E   | -stub impl + feature gating                          | 1d       | v0.3.5       |
| P0-F   | Tests + smoke + docs                                 | 1d       | v0.3.6 (P0 done) |

Total ~6.5 days. Auto-promote per the standing rule (refactor; no schema / wire change). Each slice is independently shippable + verifiable.

## 8. Out of scope for P0

- Moving `-real` to a private repo (Phase 1+).
- Distribution as a binary `.rlib` / `.so` (M2 / M3 in infosec spec).
- Anti-debug / integrity checks inside the closed binary (Phase 4).
- Any wire-format or protocol changes (D7-C, v0.3.1 async-finalize remain
  on their own tracks).

## 9. Validation checklist for P0-F

- `cargo build` (default features) succeeds.
- `cargo build --no-default-features --features bench-stub` succeeds.
- `cargo test --features telemetry` passes 581+ lib tests under `-real`.
- `cargo run -- bench-me ...` works on dev under `-real` (smoke against
  `corpus-v3-quick`; submission should be accepted; cross-stack lock still
  validates).
- `cargo run --no-default-features --features bench-stub -- bench-me ...`
  returns a clean "bench not available in this build" message; no panic.
- Linux + Windows + ARM cross-build paths all succeed (existing
  `cross-build-drop.sh`).
- Engine version bumps to v0.3.6 once P0-F lands. No protocol_version
  change; engine still speaks `v3.1-mutate` for `corpus-v3-*`.

---

**Next:** P0-B — workspace skeleton + leaf crates. ETA today daylight if no
design pushback on this doc.
