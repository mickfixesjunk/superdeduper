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
//! - [`HardwareFingerprint`] — submission wire struct describing the
//!   client machine (CPU model, disk class, etc.). Moved here from the
//!   engine binary's `leaderboard::hardware` in Phase 2-A so the
//!   bench-real submission path can build payloads without back-
//!   importing engine internals.
//! - [`SubmissionExecutor`] — non-bench HMAC'd scan-submit path. Less
//!   sensitive; intended to stay public even after `BenchExecutor` moves
//!   to a closed-source repo.
//! - [`BenchError`] — structured error type at both trait boundaries.
//!   Stable enum; `-real` keeps `anyhow` inside the impl.
//!
//! ## Scope progression
//!
//! P0-C (2026-05-31): trait surface + opaque placeholder types so the
//! workspace builds standalone with no engine consumers.
//!
//! P0-D Phase 1 (this commit, 2026-05-31): SIMPLE types replaced with their
//! real engine shapes — `ChallengePosition`, `DebugDedupDiff` +
//! `DebugDedupDiffReport`, `InstallKey`. Bench-real crate added as workspace
//! member with a `BenchReal` stub impl + the `d7_probe` module moved across
//! the cut-line as a proof-of-concept.
//!
//! P0-D Phase 2 (post-launch): COMPLEX types — `BenchContext` (lifetime
//! decision), `BenchOutcome`, `SubmissionInputs` (depends on
//! `HardwareFingerprint`; needs cross-module type extraction first),
//! `SubmitOutcome` (rich enum). And the 5 remaining module moves
//! (`bench_run`, `bench_client`, `bench`, `bench_corpus`, submission HTTP
//! path). See `docs/phase-0-p0d-move-plan.md` for the full execution
//! catalog.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
/// Engine-canonical submission inputs.
///
/// Mirrors the backend schema's required top-level keys (less
/// `install_id` + `timestamp` which `build_payload` synthesises from
/// freshest sources): `client_version`, `hardware`, `run_shape`,
/// `result_summary`.
///
/// Phase 2-B 2026-06-01: real shape replacing the P0-D Phase 1
/// placeholder. The placeholder had 3 fields (client_version + run_uuid
/// + payload_json); this is the real wire-shape that engine
/// submission.rs + bench_run.rs use.
#[derive(Debug, Clone)]
pub struct SubmissionInputs {
    pub client_version: String,
    pub hardware: HardwareFingerprint,
    pub run_shape: RunShape,
    pub result_summary: ResultSummary,
    /// Local copy of the run UUID -- useful for engine-side
    /// diagnostic logging. NOT on the wire; backend assigns its own
    /// submission_id.
    pub run_uuid: String,
    /// #82 -- `scan_history::ScanRecord::scan_id` of the row this
    /// submission belongs to. Threaded through so the submit worker
    /// can stamp the server-issued `submission_id` back onto the
    /// right history row at POST-success time.
    pub scan_id: Option<String>,
    /// T-BENCH-ME: present only for `scope = "canonical-bench"`
    /// submissions. Carries the work-proof + version-binding fields.
    /// `None` for every ordinary scan submission.
    pub bench: Option<CanonicalBench>,
    /// Mick bench-lane UX: the user's explicit lane choice for this
    /// submission. Server treats this as authoritative; when present,
    /// body.lane overrides the server-derived has_account_linkage
    /// gating. Wire form: `"lane": "ranked"` or `"lane": "casual"`.
    pub lane: Option<String>,
}

/// One leaderboard rank-entry the server returns in `SubmitOutcome::Accepted.ranks`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RankEntry {
    pub category: String,
    pub bracket: String,
    pub rank: u64,
    pub bucket_size: u64,
}

/// Outcome of a non-bench submit.
///
/// Phase 2-B 2026-06-01: real enum replacing the P0-D Phase 1
/// placeholder struct. The placeholder had submission_id +
/// server_response; this is the real shape with Accepted /
/// DuplicateNoChange / Rejected / Transient / FlaggedForReview
/// variants matching engine submission::SubmitOutcome.
#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    /// 200 OK -- backend accepted and ranked.
    Accepted {
        submission_id: String,
        ranks: Vec<RankEntry>,
        achievements_unlocked: Vec<String>,
        profile_url: Option<String>,
    },
    /// 409 -- same payload hash already on file. Neutral status.
    /// `submission_id` carries the EXISTING submission_id when the
    /// server supplies it (post-#99 contract A); `None` if the server
    /// omitted it (older server / unresolvable).
    DuplicateNoChange { submission_id: Option<String> },
    /// 4xx (other than 409). Caller should surface the reason; don't
    /// retry (the payload is wrong, not the network).
    Rejected { status: u16, reason: String },
    /// 5xx / network. Caller should enqueue to disk for next launch.
    Transient { reason: String },
    /// User clicked "Submit for review" on a Rejected outcome. We
    /// saved a copy to the local review queue and (best-effort)
    /// uploaded to /api/v1/submit/review. `review_id` is the
    /// server-assigned id on success; `None` if the upload didn't
    /// land but the local save did.
    FlaggedForReview {
        review_id: Option<String>,
        local_path: String,
        original_status: u16,
        original_reason: String,
    },
}

