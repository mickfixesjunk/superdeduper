# superdeduper-bench-iface — AGENTS guide

## Purpose

Phase 0 leaf crate that defines the trait/type boundary between the
engine binary and the future `superdeduper-bench-real` (default impl) /
`superdeduper-bench-stub` (Err(Unavailable) impl) crates. The cut-line
exists so anti-cheat-sensitive bench code can move to a closed-source
repo without forcing the engine binary to re-import its own internals.

This crate is pure-types + traits. No I/O, no engine-internal
dependencies — only `serde`, `serde_json`, `thiserror`, and an optional
`schemars` derive gated behind a `telemetry` feature. Both the engine
and `superdeduper-bench-real` depend on it; the engine re-exports many
of the wire-struct types via `pub use` for back-compat with old call
sites (`leaderboard::hardware`, `leaderboard::submission`).

The crate started life as a P0-C scaffold with placeholder types; over
P0-D Phase 1, Phase 2-A, Phase 2-B, and v0.3.21/v0.3.25 each wave of
placeholders has been replaced with the engine's real shapes so the
bench-real crate can build payloads end-to-end.

## Files

### `Cargo.toml`
Manifest. Declares `default = []` features and one optional dep
(`schemars`) gated behind the `telemetry` feature. The engine forwards
its own `telemetry` feature here so the `schema/submit.schema.json`
regen guard keeps working after the wire types moved.

### `src/lib.rs`
Single-module crate. Defines the entire iface surface:

Public types (wire / value types):
- `BenchContext` — inputs the bench-flow executor needs (install creds,
  server URL, corpus_version, tier, workroot, fresh flag, lane).
- `BenchOutcome` — result of a bench-flow run (run id, scan counts,
  dedupe_secs, result_digest, cold_enforced, optional `submit`
  outcome + assembled `SubmissionInputs`).
- `SubmissionInputs` — engine-canonical /submit body shape
  (client_version + hardware + run_shape + result_summary + run_uuid +
  optional scan_id, bench, lane).
- `RankEntry` — one leaderboard rank row inside `SubmitOutcome::Accepted.ranks`.
- `SubmitOutcome` — enum: `Accepted | DuplicateNoChange | Rejected |
  Transient | FlaggedForReview`.
- `ChallengePosition` / `ChallengeAnswer` — T-BENCH-ME challenge wire
  structs.
- `DebugDedupDiff` / `DebugDedupDiffReport` — diagnostic shapes for
  `sd debug dedup-diff`.
- `InstallKey(pub [u8; 32])` — HMAC install-key newtype.
- `HardwareFingerprint` — submission wire struct describing the client
  machine. `#[cfg_attr(feature = "telemetry", derive(schemars::JsonSchema))]`.
- `CanonicalBench` — T-BENCH-ME top-level submission block.
- `RunShape` / `ResultSummary` — backend-schema-shaped submission blocks.
  Both `#[cfg_attr(feature = "telemetry", derive(schemars::JsonSchema))]`.
- `BenchError` — `thiserror` enum: `Unavailable | Network | Hmac |
  ServerRejected | CorpusIo | Cancelled | Internal`.
- `BenchServices<'a>` — bundles the 4 closures (`progress`, `cancel`,
  optional `submit_fn`, `hardware_detect`) bench-real needs at call
  time. Single shared `'a` lifetime.

Public traits:
- `BenchExecutor: Send + Sync` — `run_bench(ctx, services) -> Result<BenchOutcome, BenchError>` +
  `debug_dedup_diff(corpus_dir) -> Result<DebugDedupDiffReport, BenchError>`.
- `SubmissionExecutor: Send + Sync` — `submit_recorded(inputs, server_url, install_id, install_key)`.

Tests:
- `ScaffoldStub` impls both traits returning `Err(Unavailable)`.
- `bench_executor_is_dyn_safe` / `submission_executor_is_dyn_safe`
  assert dyn-compatibility (call sites use `Box<dyn BenchExecutor>`).
- `stub_returns_unavailable` exercises the stub end-to-end via
  `BenchServices`.
- `bench_error_renders_human_readable_messages` checks `thiserror`
  Display output.

Who calls this:
- `crates/superdeduper-bench-real/src/{lib.rs, bench_run.rs, bench_client.rs, submission_http.rs}`
- Engine: `src/main.rs`, `src/leaderboard/{hardware.rs, submission.rs, payload_meta.rs}`,
  `src/gui/widgets/bench_modal.rs`

Feature gates:
- `telemetry` — `dep:schemars`; enables `JsonSchema` derives on
  `HardwareFingerprint`, `RunShape`, `ResultSummary`. Default off.

## Invariants / Gotchas

- **Wire byte-exactness**: `HardwareFingerprint`, `RunShape`,
  `ResultSummary`, `CanonicalBench`, `ChallengePosition`,
  `ChallengeAnswer`, `RankEntry` are wire-shaped. The backend's Zod
  schema is `additionalProperties: false` for `hardware`. Adding,
  removing, or renaming fields without coordinating the backend schema
  bump WILL silently break submissions or fail server-side validation.
  `actions_taken_summary` map keys are LOCKED to web's lifetime-audit
  strings (see comment at `ResultSummary.actions_taken_summary`).
