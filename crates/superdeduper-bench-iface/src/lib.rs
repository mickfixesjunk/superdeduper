//! superdeduper-bench-iface
//!
//! Phase 0 trait extraction iface crate. Defines the public boundary
//! between the engine and the future `superdeduper-bench-real` (default
//! impl) / `superdeduper-bench-stub` (Err(Unavailable) impl) crates.
//!
//! See `docs/phase-0-trait-extraction.md` (engine repo) for the cut-line
//! rationale, dependency graph, and migration plan.
//!
//! ## Surface
//!
//! - [`BenchExecutor`] — bench-flow path (`run_bench` + `debug_dedup_diff`).
//!   Anti-cheat-sensitive; Phase 1+ closed-source candidate per Q5
//!   resolution.
//! - [`SubmissionExecutor`] — non-bench HMAC'd scan-submit path. Less
//!   sensitive; intended to stay public even after `BenchExecutor` moves
//!   to a closed-source repo.
//! - [`BenchError`] — structured error type at both trait boundaries.
//!   Stable enum; `-real` keeps `anyhow` inside the impl.
//!
//! ## Scaffold scope (P0-C, 2026-05-31)
//!
//! Method signatures use opaque placeholder types (newtype-wrapped
//! `String` / `Vec<u8>`) so the workspace builds standalone. Concrete
//! types (`BenchContext`, `BenchOutcome`, `SubmissionInputs`,
//! `SubmitOutcome`, `ChallengePosition`, `DebugDedupDiffReport`) are
//! still owned by the engine `src/leaderboard/` modules and will move
//! into this crate in P0-D when the `-real` impl pulls the leaderboard
//! internals across the cut-line. Until then, engine call sites remain
//! inline and this crate has no consumers.

use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------- types

/// Inputs the bench-flow executor needs to run `POST /bench/start` and
/// drive the bench loop to completion. Opaque placeholder for the
/// scaffold; P0-D fills in the real fields when the leaderboard internals
/// cross the cut-line.
#[derive(Debug, Clone)]
pub struct BenchContext {
    pub install_id: String,
    pub corpus_version: String,
    pub tier: String,
    pub lane: Option<String>,
}

/// Result of a bench-flow run. Opaque placeholder for the scaffold; P0-D
/// replaces this with the real `BenchOutcome` (currently in
/// `src/leaderboard/bench_run.rs`).
#[derive(Debug, Clone)]
pub struct BenchOutcome {
    pub bench_run_id: String,
    pub result_digest_v3_1: String,
    pub dedupe_secs: f64,
    pub submit_response: Option<String>,
}

/// Inputs for the non-bench HMAC scan-submit path. Opaque placeholder
/// for the scaffold; P0-D replaces this with the real `SubmissionInputs`
/// (currently in `src/leaderboard/submission.rs`).
#[derive(Debug, Clone)]
pub struct SubmissionInputs {
    pub client_version: String,
    pub run_uuid: String,
    pub payload_json: String,
}

/// Outcome of a non-bench submit. Opaque placeholder for the scaffold.
#[derive(Debug, Clone)]
pub struct SubmitOutcome {
    pub submission_id: String,
    pub server_response: String,
}

/// One challenge position descriptor. Opaque placeholder for the
/// scaffold; P0-D moves the real `ChallengePosition` (currently in
/// `src/leaderboard/bench_client.rs`) into this crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChallengePosition {
    pub path_index: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
}

/// Diagnostic report from `BenchExecutor::debug_dedup_diff`. Opaque
/// placeholder for the scaffold.
#[derive(Debug, Clone)]
pub struct DebugDedupDiffReport {
    pub files_enumerated: u64,
    pub candidate_count: u64,
    pub parallel_dup_groups: usize,
    pub serial_dup_groups: usize,
    pub buffered_dup_groups: usize,
    pub diff_count: usize,
}

/// HMAC install key newtype — opaque secret material at the trait
/// boundary. P0-D will replace with the engine's existing newtype
/// (`leaderboard::install::InstallKey`).
#[derive(Debug, Clone)]
pub struct InstallKey(pub Vec<u8>);

// --------------------------------------------------------------- errors

/// Stable error surface at both trait boundaries. `anyhow` stays inside
/// the `-real` impl; callers across the cut-line match on this enum.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// Returned by the `-stub` impl from every method when the engine
    /// is built without the `bench-real` feature.
    #[error("bench executor unavailable (built with --no-default-features)")]
    Unavailable,

    /// Network / transport / connection failure (DNS, TLS, timeout,
    /// 5xx from server).
    #[error("network: {0}")]
    Network(String),

    /// HMAC signing failure (key material malformed, install state
    /// corrupted, OS keystore unreadable).
    #[error("hmac signing: {0}")]
    Hmac(String),

    /// Server rejected the submission with a 4xx (validation,
    /// auth, plausibility cap, etc.). `body` is the verbatim server
    /// response.
    #[error("server rejected ({status}): {body}")]
    ServerRejected { status: u16, body: String },

    /// Corpus read / hash / scan failure during the bench loop.
    #[error("corpus io: {0}")]
    CorpusIo(String),

    /// Cancelled by the caller (cancel callback returned true).
    #[error("cancelled by caller")]
    Cancelled,

    /// Internal invariant violation in the bench loop (would-be panic
    /// in the impl, surfaced as a structured error here so the GUI
    /// can show a clean message).
    #[error("internal: {0}")]
    Internal(String),
}