/// One challenge position descriptor.
///
/// P0-D Phase 1 (2026-05-31): canonical home. The P0-C placeholder was
/// already byte-exact to the real shape in `bench_client.rs`; this
/// commit promotes the placeholder to canonical so engine can drop the
/// duplicate definition and `pub use` from here in Phase 2.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChallengePosition {
    pub path_index: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
}

/// One challenge answer (web's DEPLOYED wire): the descriptor echoed
/// back + the tag-0x02 `challenge_hash` of the bytes there
/// (std-base64). The server regenerates the same range from the
/// private seed and direct-compares.
///
/// Phase 2-B 2026-06-01: moved here from `bench_client.rs` alongside
/// the bench-client crate move. Mirror of ChallengePosition's earlier
/// promote (the public engine's `--bench-me` paths build these
/// structs; bench-real composes them; iface defines the shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChallengeAnswer {
    pub path_index: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub challenge_hash: String,
}

/// One row in `DebugDedupDiffReport.diffs` — a candidate whose three
/// content reads (parallel cold-bypass, serial cold-bypass, serial
/// buffered) don't all agree on `blake3(content)`. P0-D Phase 1
/// (2026-05-31): real shape moved from `src/leaderboard/bench_run.rs`.
#[derive(Debug, Clone)]
pub struct DebugDedupDiff {
    pub path_index: u64,
    pub path: PathBuf,
    pub size: u64,
    pub parallel_hash: [u8; 32],
    pub parallel_cold: bool,
    pub serial_hash: [u8; 32],
    pub serial_cold: bool,
    pub buffered_hash: [u8; 32],
}

/// Diagnostic report from [`BenchExecutor::debug_dedup_diff`]. Three dup-
/// group counts should agree on a correct dedup; any divergence is the
/// bug. P0-D Phase 1 (2026-05-31): real shape moved from
/// `src/leaderboard/bench_run.rs`, replacing the P0-C placeholder. The
/// scaffold dropped `diffs: Vec<DebugDedupDiff>` for simplicity; the
/// real consumer (`sd debug dedup-diff`) prints the per-candidate
/// divergence list, so the placeholder shape would have undercounted on
/// move-day.
#[derive(Debug, Clone)]
pub struct DebugDedupDiffReport {
    pub files_enumerated: u64,
    pub candidate_count: u64,
    pub parallel_dup_groups: usize,
    pub serial_dup_groups: usize,
    pub buffered_dup_groups: usize,
    /// Candidates where the three reads disagree, sorted by `path_index`.
    pub diffs: Vec<DebugDedupDiff>,
}

/// HMAC install key newtype — opaque secret material at the trait
/// boundary. P0-D Phase 1 (2026-05-31): corrected from `Vec<u8>` to
/// `[u8; 32]` to match the engine's `leaderboard::install::InstallKey`
/// type alias (which is `[u8; 32]`, not a heap vec). Length-erased
/// newtype keeps the iface ABI stable when the engine type's exact
/// representation evolves.
#[derive(Debug, Clone)]
pub struct InstallKey(pub [u8; 32]);

// --------------------------------------------------------------- hardware