- **`#[serde(default)]` on `HardwareFingerprint.is_dev_drive`** is
  load-bearing — keeps old engine submissions readable (web sees them
  as `false`). Do not remove without a coordinated backend rev.
- **`#[serde(skip_serializing_if = ...)]`** on optional `RunShape` and
  `ResultSummary` fields is load-bearing for the `additionalProperties:
  false` schema gate — emitting an explicit `null` would be rejected
  for fields the schema doesn't enumerate as nullable.
- **`InstallKey` is `[u8; 32]`** not `Vec<u8>`. Matches the engine's
  `leaderboard::install::InstallKey` type alias exactly.
- **`BenchServices` lifetime**: all four fields share `'a` deliberately;
  the bench-real impl spawns a scoped thread that propagates `cancel`
  into an `AtomicBool`. Loosening `Send + Sync` on `cancel` or `Send`
  on `progress` will silently break mid-run cancellation (the v0.3.21
  regression that codex-review caught).
- **`pub trait` dyn-safety**: there are two tests guarding
  `Box<dyn BenchExecutor>` / `Box<dyn SubmissionExecutor>`. Don't add
  generic methods, `Self: Sized`-only methods, or async-fn-in-trait to
  these traits without updating the engine call sites.
- **`schemars` is optional**: builds with `--no-default-features` MUST
  still compile. Don't add unconditional `JsonSchema` derives.

## Dependencies

- INCOMING:
  - `crates/superdeduper-bench-real/*` (default trait impl).
  - Engine binary (`src/main.rs`, `src/leaderboard/*`,
    `src/gui/widgets/bench_modal.rs`); re-exports `HardwareFingerprint`,
    `RunShape`, `ResultSummary`, `CanonicalBench` via `pub use` from
    `leaderboard::{hardware, submission}`.
- OUTGOING:
  - `serde` + `serde_json` (wire derives).
  - `thiserror` (BenchError Display).
  - `schemars` (telemetry only).
  - `std::path::{Path, PathBuf}`.

## Refactor Hints

- Cohesion is high: all types orbit `SubmissionInputs` /
  `BenchOutcome`. The module is one file (~740 lines) which is fine
  for a leaf type-crate; sub-moduling (e.g. `wire::hardware`,
  `wire::run_shape`, `executor`, `errors`) would help only if it grew
  past ~1.5kloc.
- Suspect dead code: none confirmed — every pub item is referenced.
  `RankEntry` is `serde::Deserialize` (only `Serialize` would be
  needed for outgoing wire); confirmed used in
  `crates/superdeduper-bench-real/src/submission_http.rs` and engine
  submission modules (grep `RankEntry` returned 8 hits across the
  workspace).
- Doc comments at line 113-114 contain a stale/contradictory paragraph
  (see findings) — refactor opportunity to delete the old placeholder
  sentence.
- The two `#[cfg_attr(feature = "telemetry", derive(schemars::JsonSchema))]`
  derives could be promoted to a single internal `macro_rules!` for
  brevity, but it's only 3 sites — not worth it today.
- `BenchServices::submit_fn` is `Option<&'a mut dyn FnMut(&SubmissionInputs) -> SubmitOutcome>`
  — note no `Result` wrap. Currently the engine maps transport errors
  into `SubmitOutcome::Transient` before returning; refactorers tempted
  to make this `-> Result<SubmitOutcome, BenchError>` should know the
  current wrapping is deliberate (the `BenchOutcome.submit` slot only
  cares about the user-surfaced enum).

## Wire Surfaces

- The structs `HardwareFingerprint`, `RunShape`, `ResultSummary`,
  `CanonicalBench`, `SubmissionInputs` (composed), `ChallengePosition`,
  `ChallengeAnswer`, `RankEntry`, and `SubmitOutcome::Accepted` are
  serialized to / deserialized from the backend at
  `api.superdeduper.io/api/v1/submit` and `POST /bench/start` /
  `/bench/challenges`.
- Schema source-of-truth: backend's Zod schema at
  `api.superdeduper.io/api/v1/submit/schema.json`; engine regen-guard
  test materializes `schema/submit.schema.json` via the `telemetry`
  feature's `schemars` derives.
- `BenchError` variant strings are surfaced verbatim in the GUI
  status panel + CLI stderr (`thiserror` Display).
- No env vars are read by this crate.
- No CLI flags are owned by this crate (the consuming engine binary
  parses `--bench-me`, `--fresh`, `--lane`, etc., and constructs
  `BenchContext` from them).

## Notes on referenced docs

- `docs/phase-0-trait-extraction.md` exists in the engine repo
  (confirmed).
- `docs/phase-0-p0d-move-plan.md` is referenced at lib.rs:42 but does
  NOT exist on disk — see findings.
