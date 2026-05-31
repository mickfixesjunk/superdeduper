//! G-track: gamification + leaderboards + achievements + canonical
//! bench corpus. Engine-side implementation per:
//!
//! * `~/sd-bench-local/design/gamification-client-spec.md` — contract
//! * `~/sd-bench-local/design/gamification-design.md` — vision
//! * `~/sd-bench-local/design/gamification-threat-model.md` — defenses
//! * `~/sd-bench-local/design/gamification-backend-spec.md` — API
//!
//! Phased rollout (single `feat/g-track` branch, one user-visible
//! release once all phases land):
//!
//! * **G1** (in progress) — hardware-detect / install.json /
//!   registration / HMAC submit / CLI `--share` / GUI post-scan modal
//! * **G2** — achievement evaluator + GUI unlock-toast + CLI rank
//! * **G3** — Google + Discord OAuth + cross-machine roll-up
//! * **G4** — canonical-bench corpus generator + Merkle proof flow
//!
//! Strictly gated on the `telemetry` Cargo feature. Distros / paranoid
//! users can ship a telemetry-stripped binary with
//! `--no-default-features --features gui`.

#![cfg(feature = "telemetry")]

pub mod account_badge_summary;
pub mod account_display_name;
pub mod account_privacy;
pub mod action_submission;
pub mod captcha;
pub mod pending_actions; // #99 v0.2.39 action-credit queue (skip-submit-then-recycle)
pub mod catalog;
pub mod hardware;
pub mod hmac_signer;
pub mod install;
pub mod oauth;
pub mod payload_meta;
pub mod predicates;
pub mod ranks_poll;
pub mod registration;
pub mod submission;
pub mod vanity_slug;

// G2-G4 modules slot in here as the phases land:
// pub mod achievements;  // G2
/// G4 / T-BENCH-ME canonical-bench primitives: deterministic ChaCha20 corpus
/// content (O(1) random access) + the FROZEN byte-exact Merkle proof
/// (per-1MiB-chunk leaves, RFC-6962 promote-last, base64-padded root) +
/// single-round challenge derivation. (corpus generator + --bench-me flow
/// build on these.) Telemetry-gated (the file is #![cfg(feature="telemetry")]).
///
/// P0-D Phase 1 (2026-05-31): module implementation moved to
/// `crates/superdeduper-bench-real/src/bench.rs`. The `pub use` shim
/// preserves the `leaderboard::bench::*` import path used by main.rs's
/// `run_make_bench_corpus` CLI handler + the bench_run / bench_corpus
/// internal callers (which now go cross-crate via the same workspace).
pub use superdeduper_bench_real::bench;
/// G4 / T-BENCH-ME corpus generator: the deterministic corpus-v1 layout
/// (B.3/B.4 production contract) on top of `bench`'s frozen primitives — pure
/// plan (file descriptors + groundtruth_dupsets + aggregates) plus
/// materialization (leaves / on-disk corpus / signed-shape manifest).
///
/// P0-D Phase 1 (2026-05-31): module implementation moved to
/// `crates/superdeduper-bench-real/src/bench_corpus.rs`. Re-exported here
/// for the same reason as `bench`.
pub use superdeduper_bench_real::bench_corpus;
/// G4 / T-BENCH-ME CLIENT bench primitives (post-Merkle-drop model): the public
/// `--bench-me` answers the server's challenge by hashing downloaded bytes at
/// server-issued offsets + commits to its dedupe result via `result_digest`.
/// No generation / seed / Merkle tree — this is what survives the generator strip.
pub mod bench_client; // G4
/// G4 / T-BENCH-ME shared `--bench-me` orchestration (download -> dedup ->
/// challenge -> submit), called by BOTH the CLI and the GUI button so they
/// can't drift. Telemetry-gated.
pub mod bench_run; // G4
/// D7 calibration probe (Phase C anti-cheat hardware-claim axis). Pure-function
/// probe-offset derivation from server-issued `calibration_seed` + on-disk
/// corpus layout. Engine + server compute byte-identical sequences; the
/// server verifier re-derives the same `(file_index, byte_offset)` pairs to
/// confirm probes hit the expected positions. Probe EXECUTION (read_uncached
/// + Instant timing) lives in bench_run.rs; this module is the cross-stack
/// byte-exact lock surface only.
///
/// P0-D Phase 1 (2026-05-31): module implementation moved to
/// `crates/superdeduper-bench-real/src/d7_probe.rs`. This `pub use`
/// preserves the `leaderboard::d7_probe::*` import path for any future
/// engine consumer; today no engine code outside the bench cluster
/// reaches d7_probe, so the re-export is forward-compatibility only.
pub use superdeduper_bench_real::d7_probe;
