//! Engine-side stub for leaderboard + telemetry submissions.
//!
//! The backend at `https://api.superdeduper.com` is built by the
//! **superdeduper-backend** agent and the UI is built by the
//! **superdeduper-website** agent. This module is the engine's
//! contract for what data goes in submissions and how it's signed.
//!
//! See `docs/leaderboard-spec.md` and `docs/preflight-spec.md` for the
//! full design. This file ships intentionally non-functional (no-op
//! submit) until the backend lands, but the shapes here are the wire
//! format the backend will receive.

use std::path::PathBuf;

use serde::Serialize;

/// The full submission body for the leaderboard endpoint.
/// `POST https://api.superdeduper.com/v1/leaderboard-submit`
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardSubmission {
    pub schema: &'static str,
    pub run_uuid: String,
    pub sd_version: String,
    pub sd_build_hash: String,
    pub timestamp_unix: i64,

    pub hardware: HardwareInfo,
    pub workload: WorkloadShape,
    pub results: ScanResults,
    pub attestation: AttestationFields,
    pub anti_cheat: AntiCheatBlob,
}

#[derive(Debug, Clone, Serialize)]
pub struct HardwareInfo {
    pub cpu_model: String,
    pub cpu_threads: usize,
    /// Backend-defined coarse bucket — `x86_64-modern-high`,
    /// `x86_64-modern-mid`, `x86_64-legacy`, `arm64-modern`, etc.
    pub cpu_class: String,
    pub ram_gb_tier: u32,
    pub drive_class: String,
    pub drive_seq_read_mbps_observed: f64,
    pub os: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkloadShape {
    /// `<1k`, `1k-10k`, `10k-100k`, `100k-1M`, `>1M`.
    pub file_count_bucket: String,
    /// `<1GB`, `1-10GB`, `10-100GB`, `100-1000GB`, `>1TB`.
    pub total_size_bucket_gb: String,
    /// `<10KB`, `10-100KB`, …, `>100MB`.
    pub avg_file_size_bucket: String,
    /// `0-10%`, `10-25%`, `25-50%`, `50-100%`.
    pub dup_density_pct_bucket: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanResults {
    pub wall_clock_ms: u64,
    pub bytes_hashed: u64,
    pub peak_rss_mb: u64,
    pub reclaimable_inode_bytes: u64,
    pub dup_groups: u32,
    pub hash_algo: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttestationFields {
    pub defender_rtp_state_pre: Option<bool>,
    pub defender_rtp_state_post: Option<bool>,
    /// One of `"purged"`, `"unknown"`, `"warm"`.
    pub cache_state_pre: String,
    /// SHA-256 of the preflight diagnose JSON, when one was run.
    pub preflight_report_hash: Option<String>,
    /// Hash of the workload's file-size distribution (no paths or
    /// content). Two users scanning the same canonical corpus produce
    /// the same value here.
    pub corpus_signature_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AntiCheatBlob {
    /// Base64 of an engine-signed attestation. The backend verifies
    /// against the release public key list. Key rotation per release;
    /// any modified binary won't have a valid signing key.
    pub engine_attestation_blob: String,
    /// SHA-256 of internal diagnostic counters (bytes_read per tier,
    /// elapsed_ms per stage, etc.). Used as a cross-check against the
    /// declared `wall_clock_ms` / `bytes_hashed`.
    pub scan_log_proof_hash: String,
}

/// Preflight submission — separate, opt-in, anonymous. No auth.
/// `POST https://api.superdeduper.com/v1/preflight-submit`
#[derive(Debug, Clone, Serialize)]
pub struct PreflightSubmission {
    pub schema: &'static str,
    pub timestamp_unix: i64,
    /// Stable irreversible hash of (cpu_model, cpu_threads, ram_gb,
    /// drive_class, drive_serial_hash). Does NOT identify the user.
    pub machine_identity_hash: String,
    pub diagnose_report_json: serde_json::Value,
    /// Bucketed workload shape (same as LeaderboardSubmission's).
    pub workload: WorkloadShape,
    pub measured_wall_clock_ms: u64,
}

/// Inputs to `build_submission`. Holds the engine state that the
/// caller (GUI or CLI flag) wants to turn into a wire payload.
pub struct SubmissionContext {
    pub run_uuid: String,
    pub sd_version: String,
    pub sd_build_hash: String,
    pub hardware: HardwareInfo,
    pub workload: WorkloadShape,
    pub results: ScanResults,
    pub attestation: AttestationFields,
}

/// Build a leaderboard payload from a completed scan. Caller supplies
/// everything; this function just packages it into the wire format
/// and includes the anti-cheat blob.
pub fn build_submission(ctx: SubmissionContext) -> LeaderboardSubmission {
    let anti_cheat = sign_attestation(&ctx);
    LeaderboardSubmission {
        schema: "superdeduper.leaderboard-submit.v1",
        run_uuid: ctx.run_uuid,
        sd_version: ctx.sd_version,
        sd_build_hash: ctx.sd_build_hash,
        timestamp_unix: now_unix(),
        hardware: ctx.hardware,
        workload: ctx.workload,
        results: ctx.results,
        attestation: ctx.attestation,
        anti_cheat,
    }
}

/// Stub: produces a deterministic but unverifiable blob.
///
/// Real implementation pulls the release signing key (ed25519
/// recommended; small and fast) baked into the binary at build time,
/// signs a tuple of `(run_uuid, wall_clock_ms, corpus_signature_hash,
/// defender_state_pre, preflight_report_hash)` with it, base64-encodes
/// the signature.
///
/// Backend verifies with the matching public key, also baked into the
/// release list it knows about. New release → new key; old releases'
/// submissions stay verifiable.
pub fn sign_attestation(_ctx: &SubmissionContext) -> AntiCheatBlob {
    // TODO(post-backend-ship): real ed25519 signing here.
    AntiCheatBlob {
        engine_attestation_blob: "STUB_NOT_FOR_PRODUCTION".to_string(),
        scan_log_proof_hash: "STUB_NOT_FOR_PRODUCTION".to_string(),
    }
}

/// POST to the backend. No-op stub. Real implementation:
/// 1. Send via `ureq` (already a transitive dependency for some
///    crates; check before adding a direct dep).
/// 2. Handle 4xx (validation rejection → log + return error) and 5xx
///    (transient → retry with backoff).
/// 3. Cache failed submissions to disk; retry next launch.
#[allow(dead_code)]
pub fn submit(_submission: LeaderboardSubmission) -> std::io::Result<()> {
    // TODO(post-backend-ship): real HTTP submission here.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "telemetry submission stub — backend not yet implemented",
    ))
}

/// Auth token storage. The GUI's Google OAuth flow obtains a JWT and
/// hands it here for persistence. Same storage location for CLI use
/// (future `--submit-leaderboard` flag reads this).
#[allow(dead_code)]
pub fn token_path() -> Option<PathBuf> {
    // %LOCALAPPDATA%\superdeduper\auth.json (Windows)
    // $XDG_DATA_HOME/superdeduper/auth.json    (Linux)
    // ~/Library/Application Support/superdeduper/auth.json (macOS)
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|s| PathBuf::from(s).join("superdeduper").join("auth.json"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|s| {
            PathBuf::from(s)
                .join("Library")
                .join("Application Support")
                .join("superdeduper")
                .join("auth.json")
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(|s| PathBuf::from(s).join("superdeduper").join("auth.json"))
            .or_else(|| {
                std::env::var_os("HOME").map(|s| {
                    PathBuf::from(s)
                        .join(".local")
                        .join("share")
                        .join("superdeduper")
                        .join("auth.json")
                })
            })
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