/// Hardware-detect submission wire struct. Field shape is dictated by
/// the backend's Zod schema at `api.superdeduper.io/api/v1/submit/schema.json`
/// — emit exactly the keys it lists in `hardware.required`
/// (`additionalProperties: false`). Fields the client can't reliably
/// detect fall back to documented defaults rather than failing.
///
/// Phase 2-A 2026-06-01: moved here from the engine binary's
/// `leaderboard::hardware` module so bench-real's submission path can
/// build payloads without back-importing engine internals. The engine
/// keeps the `detect()` / `detect_with_root_hint()` probe functions in
/// `leaderboard::hardware` and re-exports this struct via `pub use`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "telemetry", derive(schemars::JsonSchema))]
pub struct HardwareFingerprint {
    /// Vendor + model as reported by the OS, e.g.
    /// `"AMD Ryzen 9 9950X3D 16-Core Processor"`.
    pub cpu_model_string: String,
    /// Physical cores. Distinct from `cpu_threads` on
    /// hyperthreaded CPUs (cores < threads).
    pub cpu_cores: u32,
    /// `std::thread::available_parallelism()` — total logical
    /// threads visible to the scheduler.
    pub cpu_threads: u32,
    /// CPUID ISA flag names, lowercase + dash-or-digit-only per
    /// the backend pattern `^[a-z0-9-]{1,20}$`. Sorted.
    pub cpu_isa_flags: Vec<String>,
    /// Snapped enum: `"4" | "8" | "16" | "32" | "64" | "128" |
    /// "256" | "512" | "1024"`. Round-down to the nearest enum
    /// value (so 30 GB -> "16"). Defaults to "16" if detection
    /// fails (sensible mid-range guess).
    pub ram_total_gb_bucket: String,
    /// OS-specific version label, e.g. `"Windows 11 25H2"`,
    /// `"macOS 14.5"`, `"Ubuntu 22.04"`. Free-form string.
    pub os_version: String,
    /// Enum: `"Home" | "Pro" | "Enterprise" | "Education" |
    /// "Server" | "Other"`. Parsed out of the full Windows
    /// edition string when possible; "Other" on macOS / Linux.
    pub os_edition: String,
    /// Enum: `"NVMe-Gen5" | "NVMe-Gen4" | "NVMe-Gen3" |
    /// "SATA-SSD" | "HDD" | "USB-SSD" | "USB-HDD" | "network" |
    /// "mixed"`. Defaults to "mixed" when detection fails.
    pub disk_class: String,
    /// Enum: `"NTFS" | "ReFS" | "exFAT" | "FAT32" |
    /// "network-SMB" | "other"`. Defaults to "NTFS" on Windows,
    /// "other" elsewhere.
    pub filesystem: String,
    /// NTFS cluster size in KB. Defaults to 4 (the NTFS default
    /// for volumes <= 16 TB).
    pub cluster_size_kb: u32,
    /// Snapped enum power-of-two from "1" to "32768" (GB).
    /// Defaults to "1024" (1 TB - common modern drive) when
    /// volume probe isn't wired.
    pub volume_size_gb_bucket: String,
    /// #130 Pathfinder Phase 3 — `true` when ANY local volume on
    /// this machine is a Windows Dev Drive (ReFS + developer-
    /// workload flag set, Windows 11 24H2+). Defaults to false
    /// on non-Windows and when no Dev Drive is detected.
    /// `#[serde(default)]` keeps old engine submissions readable
    /// (web sees them as false).
    #[serde(default)]
    pub is_dev_drive: bool,
}

// --------------------------------------------------------------- canonical_bench

/// T-BENCH-ME canonical-bench top-level submission fields (server-
/// direct-verify model; Merkle DROPPED). The engine's `build_payload`
/// lifts these to the top level of the /submit body when present.
///
/// Phase 2-B 2026-06-01: moved here from `leaderboard::submission`
/// alongside RunShape / ResultSummary. Engine keeps the construction
/// path (to_canonical_bench in bench_client) + re-exports this struct
/// via `pub use` for back-compat with existing call sites.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanonicalBench {
    pub protocol_version: String,
    pub corpus_version: String,
    pub tier: String,
    pub bench_run_id: String,
    /// web's DEPLOYED envelope: `{ answers: [{path_index, byte_offset,
    /// byte_length, challenge_hash}], result_digest }`. No merkle_root,
    /// no audit_path -- server regenerates each range from the private
    /// seed (tag 0x02) + direct-compares; checks result_digest ==
    /// digest(groundtruth).
    pub bench_proof: serde_json::Value,
    /// #106 -- whether EVERY timed candidate read bypassed the OS
    /// cache. The server's per-run gate routes cold_enforced=false
    /// runs to the casual (warm) board, excluded from the competitive
    /// cold HoF.
    pub cold_enforced: bool,
}

// --------------------------------------------------------------- run_shape