// --------------------------------------------------------------- traits

/// Bench-flow executor. Anti-cheat-sensitive surface; Phase 1+
/// closed-source candidate per `docs/phase-0-trait-extraction.md` §2 Q5.
///
/// Implementations:
/// - `superdeduper-bench-real`: default (feature = "bench-real"). Wraps
///   the current leaderboard / bench_run internals.
/// - `superdeduper-bench-stub`: opt-in (feature = "bench-stub", aka
///   `--no-default-features`). Every method returns
///   `Err(BenchError::Unavailable)`. Used for binary slices that ship
///   without bench/anti-cheat code (audit builds, hermetic dev images).
pub trait BenchExecutor: Send + Sync {
    /// Run the canonical bench-me loop: `POST /bench/start`, download
    /// corpus, dedup, answer challenges, submit. Returns the outcome the
    /// CLI / GUI surfaces to the user.
    ///
    /// `progress` is invoked with short, human-facing status strings as
    /// the bench advances stages. `cancel` is polled between stages; the
    /// impl bails with `BenchError::Cancelled` the moment it returns
    /// true.
    fn run_bench(
        &self,
        ctx: BenchContext,
        progress: &mut dyn FnMut(&str),
        cancel: &dyn Fn() -> bool,
    ) -> Result<BenchOutcome, BenchError>;

    /// Diagnostic helper used by `sd debug dedup-diff`. Dedups a corpus
    /// directory three ways (parallel-cold, serial-cold, serial-buffered)
    /// and reports per-candidate hash divergence. No network, no
    /// submission, telemetry-only.
    fn debug_dedup_diff(
        &self,
        corpus_dir: &Path,
    ) -> Result<DebugDedupDiffReport, BenchError>;
}

/// Non-bench submission executor. HMAC'd HTTP scan-submit. Less
/// sensitive than `BenchExecutor`; could stay public even after
/// `BenchExecutor` moves to the private closed-source repo.
pub trait SubmissionExecutor: Send + Sync {
    /// Submit a recorded scan payload to the leaderboard endpoint.
    /// Triggered from the GUI scan-complete modal and the resubmit-pending
    /// queue.
    fn submit_recorded(
        &self,
        inputs: SubmissionInputs,
        install_id: &str,
        install_key: &InstallKey,
    ) -> Result<SubmitOutcome, BenchError>;
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub implementation used to verify the trait surface compiles +
    /// is dyn-safe (the engine call sites will use `Box<dyn BenchExecutor>`
    /// once P0-D wires this up).
    struct ScaffoldStub;

    impl BenchExecutor for ScaffoldStub {
        fn run_bench(
            &self,
            _ctx: BenchContext,
            _progress: &mut dyn FnMut(&str),
            _cancel: &dyn Fn() -> bool,
        ) -> Result<BenchOutcome, BenchError> {
            Err(BenchError::Unavailable)
        }

        fn debug_dedup_diff(
            &self,
            _corpus_dir: &Path,
        ) -> Result<DebugDedupDiffReport, BenchError> {
            Err(BenchError::Unavailable)
        }
    }

    impl SubmissionExecutor for ScaffoldStub {
        fn submit_recorded(
            &self,
            _inputs: SubmissionInputs,
            _install_id: &str,
            _install_key: &InstallKey,
        ) -> Result<SubmitOutcome, BenchError> {
            Err(BenchError::Unavailable)
        }
    }

    #[test]
    fn bench_executor_is_dyn_safe() {
        let _: Box<dyn BenchExecutor> = Box::new(ScaffoldStub);
    }

    #[test]
    fn submission_executor_is_dyn_safe() {
        let _: Box<dyn SubmissionExecutor> = Box::new(ScaffoldStub);
    }

    #[test]
    fn stub_returns_unavailable() {
        let stub = ScaffoldStub;
        let ctx = BenchContext {
            install_id: "id".into(),
            corpus_version: "cv".into(),
            tier: "quick".into(),
            lane: None,
        };
        let mut progress = |_: &str| {};
        let cancel = || false;
        let r = stub.run_bench(ctx, &mut progress, &cancel);
        assert!(matches!(r, Err(BenchError::Unavailable)));
    }

    #[test]
    fn bench_error_renders_human_readable_messages() {
        // Sanity: thiserror Display impls are wired so the GUI can show
        // a clean message for each variant without unwrapping the
        // implementation-internal error chain.
        assert_eq!(
            format!("{}", BenchError::Unavailable),
            "bench executor unavailable (built with --no-default-features)"
        );
        assert_eq!(
            format!("{}", BenchError::Network("dns".into())),
            "network: dns"
        );
        assert_eq!(
            format!(
                "{}",
                BenchError::ServerRejected { status: 422, body: "bad lane".into() }
            ),
            "server rejected (422): bad lane"
        );
    }
}