/// `run_shape` block per backend schema. Engine-canonical describing
/// the scan run (timing, file counts, scope enum, walker variant,
/// feature bitmap, esoteric achievement metrics).
///
/// Phase 2-B 2026-06-01: moved here from `leaderboard::submission`
/// so the bench-real submission path can build payloads without
/// back-importing engine internals. Engine keeps the builder /
/// FEATURE_BIT_* / WALKER_VARIANT_* constants + re-exports this struct
/// via `pub use`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "telemetry", derive(schemars::JsonSchema))]
pub struct RunShape {
    pub wall_clock_seconds: f64,
    pub bytes_scanned: u64,
    pub files_scanned: u64,
    /// Enum: `"river5-aes-ni" | "river5-96" | "blake3" | "sha256"
    /// | "other"`.
    pub hash_algorithm: String,
    /// Enum: `"mft" | "walker" | "hybrid"`.
    pub walker_variant: String,
    /// Enum: `"whole-volume" | "subdirectory" | "selection" |
    /// "canonical-bench"`.
    pub scope: String,
    /// Bitmap of which engine features were active during the scan.
    /// See `FEATURE_BIT_*` constants (engine-side).
    pub features_used_bitmap: u64,
    /// Enum: `"user-data" | "system" | "canonical-bench"`.
    pub corpus_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_ratio: Option<f64>,
    /// G2 client-claimed achievement IDs. Backend grants these
    /// when the predicate is `unlock_kind: client-claimed`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub easter_egg_hits: Vec<String>,
    /// G1.x server-side esoteric metrics. All optional / additive
    /// per the schema; older engines that don't compute these
    /// just omit the field. The backend uses them to grant
    /// catalog entries like "zero-byte hoarder", "hardlink
    /// archaeologist", "name-collider".

    /// Largest dup-group (by member count) whose content is zero
    /// bytes -- i.e. the count of empty-file "dups" in the largest
    /// such group. Used by backend to grant "zero-byte hoarder"
    /// when this exceeds the threshold.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub zero_byte_group_max: Option<u64>,
    /// Highest hardlink count observed on any inode during this
    /// scan. Backend uses this to grant "hardlink archaeologist".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_hardlink_count_in_scan: Option<u64>,
    /// Count of distinct (filename) values that appeared at least
    /// twice but with different content hashes -- i.e. identical
    /// filenames at different paths whose CONTENT differs. Used
    /// by backend to grant "name-collider".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name_collision_count: Option<u64>,
    /// Count of distinct network-share roots in this scan's scope
    /// (Windows UNC `\\server\share`, `smb://...`, `nfs://...`).
    /// Backend uses this to grant the latent `multi-share-maestro`
    /// achievement.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub share_count_in_scope: Option<u64>,
    /// #89 -- `true` when this scan submission carries no destructive
    /// actions. GUI scans are always dry-run at scan-finish time
    /// (actions ship later via PATCH /actions).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dry_run: Option<bool>,
    /// #89 -- count of duplicate-group cards the user opened/reviewed
    /// in the GUI since the last scan finished.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub groups_reviewed_count: Option<u64>,
}

// --------------------------------------------------------------- result_summary

/// `result_summary` block per backend schema. Counts + reclaim bytes
/// describing the duplicate sets found + (optionally) the dedupe
/// actions applied.
///
/// Phase 2-B 2026-06-01: moved here from `leaderboard::submission`
/// alongside RunShape. Engine keeps the ACTION_BYTES_KEY_* constants
/// and re-exports this struct via `pub use`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "telemetry", derive(schemars::JsonSchema))]
pub struct ResultSummary {
    pub duplicate_groups: u64,
    pub duplicate_bytes_reclaimable: u64,
    pub largest_single_group_bytes: u64,
    /// Per-action BYTES applied during dedupe, keyed by the
    /// locked schema strings web's `lifetime-audit.ts` expects:
    /// `"deleted_to_recycle_bytes"`, `"deleted_permanently_bytes"`,
    /// `"hardlink_replaced_bytes"`. Empty `{}` is valid + the
    /// natural state at scan-end (actions happen post-scan).
    /// Key names are LOCKED per design's gamification-achievement-
    /// balance.md action-bytes formula; web's auditor will reject
    /// submissions using different keys. Use the `ACTION_BYTES_KEY_*`
    /// constants in `leaderboard::submission` instead of hardcoding.
    pub actions_taken_summary: std::collections::BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder_skip_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder_skip_bytes: Option<u64>,
    /// T-BENCH-ME: the client's found duplicate sets as canonical
    /// global `path_index` lists (sorted within + across), for the
    /// server's client==groundtruth correctness gate. Present only
    /// on canonical-bench submissions; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_found_dupsets: Option<Vec<Vec<u64>>>,
}

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
